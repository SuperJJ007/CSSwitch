fn main() {
    let args_os: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args_os.get(1).and_then(|arg| arg.to_str()) == Some("codex-auth") {
        let expected_service = std::env::var("CSSWITCH_EXPECTED_CODEX_KEYCHAIN_SERVICE").ok();
        if !csswitch_gateway::codex_auth::expected_keychain_service_matches(
            expected_service.as_deref(),
        ) {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stdout().lock(),
                "{{\"schema_version\":1,\"ok\":false,\"command\":null,\"error\":{{\"code\":\"internal_error\",\"message\":\"Codex auth sidecar build identity mismatch.\",\"retryable\":false}}}}"
            );
            std::process::exit(8);
        }
        let codex_args = args_os[2..]
            .iter()
            .map(|arg| arg.to_str().map(str::to_string))
            .collect::<Option<Vec<_>>>();
        let run = match codex_args {
            Some(args) => csswitch_gateway::codex_auth::run_cli(&args),
            None => csswitch_gateway::codex_auth::run_cli(&["invalid".into(), "invalid".into()]),
        };
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout().lock(), "{}", run.json);
        if run.exit_code != 0 {
            std::process::exit(run.exit_code);
        }
        return;
    }
    let args: Vec<String> = args_os
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if args.get(1).map(String::as_str) == Some("skill-install-mcp") {
        if let Err(e) = csswitch_gateway::skill_install::run_mcp(&args[2..]) {
            eprintln!("csswitch-gateway skill installer: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("science-control") {
        match csswitch_gateway::science_control::run_cli(&args[2..]) {
            Ok(result) => println!("{result}"),
            Err(e) => {
                eprintln!("csswitch-gateway local Science control: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    match csswitch_gateway::config::GatewayConfig::from_env_args(args) {
        Ok(cfg) => {
            if let Err(e) = csswitch_gateway::server::serve(cfg) {
                eprintln!("csswitch-gateway: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("csswitch-gateway: {e}");
            std::process::exit(2);
        }
    }
}
