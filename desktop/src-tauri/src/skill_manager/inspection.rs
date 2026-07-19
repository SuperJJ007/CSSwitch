use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::{SkillErrorCode, SkillManagerError, SkillResult};
use super::model::{validate_manifest, SkillManifest};
use super::requirements::SkillRequirements;

pub(crate) const MAX_FILE_COUNT: usize = 1_000;
pub(crate) const MAX_DIRECTORY_COUNT: usize = 1_000;
pub(crate) const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_TOTAL_SIZE: u64 = 128 * 1024 * 1024;
pub(crate) const MAX_SKILL_MD_SIZE: u64 = 1024 * 1024;
pub(crate) const MAX_REQUIREMENTS_SIZE: u64 = 64 * 1024;
pub(crate) const MAX_PATH_BYTES: usize = 1_024;
pub(crate) const MAX_PATH_DEPTH: usize = 32;

#[derive(Clone, Copy)]
struct InspectionLimits {
    file_count: usize,
    directory_count: usize,
    file_size: u64,
    total_size: u64,
    skill_md_size: u64,
    path_bytes: usize,
    path_depth: usize,
}

impl Default for InspectionLimits {
    fn default() -> Self {
        Self {
            file_count: MAX_FILE_COUNT,
            directory_count: MAX_DIRECTORY_COUNT,
            file_size: MAX_FILE_SIZE,
            total_size: MAX_TOTAL_SIZE,
            skill_md_size: MAX_SKILL_MD_SIZE,
            path_bytes: MAX_PATH_BYTES,
            path_depth: MAX_PATH_DEPTH,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct InspectedFile {
    pub(crate) relative_path: PathBuf,
    pub(crate) size: u64,
    pub(crate) executable: bool,
    pub(crate) content: Vec<u8>,
}

impl std::fmt::Debug for InspectedFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InspectedFile")
            .field("relative_path", &self.relative_path)
            .field("size", &self.size)
            .field("executable", &self.executable)
            .field("content", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InspectionSummary {
    pub(crate) manifest: SkillManifest,
    pub(crate) requirements: SkillRequirements,
    pub(crate) content_hash: String,
    pub(crate) file_count: usize,
    pub(crate) total_size: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct InspectionResult {
    pub(crate) source_root: PathBuf,
    pub(crate) summary: InspectionSummary,
    pub(crate) files: Vec<InspectedFile>,
}

impl std::fmt::Debug for InspectionResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InspectionResult")
            .field("source_root", &"<redacted>")
            .field("summary", &self.summary)
            .field("files", &self.files)
            .finish()
    }
}

struct OwnedFd(RawFd);

impl Drop for OwnedFd {
    fn drop(&mut self) {
        // SAFETY: OwnedFd is created only from successful open/openat calls and owns its fd.
        unsafe { libc::close(self.0) };
    }
}

pub(crate) struct AnchoredSourceRoot {
    path: PathBuf,
    directory: OwnedFd,
    snapshot: libc::stat,
}

#[derive(Clone)]
pub(crate) struct AnchoredSourceEntry {
    name: OsString,
    snapshot: libc::stat,
}

pub(crate) struct AnchoredSkillSource {
    source_root: PathBuf,
    directory_name: OsString,
    directory: OwnedFd,
    snapshot: libc::stat,
}

impl AnchoredSourceRoot {
    pub(crate) fn open(path: &Path) -> SkillResult<Self> {
        let directory = open_source_dir(path, &|_| {})?;
        let snapshot = fd_stat(directory.0)?;
        if file_kind(snapshot.st_mode) != libc::S_IFDIR {
            return Err(unsupported_type());
        }
        Ok(Self {
            path: path.to_path_buf(),
            directory,
            snapshot,
        })
    }

    pub(crate) fn entries(&self) -> SkillResult<Vec<AnchoredSourceEntry>> {
        let mut entries = Vec::new();
        for name in read_dir_names(self.directory.0)? {
            let name_c = os_to_cstring(&name)?;
            entries.push(AnchoredSourceEntry {
                snapshot: fstatat(self.directory.0, &name_c)?,
                name,
            });
        }
        entries.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
        Ok(entries)
    }

    pub(crate) fn open_skill(
        &self,
        entry: &AnchoredSourceEntry,
    ) -> SkillResult<AnchoredSkillSource> {
        if !entry.is_directory() {
            return Err(unsupported_type());
        }
        let name_c = os_to_cstring(&entry.name)?;
        let current = fstatat(self.directory.0, &name_c)?;
        if !same_snapshot(&entry.snapshot, &current) {
            return Err(source_changed());
        }
        let directory = openat_dir(self.directory.0, &name_c)?;
        let opened = fd_stat(directory.0)?;
        if !same_snapshot(&current, &opened) {
            return Err(source_changed());
        }
        Ok(AnchoredSkillSource {
            source_root: self.path.join(&entry.name),
            directory_name: entry.name.clone(),
            directory,
            snapshot: opened,
        })
    }

    pub(crate) fn verify_path_unchanged(&self) -> SkillResult<()> {
        let current = fd_stat(self.directory.0)?;
        if !same_snapshot(&self.snapshot, &current) {
            return Err(source_changed());
        }
        let reopened = open_source_dir(&self.path, &|_| {})?;
        if !same_identity(&self.snapshot, &fd_stat(reopened.0)?) {
            return Err(source_changed());
        }
        Ok(())
    }

    pub(crate) fn verify_entry_linked(&self, source: &AnchoredSkillSource) -> SkillResult<()> {
        let name_c = os_to_cstring(&source.directory_name)?;
        let linked = fstatat(self.directory.0, &name_c)?;
        let current = fd_stat(source.directory.0)?;
        if !same_identity(&linked, &source.snapshot) || !same_identity(&current, &source.snapshot) {
            return Err(source_changed());
        }
        Ok(())
    }
}

impl AnchoredSourceEntry {
    pub(crate) fn name(&self) -> &OsStr {
        &self.name
    }

    pub(crate) fn is_directory(&self) -> bool {
        file_kind(self.snapshot.st_mode) == libc::S_IFDIR
    }

    pub(crate) fn is_symlink(&self) -> bool {
        file_kind(self.snapshot.st_mode) == libc::S_IFLNK
    }
}

impl AnchoredSkillSource {
    pub(crate) fn top_level_skill_md_kind(&self) -> SkillResult<Option<libc::mode_t>> {
        let name = os_to_cstring(OsStr::new("SKILL.md"))?;
        match fstatat_optional(self.directory.0, &name)? {
            Some(stat) => Ok(Some(file_kind(stat.st_mode))),
            None => Ok(None),
        }
    }

    pub(crate) fn inspect(&self) -> SkillResult<InspectionResult> {
        inspect_opened_skill_source(
            self.source_root.clone(),
            duplicate_fd(&self.directory)?,
            InspectionLimits::default(),
            &|_| {},
        )
    }
}

pub(crate) fn inspect_skill_source(source: &Path) -> SkillResult<InspectionResult> {
    inspect_skill_source_inner(source, InspectionLimits::default(), &|_| {}, &|_| {})
}

fn inspect_skill_source_inner(
    source: &Path,
    limits: InspectionLimits,
    before_open: &dyn Fn(&Path),
    before_source_component_open: &dyn Fn(&Path),
) -> SkillResult<InspectionResult> {
    let root_fd = open_source_dir(source, before_source_component_open)?;
    inspect_opened_skill_source(source.to_path_buf(), root_fd, limits, before_open)
}

fn inspect_opened_skill_source(
    source_root: PathBuf,
    root_fd: OwnedFd,
    limits: InspectionLimits,
    before_open: &dyn Fn(&Path),
) -> SkillResult<InspectionResult> {
    let root_before = fd_stat(root_fd.0)?;
    if file_kind(root_before.st_mode) != libc::S_IFDIR {
        return Err(unsupported_type());
    }

    let mut scanner = Scanner {
        files: Vec::new(),
        collision_keys: BTreeSet::new(),
        total_size: 0,
        directory_count: 0,
        skill_md: None,
        limits,
        before_open,
    };
    scanner.walk_dir(root_fd.0, Path::new(""), 0)?;
    scanner.files.sort_by(|left, right| {
        left.relative_path
            .as_os_str()
            .as_bytes()
            .cmp(right.relative_path.as_os_str().as_bytes())
    });
    let root_after = fd_stat(root_fd.0)?;
    if !same_snapshot(&root_before, &root_after) {
        return Err(source_changed());
    }

    let skill_md = scanner
        .skill_md
        .as_deref()
        .ok_or_else(|| manifest_error("Skill 顶层必须包含 SKILL.md"))?;
    let manifest = parse_manifest(skill_md)?;
    let requirements_json = scanner
        .files
        .iter()
        .find(|file| file.relative_path == Path::new("csswitch.skill.json"))
        .map(|file| file.content.as_slice());
    let requirements = SkillRequirements::from_public_json(requirements_json)
        .map_err(|_| manifest_error("csswitch.skill.json requirements 无效"))?;
    let content_hash = scanner.finish_hash();
    let file_count = scanner.files.len();
    Ok(InspectionResult {
        source_root,
        summary: InspectionSummary {
            manifest,
            requirements,
            content_hash,
            file_count,
            total_size: scanner.total_size,
        },
        files: scanner.files,
    })
}

fn duplicate_fd(directory: &OwnedFd) -> SkillResult<OwnedFd> {
    let fd = unsafe { libc::fcntl(directory.0, libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(SkillManagerError::safe_io());
    }
    Ok(OwnedFd(fd))
}

struct Scanner<'a> {
    files: Vec<InspectedFile>,
    collision_keys: BTreeSet<String>,
    total_size: u64,
    directory_count: usize,
    skill_md: Option<Vec<u8>>,
    limits: InspectionLimits,
    before_open: &'a dyn Fn(&Path),
}

impl Scanner<'_> {
    fn walk_dir(&mut self, dir_fd: RawFd, relative: &Path, depth: usize) -> SkillResult<()> {
        if depth > self.limits.path_depth {
            return Err(limit_error("Skill 目录层级超过限制"));
        }
        for name in read_dir_names(dir_fd)? {
            validate_component(&name)?;
            let child_relative = relative.join(&name);
            validate_relative_path(&child_relative, depth + 1, self.limits)?;
            let collision_key = child_relative
                .as_os_str()
                .as_bytes()
                .iter()
                .map(u8::to_ascii_lowercase)
                .map(char::from)
                .collect::<String>();
            if !self.collision_keys.insert(collision_key) {
                return Err(unsafe_path("Skill 中存在大小写路径碰撞"));
            }

            let name_c = os_to_cstring(&name)?;
            let before = fstatat(dir_fd, &name_c)?;
            match file_kind(before.st_mode) {
                libc::S_IFDIR => {
                    if self.directory_count >= self.limits.directory_count {
                        return Err(limit_error("Skill 目录数量超过 1,000"));
                    }
                    self.directory_count += 1;
                    let child_fd = openat_dir(dir_fd, &name_c)?;
                    let opened = fd_stat(child_fd.0)?;
                    if !same_identity(&before, &opened) {
                        return Err(source_changed());
                    }
                    self.walk_dir(child_fd.0, &child_relative, depth + 1)?;
                    let after = fd_stat(child_fd.0)?;
                    if !same_snapshot(&opened, &after) {
                        return Err(source_changed());
                    }
                }
                libc::S_IFREG => self.read_file(dir_fd, &name_c, &child_relative, &before)?,
                libc::S_IFLNK => return Err(unsafe_path("Skill 中不允许符号链接")),
                _ => return Err(unsupported_type()),
            }
        }
        Ok(())
    }

    fn read_file(
        &mut self,
        dir_fd: RawFd,
        name: &CStr,
        relative: &Path,
        before: &libc::stat,
    ) -> SkillResult<()> {
        if before.st_nlink != 1 {
            return Err(SkillManagerError::new(
                SkillErrorCode::HardlinkRejected,
                "Skill 中不允许 hardlink",
                "请把 hardlink 替换为独立的普通文件",
            ));
        }
        let size = u64::try_from(before.st_size).map_err(|_| limit_error("文件大小无效"))?;
        if size > self.limits.file_size {
            return Err(limit_error("Skill 中存在超过 16 MiB 的文件"));
        }
        if relative == Path::new("SKILL.md") && size > self.limits.skill_md_size {
            return Err(limit_error("SKILL.md 超过 1 MiB"));
        }
        if relative == Path::new("csswitch.skill.json") && size > MAX_REQUIREMENTS_SIZE {
            return Err(limit_error("csswitch.skill.json 超过 64 KiB"));
        }
        if self.files.len() >= self.limits.file_count {
            return Err(limit_error("Skill 文件数量超过 1,000"));
        }
        self.total_size = self
            .total_size
            .checked_add(size)
            .ok_or_else(|| limit_error("Skill 总大小无效"))?;
        if self.total_size > self.limits.total_size {
            return Err(limit_error("Skill 总大小超过 128 MiB"));
        }

        (self.before_open)(relative);
        let fd = openat_file(dir_fd, name)?;
        let opened = fd_stat(fd.0)?;
        if !same_snapshot(before, &opened)
            || file_kind(opened.st_mode) != libc::S_IFREG
            || opened.st_nlink != 1
        {
            return Err(source_changed());
        }
        // File is prevented from closing the borrowed fd; OwnedFd closes it exactly once.
        let mut file = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd.0) });
        let mut content = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        file.by_ref()
            .take(self.limits.file_size + 1)
            .read_to_end(&mut content)
            .map_err(|_| SkillManagerError::safe_io())?;
        let after = fd_stat(fd.0)?;
        if !same_snapshot(&opened, &after) || content.len() as u64 != size {
            return Err(source_changed());
        }

