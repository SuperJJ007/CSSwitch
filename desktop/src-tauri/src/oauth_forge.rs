//! Creates isolated local credentials used only inside a CSSwitch-managed sandbox.
//! Guardrails reject the real account directory, non-local identities, and symlink escapes.
//! Writes use restrictive permissions and atomic replacement.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

const KEY_NAMES: [&str; 4] = [
    "ANTHROPIC_API_KEY_ENCRYPTION_KEY",
    "OAUTH_ENCRYPTION_KEY",
    "JWT_SIGNING_SECRET",
    "USER_SECRET_ENCRYPTION_KEY",
];
const HKDF_INFO: &[u8] = b"operon:aes-256-gcm:oauth";
const AAD: &[u8] = b"v2:oauth";

/// Isolated credential setup summary; contains no secret material.
#[derive(Debug)]
pub struct ForgeResult {
    pub auth_dir: PathBuf,
    pub account_uuid: String,
    pub org_uuid: String,
    pub enc_file: PathBuf,
}

/// 一键登录本次对沙箱虚拟登录做了什么（供上层据实提示）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoginAction {
    Reused,   // 现有登录完整自洽，凭证原样复用（升级时可能仅补 CSSwitch 私有标记）
    Repaired, // 部分损坏，重写但沿用原 org（旧对话不丢）
    Created,  // 真首次，铸全新 org
}

/// A candidate history organization stays backend-only. The frontend receives
/// a random, one-time reference instead of this UUID or its filesystem path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryOrgCandidate {
    org_uuid: String,
    device: u64,
    inode: u64,
}

#[derive(Debug)]
pub(crate) enum EnsureVirtualLoginError {
    Message(String),
    HistoryChoiceRequired(Vec<HistoryOrgCandidate>),
}

impl std::fmt::Display for EnsureVirtualLoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => f.write_str(message),
            Self::HistoryChoiceRequired(candidates) => write!(
                f,
                "检测到 {} 份历史记录，需在 CSSwitch 中选择要恢复的一份。",
                candidates.len()
            ),
        }
    }
}

impl std::error::Error for EnsureVirtualLoginError {}

impl From<String> for EnsureVirtualLoginError {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

impl From<&str> for EnsureVirtualLoginError {
    fn from(value: &str) -> Self {
        Self::Message(value.to_string())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct VirtualOrgMarker {
    schema_version: u8,
    auth_dir_fingerprint: String,
    org_uuid: String,
}

enum MarkerRead {
    Missing,
    Valid(VirtualOrgMarker),
}

const VIRTUAL_ORG_MARKER_MAX_BYTES: u64 = 4096;
const VIRTUAL_ORG_MARKER_FILE: &str = "virtual-org.v1.json";

// ---------- 随机与编码 ----------
fn rand_bytes(n: usize) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut b = vec![0u8; n];
    f.read_exact(&mut b)?;
    Ok(b)
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0xf) as usize] as char);
    }
    s
}

/// Generates a base64-encoded random key value.
fn b64_32() -> std::io::Result<String> {
    Ok(B64.encode(rand_bytes(32)?))
}

/// RFC 4122 v4 UUID（16 随机字节 + 版本/变体位）。
fn uuid_v4() -> std::io::Result<String> {
    let mut b = rand_bytes(16)?;
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    ))
}

// ---------- v2 GCM（与二进制 ZtW/XtW、.mjs encryptTokenV2 一致） ----------
fn derive_key(oauth_key_b64: &str) -> Result<[u8; 32], String> {
    let ikm = B64
        .decode(oauth_key_b64.trim())
        .map_err(|e| format!("OAUTH_ENCRYPTION_KEY 非法 base64：{e}"))?;
    // salt 空（= Node hkdfSync 的 Buffer.alloc(0)）。HMAC 会把空 key 补零到块长，
    // 与全零 salt 等价，故 Some(&[]) 与 None 结果相同；这里显式用 Some(&[]) 对齐 Node。
    let hk = Hkdf::<Sha256>::new(Some(&[]), &ikm);
    let mut out = [0u8; 32];
    hk.expand(HKDF_INFO, &mut out)
        .map_err(|_| "hkdf expand 失败".to_string())?;
    Ok(out)
}

/// 加密：返回 `"v2:" + base64(IV ‖ 密文 ‖ tag)`。aes-gcm 把 16 字节 tag 追加在密文末尾，
/// 故 `iv ‖ (密文‖tag)` 恰为该格式。
pub fn encrypt_token_v2(plaintext: &[u8], oauth_key_b64: &str) -> Result<String, String> {
    let derived = derive_key(oauth_key_b64)?;
    let iv = rand_bytes(12).map_err(|e| e.to_string())?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&derived));
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&iv),
            Payload {
                msg: plaintext,
                aad: AAD,
            },
        )
        .map_err(|_| "aes-gcm 加密失败".to_string())?;
    let mut framed = iv;
    framed.extend_from_slice(&ct);
    Ok(format!("v2:{}", B64.encode(&framed)))
}

/// 解密 `"v2:..."`，校验 tag；失败（含篡改/密钥不符）返回 Err。
pub fn decrypt_token_v2(body: &str, oauth_key_b64: &str) -> Result<Vec<u8>, String> {
    let raw = B64
        .decode(body.strip_prefix("v2:").ok_or("缺 v2: 前缀")?)
        .map_err(|e| format!("v2 体非法 base64：{e}"))?;
    if raw.len() < 12 + 16 {
        return Err("v2 密文过短".into());
    }
    let (iv, rest) = raw.split_at(12);
    let derived = derive_key(oauth_key_b64)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&derived));
    cipher
        .decrypt(
            Nonce::from_slice(iv),
            Payload {
                msg: rest,
                aad: AAD,
            },
        )
        .map_err(|_| "aes-gcm 解密/验签失败".to_string())
}

// ---------- 路径护栏与安全写 ----------
/// 逐层向上找到最近的已存在祖先并 canonicalize（看穿符号链接），再把不存在的尾巴拼回。
fn real_ancestor(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !cur.exists() {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        match cur.parent() {
            Some(par) if par != cur => cur = par.to_path_buf(),
            _ => break,
        }
    }
    let mut base = std::fs::canonicalize(&cur).unwrap_or(cur);
    for name in tail.iter().rev() {
        base.push(name);
    }
    base
}

fn is_symlink(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn assert_not_symlink(p: &Path) -> Result<(), String> {
    if is_symlink(p) {
        return Err(format!("拒绝：{} 是符号链接，绝不跟随写入。", p.display()));
    }
    Ok(())
}

/// 安全写：拒符号链接 + O_EXCL 临时文件 + rename + chmod，避免跟随/竞态写到非预期目标。
fn safe_write(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    assert_not_symlink(path)?;
    let parent = path.parent().ok_or("目标无父目录")?;
    let suffix = hex(&rand_bytes(6).map_err(|e| e.to_string())?);
    let tmp = parent.join(format!(".tmp-{suffix}"));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true) // O_CREAT|O_EXCL
            .mode(mode)
            .open(&tmp)
            .map_err(|e| format!("建临时文件失败：{e}"))?;
        f.write_all(data)
            .map_err(|e| format!("写临时文件失败：{e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename 失败：{e}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod 失败：{e}"))?;
    Ok(())
}

fn chmod_best_effort(p: &Path, mode: u32) {
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode));
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    unsafe { libc::geteuid() }
}

