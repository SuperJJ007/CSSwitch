use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Error, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::http_codec::{write_codex_models_response, RequestHead};
use super::inference_dispatch::{
    apply_dsml_nonstream, collect_codex_nonstream, dsml_stream_filter, forward_stream_body,
    handle_codex_messages_with_catalog, handle_codex_messages_with_secrets, handle_post,
    map_codex_auth_error, openai_chat_reasoning_signer, pump_codex_stream, stream_error_event,
    write_codex_models_with_refresh, CodexComponents, CodexNonstreamError, CodexPumpError,
    RequestNonceGenerator, StreamFilter, StreamTermination,
};
#[cfg(unix)]
use super::skill_bridge_host::{
    acquire_bridge_host_lock, bridge_request_is_replay, finalize_bridge_processing,
    read_regular_bridge_request, recover_orphaned_bridge_processing, write_bridge_response_once,
    write_bridge_status, BridgeProgress,
};
use crate::anthropic_compat::KimiServerToolFilter;
use crate::codex_auth::{InferenceSecrets, OAuthErrorCode, OAuthFlowError};
use crate::codex_models::CodexModelCatalog;
use crate::codex_protocol::{ResponsesReducer, ThinkingSigner, MAX_REQUEST_BYTES};
use crate::codex_transport::CodexTransport;
use crate::config::{GatewayConfig, DEFAULT_CODEX_UPSTREAM_URL};
use crate::dsml_shim::DsmlStreamRewriter;
use crate::models::RelayModelCache;

struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(Error::new(ErrorKind::ConnectionReset, "mock read failure"))
    }
}

struct CountingEofReader {
    reads: usize,
}

fn bind_loopback() -> TcpListener {
    loop {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        if listener.local_addr().unwrap().port() != 8765 {
            return listener;
        }
    }
}

fn capture_tcp_response(handler: impl FnOnce(&mut TcpStream)) -> Vec<u8> {
    let listener = bind_loopback();
    let address = listener.local_addr().unwrap();
    let reader = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    });
    let (mut stream, _) = listener.accept().unwrap();
    handler(&mut stream);
    drop(stream);
    reader.join().unwrap()
}

fn read_mock_http_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut expected = None;
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0);
        request.extend_from_slice(&buffer[..read]);
        if expected.is_none() {
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]);
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                expected = Some(end + 4 + length);
            }
        }
        if expected.is_some_and(|length| request.len() >= length) {
            return request;
        }
    }
}

type MockCodexTransport = (
    CodexTransport,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<Vec<u8>>>>,
    thread::JoinHandle<()>,
);

fn mock_codex_transport(response: Vec<u8>) -> MockCodexTransport {
    let listener = bind_loopback();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let count_for_thread = Arc::clone(&count);
    let requests_for_thread = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut quiet_deadline = None;
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    count_for_thread.fetch_add(1, Ordering::SeqCst);
                    requests_for_thread
                        .lock()
                        .unwrap()
                        .push(read_mock_http_request(&mut stream));
                    stream.write_all(&response).unwrap();
                    stream.flush().unwrap();
                    quiet_deadline = Some(Instant::now() + Duration::from_millis(150));
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if quiet_deadline.is_some_and(|deadline| Instant::now() >= deadline)
                        || Instant::now() >= deadline
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("mock Codex accept failed: {error}"),
            }
        }
    });
    let transport = CodexTransport::for_test(format!("http://{address}/responses")).unwrap();
    (transport, count, requests, handle)
}

fn mock_codex_model_catalog(
    model_ids: &[&str],
) -> (
    CodexModelCatalog,
    thread::JoinHandle<()>,
    std::path::PathBuf,
) {
    let models: Vec<Value> = model_ids
        .iter()
        .enumerate()
        .map(|(priority, model)| {
            json!({
                "slug": model,
                "display_name": model,
                "visibility": "list",
                "supported_in_api": true,
                "priority": priority,
                "default_reasoning_level": "medium",
                "supported_reasoning_levels": [{"effort": "medium", "description": "default"}],
                "supports_reasoning_summary_parameter": true,
                "supports_parallel_tool_calls": true,
            })
        })
        .collect();
    mock_codex_model_catalog_with_models(models)
}

