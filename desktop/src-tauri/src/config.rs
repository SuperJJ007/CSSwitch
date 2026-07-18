//! 本地配置读写：正式构建使用 `~/.csswitch/config.json`，Acceptance 构建使用
//! `~/.csswitch-acceptance/config.json`。多 profile + 多模型目录形态（schema v4）。
//!
//! 安全要求（对齐 spec §3 / §5.1，参考 CC Switch 的明文本地存储但加严文件安全）：
//!   - 目录 0700，文件 0600。
//!   - 读/写前 `lstat`（symlink_metadata）拒绝符号链接，绝不跟随写到别处或读到别处。
//!   - 写用「临时文件（O_CREAT|O_EXCL, 0600）+ 原子 rename」，避免半写与竞态。
//!   - profile key 明文存盘（用户已知悉），但**绝不进日志**；回显给前端只给掩码（末 4 位）。
//!
//! 存储升级：schema_version 探测 + v1（旧固定槽）→ canonical v2 → v3 → v4，
//! 迁移留不可覆盖的版本备份，普通覆盖前留滚动 `config.json.bak`，
//! 清 key / 删 profile 后净化滚动备份（旧明文 key 不可从 .bak 恢复）。
//!
//! 所有函数以显式 `dir` 参数工作，便于用临时目录做无副作用的单元测试；
//! 生产代码用 [`default_dir`]；目录名由编译期构建变体固定，不能由运行时输入改写。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::model_catalog::{ModelRoute, RoleBindings};
use crate::provider_contracts::{CredentialSource, ModelPolicy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

struct ConfigAccessState {
    downgrade_terminal: bool,
}

static CONFIG_ACCESS: std::sync::Mutex<ConfigAccessState> =
    std::sync::Mutex::new(ConfigAccessState {
        downgrade_terminal: false,
    });

fn config_access() -> std::sync::MutexGuard<'static, ConfigAccessState> {
    CONFIG_ACCESS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn ensure_config_access_open(access: &ConfigAccessState) -> io::Result<()> {
    if access.downgrade_terminal {
        Err(io::Error::other(
            "配置已降为 v2，CSSwitch 正在终态退出；拒绝再次读取或写入，避免自动迁回 v3。",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn default_proxy_port() -> u16 {
    18991
}
pub(crate) fn default_sandbox_port() -> u16 {
    8990
}
pub(crate) fn default_mode() -> String {
    "proxy".to_string()
}

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

/// 当前配置 schema 版本。>4 的文件由更新版本 app 写入，本版本拒绝启动（不误改）。
pub const CURRENT_SCHEMA_VERSION: u32 = 4;

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBindingCommit {
    pub profile_id: String,
    pub route_fp: String,
    pub catalog_fp: String,
    pub binding_fp: String,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GatewayRuntimeJournalIdentity {
    pub provider: String,
    pub shim: String,
    pub launch_id: String,
    pub catalog_fp: String,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeTransactionJournal {
    pub transaction_id: String,
    pub target_profile_id: String,
    pub stage: String,
    pub previous_binding: Option<RuntimeBindingCommit>,
    #[serde(default)]
    pub previous_gateway: Option<GatewayRuntimeJournalIdentity>,
}

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

/// 一条命名配置。API key profile 的 key 明文存盘、只回掩码；OAuth profile 只存固定 opaque ref。
/// 运行行为由 `template_id + api_format` 经 provider-contract catalog 派生，不靠展示字段猜身份。
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub template_id: String,
    pub category: String,
    pub api_format: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// v3 单模型的进程内兼容影子。v4 canonical 配置不再序列化该字段；
    /// load/normalize 从 default_model_route_id 回填，旧调用点在分模块迁移期间仍可读。
    #[serde(default, skip_serializing)]
    pub model: String,
    #[serde(default)]
    pub model_catalog: Vec<ModelRoute>,
    #[serde(default)]
    pub default_model_route_id: String,
    #[serde(default)]
    pub role_bindings: RoleBindings,
    #[serde(default)]
    pub credential_source: CredentialSource,
    #[serde(default)]
    pub credential_ref: Option<String>,
    #[serde(default)]
    pub model_policy: ModelPolicy,
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
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// 顶层配置。字段都有默认值，缺字段的旧文件也能读。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Config {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// 生效 profile 的 id；空=无生效配置（运行时据此停代理、要求用户选）。
    #[serde(default)]
    pub active_id: String,
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
    #[serde(default = "default_sandbox_port")]
    pub sandbox_port: u16,
    /// 用户显式授权隔离 Science 通过系统 OpenSSH 读取 `~/.ssh/config`。
    /// 默认关闭；不复制或链接 `.ssh`，只在启动时注入受控 PATH wrapper。
    #[serde(default)]
    pub reuse_system_ssh: bool,
    /// 非官方 Codex → Science 桥接实验开关。默认关闭；关闭不删除 profile 或本地 OAuth。
    #[serde(default)]
    pub experimental_codex_enabled: bool,
    /// Codex 专用网络路由。v3 已发布前追加为 serde(default)，旧 v3 文件自动采用 auto。
    #[serde(default)]
    pub codex_network: csswitch_codex_network::CodexNetworkSettings,
    /// 代理的 path-secret。**持久化**并跨代理重启/切 profile/重开 app 复用，
    /// 这样已在跑的沙箱（其 ANTHROPIC_BASE_URL 里嵌了该 secret）不会因代理换 secret 而 403。
    /// 首次为空，由后端生成一次后写回。
    #[serde(default)]
    pub secret: String,
    /// 运行模式："proxy"（第三方）| "official"（真实 Claude Science）。
    #[serde(default = "default_mode")]
    pub mode: String,
    /// 一次性迁移提示（#9 甲：回填默认模型后告知用户）。get_config 读后清空。
    #[serde(default)]
    pub pending_notice: Option<String>,
    /// Last fully healthy gateway + isolated Science binding. Contains hashes
    /// and public identities only; never credentials, endpoints, URLs or prompts.
    #[serde(default)]
    pub runtime_binding: Option<RuntimeBindingCommit>,
    #[serde(default)]
    pub runtime_transaction: Option<RuntimeTransactionJournal>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            schema_version: CURRENT_SCHEMA_VERSION,
            profiles: Vec::new(),
            active_id: String::new(),
            proxy_port: default_proxy_port(),
            sandbox_port: default_sandbox_port(),
            reuse_system_ssh: false,
            experimental_codex_enabled: false,
            codex_network: csswitch_codex_network::CodexNetworkSettings::default(),
            secret: String::new(),
            mode: default_mode(),
            pending_notice: None,
            runtime_binding: None,
            runtime_transaction: None,
            extra: BTreeMap::new(),
        }
    }
}

pub(crate) fn require_template_enabled(cfg: &Config, template_id: &str) -> Result<(), String> {
    if template_id == "codex" && !cfg.experimental_codex_enabled {
        return Err(
            "Codex 桥接是实验功能，当前未启用。请先在“设置 > Codex 账号与连接”中显式开启。".into(),
        );
    }
    Ok(())
}

impl Config {
    /// 当前生效 profile（active_id 空或悬空 → None）。
    pub fn active_profile(&self) -> Option<&Profile> {
        if self.active_id.is_empty() {
            return None;
        }
        self.profile_by_id(&self.active_id)
    }
    pub fn profile_by_id(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }
    pub fn profile_by_id_mut(&mut self, id: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }
}

/// 16 字节随机 → 32 hex 字符。/dev/urandom（unix）；不可用时退回时间纳秒。
pub fn new_id() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:032x}")
}

/// epoch 毫秒（用作 created_at / sort_index 初值）。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------- 版本探测 ----------
#[derive(Debug, Clone, PartialEq)]
pub enum VersionKind {
    Legacy,
    V2,
    V3,
    V4,
    TooNew(u32),
}

#[derive(Deserialize)]
struct VersionProbe {
    #[serde(default)]
    schema_version: u32,
}

/// 先只解析 schema_version 判版本，避免用「必填字段缺失」误判旧文件。
/// <2（含缺失=0）→ Legacy；==2 → V2；==3 → V3；==4 → V4；>4 → TooNew。
pub fn detect_version(data: &[u8]) -> io::Result<VersionKind> {
    let probe: VersionProbe = serde_json::from_slice(data).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config.json 解析失败：{e}"),
        )
    })?;
    Ok(match probe.schema_version {
        v if v < 2 => VersionKind::Legacy,
        2 => VersionKind::V2,
        3 => VersionKind::V3,
        v if v == CURRENT_SCHEMA_VERSION => VersionKind::V4,
        v => VersionKind::TooNew(v),
    })
}

/// 旧固定槽 → 新 profile 列表。空槽（key/base_url/model 全空）跳过；
/// 旧 provider 指针命中已迁 profile → active_id 指它，否则 ""（不静默选第一条）。
pub fn migrate_v1_to_v2(
    mut legacy: crate::config_legacy::ConfigV1,
) -> crate::config_legacy::ConfigV2 {
    // 先把遗留裸 relay 槽归位到 relay-<preset>。
    crate::templates::migrate_legacy_relay(&mut legacy.providers, &mut legacy.provider);
    let ts = now_ms();
    let mut profiles = Vec::new();
    let mut active_id = String::new();
    for (i, (slot, pc)) in legacy.providers.iter().enumerate() {
        if pc.key.is_empty() && pc.base_url.is_empty() && pc.model.is_empty() {
            continue;
        }
        let tid = crate::templates::template_id_for_legacy_slot(slot);
        let tpl = crate::templates::by_id(tid);
        let id = new_id();
        let base_url = if pc.base_url.is_empty() {
            tpl.map(|t| t.base_url.to_string()).unwrap_or_default()
        } else {
            pc.base_url.clone()
        };
        profiles.push(crate::config_legacy::ProfileV2 {
            id: id.clone(),
            name: tpl
                .map(|t| t.name.to_string())
                .unwrap_or_else(|| slot.clone()),
            template_id: tid.to_string(),
            category: tpl
                .map(|t| t.category.to_string())
                .unwrap_or_else(|| "custom".into()),
            api_format: tpl
                .map(|t| t.api_format.to_string())
                .unwrap_or_else(|| "anthropic".into()),
            base_url,
            api_key: pc.key.clone(),
            model: pc.model.clone(),
            website_url: tpl.map(|t| t.website_url.to_string()),
            icon: tpl.map(|t| t.icon.to_string()),
            icon_color: tpl.map(|t| t.icon_color.to_string()),
            sort_index: Some(i as i64),
            created_at: Some(ts),
            notes: None,
        });
        if *slot == legacy.provider {
            active_id = id;
        }
    }
    crate::config_legacy::ConfigV2 {
        schema_version: 2,
        profiles,
        active_id,
        proxy_port: legacy.proxy_port,
        sandbox_port: legacy.sandbox_port,
        reuse_system_ssh: false,
        secret: legacy.secret,
        mode: legacy.mode,
        pending_notice: None,
    }
}

pub fn migrate_v2_to_v3(
    v2: crate::config_legacy::ConfigV2,
) -> io::Result<crate::config_legacy::ConfigV3> {
    let mut profiles = Vec::with_capacity(v2.profiles.len());
    for p in v2.profiles {
        let template_id = if crate::templates::by_id(&p.template_id).is_some() {
            p.template_id
        } else {
            "custom".to_string()
        };
        let api_format = if p.api_format.trim().is_empty() {
            crate::templates::by_id(&template_id)
                .map(|template| template.api_format.to_string())
                .unwrap_or_else(|| "anthropic".to_string())
        } else {
            p.api_format
        };
        let contract = crate::provider_contracts::contract_for(&template_id, &api_format)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let model_policy = if contract.default_model_policy == ModelPolicy::DynamicCatalog {
            crate::config_legacy::ModelPolicyV3::DynamicCatalog
        } else if matches!(template_id.as_str(), "deepseek" | "qwen") {
            crate::config_legacy::ModelPolicyV3::OptionalFixed
        } else {
            crate::config_legacy::ModelPolicyV3::RequiredFixed
        };
        profiles.push(crate::config_legacy::ProfileV3 {
            id: p.id,
            name: p.name,
            template_id,
            category: p.category,
            api_format,
            base_url: p.base_url,
            api_key: p.api_key,
            model: p.model,
            credential_source: contract.default_credential_source,
            credential_ref: None,
            model_policy,
            website_url: p.website_url,
            icon: p.icon,
            icon_color: p.icon_color,
            sort_index: p.sort_index,
            created_at: p.created_at,
            notes: p.notes,
            extra: BTreeMap::new(),
        });
    }
    Ok(crate::config_legacy::ConfigV3 {
        schema_version: 3,
        profiles,
        active_id: v2.active_id,
        proxy_port: v2.proxy_port,
        sandbox_port: v2.sandbox_port,
        reuse_system_ssh: v2.reuse_system_ssh,
        experimental_codex_enabled: false,
        codex_network: csswitch_codex_network::CodexNetworkSettings::default(),
        secret: v2.secret,
        mode: v2.mode,
        pending_notice: v2.pending_notice,
        extra: BTreeMap::new(),
    })
}