fn marker_path(sandbox_root: &Path) -> Result<PathBuf, String> {
    #[cfg(test)]
    if sandbox_root.file_name().and_then(|name| name.to_str()) != Some("home") {
        return Ok(sandbox_root
            .join(".csswitch-test-state")
            .join(VIRTUAL_ORG_MARKER_FILE));
    }
    let sandbox_dir = sandbox_root
        .parent()
        .ok_or("沙箱 HOME 无父目录，无法定位 CSSwitch 私有状态目录")?;
    Ok(sandbox_dir.join("state").join(VIRTUAL_ORG_MARKER_FILE))
}

fn validate_marker_location(path: &Path, sandbox_root: &Path) -> Result<(), String> {
    let sandbox_dir = sandbox_root
        .parent()
        .ok_or("沙箱 HOME 无父目录，无法校验 CSSwitch 私有状态目录")?;
    let state_dir = path.parent().ok_or("虚拟组织标记无父目录")?;
    for (candidate, label) in [
        (sandbox_dir, "沙箱目录"),
        (sandbox_root, "沙箱 HOME"),
        (state_dir, "CSSwitch 私有状态目录"),
    ] {
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || !metadata.file_type().is_dir()
                    || metadata.uid() != current_uid()
                    || (candidate == state_dir && metadata.mode() & 0o077 != 0)
                {
                    return Err(format!("{label} 类型或所有者不安全，已拒绝使用历史标记"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查{label}失败：{error}")),
        }
    }
    Ok(())
}

fn open_marker_parent_nofollow(path: &Path) -> Result<Option<std::fs::File>, String> {
    let parent = path.parent().ok_or("虚拟组织标记无父目录")?;
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)
    {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("打开 CSSwitch 私有状态目录失败：{error}")),
    }
}

fn auth_dir_fingerprint(resolved: &Path) -> String {
    let canonical = std::fs::canonicalize(resolved).unwrap_or_else(|_| real_ancestor(resolved));
    let mut digest = Sha256::new();
    digest.update(canonical.as_os_str().as_encoded_bytes());
    hex(&digest.finalize())
}

fn ensure_marker_parent(path: &Path, sandbox_root: &Path) -> Result<(), String> {
    let sandbox_dir = sandbox_root
        .parent()
        .ok_or("沙箱 HOME 无父目录，无法创建 CSSwitch 私有状态目录")?;
    assert_not_symlink(sandbox_dir)?;
    assert_not_symlink(sandbox_root)?;
    let parent = path.parent().ok_or("虚拟组织标记无父目录")?;
    assert_not_symlink(parent)?;
    std::fs::create_dir_all(parent).map_err(|e| format!("创建 CSSwitch 私有状态目录失败：{e}"))?;
    let metadata = std::fs::symlink_metadata(parent)
        .map_err(|e| format!("检查 CSSwitch 私有状态目录失败：{e}"))?;
    if !metadata.is_dir() || metadata.uid() != current_uid() {
        return Err("拒绝使用非当前用户所有的 CSSwitch 私有状态目录".into());
    }
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("收紧 CSSwitch 私有状态目录权限失败：{e}"))?;
    validate_marker_location(path, sandbox_root)?;
    Ok(())
}

fn read_marker(resolved: &Path, sandbox_root: &Path) -> Result<MarkerRead, String> {
    let path = marker_path(sandbox_root)?;
    validate_marker_location(&path, sandbox_root)?;
    let Some(parent) = open_marker_parent_nofollow(&path)? else {
        return Ok(MarkerRead::Missing);
    };
    let name = std::ffi::CString::new(VIRTUAL_ORG_MARKER_FILE).expect("static filename");
    // SAFETY: parent is an owned directory fd; name is NUL-terminated; flags
    // request a read-only, non-following descriptor.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(MarkerRead::Missing);
        }
        return Err(format!("读取 CSSwitch 历史标记失败：{error}"));
    }
    // SAFETY: fd was returned uniquely by openat and is transferred to File.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .map_err(|e| format!("检查 CSSwitch 历史标记失败：{e}"))?;
    if !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.len() > VIRTUAL_ORG_MARKER_MAX_BYTES
        || metadata.mode() & 0o077 != 0
    {
        return Err("CSSwitch 历史标记类型、所有者、大小或权限不安全，已拒绝使用".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("读取 CSSwitch 历史标记失败：{e}"))?;
    if bytes.len() as u64 > VIRTUAL_ORG_MARKER_MAX_BYTES {
        return Err("CSSwitch 历史标记超过大小限制，已拒绝使用".into());
    }
    let marker: VirtualOrgMarker =
        serde_json::from_slice(&bytes).map_err(|_| "CSSwitch 历史标记不是合法 JSON")?;
    if marker.schema_version != 1
        || !looks_like_uuid(&marker.org_uuid)
        || marker.auth_dir_fingerprint != auth_dir_fingerprint(resolved)
    {
        return Err("CSSwitch 历史标记与当前隔离目录不匹配，已拒绝自动恢复".into());
    }
    Ok(MarkerRead::Valid(marker))
}

/// Upgrade bridge for an already-running, intact v0.8.0 virtual login. This
/// writes only CSSwitch-owned marker state; Science credentials stay read-only.
pub(crate) fn bootstrap_marker_for_intact_login(
    auth_dir: &Path,
    email: &str,
    sandbox_root: &Path,
) -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无 HOME 环境变量")?;
    let resolved = resolve_guarded(auth_dir, email, sandbox_root, &home.join(".claude-science"))?;
    let intact = read_intact_login(&resolved, email).ok_or("隔离虚拟登录不完整，不能补历史标记")?;
    match read_marker(&resolved, sandbox_root)? {
        MarkerRead::Missing => write_marker(&resolved, sandbox_root, &intact.org_uuid),
        MarkerRead::Valid(marker) if marker.org_uuid == intact.org_uuid => Ok(()),
        MarkerRead::Valid(_) => Err("CSSwitch 历史标记与当前完整登录不一致".into()),
    }
}

fn write_marker(resolved: &Path, sandbox_root: &Path, org_uuid: &str) -> Result<(), String> {
    if !looks_like_uuid(org_uuid) {
        return Err("拒绝写入非法历史组织标记".into());
    }
    let path = marker_path(sandbox_root)?;
    ensure_marker_parent(&path, sandbox_root)?;
    let marker = VirtualOrgMarker {
        schema_version: 1,
        auth_dir_fingerprint: auth_dir_fingerprint(resolved),
        org_uuid: org_uuid.to_string(),
    };
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|e| format!("序列化 CSSwitch 历史标记失败：{e}"))?;
    safe_write(&path, &bytes, 0o600)
}

// ---------- 主流程 ----------
// 注：一次性 `forge`（总是铸新 org）已被幂等的 `ensure_virtual_login` 取代（修 #3/#6），
// 应用不再需要它。`forge_guarded`（= 护栏 + 一次性铸新）仍保留：既是 `ensure_virtual_login`
// 的「铸新/修复」写入路径基石，也供测试用注入假 real_cred_dir 直接验证护栏与写入。
/// 可注入「真实凭证目录」的内层（供测试传入临时目录，不碰真实 `~/.claude-science`）。
/// 一次性铸新（org/account 均新）。=`resolve_guarded` 护栏 + `write_login(None,None)`。
/// 现仅测试使用（应用走幂等 `ensure_virtual_login`）→ `#[cfg(test)]` 避免非 test 构建 dead_code。
#[cfg(test)]
fn forge_guarded(
    auth_dir: &Path,
    email: &str,
    sandbox_root: &Path,
    real_cred_dir: &Path,
) -> Result<ForgeResult, String> {
    let resolved = resolve_guarded(auth_dir, email, sandbox_root, real_cred_dir)?;
    let forged = write_login(&resolved, email, None, None)?;
    write_marker(&resolved, sandbox_root, &forged.org_uuid)?;
    Ok(forged)
}

