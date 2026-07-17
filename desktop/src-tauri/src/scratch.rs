//! scratch 事务内核（spec §4.4 / §11）：起一个【临时代理】（scratch 端口 + scratch secret +
//! 候选 provider/base_url/key/model 注环境；native=deepseek/qwen 或 relay），探 /v1/models 或
//! /v1/messages，据状态码判定，
//! 探完杀净。**绝不写 config、不改 AppState、不碰正在服务 Science 的正式代理。**
//! 与 native-entry spec 的 validate_and_save 共用同一内核（绝不各写一份）。

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::runtime::operation::{self, OperationStage, OperationTrace};

/// 探测类型：Models 验端点+鉴权（透传预设保存/获取模型）；Message 验具体模型（选了模型时）。
#[derive(Clone, Copy)]
pub enum ProbeKind {
    Models,
    Message,
}

impl ProbeKind {
    fn as_str(&self) -> &'static str {
        match self {
            ProbeKind::Models => "models",
            ProbeKind::Message => "message",
        }
    }
}

/// 一次探测的原始结果。
pub struct ProbeResult {
    pub status: Option<u16>,
    pub body: String,
}

/// 探测结论（纯分类，供 save/fetch 命令决策）。
#[derive(Debug, PartialEq)]
pub enum ProbeOutcome {
    Ok,                     // 200：可提交
    Auth(u16),              // 401/403：key/权限有误，不提交、不回列表
    ModelError(u16),        // 400/404/422：模型不被接受，不提交
    Unsupported(u16),       // 405：端点明确不提供该探测（GET /v1/models 不支持）——「发现不支持」
    Ambiguous(Option<u16>), // 429/5xx/其它：无法确认，不提交、给「跳过验证」出口
    NoResponse,             // 网络不通 / 无响应
}

/// 把探测状态码分类成结论（纯函数）。
pub fn classify(status: Option<u16>) -> ProbeOutcome {
    match status {
        Some(200) => ProbeOutcome::Ok,
        Some(c @ (401 | 403)) => ProbeOutcome::Auth(c),
        Some(c @ (400 | 404 | 422)) => ProbeOutcome::ModelError(c),
        Some(405) => ProbeOutcome::Unsupported(405),
        Some(c) => ProbeOutcome::Ambiguous(Some(c)), // 429 / 5xx / 其它
        None => ProbeOutcome::NoResponse,
    }
}

/// 「获取模型」降级 source 语义（纯函数，供 fetch_models 用；spec v3 §3.4.3）：
/// 4xx（端点不接受/不提供该发现请求）→ "unsupported"（端点未提供模型列表，用内置）；
/// 429/5xx/无响应 → "network"（上游临时/网络问题，用内置可重试）。
/// Auth(401/403) 不进此函数：fetch_models 对 Auth 直接报错，绝不掩盖坏 key。
pub fn discovery_fallback_source(outcome: &ProbeOutcome) -> &'static str {
    match outcome {
        ProbeOutcome::ModelError(_) | ProbeOutcome::Unsupported(_) => "unsupported",
        _ => "network",
    }
}

/// Parse only the bounded, allowlisted error kind emitted by our own Gateway
/// `/v1/models` endpoint. Never forward its free-form message or upstream body.
pub fn gateway_models_error_kind(body: &str) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    match value
        .get("error_kind")
        .and_then(serde_json::Value::as_str)?
    {
        "protocol" => Some("protocol"),
        "network" => Some("network"),
        "upstream" => Some("upstream"),
        "cache" => Some("cache"),
        "cache_invalidated" => Some("cache_invalidated"),
        "internal" => Some("internal"),
        _ => None,
    }
}

/// 取一个空闲端口：bind 127.0.0.1:0 让内核分配，随即释放（临时代理稍后 bind，有绑定重试兜底 TOCTOU）。
pub fn pick_scratch_port() -> Option<u16> {
    use std::net::TcpListener;
    let l = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = l.local_addr().ok()?.port();
    // l 在此 drop，端口释放。
    Some(port)
}

/// 起临时代理时持有其 Child，作用域结束（含 early return / panic）必 kill——绝不留孤儿。
struct ScratchGuard(Option<Child>);
impl Drop for ScratchGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl ScratchGuard {
    fn liveness(&mut self) -> crate::proc::ChildLiveness {
        match self.0.as_mut() {
            Some(child) => crate::proc::poll_child_liveness(child),
            None => crate::proc::ChildLiveness::Unknown("scratch child already released".into()),
        }
    }

