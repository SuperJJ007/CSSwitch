//! 进程管家用到的辅助：探活、依赖定位、一次性 secret 生成、上游可达性。
//! 跨平台适配：`/dev/urandom` 改用 `rand::OsRng`，文件可执行判断用 `fs_ext::is_executable`。
//! 有状态的子进程编排放在 lib.rs（持 Child 句柄）。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
// Command/Stdio 仅在 Unix 的 which_via_login_shell() 中使用，
// Windows 上该函数被 #[cfg(unix)] 跳过，但保留 import 避免 Linux 编译报错。
#[allow(unused_imports)]
use std::process::{Command, Stdio};
use std::time::Duration;

use rand::rngs::OsRng;
use rand::RngCore;

/// 对本地回环代理做 HTTP 探活：`GET /<secret>/health`，响应状态行含 200 即视为健康。
/// 代理带 path-secret 鉴权时必须带上 secret，否则会拿到 403。
pub fn http_health(port: u16, secret: Option<&str>, timeout_ms: u64) -> bool {
    let addr = match ("127.0.0.1", port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
    {
        Some(a) => a,
        None => return false,
    };
    let dur = Duration::from_millis(timeout_ms);
    let mut stream = match TcpStream::connect_timeout(&addr, dur) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(dur));
    let _ = stream.set_write_timeout(Some(dur));
    let path = match secret {
        Some(s) if !s.is_empty() => format!("/{s}/health"),
        _ => "/health".to_string(),
    };
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    // 只需读到状态行；读上限防呆。
    let mut chunk = [0u8; 1024];
    while buf.len() < 8192 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or("");
    // 形如 "HTTP/1.1 200 OK"：严格取第二段等于 200，避免 contains 误配 reason phrase。
    status_line.split_whitespace().nth(1) == Some("200")
}

/// 向本地回环代理 POST 一段 JSON（`POST /<secret><path>`），返回 HTTP 响应状态码；
/// 连不上 / 无响应返回 None。用于「存 key 后用最小请求真正验一次 key」——
/// 请求经代理打到上游，200=可用，401/403=key 被拒。回环明文，无需 TLS。
/// timeout_ms 要给足（代理要转发上游），建议调用方传 ~15000。
pub fn http_post_status(
    port: u16,
    secret: Option<&str>,
    path_suffix: &str,
    body: &[u8],
    timeout_ms: u64,
) -> Option<u16> {
    let addr = ("127.0.0.1", port).to_socket_addrs().ok()?.next()?;
    let dur = Duration::from_millis(timeout_ms);
    let mut stream = TcpStream::connect_timeout(&addr, dur).ok()?;
    let _ = stream.set_read_timeout(Some(dur));
    let _ = stream.set_write_timeout(Some(dur));
    let path = match secret {
        Some(s) if !s.is_empty() => format!("/{s}{path_suffix}"),
        _ => path_suffix.to_string(),
    };
    let req = format!(
        "POST {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).ok()?;
    stream.write_all(body).ok()?;
    // 只需读到状态行（非流式响应，状态行随首个头块到达）；读上限防呆。
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < 8192 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or("");
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
}

/// 向本地回环代理 GET 一段路径（`GET /<secret><path>`），返回 (状态码, 响应体)。
/// 连不上 / 无响应返回 None。用于经代理回源拉中转站 `/v1/models`——代理有 TLS（urllib），
/// 这里只打回环明文，无需在 Rust 侧引 TLS。timeout_ms 要给足（代理要转发上游），建议 ~15000。
pub fn http_get_body(
    port: u16,
    secret: Option<&str>,
    path_suffix: &str,
    timeout_ms: u64,
) -> Option<(u16, String)> {
    let addr = ("127.0.0.1", port).to_socket_addrs().ok()?.next()?;
    let dur = Duration::from_millis(timeout_ms);
    let mut stream = TcpStream::connect_timeout(&addr, dur).ok()?;
    let _ = stream.set_read_timeout(Some(dur));
    let _ = stream.set_write_timeout(Some(dur));
    let path = match secret {
        Some(s) if !s.is_empty() => format!("/{s}{path_suffix}"),
        _ => path_suffix.to_string(),
    };
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    // 读完整响应（状态行 + 头 + 体）。上限防呆：模型列表通常 < 数十 KB，给 1 MiB。
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if buf.len() > 1_048_576 {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())?;
    // 分割头与体：首个空行之后即响应体。
    let body = match text.split_once("\r\n\r\n") {
        Some((_, b)) => b.to_string(),
        None => String::new(),
    };
    Some((status, body))
}

/// 上游主机可达性（仅 TCP 连通，不校验 key）。绿灯=可达，黄灯=不可达。
pub fn tcp_reachable(host: &str, port: u16, timeout_ms: u64) -> bool {
    let dur = Duration::from_millis(timeout_ms);
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            for a in addrs {
                if TcpStream::connect_timeout(&a, dur).is_ok() {
                    return true;
                }
            }
            false
        }
        Err(_) => false,
    }
}