/// 护栏（写任何东西之前；`real_ancestor` 已看穿符号链接）：真实目录保护（护栏 0，铁律最高
/// 优先）→ 沙箱根内（护栏 1）→ 假账号 email。全过则返回解析后的写入根 `resolved`。
fn resolve_guarded(
    auth_dir: &Path,
    email: &str,
    sandbox_root: &Path,
    real_cred_dir: &Path,
) -> Result<PathBuf, String> {
    let resolved = real_ancestor(auth_dir);
    // 载重护栏 0（铁律 1，最高优先，先于沙箱根检查）：解析后的写入根绝不落在真实
    // ~/.claude-science 之内或其本身。防「把 ~/.csswitch/sandbox（或其祖先）预置成指向
    // 真实目录的符号链接」——此时 sandbox_root 也会解析进真实树，令下方『沙箱根内』检查失效
    // （resolved 与 root 同处真实树内而放行）。这条不依赖沙箱根，是对真实目录的绝对保护：
    // 任何异常布局都绝不触碰真实目录。
    let real_root = real_ancestor(real_cred_dir);
    if resolved.starts_with(&real_root) {
        return Err(format!(
            "拒绝：auth_dir 解析到真实 Science 目录（{}）之内或本身，铁律禁止触碰。",
            real_root.display()
        ));
    }
    // 载重护栏 1（修 P1）：resolved 必须落在沙箱根之下。预置符号链接把 auth_dir 或其祖先
    // 链到沙箱外任意目录会让 canonicalize 解引用到别处；把写重定向挡在写任何文件之前
    // （Node 版的沙箱范围限制在此以「根内」形式恢复）。
    let root = real_ancestor(sandbox_root);
    if !resolved.starts_with(&root) {
        return Err(format!(
            "拒绝：auth_dir 解析到沙箱根之外（{} 不在 {} 下），疑似符号链接重定向。",
            resolved.display(),
            root.display()
        ));
    }
    if !email.ends_with("localhost.invalid") {
        return Err(format!(
            "拒绝：email 必须以 localhost.invalid 结尾（当前 {email}），确保是假账号。"
        ));
    }
    Ok(resolved)
}

/// 在已通过护栏的 `resolved` 写一套虚拟登录。`prefer_org`/`prefer_account` 为 Some 则复用
/// （修复时保住 org，令旧对话 DB 仍挂得上），为 None 则新铸。写入逻辑与旧 forge 一致。
fn write_login(
    resolved: &Path,
    email: &str,
    prefer_org: Option<String>,
    prefer_account: Option<String>,
) -> Result<ForgeResult, String> {
    std::fs::create_dir_all(resolved).map_err(|e| format!("建 auth_dir 失败：{e}"))?;
    chmod_best_effort(resolved, 0o700);

    // —— encryption.key：复用已存在的（保持旧 .enc 可解），否则新造 ——
    let key_file = resolved.join("encryption.key");
    assert_not_symlink(&key_file)?;
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    if key_file.exists() {
        let txt = std::fs::read_to_string(&key_file)
            .map_err(|e| format!("读 encryption.key 失败：{e}"))?;
        for line in txt.lines() {
            if let Some(eq) = line.find('=') {
                if eq > 0 {
                    let v = line[eq + 1..].trim();
                    if !v.is_empty() {
                        keys.insert(line[..eq].trim().to_string(), v.to_string());
                    }
                }
            }
        }
    }
    // P2a：复用的 OAUTH_ENCRYPTION_KEY 必须能 base64 解出 ≥16 字节，否则丢弃 → 下面 fill 循环
    // 重造。免得「present 但非法 base64」的 key 被留用 → 后续 encrypt_token_v2 里 derive_key
    // 直接报错而非自愈。只校验这一把（我们加密 .enc 用它）；另三把 Science 内部用，present 则留。
    let oauth_usable = keys
        .get("OAUTH_ENCRYPTION_KEY")
        .map(|v| B64.decode(v.trim()).map(|b| b.len() >= 16).unwrap_or(false))
        .unwrap_or(false);
    if !oauth_usable {
        keys.remove("OAUTH_ENCRYPTION_KEY");
    }
    for k in KEY_NAMES {
        if !keys.contains_key(k) {
            keys.insert(k.to_string(), b64_32().map_err(|e| e.to_string())?);
        }
    }
    let key_blob = KEY_NAMES
        .iter()
        .map(|k| format!("{k}={}", keys[*k]))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    safe_write(&key_file, key_blob.as_bytes(), 0o600)?;

    // —— 令牌 blob（字段对齐 _adapt / _tryOauthToken）：org/account 有偏好则复用，否则新铸 ——
    let account_uuid = match prefer_account {
        Some(a) => a,
        None => uuid_v4().map_err(|e| e.to_string())?,
    };
    let org_uuid = match prefer_org {
        Some(o) => o,
        None => uuid_v4().map_err(|e| e.to_string())?,
    };
    let access = format!(
        "sk-ant-virtual-{}",
        hex(&rand_bytes(24).map_err(|e| e.to_string())?)
    );
    let blob = json!({
        "access_token": access,          // 代理会剥离，值任意
        "refresh_token": "",
        "api_key": null,
        "token_expires_at": "2099-01-01T00:00:00.000Z", // 远期 → 绝不联网刷新
        "provider": "claude_ai",
        "scopes": "user:inference user:file_upload user:profile user:mcp_servers user:plugins",
        "email": email,
        "account_uuid": account_uuid.clone(),
        "subscription_type": "max",
        "rate_limit_tier": null,
        "seat_tier": null,
        "org_uuid": org_uuid.clone(),
        "billing_type": null,
        "has_extra_usage_enabled": false
    });
    let plaintext = serde_json::to_vec(&blob).map_err(|e| format!("序列化 blob 失败：{e}"))?;
    let oauth_key = keys
        .get("OAUTH_ENCRYPTION_KEY")
        .ok_or("缺 OAUTH_ENCRYPTION_KEY")?;
    let enc_body = encrypt_token_v2(&plaintext, oauth_key)?;

    // —— 写 .oauth-tokens/<sanitized>.enc；先清其它 .enc 保证唯一 ——
    let tok_dir = resolved.join(".oauth-tokens");
    assert_not_symlink(&tok_dir)?;
    std::fs::create_dir_all(&tok_dir).map_err(|e| format!("建 .oauth-tokens 失败：{e}"))?;
    chmod_best_effort(&tok_dir, 0o700);
    if let Ok(rd) = std::fs::read_dir(&tok_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "enc").unwrap_or(false) {
                assert_not_symlink(&p)?;
                // 删除失败必须显式失败（修 P2）：否则残留旧 .enc + 新 .enc = 多个，
                // 而 Science 预期目录内恰好一个 → 会「显示启动成功却仍登录不上」。
                std::fs::remove_file(&p).map_err(|err| {
                    format!(
                        "删除旧令牌 {} 失败：{err}（需目录内恰好一个 .enc）",
                        p.display()
                    )
                })?;
            }
        }
    }
    let user_id: String = account_uuid
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    let enc_file = tok_dir.join(format!("{user_id}.enc"));
    safe_write(&enc_file, enc_body.as_bytes(), 0o600)?;

    // —— 自校验：用同样逻辑解密回读，确保 Science 能解开 ——
    let roundtrip = decrypt_token_v2(&enc_body, oauth_key)?;
    let rt: serde_json::Value =
        serde_json::from_slice(&roundtrip).map_err(|e| format!("自校验解析失败：{e}"))?;
    if rt.get("email").and_then(|v| v.as_str()) != Some(email) {
        return Err("自校验失败：解密回读的 email 不符".into());
    }

    // Persist the sandbox organization identifier.
    let org_json = serde_json::to_string_pretty(&json!({ "org_uuid": org_uuid })).unwrap() + "\n";
    safe_write(
        &resolved.join("active-org.json"),
        org_json.as_bytes(),
        0o600,
    )?;

    Ok(ForgeResult {
        auth_dir: resolved.to_path_buf(),
        account_uuid,
        org_uuid,
        enc_file,
    })
}

