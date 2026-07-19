use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::auth::{strip_path_secret, AuthResult};
use crate::config::GatewayConfig;
use crate::{
    anthropic_compat::{self, AnthropicMetadata, KimiServerToolFilter},
    codex_auth, codex_models, codex_protocol, codex_transport, connect,
    dsml_shim::{DsmlDetector, DsmlStreamRewriter},
    messages, models, openai_chat, openai_responses, policy,
};

const BRIDGE_REPLAY_WINDOW_SECONDS: u64 = 185;
const BRIDGE_HEARTBEAT_SECONDS: u64 = 2;

#[cfg(unix)]
#[derive(Clone)]
struct BridgeProgress {
    phase: String,
    message: String,
    sequence: u64,
}

struct RequestHead {
    method: String,
    target: String,
    headers: HashMap<String, String>,
}

#[derive(Clone, Copy, Default)]
struct CodexComponents<'a> {
    transport: Option<&'a codex_transport::CodexTransport>,
    models: Option<&'a codex_models::CodexModelCatalog>,
}

struct RequestNonceGenerator {
    process_prefix: String,
    request_counter: AtomicU64,
}

impl RequestNonceGenerator {
    fn new() -> Result<Self, String> {
        let mut process_prefix = [0_u8; 16];
        getrandom::getrandom(&mut process_prefix)
            .map_err(|e| format!("request nonce random initialization failed: {e}"))?;
        Ok(Self::with_prefix(process_prefix))
    }

    fn with_prefix(process_prefix: [u8; 16]) -> Self {
        let process_prefix = process_prefix
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            process_prefix,
            request_counter: AtomicU64::new(0),
        }
    }

    fn next_nonce(&self) -> String {
        let request_number = self
            .request_counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("request nonce counter exhausted")
            + 1;
        format!("{}{:016x}", self.process_prefix, request_number)
    }
}

enum StreamFilter {
    Kimi(KimiServerToolFilter),
    DsmlDetect(DsmlDetector),
    DsmlRewrite(DsmlStreamRewriter),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamTermination {
    NormalEof,
    UpstreamReadError,
    DownstreamWriteError,
}

impl StreamFilter {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        match self {
            StreamFilter::Kimi(filter) => filter.feed(chunk),
            StreamFilter::DsmlDetect(detector) => {
                detector.feed(chunk);
                chunk.to_vec()
            }
            StreamFilter::DsmlRewrite(rewriter) => rewriter.feed(chunk),
        }
    }

    fn finalize(&mut self) -> Vec<u8> {
        match self {
            StreamFilter::Kimi(filter) => filter.finalize(),
            StreamFilter::DsmlDetect(_) => Vec::new(),
            StreamFilter::DsmlRewrite(rewriter) => rewriter.finalize(),
        }
    }

    fn log_stats(&self) {
        match self {
            StreamFilter::Kimi(filter) if filter.dropped() > 0 => {
                eprintln!(
                    "relay stream rules=tool.kimi.web_search.server-tool-filter dropped={}",
                    filter.dropped()
                );
            }
            StreamFilter::DsmlDetect(detector) if detector.found => {
                eprintln!("deepseek stream DSML detect found=true");
            }
            StreamFilter::DsmlRewrite(rewriter) if rewriter.synthesized => {
                eprintln!("deepseek stream DSML rewrite tool_use={}", rewriter.tool_n);
            }
            _ => {}
        }
    }
}

fn read_head(stream: &mut TcpStream) -> Result<RequestHead, String> {
    let mut buf = Vec::with_capacity(4096);
    let mut byte = [0_u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("empty request".to_string());
        }
        buf.push(byte[0]);
        if buf.len() > 64 * 1024 {
            return Err("request headers too large".to_string());
        }
    }
    let text = std::str::from_utf8(&buf).map_err(|_| "invalid request headers".to_string())?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_string();
    let target = parts.next().ok_or("missing target")?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok(RequestHead {
        method,
        target,
        headers,
    })
}

fn content_length(headers: &HashMap<String, String>) -> Result<usize, String> {
    let Some(raw) = headers.get("content-length") else {
        return Ok(0);
    };
    let parsed = raw
        .parse::<i64>()
        .map_err(|_| "invalid Content-Length".to_string())?;
    if parsed < 0 {
        return Err("invalid Content-Length".to_string());
    }
    Ok(parsed as usize)
}

fn read_body(stream: &mut TcpStream, len: usize) -> Result<Vec<u8>, String> {
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(body)
}

fn json_bytes(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"internal\"}".to_vec())
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn write_json(stream: &mut TcpStream, status: u16, reason: &str, value: Value) {
    let body = json_bytes(value);
    write_response(stream, status, reason, "application/json", &body);
}

fn write_codex_models_response(
    stream: &mut TcpStream,
    snapshot: &codex_models::CodexModelsSnapshot,
) {
    let body = json_bytes(snapshot.response_body());
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nx-csswitch-model-source: {}\r\nx-csswitch-model-age-seconds: {}\r\nconnection: close\r\n\r\n",
        body.len(),
        snapshot.source().as_str(),
        snapshot.age_seconds(),
    );
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

fn typed_error_json(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    error_type: &str,
    message: &str,
) {
    write_json(
        stream,
        status,
        reason,
        json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": message,
            },
        }),
    );
}

fn forbidden_json(stream: &mut TcpStream) {
    typed_error_json(stream, 403, "Forbidden", "permission_error", "forbidden");
}

fn invalid_request_json(stream: &mut TcpStream, detail: &str) {
    typed_error_json(stream, 400, "Bad Request", "invalid_request_error", detail);
}

fn route_unknown_json(stream: &mut TcpStream, detail: &str) {
    typed_error_json(stream, 400, "Bad Request", "route_unknown", detail);
}

fn request_too_large_json(stream: &mut TcpStream) {
    typed_error_json(
        stream,
        413,
        "Payload Too Large",
        "invalid_request_error",
        "request body is too large",
    );
}

fn not_found_json(stream: &mut TcpStream, path: &str) {
    typed_error_json(stream, 404, "Not Found", "not_found_error", path);
}

fn api_error_json(stream: &mut TcpStream, status: u16, detail: &str) {
    typed_error_json(stream, status, status_reason(status), "api_error", detail);
}

fn status_reason(status: u16) -> &'static str {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("Error")
}

fn dequery(path: &str) -> &str {
    path.split_once('?').map(|(p, _)| p).unwrap_or(path)
}

fn models_error_json(
    stream: &mut TcpStream,
    status: u16,
    error_kind: &str,
    upstream_status: Option<u16>,
    message: &str,
) {
    write_json(
        stream,
        status,
        status_reason(status),
        json!({
            "error_kind": error_kind,
            "upstream_status": upstream_status,
            "message": message,
        }),
    );
}