    fn require_running(&mut self, context: &str) -> Result<(), String> {
        match self.0.as_mut() {
            Some(child) => crate::proc::require_child_running(child, context),
            None => Err(format!(
                "{context}存活状态未知：scratch child already released"
            )),
        }
    }
}

/// 临时代理的环境注入清单（纯函数，便于测试）：候选 key 注入指定 `key_env`；`base_url` 非空
/// 才注入对应 adapter 的 base env（native=deepseek/qwen 传空 → 不注入，走各自硬编码官方端点）；
/// `model` 非空注入对应 adapter 的 model env。修真机 P1：让 native 也能被临时代理探测。
pub fn scratch_env(
    provider: &str,
    key_env: &str,
    key: &str,
    base_url: &str,
    model: Option<&str>,
    relay_thinking: &str,
) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if !key_env.is_empty() {
        v.push((key_env.to_string(), key.to_string()));
    }
    if !base_url.is_empty() {
        let env = if matches!(provider, "openai-custom" | "openai-responses") {
            "CSSWITCH_OPENAI_BASE_URL"
        } else {
            "CSSWITCH_RELAY_BASE_URL"
        };
        v.push((env.to_string(), base_url.to_string()));
    }
    if let Some(m) = model {
        if !m.is_empty() {
            let env = if matches!(provider, "openai-custom" | "openai-responses") {
                "CSSWITCH_OPENAI_MODEL"
            } else {
                "CSSWITCH_RELAY_MODEL"
            };
            v.push((env.to_string(), m.to_string()));
        }
    }
    if !matches!(provider, "openai-custom" | "openai-responses") && !relay_thinking.is_empty() {
        v.push((
            "CSSWITCH_RELAY_THINKING".to_string(),
            relay_thinking.to_string(),
        ));
    }
    v
}

/// 临时代理探测目标：`provider` 直接作 `--provider`（native=deepseek/qwen；中转站=relay）；
/// `key_env` 决定候选 key 注入哪个环境变量（native 用各自 `*_API_KEY`，relay 用 `CSSWITCH_RELAY_KEY`）；
/// `base_url` 非空才注入 `CSSWITCH_RELAY_BASE_URL`（native 传空 → 走硬编码官方端点）；
/// `model` 非空注入 `CSSWITCH_RELAY_MODEL`（仅 relay 生效）。
pub struct ScratchTarget<'a> {
    pub provider: &'a str,
    pub key_env: &'a str,
    pub base_url: &'a str,
    pub key: &'a str,
    pub model: Option<&'a str>,
    pub relay_thinking: &'a str, // relay thinking 策略（模板 thinking_policy），非空注入 CSSWITCH_RELAY_THINKING
}

pub struct ScratchBackend {
    bin: PathBuf,
    shim_mode: String,
    codex_network_route: Option<csswitch_codex_network::ResolvedCodexNetworkRoute>,
}

pub(crate) fn backend_for_app<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    adapter: &str,
) -> Result<ScratchBackend, String> {
    let shim_mode = crate::runtime::provider::current_shim_mode_for_adapter(adapter);
    let bin = crate::runtime::proxy_lifecycle::gateway_bin_path(app).ok_or(
        "找不到 csswitch-gateway 二进制；请重新安装完整应用，开发态可设置绝对 CSSWITCH_GATEWAY_BIN。",
    )?;
    let codex_network_route = if adapter == "codex" {
        let cfg = crate::config::load_from(&crate::config::default_dir())
            .map_err(|error| error.to_string())?;
        Some(
            csswitch_codex_network::resolve_from_process(&cfg.codex_network)
                .map_err(|_| "proxy_config_invalid：Codex 网络代理配置非法。".to_string())?,
        )
    } else {
        None
    };
    Ok(ScratchBackend {
        bin,
        shim_mode: shim_mode.to_string(),
        codex_network_route,
    })
}

impl ScratchBackend {
    fn gateway_kind(&self) -> &'static str {
        "rust"
    }

    fn shim_mode(&self) -> &str {
        &self.shim_mode
    }
}

