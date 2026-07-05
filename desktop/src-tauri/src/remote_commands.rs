//! 远程管理 Tauri Commands。
//!
//! 本模块提供所有与远程 Linux 服务器交互的 Tauri 命令，前端通过 `invoke()` 调用。
//! 每个命令委托给 `remote::ssh` 模块执行 SSH + Helper JSON 协议。
//! SSH 操作本身是阻塞的，Tauri 会自动在后台线程池执行 `#[tauri::command]` fn。
//! 对于需要在 async 上下文中调用的场景（如 health 内部递归调用），使用
//! [`run_blocking`] 在独立线程中执行以避免阻塞当前 async runtime。
//!
//! 命令分为四组：
//! 1. Profile 管理 — 增删改查远程服务器连接配置
//! 2. 健康检查 — SSH 连通性、Helper 版本/能力检测
//! 3. 代理/配置 — 远程代理启停、配置文件读写
//! 4. 便利操作 — 一键开始、日志查看、诊断

use crate::remote::{
    self, RemoteAuthMethod, RemoteHealth, RemoteHostProfile, REQUIRED_CAPABILITIES,
};
use crate::{config, templates};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

// P1-8 修复：health check 结果缓存，减少频繁 SSH 连接
lazy_static::lazy_static! {
    static ref HEALTH_CACHE: Arc<Mutex<HashMap<String, (RemoteHealth, std::time::Instant)>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

const HEALTH_CACHE_TTL_SECS: u64 = 5;

// ============================================================================
// 线程辅助 — 在独立 OS 线程中执行阻塞 I/O，避免卡住 Tauri 事件循环
// ============================================================================

/// 在当前线程之外的独立 OS 线程中运行一段阻塞代码，通过 channel 取回结果。
/// 用于 async 上下文中需要执行 SSH（需要 `Send + 'static`）的场景。
/// 当前所有远程命令已改为 sync fn（由 Tauri 运行时自动分派到线程池），
/// 此函数预留给将来可能的持久会话模式（serve）或高频轮询场景。
#[allow(dead_code)]
fn run_blocking<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv().unwrap_or(Err("后台任务线程异常退出".to_string()))
}

// ============================================================================
// 1. Profile 管理（纯本地 I/O，无需 spawn）
// ============================================================================

/// 列出所有远程服务器 Profile。
#[tauri::command]
pub fn remote_list_profiles() -> Result<Vec<RemoteHostProfile>, String> {
    remote::load_profiles()
}

/// 保存（新增或更新）一个远程服务器 Profile。
#[tauri::command]
pub fn remote_save_profile(profile: RemoteHostProfile) -> Result<RemoteHostProfile, String> {
    remote::upsert_profile(profile)
}

/// 删除指定 ID 的远程服务器 Profile。
#[tauri::command]
pub fn remote_delete_profile(id: String) -> Result<bool, String> {
    remote::delete_profile(&id)
}

/// 校验 Profile 字段但不保存。
#[tauri::command]
pub fn remote_validate_profile(profile: RemoteHostProfile) -> Result<bool, String> {
    remote::validate_profile(&profile).map(|_| true)
}

// ============================================================================
// 2. 健康检查（SSH，阻塞 I/O）
// ============================================================================

/// 检查远程服务器健康状态：SSH 连通性 + Helper 版本/能力。
/// SSH 是阻塞 I/O，Tauri 自动在后台线程执行此命令。
/// P1-8 修复：使用缓存减少频繁 SSH 连接（TTL 5秒）。
#[tauri::command]
pub fn remote_check_health(profile: RemoteHostProfile) -> Result<RemoteHealth, String> {
    // P1-8 修复：先检查缓存
    {
        let cache = HEALTH_CACHE.lock().unwrap();
        if let Some((cached_health, cached_time)) = cache.get(&profile.id) {
            if cached_time.elapsed().as_secs() < HEALTH_CACHE_TTL_SECS {
                // 缓存未过期，直接返回
                return Ok(cached_health.clone());
            }
        }
    }

    // 缓存过期或不存在，执行实际检查
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // 快速 SSH 连通性测试（调用 helper status）
    let reachable = remote::ssh::run_helper_json_simple::<Value>(
        &profile,
        &["status".to_string()],
    )
    .is_ok();

    let health = if !reachable {
        RemoteHealth {
            reachable: false,
            helper_installed: false,
            helper_version: None,
            desktop_version: env!("CARGO_PKG_VERSION").to_string(),
            compatible: false,
            platform: None,
            arch: None,
            capabilities: vec![],
            proxy_running: false,
            sandbox_running: false,
            last_error: Some(
                "无法通过 SSH 连接到服务器。请检查地址、端口和认证配置。".to_string(),
            ),
            last_check: now,
        }
    } else {
        // 获取详细状态（带重试）
        let status_result = remote::ssh::run_helper_json_with_retry::<Value>(
            &profile,
            &["status".to_string()],
        );

        match status_result {
            Ok(status) => parse_health_from_status(&status, now),
            Err(e) => RemoteHealth {
                reachable: true,
                helper_installed: false,
                helper_version: None,
                desktop_version: env!("CARGO_PKG_VERSION").to_string(),
                compatible: false,
                platform: None,
                arch: None,
                capabilities: vec![],
                proxy_running: false,
                sandbox_running: false,
                last_error: Some(format!("Helper 不存在或无法执行：{}", e.message)),
                last_check: now,
            },
        }
    };

    // P1-8 修复：更新缓存
    {
        let mut cache = HEALTH_CACHE.lock().unwrap();
        cache.insert(profile.id.clone(), (health.clone(), std::time::Instant::now()));
    }

    Ok(health)
}

