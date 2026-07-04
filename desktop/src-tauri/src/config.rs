//! 本地配置读写：`~/.csswitch/config.json`。
//!
//! 安全要求（对齐 spec §3 / §5.1，参考 CC Switch 的明文本地存储但加严文件安全）：
//!   - 目录 0700，文件 0600。
//!   - 读/写前 `lstat`（symlink_metadata）拒绝符号链接，绝不跟随写到别处或读到别处。
//!   - 写用「临时文件（O_CREAT|O_EXCL, 0600）+ 原子 rename」，避免半写与竞态。
//!   - provider key 明文存盘（用户已知悉），但**绝不进日志**；回显给前端只给掩码（末 4 位）。
//!
//! 所有函数以显式 `dir` 参数工作，便于用临时目录做无副作用的单元测试；
//! 生产代码用 [`default_dir`]（`$HOME/.csswitch`）。

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn default_provider() -> String {
    "deepseek".to_string()
}
fn default_proxy_port() -> u16 {
    18991
}
fn default_sandbox_port() -> u16 {
    8990
}
fn default_mode() -> String {
    "proxy".to_string()
}

/// 单个 provider 的配置。目前只有 key（明文存盘）。
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct ProviderCfg {
    #[serde(default)]
    pub key: String,
}

/// 顶层配置。字段都有默认值，缺字段的旧文件也能读。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Config {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
    #[serde(default = "default_sandbox_port")]
    pub sandbox_port: u16,
    /// 代理的 path-secret。**持久化**并跨代理重启/换 key/换 provider/重开 app 复用，
    /// 这样已在跑的沙箱（其 ANTHROPIC_BASE_URL 里嵌了该 secret）不会因代理换 secret 而 403。
    /// 首次为空，由后端生成一次后写回。
    #[serde(default)]
    pub secret: String,
    /// 运行模式："proxy"（第三方：起沙箱+代理+虚拟登录）| "official"（用你真实的 Claude Science）。
    /// 官方模式下 CSSwitch 只负责把你交回真实客户端，绝不碰/托管你的官方登录。
    #[serde(default = "default_mode")]
    pub mode: String,
    /// CONNECT 隧道上游代理（host:port，如 "127.0.0.1:7890"）。非空时非 Anthropic 域的
    /// HTTPS CONNECT 隧道经此代理而非直连目标，解决 MCP 外部数据库（PubMed/ChEMBL 等）
    /// 在国内直连不可达的问题（#11）。
    #[serde(default)]
    pub upstream_proxy: String,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCfg>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            provider: default_provider(),
            proxy_port: default_proxy_port(),
            sandbox_port: default_sandbox_port(),
            secret: String::new(),
            mode: default_mode(),
            upstream_proxy: String::new(),
            providers: BTreeMap::new(),
        }
    }
}

impl Config {
    /// 取某 provider 的完整 key（供后端注入子进程环境变量用；调用方绝不可打印/记录）。
    pub fn key_for(&self, provider: &str) -> Option<String> {
        self.providers
            .get(provider)
            .map(|p| p.key.clone())
            .filter(|k| !k.is_empty())
    }
}

/// 生产环境配置目录：`$HOME/.csswitch`。
pub fn default_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".csswitch")
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

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
    let md = fs::metadata(dir)?;
    if !md.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("配置目录不是目录：{}", dir.display()),
        ));
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// 从 `dir/config.json` 读配置。文件不存在返回 [`Config::default`]。
/// 文件是符号链接则报错（不跟随读）。读到后把权限复位为 0600。
pub fn load_from(dir: &Path) -> io::Result<Config> {
    // 目录本身也不许是符号链接：否则攻击者把 ~/.csswitch 换成软链，
    // 就能让读取跟随到别处（与文件头声明的「读/写前 lstat 拒绝符号链接」一致）。
    assert_not_symlink(dir)?;
    let path = config_path(dir);
    assert_not_symlink(&path)?;
    let data = match fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e),
    };
    // 存在即复位权限，抵御外部把它改宽。
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    let cfg: Config = serde_json::from_slice(&data).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config.json 解析失败：{e}"),
        )
    })?;
    Ok(cfg)
}