        if relative == Path::new("SKILL.md") {
            self.skill_md = Some(content.clone());
        }
        self.files.push(InspectedFile {
            relative_path: relative.to_path_buf(),
            size,
            executable: before.st_mode & 0o111 != 0,
            content,
        });
        Ok(())
    }

    fn finish_hash(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"CSSWITCH-SKILL-CONTENT-V2\0");
        digest.update((self.files.len() as u64).to_be_bytes());
        for file in &self.files {
            let path = file.relative_path.as_os_str().as_bytes();
            let content = &file.content;
            digest.update((path.len() as u64).to_be_bytes());
            digest.update(path);
            digest.update((content.len() as u64).to_be_bytes());
            digest.update([u8::from(file.executable)]);
            digest.update(content);
        }
        let bytes = digest.finalize();
        let mut result = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(&mut result, "{byte:02x}").expect("writing to String cannot fail");
        }
        result
    }
}

fn parse_manifest(content: &[u8]) -> SkillResult<SkillManifest> {
    let text =
        std::str::from_utf8(content).map_err(|_| manifest_error("SKILL.md 必须使用 UTF-8 编码"))?;
    let normalized = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines = normalized.lines().collect::<Vec<_>>();
    if lines.first().copied() != Some("---") {
        return Err(manifest_error("SKILL.md 必须以 YAML frontmatter 开始"));
    }
    let mut fields = BTreeMap::new();
    let mut index = 1;
    let mut closed = false;
    while index < lines.len() {
        let line = lines[index];
        if line == "---" {
            closed = true;
            break;
        }
        index += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.chars().next().is_some_and(char::is_whitespace) {
            // Indented YAML belongs to the preceding extension field. Unknown extension
            // fields are intentionally tolerated and do not affect CSSwitch metadata.
            continue;
        }
        let (key, raw_value) = line
            .split_once(':')
            .ok_or_else(|| manifest_error("frontmatter 字段必须使用 key: value 格式"))?;
        let key = key.trim();
        if !matches!(key, "name" | "description" | "version" | "license") {
            continue;
        }
        if fields.contains_key(key) {
            return Err(manifest_error("frontmatter 包含重复字段"));
        }
        let raw_value = raw_value.trim();
        let value = if matches!(raw_value, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            let block_start = index;
            while index < lines.len()
                && lines[index] != "---"
                && (lines[index].trim().is_empty()
                    || lines[index].chars().next().is_some_and(char::is_whitespace))
            {
                index += 1;
            }
            parse_block_scalar(raw_value, &lines[block_start..index])?
        } else {
            parse_scalar(raw_value)?
        };
        fields.insert(key.to_string(), value);
    }
    if !closed {
        return Err(manifest_error("SKILL.md frontmatter 缺少结束分隔符"));
    }
    let manifest = SkillManifest {
        name: fields
            .remove("name")
            .ok_or_else(|| manifest_error("frontmatter 缺少 name"))?,
        description: fields
            .remove("description")
            .ok_or_else(|| manifest_error("frontmatter 缺少 description"))?,
        declared_version: fields.remove("version"),
        license: fields.remove("license"),
    };
    validate_manifest(&manifest).map_err(|_| manifest_error("Skill manifest 字段无效"))?;
    Ok(manifest)
}

