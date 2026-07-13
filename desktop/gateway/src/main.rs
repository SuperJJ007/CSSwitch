fn main() {
    let args: Vec<String> = std::env::args().collect();
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
