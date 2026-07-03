//! CSSwitch 菜单栏 app 后端（进程管家）。
//!
//! 职责：管理「翻译代理」与「沙箱 Science」两个子进程的生命周期；读写
//! `~/.csswitch/config.json`；把第三方 key 以【环境变量】注入代理子进程（绝不进 argv）；
//! 探活；把沙箱 URL 交系统浏览器打开。已验证的越权/翻译逻辑仍留在 Python/Node/shell
//! 里被当作子进程调用，以保住铁律护栏与已验证行为。
//!
//! 铁律相关：key 只在内存与 0600 的 config.json；回显前端只给掩码；沙箱端口/目录护栏
//! 由被调脚本负责（对 8765 与真实目录失败关闭）；退 app 默认停代理、保留沙箱。

mod config;
mod oauth_forge;
mod proc;
mod relay_presets;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tauri::{Manager, State};

const SCIENCE_BIN: &str = "/Applications/Claude Science.app/Contents/Resources/bin/claude-science";

#[derive(Default)]
struct AppState {
    proxy: Option<Child>,
    proxy_port: u16,
    secret: String,
    provider: String,
    /// 当前代理进程所用 key 的非加密指纹（仅内存、绝不落盘/打印）。
    /// 换 key 后指纹变化 → 触发重启，避免复用带旧 key 的代理。
    key_fp: u64,
    sandbox: Option<Child>,
    sandbox_port: u16,
    sandbox_url: Option<String>,
}

/// key 的非加密指纹（SipHash），只用于判断「key 是否变了」。绝不打印、绝不落盘。
fn key_fingerprint(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

// ---------- provider 元信息 ----------
fn key_env(provider: &str) -> &'static str {
    match provider {
        "qwen" => "DASHSCOPE_API_KEY",
        "relay" => "CSSWITCH_RELAY_KEY",
        _ => "DEEPSEEK_API_KEY",
    }
}

/// 上游主机名（供 status 上游灯做 TCP 可达性探测）。relay 从其 base_url 解析。
fn upstream_host(provider: &str, base_url: &str) -> String {
    match provider {
        "qwen" => "dashscope.aliyuncs.com".to_string(),
        "relay" => parse_host(base_url).unwrap_or_default(),
        _ => "api.deepseek.com".to_string(),
    }
}

/// 从 `http(s)://host[:port]/path` 里抽出 host。解析不出返回 None（不引 url crate）。
fn parse_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("")
        .to_string();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// 判断模型 id 是否会平铺进 Science 选择器主列表（claude-{opus|sonnet|haiku}-<数字…>）。
/// 仅用于「获取模型」结果排序（主列表项排前），非鉴权路径。
fn is_main_list_model(id: &str) -> bool {
    for fam in ["claude-opus-", "claude-sonnet-", "claude-haiku-"] {
        if let Some(rest) = id.strip_prefix(fam) {
            return rest
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false);
        }
    }
    false
}

// ---------- 路径与日志 ----------
/// 定位 CSSwitch 仓库根（含 proxy/csswitch_proxy.py）。优先 CSSWITCH_REPO，
/// 否则从可执行文件与当前目录逐级上溯。找不到返回 None。
fn repo_root() -> Option<PathBuf> {
    let marker = Path::new("proxy/csswitch_proxy.py");
    // 显式指定优先：规范化后再判定，避免相对/软链歧义。
    if let Some(r) = std::env::var_os("CSSWITCH_REPO") {
        if let Ok(p) = std::fs::canonicalize(PathBuf::from(r)) {
            if p.join(marker).is_file() {
                return Some(p);
            }
        }
    }
    // 否则只从【可执行文件位置】上溯。刻意不看 current_dir：启动目录可被影响，
    // 若据此找到别处的 csswitch_proxy.py，会把带 key 的环境交给来路不明的脚本。
    if let Ok(exe) = std::env::current_exe() {
        let mut dir: Option<&Path> = exe.parent();
        while let Some(d) = dir {
            if d.join(marker).is_file() {
                return Some(d.to_path_buf());
            }
            dir = d.parent();
        }
    }
    None
}

/// 定位「资源根」（含 proxy/、scripts/）。打包成 .app 后，proxy/ 与 scripts/ 被
/// bundle 进 `Contents/Resources`；开发态则回退到仓库根。找不到返回 None。
/// 这样从 Finder 启动的正式 .app 也能找到代理脚本（修 P1-1）。
fn asset_root(app: &tauri::AppHandle) -> Option<PathBuf> {
    let marker = Path::new("proxy/csswitch_proxy.py");
    // 打包态：Tauri 资源目录。
    if let Ok(res) = app.path().resource_dir() {
        if res.join(marker).is_file() {
            return Some(res);
        }
    }
    // 开发态：从可执行文件位置上溯（见 repo_root 注释，刻意不看 current_dir）。
    repo_root()
}

/// 沙箱可写工作目录（独立 HOME）：`~/.csswitch/sandbox/home`。
/// 打包后资源目录只读，沙箱状态（虚拟登录、克隆运行时、钥匙串）必须落在可写处；
/// 该路径同时交给 launch/stop 脚本（`SANDBOX_HOME` 环境变量）与取 URL 逻辑，三者一致。
fn sandbox_home() -> PathBuf {
    config::default_dir().join("sandbox").join("home")
}

fn log_path(name: &str) -> PathBuf {
    config::default_dir().join("logs").join(name)
}

/// `O_NOFOLLOW` 的平台常量（本项目不引 libc）。macOS/BSD=0x0100，Linux=0x20000。
const fn libc_o_nofollow() -> i32 {
    if cfg!(target_os = "linux") {
        0x2_0000
    } else {
        0x0100
    }
}

/// 打开（truncate）一个子进程日志文件，父目录 0700、文件 0600（防同机其它用户读到 secret 尾巴）。
fn open_log(name: &str) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let p = log_path(name);
    if let Some(parent) = p.parent() {
        config::assert_not_symlink(parent)?;
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    // 日志路径不许是符号链接：否则 truncate+写会覆盖链接目标文件（修 P2-1）。
    config::assert_not_symlink(&p)?;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        // O_NOFOLLOW：即便在 lstat 与 open 之间被换成软链，也拒绝跟随。
        .custom_flags(libc_o_nofollow())
        .open(&p)?;
    // 文件已存在时 mode() 不复位，显式再夹一次。
    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    Ok(f)
}