fn parse_block_scalar(style: &str, lines: &[&str]) -> SkillResult<String> {
    if lines.is_empty() {
        return Err(manifest_error("frontmatter 多行字段不能为空"));
    }
    let indentation = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches([' ', '\t']).len())
        .min()
        .ok_or_else(|| manifest_error("frontmatter 多行字段不能为空"))?;
    if indentation == 0 {
        return Err(manifest_error("frontmatter 多行字段必须缩进"));
    }
    let values = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                Ok("")
            } else {
                line.get(indentation..)
                    .ok_or_else(|| manifest_error("frontmatter 多行字段缩进无效"))
            }
        })
        .collect::<SkillResult<Vec<_>>>()?;
    let mut value = if style.starts_with('>') {
        values.join(" ")
    } else {
        values.join("\n")
    };
    if !style.ends_with('-') {
        value.push('\n');
    }
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(manifest_error("frontmatter 多行字段不能为空"));
    }
    Ok(value)
}

fn parse_scalar(value: &str) -> SkillResult<String> {
    if value.is_empty() {
        return Err(manifest_error("frontmatter 字段不能为空"));
    }
    let parsed = if value.starts_with('"') || value.starts_with('\'') {
        let quote = value.as_bytes()[0];
        if value.len() < 2 || value.as_bytes()[value.len() - 1] != quote {
            return Err(manifest_error("frontmatter 引号不匹配"));
        }
        let inner = &value[1..value.len() - 1];
        if inner.contains('\\') || inner.contains('\n') || inner.contains('\r') {
            return Err(manifest_error("MVP frontmatter 不支持转义或多行值"));
        }
        inner
    } else {
        if value.chars().any(|ch| {
            matches!(
                ch,
                '#' | '[' | ']' | '{' | '}' | '&' | '*' | '!' | '|' | '>'
            )
        }) {
            return Err(manifest_error("frontmatter 值必须是简单标量"));
        }
        value
    };
    Ok(parsed.trim().to_string())
}