/// 在 PATH 里找可执行文件（简易 which），并对 macOS GUI 最小 PATH 做兜底。
///
/// 从访达 / .app 启动的 GUI 进程拿到的是最小 PATH（`/usr/bin:/bin:/usr/sbin:/sbin`），
/// **不含** Homebrew(`/usr/local/bin`、`/opt/homebrew/bin`)、nvm / volta / asdf 等
/// node 常见安装位置 → `which("node")` 在正常 PATH 里查不到（`python3` 因
/// `/usr/bin/python3` 在系统 PATH 里才没事）。故 PATH 未命中时，再扫一遍
/// [`common_bin_dirs`] 里的常见安装目录（修 #2）。找不到返回 None。
pub fn which(name: &str) -> Option<PathBuf> {
    // 绝对/相对路径直接判定。
    let p = PathBuf::from(name);
    if p.is_absolute() {
        return if is_exec(&p) { Some(p) } else { None };
    }
    // 1) 正常 PATH（从终端启动时够用）。PATH 缺失也不早退，继续走兜底。
    if let Some(path) = std::env::var_os("PATH") {
        if let Some(hit) = find_in_dirs(name, std::env::split_paths(&path)) {
            return Some(hit);
        }
    }
    // 2) GUI/.app 最小 PATH 兜底：扫常见安装目录（仅 Unix）。
    #[cfg(unix)]
    if let Some(hit) = find_in_dirs(name, common_bin_dirs()) {
        return Some(hit);
    }
    None
}

/// 在给定目录序列里找可执行文件（第一个命中即返回）。
fn find_in_dirs(name: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    for dir in dirs {
        let cand = dir.join(name);
        if is_exec(&cand) {
            return Some(cand);
        }
    }
    None
}

/// Unix 上 node/python 等的常见安装目录（不含系统最小 PATH 已覆盖的 `/usr/bin` 等）：
/// Homebrew(Apple Silicon / Intel)、MacPorts、volta、asdf、`~/.local/bin`，
/// 以及 nvm 各版本 `~/.nvm/versions/node/<ver>/bin`（目录枚举）。
/// Windows 上不需要此项（PATH 已包含常见安装位置或被远程模式替代）。
#[cfg(unix)]
fn common_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"), // Homebrew（Apple Silicon）
        PathBuf::from("/usr/local/bin"),    // Homebrew（Intel）/ 手动安装
        PathBuf::from("/opt/local/bin"),    // MacPorts
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".volta/bin"));
        dirs.push(home.join(".asdf/shims"));
        dirs.push(home.join(".local/bin"));
        // nvm：版本目录动态，枚举 ~/.nvm/versions/node/*/bin。
        if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            for e in entries.flatten() {
                dirs.push(e.path().join("bin"));
            }
        }
    }
    dirs
}

/// [`which`] 找不到时的最后兜底：用登录 shell 解析用户的**真实 PATH**。
/// 仅 Unix 平台可用（依赖 zsh）。Windows 上由远程模式替代本地查找。
///
/// GUI/.app 从访达启动只有最小 PATH，且用户可能用 fnm / nvm / asdf 等在 `.zshrc`
/// 里配置的版本管理器（[`common_bin_dirs`] 的静态枚举覆盖不到）。这里跑
/// `zsh -lic 'command -v <name>'`（登录 + 交互 shell，会 source 用户 rc）拿其真实
/// 解析路径。用独立线程 + `recv_timeout` 兜底，病态 rc 不会卡死调用方。
#[cfg(unix)]
pub fn which_via_login_shell(name: &str) -> Option<PathBuf> {
    // name 出自本代码（"node"/"python3"），仍做白名单，杜绝拼进 shell 的注入面。
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
    {
        return None;
    }
    let arg = format!("command -v {name} 2>/dev/null");
    // spawn + 轮询 + 超时 kill：病态 rc 卡死时**终止** zsh，绝不泄漏线程/进程（修 P3）。
    let mut child = Command::new("zsh")
        .args(["-lic", &arg])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // 3s 足够 command -v。到点未退则 kill 后放弃。
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    // rc 可能往 stdout 打噪声：从后往前取第一条「绝对路径且可执行」的行。
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines().rev() {
        let p = PathBuf::from(line.trim());
        if p.is_absolute() && is_exec(&p) {
            return Some(p);
        }
    }
    None
}

/// 定位可执行文件（含登录 shell 兜底）：[`which`]（PATH + 常见安装目录）未命中时，
/// 在 Unix 上再用 [`which_via_login_shell`] 解析用户真实 PATH。
/// Windows 上仅走 `which()`（PATH 搜索），因为无 zsh 登录 shell 且远程模式为主要场景。
/// node / python3 都走这个，覆盖「GUI 最小 PATH + 版本管理器」问题（修 #2）。
pub fn find_exe(name: &str) -> Option<PathBuf> {
    let hit = which(name);
    #[cfg(unix)]
    let hit = hit.or_else(|| which_via_login_shell(name));
    hit
}

