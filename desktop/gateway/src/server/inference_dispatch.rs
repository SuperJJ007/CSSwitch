use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::auth::{strip_path_secret, AuthResult};
use crate::config::GatewayConfig;
use crate::{
    anthropic_compat::{self, AnthropicMetadata, KimiServerToolFilter},
    codex_auth, codex_models, codex_protocol, codex_transport,
    dsml_shim::{DsmlDetector, DsmlStreamRewriter},
    messages, models, openai_chat, openai_responses, policy,
};

use super::http_codec::{
    api_error_json, codex_model_unavailable_json, content_length, dequery, forbidden_json,
    invalid_request_json, models_error_json, not_found_json, read_body, request_too_large_json,
    route_unknown_json, status_reason, typed_error_json, upstream_protocol_error_json,
    write_codex_models_response, write_json, write_response, RequestHead,
};

#[derive(Clone, Copy, Default)]
pub(super) struct CodexComponents<'a> {
    pub(super) transport: Option<&'a codex_transport::CodexTransport>,
    pub(super) models: Option<&'a codex_models::CodexModelCatalog>,
}

pub(super) struct RequestNonceGenerator {
    process_prefix: String,
    request_counter: AtomicU64,
}

impl RequestNonceGenerator {
    pub(super) fn new() -> Result<Self, String> {
        let mut process_prefix = [0_u8; 16];
        getrandom::getrandom(&mut process_prefix)
            .map_err(|e| format!("request nonce random initialization failed: {e}"))?;
        Ok(Self::with_prefix(process_prefix))
    }