fn open_source_dir(source: &Path, before_component_open: &dyn Fn(&Path)) -> SkillResult<OwnedFd> {
    if !source.is_absolute() {
        return Err(SkillManagerError::new(
            SkillErrorCode::InvalidSource,
            "Skill 来源路径必须是绝对路径",
            "请通过本地文件夹选择器重新选择 Skill",
        ));
    }
    let root_c = CString::new("/").expect("root contains no NUL");
    // SAFETY: root_c is valid and the flags only open an existing directory.
    let root_fd = unsafe {
        libc::open(
            root_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(SkillManagerError::safe_io());
    }
    let mut current_fd = OwnedFd(root_fd);
    let mut current = PathBuf::new();
    for component in source.components() {
        match component {
            Component::RootDir => current.push(Path::new("/")),
            Component::Normal(value) => {
                current.push(value);
                let name = os_to_cstring(value)?;
                let before = fstatat(current_fd.0, &name)?;
                if file_kind(before.st_mode) == libc::S_IFLNK {
                    return Err(unsafe_path("Skill 来源路径不能经过符号链接"));
                }
                if file_kind(before.st_mode) != libc::S_IFDIR {
                    return Err(SkillManagerError::new(
                        SkillErrorCode::InvalidSource,
                        "Skill 来源必须是可读的普通目录",
                        "请选择一个真实的本地 Skill 文件夹",
                    ));
                }
                before_component_open(&current);
                let next = openat_dir(current_fd.0, &name)?;
                let opened = fd_stat(next.0)?;
                if !same_identity(&before, &opened) {
                    return Err(source_changed());
                }
                current_fd = next;
            }
            _ => return Err(unsafe_path("Skill 来源路径包含不安全组件")),
        }
    }
    Ok(current_fd)
}

fn validate_component(name: &OsStr) -> SkillResult<()> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(unsafe_path("Skill 路径包含非法组件"));
    }
    if !bytes.is_ascii() {
        return Err(unsafe_path(
            "MVP 为避免 Unicode 路径碰撞，只接受 ASCII 文件名",
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &Path, depth: usize, limits: InspectionLimits) -> SkillResult<()> {
    if path.is_absolute()
        || depth > limits.path_depth
        || path.as_os_str().as_bytes().len() > limits.path_bytes
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(unsafe_path("Skill 相对路径不安全或超过限制"));
    }
    Ok(())
}

fn read_dir_names(dir_fd: RawFd) -> SkillResult<Vec<std::ffi::OsString>> {
    // SAFETY: dup returns an independent descriptor or -1; fdopendir takes ownership on success.
    let duplicate = unsafe { libc::dup(dir_fd) };
    if duplicate < 0 {
        return Err(SkillManagerError::safe_io());
    }
    // SAFETY: duplicate is a valid directory descriptor.
    let directory = unsafe { libc::fdopendir(duplicate) };
    if directory.is_null() {
        // SAFETY: fdopendir failed and did not take ownership of duplicate.
        unsafe { libc::close(duplicate) };
        return Err(SkillManagerError::safe_io());
    }
    let result = collect_dir_names(|| {
        set_errno(0);
        // SAFETY: directory remains valid until closedir below; each entry name is copied.
        let entry = unsafe { libc::readdir(directory) };
        if entry.is_null() {
            return if current_errno() == 0 {
                Ok(None)
            } else {
                Err(SkillManagerError::safe_io())
            };
        }
        // SAFETY: d_name is a NUL-terminated array supplied by readdir.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        Ok(Some(OsStr::from_bytes(name.to_bytes()).to_os_string()))
    });
    // SAFETY: directory was returned by fdopendir and is closed exactly once.
    let close_result = unsafe { libc::closedir(directory) };
    if close_result != 0 {
        return Err(SkillManagerError::safe_io());
    }
    result
}

fn collect_dir_names(
    mut next: impl FnMut() -> SkillResult<Option<std::ffi::OsString>>,
) -> SkillResult<Vec<std::ffi::OsString>> {
    let mut names = Vec::new();
    while let Some(name) = next()? {
        let bytes = name.as_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(name);
        }
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

#[cfg(target_os = "macos")]
fn errno_pointer() -> *mut libc::c_int {
    // SAFETY: __error returns the current thread's errno address on macOS.
    unsafe { libc::__error() }
}

#[cfg(not(target_os = "macos"))]
fn errno_pointer() -> *mut libc::c_int {
    // SAFETY: __errno_location returns the current thread's errno address on supported Unix CI.
    unsafe { libc::__errno_location() }
}

fn set_errno(value: libc::c_int) {
    // SAFETY: errno_pointer points at writable thread-local errno storage.
    unsafe { *errno_pointer() = value };
}

fn current_errno() -> libc::c_int {
    // SAFETY: errno_pointer points at readable thread-local errno storage.
    unsafe { *errno_pointer() }
}

#[cfg(test)]
fn path_to_cstring(path: &Path) -> SkillResult<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| unsafe_path("路径包含 NUL 字节"))
}