/// 把字符串里的 secret 明文替换成 ****，用于任何要回显给前端的错误尾巴。
fn redact(s: &str, secret: &str) -> String {
    if secret.is_empty() {
        s.to_string()
    } else {
        s.replace(secret, "****")
    }
}

fn tail_file(path: &Path, max: usize) -> String {
    match std::fs::read(path) {
        Ok(b) => {
            let start = b.len().saturating_sub(max);
            String::from_utf8_lossy(&b[start..]).trim().to_string()
        }
        Err(_) => String::new(),
    }
}

fn kill_child(slot: &mut Option<Child>) {
    if let Some(mut c) = slot.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// 取锁并从 poison 中恢复：某线程持锁时 panic 不应把整个 app 卡死。
fn lock(m: &Mutex<AppState>) -> std::sync::MutexGuard<'_, AppState> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 用系统浏览器打开 URL（macOS `open`）。校验退出码：非零视为失败（P2c）。
fn open_in_browser(url: &str) -> Result<(), String> {
    let st = Command::new("open")
        .arg(url)
        .status()
        .map_err(|e| format!("打开浏览器失败：{e}"))?;
    if !st.success() {
        return Err(format!("open 非零退出（{:?}）", st.code()));
    }
    Ok(())
}

// ---------- 代理生命周期核心 ----------
/// 转义 ERE（extended regex）元字符，让路径按字面参与 `pkill -f` 匹配（避免路径里的
/// `.`/`(`/`[` 等被当作正则、误配或失配）。
fn ere_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if "\\.^$*+?()[]{}|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// 本次 ensure_proxy 对代理做了什么（供一键据实提示）。
#[derive(Clone, Copy, PartialEq)]
enum ProxyAction {
    Reused,    // 端口+provider+key 指纹一致且健康，原样复用
    Restarted, // 首次起 / 换 key / 换 provider / 不健康，重起了代理
}

/// 确保代理在跑且健康；返回 (端口, secret, 本次动作)。幂等：已健康则复用。
fn ensure_proxy(
    app: &tauri::AppHandle,
    state: &State<'_, Mutex<AppState>>,
) -> Result<(u16, String, ProxyAction), String> {
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let provider = cfg.provider.clone();
    let key = cfg
        .key_for(&provider)
        .ok_or_else(|| format!("缺少 {provider} 的 API key，请先在面板填写并保存。"))?;
    // relay（中转站）：base_url 必填，作上游根地址。非 relay 为空串。
    let base_url = cfg.base_url_for(&provider).unwrap_or_default();
    if provider == "relay" && base_url.is_empty() {
        return Err(
            "中转站模式需要填 base_url（如 https://your-relay/claude），请先在面板填写并保存。"
                .into(),
        );
    }
    // 指纹并入 base_url：换 key 或换中转站地址都触发代理重启（避免复用旧上游）。
    let key_fp = key_fingerprint(&format!("{key}\n{base_url}"));
    let port = cfg.proxy_port;
    let root = asset_root(app)
        .ok_or("找不到代理脚本 proxy/csswitch_proxy.py（打包资源或仓库根均未命中）。开发态可设 CSSWITCH_REPO。")?;
    let py = proc::find_exe("python3")
        .ok_or("缺少依赖 python3（起翻译代理需要）。已查 PATH、常见目录与登录 shell 仍未找到；macOS 一般自带 /usr/bin/python3（装 Xcode 命令行工具：xcode-select --install）。")?;

    // path-secret：**持久化复用**。已在跑的沙箱把该 secret 嵌进了 ANTHROPIC_BASE_URL，
    // 若每次起代理都换 secret，代理一重启（换 key/换 provider/重开 app）沙箱就会拿旧 secret
    // 打到新代理 → 全部 403（修 P1：代理重启后沙箱失联）。故从 config 读稳定 secret，
    // 首次为空才生成一次并写回，之后所有代理进程都复用它。
    let secret = if !cfg.secret.is_empty() {
        cfg.secret.clone()
    } else {
        let s = proc::gen_secret().map_err(|e| format!("无法生成安全 secret：{e}"))?;
        let s2 = s.clone();
        config::update(&dir, move |c| c.secret = s2).map_err(|e| e.to_string())?;
        s
    };

    // 整个「检查 → 清残留 → 起进程 → 记账」在同一把锁内完成，避免并发双击时
    // 两路都判定「没健康代理」各起一个、后者覆盖前者的 Child 句柄导致前者被孤儿泄漏。
    {
        let mut st = lock(state);
        // 幂等：已在跑且健康、且【端口 + provider + key 指纹】都一致才复用。
        // 只比端口会在「换 provider / 换 key」后误用带旧配置的代理（修 P1-2）。
        if st.proxy.is_some()
            && st.proxy_port == port
            && st.provider == provider
            && st.key_fp == key_fp
            && proc::http_health(port, Some(&st.secret), 500)
        {
            return Ok((port, st.secret.clone(), ProxyAction::Reused));
        }
        // 清残留（换端口/换 provider/换 key/不健康）。
        kill_child(&mut st.proxy);
        let script = root.join("proxy/csswitch_proxy.py");
        // 再清掉上次会话遗留、绑在同端口上的孤儿代理：崩溃或强退不会触发本进程的 kill，
        // 孤儿仍占着端口 → 新代理绑不上（Errno 48）→ 探活超时。
        // 收紧（P2 GPT 复审）：匹配【本安装的绝对脚本路径】+ 端口，而非仅「脚本名+端口」，
        // 避免误杀另一个 checkout / 用户手启的同名代理。路径里的正则元字符转义按字面匹配。
        let pat = format!("{}.*--port {port}", ere_escape(&script.to_string_lossy()));
        let _ = Command::new("pkill").arg("-f").arg(&pat).status();

        let logf = open_log("proxy.log").map_err(|e| format!("建日志失败：{e}"))?;
        let logf2 = logf.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&py);
        cmd.arg(&script)
            .arg("--provider")
            .arg(&provider)
            .arg("--port")
            .arg(port.to_string())
            .arg("--auth-token")
            .arg(&secret)
            // key 经环境变量注入，绝不作为命令行参数（避免 ps 泄露）。
            .env(key_env(&provider), &key);
        // relay：中转站 base_url 经环境变量交给代理（非密钥，但与 key 一致走 env）。
        if provider == "relay" {
            cmd.env("CSSWITCH_RELAY_BASE_URL", &base_url);
        }
        let child = cmd
            .stdout(Stdio::from(logf))
            .stderr(Stdio::from(logf2))
            .spawn()
            .map_err(|e| format!("启动代理失败：{e}"))?;
        st.proxy = Some(child);
        st.proxy_port = port;
        st.secret = secret.clone();
        st.provider = provider;
        st.key_fp = key_fp;
    }

    // 探活最多 ~4s（锁外，不阻塞 status 等命令）。
    let mut ok = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        if proc::http_health(port, Some(&secret), 400) {
            ok = true;
            break;
        }
    }
    if !ok {
        let mut st = lock(state);
        // 只在仍是本次起的代理时才清（secret 匹配），避免误杀并发重启起来的新代理。
        if st.secret == secret {
            kill_child(&mut st.proxy);
        }
        let tail = redact(&tail_file(&log_path("proxy.log"), 500), &secret);
        return Err(format!(
            "代理起后探活超时（端口 {port} 可能被占用，或 key 无效）。\n{tail}"
        ));
    }
    Ok((port, secret, ProxyAction::Restarted))
}