/// 起一个临时代理并探测，探完杀净。**不碰 config / AppState / 正式代理**（修 P1-1/P1-2）。
/// Rust sidecar 由调用方解析；`target` 描述要探测的候选连接（key 经 env 注入，绝不进
/// argv）。provider 由调用方给（native 用 deepseek/qwen 探上游）。
pub fn scratch_probe(
    backend: &ScratchBackend,
    target: &ScratchTarget,
    kind: ProbeKind,
    trace: Option<&OperationTrace>,
    cancel: Option<&AtomicBool>,
) -> ProbeResult {
    let cancelled = || cancel.is_some_and(|flag| flag.load(Ordering::SeqCst));
    if cancelled() {
        return ProbeResult {
            status: None,
            body: "临时探测已取消".into(),
        };
    }
    let port = match pick_scratch_port() {
        Some(p) => p,
        None => {
            return ProbeResult {
                status: None,
                body: "无法分配临时端口".into(),
            }
        }
    };
    let secret = match crate::proc::gen_secret() {
        Ok(s) => s,
        Err(_) => {
            return ProbeResult {
                status: None,
                body: "无法生成 secret".into(),
            }
        }
    };
    let launch_id = match crate::proc::gen_secret() {
        Ok(id) => id,
        Err(e) => {
            return ProbeResult {
                status: None,
                body: format!("无法生成临时 gateway launch_id：{e}"),
            }
        }
    };
    let mut cmd = Command::new(&backend.bin);
    if let Some(t) = trace {
        t.stage(
            OperationStage::ScratchSpawn,
            format!(
                "provider={} kind={} gateway={}",
                target.provider,
                kind.as_str(),
                backend.gateway_kind()
            ),
        );
    }
    if let Err(error) = crate::runtime::proxy_lifecycle::configure_managed_proxy_command(
        &mut cmd,
        target.provider,
        backend.shim_mode(),
        port,
        &secret,
        &launch_id,
    ) {
        return ProbeResult {
            status: None,
            body: error,
        };
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    // key/base_url/model 经 env 注入（绝不进 argv，避免 ps 泄露）；native 不带 relay base。
    for (k, v) in scratch_env(
        target.provider,
        target.key_env,
        target.key,
        target.base_url,
        target.model,
        target.relay_thinking,
    ) {
        cmd.env(k, v);
    }
    if let Some(route) = &backend.codex_network_route {
        match csswitch_codex_network::encode_route(route) {
            Ok(encoded) => {
                cmd.env(csswitch_codex_network::ROUTE_ENV, encoded);
            }
            Err(_) => {
                return ProbeResult {
                    status: None,
                    body: "Codex 网络路由编码失败".into(),
                };
            }
        }
    }
    if cancelled() {
        return ProbeResult {
            status: None,
            body: "临时探测已取消".into(),
        };
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult {
                status: None,
                body: format!("起临时代理失败：{e}"),
            }
        }
    };
    let mut guard = ScratchGuard(Some(child)); // 作用域结束必杀
                                               // 探活最多 ~4s。
    let mut alive = false;
    let mut early_exit = None;
    for _ in 0..(operation::SCRATCH_READY_BUDGET_MS / operation::POLL_INTERVAL_MS) {
        if cancelled() {
            return ProbeResult {
                status: None,
                body: "临时探测已取消".into(),
            };
        }
        std::thread::sleep(Duration::from_millis(operation::POLL_INTERVAL_MS));
        if cancelled() {
            return ProbeResult {
                status: None,
                body: "临时探测已取消".into(),
            };
        }
        match guard.liveness() {
            crate::proc::ChildLiveness::Exited(status) => {
                early_exit = Some(format!(
                    "临时 {} gateway 提前退出（{status}）；若端口被旧实例占用，已拒绝接管而不会误杀旧进程。",
                    backend.gateway_kind()
                ));
                break;
            }
            crate::proc::ChildLiveness::Running => {}
            crate::proc::ChildLiveness::Unknown(error) => {
                early_exit = Some(format!(
                    "无法确认临时 {} gateway 是否存活：{error}",
                    backend.gateway_kind()
                ));
                break;
            }
        }
        let healthy = crate::proc::http_health_gateway(
            port,
            Some(&secret),
            operation::LOCAL_HEALTH_TIMEOUT_MS,
            backend.gateway_kind(),
            Some(target.provider),
            Some(backend.shim_mode()),
            Some(&launch_id),
        );
        if healthy {
            alive = true;
            break;
        }
    }
    if let Some(t) = trace {
        t.stage(
            OperationStage::ScratchHealth,
            if alive { "ready" } else { "not_ready" },
        );
    }
    if !alive {
        return ProbeResult {
            status: None,
            body: early_exit.unwrap_or_else(|| {
                "临时代理未就绪（多为 key/base_url 无效、依赖缺失或端口被旧实例占用）".into()
            }),
        };
    }
    if let Err(error) = guard.require_running(&format!(
        "临时 {} gateway 在上游探测前",
        backend.gateway_kind()
    )) {
        return ProbeResult {
            status: None,
            body: error,
        };
    }
    if cancelled() {
        return ProbeResult {
            status: None,
            body: "临时探测已取消".into(),
        };
    }
    match kind {
        ProbeKind::Models => {
            if let Some(t) = trace {
                t.stage(OperationStage::ScratchUpstreamProbe, "GET /v1/models");
            }
            let timeout_ms = if target.provider == "codex" {
                operation::CODEX_MODELS_PROBE_TIMEOUT_MS
            } else {
                operation::UPSTREAM_PROBE_TIMEOUT_MS
            };
            match crate::proc::http_get_body_cancellable(
                port,
                Some(&secret),
                "/v1/models",
                timeout_ms,
                cancel,
            ) {
                Some((code, body)) => ProbeResult {
                    status: Some(code),
                    body,
                },
                None => ProbeResult {
                    status: None,
                    body: String::new(),
                },
            }
        }
        ProbeKind::Message => {
            // model 由 CSSWITCH_RELAY_MODEL 强制，请求体模型名占位即可（会被 override）。
            let payload = br#"{"model":"claude-opus-4-8","max_tokens":1,"messages":[{"role":"user","content":"ping"}]}"#;
            if let Some(t) = trace {
                t.stage(OperationStage::ScratchUpstreamProbe, "POST /v1/messages");
            }
            match crate::proc::http_post_status(
                port,
                Some(&secret),
                "/v1/messages",
                payload,
                operation::UPSTREAM_PROBE_TIMEOUT_MS,
            ) {
                Some(code) => ProbeResult {
                    status: Some(code),
                    body: String::new(),
                },
                None => ProbeResult {
                    status: None,
                    body: String::new(),
                },
            }
        }
    }
    // guard drop → 杀临时代理。
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn cancelled_probe_never_spawns_the_scratch_gateway() {
        let cancel = AtomicBool::new(true);
        let backend = ScratchBackend {
            bin: PathBuf::from("/definitely/missing/csswitch-gateway"),
            shim_mode: "off".into(),
            codex_network_route: None,
        };
        let result = scratch_probe(
            &backend,
            &ScratchTarget {
                provider: "codex",
                key_env: "",
                base_url: "",
                key: "",
                model: None,
                relay_thinking: "",
            },
            ProbeKind::Models,
            None,
            Some(&cancel),
        );
        assert_eq!(result.status, None);
        assert_eq!(result.body, "临时探测已取消");
    }

    #[test]
    fn scratch_probe_can_use_rust_backend_for_message_probe() {
        let bin = std::env::var_os("CSSWITCH_GATEWAY_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../gateway/target/debug/csswitch-gateway")
            });
        if !bin.is_file() {
            panic!(
                "csswitch-gateway binary not built; run `cargo build --manifest-path desktop/gateway/Cargo.toml` or set CSSWITCH_GATEWAY_BIN"
            );
        }

        let upstream = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let n = stream.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf).to_string();
            let _ = tx.send(head.lines().next().unwrap_or("").to_string());
            let body = br#"{"choices":[{"message":{"content":"pong"}}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        });

        let backend = ScratchBackend {
            bin,
            shim_mode: "off".into(),
            codex_network_route: None,
        };
        let result = scratch_probe(
            &backend,
            &ScratchTarget {
                provider: "openai-custom",
                key_env: "CSSWITCH_OPENAI_KEY",
                base_url: &format!("http://127.0.0.1:{upstream_port}/up"),
                key: "test-key",
                model: Some("mock-model"),
                relay_thinking: "",
            },
            ProbeKind::Message,
            None,
            None,
        );
        assert_eq!(result.status, Some(200));
        let request_line = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(request_line, "POST /up/v1/chat/completions HTTP/1.1");
    }

    #[test]
    fn classify_maps_status_to_outcome() {
        assert_eq!(classify(Some(200)), ProbeOutcome::Ok);
        assert_eq!(classify(Some(401)), ProbeOutcome::Auth(401));
        assert_eq!(classify(Some(403)), ProbeOutcome::Auth(403));
        assert_eq!(classify(Some(404)), ProbeOutcome::ModelError(404));
        assert_eq!(classify(Some(400)), ProbeOutcome::ModelError(400));
        // 405 Method Not Allowed = 端点明确不提供该探测（GET /v1/models 不支持）→ 独立语义，
        // 不再混进 Ambiguous「其它」。修 spec v3 §3.4.2：405 曾被误标网络故障。
        assert_eq!(classify(Some(405)), ProbeOutcome::Unsupported(405));
        assert_eq!(classify(Some(429)), ProbeOutcome::Ambiguous(Some(429)));
        assert_eq!(classify(Some(502)), ProbeOutcome::Ambiguous(Some(502)));
        assert_eq!(classify(None), ProbeOutcome::NoResponse);
    }

    #[test]
    fn discovery_fallback_source_splits_unsupported_from_network() {
        // 「获取模型」降级语义（spec v3 §3.4.3）：4xx=端点不提供发现（unsupported）；
        // 5xx/429/无响应=上游临时/网络（network）。用于前端区分提示，不掩盖坏 key（Auth 另处理）。
        assert_eq!(
            discovery_fallback_source(&ProbeOutcome::ModelError(404)),
            "unsupported"
        );
        assert_eq!(
            discovery_fallback_source(&ProbeOutcome::Unsupported(405)),
            "unsupported"
        );
        assert_eq!(
            discovery_fallback_source(&ProbeOutcome::Ambiguous(Some(429))),
            "network"
        );
        assert_eq!(
            discovery_fallback_source(&ProbeOutcome::NoResponse),
            "network"
        );
    }

    #[test]
    fn gateway_models_error_kind_is_allowlisted_and_ignores_messages() {
        assert_eq!(
            gateway_models_error_kind(
                r#"{"error_kind":"protocol","message":"private upstream detail"}"#
            ),
            Some("protocol")
        );
        assert_eq!(
            gateway_models_error_kind(r#"{"error_kind":"future-kind"}"#),
            None
        );
        assert_eq!(gateway_models_error_kind("not-json"), None);
    }

    #[test]
    fn scratch_env_native_uses_native_key_env_and_no_relay_base() {
        // native：key 进 DEEPSEEK_API_KEY，绝不设 CSSWITCH_RELAY_BASE_URL（否则会被当中转站）。
        let env = scratch_env("deepseek", "DEEPSEEK_API_KEY", "sk-x", "", None, "");
        assert_eq!(
            env,
            vec![("DEEPSEEK_API_KEY".to_string(), "sk-x".to_string())]
        );
    }

    #[test]
    fn scratch_env_relay_sets_base_url_and_model() {
        let env = scratch_env(
            "relay",
            "CSSWITCH_RELAY_KEY",
            "sk-y",
            "https://r/claude",
            Some("m1"),
            "",
        );
        assert_eq!(
            env,
            vec![
                ("CSSWITCH_RELAY_KEY".to_string(), "sk-y".to_string()),
                (
                    "CSSWITCH_RELAY_BASE_URL".to_string(),
                    "https://r/claude".to_string()
                ),
                ("CSSWITCH_RELAY_MODEL".to_string(), "m1".to_string()),
            ]
        );
    }

    #[test]
    fn scratch_env_models_discovery_does_not_pin_relay_model() {
        let env = scratch_env(
            "relay",
            "CSSWITCH_RELAY_KEY",
            "sk-y",
            "https://r/claude",
            None,
            "",
        );
        assert!(env.iter().any(|(k, _)| k == "CSSWITCH_RELAY_BASE_URL"));
        assert!(!env.iter().any(|(k, _)| k == "CSSWITCH_RELAY_MODEL"));
        assert!(!env.iter().any(|(k, _)| k == "CSSWITCH_OPENAI_MODEL"));
    }

    #[test]
    fn scratch_env_models_discovery_does_not_pin_openai_model() {
        let env = scratch_env(
            "openai-custom",
            "CSSWITCH_OPENAI_KEY",
            "sk-z",
            "https://open.bigmodel.cn/api/paas/v4",
            None,
            "",
        );
        assert!(env.iter().any(|(k, _)| k == "CSSWITCH_OPENAI_BASE_URL"));
        assert!(!env.iter().any(|(k, _)| k == "CSSWITCH_OPENAI_MODEL"));
        assert!(!env.iter().any(|(k, _)| k == "CSSWITCH_RELAY_MODEL"));
    }

    #[test]
    fn scratch_env_openai_custom_sets_openai_base_and_model() {
        let env = scratch_env(
            "openai-custom",
            "CSSWITCH_OPENAI_KEY",
            "sk-z",
            "https://open.bigmodel.cn/api/paas/v4",
            Some("glm-4.5"),
            "enabled",
        );
        assert_eq!(
            env,
            vec![
                ("CSSWITCH_OPENAI_KEY".to_string(), "sk-z".to_string()),
                (
                    "CSSWITCH_OPENAI_BASE_URL".to_string(),
                    "https://open.bigmodel.cn/api/paas/v4".to_string()
                ),
                ("CSSWITCH_OPENAI_MODEL".to_string(), "glm-4.5".to_string()),
            ]
        );
    }

    #[test]
    fn scratch_env_openai_responses_sets_openai_base_and_model() {
        let env = scratch_env(
            "openai-responses",
            "CSSWITCH_OPENAI_KEY",
            "sk-z",
            "https://api.openai.com/v1",
            Some("gpt-5.2"),
            "enabled",
        );
        assert_eq!(
            env,
            vec![
                ("CSSWITCH_OPENAI_KEY".to_string(), "sk-z".to_string()),
                (
                    "CSSWITCH_OPENAI_BASE_URL".to_string(),
                    "https://api.openai.com/v1".to_string()
                ),
                ("CSSWITCH_OPENAI_MODEL".to_string(), "gpt-5.2".to_string()),
            ]
        );
    }

    #[test]
    fn scratch_env_relay_injects_thinking_policy() {
        let env = scratch_env(
            "relay",
            "CSSWITCH_RELAY_KEY",
            "sk-y",
            "https://r/claude",
            Some("m1"),
            "enabled",
        );
        assert!(env.contains(&("CSSWITCH_RELAY_THINKING".to_string(), "enabled".to_string())));
    }

    #[test]
    fn scratch_env_empty_thinking_not_injected() {
        let env = scratch_env(
            "relay",
            "CSSWITCH_RELAY_KEY",
            "sk-y",
            "https://r/claude",
            None,
            "",
        );
        assert!(!env.iter().any(|(k, _)| k == "CSSWITCH_RELAY_THINKING"));
    }

    #[test]
    fn scratch_env_gateway_owned_auth_never_invents_a_key_env() {
        let env = scratch_env("codex", "", "", "", None, "");
        assert!(env.is_empty());
    }

    #[test]
    fn pick_scratch_port_returns_usable_nonreserved_port() {
        let p = pick_scratch_port().expect("应能分配端口");
        assert!(p > 1024, "内核分配的临时端口应 > 1024");
        assert_ne!(p, 8765, "绝不撞真实 Science 保留端口");
    }

    #[test]
    fn two_picks_are_bindable() {
        // pick_scratch_port 内部 bind :0 后 drop listener 释放端口，返回的端口应可再次 bind
        // （证明本 fn 未持有它，临时代理稍后能绑）。并行测试下另一个分配器可能抢走刚释放的
        // 端口（OS 端口重绑 race），故重试几次：只要有一次能再 bind 即证明端口确被释放；若
        // pick_scratch_port 真持有端口（bug），所有重试都会失败 → 仍被捕获。
        use std::net::TcpListener;
        let rebound = (0..8).any(|_| {
            let p = pick_scratch_port().unwrap();
            TcpListener::bind(("127.0.0.1", p)).is_ok()
        });
        assert!(rebound, "pick_scratch_port 返回的端口应已释放、可再 bind");
    }
}