fn os_to_cstring(value: &OsStr) -> SkillResult<CString> {
    CString::new(value.as_bytes()).map_err(|_| unsafe_path("路径包含 NUL 字节"))
}

fn openat_dir(parent: RawFd, name: &CStr) -> SkillResult<OwnedFd> {
    // SAFETY: name is NUL terminated; flags open an existing directory without following its leaf.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(source_changed());
    }
    Ok(OwnedFd(fd))
}

fn openat_file(parent: RawFd, name: &CStr) -> SkillResult<OwnedFd> {
    // SAFETY: name is NUL terminated; flags open an existing file without following its leaf.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(source_changed());
    }
    Ok(OwnedFd(fd))
}

fn fd_stat(fd: RawFd) -> SkillResult<libc::stat> {
    // SAFETY: zeroed stat is a valid output buffer for fstat.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fd is open and stat points to writable memory.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(SkillManagerError::safe_io());
    }
    Ok(stat)
}

fn fstatat(parent: RawFd, name: &CStr) -> SkillResult<libc::stat> {
    // SAFETY: zeroed stat is a valid output buffer for fstatat.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: parent/name are valid and AT_SYMLINK_NOFOLLOW prevents leaf traversal.
    if unsafe { libc::fstatat(parent, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(source_changed());
    }
    Ok(stat)
}

fn fstatat_optional(parent: RawFd, name: &CStr) -> SkillResult<Option<libc::stat>> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        return Ok(Some(unsafe { stat.assume_init() }));
    }
    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(SkillManagerError::safe_io())
    }
}

fn same_identity(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && file_kind(left.st_mode) == file_kind(right.st_mode)
}

fn same_snapshot(left: &libc::stat, right: &libc::stat) -> bool {
    same_identity(left, right)
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

fn file_kind(mode: libc::mode_t) -> libc::mode_t {
    mode & libc::S_IFMT
}

fn unsafe_path(message: &str) -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::UnsafePath,
        message,
        "请移除链接、特殊路径或碰撞名称后重试",
    )
}

fn unsupported_type() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::UnsupportedFileType,
        "Skill 只允许普通目录和普通文件",
        "请移除 FIFO、socket、device 或其他特殊文件",
    )
}

fn limit_error(message: &str) -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::LimitExceeded,
        message,
        "请精简 Skill 内容后重试",
    )
}

fn source_changed() -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::SourceChanged,
        "扫描期间 Skill 来源发生变化",
        "请停止修改该目录后重试",
    )
}