fn legacy_native_model<'a>(template_id: &str, model: &'a str) -> Option<&'a str> {
    match (template_id, model.trim()) {
        ("deepseek", "claude-opus-4-8") => Some("deepseek-v4-pro"),
        ("deepseek", "claude-sonnet-5" | "claude-sonnet-4-6" | "claude-haiku-4-5") => {
            Some("deepseek-v4-flash")
        }
        ("qwen", "claude-opus-4-8") => Some("qwen3.7-max"),
        ("qwen", "claude-sonnet-5" | "claude-sonnet-4-6") => Some("qwen-plus-latest"),
        ("qwen", "claude-haiku-4-5") => Some("qwen-turbo"),
        (_, "") => None,
        (_, value) => Some(value),
    }
}

fn set_catalog_default(
    routes: &mut [ModelRoute],
    default_selector: &mut String,
    upstream_model: &str,
) -> bool {
    if let Some(index) = routes
        .iter()
        .position(|route| route.upstream_model == upstream_model)
    {
        routes.swap(0, index);
        *default_selector = routes[0].selector_id.clone();
        true
    } else {
        false
    }
}

fn append_notice(existing: Option<String>, next: String) -> Option<String> {
    Some(match existing {
        Some(existing) if !existing.trim().is_empty() => format!("{existing}\n{next}"),
        _ => next,
    })
}

pub fn migrate_v3_to_v4(v3: crate::config_legacy::ConfigV3) -> io::Result<Config> {
    let mut profiles = Vec::with_capacity(v3.profiles.len());
    let mut incomplete_ids = BTreeSet::new();
    for mut p in v3.profiles {
        if crate::templates::by_id(&p.template_id).is_none() {
            p.template_id = "custom".into();
        }
        if p.api_format.trim().is_empty() {
            p.api_format = crate::templates::by_id(&p.template_id)
                .map(|template| template.api_format.to_string())
                .unwrap_or_else(|| "anthropic".into());
        }
        let contract = crate::provider_contracts::contract_for(&p.template_id, &p.api_format)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let dynamic = contract.default_model_policy == ModelPolicy::DynamicCatalog;
        if (p.model_policy == crate::config_legacy::ModelPolicyV3::DynamicCatalog) != dynamic {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "profile `{}` 的 v3 model_policy 与 provider contract 不一致",
                    p.id
                ),
            ));
        }
        let (mut model_catalog, mut default_model_route_id, role_bindings) = if dynamic {
            (Vec::new(), String::new(), RoleBindings::default())
        } else if matches!(p.template_id.as_str(), "deepseek" | "qwen") {
            let (mut routes, mut default, bindings) =
                crate::model_catalog::preset_catalog(&p.template_id)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let raw = p.model.trim();
            if let Some(index) = routes
                .iter()
                .position(|route| !raw.is_empty() && route.selector_id == raw)
            {
                routes.swap(0, index);
                default = routes[0].selector_id.clone();
            } else if let Some(legacy) = legacy_native_model(&p.template_id, &p.model) {
                if !set_catalog_default(&mut routes, &mut default, legacy) {
                    let (manual, manual_default, _) = crate::model_catalog::single_route_catalog(
                        &crate::model_catalog::namespace_for(&p.template_id, &p.api_format),
                        legacy,
                        None,
                        None,
                    )
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    routes.insert(0, manual.into_iter().next().expect("single route"));
                    default = manual_default;
                }
            }
            (routes, default, bindings)
        } else {
            if let Some(model) = legacy_native_model(&p.template_id, &p.model) {
                crate::model_catalog::single_route_catalog(
                    &crate::model_catalog::namespace_for(&p.template_id, &p.api_format),
                    model,
                    None,
                    None,
                )
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            } else {
                incomplete_ids.insert(p.id.clone());
                (Vec::new(), String::new(), RoleBindings::default())
            }
        };
        let model = model_catalog
            .iter()
            .find(|route| route.selector_id == default_model_route_id)
            .map(|route| route.upstream_model.clone())
            .unwrap_or_default();
        profiles.push(Profile {
            id: p.id,
            name: p.name,
            template_id: p.template_id,
            category: p.category,
            api_format: p.api_format,
            base_url: p.base_url,
            api_key: p.api_key,
            model,
            model_catalog: std::mem::take(&mut model_catalog),
            default_model_route_id: std::mem::take(&mut default_model_route_id),
            role_bindings,
            credential_source: p.credential_source,
            credential_ref: p.credential_ref,
            model_policy: if dynamic {
                ModelPolicy::DynamicCatalog
            } else {
                ModelPolicy::SavedCatalog
            },
            website_url: p.website_url,
            icon: p.icon,
            icon_color: p.icon_color,
            sort_index: p.sort_index,
            created_at: p.created_at,
            notes: p.notes,
            extra: p.extra,
        });
    }
    let incomplete_active = incomplete_ids.contains(&v3.active_id);
    let pending_notice = if incomplete_ids.is_empty() {
        v3.pending_notice
    } else {
        append_notice(
            v3.pending_notice,
            format!(
                "{} 个旧静态配置缺少模型目录，已保留为未完成配置{}。",
                incomplete_ids.len(),
                if incomplete_active {
                    "；原生效配置已安全取消激活"
                } else {
                    ""
                }
            ),
        )
    };
    Ok(Config {
        schema_version: CURRENT_SCHEMA_VERSION,
        profiles,
        active_id: if incomplete_active {
            String::new()
        } else {
            v3.active_id
        },
        proxy_port: v3.proxy_port,
        sandbox_port: v3.sandbox_port,
        reuse_system_ssh: v3.reuse_system_ssh,
        experimental_codex_enabled: v3.experimental_codex_enabled,
        codex_network: v3.codex_network,
        secret: v3.secret,
        mode: v3.mode,
        pending_notice,
        runtime_binding: None,
        runtime_transaction: None,
        extra: v3.extra,
    })
}

#[cfg(not(feature = "acceptance-build"))]
pub(crate) const CONFIG_DIR_NAME: &str = ".csswitch";
#[cfg(feature = "acceptance-build")]
pub(crate) const CONFIG_DIR_NAME: &str = ".csswitch-acceptance";

fn default_dir_from_home(home: &Path) -> PathBuf {
    home.join(CONFIG_DIR_NAME)
}

/// 构建变体固定的配置目录。正式构建为 `$HOME/.csswitch`，Acceptance 为
/// `$HOME/.csswitch-acceptance`；两者不会因 Finder 使用同一个 HOME 而互相迁移配置。
pub fn default_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    default_dir_from_home(&home)
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