/// 停沙箱。返回 Err 表示 stop 脚本非零退出（Science 可能没停干净），
/// 调用方据此如实报告，不再无条件报「已停止」（修 P1 停止虚假成功）。
fn stop_sandbox_inner(app: &tauri::AppHandle, st: &mut AppState) -> Result<(), String> {
    // 沙箱由脚本以 --detached 起 Science，本进程持有的是脚本 child（已退出）。
    // 真正停 Science 要调 stop 脚本（按 data-dir，绝不碰真实 8765）。
    // 修 P1（GPT 复审）：定位不到资源根 / 停止脚本时，绝不静默返回成功——detached 沙箱
    // 可能仍在跑，谎报「已停止」会让「切官方模式」误以为第三方链路已拆。此时如实报错。
    let mut err = None;
    match asset_root(app) {
        Some(root) => {
            let stop = root.join("scripts/stop-science-sandbox.sh");
            if stop.is_file() {
                match Command::new("zsh") // stop 脚本是 #!/bin/zsh（用了 ${VAR:A} realpath）
                    .arg(&stop)
                    // 与 launch 时一致的可写沙箱 HOME，stop 才能按同一 data-dir 停对进程。
                    .env("SANDBOX_HOME", sandbox_home())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                {
                    Ok(s) if s.success() => {}
                    Ok(s) => err = Some(format!("停止沙箱脚本非零退出（{:?}）。", s.code())),
                    Err(e) => err = Some(format!("调用停止沙箱脚本失败：{e}")),
                }
            } else {
                err = Some(format!(
                    "找不到停止脚本 {}，无法确认沙箱已停止（沙箱可能仍在运行）。",
                    stop.display()
                ));
            }
        }
        None => {
            err = Some(
                "定位不到资源根，取不到停止脚本，无法确认沙箱已停止（沙箱可能仍在运行）。"
                    .to_string(),
            );
        }
    }
    kill_child(&mut st.sandbox);
    st.sandbox_url = None;
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ---------- Tauri commands ----------
#[tauri::command]
fn get_config() -> Result<serde_json::Value, String> {
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let mut keys = serde_json::Map::new();
    for p in ["deepseek", "qwen", "relay"] {
        let masked = cfg.key_for(p).map(|k| config::mask(&k)).unwrap_or_default();
        keys.insert(p.to_string(), serde_json::Value::String(masked));
    }
    Ok(json!({
        "provider": cfg.provider,
        "proxy_port": cfg.proxy_port,
        "sandbox_port": cfg.sandbox_port,
        "mode": cfg.mode,
        "keys": keys,
        // 中转站 base_url（非密钥，可回显），供面板回填。
        "relay_base_url": cfg.base_url_for("relay").unwrap_or_default(),
    }))
}

/// 切换运行模式（"proxy" 第三方 / "official" 官方）。
///
/// 切到「官方」是**真正的切换**，不只是改配置：先把第三方链路拆掉（停沙箱 Science + 杀代理、
/// 清 secret）。否则代理/沙箱会留在后台空跑；且 macOS 单实例语义下，后面 `open` 可能只是聚焦
/// 还活着的沙箱实例（带着改过的 ANTHROPIC_* 环境）而非官方实例，把用户误导回第三方链路。
/// 切回「第三方」不自动起任何东西（仍需用户填 key 后点「一键开始」）。全程绝不碰真实 8765。
#[tauri::command]
fn set_mode(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    mode: String,
) -> Result<(), String> {
    if mode != "proxy" && mode != "official" {
        return Err(format!("未知模式：{mode}（只支持 proxy / official）。"));
    }
    let dir = config::default_dir();

    // 事务化（修 P2 GPT 复审）：切官方要「先拆第三方链路，成功了再落盘 official」。
    // 旧序（先落盘再拆）若拆沙箱失败，会留下「磁盘=official、UI/进程=第三方」的状态分裂
    // （前端收到 Err 保持第三方 UI，磁盘却已是 official，下次启动就错进官方模式）。
    // 现序保证：拆失败 → 不落盘、保持 proxy 模式、如实报错，磁盘/UI/进程一致。
    if mode == "official" {
        {
            let mut st = lock(&state);
            // 先停沙箱：失败就在动代理/落盘之前中止，状态不分裂。
            stop_sandbox_inner(&app, &mut st).map_err(|e| {
                format!("停止沙箱失败，未切换到官方模式：{e}（真实实例 8765 未受影响）")
            })?;
            kill_child(&mut st.proxy);
            st.secret.clear();
        }
    }
    // 拆链已成功（或切回 proxy 无需拆）→ 落盘。
    config::update(&dir, {
        let mode = mode.clone();
        move |c| c.mode = mode
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 官方模式：干净地打开用户【真实】的 Claude Science（用户自己的官方登录与订阅）。
///
/// 铁律：绝不碰/复制真实凭证；用 `open`（系统 LaunchServices 正常启动）而非注入环境变量，
/// 并显式抹掉任何 `ANTHROPIC_*`，确保**不用改过的环境变量启动真实实例**（真实实例走它自己的
/// 官方端点，不经本代理）。CSSwitch 只把用户交回官方客户端，不托管其登录。
#[tauri::command]
fn open_official() -> Result<(), String> {
    let app_path = "/Applications/Claude Science.app";
    let mut cmd = Command::new("open");
    if Path::new(app_path).is_dir() {
        cmd.arg(app_path);
    } else {
        cmd.arg("-a").arg("Claude Science");
    }
    // 防御性：即便 `open` 通常不向被启动 app 传本进程环境，也显式抹掉，杜绝把改过的
    // ANTHROPIC_* 带进真实实例（铁律 3）。
    cmd.env_remove("ANTHROPIC_BASE_URL")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN");
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err("未能打开 Claude Science。请确认已安装官方 Claude Science。".into()),
        Err(e) => Err(format!("打开官方 Claude Science 失败：{e}")),
    }
}

#[derive(Deserialize)]
struct UiSettings {
    provider: String,
    proxy_port: u16,
    sandbox_port: u16,
    /// 仅「中转站」(relay) 用：中转站 base_url。其它 provider 前端不传（None → 不改动已存值）。
    #[serde(default)]
    base_url: Option<String>,
}

#[tauri::command]
fn set_config(cfg: UiSettings) -> Result<(), String> {
    // 铁律防御：代理/沙箱端口都不许用真实实例保留端口 8765。
    if cfg.proxy_port == 8765 || cfg.sandbox_port == 8765 {
        return Err("端口 8765 是真实 Science 实例保留端口，不能用。".into());
    }
    // 只认已实现的 provider，避免存进未知值后起代理时才失败（修 P2-3）。
    if cfg.provider != "deepseek" && cfg.provider != "qwen" && cfg.provider != "relay" {
        return Err(format!(
            "未知 provider：{}（只支持 deepseek / qwen / relay）。",
            cfg.provider
        ));
    }
    // 端口 0 非法（无法监听/探活）。
    if cfg.proxy_port == 0 || cfg.sandbox_port == 0 {
        return Err("端口不能为 0。".into());
    }
    // 代理与沙箱不能同端口，否则互相抢占。
    if cfg.proxy_port == cfg.sandbox_port {
        return Err("代理端口与沙箱端口不能相同。".into());
    }
    // base_url 只做「非空时校验格式」——不在这里强制必填，否则「先选中转站、再填地址」
    // 的流程会在切 provider 时就被拦下。必填校验放在真正要用它的 ensure_proxy /
    // fetch_relay_models（那里给的提示也更贴合动作）。
    let base_url = cfg.base_url.clone().unwrap_or_default().trim().to_string();
    if !base_url.is_empty()
        && !(base_url.starts_with("http://") || base_url.starts_with("https://"))
    {
        return Err("中转站 base_url 必须以 http:// 或 https:// 开头。".into());
    }
    let dir = config::default_dir();
    config::update(&dir, move |c| {
        c.provider = cfg.provider;
        c.proxy_port = cfg.proxy_port;
        c.sandbox_port = cfg.sandbox_port;
        // 只在前端传了 base_url（即用户在中转站模式）时写入 relay 的 base_url，
        // 避免在别的 provider 下改端口时误清空已存的中转站地址。
        if !base_url.is_empty() {
            c.providers.entry("relay".into()).or_default().base_url = base_url;
        }
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn save_provider_key(provider: String, key: String) -> Result<String, String> {
    let dir = config::default_dir();
    let key2 = key.clone();
    config::update(&dir, move |c| {
        c.providers.entry(provider).or_default().key = key2;
    })
    .map_err(|e| e.to_string())?;
    Ok(config::mask(&key))
}

#[tauri::command]
fn start_proxy(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<serde_json::Value, String> {
    let (port, _secret, _action) = ensure_proxy(&app, &state)?;
    Ok(json!({ "port": port }))
}

/// 「存 key 即验证」：确保代理在跑，再经代理向上游发一个**最小**请求
/// （`max_tokens:1`，一句 "ping"），据响应状态码判断 key 是否真的可用。
/// 返回 `{ok, hint}`：ok=true 表示上游接受（key 有效）；ok=false 表示上游拒绝或异常，
/// hint 给人话。彻底避免「只看绿灯（代理起来了）≠ key 真能用」。
#[tauri::command]
fn verify_key(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<serde_json::Value, String> {
    let (port, secret, _action) = ensure_proxy(&app, &state)?;
    // 走稳定模型 id（代理内部映射到当前 provider 的真实模型），非流式、只要 1 个 token。
    let body = br#"{"model":"claude-opus-4-8","max_tokens":1,"messages":[{"role":"user","content":"ping"}]}"#;
    match proc::http_post_status(port, Some(&secret), "/v1/messages", body, 15000) {
        Some(200) => Ok(json!({ "ok": true, "hint": "key 有效，上游已接受。" })),
        Some(code @ (401 | 403)) => Ok(
            json!({ "ok": false, "hint": format!("上游拒绝（{code}），key 可能无效或无权限。") }),
        ),
        Some(code) => Ok(json!({
            "ok": false,
            "hint": format!("上游返回 {code}，可能是 key 无效、额度不足或上游异常。")
        })),
        None => Err("验证请求无响应（多为网络或上游不通）。".to_string()),
    }
}

#[derive(Deserialize)]
struct RelayModelsReq {
    base_url: String,
    key: String,
}

/// 「获取中转站可用模型」：把面板填的 base_url / token 存进 relay 配置（token 为空=沿用已存）、
/// 切到 relay，起 relay 代理，再经**本地回环代理**回源拉中转站 `/v1/models`
/// （回源的 TLS 由代理的 urllib 完成，Rust 侧只打回环明文，无需引 TLS）。
/// 返回 `{models:[id,...]}`——会平铺进 Science 选择器主列表的 id 排前。
#[tauri::command]
fn fetch_relay_models(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    req: RelayModelsReq,
) -> Result<serde_json::Value, String> {
    let dir = config::default_dir();
    let base_url = req.base_url.trim().to_string();
    let key = req.key.trim().to_string();
    // 落盘 relay 配置并切到 relay（供 ensure_proxy 起 relay 代理）。token 为空表示沿用已存的。
    let (bu, k) = (base_url.clone(), key.clone());
    config::update(&dir, move |c| {
        c.provider = "relay".into();
        let e = c.providers.entry("relay".into()).or_default();
        if !bu.is_empty() {
            e.base_url = bu;
        }
        if !k.is_empty() {
            e.key = k;
        }
    })
    .map_err(|e| e.to_string())?;
    // 用落盘后的最终值校验。
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let bu = cfg.base_url_for("relay").unwrap_or_default();
    if bu.is_empty() || !(bu.starts_with("http://") || bu.starts_with("https://")) {
        return Err("请先填写中转站 base_url（http:// 或 https:// 开头）。".into());
    }
    if cfg.key_for("relay").is_none() {
        return Err("请先填写中转站 API Key / Token。".into());
    }
    // 起 relay 代理（内部已校验 key/base_url、探活），经回环代理回源拉 /v1/models。
    let (port, secret, _action) = ensure_proxy(&app, &state)?;
    match proc::http_get_body(port, Some(&secret), "/v1/models", 20000) {
        Some((200, body)) => {
            let v: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| format!("解析模型列表失败：{e}"))?;
            let mut ids: Vec<String> = v
                .get("data")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if ids.is_empty() {
                return Err("中转站没有返回可用模型（/v1/models 为空）。".into());
            }
            // 稳定排序：主列表项（会平铺进选择器）排前，其余保持上游顺序。
            ids.sort_by_key(|id| if is_main_list_model(id) { 0u8 } else { 1u8 });
            Ok(json!({ "models": ids }))
        }
        Some((code @ (401 | 403), _)) => Err(format!("中转站拒绝（{code}），key 或权限可能有误。")),
        Some((code, _)) => Err(format!("中转站返回 {code}，无法获取模型列表。")),
        None => Err("获取模型无响应（多为网络或中转站不通）。".into()),
    }
}

#[tauri::command]
fn stop_all(app: tauri::AppHandle, state: State<'_, Mutex<AppState>>) -> Result<(), String> {
    let mut st = lock(&state);
    // 先停沙箱并记录结果；代理无论如何都杀。沙箱没停干净则如实返错，不虚报成功。
    let sandbox_res = stop_sandbox_inner(&app, &mut st);
    kill_child(&mut st.proxy);
    st.secret.clear();
    sandbox_res.map_err(|e| format!("代理已停；但{e}真实实例 8765 未受影响。"))
}

#[tauri::command]
fn one_click_login(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
) -> Result<serde_json::Value, String> {
    // 1~3. 确保代理在跑且健康（内部已查 key、探活）。带回本次是复用还是重启。
    let (pport, secret, proxy_action) = ensure_proxy(&app, &state)?;

    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let sport = cfg.sandbox_port;

    // sandbox_home() 作沙箱根：伪造器要求解析后的 auth_dir 落在其下，防符号链接重定向（P1）。
    let sbx_home = sandbox_home();
    let auth_dir = sbx_home.join(".claude-science");

    // 沙箱已健康 → 但「daemon 活着」≠「登录态可用」：先只读校验虚拟登录是否自洽（修 0.2.1 Bug2）。
    // - 自洽 → 绝不重伪造、绝不重跑 launch（连 auth 文件都不读，operon 可能正在用），只重取
    //   URL + 打开。修 #3/#6：活动 org 不变，旧对话一直在。
    // - 健康但登录失效（旧版遗留 / 凭证损坏 / 已落登录页）→ 重开也只会再落登录页，故停沙箱、
    //   落到下面「修复保 org + 重启」路径自愈（0.2.0 的健康快捷路径漏了这一步）。
    // P2b：asset_root() 只在下面「需启动」分支才取。
    // P2（GPT 复审）：用 sandbox_running_ours 而非裸端口 /health——按 data-dir 强身份判定，
    // 避免端口被冒名服务占用且恰好返回 200 时误报「已重新打开 Science」。
    if sandbox_running_ours(sport) {
        if oauth_forge::login_intact(&auth_dir, "virtual@localhost.invalid", &sbx_home) {
            let url = sandbox_url(sport);
            {
                let mut st = lock(&state);
                st.sandbox_port = sport;
                st.sandbox_url = Some(url.clone());
            }
            let base = match proxy_action {
                ProxyAction::Reused => "已在运行",
                ProxyAction::Restarted => "已用新配置重启代理，Science 沿用不变",
            };
            // P2c：捕获打开结果——open 失败不谎报「已重新打开」，改提示手动打开。
            let msg = match open_in_browser(&url) {
                Ok(()) => format!("{base}，已重新打开 Science。"),
                Err(_) => format!("{base}，服务已就绪，请手动打开：{url}"),
            };
            return Ok(json!({ "url": url, "msg": msg, "action": "reopened" }));
        }
        // 健康但登录态失效：停沙箱，让下面 relaunch 拿到修复后的登录材料（daemon 运行中不会
        // 重读 auth）。ensure_virtual_login 幂等：保住 org（旧对话不丢），只重铸失效的登录。
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_inner(&app, &mut st);
        }
    }

    // 沙箱没起 / 挂了 / 登录失效已停 → 需要 launch 资源，此时才定位（P2b）。确保虚拟登录（幂等）+ launch。
    let root = asset_root(&app)
        .ok_or("找不到 scripts/launch-virtual-sandbox.sh（打包资源或仓库根均未命中）。")?;

    // 进程内确保虚拟 OAuth（Rust 原生密码学，零 node）。幂等：现有登录完整就复用、部分坏就
    // 修复但保住 org、真首次才铸新 —— 修 #3/#6 的核心（不再无条件换 org 孤儿化旧对话）。
    let (forged, login_action) =
        oauth_forge::ensure_virtual_login(&auth_dir, "virtual@localhost.invalid", &sbx_home)
            .map_err(|e| format!("写虚拟登录失败：{e}"))?;

    let launch = root.join("scripts/launch-virtual-sandbox.sh");
    if !launch.is_file() {
        return Err("找不到 scripts/launch-virtual-sandbox.sh。".into());
    }

    // 4. 起沙箱：脚本以 --detached 起 Science，然后返回。
    let proxy_url = format!("http://127.0.0.1:{pport}/{secret}");
    let logf = open_log("sandbox.log").map_err(|e| format!("建日志失败：{e}"))?;
    // 虚拟登录摘要面包屑（无密钥；uuid/假账号/沙箱路径均不敏感），便于用户附日志排查。
    {
        use std::io::Write;
        let mut lw = &logf;
        let _ = writeln!(
            lw,
            "[oauth] 虚拟登录已就绪（Rust，零 node；action={:?}）：auth_dir={} account={} org={} enc={}",
            login_action,
            forged.auth_dir.display(),
            forged.account_uuid,
            forged.org_uuid,
            forged.enc_file.display()
        );
    }
    let logf2 = logf.try_clone().map_err(|e| e.to_string())?;
    let status = Command::new("zsh") // launch 脚本是 #!/bin/zsh（用了 ${VAR:A} realpath）
        .arg(&launch)
        .arg("--port")
        .arg(sport.to_string())
        .arg("--proxy-url")
        .arg(&proxy_url)
        .arg("--skip-oauth-forge") // OAuth 已由上面 Rust 进程内伪造，脚本别再调 node
        // 沙箱状态落在可写目录（打包后资源目录只读），launch/stop/取 URL 三处同一路径。
        .env("SANDBOX_HOME", sandbox_home())
        .stdout(Stdio::from(logf))
        .stderr(Stdio::from(logf2))
        .status()
        .map_err(|e| format!("起沙箱失败：{e}"))?;
    if !status.success() {
        let tail = redact(&tail_file(&log_path("sandbox.log"), 600), &secret);
        return Err(format!("起沙箱脚本失败。\n{tail}"));
    }

    // 5. 轮询沙箱 /health 直到就绪或超时（~8s）。
    let mut ok = false;
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(100));
        if proc::http_health(sport, None, 400) {
            ok = true;
            break;
        }
    }
    if !ok {
        let tail = redact(&tail_file(&log_path("sandbox.log"), 600), &secret);
        // 探活超时：脚本已把 Science 以 --detached 起在后台，必须停掉，
        // 否则留一个孤儿沙箱进程（修 P2-2）。
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_inner(&app, &mut st); // best-effort 清理，结果不影响这里的报错
        }
        return Err(format!(
            "沙箱起后探活超时（端口 {sport}）。已尝试停掉刚起的沙箱。\n{tail}"
        ));
    }

    // 5b. 身份确认（修 P2 GPT 复审）：/health 200 只证明端口在服务，不证明是我们的 Science。
    // 用 data-dir 强身份再确认一次；不是我们的（端口被冒名服务占用）→ 当启动失败处理，
    // 停掉可能已在后台的沙箱并如实报错，别对着冒名服务谎报「已启动」。
    if !sandbox_running_ours(sport) {
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_inner(&app, &mut st);
        }
        return Err(format!(
            "端口 {sport} 有服务响应，但按 data-dir 确认不是本沙箱 Science（疑似被其它服务占用）。已尝试停掉刚起的沙箱。"
        ));
    }

    // 6. 取 UI URL（登录态），交系统浏览器打开。
    let url = sandbox_url(sport);
    {
        let mut st = lock(&state);
        st.sandbox_port = sport;
        st.sandbox_url = Some(url.clone());
    }
    let started = match login_action {
        oauth_forge::LoginAction::Created => "已启动",
        _ => "沙箱已重新启动，沿用原有对话", // Reused / Repaired
    };
    // P2c：同样捕获打开结果。
    let msg = match open_in_browser(&url) {
        Ok(()) => format!("{started}。"),
        Err(_) => format!("{started}，服务已就绪，请手动打开：{url}"),
    };
    Ok(json!({ "url": url, "msg": msg, "action": "started" }))
}

/// 从 `claude-science url` 的 stdout 里取**第一条**合法 http(s) URL。
/// Science 的 `url` 命令会输出多行（第一行是真 URL，随后行是「single-use…」说明）；把整段
/// stdout 当 URL 交给 `open` 会带上换行与说明文字 → 打开错误入口、nonce 不被正确消费 → 落到
/// `/login`（修 0.2.1 Bug1）。故逐行找第一条以 `http://`/`https://` 开头的行，并只取该行首个
/// 非空白 token（URL 内不含空白，若同行尾随了说明也被切掉）。找不到返回 None。
fn first_http_url(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let t = line.trim();
        if t.starts_with("http://") || t.starts_with("https://") {
            let url = t.split_whitespace().next().unwrap_or(t);
            return Some(url.to_string());
        }
    }
    None
}

/// 取沙箱 UI 链接：`<bin> url --data-dir <home>/.claude-science`，HOME 指向沙箱 HOME。
/// 失败退回 http://127.0.0.1:<port>。沙箱 HOME 用 [`sandbox_home`]（与 launch 时一致）。
fn sandbox_url(port: u16) -> String {
    let home = sandbox_home();
    let data_dir = home.join(".claude-science");
    if Path::new(SCIENCE_BIN).is_file() {
        if let Ok(out) = Command::new(SCIENCE_BIN)
            .arg("url")
            .arg("--data-dir")
            .arg(&data_dir)
            .env("HOME", &home)
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            // 只取第一条合法 URL（修 0.2.1 Bug1）：url 命令多行输出里第一行才是真 URL。
            if let Some(url) = first_http_url(&s) {
                return url;
            }
        }
    }
    format!("http://127.0.0.1:{port}")
}