fn manifest_error(message: &str) -> SkillManagerError {
    SkillManagerError::new(
        SkillErrorCode::InvalidManifest,
        message,
        "请修正顶层 SKILL.md 或 csswitch.skill.json 后重试",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = crate::test_temp_root().join(format!(
                "csswitch-skill-inspection-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn valid() -> Self {
            let dir = Self::new();
            fs::write(
                dir.0.join("SKILL.md"),
                "---\nname: Probe Skill\ndescription: Deterministic test skill\n---\nBody\n",
            )
            .unwrap();
            dir
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn valid_skill_hash_normalizes_metadata_but_binds_executable_semantics() {
        let dir = TestDir::valid();
        fs::create_dir(dir.0.join("scripts")).unwrap();
        fs::write(dir.0.join("scripts/run.sh"), b"echo safe\n").unwrap();
        let first = inspect_skill_source(&dir.0).unwrap();
        fs::set_permissions(
            dir.0.join("scripts/run.sh"),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let second = inspect_skill_source(&dir.0).unwrap();
        assert_ne!(first.summary.content_hash, second.summary.content_hash);
        assert!(
            !first
                .files
                .iter()
                .find(|file| file.relative_path == Path::new("scripts/run.sh"))
                .unwrap()
                .executable
        );
        assert!(
            second
                .files
                .iter()
                .find(|file| file.relative_path == Path::new("scripts/run.sh"))
                .unwrap()
                .executable
        );
        fs::set_permissions(
            dir.0.join("scripts/run.sh"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let third = inspect_skill_source(&dir.0).unwrap();
        assert_eq!(second.summary.content_hash, third.summary.content_hash);
        assert_eq!(first.summary.file_count, 2);
        assert_eq!(first.summary.manifest.name, "Probe Skill");
        assert_eq!(
            first.summary.requirements.needs_network,
            super::super::requirements::FlagRequirement::default()
        );
        assert_eq!(
            first.summary.requirements.restart_required.value,
            super::super::requirements::RequirementFlag::True
        );
    }

    #[test]
    fn nature_figure_frontmatter_accepts_folded_description_and_extensions() {
        let dir = TestDir::new();
        fs::write(
            dir.0.join("SKILL.md"),
            "---\nname: nature-figure\ndescription: >-\n  Publication-quality scientific figure generation inspired by Nature.\n  Supports multi-panel layouts and journal-ready exports.\nauthor: Scientific Skills Community\ntags:\n  - plotting\n  - research\nversion: 1.2.0\n---\n# Nature Figure\n",
        )
        .unwrap();
        let inspected = inspect_skill_source(&dir.0).unwrap();
        assert_eq!(inspected.summary.manifest.name, "nature-figure");
        assert_eq!(
            inspected.summary.manifest.description,
            "Publication-quality scientific figure generation inspired by Nature. Supports multi-panel layouts and journal-ready exports."
        );
        assert_eq!(
            inspected.summary.manifest.declared_version.as_deref(),
            Some("1.2.0")
        );
    }

    #[test]
    fn body_text_is_never_used_to_guess_runtime_requirements() {
        use super::super::requirements::{FlagRequirement, ListRequirement};

        let dir = TestDir::valid();
        fs::write(
            dir.0.join("notes.txt"),
            b"ssh HTTPS_PROXY mcp execute /usr/bin/python3 secret-looking-token",
        )
        .unwrap();
        let inspected = inspect_skill_source(&dir.0).unwrap();
        assert_eq!(
            inspected.summary.requirements.needs_network,
            FlagRequirement::default()
        );
        assert_eq!(
            inspected.summary.requirements.needs_ssh,
            FlagRequirement::default()
        );
        assert_eq!(
            inspected.summary.requirements.needs_mcp,
            FlagRequirement::default()
        );
        assert_eq!(
            inspected.summary.requirements.needs_local_command,
            FlagRequirement::default()
        );
        assert_eq!(
            inspected.summary.requirements.required_binaries,
            ListRequirement::default()
        );
    }

    #[test]
    fn public_requirements_are_strict_normalized_and_hash_bound() {
        use super::super::requirements::{RequirementFlag, RequirementSource};

        let dir = TestDir::valid();
        fs::write(
            dir.0.join("csswitch.skill.json"),
            br#"{
                "schema_version": 1,
                "requirements": {
                    "needs_network": true,
                    "required_binaries": ["python3", "git", "python3"],
                    "required_runtime_assets": ["templates/default.txt"]
                }
            }"#,
        )
        .unwrap();
        let first = inspect_skill_source(&dir.0).unwrap();
        assert_eq!(
            first.summary.requirements.needs_network.value,
            RequirementFlag::True
        );
        assert_eq!(
            first.summary.requirements.needs_network.source,
            RequirementSource::Declared
        );
        assert_eq!(
            first.summary.requirements.required_binaries.values,
            ["git", "python3"]
        );

        fs::write(
            dir.0.join("csswitch.skill.json"),
            br#"{"schema_version":1,"requirements":{"needs_network":false}}"#,
        )
        .unwrap();
        let second = inspect_skill_source(&dir.0).unwrap();
        assert_ne!(first.summary.content_hash, second.summary.content_hash);
        assert_eq!(
            second.summary.requirements.needs_network.value,
            RequirementFlag::False
        );
    }

    #[test]
    fn invalid_or_oversized_public_requirements_fail_without_body_disclosure() {
        let invalid = TestDir::valid();
        let secret = "DO_NOT_LOG_REQUIREMENT_SECRET";
        fs::write(
            invalid.0.join("csswitch.skill.json"),
            format!(
                "{{\"schema_version\":1,\"requirements\":{{\"required_runtime_assets\":[\"../{secret}\"]}}}}"
            ),
        )
        .unwrap();
        let error = inspect_skill_source(&invalid.0).unwrap_err();
        assert_eq!(error.code, SkillErrorCode::InvalidManifest);
        let encoded = serde_json::to_string(&error).unwrap();
        assert!(!encoded.contains(secret));
        assert!(!encoded.contains(invalid.0.to_string_lossy().as_ref()));

        let oversized = TestDir::valid();
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(oversized.0.join("csswitch.skill.json"))
            .unwrap();
        file.set_len(MAX_REQUIREMENTS_SIZE + 1).unwrap();
        assert_eq!(
            inspect_skill_source(&oversized.0).unwrap_err().code,
            SkillErrorCode::LimitExceeded
        );
    }

    #[test]
    fn source_and_child_symlinks_fail_closed() {
        let dir = TestDir::valid();
        let root_link = dir.0.with_extension("link");
        symlink(&dir.0, &root_link).unwrap();
        assert_eq!(
            inspect_skill_source(&root_link).unwrap_err().code,
            SkillErrorCode::UnsafePath
        );
        fs::remove_file(root_link).unwrap();

        symlink(dir.0.join("SKILL.md"), dir.0.join("linked.md")).unwrap();
        assert_eq!(
            inspect_skill_source(&dir.0).unwrap_err().code,
            SkillErrorCode::UnsafePath
        );
        fs::remove_file(dir.0.join("linked.md")).unwrap();
        symlink(dir.0.join("missing"), dir.0.join("middle")).unwrap();
        assert_eq!(
            inspect_skill_source(&dir.0).unwrap_err().code,
            SkillErrorCode::UnsafePath
        );
    }

    #[test]
    fn hardlinks_and_special_files_fail_closed() {
        let dir = TestDir::valid();
        fs::hard_link(dir.0.join("SKILL.md"), dir.0.join("copy.md")).unwrap();
        assert_eq!(
            inspect_skill_source(&dir.0).unwrap_err().code,
            SkillErrorCode::HardlinkRejected
        );

        let special = TestDir::valid();
        let fifo = special.0.join("pipe");
        let fifo_c = path_to_cstring(&fifo).unwrap();
        // SAFETY: fifo_c is a valid path and mode contains only permission bits.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert_eq!(
            inspect_skill_source(&special.0).unwrap_err().code,
            SkillErrorCode::UnsupportedFileType
        );
    }

    #[test]
    fn manifest_and_limits_are_enforced() {
        let missing = TestDir::new();
        assert_eq!(
            inspect_skill_source(&missing.0).unwrap_err().code,
            SkillErrorCode::InvalidManifest
        );

        let invalid = TestDir::new();
        fs::write(
            invalid.0.join("SKILL.md"),
            "---\nname: no description\n---\n",
        )
        .unwrap();
        assert_eq!(
            inspect_skill_source(&invalid.0).unwrap_err().code,
            SkillErrorCode::InvalidManifest
        );

        let huge = TestDir::valid();
        let file = OpenOptions::new()
            .write(true)
            .open(huge.0.join("SKILL.md"))
            .unwrap();
        file.set_len(MAX_SKILL_MD_SIZE + 1).unwrap();
        assert_eq!(
            inspect_skill_source(&huge.0).unwrap_err().code,
            SkillErrorCode::LimitExceeded
        );
    }

    #[test]
    fn unsafe_names_case_collisions_and_relative_sources_are_rejected() {
        let unicode = TestDir::valid();
        fs::write(unicode.0.join("Résumé.md"), b"x").unwrap();
        assert_eq!(
            inspect_skill_source(&unicode.0).unwrap_err().code,
            SkillErrorCode::UnsafePath
        );

        let collision = TestDir::valid();
        fs::write(collision.0.join("A.txt"), b"first").unwrap();
        fs::write(collision.0.join("a.txt"), b"second").unwrap();
        let case_entries = fs::read_dir(&collision.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("a.txt")
            })
            .count();
        if case_entries == 2 {
            assert_eq!(
                inspect_skill_source(&collision.0).unwrap_err().code,
                SkillErrorCode::UnsafePath
            );
        }

        assert_eq!(
            inspect_skill_source(Path::new("relative/skill"))
                .unwrap_err()
                .code,
            SkillErrorCode::InvalidSource
        );

        let limits = InspectionLimits::default();
        assert_eq!(
            validate_relative_path(Path::new("../escape"), 1, limits)
                .unwrap_err()
                .code,
            SkillErrorCode::UnsafePath
        );
        assert_eq!(
            validate_relative_path(Path::new("/absolute"), 1, limits)
                .unwrap_err()
                .code,
            SkillErrorCode::UnsafePath
        );
    }

    #[test]
    fn hash_uses_path_and_content_not_traversal_order() {
        let first = TestDir::valid();
        fs::write(first.0.join("b.txt"), b"second").unwrap();
        fs::write(first.0.join("a.txt"), b"first").unwrap();
        let second = TestDir::valid();
        fs::write(second.0.join("a.txt"), b"first").unwrap();
        fs::write(second.0.join("b.txt"), b"second").unwrap();
        assert_eq!(
            inspect_skill_source(&first.0).unwrap().summary.content_hash,
            inspect_skill_source(&second.0)
                .unwrap()
                .summary
                .content_hash
        );
    }

    #[test]
    fn errors_do_not_disclose_source_paths_or_content() {
        let dir = TestDir::new();
        let secret = "TOP_SECRET_SKILL_BODY";
        fs::write(dir.0.join("SKILL.md"), secret).unwrap();
        let error = inspect_skill_source(&dir.0).unwrap_err();
        let encoded = serde_json::to_string(&error).unwrap();
        assert!(!encoded.contains(dir.0.to_string_lossy().as_ref()));
        assert!(!encoded.contains(secret));
    }

    #[test]
    fn injected_small_limits_cover_count_size_total_depth_and_path() {
        let count = TestDir::valid();
        fs::write(count.0.join("extra.txt"), b"x").unwrap();
        let limits = InspectionLimits {
            file_count: 1,
            ..InspectionLimits::default()
        };
        assert_eq!(
            inspect_skill_source_inner(&count.0, limits, &|_| {}, &|_| {})
                .unwrap_err()
                .code,
            SkillErrorCode::LimitExceeded
        );

        let file_size = TestDir::valid();
        let limits = InspectionLimits {
            file_size: 8,
            skill_md_size: 1_024,
            ..InspectionLimits::default()
        };
        assert_eq!(
            inspect_skill_source_inner(&file_size.0, limits, &|_| {}, &|_| {})
                .unwrap_err()
                .code,
            SkillErrorCode::LimitExceeded
        );

        let total = TestDir::valid();
        let limits = InspectionLimits {
            total_size: 32,
            ..InspectionLimits::default()
        };
        assert_eq!(
            inspect_skill_source_inner(&total.0, limits, &|_| {}, &|_| {})
                .unwrap_err()
                .code,
            SkillErrorCode::LimitExceeded
        );

        let nested = TestDir::valid();
        fs::create_dir(nested.0.join("one")).unwrap();
        fs::write(nested.0.join("one/file"), b"x").unwrap();
        let limits = InspectionLimits {
            path_depth: 1,
            ..InspectionLimits::default()
        };
        assert_eq!(
            inspect_skill_source_inner(&nested.0, limits, &|_| {}, &|_| {})
                .unwrap_err()
                .code,
            SkillErrorCode::UnsafePath
        );

        let long_path = TestDir::valid();
        fs::write(long_path.0.join("long-name"), b"x").unwrap();
        let limits = InspectionLimits {
            path_bytes: 5,
            ..InspectionLimits::default()
        };
        assert_eq!(
            inspect_skill_source_inner(&long_path.0, limits, &|_| {}, &|_| {})
                .unwrap_err()
                .code,
            SkillErrorCode::UnsafePath
        );
    }

    #[test]
    fn source_replacement_during_scan_is_detected() {
        let dir = TestDir::valid();
        fs::write(dir.0.join("race.txt"), b"original").unwrap();
        let root = dir.0.clone();
        let hook = move |relative: &Path| {
            if relative == Path::new("race.txt") {
                let target = root.join(relative);
                fs::remove_file(&target).unwrap();
                fs::write(target, b"replaced").unwrap();
            }
        };
        assert_eq!(
            inspect_skill_source_inner(&dir.0, InspectionLimits::default(), &hook, &|_| {})
                .unwrap_err()
                .code,
            SkillErrorCode::SourceChanged
        );
    }

    #[test]
    fn same_inode_rewrite_with_restored_mtime_is_detected_by_ctime() {
        use std::io::{Seek, SeekFrom, Write};
        use std::time::Duration;

        let dir = TestDir::valid();
        let target = dir.0.join("same.txt");
        fs::write(&target, b"original").unwrap();
        let original_mtime = fs::metadata(&target).unwrap().modified().unwrap();
        let root = dir.0.clone();
        let hook = move |relative: &Path| {
            if relative == Path::new("same.txt") {
                std::thread::sleep(Duration::from_millis(2));
                let target = root.join(relative);
                let mut file = OpenOptions::new().write(true).open(&target).unwrap();
                file.seek(SeekFrom::Start(0)).unwrap();
                file.write_all(b"replaced").unwrap();
                file.sync_all().unwrap();
                file.set_times(std::fs::FileTimes::new().set_modified(original_mtime))
                    .unwrap();
            }
        };
        assert_eq!(
            inspect_skill_source_inner(&dir.0, InspectionLimits::default(), &hook, &|_| {})
                .unwrap_err()
                .code,
            SkillErrorCode::SourceChanged
        );
    }

    #[test]
    fn ancestor_symlink_swap_is_blocked_by_component_openat() {
        use std::sync::atomic::AtomicBool;

        let outer = TestDir::new();
        let real = outer.0.join("real");
        let held = outer.0.join("held");
        let attacker = outer.0.join("attacker");
        let source = real.join("skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("SKILL.md"),
            "---\nname: Safe\ndescription: Safe source\n---\n",
        )
        .unwrap();
        fs::create_dir(&attacker).unwrap();
        let swapped = AtomicBool::new(false);
        let real_for_hook = real.clone();
        let held_for_hook = held.clone();
        let attacker_for_hook = attacker.clone();
        let hook = |component: &Path| {
            if component == real_for_hook && !swapped.swap(true, Ordering::SeqCst) {
                fs::rename(&real_for_hook, &held_for_hook).unwrap();
                symlink(&attacker_for_hook, &real_for_hook).unwrap();
            }
        };
        assert!(matches!(
            inspect_skill_source_inner(&source, InspectionLimits::default(), &|_| {}, &hook)
                .unwrap_err()
                .code,
            SkillErrorCode::SourceChanged | SkillErrorCode::UnsafePath
        ));
    }

    #[test]
    fn partial_directory_read_error_is_not_treated_as_eof() {
        let calls = std::cell::Cell::new(0);
        let error = collect_dir_names(|| {
            let call = calls.get();
            calls.set(call + 1);
            if call == 0 {
                Ok(Some(std::ffi::OsString::from("SKILL.md")))
            } else {
                Err(SkillManagerError::safe_io())
            }
        })
        .unwrap_err();
        assert_eq!(error.code, SkillErrorCode::IoFailed);
    }

    #[test]
    fn empty_directory_count_is_bounded() {
        let dir = TestDir::valid();
        fs::create_dir(dir.0.join("one")).unwrap();
        fs::create_dir(dir.0.join("two")).unwrap();
        let limits = InspectionLimits {
            directory_count: 1,
            ..InspectionLimits::default()
        };
        assert_eq!(
            inspect_skill_source_inner(&dir.0, limits, &|_| {}, &|_| {})
                .unwrap_err()
                .code,
            SkillErrorCode::LimitExceeded
        );
    }

    #[test]
    fn unix_socket_is_rejected_without_connecting() {
        use std::os::unix::net::UnixListener;

        let dir = TestDir::valid();
        let Ok(_listener) = UnixListener::bind(dir.0.join("socket")) else {
            // Some CI/sandbox profiles deny AF_UNIX bind; FIFO coverage above still verifies
            // that all non-regular file kinds take the shared fail-closed branch.
            return;
        };
        assert_eq!(
            inspect_skill_source(&dir.0).unwrap_err().code,
            SkillErrorCode::UnsupportedFileType
        );
    }
}