// ---------- 幂等：现有登录读取/校验 ----------
/// 严格校验：s 形如 8-4-4-4-12 的十六进制 UUID。
fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// 解析 encryption.key 拿 OAUTH_ENCRYPTION_KEY（非空才算）。
fn parse_oauth_key(resolved: &Path) -> Option<String> {
    let txt = std::fs::read_to_string(resolved.join("encryption.key")).ok()?;
    for line in txt.lines() {
        if let Some(v) = line.strip_prefix("OAUTH_ENCRYPTION_KEY=") {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// `.oauth-tokens/` 下恰好一个 `.enc` 才返回其路径；零个或多于一个都返回 None。
fn single_enc(resolved: &Path) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    for e in std::fs::read_dir(resolved.join(".oauth-tokens"))
        .ok()?
        .flatten()
    {
        let p = e.path();
        if p.extension().map(|x| x == "enc").unwrap_or(false) {
            if found.is_some() {
                return None;
            }
            found = Some(p);
        }
    }
    found
}

/// active-org.json 里合法 UUID 的 org_uuid。
fn read_active_org(resolved: &Path) -> Option<String> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(resolved.join("active-org.json")).ok()?)
            .ok()?;
    let o = v.get("org_uuid")?.as_str()?;
    if looks_like_uuid(o) {
        Some(o.to_string())
    } else {
        None
    }
}

/// Scan owned, real directories only. A symlink named like an organization is
/// never a recovery candidate, even when its target is a directory.
fn scan_org_dirs(resolved: &Path) -> Vec<HistoryOrgCandidate> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(resolved.join("orgs")) {
        for e in rd.flatten() {
            let Ok(metadata) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if !metadata.file_type().is_dir() || metadata.uid() != current_uid() {
                continue;
            }
            if let Some(name) = e.file_name().to_str() {
                if looks_like_uuid(name) {
                    v.push(HistoryOrgCandidate {
                        org_uuid: name.to_string(),
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    });
                }
            }
        }
    }
    v.sort_by(|left, right| left.org_uuid.cmp(&right.org_uuid));
    v
}

fn candidate_is_current(resolved: &Path, candidate: &HistoryOrgCandidate) -> bool {
    let path = resolved.join("orgs").join(&candidate.org_uuid);
    std::fs::symlink_metadata(path)
        .map(|metadata| {
            metadata.file_type().is_dir()
                && metadata.uid() == current_uid()
                && metadata.dev() == candidate.device
                && metadata.ino() == candidate.inode
        })
        .unwrap_or(false)
}

/// 今天的 UTC 日期 `YYYY-MM-DD`（无外部 crate；Howard Hinnant civil-from-days）。
fn today_utc_ymd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64; // 自 1970-01-01 的天数
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// `token_expires_at`（ISO8601）的日期部分是否 ≥ 今天（UTC），即尚未过期（P2）。
/// 取前 10 字符 `YYYY-MM-DD` 与今天按字典序比较（ISO8601 日期字典序即时间序）；
/// 格式不对（长度不足 / 非 `dddd-dd-dd`）视为过期。粒度到「天」足够区分远期 vs 过去。
fn token_not_expired(expires_at: &str) -> bool {
    if expires_at.len() < 10 {
        return false;
    }
    let date = &expires_at[..10];
    let b = date.as_bytes();
    let shaped = b.iter().enumerate().all(|(i, &c)| match i {
        4 | 7 => c == b'-',
        _ => c.is_ascii_digit(),
    });
    shaped && date >= today_utc_ymd().as_str()
}

/// 现有登录是否「完整且自洽」；是则返回其身份（用于原样复用）。任何一步不满足返回 None
/// （→ 降级到修复，安全方向）。校验从严（P2）：读路径不得是符号链接；解密后 org 一致、
/// email 是假账号、`account_uuid` 是合法 UUID、`provider`=claude_ai、`access_token` 非空、
/// `token_expires_at` 未过期。
fn read_intact_login(resolved: &Path, email: &str) -> Option<ForgeResult> {
    // 读路径不得是符号链接（不跟随；可疑布局 → 视作不自洽走修复，修复端有 assert_not_symlink）。
    if is_symlink(&resolved.join("encryption.key"))
        || is_symlink(&resolved.join(".oauth-tokens"))
        || is_symlink(&resolved.join("active-org.json"))
    {
        return None;
    }
    let key = parse_oauth_key(resolved)?;
    let enc = single_enc(resolved)?;
    if is_symlink(&enc) {
        return None;
    }
    let active_org = read_active_org(resolved)?;
    let body = std::fs::read_to_string(&enc).ok()?;
    let blob: serde_json::Value =
        serde_json::from_slice(&decrypt_token_v2(&body, &key).ok()?).ok()?;
    let blob_org = blob.get("org_uuid")?.as_str()?;
    let blob_email = blob.get("email")?.as_str()?;
    let account = blob.get("account_uuid")?.as_str()?;
    let provider_ok = blob.get("provider").and_then(|v| v.as_str()) == Some("claude_ai");
    let access_ok = blob
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let expiry_ok = blob
        .get("token_expires_at")
        .and_then(|v| v.as_str())
        .map(token_not_expired)
        .unwrap_or(false);
    if blob_org != active_org
        || blob_email != email
        || !blob_email.ends_with("localhost.invalid")
        || !looks_like_uuid(account)
        || !provider_ok
        || !access_ok
        || !expiry_ok
    {
        return None;
    }
    Some(ForgeResult {
        auth_dir: resolved.to_path_buf(),
        account_uuid: account.to_string(),
        org_uuid: active_org,
        enc_file: enc,
    })
}

/// 幂等虚拟登录：完整自洽→复用；部分损坏→修复但保 org；真首次→铸新。
pub fn ensure_virtual_login(
    auth_dir: &Path,
    email: &str,
    sandbox_root: &Path,
) -> Result<(ForgeResult, LoginAction), EnsureVirtualLoginError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无 HOME 环境变量")?;
    ensure_virtual_login_guarded(auth_dir, email, sandbox_root, &home.join(".claude-science"))
}