/// 安装/升级远程 Helper。
/// 通过 SSH 执行安装脚本：从 GitHub Releases 下载 helper 二进制到远程服务器。
#[tauri::command]
pub fn remote_install_helper(profile: RemoteHostProfile) -> Result<RemoteHealth, String> {
    // 直接执行 SSH 安装命令（安装脚本为 shell 脚本，不符合 helper JSON 协议格式，
    // 因此不走 run_helper_json，而是直接执行 ssh 命令并验证退出码和 status 输出）。
    let args = remote::ssh::build_helper_install_args(&profile);
    let mut cmd = std::process::Command::new("ssh");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let output = cmd.args(&args).output()
        .map_err(|e| format!("无法启动 SSH：{e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("Helper 安装失败。请确认远程服务器可访问 GitHub：{stderr}"));
    }
    // 安装成功后重新检查健康
    remote_check_health(profile)
}

// ============================================================================
// 3. 配置（SSH，阻塞 I/O）
// ============================================================================

/// 读取远程服务器上的配置。
#[tauri::command]
pub fn remote_get_config(profile: RemoteHostProfile) -> Result<Value, String> {
    remote::ssh::run_helper_json_with_retry::<Value>(
        &profile,
        &["config".to_string(), "get".to_string()],
    )
    .map_err(|e| e.message)
}

/// 写入远程配置。
#[tauri::command]
pub fn remote_set_config(profile: RemoteHostProfile, config_json: String) -> Result<(), String> {
    remote::ssh::run_helper_json_with_retry::<Value>(
        &profile,
        &["config".to_string(), "set".to_string(), config_json],
    )
    .map(|_| ())
    .map_err(|e| e.message)
}

/// 保存 Provider Key 到远程配置。
/// 返回掩码后的 key（仅末 4 位可见）。
#[tauri::command]
pub fn remote_save_provider_key(
    profile: RemoteHostProfile,
    provider: String,
    key: String,
) -> Result<String, String> {
    let result: Value = remote::ssh::run_helper_json_with_retry::<Value>(
        &profile,
        &[
            "config".to_string(),
            "save-key".to_string(),
            provider,
            key,
        ],
    )
    .map_err(|e| e.message)?;

    Ok(result["masked"].as_str().unwrap_or("••••").to_string())
}

// ============================================================================
// 4. 代理（SSH，阻塞 I/O）
// ============================================================================

fn remote_active_config_for_start(
    provider: &str,
    proxy_port: u16,
    sandbox_port: Option<u16>,
    secret: &str,
) -> Result<(config::Config, String), String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
    let active = cfg
        .active_profile()
        .cloned()
        .ok_or("没有生效的配置 Profile。请先在本地配置里选择当前模型来源。")?;
    let adapter = templates::adapter_for(&active.template_id).to_string();
    if !provider.is_empty() && provider != adapter && provider != active.template_id {
        return Err(format!(
            "远程启动来源不匹配：当前 Profile 是 {}，但启动请求是 {provider}。",
            active.template_id
        ));
    }
    if active.api_key.trim().is_empty() {
        return Err("当前 Profile 未填写 API Key。请填写后重试。".into());
    }
    if adapter == "relay" {
        if active.base_url.trim().is_empty()
            || !(active.base_url.starts_with("http://") || active.base_url.starts_with("https://"))
        {
            return Err("relay 配置需要 http(s):// 开头的 base_url。".into());
        }
        if active.model.trim().is_empty() {
            return Err("relay 配置需要选择或填写模型。".into());
        }
    }

    let remote_cfg = config::Config {
        schema_version: config::CURRENT_SCHEMA_VERSION,
        profiles: vec![active.clone()],
        active_id: active.id.clone(),
        proxy_port,
        sandbox_port: sandbox_port.unwrap_or(cfg.sandbox_port),
        secret: secret.to_string(),
        mode: cfg.mode,
        pending_notice: None,
    };
    Ok((remote_cfg, adapter))
}

