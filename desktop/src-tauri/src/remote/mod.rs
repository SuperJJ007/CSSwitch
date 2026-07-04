//! 远程服务器管理模块。
//!
//! 通过 SSH 连接远程 Linux 服务器，执行 `csswitch-helper` CLI 来管理：
//! - 翻译代理的启停与状态监控
//! - 配置文件读写（~/.csswitch/config.json）
//! - Claude Science 沙箱管理
//! - 日志查看与诊断
//!
//! 架构参考 cc-switch-remote 的 `remote/` 模块，按 CSSwitch 需求大幅简化。

pub mod ssh;
pub mod store;
pub mod types;

// 重新导出常用类型和函数，方便外部模块使用。
pub use store::{delete_profile, load_profiles, upsert_profile, validate_profile};
