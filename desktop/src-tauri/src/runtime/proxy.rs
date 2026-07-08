/// 本次 ensure_proxy 对代理做了什么（供一键据实提示）。
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ProxyAction {
    Reused,    // 端口+adapter+key 指纹一致且健康，原样复用
    Restarted, // 首次起 / 换 key / 换 profile / 不健康，重起了代理
}

impl ProxyAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ProxyAction::Reused => "reused",
            ProxyAction::Restarted => "restarted",
        }
    }
}

/// 探活结束回锁后是否可写回 `st.proxy`：generation 未被取代【且】secret 仍是本次启动的。
/// 抽成纯函数便于确定性单测（gen 同/异 × secret 同/异 4 组合）。
/// secret 合取防「冷启动双起、两个不同 secret、generation 却相等」的窄窗：另起若用不同 secret
/// 重置了槽位，本次就不该拿旧 child 覆盖它（起代理前会把 `st.secret` 预置成本次 secret，故合法启动上恒真）。
pub(crate) fn should_write_back(
    gen_captured: u64,
    gen_now: u64,
    st_secret: &str,
    my_secret: &str,
) -> bool {
    gen_captured == gen_now && st_secret == my_secret
}

/// 探活超时的原因措辞（纯函数，修真机 P2）：本地 `/health` 不验上游 key，故探活超时与 key 有效性
/// 无关。日志出现绑定失败（Address already in use / EADDRINUSE）→ 明确报端口占用；否则报「探活超时」
/// （多为 python 依赖缺失 / 脚本异常），绝不再含糊说「或 key 无效」。
pub(crate) fn health_timeout_reason(port: u16, tail: &str) -> String {
    let occupied = tail.contains("Address already in use")
        || tail.contains("EADDRINUSE")
        || tail.contains("Errno 48") // macOS EADDRINUSE
        || tail.contains("Errno 98"); // Linux EADDRINUSE
    if occupied {
        format!("端口 {port} 已被占用，换个端口或先停掉占用进程后重试。")
    } else {
        format!(
            "代理起后探活超时（端口 {port}）：多为 python 依赖缺失或代理脚本异常，请查看代理日志。"
        )
    }
}

/// Escape ERE metacharacters so a path can be matched literally by `pkill -f`.
pub(crate) fn ere_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if "\\.^$*+?()[]{}|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{ere_escape, health_timeout_reason, should_write_back};

    #[test]
    fn should_write_back_requires_both_gen_and_secret() {
        // gen 同 + secret 同 → 写回（合法启动，未被取代）
        assert!(should_write_back(5, 5, "sekret", "sekret"));
        // gen 同 + secret 异 → 不写回（被并发另起用不同 secret 占了槽，冷启动双起窄窗）
        assert!(!should_write_back(5, 5, "other", "sekret"));
        // gen 异 + secret 同 → 不写回（被清 key/停/切 bump 取代）
        assert!(!should_write_back(5, 6, "sekret", "sekret"));
        // gen 异 + secret 异 → 不写回
        assert!(!should_write_back(5, 6, "other", "sekret"));
    }

    #[test]
    fn health_timeout_reason_flags_port_conflict_and_never_blames_key() {
        // 端口占用：明确报占用、带端口号，绝不提「key 无效」。
        let occ = health_timeout_reason(18991, "OSError: [Errno 48] Address already in use");
        assert!(occ.contains("18991"));
        assert!(occ.contains("占用"), "应明确报端口占用：{occ}");
        assert!(!occ.contains("key"), "端口占用不该扯上 key：{occ}");
        // 其它探活失败（依赖缺失等）：本地探活与 key 有效性无关，不得说「key 无效」。
        let generic = health_timeout_reason(18991, "ModuleNotFoundError: No module named 'x'");
        assert!(
            !generic.contains("key 无效"),
            "本地探活超时与 key 有效性无关：{generic}"
        );
    }

    #[test]
    fn ere_escape_makes_path_literal_for_extended_regex() {
        assert_eq!(
            ere_escape("/tmp/a+b(proxy).py"),
            "/tmp/a\\+b\\(proxy\\)\\.py"
        );
    }
}