/// 可注入「真实凭证目录」的内层（测试用）。
fn ensure_virtual_login_guarded(
    auth_dir: &Path,
    email: &str,
    sandbox_root: &Path,
    real_cred_dir: &Path,
) -> Result<(ForgeResult, LoginAction), EnsureVirtualLoginError> {
    let resolved = resolve_guarded(auth_dir, email, sandbox_root, real_cred_dir)?;
    let marker = read_marker(&resolved, sandbox_root)?;
    // 完整自洽 → 凭证原样复用。0.8.0 升级时只在 CSSwitch 私有状态目录补标记，
    // 不重写 Science 凭证；以后 Sign out 才能可靠找回同一份历史。
    if let Some(fr) = read_intact_login(&resolved, email) {
        match marker {
            MarkerRead::Missing => write_marker(&resolved, sandbox_root, &fr.org_uuid)?,
            MarkerRead::Valid(existing) if existing.org_uuid == fr.org_uuid => {}
            MarkerRead::Valid(_) => {
                let candidates = scan_org_dirs(&resolved);
                if candidates.len() > 1 {
                    return Err(EnsureVirtualLoginError::HistoryChoiceRequired(candidates));
                }
                return Err("CSSwitch 历史标记与当前完整登录不一致，已拒绝静默覆盖".into());
            }
        }
        return Ok((fr, LoginAction::Reused));
    }
    let candidates = scan_org_dirs(&resolved);
    let (prior_org, action) = match marker {
        MarkerRead::Valid(existing) => match candidates
            .iter()
            .find(|candidate| candidate.org_uuid == existing.org_uuid)
        {
            Some(candidate) => (Some(candidate.org_uuid.clone()), LoginAction::Repaired),
            // A marker is written before the first Science launch creates its
            // org directory. If no history directory exists yet, preserving
            // that marker org is safer than silently minting another one.
            None if candidates.is_empty() => {
                (Some(existing.org_uuid.clone()), LoginAction::Repaired)
            }
            None if candidates.len() > 1 => {
                return Err(EnsureVirtualLoginError::HistoryChoiceRequired(candidates));
            }
            None => {
                return Err(
                    "上一次 CSSwitch 历史标记对应的目录已不存在，已拒绝静默改用另一份历史".into(),
                )
            }
        },
        MarkerRead::Missing => match candidates.len() {
            0 => (None, LoginAction::Created),
            1 => (Some(candidates[0].org_uuid.clone()), LoginAction::Repaired),
            _ => return Err(EnsureVirtualLoginError::HistoryChoiceRequired(candidates)),
        },
    };
    // Never borrow account/org identity from a decryptable token. The isolated
    // data-dir may also contain a prior official login; only the CSSwitch marker
    // or a verified history-directory choice may select an organization.
    let fr = write_login(&resolved, email, prior_org, None)?;
    write_marker(&resolved, sandbox_root, &fr.org_uuid)?;
    Ok((fr, action))
}

/// Complete an explicit legacy-history choice. The candidate is a backend-only
/// inode snapshot created by `ensure_virtual_login`; it is revalidated before
/// any credential file is written.
pub(crate) fn restore_history_choice(
    auth_dir: &Path,
    email: &str,
    sandbox_root: &Path,
    candidate: &HistoryOrgCandidate,
) -> Result<(ForgeResult, LoginAction), String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("无 HOME 环境变量")?;
    let resolved = resolve_guarded(auth_dir, email, sandbox_root, &home.join(".claude-science"))?;
    if !candidate_is_current(&resolved, candidate) {
        return Err("历史记录候选已变化，本次选择已作废；请重新点击一键开始".into());
    }
    let forged = write_login(&resolved, email, Some(candidate.org_uuid.clone()), None)?;
    write_marker(&resolved, sandbox_root, &forged.org_uuid)?;
    Ok((forged, LoginAction::Repaired))
}

/// 只读判定：沙箱里的虚拟登录当前是否「完整自洽」（可直接复用）。**绝不写任何文件**
/// （operon 可能正在读）。供一键健康快捷路径判断：`8990` daemon 活着 ≠ 登录态可用；若登录已
/// 失效（旧版遗留 / 凭证损坏 / 已落登录页），重开也只会再落登录页，应改走「停沙箱 → 修复保
/// org → 重启」。这样 0.2.0 的健康快捷路径不再把「健康但登录失效」当成可用（修 0.2.1 Bug2）。
pub fn login_intact(auth_dir: &Path, email: &str, sandbox_root: &Path) -> bool {
    match std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude-science")) {
        Some(real) => login_intact_guarded(auth_dir, email, sandbox_root, &real),
        None => false,
    }
}

