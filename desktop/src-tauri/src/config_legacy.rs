//! 旧固定槽配置（schema v1）的只读副本，仅供一次性迁移读取。生产代码不再写它。
//!
//! v1 = PR #4 「每家固定槽」形态：顶层 `provider` 指针 + `providers: {slot -> {key, base_url, model}}`。
//! v2（[`crate::config`]）改为用户自管命名 `profiles` 列表 + `active_id` 生效指针。
//! 迁移只读这里、写新结构，读完即弃；这些类型永不参与保存。
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 旧单槽配置（等价于旧 `config::ProviderCfg`）。字段全 optional，缺字段的更旧文件也能读。
#[derive(Deserialize, Clone, Default)]
pub struct ProviderCfgV1 {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
}

/// 旧顶层配置（等价于旧 `config::Config`）。端口/mode 复用新配置的默认函数保持一致。
#[derive(Deserialize, Clone)]
pub struct ConfigV1 {
    #[serde(default)]
    pub provider: String,
    #[serde(default = "crate::config::default_proxy_port")]
    pub proxy_port: u16,
    #[serde(default = "crate::config::default_sandbox_port")]
    pub sandbox_port: u16,
    #[serde(default)]
    pub secret: String,
    #[serde(default = "crate::config::default_mode")]
    pub mode: String,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCfgV1>,
}

/// schema v2 的只读/降级形态。v3 生产配置不直接复用这些类型，避免把新字段
/// 意外序列化给旧版本。
#[derive(Deserialize, Serialize, Clone, Default, Debug, PartialEq)]
pub struct ProfileV2 {
    pub id: String,
    pub name: String,
    pub template_id: String,
    pub category: String,
    pub api_format: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub website_url: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub icon_color: Option<String>,
    #[serde(default)]
    pub sort_index: Option<i64>,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct ConfigV2 {
    pub schema_version: u32,
    #[serde(default)]
    pub profiles: Vec<ProfileV2>,
    #[serde(default)]
    pub active_id: String,
    #[serde(default = "crate::config::default_proxy_port")]
    pub proxy_port: u16,
    #[serde(default = "crate::config::default_sandbox_port")]
    pub sandbox_port: u16,
    #[serde(default)]
    pub reuse_system_ssh: bool,
    #[serde(default)]
    pub secret: String,
    #[serde(default = "crate::config::default_mode")]
    pub mode: String,
    #[serde(default)]
    pub pending_notice: Option<String>,
}