/// 启动远程代理。
#[tauri::command]
pub fn remote_start_proxy(
    profile: RemoteHostProfile,
    provider: String,
    port: u16,
    secret: String,
) -> Result<Value, String> {
    let (remote_cfg, adapter) = remote_active_config_for_start(&provider, port, None, &secret)?;
    let config_json = serde_json::to_string(&remote_cfg).map_err(|e| e.to_string())?;

    remote::ssh::run_helper_json_with_retry::<Value>(
        &profile,
        &[
            "config".to_string(),
            "set".to_string(),
            config_json,
        ],
    )
    .map_err(|e| format!("同步当前 Profile 到服务器失败：{}", e.message))?;

    remote::ssh::run_helper_json_with_retry::<Value>(
        &profile,
        &["proxy".to_string(), "stop".to_string()],
    )
    .map_err(|e| format!("停止旧远程代理失败：{}", e.message))?;

    remote::ssh::run_helper_json_with_retry::<Value>(
        &profile,
        &[
            "proxy".to_string(),
            "start".to_string(),
            adapter,
            port.to_string(),
            secret,
        ],
    )
    .map_err(|e| e.message)
}

/// 停止远程代理。
#[tauri::command]
pub fn remote_stop_proxy(profile: RemoteHostProfile) -> Result<(), String> {
    remote::ssh::run_helper_json_with_retry::<Value>(
        &profile,
        &["proxy".to_string(), "stop".to_string()],
    )
    .map(|_| ())
    .map_err(|e| e.message)
}

/// 查询远程代理状态。
#[tauri::command]
pub fn remote_proxy_status(profile: RemoteHostProfile) -> Result<Value, String> {
    remote::ssh::run_helper_json_with_retry::<Value>(
        &profile,
        &["proxy".to_string(), "status".to_string()],
    )
    .map_err(|e| e.message)
}

/// 验证远程代理上的 Key 有效性（慢速：需经代理→上游往返）。
#[tauri::command]
pub fn remote_verify_key(
    profile: RemoteHostProfile,
    port: u16,
    secret: String,
) -> Result<Value, String> {
    remote::ssh::run_helper_json_slow::<Value>(
        &profile,
        &["verify".to_string(), port.to_string(), secret],
    )
    .map_err(|e| e.message)
}

// ============================================================================
// 5. 便利操作
// ============================================================================

/// 远程综合状态（三盏灯：proxy / sandbox / upstream）。
/// 返回格式与本地 `status` 命令一致，前端 `refreshStatus()` 无需修改。
#[tauri::command]
pub fn remote_status(profile: RemoteHostProfile) -> Result<Value, String> {
    let status: Value = remote::ssh::run_helper_json_with_retry::<Value>(
        &profile,
        &["status".to_string()],
    )
    .map_err(|e| e.message)?;

    let proxy_running = status["proxy_running"].as_bool().unwrap_or(false);
    let upstream_reachable = status["platform"].as_str().is_some();

    Ok(json!({
        "proxy": if proxy_running { "green" } else { "amber" },
        "sandbox": if status["sandbox_running"].as_bool().unwrap_or(false) { "green" } else { "amber" },
        "upstream": if upstream_reachable { "green" } else { "amber" },
        "remote": true,
    }))
}

/// 查看远程日志。
#[tauri::command]
pub fn remote_logs(
    profile: RemoteHostProfile,
    name: String,
    lines: Option<u32>,
) -> Result<Value, String> {
    let mut args = vec!["logs".to_string(), name];
    if let Some(n) = lines {
        args.push(n.to_string());
    }
    remote::ssh::run_helper_json_with_retry::<Value>(&profile, &args).map_err(|e| e.message)
}

/// 远程诊断。
#[tauri::command]
pub fn remote_doctor(profile: RemoteHostProfile) -> Result<Value, String> {
    remote::ssh::run_helper_json_with_retry::<Value>(&profile, &["doctor".to_string()])
        .map_err(|e| e.message)
}

fn remote_tunnel_hint(profile: &RemoteHostProfile, sandbox_port: u16) -> String {
    let mut parts = vec!["ssh".to_string()];
    if let RemoteAuthMethod::KeyFile { path } = &profile.auth_method {
        parts.push("-i".to_string());
        parts.push(path.clone());
    }
    parts.push("-p".to_string());
    parts.push(profile.port.to_string());
    parts.push("-N".to_string());
    parts.push("-L".to_string());
    parts.push(format!("{sandbox_port}:127.0.0.1:{sandbox_port}"));
    parts.push(format!("{}@{}", profile.username, profile.host));
    parts.join(" ")
}