const MAX_CONFIG_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// 若 path 存在且是符号链接则报错（不跟随）。path 不存在返回 Ok。
pub(crate) fn assert_not_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("拒绝符号链接（防跟随写/读到别处）：{}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 确保配置目录存在且是普通目录、权限 0700。目录是符号链接则拒绝。
fn ensure_dir(dir: &Path) -> io::Result<()> {
    assert_not_symlink(dir)?;
    if !dir.exists() {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// 配置文件的所有关键操作都锚定到同一个已打开目录描述符。即使路径名随后被
/// rename/替换，openat/renameat/linkat 仍只作用于最初审计过的目录。
struct SecureDir {
    file: fs::File,
    path: PathBuf,
    normalize_file_permissions: bool,
}

impl SecureDir {
    fn open(path: &Path, create: bool) -> io::Result<Self> {
        Self::open_with_policy(path, create, true)
    }

    /// 用户自己选择的 export 父目录必须已存在，且 CSSwitch 不得擅自 chmod 它。
    fn open_unmanaged(path: &Path) -> io::Result<Self> {
        Self::open_with_policy(path, false, false)
    }

    fn open_with_policy(
        path: &Path,
        create: bool,
        normalize_permissions: bool,
    ) -> io::Result<Self> {
        assert_not_symlink(path)?;
        if create {
            ensure_dir(path)?;
        }
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(path)?;
        if !file.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("配置目录不是目录：{}", path.display()),
            ));
        }
        if normalize_permissions {
            file.set_permissions(fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            normalize_file_permissions: normalize_permissions,
        })
    }

    fn name(name: &str) -> io::Result<CString> {
        if name.is_empty() || name.as_bytes().contains(&b'/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "配置目录内部文件名非法",
            ));
        }
        CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "配置目录内部文件名包含 NUL"))
    }

    fn read_regular_snapshot(&self, name: &str) -> io::Result<Option<(Vec<u8>, u32)>> {
        let name = Self::name(name)?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            if error.raw_os_error() == Some(libc::ELOOP) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "拒绝配置目录内的符号链接",
                ));
            }
            return Err(error);
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "拒绝配置目录内的非普通文件",
            ));
        }
        if self.normalize_file_permissions && metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "拒绝配置目录内具有多个 hard link 的文件",
            ));
        }
        if metadata.len() > MAX_CONFIG_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "配置或备份文件过大",
            ));
        }
        let mut mode = metadata.permissions().mode() & 0o777;
        if self.normalize_file_permissions {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            mode = 0o600;
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_CONFIG_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONFIG_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "配置或备份文件过大",
            ));
        }
        Ok(Some((bytes, mode)))
    }

    /// 只确认模块自有 pending 名是否为普通文件；不 chmod、不要求单 hard link。
    fn regular_exists_allow_hardlinks(&self, name: &str) -> io::Result<bool> {
        let name = Self::name(name)?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(false);
            }
            if error.raw_os_error() == Some(libc::ELOOP) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "拒绝版本备份 pending 符号链接",
                ));
            }
            return Err(error);
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "拒绝非普通版本备份 pending 文件",
            ));
        }
        Ok(true)
    }

    fn read_regular(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        self.read_regular_snapshot(name)
            .map(|snapshot| snapshot.map(|(bytes, _)| bytes))
    }

    fn create_new(&self, name: &str) -> io::Result<fs::File> {
        let name = Self::name(name)?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let from = Self::name(from)?;
        let to = Self::name(to)?;
        let result = unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                from.as_ptr(),
                self.file.as_raw_fd(),
                to.as_ptr(),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn link(&self, from: &str, to: &str) -> io::Result<()> {
        let from = Self::name(from)?;
        let to = Self::name(to)?;
        let result = unsafe {
            libc::linkat(
                self.file.as_raw_fd(),
                from.as_ptr(),
                self.file.as_raw_fd(),
                to.as_ptr(),
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn unlink(&self, name: &str) -> io::Result<()> {
        let name = Self::name(name)?;
        let result = unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn display_path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn same_directory(&self, other: &Self) -> io::Result<bool> {
        let left = self.file.metadata()?;
        let right = other.file.metadata()?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
}

// ---------- 备份 ----------
/// 迁移前备份旧 config.json → config.json.v1.bak。源不存在 / 备份失败 → Err（中止迁移）。
#[cfg(test)]
pub fn write_migration_backup(dir: &Path) -> io::Result<()> {
    let secure = SecureDir::open(dir, false)?;
    let data = secure
        .read_regular("config.json")?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "config.json 不存在"))?;
    write_versioned_backup_bytes_in(&secure, 1, &data).map(|_| ())
}

fn backup_suffix() -> String {
    let millis = now_ms();
    let id = new_id();
    format!("{millis}-{}", &id[..8])
}

fn backup_content_suffix(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 版本迁移备份：固定名存在且内容相同就复用；内容不同时写唯一后缀，永不覆盖。
/// temp 在同目录完整 fsync 后用 hard_link 原子发布，link 本身具有 O_EXCL 语义。
#[cfg(test)]
fn write_versioned_backup_bytes(dir: &Path, version: u32, bytes: &[u8]) -> io::Result<PathBuf> {
    let secure = SecureDir::open(dir, true)?;
    write_versioned_backup_bytes_in(&secure, version, bytes)
}

fn write_versioned_backup_bytes_in(
    secure: &SecureDir,
    version: u32,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let primary = format!("config.json.v{version}.bak");
    let content_suffix = backup_content_suffix(bytes);
    let alternate = format!("config.json.v{version}.bak.{content_suffix}");
    let pending = format!(".config.json.v{version}.bak.pending-{content_suffix}");

    // link+dir-fsync 后若进程崩溃，pending hard link 可能在重启后重新出现。
    // 当前迁移输入在 backup 成功前不会改变，因此内容哈希能稳定定位并清理残留。
    if secure.regular_exists_allow_hardlinks(&pending)? {
        secure.unlink(&pending)?;
        secure.sync()?;
    }
    let target = match secure.read_regular(&primary)? {
        Some(existing) if existing == bytes => return Ok(secure.display_path(&primary)),
        Some(_) => alternate,
        None => primary.clone(),
    };
    if let Some(existing) = secure.read_regular(&target)? {
        if existing == bytes {
            return Ok(secure.display_path(&target));
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "版本备份哈希目标冲突",
        ));
    }
    let result = (|| -> io::Result<()> {
        let mut file = secure.create_new(&pending)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        secure.link(&pending, &target)?;
        if let Err(error) = secure.sync() {
            let _ = secure.unlink(&target);
            let _ = secure.unlink(&pending);
            let _ = secure.sync();
            return Err(error);
        }
        secure.unlink(&pending)?;
        secure.sync()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = secure.unlink(&pending);
    }
    result?;
    Ok(secure.display_path(&target))
}

/// 普通保存前的单份滚动备份 → config.json.bak。best-effort（调用方可忽略 Err），但写法仍原子/0600。
pub fn write_rolling_backup(dir: &Path) -> io::Result<()> {
    let access = config_access();
    ensure_config_access_open(&access)?;
    write_rolling_backup_unlocked(dir)
}

fn write_rolling_backup_unlocked(dir: &Path) -> io::Result<()> {
    let secure = SecureDir::open(dir, false)?;
    let data = secure
        .read_regular("config.json")?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "config.json 不存在"))?;
    atomic_write_named_bytes_in(&secure, "config.json.bak", &data, None, |secure| {
        secure.sync()
    })
}

/// 清 key / 删 profile 后净化滚动备份：直接删，避免旧明文 key 残留可恢复。
pub fn drop_rolling_backup(dir: &Path) {
    let access = config_access();
    if ensure_config_access_open(&access).is_err() {
        return;
    }
    if let Ok(secure) = SecureDir::open(dir, false) {
        let _ = secure.unlink("config.json.bak");
        let _ = secure.sync();
    }
}

/// 从 `dir/config.json` 读配置。文件不存在返回 [`Config::default`]。
/// v1/v2/v3 先完整解析、迁移、校验，再写不可覆盖的版本备份，最后只原子提交一次 v4。
/// v4 悬空 active_id 归一化为空。文件/目录是符号链接则报错（不跟随读）。
pub fn load_from(dir: &Path) -> io::Result<Config> {
    let access = config_access();
    ensure_config_access_open(&access)?;
    load_from_unlocked(dir)
}

fn load_from_unlocked(dir: &Path) -> io::Result<Config> {
    // 目录本身也不许是符号链接：否则攻击者把 ~/.csswitch 换成软链就能让读取跟随到别处。
    assert_not_symlink(dir)?;
    let secure = match SecureDir::open(dir, false) {
        Ok(secure) => secure,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error),
    };
    let data = match secure.read_regular("config.json")? {
        Some(data) => data,
        None => return Ok(Config::default()),
    };
    match detect_version(&data)? {
        VersionKind::TooNew(v) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config.json 由更新版本（schema {v}）写入，请升级 CSSwitch 后再打开。"),
        )),
        VersionKind::Legacy => {
            let legacy: crate::config_legacy::ConfigV1 =
                serde_json::from_slice(&data).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("旧 config 解析失败：{e}"),
                    )
                })?;
            let v2 = migrate_v1_to_v2(legacy);
            let canonical_v2 = serde_json::to_vec_pretty(&v2).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("v2 备份序列化失败：{error}"),
                )
            })?;
            let cfg = normalize_active(migrate_v3_to_v4(migrate_v2_to_v3(v2)?)?);
            validate_loaded_ports(&cfg)?;
            validate_profile_contracts(&cfg)?;
            write_versioned_backup_bytes_in(&secure, 1, &data)?;
            write_versioned_backup_bytes_in(&secure, 2, &canonical_v2)?;
            commit_migrated_config(&secure, &data, &cfg)?;
            Ok(cfg)
        }
        VersionKind::V2 => {
            let v2: crate::config_legacy::ConfigV2 =
                serde_json::from_slice(&data).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("v2 config.json 解析失败：{e}"),
                    )
                })?;
            let canonical_v2 = serde_json::to_vec_pretty(&v2).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("v2 备份序列化失败：{error}"),
                )
            })?;
            let cfg = normalize_active(migrate_v3_to_v4(migrate_v2_to_v3(v2)?)?);
            validate_loaded_ports(&cfg)?;
            validate_profile_contracts(&cfg)?;
            write_versioned_backup_bytes_in(&secure, 2, &canonical_v2)?;
            commit_migrated_config(&secure, &data, &cfg)?;
            Ok(cfg)
        }
        VersionKind::V3 => {
            let v3: crate::config_legacy::ConfigV3 =
                serde_json::from_slice(&data).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("v3 config.json 解析失败：{e}"),
                    )
                })?;
            let cfg = normalize_active(migrate_v3_to_v4(v3)?);
            validate_loaded_ports(&cfg)?;
            validate_profile_contracts(&cfg)?;
            write_versioned_backup_bytes_in(&secure, 3, &data)?;
            commit_migrated_config(&secure, &data, &cfg)?;
            Ok(cfg)
        }
        VersionKind::V4 => {
            let cfg: Config = serde_json::from_slice(&data).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("v4 config.json 解析失败：{e}"),
                )
            })?;
            let cfg = normalize_active(cfg);
            validate_loaded_ports(&cfg)?;
            validate_profile_contracts(&cfg)?;
            Ok(cfg)
        }
    }
}

fn commit_migrated_config(secure: &SecureDir, original: &[u8], cfg: &Config) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(cfg).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("v4 配置序列化失败：{error}"),
        )
    })?;
    let decoded: Config = serde_json::from_slice(&json).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("v4 配置回读验证失败：{error}"),
        )
    })?;
    let decoded = normalize_active(decoded);
    validate_loaded_ports(&decoded)?;
    validate_profile_contracts(&decoded)?;
    atomic_write_named_bytes_in(secure, "config.json", &json, Some(original), |secure| {
        secure.sync()
    })?;

    let published = secure
        .read_regular("config.json")?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "v4 配置提交后消失"));
    let post_check = published.and_then(|published| {
        if published != json {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "v4 配置提交后字节校验不一致",
            ));
        }
        let reread: Config = serde_json::from_slice(&published).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("v4 配置提交后解析失败：{error}"),
            )
        })?;
        let reread = normalize_active(reread);
        validate_loaded_ports(&reread)?;
        validate_profile_contracts(&reread)
    });
    if let Err(post_error) = post_check {
        return match atomic_write_named_bytes_in(
            secure,
            "config.json",
            original,
            Some(&json),
            |secure| secure.sync(),
        ) {
            Ok(()) => Err(post_error),
            Err(rollback_error) => Err(io::Error::other(format!(
                "v4 配置提交后验证失败：{post_error}；恢复原配置也失败：{rollback_error}"
            ))),
        };
    }
    Ok(())
}

fn validate_loaded_ports(cfg: &Config) -> io::Result<()> {
    validate_runtime_ports(cfg.proxy_port, cfg.sandbox_port).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config.json 端口无效：{e}"),
        )
    })
}

/// 加载后归一化两个不变式（spec §4）：
/// - `template_id` 未命中注册表 → 归一化为 `custom`（保留连接字段；据它派生 adapter/UI 能力）；
/// - `active_id` 指向不存在的 profile → 归一化为空（运行时据此停代理、要求用户选）。
fn normalize_active(mut cfg: Config) -> Config {
    for p in cfg.profiles.iter_mut() {
        if p.api_format.trim().is_empty() {
            p.api_format = crate::templates::by_id(&p.template_id)
                .map(|template| template.api_format.to_string())
                .unwrap_or_else(|| "anthropic".to_string());
        }
        let known_contract =
            crate::provider_contracts::contract_for(&p.template_id, &p.api_format).is_ok();
        if crate::templates::by_id(&p.template_id).is_none() && !known_contract {
            p.template_id = "custom".to_string();
        }
        p.model = p
            .model_catalog
            .iter()
            .find(|route| route.selector_id == p.default_model_route_id)
            .map(|route| route.upstream_model.clone())
            .unwrap_or_default();
    }
    if !cfg.active_id.is_empty() && cfg.profile_by_id(&cfg.active_id).is_none() {
        cfg.active_id.clear();
    }
    cfg
}

fn validate_profile_contracts(cfg: &Config) -> io::Result<()> {
    if cfg.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("只接受 canonical schema v{CURRENT_SCHEMA_VERSION}"),
        ));
    }
    let mut ids = BTreeSet::new();
    for reserved in [
        "schema_version",
        "profiles",
        "active_id",
        "proxy_port",
        "sandbox_port",
        "reuse_system_ssh",
        "experimental_codex_enabled",
        "codex_network",
        "secret",
        "mode",
        "pending_notice",
        "runtime_binding",
        "runtime_transaction",
    ] {
        if cfg.extra.contains_key(reserved) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config extension 与 canonical 字段冲突：{reserved}"),
            ));
        }
    }
    for reserved in ["mode", "proxy_url"] {
        if cfg.codex_network.extra.contains_key(reserved) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("codex_network extension 与 canonical 字段冲突：{reserved}"),
            ));
        }
    }
    for profile in &cfg.profiles {
        for reserved in [
            "id",
            "name",
            "template_id",
            "category",
            "api_format",
            "base_url",
            "api_key",
            "model",
            "model_catalog",
            "default_model_route_id",
            "role_bindings",
            "credential_source",
            "credential_ref",
            "model_policy",
            "website_url",
            "icon",
            "icon_color",
            "sort_index",
            "created_at",
            "notes",
        ] {
            if profile.extra.contains_key(reserved) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "profile `{}` extension 与 canonical 字段冲突：{reserved}",
                        profile.id
                    ),
                ));
            }
        }
        if profile.id.trim().is_empty() || profile.id.len() > 256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "profile id 不能为空且不得超过 256 字节",
            ));
        }
        if !ids.insert(profile.id.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("profile id 重复：{}", profile.id),
            ));
        }
        let contract =
            crate::provider_contracts::contract_for(&profile.template_id, &profile.api_format)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if !contract
            .credential_sources
            .contains(&profile.credential_source)
            || !contract.model_policies.contains(&profile.model_policy)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "profile `{}` 的 credential/model policy 不符合 provider contract",
                    profile.id
                ),
            ));
        }
        match profile.model_policy {
            ModelPolicy::DynamicCatalog => {
                if profile.template_id != "codex"
                    || !profile.model_catalog.is_empty()
                    || !profile.default_model_route_id.is_empty()
                    || !profile.role_bindings.all_empty()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("动态目录 profile `{}` 含静态目录或不是 Codex", profile.id),
                    ));
                }
            }
            ModelPolicy::SavedCatalog if profile.model_catalog.is_empty() => {
                if cfg.active_id == profile.id
                    || !profile.default_model_route_id.is_empty()
                    || !profile.role_bindings.all_empty()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("未完成静态 profile `{}` 不得激活或保存悬空绑定", profile.id),
                    ));
                }
            }
            ModelPolicy::SavedCatalog => {
                crate::model_catalog::validate_saved_catalog(
                    &profile.model_catalog,
                    &profile.default_model_route_id,
                    &profile.role_bindings,
                )
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("profile `{}` 模型目录无效：{error}", profile.id),
                    )
                })?;
            }
        }
        match profile.credential_source {
            CredentialSource::ApiKey if profile.credential_ref.is_some() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("API-key profile `{}` 不得保存 credential_ref", profile.id),
                ));
            }
            CredentialSource::CsswitchOauth => {
                if profile.credential_ref.as_deref() != Some("csswitch:codex:default")
                    || !profile.api_key.is_empty()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "OAuth profile `{}` 的 credential_ref 或 api_key 非法",
                            profile.id
                        ),
                    ));
                }
            }
            CredentialSource::None
                if profile.credential_ref.is_some() || !profile.api_key.is_empty() =>
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("无凭据 profile `{}` 不得保存 credential 数据", profile.id),
                ));
            }
            _ => {}
        }
    }
    if !cfg.active_id.is_empty() && !ids.contains(&cfg.active_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("active_id 指向不存在的 profile：{}", cfg.active_id),
        ));
    }
    Ok(())
}