fn mock_codex_model_catalog_with_models(
    models: Vec<Value>,
) -> (
    CodexModelCatalog,
    thread::JoinHandle<()>,
    std::path::PathBuf,
) {
    use std::os::unix::fs::PermissionsExt;

    let listener = bind_loopback();
    let address = listener.local_addr().unwrap();
    let body = serde_json::to_vec(&json!({"models": models})).unwrap();
    let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_mock_http_request(&mut stream);
        stream.write_all(&response).unwrap();
        stream.write_all(&body).unwrap();
        stream.flush().unwrap();
    });
    let mut random = [0_u8; 8];
    getrandom::getrandom(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!(
        "csswitch-server-codex-models-{}-{}",
        std::process::id(),
        u64::from_ne_bytes(random)
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let catalog =
        CodexModelCatalog::for_test(format!("http://{address}/models"), root.clone()).unwrap();
    (catalog, server, root)
}

fn http_sse_response(body: &[u8]) -> Vec<u8> {
    let mut response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
    response.extend_from_slice(body);
    response
}

fn complete_codex_sse() -> Vec<u8> {
    [
            json!({"type": "response.created", "response": {"id": "resp"}}),
            json!({"type": "response.output_text.delta", "item_id": "msg", "delta": "hello"}),
            json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg", "content": [{"type": "output_text", "text": "hello"}]}}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 2, "output_tokens": 1}}}),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
        .into_bytes()
}

fn codex_request(stream: bool) -> Value {
    json!({
        "model": "gpt-test",
        "max_tokens": 128,
        "stream": stream,
        "messages": [{"role": "user", "content": "hello"}],
    })
}

fn http_body(response: &[u8]) -> &[u8] {
    let end = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap();
    &response[end + 4..]
}

#[test]
fn codex_handler_streams_upstream_once_for_stream_and_nonstream() {
    for is_stream in [false, true] {
        let (transport, count, requests, upstream) =
            mock_codex_transport(http_sse_response(&complete_codex_sse()));
        let response = capture_tcp_response(|stream| {
            handle_codex_messages_with_secrets(
                stream,
                &codex_request(is_stream),
                is_stream,
                InferenceSecrets::for_test("access", "account"),
                &transport,
                |_, _| panic!("successful request must not reject auth"),
            )
        });
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        if is_stream {
            let text = String::from_utf8_lossy(&response);
            assert!(text.contains("event: message_start"));
            assert!(text.contains("event: message_stop"));
        } else {
            let body: Value = serde_json::from_slice(http_body(&response)).unwrap();
            assert_eq!(body["content"][0]["text"], "hello");
            assert_eq!(
                body["usage"],
                json!({"input_tokens": 2, "output_tokens": 1})
            );
        }
        let request = requests.lock().unwrap()[0].clone();
        let upstream_body: Value = serde_json::from_slice(http_body(&request)).unwrap();
        assert_eq!(upstream_body["stream"], true);
        assert_eq!(upstream_body["store"], false);
        assert_eq!(upstream_body["model"], "gpt-test");
    }
}

#[test]
fn codex_catalog_capabilities_drive_each_selected_model_request() {
    let models = vec![
        json!({
            "slug": "gpt-sequential",
            "display_name": "Sequential",
            "visibility": "list",
            "supported_in_api": false,
            "priority": 0,
            "default_reasoning_level": "low",
            "supported_reasoning_levels": [{"effort": "low"}],
            "supports_reasoning_summary_parameter": false,
            "supports_parallel_tool_calls": false
        }),
        json!({
            "slug": "gpt-parallel",
            "display_name": "Parallel",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 1,
            "default_reasoning_level": "high",
            "supported_reasoning_levels": [{"effort": "medium"}, {"effort": "high"}],
            "supports_reasoning_summary_parameter": true,
            "supports_parallel_tool_calls": true
        }),
        json!({
            "slug": "gpt-lite",
            "display_name": "Lite",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 2,
            "default_reasoning_level": "high",
            "supported_reasoning_levels": [{"effort": "high"}],
            "supports_reasoning_summary_parameter": true,
            "supports_parallel_tool_calls": true,
            "use_responses_lite": true
        }),
    ];
    let (catalog, models_server, root) = mock_codex_model_catalog_with_models(models);
    let mut models_server = Some(models_server);

    for (index, (alias, requested_effort, effort, summary, parallel, lite)) in [
        (
            "claude-csswitch-codex-gpt-sequential",
            None,
            "low",
            false,
            false,
            false,
        ),
        (
            "claude-csswitch-codex-gpt-parallel",
            Some("medium"),
            "medium",
            true,
            true,
            false,
        ),
        (
            "claude-csswitch-codex-gpt-lite",
            Some("high"),
            "high",
            true,
            false,
            true,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let (transport, posts, requests, upstream) =
            mock_codex_transport(http_sse_response(&complete_codex_sse()));
        let mut request = codex_request(false);
        request["model"] = json!(alias);
        request["tools"] = json!([{
            "name": "read",
            "description": "read",
            "input_schema": {"type": "object"}
        }]);
        request["tool_choice"] = json!({"type": "auto"});
        if let Some(requested_effort) = requested_effort {
            request["output_config"] = json!({"effort": requested_effort});
        }
        let response = capture_tcp_response(|stream| {
            handle_codex_messages_with_catalog(
                stream,
                &request,
                false,
                InferenceSecrets::for_test("access", "account"),
                &transport,
                &catalog,
                |_, _| panic!("successful request must not reject auth"),
            )
        });
        if index == 0 {
            models_server.take().unwrap().join().unwrap();
        }
        upstream.join().unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        let raw_request = requests.lock().unwrap()[0].clone();
        let upstream_body: Value = serde_json::from_slice(http_body(&raw_request)).unwrap();
        assert_eq!(
            upstream_body["model"],
            alias.trim_start_matches("claude-csswitch-codex-")
        );
        assert_eq!(upstream_body["reasoning"]["effort"], effort);
        assert_eq!(upstream_body["reasoning"].get("summary").is_some(), summary);
        assert_eq!(upstream_body["parallel_tool_calls"], parallel);
        let request_head = String::from_utf8_lossy(
            &raw_request[..raw_request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap()],
        )
        .to_ascii_lowercase();
        assert_eq!(
            request_head.contains("x-openai-internal-codex-responses-lite: true"),
            lite
        );
        if lite {
            assert!(upstream_body.get("tools").is_none());
            assert!(upstream_body.get("instructions").is_none());
            assert_eq!(upstream_body["input"][0]["type"], "additional_tools");
            assert_eq!(upstream_body["reasoning"]["context"], "all_turns");
        }
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn codex_named_tool_choice_posts_exact_function_and_unknown_name_posts_nothing() {
    let mut request = codex_request(false);
    request["tools"] = json!([{
        "name": "read",
        "description": "read",
        "input_schema": {"type": "object"}
    }]);
    request["tool_choice"] = json!({"type": "tool", "name": "read"});
    let (transport, posts, requests, upstream) =
        mock_codex_transport(http_sse_response(&complete_codex_sse()));
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_secrets(
            stream,
            &request,
            false,
            InferenceSecrets::for_test("access", "account"),
            &transport,
            |_, _| panic!("successful request must not reject auth"),
        )
    });
    upstream.join().unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200 OK"));
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    let upstream_body: Value =
        serde_json::from_slice(http_body(&requests.lock().unwrap()[0])).unwrap();
    assert_eq!(
        upstream_body["tool_choice"],
        json!({"type": "function", "name": "read"})
    );

    request["tool_choice"] = json!({"type": "tool", "name": "missing"});
    let unreachable = CodexTransport::for_test("http://127.0.0.1:1/responses".into()).unwrap();
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_secrets(
            stream,
            &request,
            false,
            InferenceSecrets::for_test("access", "account"),
            &unreachable,
            |_, _| panic!("invalid request must not reject auth"),
        )
    });
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request"));
    assert!(String::from_utf8_lossy(&response).contains("forced tool is not declared"));
}

#[test]
fn codex_raw_or_unknown_account_model_is_rejected_before_inference_post() {
    let (catalog, models_server, root) = mock_codex_model_catalog(&["gpt-known"]);
    let transport = CodexTransport::for_test("http://127.0.0.1:1/responses".into()).unwrap();
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_catalog(
            stream,
            &codex_request(false),
            false,
            InferenceSecrets::for_test("access", "account"),
            &transport,
            &catalog,
            |_, _| panic!("unknown model must not reject auth"),
        )
    });
    models_server.join().unwrap();
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request"));
    let response = String::from_utf8_lossy(&response);
    assert!(response.contains("\"type\":\"route_unknown\""));
    assert!(response.contains("model selector is not available for this Codex account"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn codex_role_alias_uses_first_standard_model_and_preserves_forced_tool() {
    let models = vec![
        json!({
            "slug": "gpt-lite-first",
            "display_name": "Lite",
            "visibility": "list",
            "priority": 0,
            "use_responses_lite": true,
        }),
        json!({
            "slug": "gpt-standard",
            "display_name": "Standard",
            "visibility": "list",
            "priority": 1,
            "supports_parallel_tool_calls": true,
        }),
    ];
    let (catalog, models_server, root) = mock_codex_model_catalog_with_models(models);
    let (transport, posts, requests, upstream) =
        mock_codex_transport(http_sse_response(&complete_codex_sse()));
    let mut request = codex_request(false);
    request["model"] = json!("claude-haiku-4-5-20251001");
    request["tools"] = json!([{
        "name": "create_work_item",
        "strict": true,
        "input_schema": {
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"],
            "additionalProperties": false,
        },
    }]);
    request["tool_choice"] = json!({"type": "tool", "name": "create_work_item"});
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_catalog(
            stream,
            &request,
            false,
            InferenceSecrets::for_test("access", "account"),
            &transport,
            &catalog,
            |_, _| panic!("successful request must not reject auth"),
        )
    });
    models_server.join().unwrap();
    upstream.join().unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200 OK"));
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    let upstream_body: Value =
        serde_json::from_slice(http_body(&requests.lock().unwrap()[0])).unwrap();
    assert_eq!(upstream_body["model"], "gpt-standard");
    assert_eq!(
        upstream_body["tool_choice"],
        json!({"type": "function", "name": "create_work_item"})
    );
    assert_eq!(upstream_body["tools"][0]["strict"], true);

    let (transport, posts, requests, upstream) =
        mock_codex_transport(http_sse_response(&complete_codex_sse()));
    let mut classifier = codex_request(false);
    classifier["model"] = json!("claude-sonnet-4-6");
    classifier["tools"] = json!([{
        "name": "classify_memory",
        "strict": true,
        "input_schema": {
            "type": "object",
            "properties": {"risk": {"type": "string"}},
            "required": ["risk"],
            "additionalProperties": false,
        },
    }]);
    classifier["tool_choice"] = json!({"type": "tool", "name": "classify_memory"});
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_catalog(
            stream,
            &classifier,
            false,
            InferenceSecrets::for_test("access", "account"),
            &transport,
            &catalog,
            |_, _| panic!("successful classifier must not reject auth"),
        )
    });
    upstream.join().unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200 OK"));
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    let classifier_body: Value =
        serde_json::from_slice(http_body(&requests.lock().unwrap()[0])).unwrap();
    assert_eq!(classifier_body["model"], "gpt-standard");
    assert_eq!(
        classifier_body["tool_choice"],
        json!({"type": "function", "name": "classify_memory"})
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn codex_role_without_standard_model_and_lite_forced_tool_fail_before_post() {
    let models = vec![json!({
        "slug": "gpt-lite",
        "display_name": "Lite",
        "visibility": "list",
        "priority": 0,
        "use_responses_lite": true,
    })];
    let (catalog, models_server, root) = mock_codex_model_catalog_with_models(models);
    let unreachable = CodexTransport::for_test("http://127.0.0.1:1/responses".into()).unwrap();

    let mut role_request = codex_request(false);
    role_request["model"] = json!("claude-sonnet-4-6");
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_catalog(
            stream,
            &role_request,
            false,
            InferenceSecrets::for_test("access", "account"),
            &unreachable,
            &catalog,
            |_, _| panic!("unavailable model must not reject auth"),
        )
    });
    models_server.join().unwrap();
    assert!(response.starts_with(b"HTTP/1.1 503 Service Unavailable"));
    assert!(String::from_utf8_lossy(&response).contains("codex_model_unavailable"));

    let mut exact_request = codex_request(false);
    exact_request["model"] = json!("claude-csswitch-codex-gpt-lite");
    exact_request["tools"] = json!([{
        "name": "classify_memory",
        "input_schema": {"type": "object"},
    }]);
    exact_request["tool_choice"] = json!({"type": "tool", "name": "classify_memory"});
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_catalog(
            stream,
            &exact_request,
            false,
            InferenceSecrets::for_test("access", "account"),
            &unreachable,
            &catalog,
            |_, _| panic!("invalid Lite request must not reject auth"),
        )
    });
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request"));
    assert!(String::from_utf8_lossy(&response)
        .contains("tool choice is unsupported for Responses Lite"));

    for tool_choice in [json!({"type": "none"}), json!({"type": "required"})] {
        let mut empty_tools_request = codex_request(false);
        empty_tools_request["model"] = json!("claude-csswitch-codex-gpt-lite");
        empty_tools_request["tools"] = json!([]);
        empty_tools_request["tool_choice"] = tool_choice;
        let response = capture_tcp_response(|stream| {
            handle_codex_messages_with_catalog(
                stream,
                &empty_tools_request,
                false,
                InferenceSecrets::for_test("access", "account"),
                &unreachable,
                &catalog,
                |_, _| panic!("invalid Lite request must not reject auth"),
            )
        });
        assert!(response.starts_with(b"HTTP/1.1 400 Bad Request"));
        assert!(String::from_utf8_lossy(&response)
            .contains("tool choice is unsupported for Responses Lite"));
    }

    let mut incompatible_schema_request = codex_request(false);
    incompatible_schema_request["model"] = json!("claude-csswitch-codex-gpt-lite");
    incompatible_schema_request["output_config"] = json!({
        "format": {
            "type": "json_schema",
            "schema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "summary": {"type": "string"},
                },
                "required": ["title"],
                "additionalProperties": false,
            },
        },
    });
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_catalog(
            stream,
            &incompatible_schema_request,
            false,
            InferenceSecrets::for_test("access", "account"),
            &unreachable,
            &catalog,
            |_, _| panic!("incompatible schema must not reject auth"),
        )
    });
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request"));
    assert!(String::from_utf8_lossy(&response)
        .contains("schema is incompatible with OpenAI strict mode"));

    let mut malformed_ref_request = codex_request(false);
    malformed_ref_request["model"] = json!("claude-csswitch-codex-gpt-lite");
    malformed_ref_request["output_config"] = json!({
        "format": {
            "type": "json_schema",
            "schema": {
                "type": "object",
                "properties": {"value": {"$ref": "#/$defs"}},
                "$defs": {"item": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false,
            },
        },
    });
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_catalog(
            stream,
            &malformed_ref_request,
            false,
            InferenceSecrets::for_test("access", "account"),
            &unreachable,
            &catalog,
            |_, _| panic!("malformed ref schema must not reject auth"),
        )
    });
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request"));
    assert!(String::from_utf8_lossy(&response)
        .contains("schema is incompatible with OpenAI strict mode"));

    let mut level_eleven = json!({"type": "string"});
    for _ in 0..9 {
        level_eleven = json!({"type": "array", "items": level_eleven});
    }
    let mut deep_schema_request = codex_request(false);
    deep_schema_request["model"] = json!("claude-csswitch-codex-gpt-lite");
    deep_schema_request["output_config"] = json!({
        "format": {
            "type": "json_schema",
            "schema": {
                "type": "object",
                "properties": {"value": level_eleven},
                "required": ["value"],
                "additionalProperties": false,
            },
        },
    });
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_catalog(
            stream,
            &deep_schema_request,
            false,
            InferenceSecrets::for_test("access", "account"),
            &unreachable,
            &catalog,
            |_, _| panic!("level-eleven schema must not reject auth"),
        )
    });
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request"));
    assert!(String::from_utf8_lossy(&response)
        .contains("schema is incompatible with OpenAI strict mode"));

    let mut unsupported_effort_request = codex_request(false);
    unsupported_effort_request["model"] = json!("claude-csswitch-codex-gpt-lite");
    unsupported_effort_request["output_config"] = json!({"effort": "high"});
    let response = capture_tcp_response(|stream| {
        handle_codex_messages_with_catalog(
            stream,
            &unsupported_effort_request,
            false,
            InferenceSecrets::for_test("access", "account"),
            &unreachable,
            &catalog,
            |_, _| panic!("unsupported effort must not reject auth"),
        )
    });
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request"));
    assert!(String::from_utf8_lossy(&response)
        .contains("output_config.effort is unsupported by the selected Codex model"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn codex_models_response_exposes_science_aliases_and_cache_diagnostics() {
    let models = [
        ("gpt-5.6-sol", "GPT-5.6-Sol"),
        ("gpt-5.6-terra", "GPT-5.6-Terra"),
        ("gpt-5.6-luna", "GPT-5.6-Luna"),
    ]
    .into_iter()
    .enumerate()
    .map(|(priority, (slug, display_name))| {
        json!({
            "slug": slug,
            "display_name": display_name,
            "visibility": "list",
            "supported_in_api": true,
            "priority": priority + 1,
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [{"effort": "medium"}],
            "supports_reasoning_summaries": true,
            "supports_parallel_tool_calls": true,
        })
    })
    .collect();
    let (catalog, models_server, root) = mock_codex_model_catalog_with_models(models);
    let snapshot = catalog
        .list(&InferenceSecrets::for_test("access", "account"))
        .unwrap();
    models_server.join().unwrap();
    let response = capture_tcp_response(|stream| {
        write_codex_models_response(stream, &snapshot);
    });
    let head = String::from_utf8_lossy(
        &response[..response
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap()],
    );
    assert!(head.contains("x-csswitch-model-source: live"));
    assert!(head.contains("x-csswitch-model-age-seconds: 0"));
    let body: Value = serde_json::from_slice(http_body(&response)).unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 3);
    assert_eq!(body["data"][0]["id"], "claude-opus-5");
    assert_eq!(body["data"][0]["display_name"], "Codex / GPT-5.6-Sol");
    assert_eq!(body["data"][0]["reasoning_efforts"], json!(["medium"]));
    assert_eq!(body["data"][0]["default_reasoning_effort"], "medium");
    assert_eq!(body["data"][0]["reviewer_compatible"], true);
    assert_eq!(body["data"][0]["transport"], "responses");
    assert_eq!(body["data"][1]["id"], "claude-sonnet-5");
    assert_eq!(body["data"][1]["display_name"], "Codex / GPT-5.6-Terra");
    assert_eq!(body["data"][2]["id"], "claude-haiku-4-5-20251001");
    assert_eq!(body["data"][2]["display_name"], "Codex / GPT-5.6-Luna");
    assert!(!serde_json::to_string(&body).unwrap().contains("Opus 5"));
    assert!(!serde_json::to_string(&body).unwrap().contains("Sonnet 5"));
    assert!(!body["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["id"] == "claude-opus-4-8"));
    assert_eq!(body["diagnostics"]["source"], "live");
    assert_eq!(body["diagnostics"]["stale"], false);
    assert!(!serde_json::to_string(&body)
        .unwrap()
        .contains("\"id\":\"gpt-5.6-sol\""));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn r0_scratch_models_401_wires_observed_generation_to_guarded_refresh() {
    use std::os::unix::fs::PermissionsExt;

    let listener = bind_loopback();
    let address = listener.local_addr().unwrap();
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_mock_http_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .unwrap();
    });
    let mut random = [0_u8; 8];
    getrandom::getrandom(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!(
        "csswitch-r0-f-scratch-401-{}-{}",
        std::process::id(),
        u64::from_ne_bytes(random)
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let catalog =
        CodexModelCatalog::for_test(format!("http://{address}/models"), root.clone()).unwrap();
    let secrets = InferenceSecrets::for_test("access", "account");
    let observed_generation = secrets.auth_generation();
    let refresh_call = Arc::new(Mutex::new(None));
    let recorded = Arc::clone(&refresh_call);
    let state_root = root.join("auth-root");
    let response = capture_tcp_response(|stream| {
        write_codex_models_with_refresh(
            stream,
            &catalog,
            &secrets,
            true,
            Some(state_root.clone()),
            |root, generation| *recorded.lock().unwrap() = Some((root, generation)),
        )
    });
    upstream.join().unwrap();
    assert!(response.starts_with(b"HTTP/1.1 401 Unauthorized"));
    assert_eq!(
        *refresh_call.lock().unwrap(),
        Some((state_root, observed_generation))
    );
    assert!(!root.join("codex-models-cache.v3.json").exists());
    let epoch: Value = serde_json::from_slice(
        &std::fs::read(root.join("codex-models-cache-epoch.v3.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(epoch["auth_generation"], observed_generation);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn codex_inference_401_and_403_invalidate_catalog_without_repost() {
    for (status, reason, expected_refreshes) in [(401, "Unauthorized", 1), (403, "Forbidden", 0)] {
        let (catalog, models_server, root) = mock_codex_model_catalog(&["gpt-known"]);
        let (transport, posts, requests, upstream) = mock_codex_transport(
            format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .into_bytes(),
        );
        let secrets = InferenceSecrets::for_test("access", "account");
        let auth_epoch = secrets.auth_epoch().to_string();
        let auth_generation = secrets.auth_generation();
        let account_hash = secrets.account_hash().to_string();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let refreshes_for_handler = Arc::clone(&refreshes);
        let mut request = codex_request(false);
        request["model"] = json!("claude-csswitch-codex-gpt-known");
        let response = capture_tcp_response(|stream| {
            handle_codex_messages_with_catalog(
                stream,
                &request,
                false,
                secrets,
                &transport,
                &catalog,
                |rejected_status, _| {
                    catalog.invalidate_identity(&auth_epoch, auth_generation, &account_hash);
                    if rejected_status == 401 {
                        refreshes_for_handler.fetch_add(1, Ordering::SeqCst);
                    }
                },
            )
        });
        models_server.join().unwrap();
        upstream.join().unwrap();
        assert!(response.starts_with(format!("HTTP/1.1 {status}").as_bytes()));
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        assert_eq!(refreshes.load(Ordering::SeqCst), expected_refreshes);
        let upstream_body: Value =
            serde_json::from_slice(http_body(&requests.lock().unwrap()[0])).unwrap();
        assert_eq!(upstream_body["model"], "gpt-known");
        assert!(!root.join("codex-models-cache.v3.json").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn codex_401_refreshes_only_for_next_request_and_never_reposts() {
    let response =
        b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec();
    let (transport, count, _requests, upstream) = mock_codex_transport(response);
    let refreshes = Arc::new(AtomicUsize::new(0));
    let refreshes_for_handler = Arc::clone(&refreshes);
    let downstream = capture_tcp_response(|stream| {
        handle_codex_messages_with_secrets(
            stream,
            &codex_request(false),
            false,
            InferenceSecrets::for_test("access", "account"),
            &transport,
            |status, _| {
                assert_eq!(status, 401);
                refreshes_for_handler.fetch_add(1, Ordering::SeqCst);
            },
        )
    });
    upstream.join().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert!(downstream.starts_with(b"HTTP/1.1 401 Unauthorized"));
}

#[test]
fn codex_429_empty_200_and_interrupted_sse_do_not_retry() {
    let cases = [
            (
                b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
                false,
                "HTTP/1.1 429",
            ),
            (http_sse_response(b""), false, "HTTP/1.1 502"),
            (
                http_sse_response(
                    b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"partial\"}\n\n",
                ),
                true,
                "event: error",
            ),
        ];
    for (response, is_stream, expected) in cases {
        let (transport, count, _requests, upstream) = mock_codex_transport(response);
        let refreshes = Arc::new(AtomicUsize::new(0));
        let refreshes_for_handler = Arc::clone(&refreshes);
        let downstream = capture_tcp_response(|stream| {
            handle_codex_messages_with_secrets(
                stream,
                &codex_request(is_stream),
                is_stream,
                InferenceSecrets::for_test("access", "account"),
                &transport,
                |_, _| {
                    refreshes_for_handler.fetch_add(1, Ordering::SeqCst);
                },
            )
        });
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(refreshes.load(Ordering::SeqCst), 0);
        let text = String::from_utf8_lossy(&downstream);
        assert!(text.contains(expected));
        if is_stream {
            assert_eq!(text.matches("event: error").count(), 1);
        }
    }
}

struct OneReadThenPanic {
    bytes: Option<Vec<u8>>,
    reads: Arc<AtomicUsize>,
}

impl Read for OneReadThenPanic {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read_number = self.reads.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            read_number, 0,
            "downstream cancellation must stop upstream reads"
        );
        let bytes = self.bytes.take().unwrap();
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
}

#[test]
fn codex_downstream_cancellation_stops_before_another_upstream_read() {
    let signer = ThinkingSigner::new(&[9_u8; 32]).unwrap();
    let epoch = "ab".repeat(16);
    let mut reducer = ResponsesReducer::new("gpt", &epoch, "cdcd", &signer);
    let reads = Arc::new(AtomicUsize::new(0));
    let upstream = OneReadThenPanic {
        bytes: Some(
            b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n".to_vec(),
        ),
        reads: Arc::clone(&reads),
    };
    let result = pump_codex_stream(upstream, &mut reducer, |_| {
        Err(Error::new(ErrorKind::BrokenPipe, "client closed"))
    });
    assert_eq!(result, Err(CodexPumpError::DownstreamWrite));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn codex_terminal_event_stops_stream_and_nonstream_before_another_read() {
    let mut bytes = complete_codex_sse();
    bytes.extend_from_slice(b"data: [DONE]\n\n");

    let signer = ThinkingSigner::new(&[9_u8; 32]).unwrap();
    let epoch = "ab".repeat(16);
    let mut stream_reducer = ResponsesReducer::new("gpt", &epoch, "cdcd", &signer);
    let stream_reads = Arc::new(AtomicUsize::new(0));
    let stream_result = pump_codex_stream(
        OneReadThenPanic {
            bytes: Some(bytes.clone()),
            reads: Arc::clone(&stream_reads),
        },
        &mut stream_reducer,
        |_| Ok(()),
    );
    assert_eq!(stream_result, Ok(()));
    assert_eq!(stream_reads.load(Ordering::SeqCst), 1);

    let mut nonstream_reducer = ResponsesReducer::new("gpt", &epoch, "cdcd", &signer);
    let nonstream_reads = Arc::new(AtomicUsize::new(0));
    let result = Arc::new(Mutex::new(None));
    let result_for_handler = Arc::clone(&result);
    let _ = capture_tcp_response(|downstream| {
        *result_for_handler.lock().unwrap() = Some(collect_codex_nonstream(
            OneReadThenPanic {
                bytes: Some(bytes),
                reads: Arc::clone(&nonstream_reads),
            },
            downstream,
            &mut nonstream_reducer,
        ));
    });
    assert_eq!(*result.lock().unwrap(), Some(Ok(())));
    assert_eq!(nonstream_reads.load(Ordering::SeqCst), 1);
}

struct MustNotRead;

impl Read for MustNotRead {
    fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
        panic!("closed nonstream downstream must cancel before upstream read")
    }
}

#[test]
fn codex_nonstream_closed_downstream_cancels_before_upstream_read() {
    let listener = bind_loopback();
    let address = listener.local_addr().unwrap();
    let client = TcpStream::connect(address).unwrap();
    let (downstream, _) = listener.accept().unwrap();
    drop(client);
    thread::sleep(Duration::from_millis(10));

    let signer = ThinkingSigner::new(&[9_u8; 32]).unwrap();
    let epoch = "ab".repeat(16);
    let mut reducer = ResponsesReducer::new("gpt", &epoch, "cdcd", &signer);
    assert_eq!(
        collect_codex_nonstream(MustNotRead, &downstream, &mut reducer),
        Err(CodexNonstreamError::DownstreamClosed)
    );
}

#[test]
fn codex_disconnect_cancels_stalled_upstream_for_stream_and_nonstream() {
    for is_stream in [false, true] {
        let upstream_listener = bind_loopback();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_requests = Arc::new(AtomicUsize::new(0));
        let upstream_requests_for_server = Arc::clone(&upstream_requests);
        let (upstream_ready_tx, upstream_ready_rx) = std::sync::mpsc::channel();
        let (release_upstream_tx, release_upstream_rx) = std::sync::mpsc::channel();
        let upstream = thread::spawn(move || {
            let (mut stream, _) = upstream_listener.accept().unwrap();
            read_mock_http_request(&mut stream);
            upstream_requests_for_server.fetch_add(1, Ordering::SeqCst);
            stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 1048576\r\nconnection: close\r\n\r\n",
                    )
                    .unwrap();
            stream.flush().unwrap();
            upstream_ready_tx.send(()).unwrap();
            let _ = release_upstream_rx.recv_timeout(Duration::from_secs(2));
        });
        let transport =
            CodexTransport::for_test(format!("http://{upstream_address}/responses")).unwrap();

        let downstream_listener = bind_loopback();
        let downstream_address = downstream_listener.local_addr().unwrap();
        let downstream_client = TcpStream::connect(downstream_address).unwrap();
        let (mut downstream, _) = downstream_listener.accept().unwrap();
        let (handler_done_tx, handler_done_rx) = std::sync::mpsc::channel();
        let handler = thread::spawn(move || {
            handle_codex_messages_with_secrets(
                &mut downstream,
                &codex_request(is_stream),
                is_stream,
                InferenceSecrets::for_test("access", "account"),
                &transport,
                |_, _| panic!("cancelled request must not reject auth"),
            );
            handler_done_tx.send(()).unwrap();
        });

        upstream_ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        drop(downstream_client);
        handler_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("downstream disconnect must cancel a stalled upstream read");
        assert_eq!(upstream_requests.load(Ordering::SeqCst), 1);

        release_upstream_tx.send(()).unwrap();
        handler.join().unwrap();
        upstream.join().unwrap();
    }
}

#[test]
fn codex_auth_errors_distinguish_login_from_transient_refresh_failure() {
    let missing = map_codex_auth_error(OAuthFlowError::new(
        OAuthErrorCode::NotAuthenticated,
        false,
        "missing",
    ));
    assert_eq!(missing.status, 401);
    assert_eq!(missing.error_type, "authentication_error");

    let network = map_codex_auth_error(OAuthFlowError::new(
        OAuthErrorCode::OAuthNetwork,
        true,
        "network",
    ));
    assert_eq!(network.status, 503);
    assert_eq!(network.error_type, "api_error");
    assert_ne!(network.message, "Codex login is required");
}

#[test]
fn every_provider_request_body_limit_returns_413_before_allocation() {
    for provider in ["codex", "deepseek", "relay", "openai-custom"] {
        let mut headers = HashMap::new();
        headers.insert(
            "content-length".to_string(),
            (MAX_REQUEST_BYTES + 1).to_string(),
        );
        let head = RequestHead {
            method: "POST".into(),
            target: "/v1/messages".into(),
            headers,
        };
        let cfg = GatewayConfig {
            provider: provider.into(),
            port: 0,
            auth_secret: None,
            api_key: None,
            upstream_url: DEFAULT_CODEX_UPSTREAM_URL.into(),
            models_url: None,
            relay_thinking: None,
            provider_contract: None,
            intent: crate::config::GatewayIntent::Formal,
            static_model_resolver: None,
            shim_mode: "off".into(),
            codex_state_root: None,
            codex_contract: None,
            launch_id: "test".into(),
            skill_data_dir: None,
            skill_bridge_dir: None,
            skill_bridge_token: None,
            skill_authority_fence: None,
            science_host_context: None,
        };
        let response = capture_tcp_response(|stream| {
            handle_post(
                stream,
                &cfg,
                "/v1/messages",
                &head,
                None,
                &RelayModelCache::default(),
                CodexComponents::default(),
            )
        });
        assert!(
            response.starts_with(b"HTTP/1.1 413 Payload Too Large"),
            "provider {provider} must reject before reading or allocating the body"
        );
    }
}

#[cfg(unix)]
#[test]
fn bridge_replay_window_rejects_duplicates_and_prunes_expired_ids() {
    let mut used = HashMap::new();
    assert!(!bridge_request_is_replay(&mut used, "first", 100, 100));
    assert!(bridge_request_is_replay(&mut used, "first", 100, 101));
    assert!(!bridge_request_is_replay(&mut used, "second", 286, 286));
    assert_eq!(used.len(), 1, "expired replay ids must not grow forever");
}

#[cfg(unix)]
fn bridge_temp_dir(label: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::path::PathBuf::from("/private/tmp").join(format!(
        "csswitch-bridge-{label}-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    root
}

#[cfg(unix)]
#[test]
fn bridge_status_has_bounded_heartbeat_contract() {
    let bridge = bridge_temp_dir("status");
    let id = "1".repeat(32);
    let progress = BridgeProgress {
        phase: "download".into(),
        message: "downloading".into(),
        sequence: 4,
    };
    write_bridge_status(&bridge, &id, "install", 100, 1_800, &progress).unwrap();
    let status: Value =
        serde_json::from_slice(&std::fs::read(bridge.join(format!("{id}.status.json"))).unwrap())
            .unwrap();
    assert_eq!(status["status"], "PROCESSING");
    assert_eq!(status["phase"], "download");
    assert_eq!(status["sequence"], 4);
    assert_eq!(status["deadline_at"], 1_900);
    assert_eq!(status["poll_after_seconds"], 3);
    std::fs::remove_dir_all(bridge).unwrap();
}

#[cfg(unix)]
#[test]
fn bridge_finalization_cleans_processing_for_success_failure_and_timeout() {
    let bridge = bridge_temp_dir("finalize");
    for (digit, status) in [
        ('2', "BUNDLE_INSTALLED_ATTACHED"),
        ('3', "INSTALL_FAILED"),
        ('4', "GITHUB_TIMEOUT"),
    ] {
        let id = digit.to_string().repeat(32);
        std::fs::write(bridge.join(format!("{id}.processing")), b"{}").unwrap();
        std::fs::write(bridge.join(format!("{id}.status.json")), b"{}").unwrap();
        finalize_bridge_processing(&bridge, &id, &json!({"status": status})).unwrap();
        assert!(!bridge.join(format!("{id}.processing")).exists());
        assert!(!bridge.join(format!("{id}.status.json")).exists());
        let response: Value = serde_json::from_slice(
            &std::fs::read(bridge.join(format!("{id}.response.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(response["status"], status);
    }
    std::fs::remove_dir_all(bridge).unwrap();
}

#[cfg(unix)]
#[test]
fn duplicate_bridge_id_never_overwrites_first_final_response() {
    let bridge = bridge_temp_dir("duplicate");
    let id = "5".repeat(32);
    assert!(write_bridge_response_once(&bridge, &id, &json!({"status":"FIRST"})).unwrap());
    assert!(!write_bridge_response_once(&bridge, &id, &json!({"status":"SECOND"})).unwrap());
    let response: Value =
        serde_json::from_slice(&std::fs::read(bridge.join(format!("{id}.response.json"))).unwrap())
            .unwrap();
    assert_eq!(response["status"], "FIRST");
    std::fs::remove_dir_all(bridge).unwrap();
}

#[cfg(unix)]
#[test]
fn gateway_restart_recovers_orphaned_processing_to_final_response() {
    let bridge = bridge_temp_dir("recover");
    let id = "6".repeat(32);
    std::fs::write(bridge.join(format!("{id}.processing")), b"{}").unwrap();
    std::fs::write(bridge.join(format!("{id}.status.json")), b"{}").unwrap();
    recover_orphaned_bridge_processing(&bridge).unwrap();
    assert!(!bridge.join(format!("{id}.processing")).exists());
    assert!(!bridge.join(format!("{id}.status.json")).exists());
    let response: Value =
        serde_json::from_slice(&std::fs::read(bridge.join(format!("{id}.response.json"))).unwrap())
            .unwrap();
    assert_eq!(response["status"], "REQUEST_INTERRUPTED");
    assert_eq!(response["retryable"], true);
    assert_eq!(response["request_terminal"], true);
    assert_eq!(response["automatic_retry_allowed"], false);
    assert_eq!(response["attach_state"], "UNKNOWN");
    std::fs::remove_dir_all(bridge).unwrap();
}

#[cfg(unix)]
#[test]
fn bridge_host_lock_allows_only_one_recovery_owner() {
    let bridge = bridge_temp_dir("host-lock");
    let first = acquire_bridge_host_lock(&bridge).unwrap();
    assert!(acquire_bridge_host_lock(&bridge).is_err());
    drop(first);
    let second = acquire_bridge_host_lock(&bridge).unwrap();
    drop(second);
    std::fs::remove_dir_all(bridge).unwrap();
}

#[cfg(unix)]
#[test]
fn bridge_reader_rejects_symlink_and_fifo_without_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::path::PathBuf::from("/private/tmp").join(format!(
        "csswitch-bridge-reader-{}-{suffix}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let regular = root.join("regular");
    std::fs::write(&regular, b"{}").unwrap();
    std::fs::set_permissions(&regular, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_regular_bridge_request(&regular).unwrap(), b"{}");

    let link = root.join("link");
    symlink(&regular, &link).unwrap();
    assert!(read_regular_bridge_request(&link).is_err());

    let fifo = root.join("fifo");
    let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    let started = Instant::now();
    assert!(read_regular_bridge_request(&fifo).is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn request_nonce_generator_is_sequential_and_id_safe() {
    let generator = RequestNonceGenerator::with_prefix([0xab; 16]);
    let first = generator.next_nonce();
    let second = generator.next_nonce();

    assert_eq!(first, format!("{}0000000000000001", "ab".repeat(16)));
    assert_eq!(second, format!("{}0000000000000002", "ab".repeat(16)));
    assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(second.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

#[test]
fn request_nonce_generator_is_unique_under_concurrency() {
    const THREADS: usize = 16;
    const PER_THREAD: usize = 128;

    let generator = Arc::new(RequestNonceGenerator::with_prefix([0x3c; 16]));
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles = (0..THREADS)
        .map(|_| {
            let generator = Arc::clone(&generator);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                (0..PER_THREAD)
                    .map(|_| generator.next_nonce())
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();

    let nonces = handles
        .into_iter()
        .flat_map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    let unique = nonces.iter().collect::<HashSet<_>>();

    assert_eq!(nonces.len(), THREADS * PER_THREAD);
    assert_eq!(unique.len(), nonces.len());
    assert!(nonces.iter().all(|nonce| nonce.len() == 48));
    assert!(nonces.iter().all(|nonce| nonce
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())));
}

impl Read for CountingEofReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        self.reads += 1;
        Ok(0)
    }
}

fn kimi_complete_then_partial() -> Vec<u8> {
    concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"buffered\"}}"
        )
        .as_bytes()
        .to_vec()
}

fn complete_kimi_envelope() -> Vec<u8> {
    concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"plan\",\"signature\":\"opaque\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"name\":\"web_search\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .as_bytes()
        .to_vec()
}

fn dsml_complete_start_then_partial_tool_delta() -> Vec<u8> {
    let start = format!(
        "event: message_start\ndata: {}\n\nevent: content_block_start\ndata: {}\n\n",
        json!({
            "type": "message_start",
            "message": {"id": "m", "type": "message"},
        }),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        })
    );
    let leak = concat!(
        "<｜｜DSML｜｜tool_calls>",
        "<｜｜DSML｜｜invoke name=\"web_search\">",
        "<｜｜DSML｜｜parameter name=\"query\" string=\"true\">cats",
        "</｜｜DSML｜｜parameter>",
        "</｜｜DSML｜｜invoke>",
        "</｜｜DSML｜｜tool_calls>"
    );
    let delta = format!(
        "event: content_block_delta\ndata: {}",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": leak},
        })
    );
    format!("{start}{delta}").into_bytes()
}

fn dsml_filter() -> Option<StreamFilter> {
    let mut tools = Map::<String, Value>::new();
    tools.insert(
        "web_search".to_string(),
        json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
        }),
    );
    Some(StreamFilter::DsmlRewrite(DsmlStreamRewriter::new(
        tools, "test",
    )))
}

#[test]
fn kimi_read_error_is_terminal_and_does_not_finalize_buffer() {
    let first = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\"}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\"}"
    )
    .as_bytes();
    let mut upstream = FailingReader;
    let mut filter = Some(StreamFilter::Kimi(KimiServerToolFilter::new()));
    let mut output = Vec::new();

    let termination = forward_stream_body(&mut upstream, first, &mut filter, |chunk| {
        output.extend_from_slice(chunk);
        Ok(())
    });

    assert_eq!(termination, StreamTermination::UpstreamReadError);
    assert!(String::from_utf8_lossy(&output).contains("event: message_start"));
    assert!(output.ends_with(&stream_error_event("upstream stream read failed")));
    let StreamFilter::Kimi(filter) = filter.as_mut().unwrap() else {
        panic!("expected Kimi filter");
    };
    assert!(filter.finalize().is_err(), "buffer must remain unflushed");
}

#[test]
fn kimi_complete_envelope_preserves_thinking_usage_terminal_and_compacts_indexes() {
    let first = complete_kimi_envelope();
    let mut upstream = Cursor::new(Vec::<u8>::new());
    let mut filter = Some(StreamFilter::Kimi(KimiServerToolFilter::new()));
    let mut output = Vec::new();
    let termination = forward_stream_body(&mut upstream, &first, &mut filter, |chunk| {
        output.extend_from_slice(chunk);
        Ok(())
    });
    let text = String::from_utf8(output).unwrap();
    assert_eq!(termination, StreamTermination::NormalEof);
    assert!(text.contains("\"thinking\":\"plan\""));
    assert!(text.contains("\"signature\":\"opaque\""));
    assert!(text.contains("\"type\":\"server_tool_use\""));
    assert!(text.contains("\"index\":1"));
    assert!(text.contains("\"index\":2"));
    assert!(text.contains("\"stop_reason\":\"end_turn\""));
    assert!(text.contains("\"output_tokens\":9"));
    assert_eq!(text.matches("event: message_stop").count(), 1);
    assert!(!text.contains("event: error"));
}

#[test]
fn kimi_unsigned_thinking_is_dropped_without_failing_stream() {
    let first = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"secret\",\"signature\":\"\"}}\n\n",
        );
    let tail = concat!(
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
    let mut upstream = Cursor::new(tail.as_bytes());
    let mut filter = Some(StreamFilter::Kimi(KimiServerToolFilter::new()));
    let mut output = Vec::new();
    let termination = forward_stream_body(&mut upstream, first.as_bytes(), &mut filter, |chunk| {
        output.extend_from_slice(chunk);
        Ok(())
    });
    let text = String::from_utf8(output).unwrap();
    assert_eq!(termination, StreamTermination::NormalEof);
    assert!(text.contains("event: message_start"));
    assert!(!text.contains("event: error"));
    assert_eq!(text.matches("event: message_stop").count(), 1);
    assert!(!text.contains("secret"));
}

#[test]
fn deepseek_dsml_modes_never_select_or_apply_kimi_rules() {
    let mut tools = Map::new();
    tools.insert("lookup".to_string(), json!({"type": "object"}));
    let base = GatewayConfig {
        provider: "deepseek".into(),
        port: 0,
        auth_secret: Some("test-secret".into()),
        api_key: Some("test-key".into()),
        upstream_url: "https://api.deepseek.com/anthropic/v1/messages".into(),
        models_url: None,
        relay_thinking: None,
        provider_contract: None,
        intent: crate::config::GatewayIntent::Formal,
        static_model_resolver: None,
        shim_mode: "off".into(),
        codex_state_root: None,
        codex_contract: None,
        launch_id: "test".into(),
        skill_data_dir: None,
        skill_bridge_dir: None,
        skill_bridge_token: None,
        skill_authority_fence: None,
        science_host_context: None,
    };
    assert!(dsml_stream_filter(&base, &tools, Some("nonce")).is_none());
    let mut detect = base.clone();
    detect.shim_mode = "detect".into();
    assert!(matches!(
        dsml_stream_filter(&detect, &tools, Some("nonce")),
        Some(StreamFilter::DsmlDetect(_))
    ));
    let mut rewrite = base.clone();
    rewrite.shim_mode = "rewrite".into();
    assert!(matches!(
        dsml_stream_filter(&rewrite, &tools, Some("nonce")),
        Some(StreamFilter::DsmlRewrite(_))
    ));
    let body =
        br#"{"type":"message","content":[{"type":"thinking","thinking":"","signature":""}]}"#
            .to_vec();
    assert_eq!(
        apply_dsml_nonstream(&base, &tools, body.clone(), Some("nonce")),
        body
    );
}

#[test]
fn openai_reasoning_history_survives_loopback_token_rotation_but_not_profile_changes() {
    let base = GatewayConfig {
        provider: "openai-custom".into(),
        port: 0,
        auth_secret: Some("launch-token-a".into()),
        api_key: Some("stable-provider-key".into()),
        upstream_url: "https://example.invalid/v1/chat/completions".into(),
        models_url: None,
        relay_thinking: None,
        provider_contract: None,
        intent: crate::config::GatewayIntent::Formal,
        static_model_resolver: None,
        shim_mode: "off".into(),
        codex_state_root: None,
        codex_contract: None,
        launch_id: "launch-a".into(),
        skill_data_dir: None,
        skill_bridge_dir: None,
        skill_bridge_token: None,
        skill_authority_fence: None,
        science_host_context: None,
    };
    let first_signer = openai_chat_reasoning_signer(&base).unwrap();
    let first = crate::openai_chat::openai_to_anthropic(
        &json!({
            "id": "chatcmpl_restart",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "stable across restart",
                    "tool_calls": [{
                        "id": "call_restart",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"},
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2},
        }),
        "claude-opus-4-8",
        "kimi-k3",
        &first_signer,
    )
    .unwrap();
    let next_request = json!({
        "messages": [
            {"role": "assistant", "content": first["content"]},
            {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "call_restart", "content": "ok",
            }]},
        ],
    });

    let mut restarted = base.clone();
    restarted.auth_secret = Some("launch-token-b".into());
    restarted.launch_id = "launch-b".into();
    let restarted_signer = openai_chat_reasoning_signer(&restarted).unwrap();
    assert!(crate::openai_chat::anthropic_to_openai_custom(
        &next_request,
        "kimi-k3",
        &restarted_signer,
    )
    .is_ok());

    let mut changed_key = restarted.clone();
    changed_key.api_key = Some("different-provider-key".into());
    assert!(crate::openai_chat::anthropic_to_openai_custom(
        &next_request,
        "kimi-k3",
        &openai_chat_reasoning_signer(&changed_key).unwrap(),
    )
    .is_err());
    let mut changed_endpoint = restarted;
    changed_endpoint.upstream_url = "https://other.invalid/v1/chat/completions".into();
    assert!(crate::openai_chat::anthropic_to_openai_custom(
        &next_request,
        "kimi-k3",
        &openai_chat_reasoning_signer(&changed_endpoint).unwrap(),
    )
    .is_err());
}

#[test]
fn dsml_read_error_is_terminal_and_does_not_synthesize_buffered_tool() {
    let first = dsml_complete_start_then_partial_tool_delta();
    let mut upstream = FailingReader;
    let mut filter = dsml_filter();
    let mut output = Vec::new();

    let termination = forward_stream_body(&mut upstream, &first, &mut filter, |chunk| {
        output.extend_from_slice(chunk);
        Ok(())
    });

    let error = stream_error_event("upstream stream read failed");
    assert_eq!(termination, StreamTermination::UpstreamReadError);
    assert!(output.ends_with(&error));
    assert!(!String::from_utf8_lossy(&output).contains("tool_use"));
    let StreamFilter::DsmlRewrite(filter) = filter.as_mut().unwrap() else {
        panic!("expected DSML rewrite filter");
    };
    let withheld = filter.finalize();
    assert!(
        String::from_utf8_lossy(&withheld).contains("tool_use"),
        "the buffered tool must still be present, proving the error path did not finalize it"
    );
}

#[test]
fn incomplete_clean_eof_finalizes_filters_but_fails_protocol() {
    let kimi_first = kimi_complete_then_partial();
    let mut kimi_upstream = Cursor::new(Vec::<u8>::new());
    let mut kimi_filter = Some(StreamFilter::Kimi(KimiServerToolFilter::new()));
    let mut kimi_output = Vec::new();
    let kimi_termination =
        forward_stream_body(&mut kimi_upstream, &kimi_first, &mut kimi_filter, |chunk| {
            kimi_output.extend_from_slice(chunk);
            Ok(())
        });
    assert_eq!(kimi_termination, StreamTermination::ProtocolError);
    assert!(String::from_utf8_lossy(&kimi_output).contains("message_start"));
    assert!(String::from_utf8_lossy(&kimi_output).contains("event: error"));
    assert!(!String::from_utf8_lossy(&kimi_output).contains("message_stop"));

    let dsml_first = dsml_complete_start_then_partial_tool_delta();
    let mut dsml_upstream = Cursor::new(Vec::<u8>::new());
    let mut dsml_filter = dsml_filter();
    let mut dsml_output = Vec::new();
    let dsml_termination =
        forward_stream_body(&mut dsml_upstream, &dsml_first, &mut dsml_filter, |chunk| {
            dsml_output.extend_from_slice(chunk);
            Ok(())
        });
    assert_eq!(dsml_termination, StreamTermination::ProtocolError);
    assert!(String::from_utf8_lossy(&dsml_output).contains("tool_use"));
    assert!(String::from_utf8_lossy(&dsml_output).contains("event: error"));
    assert!(!String::from_utf8_lossy(&dsml_output).contains("message_stop"));
}

#[test]
fn downstream_write_error_stops_before_more_reads_or_finalize() {
    let first = kimi_complete_then_partial();
    let mut upstream = CountingEofReader { reads: 0 };
    let mut filter = Some(StreamFilter::Kimi(KimiServerToolFilter::new()));

    let termination = forward_stream_body(&mut upstream, &first, &mut filter, |_chunk| {
        Err(Error::new(ErrorKind::BrokenPipe, "mock client closed"))
    });

    assert_eq!(termination, StreamTermination::DownstreamWriteError);
    assert_eq!(
        upstream.reads, 0,
        "must stop reading after client write failure"
    );
    let StreamFilter::Kimi(filter) = filter.as_mut().unwrap() else {
        panic!("expected Kimi filter");
    };
    assert!(
        filter.finalize().is_err(),
        "buffer must remain unflushed after client write failure"
    );
}
