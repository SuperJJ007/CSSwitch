//! csswitch-helper — CSSwitch 远程服务器管理 Helper CLI。
//!
//! 一个独立 Rust 二进制（零外部运行时依赖），部署在远程 Linux 服务器上。
//! 通过 JSON-line 协议与桌面端通信，管理本地代理进程、配置文件和沙箱。
//!
//! 用法：
//!   csswitch-helper --json status           # 健康/能力报告
//!   csswitch-helper --json proxy start ...  # 启代理
//!   csswitch-helper --json serve            # 持久 JSON-line 会话模式
//!
//! 编译（无 Tauri 依赖）：
//!   cargo build --bin csswitch-helper --no-default-features --release

// 通过 #[path] 引入共享模块（helper 不依赖 Tauri，无法用 crate:: 引用整个 lib）。
#[path = "../config.rs"]
mod config;
#[path = "../config_legacy.rs"]
mod config_legacy;
#[path = "../templates.rs"]
mod templates;
#[path = "../proc.rs"]
mod proc;
#[path = "../fs_ext.rs"]
mod fs_ext;
#[path = "../oauth_forge.rs"]
mod oauth_forge;
#[path = "../cli/mod.rs"]
mod cli;

fn main() {
    // 初始化操作日志（Plan V2 §3.7）。
    let _ = cli::logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();

    // --json 标志：控制输出格式（JSON 信封 vs 人类可读文本）
    let use_json = args.first().map_or(false, |a| a == "--json");
    let args: Vec<String> = args.into_iter().filter(|a| a != "--json").collect();

    if args.first().map_or(false, |a| a == "serve") {
        // 持久会话模式：stdin/stdout JSON-line 循环
        cli::serve::run_stdio();
    } else {
        // 单次命令模式
        let response = cli::dispatch(&args);
        if use_json {
            // JSON 输出供桌面端解析
            println!(
                "{}",
                serde_json::to_string(&response).unwrap_or_else(|_| {
                    r#"{"ok":false,"error":{"code":"serialize_error","message":"序列化响应失败"}}"#
                        .to_string()
                })
            );
        } else {
            // 人类可读输出（无 --json 标志时的默认行为）
            if response.ok {
                if let Some(data) = &response.data {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(data).unwrap_or_default()
                    );
                } else {
                    println!("OK");
                }
            } else if let Some(err) = &response.error {
                eprintln!("错误 [{}]: {}", err.code, err.message);
                if let Some(suggestion) = &err.suggestion {
                    eprintln!("建议: {suggestion}");
                }
                std::process::exit(1);
            }
        }
    }
}