/// 判断路径是否为可执行文件。
/// 跨平台：Unix 检查执行权限位 `0o111`，Windows 仅检查是否为文件（扩展名判断由调用方负责）。
fn is_exec(p: &std::path::Path) -> bool {
    match std::fs::metadata(p) {
        Ok(md) => crate::fs_ext::is_executable(&md),
        Err(_) => false,
    }
}

/// 生成一次性 path-secret。
/// 使用操作系统加密级随机源（Unix: `/dev/urandom`；Windows: `BCryptGenRandom`）取 16 字节，
/// hex 编码为 32 字符。失败关闭，绝不退回可猜的弱 secret（宁可起代理失败）。
/// 跨平台：用 `rand::OsRng` 替代直接读 `/dev/urandom`，Windows 下对应 `BCryptGenRandom`。
pub fn gen_secret() -> std::io::Result<String> {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    Ok(hex(&b))
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        s.push(H[(byte >> 4) as usize] as char);
        s.push(H[(byte & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_false_when_nothing_listening() {
        // 一个几乎肯定没人监听的高端口。
        assert!(!http_health(59999, None, 300));
    }

    /// PATH 中找 sh（仅 Unix）。
    #[cfg(unix)]
    #[test]
    fn get_body_none_when_nothing_listening() {
        // 没人监听 → 连不上 → None（与 http_health 一致的失败关闭语义）。
        assert!(http_get_body(59998, Some("secret"), "/v1/models", 300).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn which_finds_sh() {
        let sh = which("sh");
        assert!(sh.is_some(), "PATH 里应能找到 sh");
        assert!(sh.unwrap().is_absolute());
    }

    #[test]
    fn which_absent_returns_none() {
        assert!(which("definitely-not-a-real-binary-xyzzy").is_none());
    }

    /// 在常见目录找 sh（仅 Unix）。
    #[cfg(unix)]
    #[test]
    fn find_in_dirs_locates_exec() {
        // /bin/sh 几乎肯定存在且可执行。
        let hit = find_in_dirs("sh", vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        assert!(hit.is_some(), "应在 /usr/bin 或 /bin 里找到 sh");
        assert!(is_exec(&hit.unwrap()));
    }

    #[test]
    fn find_in_dirs_none_when_absent() {
        assert!(find_in_dirs("definitely-not-xyzzy", vec![PathBuf::from("/bin")]).is_none());
    }

    /// 登录 shell 解析可执行文件（仅 Unix，依赖 zsh）。
    #[cfg(unix)]
    #[test]
    fn login_shell_resolves_sh_when_zsh_present() {
        // 环境无 zsh 则跳过（CI 容器可能没有）。
        if which("zsh").is_none() {
            return;
        }
        let p = which_via_login_shell("sh");
        assert!(p.is_some(), "登录 shell 应能解析 sh");
        let p = p.unwrap();
        assert!(p.is_absolute() && is_exec(&p));
    }

    /// 登录 shell 拒绝恶意名称（仅 Unix）。
    #[cfg(unix)]
    #[test]
    fn login_shell_rejects_bad_names_without_spawning() {
        // 白名单：带 shell 元字符的名字直接拒（防注入），空名亦拒。
        assert!(which_via_login_shell("node; rm -rf /").is_none());
        assert!(which_via_login_shell("$(whoami)").is_none());
        assert!(which_via_login_shell("").is_none());
    }

    /// find_exe 应能找到 sh（仅 Unix）。
    #[cfg(unix)]
    #[test]
    fn find_exe_finds_sh() {
        assert!(find_exe("sh").is_some());
    }

    /// 常见 bin 目录覆盖 Homebrew 和版本管理器（仅 Unix）。
    #[cfg(unix)]
    #[test]
    fn common_bin_dirs_covers_homebrew_and_home_managers() {
        let dirs = common_bin_dirs();
        // Homebrew 两个前缀 + MacPorts 必在。
        assert!(dirs
            .iter()
            .any(|d| d == &PathBuf::from("/opt/homebrew/bin")));
        assert!(dirs.iter().any(|d| d == &PathBuf::from("/usr/local/bin")));
        assert!(dirs.iter().any(|d| d == &PathBuf::from("/opt/local/bin")));
        // HOME 存在时应含版本管理器目录（volta）。
        if std::env::var_os("HOME").is_some() {
            assert!(
                dirs.iter()
                    .any(|d| d.to_string_lossy().contains(".volta/bin")),
                "HOME 下应含 .volta/bin 兜底目录"
            );
        }
    }

    #[test]
    fn gen_secret_is_32_hex_and_varies() {
        let a = gen_secret().unwrap();
        let b = gen_secret().unwrap();
        assert_eq!(a.len(), 32, "urandom 路径应是 32 hex 字符");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次生成应不同");
    }

    #[test]
    fn hex_encodes_known_bytes() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
    }
}
