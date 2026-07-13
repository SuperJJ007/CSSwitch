pub(crate) fn validate_runtime_ports(proxy_port: u16, sandbox_port: u16) -> Result<(), String> {
    crate::config::validate_runtime_ports(proxy_port, sandbox_port)?;
    let preview_port = sandbox_port
        .checked_add(1)
        .ok_or("沙箱端口必须小于 65535，才能分配隔离预览端口。")?;
    if preview_port == 8765 {
        return Err("沙箱预览端口会命中真实 Science 保留端口 8765。".into());
    }
    if preview_port == proxy_port {
        return Err("代理端口不能与沙箱预览端口相同。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_runtime_ports;

    #[test]
    fn validate_runtime_ports_rejects_reserved_real_science_port() {
        assert!(validate_runtime_ports(8765, 18991).is_err());
        assert!(validate_runtime_ports(18991, 8765).is_err());
    }

    #[test]
    fn validate_runtime_ports_rejects_zero_and_same_port() {
        assert!(validate_runtime_ports(0, 18991).is_err());
        assert!(validate_runtime_ports(18991, 0).is_err());
        assert!(validate_runtime_ports(18991, 18991).is_err());
        assert!(validate_runtime_ports(8991, 8990).is_err());
        assert!(validate_runtime_ports(18991, 8764).is_err());
        assert!(validate_runtime_ports(18991, u16::MAX).is_err());
        assert!(
            crate::config::validate_runtime_ports(8991, 8990).is_ok(),
            "legacy config must remain readable so the UI can repair it"
        );
    }

    #[test]
    fn validate_runtime_ports_accepts_distinct_nonreserved_ports() {
        assert!(validate_runtime_ports(18991, 18992).is_ok());
    }
}