    pub(super) fn with_prefix(process_prefix: [u8; 16]) -> Self {
        let process_prefix = process_prefix
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            process_prefix,
            request_counter: AtomicU64::new(0),
        }
    }

    pub(super) fn next_nonce(&self) -> String {
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

pub(super) enum StreamFilter {
    Kimi(KimiServerToolFilter),
    DsmlDetect(DsmlDetector),
    DsmlRewrite(DsmlStreamRewriter),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamTermination {
    NormalEof,
    UpstreamTerminalError,
    UpstreamReadError,
    ProtocolError,
    DownstreamWriteError,
}

impl StreamFilter {
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            StreamFilter::Kimi(filter) => filter.feed(chunk),
            StreamFilter::DsmlDetect(detector) => {
                detector.feed(chunk);
                Ok(chunk.to_vec())
            }
            StreamFilter::DsmlRewrite(rewriter) => Ok(rewriter.feed(chunk)),
        }
    }

    fn finalize(&mut self) -> Result<Vec<u8>, String> {
        match self {
            StreamFilter::Kimi(filter) => filter.finalize(),
            StreamFilter::DsmlDetect(_) => Ok(Vec::new()),
            StreamFilter::DsmlRewrite(rewriter) => Ok(rewriter.finalize()),
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

pub(super) fn handle_get(
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
            if let Some(contract) = cfg.provider_contract.as_ref() {
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
            write_codex_models_with_refresh(
                stream,
                catalog,
                &secrets,
                cfg.intent == crate::config::GatewayIntent::ScratchModels,
                cfg.codex_state_root.clone(),
                |root, generation| {
                    let _ = codex_auth::refresh_production_for_generation(root, generation);
                },
            );
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

pub(super) fn write_codex_models_with_refresh(
    stream: &mut TcpStream,
    catalog: &codex_models::CodexModelCatalog,
    secrets: &codex_auth::InferenceSecrets,
    force_refresh: bool,
    state_root: Option<std::path::PathBuf>,
    refresh: impl FnOnce(std::path::PathBuf, u64),
) {
    let snapshot = if force_refresh {
        catalog.refresh_published_snapshot(secrets)
    } else {
        catalog.published_snapshot(secrets)
    };
    match snapshot {
        Ok(snapshot) => write_codex_models_response(stream, &snapshot),
        Err(error) => {
            if error.upstream_status == Some(401) {
                if let Some(root) = state_root {
                    refresh(root, secrets.auth_generation());
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

fn write_chunk(stream: &mut TcpStream, chunk: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:x}\r\n", chunk.len())?;
    stream.write_all(chunk)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

pub(super) fn stream_error_event(detail: &str) -> Vec<u8> {
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

pub(super) fn forward_stream_body<R, F>(
    upstream: &mut R,
    first: &[u8],
    filter: &mut Option<StreamFilter>,
    mut emit: F,
) -> StreamTermination
where
    R: Read,
    F: FnMut(&[u8]) -> std::io::Result<()>,
{
    let mut validator = crate::anthropic_sse::Validator::default();

    let filter_error = |emit: &mut F| {
        if emit(&stream_error_event("upstream SSE protocol error")).is_err() {
            StreamTermination::DownstreamWriteError
        } else {
            StreamTermination::ProtocolError
        }
    };

    let process = |chunk: &[u8], validator: &mut crate::anthropic_sse::Validator, emit: &mut F| {
        let validated = match validator.feed(chunk) {
            Ok(validated) => validated,
            Err(_) => {
                if emit(&stream_error_event("upstream SSE protocol error")).is_err() {
                    return Some(StreamTermination::DownstreamWriteError);
                }
                return Some(StreamTermination::ProtocolError);
            }
        };
        if !validated.bytes.is_empty() && emit(&validated.bytes).is_err() {
            return Some(StreamTermination::DownstreamWriteError);
        }
        validated
            .terminal_error
            .then_some(StreamTermination::UpstreamTerminalError)
    };

    let first = match filter.as_mut() {
        Some(filter) => match filter.feed(first) {
            Ok(chunk) => chunk,
            Err(_) => return filter_error(&mut emit),
        },
        None => first.to_vec(),
    };
    if let Some(termination) = process(&first, &mut validator, &mut emit) {
        return termination;
    }

    let mut buf = [0_u8; 8192];
    loop {
        match upstream.read(&mut buf) {
            Ok(0) => {
                if let Some(filter) = filter.as_mut() {
                    let tail = match filter.finalize() {
                        Ok(tail) => tail,
                        Err(_) => return filter_error(&mut emit),
                    };
                    if let Some(termination) = process(&tail, &mut validator, &mut emit) {
                        return termination;
                    }
                }
                return match validator.finish() {
                    Ok(terminal) => {
                        if !terminal.is_empty() && emit(&terminal).is_err() {
                            StreamTermination::DownstreamWriteError
                        } else {
                            StreamTermination::NormalEof
                        }
                    }
                    Err(_) => {
                        if emit(&stream_error_event("upstream SSE protocol error")).is_err() {
                            StreamTermination::DownstreamWriteError
                        } else {
                            StreamTermination::ProtocolError
                        }
                    }
                };
            }
            Ok(n) => {
                let chunk = if let Some(filter) = filter.as_mut() {
                    match filter.feed(&buf[..n]) {
                        Ok(chunk) => chunk,
                        Err(_) => return filter_error(&mut emit),
                    }
                } else {
                    buf[..n].to_vec()
                };
                if let Some(termination) = process(&chunk, &mut validator, &mut emit) {
                    return termination;
                }
            }
            Err(_) => {
                if emit(&stream_error_event("upstream stream read failed")).is_err() {
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
    let mut upstream = match messages::open_stream(cfg, body) {
        Ok(upstream) => upstream,
        Err(error) => {
            api_error_json(stream, error.status, &error.detail);
            return;
        }
    };
    if write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
    )
    .and_then(|_| stream.flush())
    .is_err()
    {
        return;
    }
    let termination = forward_stream_body(&mut upstream.response, &[], &mut filter, |chunk| {
        write_chunk(stream, chunk)
    });
    match termination {
        StreamTermination::NormalEof => {
            if let Some(filter) = filter.as_ref() {
                filter.log_stats();
            }
        }
        StreamTermination::UpstreamTerminalError
        | StreamTermination::UpstreamReadError
        | StreamTermination::ProtocolError => {}
        StreamTermination::DownstreamWriteError => return,
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

pub(super) fn openai_chat_reasoning_signer(
    cfg: &GatewayConfig,
) -> Result<openai_chat::ReasoningSigner, String> {
    // The loopback auth token rotates with each managed Gateway launch, so it
    // cannot key history that must survive an application restart. API keys are
    // stable profile credentials; contract + endpoint keep signatures scoped to
    // the exact provider route instead of portable across profiles.
    let secret = cfg
        .api_key
        .as_deref()
        .or(cfg.auth_secret.as_deref())
        .unwrap_or("");
    let contract_scope = cfg
        .provider_contract
        .as_ref()
        .map(|contract| contract.contract_id.as_str())
        .unwrap_or(cfg.provider.as_str());
    let context = format!("{contract_scope}\0{}", cfg.upstream_url);
    openai_chat::ReasoningSigner::new(secret, &context)
}

pub(super) fn dsml_stream_filter(
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

pub(super) fn apply_dsml_nonstream(
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
pub(super) struct CodexAuthLoadError {
    pub(super) status: u16,
    pub(super) error_type: &'static str,
    pub(super) message: &'static str,
}

pub(super) fn map_codex_auth_error(error: codex_auth::OAuthFlowError) -> CodexAuthLoadError {
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
pub(super) enum CodexPumpError {
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

pub(super) fn pump_codex_stream<R, F>(
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
pub(super) enum CodexNonstreamError {
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

pub(super) fn collect_codex_nonstream<R: Read>(
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
    supported_reasoning_efforts: &'a [String],
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
        supported_reasoning_efforts: policy.supported_reasoning_efforts,
        supports_reasoning_summary: policy.supports_reasoning_summary,
        supports_parallel_tool_calls: policy.supports_parallel_tool_calls,
        use_responses_lite: policy.use_responses_lite,
    };
    let translated = match codex_protocol::translate_anthropic_exchange(raw, &context, &signer) {
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
    let body = match serde_json::to_vec(&translated.body) {
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
    let mut reducer = codex_protocol::ResponsesReducer::new_with_tool_names(
        target_model,
        secrets.auth_epoch(),
        secrets.account_hash(),
        &signer,
        translated.tool_names,
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
pub(super) fn handle_codex_messages_with_secrets(
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

pub(super) fn handle_codex_messages_with_catalog(
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
    let target_model = match snapshot.resolve_request_model(requested_model) {
        Ok(model) => model,
        Err(codex_models::CodexModelResolutionError::UnknownSelector) => {
            route_unknown_json(
                stream,
                "model selector is not available for this Codex account",
            );
            return;
        }
        Err(codex_models::CodexModelResolutionError::NoCompatibleStandardResponsesModel) => {
            codex_model_unavailable_json(
                stream,
                "this Codex account has no compatible standard Responses model for the requested Claude role",
            );
            return;
        }
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
            supported_reasoning_efforts: target_model.supported_reasoning_efforts(),
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
        let reasoning_signer = match openai_chat_reasoning_signer(cfg) {
            Ok(signer) => signer,
            Err(error) => {
                api_error_json(stream, 500, &error);
                return;
            }
        };
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
            openai_chat::anthropic_to_openai_custom(&raw, &target_model, &reasoning_signer)
                .map(|body| (body, None))
        } else {
            openai_chat::anthropic_to_openai(&raw, &target_model, &reasoning_signer)
                .map(|body| (body, None))
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
                    openai_responses::openai_to_anthropic_validated(
                        &openai_resp,
                        &model_id,
                        &known_tools,
                    )
                } else {
                    openai_chat::openai_to_anthropic(
                        &openai_resp,
                        &model_id,
                        &target_model,
                        &reasoning_signer,
                    )
                };
                let anthropic_resp = match anthropic_resp {
                    Ok(response) => response,
                    Err(error) => {
                        if cfg.provider == "openai-responses" {
                            upstream_protocol_error_json(stream, &error);
                        } else {
                            api_error_json(stream, 502, &error);
                        }
                        return;
                    }
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
            Ok(mut resp) => {
                if metadata.target_model.to_ascii_lowercase().contains("kimi") {
                    resp.body = match anthropic_compat::filter_kimi_nonstream_response(&resp.body) {
                        Ok(body) => body,
                        Err(error) => {
                            api_error_json(stream, 502, &error);
                            return;
                        }
                    };
                }
                write_response(
                    stream,
                    resp.status,
                    status_reason(resp.status),
                    &resp.content_type,
                    &resp.body,
                )
            }
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

pub(super) fn handle_post(
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
    if path != "/v1/messages" {
        // Discard any already-buffered request bytes before closing so an
        // early route rejection cannot turn the flushed 404 into a TCP RST.
        let _ = stream.shutdown(std::net::Shutdown::Read);
        not_found_json(stream, &path);
        return;
    }
    let len = match content_length(&head.headers) {
        Ok(len) => len,
        Err(e) => {
            invalid_request_json(stream, &e);
            return;
        }
    };
    if codex_protocol::validate_request_body_size(len).is_err() {
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
    handle_messages(stream, cfg, body, request_nonces, relay_models, codex);
}
