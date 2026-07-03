//! scratch 事务内核（spec §4.4 / §11）：起一个【临时 relay 代理】（scratch 端口 + scratch
//! secret + 候选 base_url/key/model 注环境），探 /v1/models 或 /v1/messages，据状态码判定，
//! 探完杀净。**绝不写 config、不改 AppState、不碰正在服务 Science 的正式代理。**
//! 与 native-entry spec 的 validate_and_save 共用同一内核（绝不各写一份）。

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// 探测类型：Models 验端点+鉴权（透传预设保存/获取模型）；Message 验具体模型（选了模型时）。
pub enum ProbeKind {
    Models,
    Message,
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
    Ambiguous(Option<u16>), // 429/5xx/其它：无法确认，不提交、给「跳过验证」出口
    NoResponse,             // 网络不通 / 无响应
}

/// 把探测状态码分类成结论（纯函数）。
pub fn classify(status: Option<u16>) -> ProbeOutcome {
    match status {
        Some(200) => ProbeOutcome::Ok,
        Some(c @ (401 | 403)) => ProbeOutcome::Auth(c),
        Some(c @ (400 | 404 | 422)) => ProbeOutcome::ModelError(c),
        Some(c) => ProbeOutcome::Ambiguous(Some(c)), // 429 / 5xx / 其它
        None => ProbeOutcome::NoResponse,
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

/// 起一个临时 relay 代理并探测，探完杀净。**不碰 config / AppState / 正式代理**（修 P1-1/P1-2）。
/// py/script 由调用方经 asset_root + find_exe 提供；base_url/key 为候选值（key 经 env 注入，绝不进 argv）。
pub fn scratch_probe(
    py: &Path,
    script: &Path,
    base_url: &str,
    key: &str,
    model: Option<&str>,
    kind: ProbeKind,
) -> ProbeResult {
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
    let mut cmd = Command::new(py);
    cmd.arg(script)
        .arg("--provider")
        .arg("relay") // Python 只认 relay；relay-<preset> 是 Rust 侧概念
        .arg("--port")
        .arg(port.to_string())
        .arg("--auth-token")
        .arg(&secret)
        .env("CSSWITCH_RELAY_BASE_URL", base_url)
        .env("CSSWITCH_RELAY_KEY", key)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(m) = model {
        if !m.is_empty() {
            cmd.env("CSSWITCH_RELAY_MODEL", m);
        }
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
    let _guard = ScratchGuard(Some(child)); // 作用域结束必杀
                                            // 探活最多 ~4s。
    let mut alive = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        if crate::proc::http_health(port, Some(&secret), 400) {
            alive = true;
            break;
        }
    }
    if !alive {
        return ProbeResult {
            status: None,
            body: "临时代理未就绪（多为 key/base_url 无效或依赖缺失）".into(),
        };
    }
    match kind {
        ProbeKind::Models => {
            match crate::proc::http_get_body(port, Some(&secret), "/v1/models", 20000) {
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
            match crate::proc::http_post_status(port, Some(&secret), "/v1/messages", payload, 20000)
            {
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
    // _guard drop → 杀临时代理。
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_status_to_outcome() {
        assert_eq!(classify(Some(200)), ProbeOutcome::Ok);
        assert_eq!(classify(Some(401)), ProbeOutcome::Auth(401));
        assert_eq!(classify(Some(403)), ProbeOutcome::Auth(403));
        assert_eq!(classify(Some(404)), ProbeOutcome::ModelError(404));
        assert_eq!(classify(Some(400)), ProbeOutcome::ModelError(400));
        assert_eq!(classify(Some(429)), ProbeOutcome::Ambiguous(Some(429)));
        assert_eq!(classify(Some(502)), ProbeOutcome::Ambiguous(Some(502)));
        assert_eq!(classify(None), ProbeOutcome::NoResponse);
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