/// 远程一键开始：同步当前 Profile → 起代理 → 起沙箱。
#[tauri::command]
pub fn remote_one_click(
    profile: RemoteHostProfile,
    provider: String,
    proxy_port: u16,
    sandbox_port: u16,
) -> Result<Value, String> {
    let secret = config::new_id();
    let max_proxy_port = proxy_port.saturating_add(20);
    let mut selected_proxy_port = proxy_port;
    let mut proxy_result = None;
    let mut last_port_error = None;

    for candidate_proxy_port in proxy_port..=proxy_port.saturating_add(20) {
        let (remote_cfg, adapter) = remote_active_config_for_start(
            &provider,
            candidate_proxy_port,
            Some(sandbox_port),
            &secret,
        )?;
        let config_json = serde_json::to_string(&remote_cfg).map_err(|e| e.to_string())?;

        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile,
            &[
                "config".to_string(),
                "set".to_string(),
                config_json,
            ],
        )
        .map_err(|e| format!("同步当前 Profile 到服务器失败：{}", e.message))?;

        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile,
            &["proxy".to_string(), "stop".to_string()],
        )
        .map_err(|e| format!("停止旧远程代理失败：{}", e.message))?;

        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile,
            &["sandbox".to_string(), "stop".to_string()],
        )
        .map_err(|e| format!("停止旧远程沙箱失败：{}", e.message))?;

        match remote::ssh::run_helper_json_with_retry::<Value>(
            &profile,
            &[
                "proxy".to_string(),
                "start".to_string(),
                adapter,
                candidate_proxy_port.to_string(),
                secret.clone(),
            ],
        ) {
            Ok(result) => {
                selected_proxy_port = candidate_proxy_port;
                proxy_result = Some(result);
                break;
            }
            Err(e) if e.code == "port_in_use" && candidate_proxy_port < max_proxy_port => {
                last_port_error = Some(e.message);
                continue;
            }
            Err(e) => return Err(format!("启动远程代理失败：{}", e.message)),
        }
    }

    let proxy_result = proxy_result.ok_or_else(|| {
        format!(
            "启动远程代理失败：端口 {}-{} 均不可用。{}",
            proxy_port,
            max_proxy_port,
            last_port_error.unwrap_or_else(|| "请换一个代理端口后重试。".to_string())
        )
    })?;

    let proxy_url = format!("http://127.0.0.1:{selected_proxy_port}/{secret}");
    let sandbox_result = remote::ssh::run_helper_json_with_retry::<Value>(
        &profile,
        &[
            "sandbox".to_string(),
            "start".to_string(),
            sandbox_port.to_string(),
            proxy_url.clone(),
        ],
    )
    .map_err(|e| format!("启动远程沙箱失败：{}", e.message))?;

    Ok(json!({
        "ok": true,
        "proxy_port": selected_proxy_port,
        "requested_proxy_port": proxy_port,
        "sandbox_port": sandbox_port,
        "proxy_url": proxy_url,
        "local_url": format!("http://127.0.0.1:{sandbox_port}"),
        "remote_url": format!("http://{}:{sandbox_port}", profile.host),
        "tunnel_hint": remote_tunnel_hint(&profile, sandbox_port),
        "proxy": proxy_result,
        "sandbox": sandbox_result,
    }))
}

// ============================================================================
// 内部辅助
// ============================================================================

/// 将 Helper 的 `status` 命令返回值解析为 `RemoteHealth` 结构。
fn parse_health_from_status(status: &Value, now: i64) -> RemoteHealth {
    let capabilities: Vec<String> = status["capabilities"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    // 兼容性检查：所需能力是否齐全
    let compatible = REQUIRED_CAPABILITIES
        .iter()
        .all(|req| capabilities.iter().any(|c| c == *req));

    RemoteHealth {
        reachable: true,
        helper_installed: true,
        helper_version: status["version"].as_str().map(String::from),
        desktop_version: env!("CARGO_PKG_VERSION").to_string(),
        compatible,
        platform: status["platform"].as_str().map(String::from),
        arch: status["arch"].as_str().map(String::from),
        capabilities,
        proxy_running: status["proxy_running"].as_bool().unwrap_or(false),
        sandbox_running: status["sandbox_running"].as_bool().unwrap_or(false),
        last_error: None,
        last_check: now,
    }
}
