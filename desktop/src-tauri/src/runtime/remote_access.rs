const DEFAULT_SSH_PORT: u16 = 22;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SshTunnelPlan {
    pub(crate) command: String,
    pub(crate) login_command: String,
    pub(crate) preview_port: u16,
}

fn valid_ssh_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 255
        && !target.starts_with('-')
        && target.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-' | b'@' | b':' | b'[' | b']' | b'%')
        })
}

/// Build a client-side local-forward command without ever exposing the CSSwitch Gateway.
///
/// CSSwitch launches Science with an explicit preview listener at `science_port + 1`. Both
/// forwards bind loopback on the SSH client as well as the server.
pub(crate) fn build_ssh_tunnel_plan(
    target: &str,
    ssh_port: u16,
    science_port: u16,
    gateway_port: u16,
) -> Result<SshTunnelPlan, String> {
    let target = target.trim();
    if !valid_ssh_target(target) {
        return Err(
            "SSH 目标只允许 user@DNS主机/IP 或带方括号的 IPv6；不得包含空格、控制符或 SSH 选项。为隔离隐藏转发，命令不读取 SSH config alias。"
                .to_string(),
        );
    }
    if ssh_port == 0 {
        return Err("SSH 端口必须是 1-65535。".to_string());
    }
    if science_port == 0 || science_port == 8765 || gateway_port == 0 {
        return Err("Science 沙箱端口无效或命中保留端口 8765。".to_string());
    }
    let preview_port = science_port
        .checked_add(1)
        .ok_or("Science 沙箱端口必须小于 65535，才能安全转发预览端口。")?;
    if preview_port == 8765 || preview_port == gateway_port || science_port == gateway_port {
        return Err(
            "Science、预览与 CSSwitch Gateway 端口必须互不冲突，且不得使用 8765。".to_string(),
        );
    }
    let port_flag = if ssh_port == DEFAULT_SSH_PORT {
        String::new()
    } else {
        format!(" -p {ssh_port}")
    };
    let command = format!(
        "ssh -F /dev/null -N -T{port_flag} -o StrictHostKeyChecking=ask \
         -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 \
         -L 127.0.0.1:{science_port}:127.0.0.1:{science_port} \
         -L 127.0.0.1:{preview_port}:127.0.0.1:{preview_port} {target}"
    );
    // The UI must never receive a one-time Science login token. This second,
    // secret-free command asks the remote host for one only after the user runs
    // it on the SSH client; the token exists solely in that terminal's output.
    let login_command = format!(
        "ssh -F /dev/null -T{port_flag} -o StrictHostKeyChecking=ask {target} \
         'safe_bin() {{ p=\"$1\"; while [ \"$p\" != / ]; do [ -L \"$p\" ] && return 1; p=${{p%/*}}; [ -n \"$p\" ] || p=/; done; [ -f \"$1\" ] && [ -x \"$1\" ]; }}; \
         H=\"$HOME/.csswitch/sandbox/home\"; A=\"/Applications/Claude Science.app/Contents/Resources/bin/claude-science\"; F=\"$H/.claude-science/bin/claude-science\"; \
         if safe_bin \"$A\"; then B=\"$A\"; elif safe_bin \"$F\"; then B=\"$F\"; else echo \"Claude Science binary unavailable\" >&2; exit 1; fi; \
         HOME=\"$H\" \"$B\" url --data-dir \"$H/.claude-science\"' \
         | sed -E 's#http://(localhost|\\[::1\\]):{science_port}/#http://127.0.0.1:{science_port}/#'"
    );
    Ok(SshTunnelPlan {
        command,
        login_command,
        preview_port,
    })
}

#[cfg(test)]
mod tests {
    use super::build_ssh_tunnel_plan;

    #[test]
    fn tunnel_forwards_science_and_preview_on_client_loopback_only() {
        let plan = build_ssh_tunnel_plan("alice@science.example", 22, 8990, 18991).unwrap();
        assert!(plan
            .command
            .starts_with("ssh -F /dev/null -N -T -o StrictHostKeyChecking=ask"));
        assert!(plan.command.contains("-L 127.0.0.1:8990:127.0.0.1:8990"));
        assert!(plan.command.contains("-L 127.0.0.1:8991:127.0.0.1:8991"));
        assert!(plan.command.ends_with("alice@science.example"));
        assert_eq!(plan.preview_port, 8991);
        assert!(
            !plan.command.contains("18991"),
            "Gateway must never be forwarded"
        );
        assert!(!plan.command.contains("0.0.0.0"));
        assert!(plan.command.contains("StrictHostKeyChecking=ask"));
        assert!(plan.login_command.contains(" url --data-dir "));
        assert!(plan.login_command.contains("sed -E"));
        assert!(!plan.login_command.contains("?token="));
    }

    #[test]
    fn tunnel_supports_nonstandard_ssh_port_and_ipv6_target() {
        let plan = build_ssh_tunnel_plan("alice@[2001:db8::1]", 2222, 9100, 18991).unwrap();
        assert!(plan.command.contains(" -p 2222 "));
        assert!(plan.command.ends_with("alice@[2001:db8::1]"));
    }

    #[test]
    fn tunnel_rejects_options_shell_text_and_invalid_ports() {
        for target in [
            "-oProxyCommand=bad",
            "user@host;touch-pwned",
            "user@host command",
            "user@host\nnext",
            "",
        ] {
            assert!(
                build_ssh_tunnel_plan(target, 22, 8990, 18991).is_err(),
                "{target:?}"
            );
        }
        assert!(build_ssh_tunnel_plan("user@host", 0, 8990, 18991).is_err());
        assert!(build_ssh_tunnel_plan("user@host", 22, 8765, 18991).is_err());
        assert!(build_ssh_tunnel_plan("user@host", 22, 8764, 18991).is_err());
        assert!(build_ssh_tunnel_plan("user@host", 22, 8990, 8991).is_err());
        assert!(build_ssh_tunnel_plan("user@host", 22, u16::MAX, 18991).is_err());
    }
}