fn handle_get(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    target: &str,
    relay_models: &models::RelayModelCache,
    codex_models: Option<&codex_models::CodexModelCatalog>,
) {
    let path = match strip_path_secret(dequery(target), cfg.auth_secret.as_deref()) {
        AuthResult::Ok(path) => path,
        AuthResult::Forbidden => {
            forbidden_json(stream);
            return;
        }
    };
    match path.as_str() {
        "/health" => {
            let mut health = json!({
                "status": "ok",
                "gateway": "rust",
                "provider": cfg.provider,
                "shim": cfg.shim_mode,
                "launch_id": cfg.launch_id,
                "intent": cfg.intent.as_str(),
            });
            if let Some(resolver) = cfg.static_model_resolver.as_ref() {
                health["catalog_fp"] = Value::String(resolver.catalog_fp().to_string());
            }
            if let Some(contract) = cfg.codex_contract.as_ref() {
                let object = health
                    .as_object_mut()
                    .expect("health response is an object");
                object.insert(
                    "provider_contract_id".into(),
                    Value::String(contract.contract_id.clone()),
                );
                object.insert(
                    "provider_contract_digest".into(),
                    Value::String(contract.catalog_digest.clone()),
                );
            }
            write_json(stream, 200, "OK", health)
        }
        "/v1/models"
            if cfg.provider != "codex"
                && cfg.intent != crate::config::GatewayIntent::ScratchModels =>
        {
            let Some(resolver) = cfg.static_model_resolver.as_ref() else {
                typed_error_json(
                    stream,
                    503,
                    "Service Unavailable",
                    "catalog_unavailable",
                    "static model catalog is unavailable",
                );
                return;
            };
            write_json(stream, 200, "OK", resolver.models_response())
        }
        "/v1/models" if cfg.provider == "qwen" => {
            write_json(stream, 200, "OK", models::qwen_models_response())
        }
        "/v1/models" if cfg.provider == "codex" => {
            let Some(catalog) = codex_models else {
                models_error_json(
                    stream,
                    502,
                    "internal",
                    None,
                    "Codex model catalog is unavailable",
                );
                return;
            };
            let secrets = match load_codex_inference_secrets(cfg) {
                Ok(secrets) => secrets,
                Err(error) => {
                    typed_error_json(
                        stream,
                        error.status,
                        status_reason(error.status),
                        error.error_type,
                        error.message,
                    );
                    return;
                }
            };
            let snapshot = if cfg.intent == crate::config::GatewayIntent::ScratchModels {
                catalog.refresh_published_snapshot(&secrets)
            } else {
                catalog.published_snapshot(&secrets)
            };
            match snapshot {
                Ok(snapshot) => write_codex_models_response(stream, &snapshot),
                Err(error) => {
                    if error.upstream_status == Some(401) {
                        if let Some(root) = cfg.codex_state_root.clone() {
                            let _ = codex_auth::refresh_production_for_generation(
                                root,
                                secrets.auth_generation(),
                            );
                        }
                    }
                    models_error_json(
                        stream,
                        error.status,
                        error.error_kind,
                        error.upstream_status,
                        error.detail,
                    );
                }
            }
        }
        "/v1/models"
            if cfg.provider == "openai-custom"
                || cfg.provider == "openai-responses"
                || cfg.provider == "relay" =>
        {
            let Some(models_url) = cfg.models_url.as_deref() else {
                models_error_json(stream, 502, "network", None, "missing models URL");
                return;
            };
            match messages::get(cfg, models_url) {
                Ok(resp) => match serde_json::from_slice::<Value>(&resp.body) {
                    Ok(raw) => {
                        let (body, ids) = models::normalize_live_models(&raw);
                        relay_models.update_from_live_models(&cfg.provider, &ids);
                        write_json(stream, 200, "OK", body);
                    }
                    Err(e) => models_error_json(
                        stream,
                        502,
                        "protocol",
                        None,
                        &format!("upstream models JSON parse failed: {e}"),
                    ),
                },
                Err(e) => models_error_json(
                    stream,
                    e.status,
                    if e.upstream_status.is_some() {
                        "upstream"
                    } else {
                        "network"
                    },
                    e.upstream_status,
                    &e.detail,
                ),
            }
        }
        "/v1/models" => write_json(stream, 200, "OK", models::deepseek_models_response()),
        _ => not_found_json(stream, &path),
    }
}

fn write_chunk(stream: &mut TcpStream, chunk: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:x}\r\n", chunk.len())?;
    stream.write_all(chunk)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn stream_error_event(detail: &str) -> Vec<u8> {
    format!(
        "event: error\ndata: {}\n\n",
        json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": detail,
            },
        })
    )
    .into_bytes()
}

fn sse_event(event: &str, data: &Value) -> Vec<u8> {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string())
    )
    .into_bytes()
}

fn forward_stream_body<R, F>(
    upstream: &mut R,
    first: &[u8],
    filter: &mut Option<StreamFilter>,
    mut emit: F,
) -> StreamTermination
where
    R: Read,
    F: FnMut(&[u8]) -> std::io::Result<()>,
{
    let first = if let Some(filter) = filter.as_mut() {
        filter.feed(first)
    } else {
        first.to_vec()
    };
    if !first.is_empty() && emit(&first).is_err() {
        return StreamTermination::DownstreamWriteError;
    }

    let mut buf = [0_u8; 8192];
    loop {
        match upstream.read(&mut buf) {
            Ok(0) => {
                if let Some(filter) = filter.as_mut() {
                    let tail = filter.finalize();
                    if !tail.is_empty() && emit(&tail).is_err() {
                        return StreamTermination::DownstreamWriteError;
                    }
                }
                return StreamTermination::NormalEof;
            }
            Ok(n) => {
                let chunk = if let Some(filter) = filter.as_mut() {
                    filter.feed(&buf[..n])
                } else {
                    buf[..n].to_vec()
                };
                if !chunk.is_empty() && emit(&chunk).is_err() {
                    return StreamTermination::DownstreamWriteError;
                }
            }
            Err(e) => {
                if emit(&stream_error_event(&e.to_string())).is_err() {
                    return StreamTermination::DownstreamWriteError;
                }
                return StreamTermination::UpstreamReadError;
            }
        }
    }
}

fn handle_stream(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    body: Vec<u8>,
    mut filter: Option<StreamFilter>,
) {
    if write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
    )
    .and_then(|_| stream.flush())
    .is_err()
    {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let cfg_for_open = cfg.clone();
    thread::spawn(move || {
        let _ = tx.send(messages::open_stream(&cfg_for_open, body));
    });
    let upstream = loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if write_chunk(stream, b": csswitch-keepalive\n\n").is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(messages::UpstreamError {
                    status: 502,
                    upstream_status: None,
                    detail: "upstream stream failed".to_string(),
                });
            }
        }
    };
    match upstream {
        Ok(mut upstream) => {
            let termination = forward_stream_body(
                &mut upstream.response,
                &upstream.first,
                &mut filter,
                |chunk| write_chunk(stream, chunk),
            );
            match termination {
                StreamTermination::NormalEof => {
                    if let Some(filter) = filter.as_ref() {
                        filter.log_stats();
                    }
                }
                StreamTermination::UpstreamReadError => {}
                StreamTermination::DownstreamWriteError => return,
            }
        }
        Err(e) => {
            if write_chunk(stream, &stream_error_event(&e.detail)).is_err() {
                return;
            }
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

fn log_relay_metadata(metadata: &AnthropicMetadata, is_stream: bool, message_count: usize) {
    let rules = if metadata.rule_ids.is_empty() {
        "-".to_string()
    } else {
        metadata.rule_ids.join(",")
    };
    eprintln!(
        "POST /v1/messages relay target={} stream={} msgs={} rules={}",
        metadata.target_model, is_stream, message_count, rules
    );
}

fn log_responses_metadata(
    transformed: &Value,
    metadata: &openai_responses::ResponsesMetadata,
    is_stream: bool,
) {
    let rules = if metadata.rule_ids.is_empty() {
        "-".to_string()
    } else {
        metadata.rule_ids.join(",")
    };
    let target_model = transformed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("");
    let input_count = transformed
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let tool_count = transformed
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    eprintln!(
        "POST /v1/messages provider=openai-responses target={} stream={} input={} tools={} rules={}",
        target_model, is_stream, input_count, tool_count, rules
    );
}

fn known_tools_from_request(raw: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(tools) = raw.get("tools").and_then(Value::as_array) else {
        return out;
    };
    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let schema = tool
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({}));
        out.insert(name.to_string(), schema);
    }
    out
}