/// 原子写 `dir/config.json`（0600）。目录/目标文件是符号链接则拒绝。
#[allow(dead_code)]
pub fn save_to(dir: &Path, cfg: &Config) -> io::Result<()> {
    let access = config_access();
    ensure_config_access_open(&access)?;
    save_to_unlocked(dir, cfg)
}

fn save_to_unlocked(dir: &Path, cfg: &Config) -> io::Result<()> {
    let secure = SecureDir::open(dir, true)?;
    save_to_secure(&secure, cfg)
}

fn save_to_secure(secure: &SecureDir, cfg: &Config) -> io::Result<()> {
    if cfg.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("只能保存 schema v{CURRENT_SCHEMA_VERSION} 配置"),
        ));
    }
    validate_loaded_ports(cfg)?;
    validate_profile_contracts(cfg)?;
    let json = serde_json::to_vec_pretty(cfg).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config 序列化失败：{e}"),
        )
    })?;

    atomic_write_named_bytes_in(secure, "config.json", &json, None, |secure| secure.sync())
}

fn atomic_write_config_bytes(dir: &Path, json: &[u8]) -> io::Result<()> {
    let secure = SecureDir::open(dir, true)?;
    atomic_write_named_bytes_in(&secure, "config.json", json, None, |secure| secure.sync())
}

#[derive(Debug)]
struct AtomicRollbackUncertain {
    commit: String,
    rollback: String,
}

impl std::fmt::Display for AtomicRollbackUncertain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "配置提交同步失败，且回滚失败：commit={}; rollback={}",
            self.commit, self.rollback
        )
    }
}

impl std::error::Error for AtomicRollbackUncertain {}

fn atomic_rollback_is_uncertain(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.is::<AtomicRollbackUncertain>())
}

/// 先发布临时文件，再持久化目录项。若发布后的同步失败，恢复发布前字节并再次
/// 同步，保证 `Err` 的可观察语义是目标文件未改变。测试可注入 commit sync 失败。
fn atomic_write_named_bytes_in<F>(
    secure: &SecureDir,
    target: &str,
    bytes: &[u8],
    expected_before: Option<&[u8]>,
    commit_sync: F,
) -> io::Result<()>
where
    F: FnOnce(&SecureDir) -> io::Result<()>,
{
    let before = secure.read_regular_snapshot(target)?;
    if let Some(expected) = expected_before {
        if before.as_ref().map(|(bytes, _)| bytes.as_slice()) != Some(expected) {
            return Err(io::Error::other("配置在迁移提交前被外部进程修改"));
        }
    }
    let suffix = backup_suffix();
    let tmp = format!(".{target}.tmp-{}-{suffix}", std::process::id());

    let mut file = secure.create_new(&tmp)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = secure.unlink(&tmp);
        return Err(error);
    }
    drop(file);

    match (before.as_ref(), secure.read_regular(target)) {
        (Some((expected, _)), Ok(Some(actual))) if expected == &actual => {}
        (None, Ok(None)) => {}
        (_, Ok(_)) => {
            let _ = secure.unlink(&tmp);
            return Err(io::Error::other("配置在提交前被并发修改"));
        }
        (_, Err(error)) => {
            let _ = secure.unlink(&tmp);
            return Err(error);
        }
    }

    if let Err(error) = secure.rename(&tmp, target) {
        let _ = secure.unlink(&tmp);
        return Err(error);
    }

    if let Err(commit_error) = commit_sync(secure) {
        let restore = if let Some((old_bytes, old_mode)) = before {
            let restore_tmp = format!(".{target}.restore-{}-{suffix}", std::process::id());
            let restore_result = (|| -> io::Result<()> {
                let mut restore_file = secure.create_new(&restore_tmp)?;
                restore_file.write_all(&old_bytes)?;
                restore_file.set_permissions(fs::Permissions::from_mode(old_mode))?;
                restore_file.sync_all()?;
                drop(restore_file);
                secure.rename(&restore_tmp, target)?;
                secure.sync()
            })();
            if restore_result.is_err() {
                let _ = secure.unlink(&restore_tmp);
            }
            restore_result
        } else {
            secure.unlink(target).and_then(|_| secure.sync())
        };
        if let Err(restore_error) = restore {
            return Err(io::Error::other(AtomicRollbackUncertain {
                commit: commit_error.to_string(),
                rollback: restore_error.to_string(),
            }));
        }
        return Err(commit_error);
    }
    Ok(())
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CodexDowngradeAction {
    ExportThenRemove,
    Remove,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct DowngradePreview {
    pub(crate) v2: crate::config_legacy::ConfigV2,
    pub(crate) exports: Vec<serde_json::Value>,
    pub(crate) fingerprint: String,
}

#[derive(Debug)]
pub(crate) struct DowngradeError {
    pub(crate) message: String,
    pub(crate) exit_required: bool,
}

impl DowngradeError {
    fn safe(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_required: false,
        }
    }

    fn commit(error: io::Error) -> Self {
        Self {
            exit_required: atomic_rollback_is_uncertain(&error),
            message: format!("v2 配置原子提交失败：{error}"),
        }
    }
}

impl From<String> for DowngradeError {
    fn from(message: String) -> Self {
        Self::safe(message)
    }
}

impl From<&str> for DowngradeError {
    fn from(message: &str) -> Self {
        Self::safe(message)
    }
}

fn latch_terminal_downgrade_outcome(
    access: &mut ConfigAccessState,
    result: &Result<Option<PathBuf>, DowngradeError>,
) {
    if result.is_ok() || result.as_ref().is_err_and(|error| error.exit_required) {
        access.downgrade_terminal = true;
    }
}

#[allow(dead_code)]
pub(crate) fn prepare_downgrade_to_v2(
    cfg: &Config,
    actions: &BTreeMap<String, CodexDowngradeAction>,
) -> Result<DowngradePreview, String> {
    validate_profile_contracts(cfg).map_err(|error| error.to_string())?;
    if !cfg.extra.is_empty()
        || !cfg.codex_network.extra.is_empty()
        || cfg.profiles.iter().any(|profile| {
            !profile.extra.is_empty()
                || !profile.role_bindings.extra.is_empty()
                || profile
                    .model_catalog
                    .iter()
                    .any(|route| !route.extra.is_empty())
        })
    {
        return Err("配置含当前版本不理解的扩展字段；为避免静默丢失，拒绝降级到 v2。".into());
    }
    let codex_ids: BTreeSet<String> = cfg
        .profiles
        .iter()
        .filter(|profile| profile.credential_source == CredentialSource::CsswitchOauth)
        .map(|profile| profile.id.clone())
        .collect();
    let action_ids: BTreeSet<String> = actions.keys().cloned().collect();
    if codex_ids != action_ids {
        return Err(
            "降级前必须为每个且仅每个 Codex profile 选择 export_then_remove 或 remove".into(),
        );
    }
    let mut profiles = Vec::new();
    let mut exports = Vec::new();
    for profile in &cfg.profiles {
        if profile.credential_source == CredentialSource::CsswitchOauth {
            if actions.get(&profile.id) == Some(&CodexDowngradeAction::ExportThenRemove) {
                exports.push(serde_json::json!({
                    "schema_version": 1,
                    "profile": {
                        "id": profile.id,
                        "name": profile.name,
                        "template_id": profile.template_id,
                        "category": profile.category,
                        "api_format": profile.api_format,
                        "model": profile.model,
                        "model_policy": profile.model_policy,
                        "website_url": profile.website_url,
                        "icon": profile.icon,
                        "icon_color": profile.icon_color,
                        "sort_index": profile.sort_index,
                        "created_at": profile.created_at,
                        "notes": profile.notes
                    }
                }));
            }
            continue;
        }
        let default_upstream = profile
            .model_catalog
            .iter()
            .find(|route| route.selector_id == profile.default_model_route_id)
            .map(|route| route.upstream_model.clone())
            .unwrap_or_default();
        if !profile.model_catalog.is_empty() {
            exports.push(serde_json::json!({
                "schema_version": 1,
                "kind": "saved_model_catalog",
                "profile": {
                    "id": profile.id,
                    "name": profile.name,
                    "template_id": profile.template_id,
                    "category": profile.category,
                    "api_format": profile.api_format,
                    "model_policy": profile.model_policy,
                    "default_model_route_id": profile.default_model_route_id,
                    "default_upstream_model": default_upstream,
                    "model_catalog": profile.model_catalog.iter().map(|route| serde_json::json!({
                        "selector_id": route.selector_id,
                        "display_name": route.display_name,
                        "upstream_model": route.upstream_model,
                        "supports_tools": route.supports_tools,
                    })).collect::<Vec<_>>(),
                    "role_bindings": {
                        "sonnet": profile.role_bindings.sonnet,
                        "opus": profile.role_bindings.opus,
                        "haiku": profile.role_bindings.haiku,
                        "fable": profile.role_bindings.fable,
                    },
                    "website_url": profile.website_url,
                    "icon": profile.icon,
                    "icon_color": profile.icon_color,
                    "sort_index": profile.sort_index,
                    "created_at": profile.created_at,
                    "notes": profile.notes,
                }
            }));
        }
        profiles.push(crate::config_legacy::ProfileV2 {
            id: profile.id.clone(),
            name: profile.name.clone(),
            template_id: profile.template_id.clone(),
            category: profile.category.clone(),
            api_format: profile.api_format.clone(),
            base_url: profile.base_url.clone(),
            api_key: profile.api_key.clone(),
            model: default_upstream,
            website_url: profile.website_url.clone(),
            icon: profile.icon.clone(),
            icon_color: profile.icon_color.clone(),
            sort_index: profile.sort_index,
            created_at: profile.created_at,
            notes: profile.notes.clone(),
        });
    }
    let active_id = if codex_ids.contains(&cfg.active_id) {
        String::new()
    } else {
        cfg.active_id.clone()
    };
    let fingerprint_bytes = serde_json::to_vec(cfg).map_err(|error| error.to_string())?;
    let mut fingerprint_hasher = Sha256::new();
    fingerprint_hasher.update(b"csswitch-v2-downgrade-preview-v1\0");
    fingerprint_hasher.update(&fingerprint_bytes);
    let fingerprint = format!("{:x}", fingerprint_hasher.finalize());
    Ok(DowngradePreview {
        v2: crate::config_legacy::ConfigV2 {
            schema_version: 2,
            profiles,
            active_id,
            proxy_port: cfg.proxy_port,
            sandbox_port: cfg.sandbox_port,
            reuse_system_ssh: cfg.reuse_system_ssh,
            secret: cfg.secret.clone(),
            mode: cfg.mode.clone(),
            pending_notice: cfg.pending_notice.clone(),
        },
        exports,
        fingerprint,
    })
}

/// 把当前 v3 原子降为 v2。调用方必须先停止受管 Codex 链路；本函数从不读取、
/// 删除或修改 CSSwitch 私有认证文件。若 action 要求 export，先把 bundle 原子持久化到调用方明确
/// 给出的目标，再提交 v2。两次提交之间崩溃只会留下“原配置 + 已完成 export”，不会
/// 出现 profile 已移除但 export 尚未落盘的数据丢失窗口。
#[allow(dead_code)]
pub(crate) fn downgrade_to_v2(
    dir: &Path,
    actions: &BTreeMap<String, CodexDowngradeAction>,
    export_destination: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    let access = config_access();
    ensure_config_access_open(&access).map_err(|error| error.to_string())?;
    downgrade_to_v2_unlocked(dir, actions, export_destination, None).map_err(|error| error.message)
}