/// 判断「我们自己的」沙箱 Science 是否在跑（供一键健康分派）。收紧（P2 GPT 复审）：优先用
/// Science 二进制按【我们的 data-dir】查 `{"running":true}`，这是强身份——不会被恰好占用
/// `port` 且返回 200 的冒名服务骗过；再叠加端口 /health 确认确实在服务。二进制不在（纯 dev /
/// 研究者机器）时退化为仅端口探活（原行为）。
fn sandbox_running_ours(port: u16) -> bool {
    let home = sandbox_home();
    let data_dir = home.join(".claude-science");
    if Path::new(SCIENCE_BIN).is_file() {
        match Command::new(SCIENCE_BIN)
            .arg("status")
            .arg("--data-dir")
            .arg(&data_dir)
            .env("HOME", &home)
            .output()
        {
            Ok(out) => {
                let s = String::from_utf8_lossy(&out.stdout);
                // 形如 {"running":true,...}：只认我们这个 data-dir 的 daemon 在跑。
                let running = s.contains("\"running\":true") || s.contains("\"running\": true");
                return running && proc::http_health(port, None, 400);
            }
            // 二进制在但调用失败 → 保守退化到端口探活，别因探测本身出错就误判没起。
            Err(_) => return proc::http_health(port, None, 400),
        }
    }
    proc::http_health(port, None, 400)
}