fn dsml_stream_filter(
    cfg: &GatewayConfig,
    known_tools: &Map<String, Value>,
    request_nonce: Option<&str>,
) -> Option<StreamFilter> {
    if cfg.provider != "deepseek" || known_tools.is_empty() {
        return None;
    }
    match cfg.shim_mode.as_str() {
        "detect" => Some(StreamFilter::DsmlDetect(DsmlDetector::new())),
        "rewrite" => request_nonce.map(|nonce| {
            StreamFilter::DsmlRewrite(DsmlStreamRewriter::new(known_tools.clone(), nonce))
        }),
        _ => None,
    }
}

fn apply_dsml_nonstream(
    cfg: &GatewayConfig,
    known_tools: &Map<String, Value>,
    body: Vec<u8>,
    request_nonce: Option<&str>,
) -> Vec<u8> {
    if cfg.provider != "deepseek" || known_tools.is_empty() {
        return body;
    }
    match cfg.shim_mode.as_str() {
        "detect" => {
            let mut detector = DsmlDetector::new();
            detector.feed(&body);
            if detector.found {
                eprintln!("deepseek nonstream DSML detect found=true");
            }
            body
        }
        "rewrite" => {
            let Some(request_nonce) = request_nonce else {
                return body;
            };
            let rewritten =
                crate::dsml_shim::rewrite_nonstream_body(&body, known_tools, request_nonce);
            if rewritten != body {
                eprintln!("deepseek nonstream DSML rewrite applied=true");
            }
            rewritten
        }
        _ => body,
    }
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(i64::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CodexAuthLoadError {
    status: u16,
    error_type: &'static str,
    message: &'static str,
}

fn map_codex_auth_error(error: codex_auth::OAuthFlowError) -> CodexAuthLoadError {
    if error.code == codex_auth::OAuthErrorCode::NotAuthenticated {
        CodexAuthLoadError {
            status: 401,
            error_type: "authentication_error",
            message: "Codex login is required",
        }
    } else if error.retryable {
        CodexAuthLoadError {
            status: 503,
            error_type: "api_error",
            message: "Codex authentication is temporarily unavailable",
        }
    } else {
        CodexAuthLoadError {
            status: 500,
            error_type: "api_error",
            message: "Codex authentication state is unavailable",
        }
    }
}

fn load_codex_inference_secrets(
    cfg: &GatewayConfig,
) -> Result<codex_auth::InferenceSecrets, CodexAuthLoadError> {
    let state_root = cfg.codex_state_root.clone().ok_or(CodexAuthLoadError {
        status: 500,
        error_type: "api_error",
        message: "Codex auth state root is unavailable",
    })?;
    let mut secrets = codex_auth::production_inference_snapshot(state_root.clone())
        .map_err(map_codex_auth_error)?;
    if secrets
        .expires_at()
        .is_some_and(|expires_at| expires_at <= unix_time_seconds())
    {
        let generation = secrets.auth_generation();
        codex_auth::refresh_production_for_generation(state_root.clone(), generation)
            .map_err(map_codex_auth_error)?;
        secrets =
            codex_auth::production_inference_snapshot(state_root).map_err(map_codex_auth_error)?;
    }
    Ok(secrets)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexPumpError {
    UpstreamRead,
    Protocol,
    DownstreamWrite,
    Cancelled,
}

fn emit_codex_events<F>(
    reducer: &mut codex_protocol::ResponsesReducer<'_>,
    events: Vec<Value>,
    emit: &mut F,
) -> Result<bool, CodexPumpError>
where
    F: FnMut(&[u8]) -> std::io::Result<()>,
{
    for event in events {
        for translated in reducer.apply(event).map_err(|_| CodexPumpError::Protocol)? {
            emit(&sse_event(translated.event, &translated.data))
                .map_err(|_| CodexPumpError::DownstreamWrite)?;
        }
        if reducer.is_complete() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn pump_codex_stream<R, F>(
    mut upstream: R,
    reducer: &mut codex_protocol::ResponsesReducer<'_>,
    mut emit: F,
) -> Result<(), CodexPumpError>
where
    R: Read,
    F: FnMut(&[u8]) -> std::io::Result<()>,
{
    let mut decoder = codex_protocol::SseDecoder::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match upstream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let mut offset = 0;
                while offset < read {
                    let (event, consumed) = decoder
                        .feed_next(&buffer[offset..read])
                        .map_err(|_| CodexPumpError::Protocol)?;
                    offset += consumed;
                    if let Some(event) = event {
                        if emit_codex_events(reducer, vec![event], &mut emit)? {
                            return Ok(());
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionAborted => {
                return Err(CodexPumpError::Cancelled);
            }
            Err(_) => return Err(CodexPumpError::UpstreamRead),
        }
    }
    let tail = decoder.finish().map_err(|_| CodexPumpError::Protocol)?;
    if emit_codex_events(reducer, tail, &mut emit)? {
        return Ok(());
    }
    reducer
        .finish_stream()
        .map_err(|_| CodexPumpError::Protocol)
}

fn finish_codex_stream_error(stream: &mut TcpStream) {
    let _ = write_chunk(stream, &stream_error_event("Codex upstream protocol error"));
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

fn forward_codex_stream<R: Read>(
    stream: &mut TcpStream,
    mut upstream: R,
    reducer: &mut codex_protocol::ResponsesReducer<'_>,
) {
    if write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
    )
    .and_then(|_| stream.flush())
    .is_err()
    {
        return;
    }
    match pump_codex_stream(&mut upstream, reducer, |chunk| write_chunk(stream, chunk)) {
        Ok(()) => {}
        Err(CodexPumpError::DownstreamWrite | CodexPumpError::Cancelled) => return,
        Err(CodexPumpError::UpstreamRead | CodexPumpError::Protocol) => {
            finish_codex_stream_error(stream);
            return;
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexNonstreamError {
    UpstreamRead,
    Protocol,
    DownstreamClosed,
}

#[cfg(unix)]
fn downstream_closed(stream: &TcpStream) -> bool {
    use std::os::fd::AsRawFd;

    let mut byte = [0_u8; 1];
    let result = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            byte.as_mut_ptr().cast(),
            byte.len(),
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if result == 0 {
        true
    } else if result > 0 {
        false
    } else {
        !matches!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        )
    }
}

#[cfg(not(unix))]
fn downstream_closed(_stream: &TcpStream) -> bool {
    false
}

struct DownstreamCancellationWatch {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl DownstreamCancellationWatch {
    fn start(
        stream: &TcpStream,
        cancellation: codex_transport::CodexCancellation,
    ) -> std::io::Result<Self> {
        let downstream = stream.try_clone()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Acquire) {
                if downstream_closed(&downstream) {
                    cancellation.cancel();
                    return;
                }
                thread::sleep(Duration::from_millis(25));
            }
        });
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for DownstreamCancellationWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn collect_codex_nonstream<R: Read>(
    mut upstream: R,
    downstream: &TcpStream,
    reducer: &mut codex_protocol::ResponsesReducer<'_>,
) -> Result<(), CodexNonstreamError> {
    let mut decoder = codex_protocol::SseDecoder::new();
    let mut buffer = [0_u8; 8192];
    loop {
        if downstream_closed(downstream) {
            return Err(CodexNonstreamError::DownstreamClosed);
        }
        match upstream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let mut offset = 0;
                while offset < read {
                    let (event, consumed) = decoder
                        .feed_next(&buffer[offset..read])
                        .map_err(|_| CodexNonstreamError::Protocol)?;
                    offset += consumed;
                    if let Some(event) = event {
                        reducer
                            .apply(event)
                            .map_err(|_| CodexNonstreamError::Protocol)?;
                        if reducer.is_complete() {
                            return Ok(());
                        }
                    }
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::ConnectionAborted
                    || downstream_closed(downstream) =>
            {
                return Err(CodexNonstreamError::DownstreamClosed);
            }
            Err(_) => return Err(CodexNonstreamError::UpstreamRead),
        }
    }
    for event in decoder
        .finish()
        .map_err(|_| CodexNonstreamError::Protocol)?
    {
        reducer
            .apply(event)
            .map_err(|_| CodexNonstreamError::Protocol)?;
        if reducer.is_complete() {
            return Ok(());
        }
    }
    reducer
        .finish_stream()
        .map_err(|_| CodexNonstreamError::Protocol)
}

#[derive(Clone, Copy, Debug, Default)]
struct CodexRequestPolicy<'a> {
    reasoning_effort: Option<&'a str>,
    supports_reasoning_summary: bool,
    supports_parallel_tool_calls: bool,
    use_responses_lite: bool,
}

fn handle_codex_messages_with_policy(
    stream: &mut TcpStream,
    raw: &Value,
    is_stream: bool,
    secrets: codex_auth::InferenceSecrets,
    transport: &codex_transport::CodexTransport,
    policy: CodexRequestPolicy<'_>,
    mut auth_rejected: impl FnMut(u16, u64),
) {
    let target_model = match raw.get("model").and_then(Value::as_str) {
        Some(model) if !model.trim().is_empty() => model,
        _ => {
            invalid_request_json(stream, "model is required for Codex");
            return;
        }
    };
    let signer = match codex_protocol::ThinkingSigner::new(secrets.thinking_key()) {
        Ok(signer) => signer,
        Err(_) => {
            api_error_json(stream, 500, "Codex thinking key is unavailable");
            return;
        }
    };
    let context = codex_protocol::RequestContext {
        target_model,
        auth_epoch: secrets.auth_epoch(),
        account_hash: secrets.account_hash(),
        reasoning_effort: policy.reasoning_effort,
        supports_reasoning_summary: policy.supports_reasoning_summary,
        supports_parallel_tool_calls: policy.supports_parallel_tool_calls,
        use_responses_lite: policy.use_responses_lite,
    };
    let translated = match codex_protocol::translate_anthropic_request(raw, &context, &signer) {
        Ok(translated) => translated,
        Err(error) => {
            if error.kind == codex_protocol::ProtocolErrorKind::Bounds {
                request_too_large_json(stream);
            } else {
                invalid_request_json(stream, error.detail);
            }
            return;
        }
    };
    let body = match serde_json::to_vec(&translated) {
        Ok(body) => body,
        Err(_) => {
            invalid_request_json(stream, "Codex request encoding failed");
            return;
        }
    };
    let generation = secrets.auth_generation();
    let cancellation = codex_transport::CodexCancellation::default();
    let _cancellation_watch = match DownstreamCancellationWatch::start(stream, cancellation.clone())
    {
        Ok(watch) => watch,
        Err(_) => {
            api_error_json(stream, 500, "Codex cancellation monitor is unavailable");
            return;
        }
    };
    let upstream =
        match transport.open_responses(&secrets, body, policy.use_responses_lite, cancellation) {
            Ok(upstream) => upstream,
            Err(error) => {
                if error.cancelled {
                    return;
                }
                if let Some(status @ (401 | 403)) = error.upstream_status {
                    auth_rejected(status, generation);
                }
                api_error_json(stream, error.status, error.detail);
                return;
            }
        };
    let mut reducer = codex_protocol::ResponsesReducer::new(
        target_model,
        secrets.auth_epoch(),
        secrets.account_hash(),
        &signer,
    );
    if is_stream {
        forward_codex_stream(stream, upstream, &mut reducer);
    } else {
        match collect_codex_nonstream(upstream, stream, &mut reducer) {
            Err(CodexNonstreamError::DownstreamClosed) => {}
            Err(CodexNonstreamError::UpstreamRead | CodexNonstreamError::Protocol) => {
                api_error_json(stream, 502, "Codex upstream protocol error");
            }
            Ok(()) => match reducer.nonstream_response() {
                Ok(response) => write_json(stream, 200, "OK", response),
                Err(_) => api_error_json(stream, 502, "Codex upstream protocol error"),
            },
        }
    }
}

#[cfg(test)]
fn handle_codex_messages_with_secrets(
    stream: &mut TcpStream,
    raw: &Value,
    is_stream: bool,
    secrets: codex_auth::InferenceSecrets,
    transport: &codex_transport::CodexTransport,
    auth_rejected: impl FnMut(u16, u64),
) {
    handle_codex_messages_with_policy(
        stream,
        raw,
        is_stream,
        secrets,
        transport,
        CodexRequestPolicy::default(),
        auth_rejected,
    );
}

fn handle_codex_messages(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    raw: &Value,
    is_stream: bool,
    transport: &codex_transport::CodexTransport,
    catalog: &codex_models::CodexModelCatalog,
) {
    let secrets = match load_codex_inference_secrets(cfg) {
        Ok(secrets) => secrets,
        Err(error) => {
            typed_error_json(
                stream,
                error.status,
                status_reason(error.status),
                error.error_type,
                error.message,
            );
            return;
        }
    };
    let auth_epoch = secrets.auth_epoch().to_string();
    let auth_generation = secrets.auth_generation();
    let account_hash = secrets.account_hash().to_string();
    let state_root = cfg.codex_state_root.clone();
    handle_codex_messages_with_catalog(
        stream,
        raw,
        is_stream,
        secrets,
        transport,
        catalog,
        move |status, generation| {
            catalog.invalidate_identity(&auth_epoch, auth_generation, &account_hash);
            if status == 401 {
                if let Some(root) = state_root.clone() {
                    let _ = codex_auth::refresh_production_for_generation(root, generation);
                }
            }
        },
    );
}

fn handle_codex_messages_with_catalog(
    stream: &mut TcpStream,
    raw: &Value,
    is_stream: bool,
    secrets: codex_auth::InferenceSecrets,
    transport: &codex_transport::CodexTransport,
    catalog: &codex_models::CodexModelCatalog,
    mut auth_rejected: impl FnMut(u16, u64),
) {
    let requested_model = match raw.get("model").and_then(Value::as_str) {
        Some(model) if !model.trim().is_empty() => model,
        _ => {
            route_unknown_json(stream, "model selector is required for Codex");
            return;
        }
    };
    let snapshot = match catalog.published_snapshot(&secrets) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            if error.upstream_status == Some(401) {
                auth_rejected(401, secrets.auth_generation());
            }
            models_error_json(
                stream,
                error.status,
                error.error_kind,
                error.upstream_status,
                error.detail,
            );
            return;
        }
    };
    let Some(target_model) = snapshot.resolve_science_model(requested_model) else {
        route_unknown_json(
            stream,
            "model selector is not available for this Codex account",
        );
        return;
    };
    let mut mapped_request = raw.clone();
    mapped_request["model"] = Value::String(target_model.raw_id().to_string());
    handle_codex_messages_with_policy(
        stream,
        &mapped_request,
        is_stream,
        secrets,
        transport,
        CodexRequestPolicy {
            reasoning_effort: target_model.default_reasoning_effort(),
            supports_reasoning_summary: target_model.supports_reasoning_summary(),
            supports_parallel_tool_calls: target_model.supports_parallel_tool_calls(),
            use_responses_lite: target_model.use_responses_lite(),
        },
        auth_rejected,
    );
}

fn handle_messages(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    body: Vec<u8>,
    request_nonces: Option<&RequestNonceGenerator>,
    _relay_models: &models::RelayModelCache,
    codex: CodexComponents<'_>,
) {
    let raw: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            invalid_request_json(stream, &e.to_string());
            return;
        }
    };
    if !raw.is_object() {
        invalid_request_json(stream, "request body must be a JSON object");
        return;
    }
    let known_tools = known_tools_from_request(&raw);
    let is_stream = raw.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if cfg.provider == "codex" {
        let Some(transport) = codex.transport else {
            api_error_json(stream, 502, "Codex transport is unavailable");
            return;
        };
        let Some(catalog) = codex.models else {
            api_error_json(stream, 502, "Codex model catalog is unavailable");
            return;
        };
        handle_codex_messages(stream, cfg, &raw, is_stream, transport, catalog);
        return;
    }
    if cfg.intent == crate::config::GatewayIntent::ScratchModels {
        route_unknown_json(stream, "scratch-models does not accept inference requests");
        return;
    }
    let requested_model = match raw.get("model").and_then(Value::as_str) {
        Some(model) if !model.trim().is_empty() => model,
        _ => {
            route_unknown_json(stream, "model selector is required");
            return;
        }
    };
    let Some(resolver) = cfg.static_model_resolver.as_ref() else {
        route_unknown_json(stream, "static model catalog is unavailable");
        return;
    };
    let Some(resolved_route) = resolver.resolve(requested_model) else {
        route_unknown_json(
            stream,
            "model selector is not present in the active profile",
        );
        return;
    };
    let target_model = resolved_route.upstream_model().to_string();
    let dsml_request_nonce =
        (cfg.provider == "deepseek" && cfg.shim_mode == "rewrite" && !known_tools.is_empty())
            .then(|| request_nonces.map(RequestNonceGenerator::next_nonce))
            .flatten();
    if cfg.provider == "qwen"
        || cfg.provider == "openai-custom"
        || cfg.provider == "openai-responses"
    {
        let model_id = raw
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("claude-sonnet-5")
            .to_string();
        let transformed = if cfg.provider == "openai-responses" {
            openai_responses::anthropic_to_openai(
                &raw,
                &target_model,
                openai_responses::is_dashscope_responses_endpoint(&cfg.provider, &cfg.upstream_url),
            )
            .map(|(body, metadata)| (body, Some(metadata)))
        } else if cfg.provider == "openai-custom" {
            openai_chat::anthropic_to_openai_custom(&raw, &target_model).map(|body| (body, None))
        } else {
            openai_chat::anthropic_to_openai(&raw, &target_model).map(|body| (body, None))
        };
        let (transformed, responses_metadata) = match transformed {
            Ok(result) => result,
            Err(e) => {
                invalid_request_json(stream, &e);
                return;
            }
        };
        let body = match serde_json::to_vec(&transformed) {
            Ok(body) => body,
            Err(e) => {
                invalid_request_json(stream, &e.to_string());
                return;
            }
        };
        if let Some(metadata) = responses_metadata.as_ref() {
            log_responses_metadata(&transformed, metadata, is_stream);
        }
        match messages::post_nonstream(cfg, body) {
            Ok(resp) => {
                let openai_resp: Value = match serde_json::from_slice(&resp.body) {
                    Ok(v) => v,
                    Err(e) => {
                        api_error_json(stream, 502, &e.to_string());
                        return;
                    }
                };
                let anthropic_resp = if cfg.provider == "openai-responses" {
                    openai_responses::openai_to_anthropic(&openai_resp, &model_id)
                } else {
                    openai_chat::openai_to_anthropic(&openai_resp, &model_id)
                };
                if is_stream {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
                    );
                    for (event, data) in openai_chat::replay_as_sse_events(&anthropic_resp) {
                        if write_chunk(stream, &sse_event(&event, &data)).is_err() {
                            return;
                        }
                    }
                    let _ = stream.write_all(b"0\r\n\r\n");
                    let _ = stream.flush();
                } else {
                    write_json(stream, 200, "OK", anthropic_resp);
                }
            }
            Err(e) => api_error_json(stream, e.status, &e.detail),
        }
        return;
    }
    if cfg.provider == "relay" {
        let (transformed, metadata) = match anthropic_compat::transform_relay_request(
            raw,
            &target_model,
            cfg.relay_thinking.as_deref(),
            &cfg.upstream_url,
        ) {
            Ok(result) => result,
            Err(e) => {
                invalid_request_json(stream, &e);
                return;
            }
        };
        let message_count = transformed
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        log_relay_metadata(&metadata, is_stream, message_count);
        let transformed = match serde_json::to_vec(&transformed) {
            Ok(body) => body,
            Err(e) => {
                invalid_request_json(stream, &e.to_string());
                return;
            }
        };
        if is_stream {
            let filter = if metadata.target_model.to_ascii_lowercase().contains("kimi") {
                Some(StreamFilter::Kimi(KimiServerToolFilter::new()))
            } else {
                None
            };
            handle_stream(stream, cfg, transformed, filter);
            return;
        }
        match messages::post_nonstream(cfg, transformed) {
            Ok(resp) => write_response(
                stream,
                resp.status,
                status_reason(resp.status),
                &resp.content_type,
                &resp.body,
            ),
            Err(e) => api_error_json(stream, e.status, &e.detail),
        }
        return;
    }
    let transformed = match policy::transform_request(raw, &target_model) {
        Ok(body) => body,
        Err(e) => {
            invalid_request_json(stream, &e);
            return;
        }
    };
    if is_stream {
        let filter = dsml_stream_filter(cfg, &known_tools, dsml_request_nonce.as_deref());
        handle_stream(stream, cfg, transformed, filter);
        return;
    }
    match messages::post_nonstream(cfg, transformed) {
        Ok(resp) => {
            let body =
                apply_dsml_nonstream(cfg, &known_tools, resp.body, dsml_request_nonce.as_deref());
            write_response(
                stream,
                resp.status,
                status_reason(resp.status),
                &resp.content_type,
                &body,
            )
        }
        Err(e) => api_error_json(stream, e.status, &e.detail),
    }
}

fn handle_post(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    target: &str,
    head: &RequestHead,
    request_nonces: Option<&RequestNonceGenerator>,
    relay_models: &models::RelayModelCache,
    codex: CodexComponents<'_>,
) {
    let path = match strip_path_secret(dequery(target), cfg.auth_secret.as_deref()) {
        AuthResult::Ok(path) => path,
        AuthResult::Forbidden => {
            forbidden_json(stream);
            return;
        }
    };
    let len = match content_length(&head.headers) {
        Ok(len) => len,
        Err(e) => {
            invalid_request_json(stream, &e);
            return;
        }
    };
    if cfg.provider == "codex" && codex_protocol::validate_request_body_size(len).is_err() {
        request_too_large_json(stream);
        return;
    }
    let body = if len == 0 {
        b"{}".to_vec()
    } else {
        match read_body(stream, len) {
            Ok(body) => body,
            Err(e) => {
                invalid_request_json(stream, &e);
                return;
            }
        }
    };
    if path != "/v1/messages" {
        not_found_json(stream, &path);
        return;
    }
    handle_messages(stream, cfg, body, request_nonces, relay_models, codex);
}

#[cfg(unix)]
fn valid_bridge_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(unix)]
fn acquire_bridge_host_lock(bridge: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let lock_path = bridge.join(".csswitch-host.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&lock_path)
        .map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("refusing unsafe Skill bridge host lock".into());
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Skill bridge already has an active host".into());
    }
    Ok(file)
}

#[cfg(unix)]
fn bridge_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(unix)]
fn bridge_operation_timeout(operation: &str) -> u64 {
    if operation == "install" {
        crate::skill_install::BRIDGE_INSTALL_RESPONSE_TIMEOUT_SECONDS
    } else {
        60
    }
}

