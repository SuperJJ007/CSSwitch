pub(crate) fn validate_runtime_ports(proxy_port: u16, sandbox_port: u16) -> Result<(), String> {
    if proxy_port == 8765 || sandbox_port == 8765 {
        return Err("端口 8765 是真实 Science 实例保留端口，不能用。".into());
    }
    if proxy_port == 0 || sandbox_port == 0 {
        return Err("端口不能为 0。".into());
    }
    if proxy_port == sandbox_port {
        return Err("代理端口与沙箱端口不能相同。".into());
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
    }

    #[test]
    fn validate_runtime_ports_accepts_distinct_nonreserved_ports() {
        assert!(validate_runtime_ports(18991, 18992).is_ok());
    }
}