#[tauri::command]
fn status(state: State<'_, Mutex<AppState>>) -> serde_json::Value {
    // 只在锁内取值，锁外做阻塞探活。
    let (pport, secret, sport, provider, base_url) = {
        let st = lock(&state);
        let cfg = config::load_from(&config::default_dir()).unwrap_or_default();
        let pport = if st.proxy_port != 0 {
            st.proxy_port
        } else {
            cfg.proxy_port
        };
        let sport = if st.sandbox_port != 0 {
            st.sandbox_port
        } else {
            cfg.sandbox_port
        };
        let base_url = cfg.base_url_for(&cfg.provider).unwrap_or_default();
        (pport, st.secret.clone(), sport, cfg.provider, base_url)
    };
    let proxy = if !secret.is_empty() && proc::http_health(pport, Some(&secret), 300) {
        "green"
    } else {
        "amber"
    };
    // 状态灯也用 data-dir 强身份（修 P2 GPT 复审），避免端口被冒名服务占用时误显绿灯。
    // status() 是按需调用（前端 refreshStatus 在动作后触发，非高频轮询），一次子进程可接受。
    let sandbox = if sandbox_running_ours(sport) {
        "green"
    } else {
        "amber"
    };
    let uhost = upstream_host(&provider, &base_url);
    let upstream = if !uhost.is_empty() && proc::tcp_reachable(&uhost, 443, 500) {
        "green"
    } else {
        "amber"
    };
    json!({ "proxy": proxy, "sandbox": sandbox, "upstream": upstream })
}