#[cfg(unix)]
fn write_bridge_status(
    bridge: &std::path::Path,
    id: &str,
    operation: &str,
    started_at: u64,
    timeout_seconds: u64,
    progress: &BridgeProgress,
) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    let now = bridge_now();
    let payload = json!({
        "schema_version": csswitch_skill_install_core::SCHEMA_VERSION,
        "status": "PROCESSING",
        "request_id": id,
        "operation": operation,
        "phase": progress.phase,
        "message": progress.message,
        "sequence": progress.sequence,
        "started_at": started_at,
        "updated_at": now,
        "elapsed_seconds": now.saturating_sub(started_at),
        "timeout_seconds": timeout_seconds,
        "deadline_at": started_at.saturating_add(timeout_seconds),
        "terminal_grace_seconds": 5,
        "poll_after_seconds": 3,
        "response_filename": format!("{id}.response.json")
    });
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = bridge.join(format!(
        ".{id}.status.{}.{}.{}.tmp",
        std::process::id(),
        progress.sequence,
        nonce
    ));
    let target = bridge.join(format!("{id}.status.json"));
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut file, &payload).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    std::fs::rename(&temp, &target).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        error.to_string()
    })
}

#[cfg(unix)]
fn bridge_final_response_exists(path: &std::path::Path) -> Result<bool, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o077 == 0 =>
        {
            Ok(true)
        }
        Ok(_) => Err("invalid existing Skill bridge response".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(unix)]