/// Production terminal variant. It serializes against every config read/write,
/// commits v2, then latches the process closed before releasing the lock. Any
/// in-flight or later status/config command therefore finishes before commit or
/// fails without observing v2; none can trigger the normal v2 -> v3 migration.
pub(crate) fn downgrade_to_v2_and_latch(
    dir: &Path,
    actions: &BTreeMap<String, CodexDowngradeAction>,
    export_destination: Option<&Path>,
    expected_fingerprint: &str,
) -> Result<Option<PathBuf>, DowngradeError> {
    let mut access = config_access();
    ensure_config_access_open(&access).map_err(|error| DowngradeError::safe(error.to_string()))?;
    let result =
        downgrade_to_v2_unlocked(dir, actions, export_destination, Some(expected_fingerprint));
    latch_terminal_downgrade_outcome(&mut access, &result);
    result
}

fn downgrade_to_v2_unlocked(
    dir: &Path,
    actions: &BTreeMap<String, CodexDowngradeAction>,
    export_destination: Option<&Path>,
    expected_fingerprint: Option<&str>,
) -> Result<Option<PathBuf>, DowngradeError> {
    let cfg = load_from_unlocked(dir).map_err(|error| DowngradeError::safe(error.to_string()))?;
    let preview = prepare_downgrade_to_v2(&cfg, actions)?;
    if expected_fingerprint
        .is_some_and(|expected| expected.is_empty() || expected != preview.fingerprint)
    {
        return Err("配置在确认后发生变化；未导出、未降级，请重新预览并确认。".into());
    }
    let v2_bytes = serde_json::to_vec_pretty(&preview.v2)
        .map_err(|error| format!("v2 配置序列化失败：{error}"))?;
    let export_bytes = if preview.exports.is_empty() {
        None
    } else {
        Some(
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 2,
                "profiles": preview.exports
            }))
            .map_err(|error| format!("兼容性元数据导出序列化失败：{error}"))?,
        )
    };

    let export_path = match (export_bytes, export_destination) {
        (Some(bytes), Some(path)) => {
            if path == config_path(dir) {
                return Err("兼容性导出目标不得覆盖 config.json".into());
            }
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or("兼容性导出目标缺少父目录")?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("兼容性导出文件名必须是有效 UTF-8")?;
            let config_dir = SecureDir::open(dir, false)
                .map_err(|error| format!("打开 CSSwitch 配置目录失败：{error}"))?;
            let export_dir = SecureDir::open_unmanaged(parent)
                .map_err(|error| format!("打开兼容性导出父目录失败：{error}"))?;
            if config_dir
                .same_directory(&export_dir)
                .map_err(|error| format!("比较 export 目录失败：{error}"))?
            {
                return Err("兼容性导出不得写入 CSSwitch 配置目录或其路径别名".into());
            }
            atomic_write_named_bytes_in(&export_dir, name, &bytes, None, |secure| secure.sync())
                .map_err(|error| format!("Codex profile export 写入失败：{error}"))?;
            Some(path.to_path_buf())
        }
        (Some(_), None) => return Err("export_then_remove 必须提供 export 目标".into()),
        (None, Some(_)) => return Err("没有需要 export 的 Codex profile".into()),
        (None, None) => None,
    };

    // 所有 action、序列化与必需 export 先完整完成；之后才允许写 v2 config。
    write_rolling_backup_unlocked(dir).map_err(|error| format!("降级滚动备份失败：{error}"))?;
    atomic_write_config_bytes(dir, &v2_bytes).map_err(DowngradeError::commit)?;
    Ok(export_path)
}

/// 序列化的「读-改-写」：进程内全局写锁下 load → apply → save，避免并发命令
/// 各读一份旧 config、各改一个字段、互相覆盖。
pub fn update<F: FnOnce(&mut Config)>(dir: &Path, f: F) -> io::Result<Config> {
    let access = config_access();
    ensure_config_access_open(&access)?;
    let mut cfg = load_from_unlocked(dir)?;
    f(&mut cfg);
    save_to_unlocked(dir, &cfg)?;
    Ok(cfg)
}

/// Serialized fallible read-modify-write. If the caller rejects the in-memory
/// mutation, no config or rolling backup is written.
pub fn update_result<T, F>(dir: &Path, f: F) -> Result<T, String>
where
    F: FnOnce(&mut Config) -> Result<(T, bool), String>,
{
    let access = config_access();
    ensure_config_access_open(&access).map_err(|error| error.to_string())?;
    let mut cfg = load_from_unlocked(dir).map_err(|error| error.to_string())?;
    let (result, changed) = f(&mut cfg)?;
    if changed {
        save_to_unlocked(dir, &cfg).map_err(|error| error.to_string())?;
    }
    Ok(result)
}