#[tauri::command]
fn open_url(state: State<'_, Mutex<AppState>>) -> Result<(), String> {
    let url = { lock(&state).sandbox_url.clone() };
    let url = url.ok_or("还没有沙箱 URL，请先「一键开始」。")?;
    open_in_browser(&url)
}

#[tauri::command]
fn run_doctor(app: tauri::AppHandle) -> Result<String, String> {
    let root = asset_root(&app).ok_or("找不到 scripts/doctor.sh（打包资源或仓库根均未命中）。")?;
    let cfg = config::load_from(&config::default_dir()).unwrap_or_default();
    let doctor = root.join("scripts/doctor.sh");
    let mut cmd = Command::new("bash");
    cmd.arg(&doctor)
        .env("CSSWITCH_PROVIDER", &cfg.provider)
        .env("CSSWITCH_PROXY_PORT", cfg.proxy_port.to_string())
        .env("CSSWITCH_SANDBOX_PORT", cfg.sandbox_port.to_string());
    // doctor 只做 -n 判空来报 key 有无。只让它知道「存在」，绝不把真实 key 传进其环境。
    if cfg.key_for(&cfg.provider).is_some() {
        cmd.env(key_env(&cfg.provider), "***present***");
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        text.push_str("\n[stderr] ");
        text.push_str(err.trim());
    }
    Ok(text)
}