fn write_bridge_response_once(
    bridge: &std::path::Path,
    id: &str,
    response: &Value,
) -> Result<bool, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let target = bridge.join(format!("{id}.response.json"));
    if bridge_final_response_exists(&target)? {
        return Ok(false);
    }
    let temp = bridge.join(format!(".{id}.response.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut file, response).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    let linked = match std::fs::hard_link(&temp, &target) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            return Err(error.to_string());
        }
    };
    std::fs::remove_file(&temp).map_err(|error| error.to_string())?;
    Ok(linked)
}

#[cfg(unix)]
fn cleanup_bridge_processing(bridge: &std::path::Path, id: &str) {
    let _ = std::fs::remove_file(bridge.join(format!("{id}.processing")));
    let _ = std::fs::remove_file(bridge.join(format!("{id}.status.json")));
}

#[cfg(unix)]
fn finalize_bridge_processing(
    bridge: &std::path::Path,
    id: &str,
    response: &Value,
) -> Result<(), String> {
    write_bridge_response_once(bridge, id, response)?;
    cleanup_bridge_processing(bridge, id);
    Ok(())
}

#[cfg(unix)]
fn recover_orphaned_bridge_processing(bridge: &std::path::Path) -> Result<(), String> {
    for entry in std::fs::read_dir(bridge).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".processing") else {
            continue;
        };
        if !valid_bridge_id(id) {
            continue;
        }
        let response = json!({
            "schema_version": csswitch_skill_install_core::SCHEMA_VERSION,
            "status": "REQUEST_INTERRUPTED",
            "request_id": id,
            "retryable": true,
            "request_terminal": true,
            "automatic_retry_allowed": false,
            "directory_commit": null,
            "attach_state": "UNKNOWN",
            "restart_required": false,
            "message": "CSSwitch 在处理该请求期间中断。宿主已清理遗留 .processing；请重新调用相同工具，让 CSSwitch 验证真实落盘和绑定状态后安全恢复。"
        });
        finalize_bridge_processing(bridge, id, &response)?;
    }
    Ok(())
}