/// 原子写 `dir/config.json`（0600）。目录/目标文件是符号链接则拒绝。
pub fn save_to(dir: &Path, cfg: &Config) -> io::Result<()> {
    ensure_dir(dir)?;
    let path = config_path(dir);
    assert_not_symlink(&path)?;
    let json = serde_json::to_vec_pretty(cfg).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config 序列化失败：{e}"),
        )
    })?;

    // 临时文件与目标同目录（保证 rename 在同一文件系统上原子）。
    // 名字带 pid + 线程 id，避免同进程并发写者撞同一个 O_EXCL 临时名。
    let tmp = dir.join(format!(
        ".config.json.tmp-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    // O_CREAT|O_EXCL + 0600：拒绝复用已有临时文件，创建即定权限。
    let write_res = (|| -> io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // rename 覆盖目标名本身（替换符号链接名而非跟随），但上面已 assert 目标非链接。
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// 序列化的「读-改-写」：进程内全局写锁下 load → apply → save，避免并发命令
/// （set_config / save_provider_key）各读一份旧 config、各改一个字段、互相覆盖。
pub fn update<F: FnOnce(&mut Config)>(dir: &Path, f: F) -> io::Result<Config> {
    static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cfg = load_from(dir)?;
    f(&mut cfg);
    save_to(dir, &cfg)?;
    Ok(cfg)
}

/// 掩码：只保留末 4 位，其余用 • 遮蔽。空 key 返回空串。
/// 绝不返回完整 key，是回显给前端的唯一形式。
pub fn mask(key: &str) -> String {
    let n = key.chars().count();
    if n == 0 {
        String::new()
    } else if n <= 4 {
        "•".repeat(n)
    } else {
        let last4: String = key.chars().skip(n - 4).collect();
        format!("{}{}", "•".repeat(n - 4), last4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

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

    #[test]
    fn load_missing_returns_default() {
        let d = tmpdir().join(".csswitch");
        let cfg = load_from(&d).unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.provider, "deepseek");
        assert_eq!(cfg.proxy_port, 18991);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let d = tmpdir().join(".csswitch");
        let mut cfg = Config {
            provider: "qwen".into(),
            proxy_port: 12345,
            ..Default::default()
        };
        cfg.providers.insert(
            "deepseek".into(),
            ProviderCfg {
                key: "sk-abcdef1234".into(),
            },
        );
        save_to(&d, &cfg).unwrap();
        let got = load_from(&d).unwrap();
        assert_eq!(got, cfg);
        assert_eq!(got.key_for("deepseek").as_deref(), Some("sk-abcdef1234"));
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
        // 目标：/tmp 下一个「真实」文件，配置文件是指向它的符号链接。
        let target = base.join("real-elsewhere.txt");
        fs::write(&target, b"ORIGINAL").unwrap();
        symlink(&target, config_path(&d)).unwrap();
        let err = save_to(&d, &Config::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // 目标零改动。
        assert_eq!(fs::read(&target).unwrap(), b"ORIGINAL");
    }

    #[test]
    fn load_rejects_symlinked_file() {
        let base = tmpdir();
        let d = base.join(".csswitch");
        fs::create_dir_all(&d).unwrap();
        let target = base.join("secret.txt");
        fs::write(&target, b"{\"provider\":\"leak\"}").unwrap();
        symlink(&target, config_path(&d)).unwrap();
        let err = load_from(&d).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn load_rejects_symlinked_dir() {
        // ~/.csswitch 本身被换成软链时，load 也必须拒绝（不跟随读到别处）——修 P1-3。
        let base = tmpdir();
        let realdir = base.join("realdir");
        fs::create_dir_all(&realdir).unwrap();
        fs::write(realdir.join("config.json"), b"{\"provider\":\"leak\"}").unwrap();
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
            c.provider = "qwen".into();
            c.providers.insert(
                "qwen".into(),
                ProviderCfg {
                    key: "k-xyz".into(),
                },
            );
        })
        .unwrap();
        let got = load_from(&d).unwrap();
        assert_eq!(got.provider, "qwen");
        assert_eq!(got.key_for("qwen").as_deref(), Some("k-xyz"));
    }

    #[test]
    fn secret_persists_and_survives_reload() {
        // path-secret 一旦生成必须持久化，代理重启/重开 app 仍是同一个值，
        // 否则已在跑的沙箱会因 secret 变了而 403（P1）。
        let d = tmpdir().join(".csswitch");
        save_to(&d, &Config::default()).unwrap();
        assert!(load_from(&d).unwrap().secret.is_empty(), "初始应为空");
        update(&d, |c| c.secret = "deadbeef00112233".into()).unwrap();
        assert_eq!(load_from(&d).unwrap().secret, "deadbeef00112233");
        // 再改别的字段，secret 不受影响。
        update(&d, |c| c.provider = "qwen".into()).unwrap();
        assert_eq!(load_from(&d).unwrap().secret, "deadbeef00112233");
    }

    #[test]
    fn mask_hides_all_but_last4() {
        assert_eq!(mask("sk-1234567890ab"), "•".repeat(11) + "90ab"); // 15 字符 → 11 掩 + 末4
        assert_eq!(mask(""), "");
        assert_eq!(mask("abc"), "•••");
        assert_eq!(mask("abcd"), "••••");
        assert_eq!(mask("abcde"), "•bcde");
        // 关键不变量：掩码绝不含完整 key。
        let full = "sk-secret-tail9999";
        assert!(!mask(full).contains("secret"));
    }
}