/// 当前 app 版本（供前端「检查更新」与页脚版本号用）。
#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 打开 GitHub Releases 页（检查更新时用系统浏览器打开，浏览器走用户自己的代理）。
#[tauri::command]
fn open_release_page() -> Result<(), String> {
    open_in_browser("https://github.com/SuperJJ007/CSswitch/releases/latest")
}

/// 打开「报 bug」页（预填 bug 模板）；用系统浏览器，走用户自己的代理。
#[tauri::command]
fn report_bug() -> Result<(), String> {
    open_in_browser("https://github.com/SuperJJ007/CSswitch/issues/new?template=bug_report.yml")
}

/// 在访达里打开日志目录 `~/.csswitch/logs`，方便用户附到 bug 反馈里（先自查有无密钥）。
#[tauri::command]
fn open_logs() -> Result<(), String> {
    let dir = config::default_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    Command::new("open")
        .arg(&dir)
        .status()
        .map_err(|e| format!("打开日志目录失败：{e}"))?;
    Ok(())
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle, state: State<'_, Mutex<AppState>>) -> Result<(), String> {
    // 默认：退 app 停代理、保留沙箱运行（spec §5.1）。
    {
        let mut st = lock(&state);
        kill_child(&mut st.proxy);
        st.secret.clear();
    }
    app.exit(0);
    Ok(())
}