#[cfg(unix)]
fn start_skill_install_bridge(cfg: &GatewayConfig) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let (Some(bridge), Some(data_dir), Some(bridge_token)) = (
        &cfg.skill_bridge_dir,
        &cfg.skill_data_dir,
        &cfg.skill_bridge_token,
    ) else {
        return Ok(());
    };
    let name = bridge
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if !bridge.is_absolute() || !name.starts_with("CSSwitch-Skill-Bridge-") {
        return Err("refusing unsafe Skill install bridge path".into());
    }
    reject_bridge_symlinks(bridge)?;
    match std::fs::symlink_metadata(bridge) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err("refusing non-directory Skill install bridge path".into())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(bridge).map_err(|error| error.to_string())?;
        }
        Err(error) => return Err(error.to_string()),
    }
    std::fs::set_permissions(bridge, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(bridge).map_err(|error| error.to_string())?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err("refusing Skill install bridge owned by another user".into());
    }
    let host_lock = acquire_bridge_host_lock(bridge)?;
    recover_orphaned_bridge_processing(bridge)?;
    let bridge = bridge.clone();
    let data_dir = data_dir.clone();
    let bridge_token = bridge_token.clone();
    let science_host_context = cfg.science_host_context.clone();
    thread::spawn(move || {
        let _host_lock = host_lock;
        let mut used_request_ids = HashMap::new();
        loop {
            let entries = match std::fs::read_dir(&bridge) {
                Ok(entries) => entries,
                Err(_) => return,
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(id) = name.strip_suffix(".request.json") else {
                    continue;
                };
                if !valid_bridge_id(id) {
                    continue;
                }
                let processing = bridge.join(format!("{id}.processing"));
                if std::fs::rename(entry.path(), &processing).is_err() {
                    continue;
                }
                let target = bridge.join(format!("{id}.response.json"));
                match bridge_final_response_exists(&target) {
                    Ok(true) => {
                        cleanup_bridge_processing(&bridge, id);
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        eprintln!("Skill bridge refused existing response for {id}: {error}");
                        let progress = BridgeProgress {
                            phase: "finalization_failed".into(),
                            message:
                                "检测到非法的既有最终响应；请求未执行，.processing 已保留供安全恢复"
                                    .into(),
                            sequence: 0,
                        };
                        let _ = write_bridge_status(
                            &bridge,
                            id,
                            "unknown",
                            bridge_now(),
                            bridge_operation_timeout("unknown"),
                            &progress,
                        );
                        continue;
                    }
                }
                let request = read_regular_bridge_request(&processing)
                    .ok()
                    .and_then(|body| serde_json::from_slice::<Value>(&body).ok());
                let operation = request
                    .as_ref()
                    .and_then(|request| request.get("operation"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let started_at = bridge_now();
                let timeout_seconds = bridge_operation_timeout(&operation);
                let progress = Arc::new(Mutex::new(BridgeProgress {
                    phase: "accepted".into(),
                    message: "宿主已接收唯一请求，正在开始处理".into(),
                    sequence: 0,
                }));
                if let Ok(snapshot) = progress.lock() {
                    let _ = write_bridge_status(
                        &bridge,
                        id,
                        &operation,
                        started_at,
                        timeout_seconds,
                        &snapshot,
                    );
                }
                let (stop_tx, stop_rx) = mpsc::channel();
                let heartbeat_bridge = bridge.clone();
                let heartbeat_id = id.to_string();
                let heartbeat_operation = operation.clone();
                let heartbeat_progress = Arc::clone(&progress);
                let heartbeat = thread::spawn(move || loop {
                    match stop_rx.recv_timeout(Duration::from_secs(BRIDGE_HEARTBEAT_SECONDS)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if let Ok(snapshot) = heartbeat_progress.lock() {
                                let _ = write_bridge_status(
                                    &heartbeat_bridge,
                                    &heartbeat_id,
                                    &heartbeat_operation,
                                    started_at,
                                    timeout_seconds,
                                    &snapshot,
                                );
                            }
                        }
                    }
                });
                let mut report_progress = |phase: &str, message: &str| {
                    if let Ok(mut state) = progress.lock() {
                        state.phase = phase.to_string();
                        state.message = message.to_string();
                        state.sequence = state.sequence.saturating_add(1);
                        let _ = write_bridge_status(
                            &bridge,
                            id,
                            &operation,
                            started_at,
                            timeout_seconds,
                            &state,
                        );
                    }
                };
                let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    request.map_or_else(bridge_request_failed, |request| {
                        if crate::skill_install::validate_bridge_request(
                            &bridge_token,
                            id,
                            &request,
                        )
                        .is_err()
                            || bridge_request_is_replay(
                                &mut used_request_ids,
                                id,
                                request
                                    .get("issued_at")
                                    .and_then(Value::as_u64)
                                    .unwrap_or_default(),
                                bridge_now(),
                            )
                        {
                            bridge_request_failed()
                        } else {
                            crate::skill_install::handle_bridge_request_with_progress(
                                &data_dir,
                                science_host_context.as_ref(),
                                &request,
                                &mut report_progress,
                            )
                        }
                    })
                }))
                .unwrap_or_else(|_| bridge_request_internal_failed());
                let _ = stop_tx.send(());
                let _ = heartbeat.join();
                match finalize_bridge_processing(&bridge, id, &response) {
                    Ok(()) => {}
                    Err(error) => {
                        eprintln!("Skill bridge final response write failed for {id}: {error}");
                        let snapshot = BridgeProgress {
                            phase: "finalization_failed".into(),
                            message: "最终响应写入失败；.processing 已保留，gateway 重启后会恢复"
                                .into(),
                            sequence: u64::MAX,
                        };
                        let _ = write_bridge_status(
                            &bridge,
                            id,
                            &operation,
                            started_at,
                            timeout_seconds,
                            &snapshot,
                        );
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    Ok(())
}

#[cfg(unix)]
fn bridge_request_is_replay(
    used: &mut HashMap<String, u64>,
    id: &str,
    issued_at: u64,
    now: u64,
) -> bool {
    used.retain(|_, issued| now.saturating_sub(*issued) <= BRIDGE_REPLAY_WINDOW_SECONDS);
    used.insert(id.to_string(), issued_at).is_some()
}

#[cfg(unix)]
fn reject_bridge_symlinks(path: &std::path::Path) -> Result<(), String> {
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("refusing symlink in Skill install bridge path".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_regular_bridge_request(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "invalid Skill bridge request")?;
    let metadata = file
        .metadata()
        .map_err(|_| "invalid Skill bridge request")?;
    if !metadata.is_file()
        || metadata.len() > 1024 * 1024
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err("invalid Skill bridge request".into());
    }
    let mut body = Vec::with_capacity(metadata.len() as usize);
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut body)
        .map_err(|_| "invalid Skill bridge request")?;
    if body.len() > 1024 * 1024 {
        return Err("invalid Skill bridge request".into());
    }
    Ok(body)
}

#[cfg(unix)]
fn bridge_request_failed() -> Value {
    json!({
        "schema_version": csswitch_skill_install_core::SCHEMA_VERSION,
        "status":"REQUEST_FAILED",
        "message":"本地 Skill 请求非法或已处理",
        "directory_commit":false,
        "restart_required":false
    })
}

#[cfg(unix)]
fn bridge_request_internal_failed() -> Value {
    json!({
        "schema_version": csswitch_skill_install_core::SCHEMA_VERSION,
        "status":"REQUEST_FAILED",
        "message":"本地 Skill 请求处理异常；宿主已停止该请求，可安全重试相同工具",
        "retryable":true,
        "directory_commit":null,
        "restart_required":false
    })
}

#[cfg(not(unix))]
fn start_skill_install_bridge(_cfg: &GatewayConfig) -> Result<(), String> {
    Ok(())
}

fn handle_one(
    cfg: GatewayConfig,
    mut stream: TcpStream,
    request_nonces: Option<Arc<RequestNonceGenerator>>,
    relay_models: Arc<models::RelayModelCache>,
    codex_transport: Option<Arc<codex_transport::CodexTransport>>,
    codex_models: Option<Arc<codex_models::CodexModelCatalog>>,
) {
    let head = match read_head(&mut stream) {
        Ok(head) => head,
        Err(e) => {
            invalid_request_json(&mut stream, &e);
            return;
        }
    };
    match head.method.as_str() {
        "CONNECT" => connect::handle_connect(&head.target, stream),
        "GET" => handle_get(
            &mut stream,
            &cfg,
            &head.target,
            &relay_models,
            codex_models.as_deref(),
        ),
        "POST" => {
            let target = head.target.clone();
            handle_post(
                &mut stream,
                &cfg,
                &target,
                &head,
                request_nonces.as_deref(),
                &relay_models,
                CodexComponents {
                    transport: codex_transport.as_deref(),
                    models: codex_models.as_deref(),
                },
            )
        }
        _ => not_found_json(&mut stream, &head.target),
    }
}

pub fn serve(cfg: GatewayConfig) -> Result<(), String> {
    let request_nonces = if cfg.provider == "deepseek" && cfg.shim_mode == "rewrite" {
        Some(Arc::new(RequestNonceGenerator::new()?))
    } else {
        None
    };
    let codex_transport = if cfg.provider == "codex" {
        let contract = cfg
            .codex_contract
            .as_ref()
            .ok_or("Codex provider contract is unavailable")?;
        Some(Arc::new(
            codex_transport::CodexTransport::production(contract)
                .map_err(|error| error.to_string())?,
        ))
    } else {
        None
    };
    let codex_models = if cfg.provider == "codex" {
        let contract = cfg
            .codex_contract
            .as_ref()
            .ok_or("Codex provider contract is unavailable")?;
        let state_root = cfg
            .codex_state_root
            .clone()
            .ok_or("Codex model catalog state root is unavailable")?;
        Some(Arc::new(
            codex_models::CodexModelCatalog::production(state_root, contract)
                .map_err(|error| error.to_string())?,
        ))
    } else {
        None
    };
    let relay_models = Arc::new(models::RelayModelCache::default());
    let listener = TcpListener::bind(("127.0.0.1", cfg.port)).map_err(|e| e.to_string())?;
    if let Err(error) = start_skill_install_bridge(&cfg) {
        eprintln!("Skill install host unavailable (gateway continues): {error}");
    }
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let cfg = cfg.clone();
                let request_nonces = request_nonces.clone();
                let relay_models = Arc::clone(&relay_models);
                let codex_transport = codex_transport.clone();
                let codex_models = codex_models.clone();
                thread::spawn(move || {
                    handle_one(
                        cfg,
                        stream,
                        request_nonces,
                        relay_models,
                        codex_transport,
                        codex_models,
                    )
                });
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::io::{Cursor, Error, ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::{json, Map, Value};

    #[cfg(unix)]
    use super::{
        acquire_bridge_host_lock, bridge_request_is_replay, finalize_bridge_processing,
        read_regular_bridge_request, recover_orphaned_bridge_processing,
        write_bridge_response_once, write_bridge_status, BridgeProgress,
    };
    use super::{
        collect_codex_nonstream, forward_stream_body, handle_codex_messages_with_catalog,
        handle_codex_messages_with_secrets, handle_post, map_codex_auth_error, pump_codex_stream,
        stream_error_event, write_codex_models_response, CodexComponents, CodexNonstreamError,
        CodexPumpError, KimiServerToolFilter, RequestHead, RequestNonceGenerator, StreamFilter,
        StreamTermination,
    };
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

        for (index, (alias, effort, summary, parallel, lite)) in [
            (
                "claude-csswitch-codex-gpt-sequential",
                "low",
                false,
                false,
                false,
            ),
            (
                "claude-csswitch-codex-gpt-parallel",
                "high",
                true,
                true,
                false,
            ),
            ("claude-csswitch-codex-gpt-lite", "high", true, false, true),
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
        assert_eq!(body["data"][0]["id"], "claude-csswitch-codex-gpt-5.6-sol");
        assert_eq!(body["data"][0]["display_name"], "Codex / GPT-5.6-Sol");
        assert_eq!(body["data"][1]["id"], "claude-csswitch-codex-gpt-5.6-terra");
        assert_eq!(body["data"][1]["display_name"], "Codex / GPT-5.6-Terra");
        assert_eq!(body["data"][2]["id"], "claude-csswitch-codex-gpt-5.6-luna");
        assert_eq!(body["data"][2]["display_name"], "Codex / GPT-5.6-Luna");
        assert_eq!(body["diagnostics"]["source"], "live");
        assert_eq!(body["diagnostics"]["stale"], false);
        assert!(!serde_json::to_string(&body)
            .unwrap()
            .contains("\"id\":\"gpt-5.6-sol\""));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn codex_inference_401_and_403_invalidate_catalog_without_repost() {
        for (status, reason, expected_refreshes) in
            [(401, "Unauthorized", 1), (403, "Forbidden", 0)]
        {
            let (catalog, models_server, root) = mock_codex_model_catalog(&["gpt-known"]);
            let (transport, posts, requests, upstream) = mock_codex_transport(
                format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                )
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
    fn codex_request_body_limit_returns_413_before_reading_body() {
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
            provider: "codex".into(),
            port: 0,
            auth_secret: None,
            api_key: None,
            upstream_url: DEFAULT_CODEX_UPSTREAM_URL.into(),
            models_url: None,
            relay_thinking: None,
            intent: crate::config::GatewayIntent::Formal,
            static_model_resolver: None,
            shim_mode: "off".into(),
            codex_state_root: None,
            codex_contract: None,
            launch_id: "test".into(),
            skill_data_dir: None,
            skill_bridge_dir: None,
            skill_bridge_token: None,
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
        assert!(response.starts_with(b"HTTP/1.1 413 Payload Too Large"));
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
        let root = crate::test_temp_root().join(format!(
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
        let status: Value = serde_json::from_slice(
            &std::fs::read(bridge.join(format!("{id}.status.json"))).unwrap(),
        )
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
        let response: Value = serde_json::from_slice(
            &std::fs::read(bridge.join(format!("{id}.response.json"))).unwrap(),
        )
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
        let response: Value = serde_json::from_slice(
            &std::fs::read(bridge.join(format!("{id}.response.json"))).unwrap(),
        )
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
        let root = crate::test_temp_root().join(format!(
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
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"buffered\"}}"
        )
        .as_bytes()
        .to_vec()
    }

    fn dsml_complete_start_then_partial_tool_delta() -> Vec<u8> {
        let start = format!(
            "event: content_block_start\ndata: {}\n\n",
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
        let first = b"event: content_block_start\ndata: {\"type\":\"content_block_start\"}";
        let mut upstream = FailingReader;
        let mut filter = Some(StreamFilter::Kimi(KimiServerToolFilter::new()));
        let mut output = Vec::new();

        let termination = forward_stream_body(&mut upstream, first, &mut filter, |chunk| {
            output.extend_from_slice(chunk);
            Ok(())
        });

        assert_eq!(termination, StreamTermination::UpstreamReadError);
        assert_eq!(output, stream_error_event("mock read failure"));
        let StreamFilter::Kimi(filter) = filter.as_mut().unwrap() else {
            panic!("expected Kimi filter");
        };
        assert!(
            !filter.finalize().is_empty(),
            "buffer must remain unflushed"
        );
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

        let error = stream_error_event("mock read failure");
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
    fn normal_eof_finalizes_kimi_and_dsml_buffers() {
        let kimi_first = kimi_complete_then_partial();
        let mut kimi_upstream = Cursor::new(Vec::<u8>::new());
        let mut kimi_filter = Some(StreamFilter::Kimi(KimiServerToolFilter::new()));
        let mut kimi_output = Vec::new();
        let kimi_termination =
            forward_stream_body(&mut kimi_upstream, &kimi_first, &mut kimi_filter, |chunk| {
                kimi_output.extend_from_slice(chunk);
                Ok(())
            });
        assert_eq!(kimi_termination, StreamTermination::NormalEof);
        assert!(String::from_utf8_lossy(&kimi_output).contains("buffered"));

        let dsml_first = dsml_complete_start_then_partial_tool_delta();
        let mut dsml_upstream = Cursor::new(Vec::<u8>::new());
        let mut dsml_filter = dsml_filter();
        let mut dsml_output = Vec::new();
        let dsml_termination =
            forward_stream_body(&mut dsml_upstream, &dsml_first, &mut dsml_filter, |chunk| {
                dsml_output.extend_from_slice(chunk);
                Ok(())
            });
        assert_eq!(dsml_termination, StreamTermination::NormalEof);
        assert!(String::from_utf8_lossy(&dsml_output).contains("tool_use"));
        assert!(!String::from_utf8_lossy(&dsml_output).contains("event: error"));
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
            String::from_utf8_lossy(&filter.finalize()).contains("buffered"),
            "buffer must remain unflushed after client write failure"
        );
    }
}