/// 可注入「真实凭证目录」的内层（测试用）。护栏失败（异常布局 / 落入真实树）视作不自洽（false），
/// 促使上层走修复路径；修复路径自身仍有护栏，绝不触碰真实目录。只读，绝不写。
fn login_intact_guarded(
    auth_dir: &Path,
    email: &str,
    sandbox_root: &Path,
    real_cred_dir: &Path,
) -> bool {
    match resolve_guarded(auth_dir, email, sandbox_root, real_cred_dir) {
        Ok(resolved) => read_intact_login(&resolved, email).is_some(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::OnceLock;

    static CTR: AtomicU32 = AtomicU32::new(0);
    static TEST_BASE: OnceLock<PathBuf> = OnceLock::new();

    fn test_base() -> &'static Path {
        TEST_BASE.get_or_init(|| loop {
            let nonce = hex(&rand_bytes(8).unwrap());
            let base = std::env::temp_dir().join(format!(
                "csswitch-forge-tests-{}-{nonce}",
                std::process::id()
            ));
            match std::fs::create_dir(&base) {
                Ok(()) => {
                    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                    return base;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create forge test root: {error}"),
            }
        })
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        // Keep the test sandbox below a process-owned parent. On Linux the
        // system temp directory is normally owned by root, while production
        // sandboxes live below the current user's ~/.csswitch directory.
        let d = test_base().join(format!("{tag}-{n}"));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn read_oauth_key(auth_dir: &Path) -> String {
        let txt = std::fs::read_to_string(auth_dir.join("encryption.key")).unwrap();
        for line in txt.lines() {
            if let Some(v) = line.strip_prefix("OAUTH_ENCRYPTION_KEY=") {
                return v.trim().to_string();
            }
        }
        panic!("no OAUTH_ENCRYPTION_KEY");
    }

    fn the_enc_file(auth_dir: &Path) -> PathBuf {
        let tok = auth_dir.join(".oauth-tokens");
        let mut encs: Vec<PathBuf> = std::fs::read_dir(&tok)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "enc").unwrap_or(false))
            .collect();
        assert_eq!(encs.len(), 1, "应恰好一个 .enc");
        encs.pop().unwrap()
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = b64_32().unwrap();
        let pt = br#"{"email":"virtual@localhost.invalid","x":1}"#;
        let body = encrypt_token_v2(pt, &key).unwrap();
        assert!(body.starts_with("v2:"));
        let back = decrypt_token_v2(&body, &key).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn decrypt_fails_on_wrong_key() {
        let k1 = b64_32().unwrap();
        let k2 = b64_32().unwrap();
        let body = encrypt_token_v2(b"hello", &k1).unwrap();
        assert!(decrypt_token_v2(&body, &k2).is_err(), "错 key 应验签失败");
    }

    #[test]
    fn forge_writes_files_and_selfchecks() {
        let dir = tmpdir("ok");
        let fake_real = tmpdir("realcred"); // 与 auth_dir 不同，护栏放行
        let email = "virtual@localhost.invalid";
        let r = forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert!(r.enc_file.is_file());
        assert!(dir.join("encryption.key").is_file());
        assert!(dir.join("active-org.json").is_file());
        // 解密回读一致
        let key = read_oauth_key(&dir);
        let body = std::fs::read_to_string(the_enc_file(&dir)).unwrap();
        let blob: serde_json::Value =
            serde_json::from_slice(&decrypt_token_v2(&body, &key).unwrap()).unwrap();
        assert_eq!(blob["email"], email);
        assert_eq!(blob["provider"], "claude_ai");
        // active-org.json 的 org_uuid 与摘要一致
        let org: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("active-org.json")).unwrap())
                .unwrap();
        assert_eq!(org["org_uuid"], r.org_uuid);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fake_real);
    }

    #[test]
    fn forge_rejects_real_cred_dir() {
        // auth_dir 指向「真实凭证目录」（这里用临时目录扮演）→ 必须拒、且不写。
        let real = tmpdir("real2");
        let r = forge_guarded(&real, "virtual@localhost.invalid", &real, &real);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("真实 Science 目录"));
        assert!(
            !real.join("encryption.key").exists(),
            "拒绝路径不应写任何文件"
        );
    }

    #[test]
    fn forge_rejects_symlink_into_real_science_tree() {
        // 铁律回归：把沙箱根的祖先预置成指向【真实 Science 目录】的符号链接——此时沙箱根
        // 自身也解析进真实树，仅靠护栏 1「沙箱根内」会放行（resolved 与 root 同在真实树内）。
        // 护栏 0 必须在写任何文件之前拒绝，且真实目录零改动。
        let real = tmpdir("real-science");
        std::fs::create_dir_all(real.join(".oauth-tokens")).unwrap();
        std::fs::write(real.join(".oauth-tokens/victim.enc"), b"keep-me").unwrap();
        std::fs::write(real.join("encryption.key"), b"KEEP=me\n").unwrap();

        let csw = tmpdir("csw");
        std::fs::create_dir_all(&csw).unwrap();
        // ~/.csswitch/sandbox -> 真实 Science 目录（预置的恶意/异常软链接）
        let sandbox_link = csw.join("sandbox");
        std::os::unix::fs::symlink(&real, &sandbox_link).unwrap();

        let sandbox_root = sandbox_link.join("home");
        let auth_dir = sandbox_root.join(".claude-science");
        let r = forge_guarded(&auth_dir, "virtual@localhost.invalid", &sandbox_root, &real);
        assert!(r.is_err(), "沙箱根经符号链接落入真实树必须被拒");
        assert!(r.unwrap_err().contains("真实 Science 目录"));
        // 真实目录零改动。
        assert_eq!(
            std::fs::read(real.join("encryption.key")).unwrap(),
            b"KEEP=me\n"
        );
        assert!(
            real.join(".oauth-tokens/victim.enc").exists(),
            "真实 .enc 不该被碰"
        );
        assert!(!real.join("home").exists(), "不该在真实树里建任何目录");
        for d in [real, csw] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn forge_rejects_symlink_escaping_sandbox_root() {
        // P1 回归：把沙箱内的 auth_dir 预置成指向沙箱外目录的符号链接，伪造器必须
        // 在写任何文件之前拒绝，且绝不碰链接目标。
        let root = tmpdir("sbroot");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tmpdir("outside");
        std::fs::create_dir_all(&outside).unwrap();
        // 预置目标里一个「不该被删」的旧 .enc 与一个「不该被覆盖」的 key 文件。
        std::fs::create_dir_all(outside.join(".oauth-tokens")).unwrap();
        std::fs::write(outside.join(".oauth-tokens/victim.enc"), b"keep-me").unwrap();
        std::fs::write(outside.join("encryption.key"), b"KEEP=me\n").unwrap();

        let auth_dir = root.join(".claude-science");
        std::os::unix::fs::symlink(&outside, &auth_dir).unwrap(); // auth_dir -> outside

        let fake_real = tmpdir("realcred5");
        let r = forge_guarded(&auth_dir, "virtual@localhost.invalid", &root, &fake_real);
        assert!(r.is_err(), "符号链接逃出沙箱根应被拒");
        assert!(r.unwrap_err().contains("沙箱根之外"));
        // 目标目录零改动。
        assert_eq!(
            std::fs::read(outside.join("encryption.key")).unwrap(),
            b"KEEP=me\n"
        );
        assert!(
            outside.join(".oauth-tokens/victim.enc").exists(),
            "旧 .enc 不该被删"
        );
        assert!(!outside.join("active-org.json").exists(), "不该写入目标");
        for d in [root, outside, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn forge_rejects_non_localhost_email() {
        let dir = tmpdir("email");
        let fake_real = tmpdir("realcred3");
        let r = forge_guarded(&dir, "attacker@example.com", &dir, &fake_real);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("localhost.invalid"));
    }

    // ---------- 幂等 ensure_virtual_login ----------
    fn read_active_org_uuid(auth_dir: &Path) -> String {
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(auth_dir.join("active-org.json")).unwrap(),
        )
        .unwrap();
        v["org_uuid"].as_str().unwrap().to_string()
    }

    #[test]
    fn ensure_reuses_intact_login() {
        let dir = tmpdir("reuse");
        let fake_real = tmpdir("realcred-reuse");
        let email = "virtual@localhost.invalid";
        let first = forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        let enc0 = std::fs::read(the_enc_file(&dir)).unwrap();
        let key0 = std::fs::read(dir.join("encryption.key")).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Reused);
        assert_eq!(r.org_uuid, first.org_uuid, "org 不变");
        assert_eq!(r.org_uuid, org0);
        assert_eq!(
            std::fs::read(the_enc_file(&dir)).unwrap(),
            enc0,
            ".enc 字节不变"
        );
        assert_eq!(
            std::fs::read(dir.join("encryption.key")).unwrap(),
            key0,
            "key 字节不变"
        );
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_still_rejects_real_cred_dir() {
        let real = tmpdir("real-ensure");
        let r = ensure_virtual_login_guarded(&real, "virtual@localhost.invalid", &real, &real);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("真实 Science 目录"));
        assert!(
            !real.join("encryption.key").exists(),
            "拒绝路径不应写任何文件"
        );
        let _ = std::fs::remove_dir_all(&real);
    }

    #[test]
    fn ensure_repairs_missing_enc_keeps_org() {
        let dir = tmpdir("rep-missing");
        let fake_real = tmpdir("realcred-rm");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        std::fs::remove_file(the_enc_file(&dir)).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired);
        assert_eq!(r.org_uuid, org0, "修复必须沿用原 org");
        assert_eq!(read_active_org_uuid(&dir), org0);
        let key = read_oauth_key(&dir);
        let body = std::fs::read_to_string(the_enc_file(&dir)).unwrap();
        assert!(decrypt_token_v2(&body, &key).is_ok());
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_repairs_extra_enc_keeps_org() {
        let dir = tmpdir("rep-extra");
        let fake_real = tmpdir("realcred-re");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        std::fs::write(dir.join(".oauth-tokens/stale.enc"), b"v2:garbage").unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired);
        assert_eq!(r.org_uuid, org0);
        let _ = the_enc_file(&dir); // 内部断言恰好一个 .enc
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_repairs_replaced_key_keeps_org() {
        let dir = tmpdir("rep-key");
        let fake_real = tmpdir("realcred-rk");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        let mut blob = String::new();
        for k in KEY_NAMES {
            blob.push_str(&format!("{k}={}\n", b64_32().unwrap()));
        }
        std::fs::write(dir.join("encryption.key"), blob).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(
            action,
            LoginAction::Repaired,
            "active-org.json 仍在 → 修复而非铸新"
        );
        assert_eq!(r.org_uuid, org0, "换 key 也不换 org");
        let key = read_oauth_key(&dir);
        let body = std::fs::read_to_string(the_enc_file(&dir)).unwrap();
        assert!(decrypt_token_v2(&body, &key).is_ok(), "重铸令牌应可解");
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_creates_on_first_run() {
        let dir = tmpdir("create");
        let fake_real = tmpdir("realcred-cr");
        let email = "virtual@localhost.invalid";
        let (r, action) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Created);
        assert!(r.enc_file.is_file());
        assert!(looks_like_uuid(&r.org_uuid));
        assert_eq!(read_active_org_uuid(&dir), r.org_uuid);
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_recovers_org_from_marker_when_active_org_missing() {
        // active-org.json 丢了，但 CSSwitch 私有标记仍在 → 取回原 org，绝不从 token 猜。
        let dir = tmpdir("tok-org");
        let fake_real = tmpdir("realcred-to");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        std::fs::remove_file(dir.join("active-org.json")).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired);
        assert_eq!(r.org_uuid, org0, "应从私有标记取回原 org");
        assert_eq!(
            read_active_org_uuid(&dir),
            org0,
            "active-org.json 应写回同 org"
        );
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_adopts_single_org_dir_when_active_and_token_gone() {
        // P1a：active-org.json 和 .enc 都没了，但 orgs/ 下恰好一个历史 org → 采用它。
        let dir = tmpdir("one-orgdir");
        let fake_real = tmpdir("realcred-1o");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        std::fs::create_dir_all(dir.join("orgs").join(&org0)).unwrap();
        std::fs::remove_file(the_enc_file(&dir)).unwrap();
        std::fs::remove_file(dir.join("active-org.json")).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired);
        assert_eq!(r.org_uuid, org0, "应采用唯一历史 org 目录");
        assert_eq!(read_active_org_uuid(&dir), org0);
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_errors_on_ambiguous_multi_org() {
        // P1a：无 active-org、无可解 token、orgs/ 下多个历史组织 → 报错中止，绝不静默新铸。
        let dir = tmpdir("multi-org");
        let fake_real = tmpdir("realcred-mo");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let a = uuid_v4().unwrap();
        let b = uuid_v4().unwrap();
        std::fs::create_dir_all(dir.join("orgs").join(&a)).unwrap();
        std::fs::create_dir_all(dir.join("orgs").join(&b)).unwrap();
        std::fs::remove_file(the_enc_file(&dir)).unwrap();
        std::fs::remove_file(dir.join("active-org.json")).unwrap();
        let r = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real);
        assert!(r.is_err(), "多历史组织无法定位活动者应报错");
        assert!(matches!(
            r.unwrap_err(),
            EnsureVirtualLoginError::HistoryChoiceRequired(ref candidates) if candidates.len() == 2
        ));
        assert!(
            !dir.join("active-org.json").exists(),
            "报错不应写 active-org.json"
        );
        assert_eq!(scan_org_dirs(&dir).len(), 2, "不应静默新铸 org");
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn marker_recovers_csswitch_history_after_signout_with_multiple_orgs() {
        let dir = tmpdir("marker-signout");
        let fake_real = tmpdir("realcred-marker-signout");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let csswitch_org = read_active_org_uuid(&dir);
        let other_org = uuid_v4().unwrap();
        std::fs::create_dir_all(dir.join("orgs").join(&csswitch_org)).unwrap();
        std::fs::create_dir_all(dir.join("orgs").join(&other_org)).unwrap();
        std::fs::remove_file(the_enc_file(&dir)).unwrap();
        std::fs::remove_file(dir.join("active-org.json")).unwrap();

        let (restored, action) =
            ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired);
        assert_eq!(restored.org_uuid, csswitch_org);
        assert!(dir.join("orgs").join(other_org).is_dir());
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn production_marker_path_rejects_symlinked_state_parent() {
        let base = tmpdir("marker-parent-link");
        let sandbox_root = base.join("sandbox/home");
        let auth_dir = sandbox_root.join(".claude-science");
        let outside = base.join("outside-state");
        let fake_real = base.join("real-science");
        std::fs::create_dir_all(&sandbox_root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("sandbox/state")).unwrap();

        let error = ensure_virtual_login_guarded(
            &auth_dir,
            "virtual@localhost.invalid",
            &sandbox_root,
            &fake_real,
        )
        .unwrap_err();
        assert!(error.to_string().contains("私有状态目录"));
        assert!(!outside.join(VIRTUAL_ORG_MARKER_FILE).exists());
        assert!(!auth_dir.join("encryption.key").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn production_marker_read_rejects_group_or_world_writable_state_dir() {
        let base = tmpdir("marker-parent-mode");
        let sandbox_root = base.join("sandbox/home");
        let auth_dir = sandbox_root.join(".claude-science");
        let fake_real = base.join("real-science");
        std::fs::create_dir_all(&sandbox_root).unwrap();
        forge_guarded(
            &auth_dir,
            "virtual@localhost.invalid",
            &sandbox_root,
            &fake_real,
        )
        .unwrap();
        std::fs::set_permissions(
            base.join("sandbox/state"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let error = ensure_virtual_login_guarded(
            &auth_dir,
            "virtual@localhost.invalid",
            &sandbox_root,
            &fake_real,
        )
        .unwrap_err();
        assert!(error.to_string().contains("私有状态目录"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn running_intact_v080_login_bootstraps_production_marker_without_rewriting_credentials() {
        let base = tmpdir("marker-bootstrap-running");
        let sandbox_root = base.join("sandbox/home");
        let auth_dir = sandbox_root.join(".claude-science");
        let fake_real = base.join("real-science");
        std::fs::create_dir_all(&sandbox_root).unwrap();
        let forged = forge_guarded(
            &auth_dir,
            "virtual@localhost.invalid",
            &sandbox_root,
            &fake_real,
        )
        .unwrap();
        let marker = marker_path(&sandbox_root).unwrap();
        std::fs::remove_file(&marker).unwrap();
        let enc_before = std::fs::read(&forged.enc_file).unwrap();
        let active_before = std::fs::read(auth_dir.join("active-org.json")).unwrap();

        bootstrap_marker_for_intact_login(&auth_dir, "virtual@localhost.invalid", &sandbox_root)
            .unwrap();
        assert!(marker.is_file());
        assert_eq!(std::fs::read(&forged.enc_file).unwrap(), enc_before);
        assert_eq!(
            std::fs::read(auth_dir.join("active-org.json")).unwrap(),
            active_before
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn legacy_choice_revalidates_directory_identity_before_restore() {
        let dir = tmpdir("choice-revalidate");
        let fake_real = tmpdir("realcred-choice-revalidate");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let first = uuid_v4().unwrap();
        let second = uuid_v4().unwrap();
        std::fs::create_dir_all(dir.join("orgs").join(&first)).unwrap();
        std::fs::create_dir_all(dir.join("orgs").join(&second)).unwrap();
        std::fs::remove_file(the_enc_file(&dir)).unwrap();
        std::fs::remove_file(dir.join("active-org.json")).unwrap();
        std::fs::remove_file(marker_path(&dir).unwrap()).unwrap();
        let candidates =
            match ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap_err() {
                EnsureVirtualLoginError::HistoryChoiceRequired(candidates) => candidates,
                other => panic!("expected history choice, got {other}"),
            };
        let chosen = candidates
            .into_iter()
            .find(|candidate| candidate.org_uuid == first)
            .unwrap();
        std::fs::remove_dir(dir.join("orgs").join(&first)).unwrap();
        std::fs::create_dir(dir.join("orgs").join(&first)).unwrap();
        assert!(restore_history_choice(&dir, email, &dir, &chosen).is_err());
        assert!(!dir.join("active-org.json").exists());
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn org_directory_scan_ignores_symlinks() {
        let dir = tmpdir("org-symlink");
        let outside = tmpdir("org-symlink-outside");
        std::fs::create_dir_all(dir.join("orgs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let real_org = uuid_v4().unwrap();
        let linked_org = uuid_v4().unwrap();
        std::fs::create_dir(dir.join("orgs").join(&real_org)).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("orgs").join(&linked_org)).unwrap();
        let candidates = scan_org_dirs(&dir);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].org_uuid, real_org);
        for d in [dir, outside] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn legacy_decryptable_token_is_not_used_as_history_identity() {
        let dir = tmpdir("token-not-identity");
        let fake_real = tmpdir("realcred-token-not-identity");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let token_org = read_active_org_uuid(&dir);
        std::fs::remove_file(dir.join("active-org.json")).unwrap();
        std::fs::remove_file(marker_path(&dir).unwrap()).unwrap();

        let (created, action) =
            ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Created);
        assert_ne!(created.org_uuid, token_org, "不得从可解 token 借用组织身份");
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_recreates_key_on_invalid_base64() {
        // P2a：OAUTH_ENCRYPTION_KEY 是非法 base64 → 不报错，重造合法 key，org 复用，新 .enc 可解。
        let dir = tmpdir("badkey");
        let fake_real = tmpdir("realcred-bk");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        let mut blob = String::new();
        for k in KEY_NAMES {
            if k == "OAUTH_ENCRYPTION_KEY" {
                blob.push_str(&format!("{k}=!!!!not-base64!!!!\n"));
            } else {
                blob.push_str(&format!("{k}={}\n", b64_32().unwrap()));
            }
        }
        std::fs::write(dir.join("encryption.key"), blob).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired, "active-org.json 仍在 → 修复");
        assert_eq!(r.org_uuid, org0, "换 key 也不换 org");
        let key = read_oauth_key(&dir);
        let body = std::fs::read_to_string(the_enc_file(&dir)).unwrap();
        assert!(
            decrypt_token_v2(&body, &key).is_ok(),
            "重造 key 后新 .enc 应可解"
        );
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_repairs_when_token_structurally_damaged() {
        // P2：.enc 能解密但结构损坏（provider 篡改 / account 非 UUID）→ 不误判 Reused，走修复保 org。
        let dir = tmpdir("bad-struct");
        let fake_real = tmpdir("realcred-bs");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        let key = read_oauth_key(&dir);
        // 用现有 key 重写一个「可解但结构坏」的 .enc：provider 被改、account 非 UUID。
        let bad = serde_json::json!({
            "email": email,
            "org_uuid": org0,
            "account_uuid": "not-a-uuid",
            "provider": "tampered",
            "token_expires_at": "2099-01-01T00:00:00.000Z"
        });
        let enc = encrypt_token_v2(&serde_json::to_vec(&bad).unwrap(), &key).unwrap();
        std::fs::write(the_enc_file(&dir), enc).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired, "结构损坏应修复而非复用");
        assert_eq!(r.org_uuid, org0, "修复仍保 org");
        // 修复后应重新自洽（provider=claude_ai、account 合法 UUID）
        assert!(
            looks_like_uuid(&r.account_uuid),
            "修复后 account 应为合法 UUID"
        );
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_repairs_when_token_expired() {
        // P2：.enc 可解但 token 已过期 → 不误判 Reused，走修复；修复后（远期）应可复用。
        let dir = tmpdir("expired");
        let fake_real = tmpdir("realcred-exp");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        let key = read_oauth_key(&dir);
        let expired = serde_json::json!({
            "email": email,
            "org_uuid": org0,
            "account_uuid": uuid_v4().unwrap(),
            "provider": "claude_ai",
            "access_token": "sk-ant-virtual-x",
            "token_expires_at": "2000-01-01T00:00:00.000Z"
        });
        let enc = encrypt_token_v2(&serde_json::to_vec(&expired).unwrap(), &key).unwrap();
        std::fs::write(the_enc_file(&dir), enc).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired, "过期令牌不应误判 Reused");
        assert_eq!(r.org_uuid, org0);
        // 修复后新令牌远期未过期 → 再次 ensure 应可复用。
        let (_r2, a2) = ensure_virtual_login_guarded(&dir, email, &dir, &fake_real).unwrap();
        assert_eq!(a2, LoginAction::Reused, "修复后应可复用");
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn login_intact_true_for_fresh_false_when_damaged_and_readonly() {
        // Bug2（0.2.1）：健康快捷路径要靠只读校验区分「自洽」与「健康但登录失效」。
        let dir = tmpdir("intact");
        let fake_real = tmpdir("realcred-intact");
        let email = "virtual@localhost.invalid";
        forge_guarded(&dir, email, &dir, &fake_real).unwrap();

        // 新鲜伪造 → 自洽 → true。
        assert!(
            login_intact_guarded(&dir, email, &dir, &fake_real),
            "新鲜伪造应判自洽"
        );

        // 只读不写：连调两次，三个文件字节都不变。
        let enc0 = std::fs::read(the_enc_file(&dir)).unwrap();
        let key0 = std::fs::read(dir.join("encryption.key")).unwrap();
        let org0 = std::fs::read(dir.join("active-org.json")).unwrap();
        assert!(login_intact_guarded(&dir, email, &dir, &fake_real));
        assert_eq!(
            std::fs::read(the_enc_file(&dir)).unwrap(),
            enc0,
            "不改 .enc"
        );
        assert_eq!(
            std::fs::read(dir.join("encryption.key")).unwrap(),
            key0,
            "不改 encryption.key"
        );
        assert_eq!(
            std::fs::read(dir.join("active-org.json")).unwrap(),
            org0,
            "不改 active-org.json"
        );

        // 删 .enc → 不自洽 → false（这一步旧 stub 恒真会失败）。
        std::fs::remove_file(the_enc_file(&dir)).unwrap();
        assert!(
            !login_intact_guarded(&dir, email, &dir, &fake_real),
            "缺 .enc 应判不自洽"
        );

        // 过期令牌 → 不自洽 → false。
        let dir2 = tmpdir("intact-exp");
        let fr2 = tmpdir("realcred-exp2");
        forge_guarded(&dir2, email, &dir2, &fr2).unwrap();
        let org2 = read_active_org_uuid(&dir2);
        let key2 = read_oauth_key(&dir2);
        let expired = serde_json::json!({
            "email": email, "org_uuid": org2, "account_uuid": uuid_v4().unwrap(),
            "provider": "claude_ai", "access_token": "sk-ant-virtual-x",
            "token_expires_at": "2000-01-01T00:00:00.000Z"
        });
        let enc = encrypt_token_v2(&serde_json::to_vec(&expired).unwrap(), &key2).unwrap();
        std::fs::write(the_enc_file(&dir2), enc).unwrap();
        assert!(
            !login_intact_guarded(&dir2, email, &dir2, &fr2),
            "过期令牌应判不自洽"
        );

        for d in [dir, fake_real, dir2, fr2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn token_expiry_check() {
        assert!(token_not_expired("2099-01-01T00:00:00.000Z"));
        assert!(!token_not_expired("2000-01-01T00:00:00.000Z"));
        assert!(!token_not_expired(""), "空串视为过期");
        assert!(!token_not_expired("2099-13"), "太短视为过期");
        assert!(!token_not_expired("20990101ZZ"), "格式不对视为过期");
        let t = today_utc_ymd();
        assert_eq!(t.len(), 10);
        assert_eq!(&t[4..5], "-");
        assert_eq!(&t[7..8], "-");
    }
}