// ---------- 入口 ----------
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(Mutex::new(AppState::default()))
        .invoke_handler(tauri::generate_handler![
            get_config,
            set_config,
            set_mode,
            open_official,
            save_provider_key,
            start_proxy,
            verify_key,
            fetch_relay_models,
            stop_all,
            one_click_login,
            status,
            open_url,
            run_doctor,
            app_version,
            open_release_page,
            report_bug,
            open_logs,
            quit_app
        ])
        .setup(|app| {
            // 正常桌面应用：进 Dock、走常规应用生命周期（默认 Regular 策略，
            // 不再设 Accessory）。窗口在 tauri.conf.json 里配了 decorations（标题栏
            // 三键：关闭/最小化/缩放）+ visible + center，启动即居中弹出、可拖动
            // （修 #4；标题栏自带拖动，顺带解决 #1 拖不动）。托盘图标已移除。

            // 遗留 provider=relay 迁移（防御项，幂等）：把 PR #4 单槽迁到多槽 relay-<preset>。
            // 本机无 relay 槽 → no-op；下游若有旧槽则一次性迁移并落盘。
            {
                let dir = config::default_dir();
                if let Ok(mut cfg) = config::load_from(&dir) {
                    if relay_presets::migrate_legacy_relay(&mut cfg) {
                        let _ = config::update(&dir, |c| *c = cfg.clone());
                    }
                }
            }

            // 关窗即退出：与「退出」按钮一致 —— 停代理、清 secret，保留沙箱运行
            // （spec §5.1）。不接这一步，从标题栏红叉关窗会绕过 quit_app 直接退，
            // 把代理子进程留成孤儿。
            if let Some(win) = app.get_webview_window("main") {
                let handle = app.handle().clone();
                win.on_window_event(move |ev| {
                    if let tauri::WindowEvent::CloseRequested { .. } = ev {
                        let state = handle.state::<Mutex<AppState>>();
                        let mut st = lock(&state);
                        kill_child(&mut st.proxy);
                        st.secret.clear();
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::{
        first_http_url, is_main_list_model, key_fingerprint, parse_host, redact, sandbox_home,
    };

    #[test]
    fn first_http_url_takes_only_first_valid_url() {
        // Science 的 `url` 命令输出两行：第一行是真 URL，第二行是「single-use…」说明。
        // 旧代码把整段 stdout 当 URL 交给 open → 换行+说明污染参数、nonce 不被消费 → 落登录页。
        // 只能取第一条合法 http(s) URL（修 0.2.1 Bug1）。
        let multi = "http://127.0.0.1:8990/setup?nonce=abc123\n\
                     This is a single-use link, expires in 60 seconds.";
        assert_eq!(
            first_http_url(multi).as_deref(),
            Some("http://127.0.0.1:8990/setup?nonce=abc123"),
            "多行输出必须只取第一行 URL，丢弃说明文字"
        );
        // 同一行 URL 后跟了说明，只取 URL token（URL 内不含空白）。
        let inline = "https://x.example/y?z=1  (single-use)";
        assert_eq!(
            first_http_url(inline).as_deref(),
            Some("https://x.example/y?z=1")
        );
        // 前导非 URL 行被跳过，取第一条 http 行。
        let lead = "Open this link in your browser:\nhttp://127.0.0.1:8990/a";
        assert_eq!(
            first_http_url(lead).as_deref(),
            Some("http://127.0.0.1:8990/a")
        );
        // 无任何 URL → None（sandbox_url 据此退回裸端口）。
        assert_eq!(first_http_url("no url here\nnor here"), None);
        // 单行纯 URL 原样返回。
        assert_eq!(
            first_http_url("http://127.0.0.1:8990").as_deref(),
            Some("http://127.0.0.1:8990")
        );
    }

    #[test]
    fn parse_host_extracts_host_from_relay_base_url() {
        assert_eq!(
            parse_host("https://byteswarm.ai/claude").as_deref(),
            Some("byteswarm.ai")
        );
        assert_eq!(
            parse_host("http://127.0.0.1:8080/v1").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            parse_host("https://relay.example.com:8443").as_deref(),
            Some("relay.example.com")
        );
        // 无 scheme / 空 → None（status 上游灯据此显黄，不误探）。
        assert_eq!(parse_host("byteswarm.ai/claude"), None);
        assert_eq!(parse_host(""), None);
    }

    #[test]
    fn main_list_model_matches_family_plus_digit() {
        // 会平铺进 Science 选择器主列表的。
        assert!(is_main_list_model("claude-opus-4-8"));
        assert!(is_main_list_model("claude-sonnet-5"));
        assert!(is_main_list_model("claude-haiku-4-5-20251001"));
        // 不会平铺（折叠进 More models）：老式命名 / family 后非数字。
        assert!(!is_main_list_model("claude-3-5-sonnet-20241022"));
        assert!(!is_main_list_model("claude-fable-5"));
        assert!(!is_main_list_model("gpt-4o"));
    }

    #[test]
    fn redact_scrubs_secret_and_is_noop_when_empty() {
        assert_eq!(
            redact("推理指向 http://127.0.0.1:18991/abcd1234 尾巴", "abcd1234"),
            "推理指向 http://127.0.0.1:18991/**** 尾巴"
        );
        assert_eq!(redact("原样返回", ""), "原样返回");
        assert!(!redact("leak abcd1234 leak abcd1234", "abcd1234").contains("abcd1234"));
    }

    #[test]
    fn key_fingerprint_stable_and_distinct() {
        // 同 key 稳定、异 key 不同：这是「换 key 触发代理重启」判断的基础（P1-2）。
        assert_eq!(key_fingerprint("sk-aaaa"), key_fingerprint("sk-aaaa"));
        assert_ne!(key_fingerprint("sk-aaaa"), key_fingerprint("sk-bbbb"));
        assert_ne!(key_fingerprint(""), key_fingerprint("x"));
    }

    #[test]
    fn sandbox_home_is_writable_under_config_dir() {
        // 沙箱状态目录必须在可写的 ~/.csswitch 下（不在只读的 .app 资源里）——P1-1。
        let h = sandbox_home();
        assert!(h.ends_with("sandbox/home"), "应以 sandbox/home 结尾：{h:?}");
        assert!(
            h.to_string_lossy().contains(".csswitch"),
            "应在 .csswitch 下：{h:?}"
        );
    }
}