/// 掩码：固定 4 个圆点 + 末 4 位（`••••tail`）。空 key 返回空串；≤4 位全遮。
/// 定长而非随 key 长度增长：长 key 的掩码不会在列表里撑出横向溢出（WKWebView 不给连续
/// 圆点断行，`word-break` 拦不住），且不泄漏 key 长度。绝不返回完整 key，是回显前端的唯一形式。
pub fn mask(key: &str) -> String {
    let n = key.chars().count();
    if n == 0 {
        String::new()
    } else if n <= 4 {
        "•".repeat(n)
    } else {
        let last4: String = key.chars().skip(n - 4).collect();
        format!("••••{last4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{symlink, FileTypeExt};

    fn tmpdir() -> PathBuf {
        // 每个测试用「进程 id + 线程 id」独立子目录，避免并行测试相互踩。
        let base = std::env::temp_dir().join(format!("csswitch-cfg-test-{}", std::process::id()));
        let d = base.join(format!("{:?}", std::thread::current().id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn mode_of(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn saved_profile(id: &str, template_id: &str, api_format: &str, upstream: &str) -> Profile {
        let (model_catalog, default_model_route_id, role_bindings) =
            crate::model_catalog::new_profile_catalog(template_id, api_format, Some(upstream))
                .unwrap();
        Profile {
            id: id.into(),
            template_id: template_id.into(),
            api_format: api_format.into(),
            model: upstream.into(),
            model_catalog,
            default_model_route_id,
            role_bindings,
            model_policy: ModelPolicy::SavedCatalog,
            ..Default::default()
        }
    }

    // ---------- A1: 结构 + 访问器 + new_id/now_ms ----------
    #[test]
    fn config_default_is_v4_empty() {
        let c = Config::default();
        assert_eq!(c.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(c.schema_version, 4);
        assert!(c.profiles.is_empty());
        assert_eq!(c.active_id, "");
        assert_eq!(c.proxy_port, 18991);
        assert!(!c.reuse_system_ssh);
        assert!(!c.experimental_codex_enabled);
        assert_eq!(
            c.codex_network.mode,
            csswitch_codex_network::CodexNetworkMode::Auto
        );
        assert!(c.codex_network.proxy_url.is_empty());
        assert_eq!(c.mode, "proxy");
    }

    #[test]
    fn default_dir_is_compile_time_isolated_by_build_variant() {
        let home = Path::new("/tmp/csswitch-home-contract");
        let got = default_dir_from_home(home);
        #[cfg(feature = "acceptance-build")]
        assert_eq!(got, home.join(".csswitch-acceptance"));
        #[cfg(not(feature = "acceptance-build"))]
        assert_eq!(got, home.join(".csswitch"));
    }

    #[test]
    fn experimental_codex_gate_is_default_off_and_provider_scoped() {
        let mut cfg = Config::default();
        assert!(super::require_template_enabled(&cfg, "codex").is_err());
        assert!(super::require_template_enabled(&cfg, "deepseek").is_ok());
        cfg.experimental_codex_enabled = true;
        assert!(super::require_template_enabled(&cfg, "codex").is_ok());
    }

    #[test]
    fn existing_v3_without_experimental_codex_flag_loads_disabled() {
        let d = tmpdir();
        fs::write(
            d.join("config.json"),
            br#"{"schema_version":3,"profiles":[],"active_id":"","proxy_port":18991,"sandbox_port":18765,"reuse_system_ssh":false,"secret":"","mode":"proxy","pending_notice":null}"#,
        )
        .unwrap();
        fs::set_permissions(d.join("config.json"), fs::Permissions::from_mode(0o600)).unwrap();

        let cfg = load_from(&d).unwrap();
        assert_eq!(cfg.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(!cfg.experimental_codex_enabled);
        assert_eq!(
            cfg.codex_network,
            csswitch_codex_network::CodexNetworkSettings::default()
        );
    }

    #[test]
    fn profile_accessors_by_id_and_active() {
        let p = Profile {
            id: "abc".into(),
            name: "DS".into(),
            template_id: "deepseek".into(),
            category: "cn_official".into(),
            api_format: "anthropic".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: "sk-1".into(),
            model: String::new(),
            ..Default::default()
        };
        let c = Config {
            profiles: vec![p.clone()],
            active_id: "abc".into(),
            ..Default::default()
        };
        assert_eq!(c.profile_by_id("abc").unwrap().name, "DS");
        assert!(c.profile_by_id("nope").is_none());
        assert_eq!(c.active_profile().unwrap().id, "abc");
        let c2 = Config {
            active_id: "".into(),
            ..c.clone()
        };
        assert!(c2.active_profile().is_none());
    }

    #[test]
    fn v3_empty_static_profile_is_preserved_incomplete_and_deactivated() {
        let v3 = crate::config_legacy::ConfigV3 {
            profiles: vec![crate::config_legacy::ProfileV3 {
                id: "p1".into(),
                name: "我的 GLM".into(),
                template_id: "glm".into(),
                category: "cn_official".into(),
                api_format: "anthropic".into(),
                model_policy: crate::config_legacy::ModelPolicyV3::RequiredFixed,
                ..Default::default()
            }],
            active_id: "p1".into(),
            schema_version: 3,
            ..crate::config_legacy::ConfigV3::default()
        };
        let cfg = migrate_v3_to_v4(v3).unwrap();
        assert!(cfg.profiles[0].model_catalog.is_empty());
        assert!(cfg.active_id.is_empty());
        assert!(cfg.pending_notice.unwrap().contains("取消激活"));
    }

    #[test]
    fn new_id_is_unique_hex_and_now_ms_positive() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(now_ms() > 0);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let d = tmpdir().join(".csswitch");
        let p = Profile {
            name: "DeepSeek".into(),
            category: "cn_official".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: "sk-abcdef1234".into(),
            ..saved_profile("id1", "deepseek", "anthropic", "deepseek-v4-pro")
        };
        let cfg = Config {
            profiles: vec![p],
            active_id: "id1".into(),
            proxy_port: 12345,
            ..Default::default()
        };
        save_to(&d, &cfg).unwrap();
        let got = load_from(&d).unwrap();
        assert_eq!(got, cfg);
        assert_eq!(got.active_profile().unwrap().api_key, "sk-abcdef1234");
    }

    #[test]
    fn load_rejects_invalid_runtime_ports() {
        let cases = [
            ("proxy_8765", 8765, 8990),
            ("sandbox_8765", 18991, 8765),
            ("proxy_zero", 0, 8990),
            ("sandbox_zero", 18991, 0),
            ("same_ports", 18991, 18991),
        ];
        for (name, proxy_port, sandbox_port) in cases {
            let d = tmpdir().join(format!(".csswitch-{name}"));
            fs::create_dir_all(&d).unwrap();
            fs::write(
                config_path(&d),
                format!(
                    r#"{{"schema_version":2,"profiles":[],"active_id":"","proxy_port":{proxy_port},"sandbox_port":{sandbox_port}}}"#
                ),
            )
            .unwrap();
            let err = load_from(&d).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{name}");
            assert!(
                err.to_string().contains("config.json 端口无效"),
                "error should identify invalid config ports for {name}: {err}"
            );
        }
    }

    #[test]
    fn load_rejects_legacy_invalid_ports_before_v2_save() {
        let d = tmpdir().join(".csswitch-legacy-bad-port");
        fs::create_dir_all(&d).unwrap();
        let legacy = r#"{
            "provider":"deepseek",
            "proxy_port":18991,
            "sandbox_port":8765,
            "secret":"sec",
            "mode":"proxy",
            "providers":{"deepseek":{"key":"sk-ds","base_url":"","model":""}}
        }"#;
        fs::write(config_path(&d), legacy).unwrap();
        let err = load_from(&d).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let after = fs::read_to_string(config_path(&d)).unwrap();
        assert!(
            !after.contains("\"schema_version\""),
            "invalid legacy config should not be saved as v2: {after}"
        );
        assert!(
            !d.join("config.json.v1.bak").exists(),
            "旧配置未通过完整校验时不得发布迁移备份"
        );
    }

    // ---------- A2: 版本探测 ----------
    #[test]
    fn detect_version_missing_field_is_legacy() {
        let d = br#"{"provider":"deepseek","providers":{}}"#;
        assert!(matches!(detect_version(d).unwrap(), VersionKind::Legacy));
    }
    #[test]
    fn detect_version_two_is_v2() {
        let d = br#"{"schema_version":2,"profiles":[],"active_id":""}"#;
        assert!(matches!(detect_version(d).unwrap(), VersionKind::V2));
    }
    #[test]
    fn detect_version_three_is_v3() {
        let d = br#"{"schema_version":3}"#;
        assert!(matches!(detect_version(d).unwrap(), VersionKind::V3));
    }
    #[test]
    fn detect_version_four_is_v4() {
        let d = br#"{"schema_version":4}"#;
        assert!(matches!(detect_version(d).unwrap(), VersionKind::V4));
    }
    #[test]
    fn detect_version_garbage_errors() {
        assert!(detect_version(b"not json").is_err());
    }

    // ---------- A4: 迁移 v1 → v2 ----------
    #[test]
    fn migrate_maps_slots_to_profiles_and_active() {
        use crate::config_legacy::{ConfigV1, ProviderCfgV1};
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "deepseek".to_string(),
            ProviderCfgV1 {
                key: "sk-ds".into(),
                base_url: "".into(),
                model: "".into(),
            },
        );
        providers.insert(
            "relay-glm".to_string(),
            ProviderCfgV1 {
                key: "glmk".into(),
                base_url: "https://open.bigmodel.cn/api/anthropic".into(),
                model: "glm-5".into(),
            },
        );
        providers.insert(
            "qwen".to_string(),
            ProviderCfgV1 {
                key: "".into(),
                base_url: "".into(),
                model: "".into(),
            },
        ); // 空槽
        let legacy = ConfigV1 {
            provider: "relay-glm".into(),
            proxy_port: 18991,
            sandbox_port: 8990,
            secret: "sec".into(),
            mode: "proxy".into(),
            providers,
        };
        let cfg = migrate_v1_to_v2(legacy);
        assert_eq!(cfg.schema_version, 2);
        assert_eq!(cfg.profiles.len(), 2, "空 qwen 槽跳过");
        let glm = cfg
            .profiles
            .iter()
            .find(|p| p.template_id == "glm")
            .unwrap();
        assert_eq!(glm.api_key, "glmk");
        assert_eq!(glm.base_url, "https://open.bigmodel.cn/api/anthropic");
        assert_eq!(glm.model, "glm-5");
        assert_eq!(glm.api_format, "anthropic");
        assert_eq!(
            cfg.active_id, glm.id,
            "旧 provider=relay-glm → 生效指该 profile"
        );
        assert_eq!(cfg.secret, "sec");
    }

    #[test]
    fn migrate_invalid_active_yields_empty() {
        use crate::config_legacy::{ConfigV1, ProviderCfgV1};
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "deepseek".to_string(),
            ProviderCfgV1 {
                key: "k".into(),
                base_url: "".into(),
                model: "".into(),
            },
        );
        // 旧 provider 指向空/不存在的槽 → active_id 必须为空（不静默选第一条）。
        let legacy = ConfigV1 {
            provider: "qwen".into(),
            proxy_port: 18991,
            sandbox_port: 8990,
            secret: "".into(),
            mode: "proxy".into(),
            providers,
        };
        let cfg = migrate_v1_to_v2(legacy);
        assert_eq!(cfg.profiles.len(), 1);
        assert_eq!(cfg.active_id, "", "非法 active → 空，等用户选");
    }

    #[test]
    fn migrate_legacy_bare_relay_slot() {
        use crate::config_legacy::{ConfigV1, ProviderCfgV1};
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "relay".to_string(),
            ProviderCfgV1 {
                key: "rk".into(),
                base_url: "https://open.bigmodel.cn/api/anthropic".into(),
                model: "".into(),
            },
        );
        let legacy = ConfigV1 {
            provider: "relay".into(),
            proxy_port: 18991,
            sandbox_port: 8990,
            secret: "".into(),
            mode: "proxy".into(),
            providers,
        };
        let cfg = migrate_v1_to_v2(legacy);
        let glm = cfg
            .profiles
            .iter()
            .find(|p| p.template_id == "glm")
            .unwrap();
        assert_eq!(glm.api_key, "rk");
        assert_eq!(cfg.active_id, glm.id);
    }

    // ---------- A5: 备份基础设施 ----------
    #[test]
    fn migration_backup_copies_and_is_0600() {
        let d = tmpdir().join(".csswitch");
        fs::create_dir_all(&d).unwrap();
        fs::write(config_path(&d), b"OLD-V1-BYTES").unwrap();
        write_migration_backup(&d).unwrap();
        let bak = d.join("config.json.v1.bak");
        assert_eq!(fs::read(&bak).unwrap(), b"OLD-V1-BYTES");
        assert_eq!(mode_of(&bak), 0o600);
    }
    #[test]
    fn migration_backup_missing_source_errors() {
        let d = tmpdir().join(".csswitch");
        fs::create_dir_all(&d).unwrap();
        assert!(write_migration_backup(&d).is_err());
    }
    #[test]
    fn rolling_backup_then_drop_removes_key_recoverability() {
        let d = tmpdir().join(".csswitch");
        fs::create_dir_all(&d).unwrap();
        fs::write(config_path(&d), br#"{"api_key":"sk-SECRET-TAIL"}"#).unwrap();
        write_rolling_backup(&d).unwrap();
        let bak = d.join("config.json.bak");
        assert!(fs::read_to_string(&bak).unwrap().contains("sk-SECRET-TAIL"));
        drop_rolling_backup(&d);
        assert!(
            !bak.exists(),
            "净化后滚动备份应删除，清了的 key 不可从 .bak 恢复"
        );
    }
    #[test]
    fn backup_rejects_symlinked_target() {
        let base = tmpdir();
        let d = base.join(".csswitch");
        fs::create_dir_all(&d).unwrap();
        fs::write(config_path(&d), b"X").unwrap();
        let elsewhere = base.join("elsewhere");
        fs::write(&elsewhere, b"ORIG").unwrap();
        symlink(&elsewhere, d.join("config.json.v1.bak")).unwrap();
        assert!(write_migration_backup(&d).is_err());
        assert_eq!(fs::read(&elsewhere).unwrap(), b"ORIG");
    }

    // ---------- A6: load_from 整合 ----------
    #[test]
    fn load_migrates_old_file_and_writes_v1_bak() {
        let d = tmpdir().join(".csswitch");
        fs::create_dir_all(&d).unwrap();
        fs::write(
            config_path(&d),
            br#"{"provider":"deepseek","providers":{"deepseek":{"key":"sk-x"}}}"#,
        )
        .unwrap();
        let cfg = load_from(&d).unwrap();
        assert_eq!(cfg.schema_version, 4);
        assert_eq!(cfg.profiles.len(), 1);
        assert_eq!(cfg.active_profile().unwrap().api_key, "sk-x");
        assert!(d.join("config.json.v1.bak").exists(), "迁移必须留 v1 备份");
        assert!(
            d.join("config.json.v2.bak").exists(),
            "迁移必须留 canonical v2 备份"
        );
        // 落盘后再读是 v4（幂等，不再迁移）。
        let again = load_from(&d).unwrap();
        assert_eq!(again, cfg);
        assert_eq!(again.schema_version, 4);
    }
    #[test]
    fn load_too_new_errors() {
        let d = tmpdir().join(".csswitch");
        fs::create_dir_all(&d).unwrap();
        fs::write(config_path(&d), br#"{"schema_version":9,"profiles":[]}"#).unwrap();
        let e = load_from(&d).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("更新版本"));
    }
    #[test]
    fn load_normalizes_dangling_active() {
        let d = tmpdir().join(".csswitch");
        let cfg = Config {
            active_id: "ghost".into(),
            profiles: vec![Profile {
                id: "real".into(),
                template_id: "deepseek".into(),
                api_format: "anthropic".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        fs::create_dir_all(&d).unwrap();
        fs::write(config_path(&d), serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
        let got = load_from(&d).unwrap();
        assert_eq!(got.active_id, "", "悬空 active → 归一化为空");
    }

    // ---------- MP-2 Minor [2]: template_id 未命中 → 归一 custom ----------
    #[test]
    fn load_normalizes_unknown_template_id_to_custom() {
        let d = tmpdir().join(".csswitch");
        let (model_catalog, default_model_route_id, role_bindings) =
            crate::model_catalog::single_route_catalog(
                "custom-anthropic",
                "relay-model-v1",
                None,
                None,
            )
            .unwrap();
        // 造一条 template_id 未命中注册表的 v2 profile（连接字段保留）。
        let cfg = Config {
            active_id: "p1".into(),
            profiles: vec![Profile {
                id: "p1".into(),
                name: "野模板".into(),
                template_id: "totally-unknown-xyz".into(),
                api_format: "anthropic".into(),
                base_url: "https://relay.example/claude".into(),
                api_key: "sk-x".into(),
                model: "relay-model-v1".into(),
                model_catalog,
                default_model_route_id,
                role_bindings,
                model_policy: ModelPolicy::SavedCatalog,
                ..Default::default()
            }],
            ..Default::default()
        };
        fs::create_dir_all(&d).unwrap();
        fs::write(config_path(&d), serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
        let got = load_from(&d).unwrap();
        let p = got.profile_by_id("p1").unwrap();
        assert_eq!(p.template_id, "custom", "未命中 template_id → 归一 custom");
        assert_eq!(p.base_url, "https://relay.example/claude", "连接字段保留");
        assert_eq!(p.api_key, "sk-x");
        assert_eq!(got.active_id, "p1", "active 仍有效，不被清空");
    }

    // ---------- 既有安全/权限不变量（保留） ----------
    #[test]
    fn load_missing_returns_default() {
        let d = tmpdir().join(".csswitch");
        let cfg = load_from(&d).unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.schema_version, 4);
        assert_eq!(cfg.proxy_port, 18991);
    }

    #[test]
    fn save_sets_dir_0700_and_file_0600() {
        let d = tmpdir().join(".csswitch");
        save_to(&d, &Config::default()).unwrap();
        assert_eq!(mode_of(&d), 0o700, "dir must be 0700");
        assert_eq!(mode_of(&config_path(&d)), 0o600, "file must be 0600");
    }

    #[test]
    fn load_resets_widened_perms_to_0600() {
        let d = tmpdir().join(".csswitch");
        save_to(&d, &Config::default()).unwrap();
        let p = config_path(&d);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        load_from(&d).unwrap();
        assert_eq!(mode_of(&p), 0o600, "load must reset perms to 0600");
    }

    #[test]
    fn save_rejects_symlinked_file_and_leaves_target_untouched() {
        let base = tmpdir();
        let d = base.join(".csswitch");
        fs::create_dir_all(&d).unwrap();
        let target = base.join("real-elsewhere.txt");
        fs::write(&target, b"ORIGINAL").unwrap();
        symlink(&target, config_path(&d)).unwrap();
        let err = save_to(&d, &Config::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&target).unwrap(), b"ORIGINAL");
    }

    #[test]
    fn load_rejects_symlinked_file() {
        let base = tmpdir();
        let d = base.join(".csswitch");
        fs::create_dir_all(&d).unwrap();
        let target = base.join("secret.txt");
        fs::write(&target, b"{\"schema_version\":2}").unwrap();
        symlink(&target, config_path(&d)).unwrap();
        let err = load_from(&d).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn load_rejects_fifo_without_blocking() {
        let d = tmpdir().join(".csswitch-fifo");
        fs::create_dir_all(&d).unwrap();
        let path = config_path(&d);
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let error = load_from(&d).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn versioned_backup_rejects_fifo_target_without_overwrite() {
        let d = tmpdir().join(".csswitch-backup-fifo");
        fs::create_dir_all(&d).unwrap();
        let target = d.join("config.json.v2.bak");
        let c_path = std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(write_versioned_backup_bytes(&d, 2, b"safe-bytes").is_err());
        assert!(fs::symlink_metadata(target).unwrap().file_type().is_fifo());
    }

    #[test]
    fn versioned_backup_recovers_published_pending_hardlink_after_crash() {
        let d = tmpdir().join(".csswitch-backup-recovery");
        fs::create_dir_all(&d).unwrap();
        let bytes = b"migration-source-bytes";
        let suffix = backup_content_suffix(bytes);
        let pending = d.join(format!(".config.json.v2.bak.pending-{suffix}"));
        let target = d.join("config.json.v2.bak");
        fs::write(&pending, bytes).unwrap();
        fs::hard_link(&pending, &target).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().nlink(), 2);

        let published = write_versioned_backup_bytes(&d, 2, bytes).unwrap();
        assert_eq!(published, target);
        assert!(!pending.exists());
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert_eq!(fs::metadata(&target).unwrap().nlink(), 1);
    }

    #[test]
    fn load_rejects_symlinked_dir() {
        let base = tmpdir();
        let realdir = base.join("realdir");
        fs::create_dir_all(&realdir).unwrap();
        fs::write(realdir.join("config.json"), b"{\"schema_version\":2}").unwrap();
        let link = base.join(".csswitch");
        symlink(&realdir, &link).unwrap();
        let err = load_from(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn ensure_dir_rejects_symlinked_dir() {
        let base = tmpdir();
        let realdir = base.join("realdir");
        fs::create_dir_all(&realdir).unwrap();
        let link = base.join(".csswitch");
        symlink(&realdir, &link).unwrap();
        let err = save_to(&link, &Config::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn no_tmp_file_left_after_save() {
        let d = tmpdir().join(".csswitch");
        save_to(&d, &Config::default()).unwrap();
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".config.json.tmp")
            })
            .collect();
        assert!(leftovers.is_empty(), "临时文件应已 rename 掉");
    }

    #[test]
    fn update_applies_and_persists() {
        let d = tmpdir().join(".csswitch");
        save_to(&d, &Config::default()).unwrap();
        update(&d, |c| {
            c.profiles.push(Profile {
                name: "Q".into(),
                ..saved_profile("id1", "qwen", "openai_chat", "qwen-plus-latest")
            });
            c.active_id = "id1".into();
        })
        .unwrap();
        let got = load_from(&d).unwrap();
        assert_eq!(got.active_id, "id1");
        assert_eq!(got.active_profile().unwrap().name, "Q");
    }

    #[test]
    fn secret_persists_and_survives_reload() {
        // path-secret 一旦生成必须持久化，代理重启/重开 app 仍是同一个值。
        let d = tmpdir().join(".csswitch");
        save_to(&d, &Config::default()).unwrap();
        assert!(load_from(&d).unwrap().secret.is_empty(), "初始应为空");
        update(&d, |c| c.secret = "deadbeef00112233".into()).unwrap();
        assert_eq!(load_from(&d).unwrap().secret, "deadbeef00112233");
        // 再改别的字段，secret 不受影响。
        update(&d, |c| c.proxy_port = 20000).unwrap();
        assert_eq!(load_from(&d).unwrap().secret, "deadbeef00112233");
    }

    #[test]
    fn v2_migration_preserves_api_key_profile_and_settings() {
        let d = tmpdir().join(".csswitch-v2-migration");
        fs::create_dir_all(&d).unwrap();
        let v2 = crate::config_legacy::ConfigV2 {
            schema_version: 2,
            profiles: vec![crate::config_legacy::ProfileV2 {
                id: "api-1".into(),
                name: "GLM".into(),
                template_id: "glm".into(),
                category: "cn_official".into(),
                api_format: "anthropic".into(),
                base_url: "https://open.bigmodel.cn/api/anthropic".into(),
                api_key: "sk-existing".into(),
                model: "glm-5.2".into(),
                ..Default::default()
            }],
            active_id: "api-1".into(),
            proxy_port: 19001,
            sandbox_port: 19002,
            reuse_system_ssh: true,
            secret: "persistent-secret".into(),
            mode: "proxy".into(),
            pending_notice: Some("keep-me".into()),
        };
        let canonical = serde_json::to_vec_pretty(&v2).unwrap();
        fs::write(config_path(&d), &canonical).unwrap();

        let migrated = load_from(&d).unwrap();
        assert_eq!(migrated.schema_version, 4);
        assert_eq!(migrated.active_id, "api-1");
        assert_eq!(migrated.proxy_port, 19001);
        assert_eq!(migrated.sandbox_port, 19002);
        assert!(migrated.reuse_system_ssh);
        assert_eq!(migrated.secret, "persistent-secret");
        assert_eq!(
            migrated.codex_network,
            csswitch_codex_network::CodexNetworkSettings::default()
        );
        let profile = migrated.active_profile().unwrap();
        assert_eq!(profile.api_key, "sk-existing");
        assert_eq!(profile.model, "glm-5.2");
        assert_eq!(profile.credential_source, CredentialSource::ApiKey);
        assert_eq!(profile.model_policy, ModelPolicy::SavedCatalog);
        assert_eq!(fs::read(d.join("config.json.v2.bak")).unwrap(), canonical);
    }

    #[test]
    fn v3_to_v4_preserves_unknown_fields_and_raw_backup_byte_for_byte() {
        let d = tmpdir().join(".csswitch-v3-extensions");
        fs::create_dir_all(&d).unwrap();
        let raw = br#"{
  "schema_version": 3,
  "profiles": [{
    "id": "qwen-legacy",
    "name": "Qwen",
    "template_id": "qwen",
    "category": "cn_official",
    "api_format": "openai_chat",
    "base_url": "https://dashscope.aliyuncs.com/compatible-mode/v1",
    "api_key": "test-only",
    "model": "claude-sonnet-5",
    "credential_source": "api_key",
    "model_policy": "optional_fixed",
    "future_profile": {"keep": 2}
  }],
  "active_id": "qwen-legacy",
  "proxy_port": 19031,
  "sandbox_port": 19032,
  "codex_network": {"mode": "auto", "proxy_url": "", "future_network": 3},
  "mode": "proxy",
  "future_top": [1, 2, 3]
}"#;
        fs::write(config_path(&d), raw).unwrap();
        let migrated = load_from(&d).unwrap();
        assert_eq!(migrated.extra["future_top"], serde_json::json!([1, 2, 3]));
        assert_eq!(migrated.profiles[0].extra["future_profile"]["keep"], 2);
        assert_eq!(migrated.codex_network.extra["future_network"], 3);
        assert_eq!(migrated.profiles[0].model, "qwen-plus-latest");
        assert_eq!(fs::read(d.join("config.json.v3.bak")).unwrap(), raw);
        let canonical: serde_json::Value =
            serde_json::from_slice(&fs::read(config_path(&d)).unwrap()).unwrap();
        assert_eq!(canonical["schema_version"], 4);
        assert!(canonical["profiles"][0].get("model").is_none());

        update(&d, |cfg| cfg.pending_notice = Some("unrelated".into())).unwrap();
        let again = load_from(&d).unwrap();
        assert_eq!(again.extra["future_top"], serde_json::json!([1, 2, 3]));
        assert_eq!(again.profiles[0].extra["future_profile"]["keep"], 2);
        assert_eq!(again.codex_network.extra["future_network"], 3);
        assert_eq!(fs::read(d.join("config.json.v3.bak")).unwrap(), raw);
    }

    #[test]
    fn v4_route_and_role_extensions_survive_unrelated_update() {
        let d = tmpdir().join(".csswitch-v4-route-extensions");
        let mut profile = saved_profile("p1", "qwen", "openai_chat", "qwen-plus-latest");
        profile.model_catalog[0]
            .extra
            .insert("future_route".into(), serde_json::json!({"keep": true}));
        profile
            .role_bindings
            .extra
            .insert("future_role".into(), serde_json::json!(7));
        let cfg = Config {
            profiles: vec![profile],
            active_id: "p1".into(),
            ..Default::default()
        };
        save_to(&d, &cfg).unwrap();
        update(&d, |cfg| cfg.proxy_port = 19041).unwrap();
        let got = load_from(&d).unwrap();
        assert_eq!(
            got.profiles[0].model_catalog[0].extra["future_route"]["keep"],
            true
        );
        assert_eq!(got.profiles[0].role_bindings.extra["future_role"], 7);
    }

    #[test]
    fn native_v3_model_variants_preserve_the_selected_upstream() {
        for (template_id, api_format, old_model, expected, expected_len) in [
            ("deepseek", "anthropic", "", "deepseek-v4-flash", 2),
            (
                "deepseek",
                "anthropic",
                "claude-haiku-4-5",
                "deepseek-v4-flash",
                2,
            ),
            (
                "deepseek",
                "anthropic",
                "deepseek-v4-pro",
                "deepseek-v4-pro",
                2,
            ),
            ("qwen", "openai_chat", "claude-opus-4-8", "qwen3.7-max", 3),
            ("qwen", "openai_chat", "qwen-turbo", "qwen-turbo", 3),
            (
                "qwen",
                "openai_chat",
                "future-qwen-exact",
                "future-qwen-exact",
                4,
            ),
        ] {
            let v3 = crate::config_legacy::ConfigV3 {
                schema_version: 3,
                profiles: vec![crate::config_legacy::ProfileV3 {
                    id: "p".into(),
                    name: "legacy".into(),
                    template_id: template_id.into(),
                    api_format: api_format.into(),
                    model: old_model.into(),
                    model_policy: crate::config_legacy::ModelPolicyV3::OptionalFixed,
                    credential_source: CredentialSource::ApiKey,
                    ..Default::default()
                }],
                ..Default::default()
            };
            let migrated = migrate_v3_to_v4(v3).unwrap();
            assert_eq!(
                migrated.profiles[0].model, expected,
                "{template_id}:{old_model}"
            );
            assert_eq!(migrated.profiles[0].model_catalog.len(), expected_len);
        }

        let selector = crate::model_catalog::selector_id_v1("qwen", "qwen-turbo");
        let v3 = crate::config_legacy::ConfigV3 {
            schema_version: 3,
            profiles: vec![crate::config_legacy::ProfileV3 {
                id: "p".into(),
                template_id: "qwen".into(),
                api_format: "openai_chat".into(),
                model: selector,
                model_policy: crate::config_legacy::ModelPolicyV3::OptionalFixed,
                credential_source: CredentialSource::ApiKey,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            migrate_v3_to_v4(v3).unwrap().profiles[0].model,
            "qwen-turbo"
        );
    }

    #[test]
    fn v2_backup_collision_never_overwrites_existing_bytes() {
        let d = tmpdir().join(".csswitch-v2-collision");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("config.json.v2.bak"), b"OLD-UNRELATED-BYTES").unwrap();
        fs::write(
            config_path(&d),
            br#"{"schema_version":2,"profiles":[],"active_id":"","proxy_port":18991,"sandbox_port":8990,"reuse_system_ssh":false,"secret":"","mode":"proxy","pending_notice":null}"#,
        )
        .unwrap();
        load_from(&d).unwrap();
        assert_eq!(
            fs::read(d.join("config.json.v2.bak")).unwrap(),
            b"OLD-UNRELATED-BYTES"
        );
        let alternates: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.json.v2.bak.")
            })
            .collect();
        assert_eq!(alternates.len(), 1);
    }

    fn codex_profile(id: &str) -> Profile {
        Profile {
            id: id.into(),
            name: "Codex account".into(),
            template_id: "codex".into(),
            category: "official".into(),
            api_format: "openai_responses".into(),
            credential_source: CredentialSource::CsswitchOauth,
            credential_ref: Some("csswitch:codex:default".into()),
            model_policy: ModelPolicy::DynamicCatalog,
            model: "gpt-test".into(),
            ..Default::default()
        }
    }

    #[test]
    fn downgrade_requires_an_action_for_every_codex_profile() {
        let cfg = Config {
            profiles: vec![codex_profile("c1"), codex_profile("c2")],
            active_id: "c1".into(),
            ..Default::default()
        };
        let actions = BTreeMap::from([("c1".into(), CodexDowngradeAction::Remove)]);
        assert!(prepare_downgrade_to_v2(&cfg, &actions).is_err());
    }

    #[test]
    fn downgrade_exports_only_metadata_and_preserves_api_key_profiles() {
        let root = tmpdir();
        let d = root.join(".csswitch-downgrade");
        let export_destination = root.join("codex-profiles-export.v1.json");
        let api = Profile {
            name: "DeepSeek".into(),
            category: "cn_official".into(),
            base_url: "https://api.deepseek.test/anthropic".into(),
            api_key: "sk-preserve".into(),
            ..saved_profile("api-1", "deepseek", "anthropic", "deepseek-v4-pro")
        };
        let cfg = Config {
            profiles: vec![api, codex_profile("codex-1")],
            active_id: "codex-1".into(),
            proxy_port: 19011,
            sandbox_port: 19012,
            reuse_system_ssh: true,
            secret: "keep-secret".into(),
            codex_network: csswitch_codex_network::CodexNetworkSettings {
                mode: csswitch_codex_network::CodexNetworkMode::Custom,
                proxy_url: "socks5h://127.0.0.1:7890".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        save_to(&d, &cfg).unwrap();
        let actions = BTreeMap::from([("codex-1".into(), CodexDowngradeAction::ExportThenRemove)]);
        let export_path = downgrade_to_v2(&d, &actions, Some(&export_destination))
            .unwrap()
            .unwrap();

        let raw = fs::read(config_path(&d)).unwrap();
        let v2: crate::config_legacy::ConfigV2 = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v2.schema_version, 2);
        assert_eq!(v2.active_id, "");
        assert_eq!(v2.profiles.len(), 1);
        assert_eq!(v2.profiles[0].id, "api-1");
        assert_eq!(v2.profiles[0].api_key, "sk-preserve");
        assert_eq!(v2.proxy_port, 19011);
        assert_eq!(v2.sandbox_port, 19012);
        assert!(v2.reuse_system_ssh);
        assert_eq!(v2.secret, "keep-secret");
        let raw_value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert!(raw_value.get("codex_network").is_none());

        let export = fs::read_to_string(export_path).unwrap();
        assert!(export.contains("Codex account"));
        assert!(export.contains("saved_model_catalog"));
        assert!(!export.contains("csswitch:codex:default"));
        assert!(!export.contains("credential_ref"));
        assert!(!export.contains("api_key"));
        assert!(!export.contains("sk-preserve"));
        assert!(!export.contains("api.deepseek.test"));
        assert!(!export.contains("keep-secret"));
    }

    #[test]
    fn downgrade_terminal_latch_is_verified_in_an_isolated_test_process() {
        if std::env::var_os("CSSWITCH_DOWNGRADE_LATCH_CHILD").is_some() {
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("config::tests::downgrade_terminal_latch_child")
            .arg("--nocapture")
            .env("CSSWITCH_DOWNGRADE_LATCH_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "terminal latch child failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn downgrade_terminal_latch_child() {
        if std::env::var_os("CSSWITCH_DOWNGRADE_LATCH_CHILD").is_none() {
            return;
        }
        let root = tmpdir();
        let dir = root.join(".csswitch-terminal-latch");
        let destination = root.join("codex-export.json");
        let cfg = Config {
            profiles: vec![codex_profile("codex-terminal")],
            active_id: "codex-terminal".into(),
            ..Default::default()
        };
        save_to(&dir, &cfg).unwrap();
        let actions = BTreeMap::from([(
            "codex-terminal".into(),
            CodexDowngradeAction::ExportThenRemove,
        )]);
        let fingerprint = prepare_downgrade_to_v2(&cfg, &actions).unwrap().fingerprint;
        downgrade_to_v2_and_latch(&dir, &actions, Some(&destination), &fingerprint).unwrap();

        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(config_path(&dir)).unwrap()).unwrap();
        assert_eq!(raw["schema_version"], 2);
        let backup_before = fs::read(dir.join("config.json.bak")).unwrap();
        for error in [
            load_from(&dir).unwrap_err(),
            update(&dir, |_| {}).unwrap_err(),
            save_to(&dir, &Config::default()).unwrap_err(),
            write_rolling_backup(&dir).unwrap_err(),
        ] {
            assert!(error.to_string().contains("终态退出"));
        }
        drop_rolling_backup(&dir);
        assert_eq!(
            fs::read(dir.join("config.json.bak")).unwrap(),
            backup_before
        );
        let raw_after: serde_json::Value =
            serde_json::from_slice(&fs::read(config_path(&dir)).unwrap()).unwrap();
        assert_eq!(raw_after["schema_version"], 2);
    }

    #[test]
    fn downgrade_export_failure_leaves_current_config_byte_identical() {
        let base = tmpdir();
        let d = base.join(".csswitch-downgrade-fail");
        let cfg = Config {
            profiles: vec![codex_profile("codex-1")],
            active_id: "codex-1".into(),
            ..Default::default()
        };
        save_to(&d, &cfg).unwrap();
        let before = fs::read(config_path(&d)).unwrap();
        let elsewhere = base.join("export-target");
        fs::write(&elsewhere, b"UNCHANGED").unwrap();
        let export_destination = base.join("codex-profiles-export.v1.json");
        symlink(&elsewhere, &export_destination).unwrap();
        let actions = BTreeMap::from([("codex-1".into(), CodexDowngradeAction::ExportThenRemove)]);
        assert!(downgrade_to_v2(&d, &actions, Some(&export_destination)).is_err());
        assert_eq!(fs::read(config_path(&d)).unwrap(), before);
        assert_eq!(fs::read(&elsewhere).unwrap(), b"UNCHANGED");
    }

    #[test]
    fn duplicate_or_empty_profile_ids_fail_before_downgrade_actions_are_folded() {
        let duplicate = Config {
            profiles: vec![codex_profile("same"), codex_profile("same")],
            active_id: "same".into(),
            ..Default::default()
        };
        let actions = BTreeMap::from([("same".into(), CodexDowngradeAction::Remove)]);
        assert!(prepare_downgrade_to_v2(&duplicate, &actions).is_err());

        let mut empty = codex_profile("");
        empty.name = "empty id".into();
        let cfg = Config {
            profiles: vec![empty],
            ..Default::default()
        };
        assert!(save_to(&tmpdir().join(".csswitch-empty-id"), &cfg).is_err());
    }

    #[test]
    fn commit_sync_failure_restores_byte_identical_config_without_residue() {
        let d = tmpdir().join(".csswitch-sync-rollback");
        save_to(&d, &Config::default()).unwrap();
        let before = fs::read(config_path(&d)).unwrap();
        let secure = SecureDir::open(&d, false).unwrap();
        let error = atomic_write_named_bytes_in(
            &secure,
            "config.json",
            br#"{"schema_version":3,"changed":true}"#,
            None,
            |_| Err(io::Error::other("injected fsync failure")),
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected fsync failure"));
        assert_eq!(fs::read(config_path(&d)).unwrap(), before);
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.contains(".tmp-") || name.contains(".restore-") || name.contains(".rollback-")
            })
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn commit_and_rollback_double_failure_is_terminal_uncertain() {
        let d = tmpdir().join(".csswitch-sync-rollback-double-fail");
        save_to(&d, &Config::default()).unwrap();
        let secure = SecureDir::open(&d, false).unwrap();
        let error = atomic_write_named_bytes_in(
            &secure,
            "config.json",
            br#"{"schema_version":3,"changed":true}"#,
            None,
            |secure| {
                fs::set_permissions(&secure.path, fs::Permissions::from_mode(0o500))?;
                Err(io::Error::other("injected commit sync failure"))
            },
        )
        .unwrap_err();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(atomic_rollback_is_uncertain(&error));
        assert!(error.to_string().contains("回滚失败"));
        let failure = DowngradeError::commit(error);
        assert!(failure.exit_required);
        let outcome = Err(failure);
        let mut access = ConfigAccessState {
            downgrade_terminal: false,
        };
        latch_terminal_downgrade_outcome(&mut access, &outcome);
        assert!(access.downgrade_terminal);
    }

    #[test]
    fn safe_precommit_downgrade_failure_does_not_latch_terminal_state() {
        let outcome = Err(DowngradeError::safe("injected precommit failure"));
        let mut access = ConfigAccessState {
            downgrade_terminal: false,
        };
        latch_terminal_downgrade_outcome(&mut access, &outcome);
        assert!(!access.downgrade_terminal);
    }

    #[test]
    fn clearing_key_removes_old_secret_from_every_regular_config_file() {
        let d = tmpdir().join(".csswitch-clear-key");
        let mut cfg = Config {
            profiles: vec![Profile {
                api_key: "sk-must-not-survive".into(),
                ..saved_profile("api-1", "deepseek", "anthropic", "deepseek-v4-pro")
            }],
            active_id: "api-1".into(),
            ..Default::default()
        };
        save_to(&d, &cfg).unwrap();
        write_rolling_backup(&d).unwrap();
        cfg.profiles[0].api_key.clear();
        save_to(&d, &cfg).unwrap();
        drop_rolling_backup(&d);
        for entry in fs::read_dir(&d).unwrap().filter_map(Result::ok) {
            if entry.file_type().unwrap().is_file() {
                let bytes = fs::read(entry.path()).unwrap();
                assert!(!bytes
                    .windows(b"sk-must-not-survive".len())
                    .any(|window| { window == b"sk-must-not-survive" }));
            }
        }
    }

    #[test]
    fn export_must_leave_app_owned_dir_and_preserves_user_parent_mode() {
        let root = tmpdir();
        let d = root.join(".csswitch-export-boundary");
        let cfg = Config {
            profiles: vec![codex_profile("codex-1")],
            active_id: "codex-1".into(),
            ..Default::default()
        };
        save_to(&d, &cfg).unwrap();
        let actions = BTreeMap::from([("codex-1".into(), CodexDowngradeAction::ExportThenRemove)]);
        for reserved in [
            "config.json",
            "config.json.bak",
            "config.json.v1.bak",
            "config.json.v2.bak",
        ] {
            assert!(downgrade_to_v2(&d, &actions, Some(&d.join(reserved))).is_err());
        }

        let export_dir = root.join("Documents");
        fs::create_dir(&export_dir).unwrap();
        fs::set_permissions(&export_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let destination = export_dir.join("codex-export.json");
        downgrade_to_v2(&d, &actions, Some(&destination)).unwrap();
        assert_eq!(mode_of(&export_dir), 0o755);
        assert_eq!(mode_of(&destination), 0o600);
    }

    #[test]
    fn failed_export_commit_preserves_existing_user_file_bytes_and_mode() {
        let root = tmpdir();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let destination = root.join("existing-export.json");
        fs::write(&destination, b"user-owned-before").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o644)).unwrap();
        let export_dir = SecureDir::open_unmanaged(&root).unwrap();
        assert!(atomic_write_named_bytes_in(
            &export_dir,
            "existing-export.json",
            b"replacement",
            None,
            |_| Err(io::Error::other("injected export dir fsync failure")),
        )
        .is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"user-owned-before");
        assert_eq!(mode_of(&destination), 0o644);
        assert_eq!(mode_of(&root), 0o755);
    }

    #[test]
    fn completed_export_survives_later_config_precommit_failure() {
        let root = tmpdir();
        let d = root.join(".csswitch-downgrade-crash-boundary");
        let cfg = Config {
            profiles: vec![codex_profile("codex-1")],
            active_id: "codex-1".into(),
            ..Default::default()
        };
        save_to(&d, &cfg).unwrap();
        let before = fs::read(config_path(&d)).unwrap();
        let fifo = d.join("config.json.bak");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let destination = root.join("codex-export.json");
        let actions = BTreeMap::from([("codex-1".into(), CodexDowngradeAction::ExportThenRemove)]);
        assert!(downgrade_to_v2(&d, &actions, Some(&destination)).is_err());
        assert_eq!(fs::read(config_path(&d)).unwrap(), before);
        let export = fs::read_to_string(destination).unwrap();
        assert!(export.contains("Codex account"));
        assert!(!export.contains("credential_ref"));
    }

    #[test]
    fn mask_hides_all_but_last4() {
        assert_eq!(mask("sk-1234567890ab"), "••••90ab"); // 定长 4 点 + 末4
        assert_eq!(mask(""), "");
        assert_eq!(mask("abc"), "•••");
        assert_eq!(mask("abcd"), "••••");
        assert_eq!(mask("abcde"), "••••bcde"); // 定长 4 点 + 末4
        let full = "sk-secret-tail9999";
        assert!(!mask(full).contains("secret"));
        // 定长：掩码总长恒为 8（4 点 + 末4），不随 key 长度变长、不泄漏长度
        assert_eq!(
            mask("sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaa1234").chars().count(),
            8
        );
    }
}
