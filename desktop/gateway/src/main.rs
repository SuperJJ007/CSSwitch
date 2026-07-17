fn main() {
    let args_os: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args_os.get(1).and_then(|arg| arg.to_str()) == Some("codex-auth") {
        let codex_args = args_os[2..]
            .iter()
            .map(|arg| arg.to_str().map(str::to_string))
            .collect::<Option<Vec<_>>>();
        let args = codex_args.unwrap_or_else(|| vec!["invalid".into(), "invalid".into()]);
        if let Some(exit_code) = csswitch_gateway::codex_auth::run_streaming_cli(&args) {
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            return;
        }
        let run = csswitch_gateway::codex_auth::run_cli(&args);
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
