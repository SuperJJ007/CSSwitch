use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_RANGE, LOCATION, RANGE, USER_AGENT};
use reqwest::redirect::Policy;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::archive::{
    package_from_github_files, package_or_bundle_from_github_archive, validate_skill_name,
};
use crate::bundle::{
    bundle_id_for_github, install_validated_bundle, recover_existing_github_bundle,
};
use crate::install::{
    commit_package, inspect_existing_github_install, ExistingGithubInstall, SourceDescriptor,
};
use crate::{
    active_org, error, InstallCommit, InstallError, InstalledPackage, PackageFile, SourceKind,
    ValidatedArchive, GITHUB_BUNDLE_OPERATION_TIMEOUT_SECONDS, MAX_ARCHIVE_BYTES, MAX_BUNDLE_FILES,
    MAX_BUNDLE_TOTAL_BYTES, MAX_FILES, MAX_FILE_BYTES, MAX_PATH_BYTES, MAX_PATH_DEPTH,
    MAX_TOTAL_BYTES,
};

const GITHUB_OPERATION_TIMEOUT: Duration = Duration::from_secs(90);
const GITHUB_BUNDLE_OPERATION_TIMEOUT: Duration =
    Duration::from_secs(GITHUB_BUNDLE_OPERATION_TIMEOUT_SECONDS);
const GITHUB_TREE_BYTES: usize = 16 * 1024 * 1024;
const GITHUB_ARCHIVE_DOWNLOAD_ATTEMPTS: usize = 3;
const DOWNLOAD_PROGRESS_BYTES: usize = 4 * 1024 * 1024;
const GITHUB_RAW_DOWNLOAD_CONCURRENCY: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubSource {
    pub owner: String,
    pub repo: String,
    pub reference: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubPackageSource {
    pub owner: String,
    pub repo: String,
    pub reference: String,
    pub path: String,
}

#[derive(Clone, Debug)]
struct GithubEndpoints {
    api_base: reqwest::Url,
    raw_base: reqwest::Url,
}

impl GithubEndpoints {
    fn production() -> Self {
        Self {
            api_base: reqwest::Url::parse("https://api.github.com/")
                .expect("static GitHub API URL"),
            raw_base: reqwest::Url::parse("https://raw.githubusercontent.com/")
                .expect("static GitHub raw URL"),
        }
    }
}

#[derive(Clone, Debug)]
struct GithubTreeFile {
    repo_path: String,
    relative_path: PathBuf,
    size: usize,
    executable: bool,
}

impl GithubSource {
    fn skill_name(&self) -> Result<String, InstallError> {
        validate_skill_name(self.path.rsplit('/').next().unwrap_or(""))
    }
}

pub fn parse_github_source(raw: &str) -> Result<GithubSource, InstallError> {
    let source = parse_github_package_source(raw)?;
    if source.path.is_empty() {
        return Err(error(
            "INVALID_SOURCE_URL",
            "单 Skill 安装只支持 /owner/repo/tree/ref/path 目录 URL",
            "source_resolution",
        ));
    }
    let source = GithubSource {
        owner: source.owner,
        repo: source.repo,
        reference: source.reference,
        path: source.path,
    };
    source.skill_name()?;
    Ok(source)
}

pub fn parse_github_package_source(raw: &str) -> Result<GithubPackageSource, InstallError> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| {
        error(
            "INVALID_SOURCE_URL",
            "Skill 来源必须是合法 GitHub URL",
            "source_resolution",
        )
    })?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(error(
            "INVALID_SOURCE_URL",
            "只支持不含认证、端口、查询或 fragment 的 https://github.com URL",
            "source_resolution",
        ));
    }
    let encoded = url.path().trim_matches('/').split('/').collect::<Vec<_>>();
    if encoded.len() < 2 || (encoded.len() > 2 && (encoded.len() < 4 || encoded[2] != "tree")) {
        return Err(error(
            "INVALID_SOURCE_URL",
            "只支持 GitHub 仓库根或 /owner/repo/tree/ref/path URL",
            "source_resolution",
        ));
    }
    let owner = percent_decode(encoded[0])?;
    let repo = percent_decode(encoded[1])?
        .trim_end_matches(".git")
        .to_string();
    let reference = if encoded.len() == 2 {
        "HEAD".to_string()
    } else {
        percent_decode(encoded[3])?
    };
    let path_segments = encoded
        .get(4..)
        .unwrap_or_default()
        .iter()
        .map(|segment| percent_decode(segment))
        .collect::<Result<Vec<_>, _>>()?;
    if reference.contains('/') {
        return Err(error(
            "SOURCE_REF_REQUIRES_COMMIT_SHA",
            "包含 / 的 branch/tag 必须改用 40 位 commit SHA URL",
            "source_resolution",
        ));
    }
    if !safe_repo_component(&owner)
        || !safe_repo_component(&repo)
        || !safe_path_component(&reference)
        || path_segments.len() > MAX_PATH_DEPTH
        || path_segments
            .iter()
            .any(|segment| !safe_path_component(segment))
    {
        return Err(error(
            "INVALID_SOURCE_URL",
            "GitHub URL 包含不安全路径组件",
            "source_resolution",
        ));
    }
    let path = path_segments.join("/");
    // Version-1 Science import-origin readers cap this compatibility field at 500 bytes.
    if path.len() > 500 {
        return Err(error(
            "INVALID_SOURCE_URL",
            "GitHub Skill 子目录超过 marker 兼容长度限制",
            "source_resolution",
        ));
    }
    Ok(GithubPackageSource {
        owner,
        repo,
        reference,
        path,
    })
}

pub fn install_github_package(
    data_dir: &Path,
    source_url: &str,
) -> Result<InstalledPackage, InstallError> {
    let mut progress = |_: &str, _: &str| {};
    install_github_package_with_progress(data_dir, source_url, &mut progress)
}

pub fn install_github_package_with_progress(
    data_dir: &Path,
    source_url: &str,
    progress: &mut dyn FnMut(&str, &str),
) -> Result<InstalledPackage, InstallError> {
    install_github_package_with_endpoints_and_progress(
        data_dir,
        source_url,
        &GithubEndpoints::production(),
        progress,
    )
}

#[cfg(test)]
fn install_github_package_with_endpoints(
    data_dir: &Path,
    source_url: &str,
    endpoints: &GithubEndpoints,
) -> Result<InstalledPackage, InstallError> {
    let mut progress = |_: &str, _: &str| {};
    install_github_package_with_endpoints_and_progress(
        data_dir,
        source_url,
        endpoints,
        &mut progress,
    )
}

fn install_github_package_with_endpoints_and_progress(
    data_dir: &Path,
    source_url: &str,
    endpoints: &GithubEndpoints,
    progress: &mut dyn FnMut(&str, &str),
) -> Result<InstalledPackage, InstallError> {
    progress("source_resolution", "正在解析 GitHub 来源并固定 commit");
    let source = parse_github_package_source(source_url)?;
    let initial_org = active_org(data_dir)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(Policy::none())
        .build()
        .map_err(|_| {
            error(
                "GITHUB_CLIENT_FAILED",
                "初始化 GitHub archive 下载客户端失败",
                "source_resolution",
            )
        })?;
    let deadline = Instant::now() + GITHUB_BUNDLE_OPERATION_TIMEOUT;
    let commit = resolve_package_commit(&client, &source, endpoints, deadline)?;
    let source_repo = format!("{}/{}", source.owner, source.repo);
    progress("recovery", "正在检查同一来源的已验证安装与中断事务");
    if let Some(bundle) =
        recover_existing_github_bundle(data_dir, &initial_org, &source_repo, &commit, &source.path)?
    {
        progress("commit", "已验证现有 bundle，无需重复下载或写入");
        return Ok(InstalledPackage::Bundle(bundle));
    }
    progress("download", "正在下载 GitHub repository archive");
    let archive =
        download_repository_archive(&client, &source, &commit, endpoints, deadline, progress)?;
    progress(
        "validation",
        "下载完成，正在校验 archive、bundle 成员和支持资源",
    );
    match package_or_bundle_from_github_archive(&archive, &source.path, &source.repo)? {
        ValidatedArchive::Skill(package) => {
            let descriptor = SourceDescriptor {
                kind: SourceKind::Github,
                repo: source_repo,
                sha: commit,
                path: source.path,
                archive_sha256: None,
            };
            progress("commit", "校验完成，正在原子提交单个 Skill");
            commit_package(data_dir, package, descriptor, &initial_org).map(InstalledPackage::Skill)
        }
        ValidatedArchive::Bundle(bundle) => {
            let collection_path = bundle.collection_path.clone();
            let descriptor = SourceDescriptor {
                kind: SourceKind::Github,
                repo: source_repo.clone(),
                sha: commit,
                path: collection_path.clone(),
                archive_sha256: None,
            };
            let bundle_id = bundle_id_for_github(&source_repo, &collection_path);
            progress("commit", "校验完成，正在原子提交完整 bundle");
            install_validated_bundle(data_dir, bundle, descriptor, &initial_org, bundle_id)
                .map(InstalledPackage::Bundle)
        }
    }
}

pub fn install_github_skill(
    data_dir: &Path,
    source_url: &str,
) -> Result<InstallCommit, InstallError> {
    install_github_skill_with_endpoints(data_dir, source_url, &GithubEndpoints::production())
}

fn install_github_skill_with_endpoints(
    data_dir: &Path,
    source_url: &str,
    endpoints: &GithubEndpoints,
) -> Result<InstallCommit, InstallError> {
    let source = parse_github_source(source_url)?;
    let initial_org = active_org(data_dir)?;
    let skill_name = source.skill_name()?;
    let source_repo = format!("{}/{}", source.owner, source.repo);
    let requested_commit =
        is_commit_sha(&source.reference).then(|| source.reference.to_ascii_lowercase());
    let existing = inspect_existing_github_install(
        data_dir,
        &initial_org,
        &skill_name,
        &source_repo,
        &source.path,
        requested_commit.as_deref(),
    )?;
    if let ExistingGithubInstall::Verified(commit) = existing {
        return Ok(commit);
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .redirect(Policy::none())
        .build()
        .map_err(|_| {
            error(
                "GITHUB_CLIENT_FAILED",
                "初始化 GitHub 下载客户端失败",
                "source_resolution",
            )
        })?;
    let deadline = Instant::now() + GITHUB_OPERATION_TIMEOUT;
    let (descriptor, legacy_recovery) = match existing {
        ExistingGithubInstall::Legacy(descriptor) => (descriptor, true),
        ExistingGithubInstall::Missing => (
            SourceDescriptor {
                kind: SourceKind::Github,
                repo: source_repo,
                sha: resolve_commit(&client, &source, endpoints, deadline)?,
                path: source.path.clone(),
                archive_sha256: None,
            },
            false,
        ),
        ExistingGithubInstall::Verified(_) => unreachable!("returned above"),
    };
    let package = download_skill_tree(&client, &source, &descriptor.sha, endpoints, deadline)
        .map_err(|cause| legacy_recovery_error(cause, legacy_recovery))?;
    commit_package(data_dir, package, descriptor, &initial_org)
}

fn legacy_recovery_error(cause: InstallError, legacy_recovery: bool) -> InstallError {
    if !legacy_recovery {
        return cause;
    }
    error(
        "LEGACY_INTEGRITY_UNVERIFIED",
        format!(
            "旧 GitHub marker 无法通过固定 commit/path 重新验证：{}",
            cause.message
        ),
        "recovery",
    )
    .retryable(cause.retryable)
}

fn resolve_commit(
    client: &Client,
    source: &GithubSource,
    endpoints: &GithubEndpoints,
    deadline: Instant,
) -> Result<String, InstallError> {
    if is_commit_sha(&source.reference) {
        return Ok(source.reference.to_ascii_lowercase());
    }
    let mut url = endpoints.api_base.clone();
    url.path_segments_mut()
        .map_err(|_| error("INVALID_SOURCE_URL", "GitHub ref 非法", "source_resolution"))?
        .pop_if_empty()
        .push("repos")
        .push(&source.owner)
        .push(&source.repo)
        .push("commits")
        .push(&source.reference);
    let response = github_get(client, url, "source_resolution")?;
    let body = read_response_limited(response, 1024 * 1024, "source_resolution", deadline)?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| {
        error(
            "GITHUB_RESPONSE_INVALID",
            "GitHub commit 响应不是合法 JSON",
            "source_resolution",
        )
    })?;
    let sha = value.get("sha").and_then(Value::as_str).unwrap_or("");
    if !is_commit_sha(sha) {
        return Err(error(
            "GITHUB_RESPONSE_INVALID",
            "GitHub commit 响应缺少完整 SHA",
            "source_resolution",
        ));
    }
    Ok(sha.to_ascii_lowercase())
}

fn resolve_package_commit(
    client: &Client,
    source: &GithubPackageSource,
    endpoints: &GithubEndpoints,
    deadline: Instant,
) -> Result<String, InstallError> {
    if is_commit_sha(&source.reference) {
        return Ok(source.reference.to_ascii_lowercase());
    }
    let mut url = endpoints.api_base.clone();
    url.path_segments_mut()
        .map_err(|_| error("INVALID_SOURCE_URL", "GitHub ref 非法", "source_resolution"))?
        .pop_if_empty()
        .push("repos")
        .push(&source.owner)
        .push(&source.repo)
        .push("commits")
        .push(&source.reference);
    let response = github_get(client, url, "source_resolution")?;
    let body = read_response_limited(response, 1024 * 1024, "source_resolution", deadline)?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| {
        error(
            "GITHUB_RESPONSE_INVALID",
            "GitHub commit 响应不是合法 JSON",
            "source_resolution",
        )
    })?;
    let sha = value.get("sha").and_then(Value::as_str).unwrap_or("");
    if !is_commit_sha(sha) {
        return Err(error(
            "GITHUB_RESPONSE_INVALID",
            "GitHub commit 响应缺少完整 SHA",
            "source_resolution",
        ));
    }
    Ok(sha.to_ascii_lowercase())
}

fn download_repository_archive(
    client: &Client,
    source: &GithubPackageSource,
    commit: &str,
    endpoints: &GithubEndpoints,
    deadline: Instant,
    progress: &mut dyn FnMut(&str, &str),
) -> Result<Vec<u8>, InstallError> {
    let mut zipball = endpoints.api_base.clone();
    zipball
        .path_segments_mut()
        .map_err(|_| error("INVALID_SOURCE_URL", "GitHub archive URL 非法", "download"))?
        .pop_if_empty()
        .push("repos")
        .push(&source.owner)
        .push(&source.repo)
        .push("zipball")
        .push(commit);
    let response = client
        .get(zipball)
        .header(USER_AGENT, "CSSwitch-Skill-Installer/0.3")
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .map_err(|problem| {
            github_transport_error(problem, "download", "GitHub archive 请求失败")
        })?;
    classify_github_status(&response, "download")?;
    if response.status().as_u16() != 302 {
        return Err(error(
            "GITHUB_REDIRECT_INVALID",
            format!("GitHub zipball 未返回预期 302：HTTP {}", response.status()),
            "download",
        ));
    }
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| reqwest::Url::parse(value).ok())
        .ok_or_else(|| {
            error(
                "GITHUB_REDIRECT_INVALID",
                "GitHub zipball 缺少合法 Location",
                "download",
            )
        })?;
    validate_codeload_location(&location, endpoints)?;
    let bytes = match download_codeload_archive(client, &location, deadline, progress) {
        Ok(bytes) => bytes,
        Err(problem)
            if matches!(
                problem.code.as_str(),
                "GITHUB_ARCHIVE_STREAM_UNBOUNDED" | "GITHUB_RANGE_UNSUPPORTED"
            ) =>
        {
            progress(
                "download_fallback",
                "GitHub archive 长连接不可安全续传，正在切换到 tree/raw 文件级下载",
            );
            download_repository_tree_archive(client, source, commit, endpoints, deadline, progress)?
        }
        Err(problem) => return Err(problem),
    };
    if !bytes.starts_with(b"PK") {
        return Err(error(
            "INVALID_ARCHIVE",
            "GitHub codeload 返回的内容不是 ZIP archive",
            "download",
        ));
    }
    Ok(bytes)
}

fn download_codeload_archive(
    client: &Client,
    location: &reqwest::Url,
    deadline: Instant,
    progress: &mut dyn FnMut(&str, &str),
) -> Result<Vec<u8>, InstallError> {
    let mut bytes = Vec::new();
    let mut last_error = None;
    for attempt in 1..=GITHUB_ARCHIVE_DOWNLOAD_ATTEMPTS {
        ensure_deadline(deadline, "download")?;
        let requested_offset = bytes.len();
        let mut request = client
            .get(location.clone())
            .header(USER_AGENT, "CSSwitch-Skill-Installer/0.3")
            .header(ACCEPT, "application/zip")
            .header(ACCEPT_ENCODING, "identity");
        if requested_offset > 0 {
            request = request.header(RANGE, format!("bytes={requested_offset}-"));
        }
        let response = match request.send() {
            Ok(response) => response,
            Err(problem) => {
                let problem =
                    github_transport_error(problem, "download", "GitHub codeload 下载失败");
                if problem.retryable
                    && attempt < GITHUB_ARCHIVE_DOWNLOAD_ATTEMPTS
                    && Instant::now() < deadline
                {
                    let downloaded_mib = bytes.len() as f64 / (1024.0 * 1024.0);
                    progress(
                        "download",
                        &format!(
                            "GitHub 下载连接建立失败，正在从 {downloaded_mib:.1} MiB 重连（连接 {}/{GITHUB_ARCHIVE_DOWNLOAD_ATTEMPTS}）",
                            attempt + 1
                        ),
                    );
                    last_error = Some(problem);
                    continue;
                }
                return Err(problem);
            }
        };
        classify_github_status(&response, "download")?;
        let status = response.status();
        if requested_offset == 0 && status.as_u16() == 200 && response.content_length().is_none() {
            return Err(error(
                "GITHUB_ARCHIVE_STREAM_UNBOUNDED",
                "GitHub codeload 未提供可验证长度，改用文件级下载",
                "download",
            )
            .retryable(true));
        }
        let total_size = if requested_offset > 0 && status.as_u16() == 206 {
            let (range_start, total_size) = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range)
                .ok_or_else(|| {
                    error(
                        "GITHUB_RANGE_INVALID",
                        "GitHub codeload 续传响应缺少合法 Content-Range",
                        "download",
                    )
                })?;
            if range_start != requested_offset {
                return Err(error(
                    "GITHUB_RANGE_INVALID",
                    "GitHub codeload 续传偏移与本地已下载字节不一致",
                    "download",
                ));
            }
            total_size
        } else if status.is_success() && status.as_u16() == 200 {
            if requested_offset > 0 {
                return Err(error(
                    "GITHUB_RANGE_UNSUPPORTED",
                    "GitHub codeload 不支持 Range 续传，改用文件级下载",
                    "download",
                )
                .retryable(true));
            }
            response
                .content_length()
                .and_then(|length| usize::try_from(length).ok())
        } else {
            return Err(error(
                "GITHUB_DOWNLOAD_FAILED",
                format!("GitHub codeload 返回 HTTP {status}"),
                "download",
            )
            .retryable(status.is_server_error()));
        };
        match read_response_into_limited_with_progress(
            response,
            MAX_ARCHIVE_BYTES,
            "download",
            deadline,
            total_size,
            &mut bytes,
            progress,
        ) {
            Ok(()) => return Ok(bytes),
            Err(_) if complete_zip_archive(&bytes) => {
                let downloaded_mib = bytes.len() as f64 / (1024.0 * 1024.0);
                progress(
                    "download",
                    &format!(
                        "GitHub 连接异常结束，但已完整接收并校验 archive：{downloaded_mib:.1} MiB"
                    ),
                );
                return Ok(bytes);
            }
            Err(problem)
                if problem.retryable
                    && attempt < GITHUB_ARCHIVE_DOWNLOAD_ATTEMPTS
                    && Instant::now() < deadline =>
            {
                let downloaded_mib = bytes.len() as f64 / (1024.0 * 1024.0);
                progress(
                    "download",
                    &format!(
                        "GitHub 下载连接中断，正在从 {downloaded_mib:.1} MiB 尝试续传（连接 {}/{GITHUB_ARCHIVE_DOWNLOAD_ATTEMPTS}）",
                        attempt + 1
                    ),
                );
                last_error = Some(problem);
            }
            Err(problem) => return Err(problem),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        error(
            "GITHUB_NETWORK_ERROR",
            "GitHub codeload 下载在受限重连后仍未完成",
            "download",
        )
        .retryable(true)
    }))
}

fn download_repository_tree_archive(
    client: &Client,
    source: &GithubPackageSource,
    commit: &str,
    endpoints: &GithubEndpoints,
    deadline: Instant,
    progress: &mut dyn FnMut(&str, &str),
) -> Result<Vec<u8>, InstallError> {
    let mut tree_url = endpoints.api_base.clone();
    tree_url
        .path_segments_mut()
        .map_err(|_| {
            error(
                "INVALID_SOURCE_URL",
                "GitHub tree URL 非法",
                "download_fallback",
            )
        })?
        .pop_if_empty()
        .push("repos")
        .push(&source.owner)
        .push(&source.repo)
        .push("git")
        .push("trees")
        .push(commit);
    tree_url.query_pairs_mut().append_pair("recursive", "1");
    let response = github_get(client, tree_url, "download_fallback")?;
    let body = read_response_limited(response, GITHUB_TREE_BYTES, "download_fallback", deadline)?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| {
        error(
            "GITHUB_RESPONSE_INVALID",
            "GitHub tree 响应不是合法 JSON",
            "download_fallback",
        )
    })?;
    if value.get("truncated").and_then(Value::as_bool) != Some(false) {
        return Err(error(
            "GITHUB_TREE_TRUNCATED",
            "GitHub tree 响应被截断，无法安全确认完整 bundle",
            "download_fallback",
        ));
    }
    let tree = value.get("tree").and_then(Value::as_array).ok_or_else(|| {
        error(
            "GITHUB_RESPONSE_INVALID",
            "GitHub tree 响应缺少条目列表",
            "download_fallback",
        )
    })?;
    let prefix = (!source.path.is_empty()).then(|| format!("{}/", source.path));
    let mut files = Vec::new();
    let mut total = 0usize;
    for entry in tree {
        let Some(repo_path) = entry.get("path").and_then(Value::as_str) else {
            return Err(error(
                "GITHUB_RESPONSE_INVALID",
                "GitHub tree 条目缺少路径",
                "download_fallback",
            ));
        };
        let relative = match prefix.as_deref() {
            Some(prefix) if repo_path.starts_with(prefix) => &repo_path[prefix.len()..],
            Some(_) => continue,
            None => repo_path,
        };
        if relative.is_empty() || ignored_github_metadata(relative) {
            continue;
        }
        validate_github_relative_path(repo_path)?;
        validate_github_relative_path(relative)?;
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let mode = entry.get("mode").and_then(Value::as_str).unwrap_or("");
        if entry_type == "tree" && mode == "040000" {
            continue;
        }
        if entry_type != "blob" || !matches!(mode, "100644" | "100755") {
            return Err(error(
                "UNSUPPORTED_ARCHIVE_ENTRY",
                format!("GitHub bundle 包含链接、子模块或特殊条目：{relative}"),
                "download_fallback",
            ));
        }
        let size = entry
            .get("size")
            .and_then(Value::as_u64)
            .and_then(|size| usize::try_from(size).ok())
            .ok_or_else(|| {
                error(
                    "GITHUB_RESPONSE_INVALID",
                    format!("GitHub blob 缺少合法大小：{relative}"),
                    "download_fallback",
                )
            })?;
        if size > MAX_FILE_BYTES {
            return Err(error(
                "FILE_TOO_LARGE",
                format!("bundle 文件超过 4 MiB：{relative}"),
                "download_fallback",
            ));
        }
        total = total.checked_add(size).ok_or_else(|| {
            error(
                "BUNDLE_LIMIT_EXCEEDED",
                "bundle 总大小溢出",
                "download_fallback",
            )
        })?;
        if total > MAX_BUNDLE_TOTAL_BYTES || files.len() >= MAX_BUNDLE_FILES {
            return Err(error(
                "BUNDLE_LIMIT_EXCEEDED",
                "bundle 超过 2000 文件或 64 MiB 限制",
                "download_fallback",
            ));
        }
        files.push(GithubTreeFile {
            repo_path: repo_path.to_string(),
            relative_path: PathBuf::from(relative),
            size,
            executable: mode == "100755",
        });
    }
    if files.is_empty() {
        return Err(error(
            "GITHUB_NOT_FOUND",
            "GitHub tree 中没有找到可安装文件",
            "download_fallback",
        ));
    }
    files.sort_by(|left, right| left.repo_path.cmp(&right.repo_path));
    let total_mib = total as f64 / (1024.0 * 1024.0);
    progress(
        "download_fallback",
        &format!(
            "已确认 {} 个文件、{total_mib:.1} MiB，正在逐文件下载并校验",
            files.len()
        ),
    );

    let archive_root = format!("{}-{}/", source.repo, &commit[..12]);
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    let mut downloaded = 0usize;
    let mut next_progress = DOWNLOAD_PROGRESS_BYTES;
    let mut completed_files = 0usize;
    for batch in files.chunks(GITHUB_RAW_DOWNLOAD_CONCURRENCY) {
        ensure_deadline(deadline, "download_fallback")?;
        let contents: Result<Vec<Vec<u8>>, InstallError> = std::thread::scope(|scope| {
            let handles = batch
                .iter()
                .map(|file| {
                    scope.spawn(move || {
                        download_github_raw_file(client, source, commit, endpoints, deadline, file)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        Err(error(
                            "GITHUB_DOWNLOAD_FAILED",
                            "GitHub raw 下载线程异常终止",
                            "download_fallback",
                        )
                        .retryable(true))
                    })
                })
                .collect()
        });
        for (file, content) in batch.iter().zip(contents?) {
            let mode = if file.executable { 0o755 } else { 0o644 };
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(mode);
            writer
                .start_file(format!("{archive_root}{}", file.repo_path), options)
                .map_err(|_| {
                    error(
                        "ARCHIVE_BUILD_FAILED",
                        "无法构建受验证的 GitHub bundle archive",
                        "download_fallback",
                    )
                })?;
            writer.write_all(&content).map_err(|_| {
                error(
                    "ARCHIVE_BUILD_FAILED",
                    "无法写入受验证的 GitHub bundle archive",
                    "download_fallback",
                )
            })?;
            completed_files = completed_files.saturating_add(1);
            downloaded = downloaded.saturating_add(content.len());
            if downloaded >= next_progress || completed_files == files.len() {
                let downloaded_mib = downloaded as f64 / (1024.0 * 1024.0);
                progress(
                    "download_fallback",
                    &format!(
                        "文件级下载：{downloaded_mib:.1} / {total_mib:.1} MiB（{completed_files}/{} 个文件）",
                        files.len()
                    ),
                );
                next_progress = downloaded.saturating_add(DOWNLOAD_PROGRESS_BYTES);
            }
        }
    }
    let archive = writer
        .finish()
        .map_err(|_| {
            error(
                "ARCHIVE_BUILD_FAILED",
                "无法完成受验证的 GitHub bundle archive",
                "download_fallback",
            )
        })?
        .into_inner();
    if archive.len() > MAX_ARCHIVE_BYTES {
        return Err(error(
            "GITHUB_RESPONSE_TOO_LARGE",
            "重建的 GitHub archive 超过 128 MiB 限制",
            "download_fallback",
        ));
    }
    progress(
        "validation",
        "文件级下载完成，正在执行同一 bundle archive 安全校验",
    );
    Ok(archive)
}

fn download_github_raw_file(
    client: &Client,
    source: &GithubPackageSource,
    commit: &str,
    endpoints: &GithubEndpoints,
    deadline: Instant,
    file: &GithubTreeFile,
) -> Result<Vec<u8>, InstallError> {
    ensure_deadline(deadline, "download_fallback")?;
    let mut raw_url = endpoints.raw_base.clone();
    {
        let mut segments = raw_url.path_segments_mut().map_err(|_| {
            error(
                "INVALID_SOURCE_URL",
                "GitHub raw URL 非法",
                "download_fallback",
            )
        })?;
        segments
            .pop_if_empty()
            .push(&source.owner)
            .push(&source.repo)
            .push(commit);
        for component in file.repo_path.split('/') {
            segments.push(component);
        }
    }
    let response = client
        .get(raw_url)
        .header(USER_AGENT, "CSSwitch-Skill-Installer/0.3")
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .map_err(|problem| {
            github_transport_error(problem, "download_fallback", "GitHub raw 文件下载失败")
        })?;
    classify_github_status(&response, "download_fallback")?;
    if !response.status().is_success() {
        return Err(error(
            "GITHUB_DOWNLOAD_FAILED",
            format!("GitHub raw 返回 HTTP {}", response.status()),
            "download_fallback",
        )
        .retryable(response.status().is_server_error()));
    }
    let content = read_response_limited(response, MAX_FILE_BYTES, "download_fallback", deadline)?;
    if content.len() != file.size {
        return Err(error(
            "GITHUB_RESPONSE_INVALID",
            format!(
                "GitHub raw 文件大小与 tree 元数据不一致：{}",
                file.repo_path
            ),
            "download_fallback",
        ));
    }
    Ok(content)
}

fn complete_zip_archive(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK") && zip::ZipArchive::new(std::io::Cursor::new(bytes)).is_ok()
}

fn parse_content_range(value: &str) -> Option<(usize, Option<usize>)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<usize>().ok()?;
    let end = end.parse::<usize>().ok()?;
    if end < start {
        return None;
    }
    let total = if total == "*" {
        None
    } else {
        Some(total.parse::<usize>().ok()?)
    };
    if total.is_some_and(|total| end >= total) {
        return None;
    }
    Some((start, total))
}

fn validate_codeload_location(
    location: &reqwest::Url,
    endpoints: &GithubEndpoints,
) -> Result<(), InstallError> {
    let production = endpoints.api_base.host_str() == Some("api.github.com");
    let valid = if production {
        location.scheme() == "https"
            && location.host_str() == Some("codeload.github.com")
            && location.port().is_none()
    } else {
        location.scheme() == endpoints.api_base.scheme()
            && location.host_str() == endpoints.api_base.host_str()
            && location.port_or_known_default() == endpoints.api_base.port_or_known_default()
    };
    if !valid
        || !location.username().is_empty()
        || location.password().is_some()
        || location.fragment().is_some()
    {
        return Err(error(
            "GITHUB_REDIRECT_INVALID",
            "GitHub archive 重定向不在受信 codeload 主机",
            "download",
        ));
    }
    Ok(())
}

fn download_skill_tree(
    client: &Client,
    source: &GithubSource,
    commit: &str,
    endpoints: &GithubEndpoints,
    deadline: Instant,
) -> Result<crate::ValidatedPackage, InstallError> {
    let mut tree_url = endpoints.api_base.clone();
    tree_url
        .path_segments_mut()
        .map_err(|_| error("INVALID_SOURCE_URL", "GitHub tree URL 非法", "download"))?
        .pop_if_empty()
        .push("repos")
        .push(&source.owner)
        .push(&source.repo)
        .push("git")
        .push("trees")
        .push(commit);
    tree_url.query_pairs_mut().append_pair("recursive", "1");
    let response = github_get(client, tree_url, "download")?;
    let body = read_response_limited(response, GITHUB_TREE_BYTES, "download", deadline)?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| {
        error(
            "GITHUB_RESPONSE_INVALID",
            "GitHub tree 响应不是合法 JSON",
            "download",
        )
    })?;
    if value.get("truncated").and_then(Value::as_bool) != Some(false) {
        return Err(error(
            "GITHUB_TREE_TRUNCATED",
            "GitHub tree 响应被截断，无法安全确认完整 Skill 子树",
            "download",
        ));
    }
    let tree = value.get("tree").and_then(Value::as_array).ok_or_else(|| {
        error(
            "GITHUB_RESPONSE_INVALID",
            "GitHub tree 响应缺少条目列表",
            "download",
        )
    })?;
    let prefix = format!("{}/", source.path.trim_matches('/'));
    let mut files = Vec::new();
    let mut total = 0usize;
    for entry in tree {
        let Some(repo_path) = entry.get("path").and_then(Value::as_str) else {
            return Err(error(
                "GITHUB_RESPONSE_INVALID",
                "GitHub tree 条目缺少路径",
                "download",
            ));
        };
        if !repo_path.starts_with(&prefix) {
            continue;
        }
        let relative = &repo_path[prefix.len()..];
        if relative.is_empty() || ignored_github_metadata(relative) {
            continue;
        }
        validate_github_relative_path(relative)?;
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let mode = entry.get("mode").and_then(Value::as_str).unwrap_or("");
        if entry_type == "tree" && mode == "040000" {
            continue;
        }
        if entry_type != "blob" || !matches!(mode, "100644" | "100755") {
            return Err(error(
                "UNSUPPORTED_ARCHIVE_ENTRY",
                format!("GitHub Skill 包含链接、子模块或特殊条目：{relative}"),
                "download",
            ));
        }
        let size = entry
            .get("size")
            .and_then(Value::as_u64)
            .and_then(|size| usize::try_from(size).ok())
            .ok_or_else(|| {
                error(
                    "GITHUB_RESPONSE_INVALID",
                    format!("GitHub blob 缺少合法大小：{relative}"),
                    "download",
                )
            })?;
        if size > MAX_FILE_BYTES {
            return Err(error(
                "FILE_TOO_LARGE",
                format!("Skill 文件超过 4 MiB：{relative}"),
                "download",
            ));
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| error("PACKAGE_TOO_LARGE", "Skill 总大小溢出", "download"))?;
        if total > MAX_TOTAL_BYTES || files.len() >= MAX_FILES {
            return Err(error(
                "PACKAGE_LIMIT_EXCEEDED",
                "Skill 超过 512 文件或 32 MiB 限制",
                "download",
            ));
        }
        files.push(GithubTreeFile {
            repo_path: repo_path.to_string(),
            relative_path: PathBuf::from(relative),
            size,
            executable: mode == "100755",
        });
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let mut package_files = Vec::with_capacity(files.len());
    for file in files {
        ensure_deadline(deadline, "download")?;
        let mut raw_url = endpoints.raw_base.clone();
        {
            let mut segments = raw_url
                .path_segments_mut()
                .map_err(|_| error("INVALID_SOURCE_URL", "GitHub raw URL 非法", "download"))?;
            segments
                .pop_if_empty()
                .push(&source.owner)
                .push(&source.repo)
                .push(commit);
            for component in file.repo_path.split('/') {
                segments.push(component);
            }
        }
        let response = client
            .get(raw_url)
            .header(USER_AGENT, "CSSwitch-Skill-Installer/0.2")
            .send()
            .map_err(|problem| {
                github_transport_error(problem, "download", "GitHub raw 文件下载失败")
            })?;
        classify_github_status(&response, "download")?;
        if !response.status().is_success() {
            return Err(error(
                "GITHUB_DOWNLOAD_FAILED",
                format!("GitHub raw 返回 HTTP {}", response.status()),
                "download",
            )
            .retryable(response.status().is_server_error()));
        }
        let content = read_response_limited(response, MAX_FILE_BYTES, "download", deadline)?;
        if content.len() != file.size {
            return Err(error(
                "GITHUB_RESPONSE_INVALID",
                format!(
                    "GitHub raw 文件大小与 tree 元数据不一致：{}",
                    file.repo_path
                ),
                "download",
            ));
        }
        package_files.push(PackageFile {
            path: file.relative_path,
            content,
            executable: file.executable,
        });
    }
    package_from_github_files(&source.skill_name()?, package_files)
}

fn github_get(client: &Client, url: reqwest::Url, phase: &str) -> Result<Response, InstallError> {
    let response = client
        .get(url)
        .header(USER_AGENT, "CSSwitch-Skill-Installer/0.2")
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .map_err(|problem| github_transport_error(problem, phase, "GitHub API 请求失败"))?;
    classify_github_status(&response, phase)?;
    if !response.status().is_success() {
        return Err(error(
            "GITHUB_RESPONSE_INVALID",
            format!("GitHub API 返回 HTTP {}", response.status()),
            phase,
        ));
    }
    Ok(response)
}

fn classify_github_status(response: &Response, phase: &str) -> Result<(), InstallError> {
    let status = response.status().as_u16();
    let rate_limited = matches!(status, 403 | 429)
        && (response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            == Some("0")
            || response.headers().contains_key("retry-after")
            || status == 429);
    if rate_limited {
        return Err(error(
            "GITHUB_RATE_LIMITED",
            "GitHub 匿名访问额度已耗尽，请稍后重试",
            phase,
        )
        .retryable(true));
    }
    match status {
        403 => Err(error(
            "GITHUB_PERMISSION_DENIED",
            "GitHub 拒绝匿名访问该来源",
            phase,
        )),
        404 => Err(error(
            "GITHUB_NOT_FOUND",
            "GitHub 仓库、ref 或 archive 不存在；私有仓库不受支持",
            phase,
        )),
        _ => Ok(()),
    }
}

fn read_response_limited(
    response: Response,
    limit: usize,
    phase: &str,
    deadline: Instant,
) -> Result<Vec<u8>, InstallError> {
    let mut progress = |_: &str, _: &str| {};
    read_response_limited_with_progress(response, limit, phase, deadline, &mut progress)
}

fn read_response_limited_with_progress(
    response: Response,
    limit: usize,
    phase: &str,
    deadline: Instant,
    progress: &mut dyn FnMut(&str, &str),
) -> Result<Vec<u8>, InstallError> {
    let total_size = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok());
    let mut bytes = Vec::new();
    read_response_into_limited_with_progress(
        response, limit, phase, deadline, total_size, &mut bytes, progress,
    )?;
    Ok(bytes)
}

fn read_response_into_limited_with_progress(
    response: Response,
    limit: usize,
    phase: &str,
    deadline: Instant,
    total_size: Option<usize>,
    bytes: &mut Vec<u8>,
    progress: &mut dyn FnMut(&str, &str),
) -> Result<(), InstallError> {
    if bytes.len() > limit || total_size.is_some_and(|length| length > limit) {
        return Err(error(
            "GITHUB_RESPONSE_TOO_LARGE",
            "GitHub 响应超过大小限制",
            phase,
        ));
    }
    let remaining_limit = limit.saturating_sub(bytes.len()).saturating_add(1);
    let mut response = response.take(remaining_limit as u64);
    let mut chunk = [0_u8; 16 * 1024];
    let mut next_progress_bytes = bytes
        .len()
        .checked_div(DOWNLOAD_PROGRESS_BYTES)
        .and_then(|multiple| multiple.checked_add(1))
        .and_then(|multiple| multiple.checked_mul(DOWNLOAD_PROGRESS_BYTES))
        .unwrap_or(usize::MAX);
    loop {
        ensure_deadline(deadline, phase)?;
        let count = response.read(&mut chunk).map_err(|problem| {
            error(
                if problem.kind() == std::io::ErrorKind::TimedOut {
                    "GITHUB_TIMEOUT"
                } else {
                    "GITHUB_NETWORK_ERROR"
                },
                "读取 GitHub 响应失败",
                phase,
            )
            .retryable(true)
        })?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if phase == "download" && bytes.len() >= next_progress_bytes {
            let downloaded_mib = bytes.len() as f64 / (1024.0 * 1024.0);
            let message = total_size.map_or_else(
                || format!("正在下载 GitHub repository archive：{downloaded_mib:.1} MiB"),
                |total| {
                    let total_mib = total as f64 / (1024.0 * 1024.0);
                    format!(
                        "正在下载 GitHub repository archive：{downloaded_mib:.1} / {total_mib:.1} MiB"
                    )
                },
            );
            progress("download", &message);
            next_progress_bytes = bytes
                .len()
                .checked_div(DOWNLOAD_PROGRESS_BYTES)
                .and_then(|multiple| multiple.checked_add(1))
                .and_then(|multiple| multiple.checked_mul(DOWNLOAD_PROGRESS_BYTES))
                .unwrap_or(usize::MAX);
        }
        if bytes.len() > limit {
            return Err(error(
                "GITHUB_RESPONSE_TOO_LARGE",
                "GitHub 响应超过大小限制",
                phase,
            ));
        }
    }
    ensure_deadline(deadline, phase)?;
    if phase == "download" {
        let downloaded_mib = bytes.len() as f64 / (1024.0 * 1024.0);
        progress(
            "download",
            &format!("GitHub repository archive 下载完成：{downloaded_mib:.1} MiB"),
        );
    }
    Ok(())
}

fn github_transport_error(problem: reqwest::Error, phase: &str, message: &str) -> InstallError {
    error(
        if problem.is_timeout() {
            "GITHUB_TIMEOUT"
        } else {
            "GITHUB_NETWORK_ERROR"
        },
        message,
        phase,
    )
    .retryable(true)
}

fn ensure_deadline(deadline: Instant, phase: &str) -> Result<(), InstallError> {
    if Instant::now() >= deadline {
        return Err(
            error("GITHUB_TIMEOUT", "GitHub Skill 下载超过允许的总时限", phase).retryable(true),
        );
    }
    Ok(())
}

fn ignored_github_metadata(path: &str) -> bool {
    path.split('/').any(|component| component == "__MACOSX")
        || path.rsplit('/').next() == Some(".DS_Store")
}

fn validate_github_relative_path(path: &str) -> Result<(), InstallError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.len() > MAX_PATH_BYTES
        || path.nfc().collect::<String>() != path
    {
        return Err(error(
            "UNSAFE_ARCHIVE_PATH",
            "GitHub Skill 包含绝对、反斜杠、NUL、非 NFC 或过长路径",
            "download",
        ));
    }
    let components = path.split('/').collect::<Vec<_>>();
    if components.len() > MAX_PATH_DEPTH
        || components
            .iter()
            .any(|component| component.is_empty() || matches!(*component, "." | ".."))
    {
        return Err(error(
            "UNSAFE_ARCHIVE_PATH",
            "GitHub Skill 包含越界、空组件或过深路径",
            "download",
        ));
    }
    Ok(())
}

fn is_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn safe_repo_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

fn safe_path_component(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
}

fn percent_decode(value: &str) -> Result<String, InstallError> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(error(
                    "INVALID_SOURCE_URL",
                    "GitHub URL 百分号编码非法",
                    "source_resolution",
                ));
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).map_err(|_| {
                error(
                    "INVALID_SOURCE_URL",
                    "GitHub URL 编码非法",
                    "source_resolution",
                )
            })?;
            output.push(u8::from_str_radix(hex, 16).map_err(|_| {
                error(
                    "INVALID_SOURCE_URL",
                    "GitHub URL 编码非法",
                    "source_resolution",
                )
            })?);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).map_err(|_| {
        error(
            "INVALID_SOURCE_URL",
            "GitHub URL 必须是 UTF-8",
            "source_resolution",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn bind_loopback() -> Option<TcpListener> {
        match TcpListener::bind(("127.0.0.1", 0)) {
            Ok(listener) => Some(listener),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => None,
            Err(error) => panic!("bind mock GitHub: {error}"),
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0_u8; 2048];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn reply(stream: &mut TcpStream, status: &str, headers: &str, body: &[u8]) {
        let response = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
    }

    fn data_dir(label: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data = crate::test_temp_root().join(format!(
            "csswitch-github-mock-{label}-{}-{suffix}/home/.claude-science",
            std::process::id()
        ));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("active-org.json"), br#"{"org_uuid":"org-test"}"#).unwrap();
        data
    }

    fn endpoints(port: u16) -> GithubEndpoints {
        GithubEndpoints {
            api_base: reqwest::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
            raw_base: reqwest::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
        }
    }

    #[test]
    fn parses_one_segment_ref_and_commit() {
        let named = parse_github_source(
            "https://github.com/anthropics/skills/tree/main/skills/internal-comms",
        )
        .unwrap();
        assert_eq!(named.reference, "main");
        assert_eq!(named.path, "skills/internal-comms");
        let sha = "a".repeat(40);
        let pinned = parse_github_source(&format!(
            "https://github.com/anthropics/skills/tree/{sha}/skills/internal-comms"
        ))
        .unwrap();
        assert_eq!(pinned.reference, sha);
    }

    #[test]
    fn package_parser_accepts_repo_ref_and_collection_roots() {
        let root = parse_github_package_source("https://github.com/owner/repo").unwrap();
        assert_eq!(root.reference, "HEAD");
        assert!(root.path.is_empty());
        let ref_root =
            parse_github_package_source("https://github.com/owner/repo/tree/main").unwrap();
        assert_eq!(ref_root.reference, "main");
        assert!(ref_root.path.is_empty());
        let collection =
            parse_github_package_source("https://github.com/owner/repo/tree/main/plugins/science")
                .unwrap();
        assert_eq!(collection.path, "plugins/science");
    }

    #[test]
    fn repo_root_bundle_uses_commit_zipball_and_one_codeload_request() {
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        for (path, body) in [
            ("repo-sha/skills/alpha/SKILL.md", b"# Alpha".as_slice()),
            ("repo-sha/skills/beta/SKILL.md", b"# Beta".as_slice()),
            (
                "repo-sha/skills/_shared/helper.py",
                b"print('ok')".as_slice(),
            ),
        ] {
            writer
                .start_file(path, SimpleFileOptions::default().unix_permissions(0o644))
                .unwrap();
            writer.write_all(body).unwrap();
        }
        let archive = writer.finish().unwrap().into_inner();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                requests.push(request.lines().next().unwrap_or("").to_string());
                match index {
                    0 => reply(
                        &mut stream,
                        "200 OK",
                        "Content-Type: application/json\r\n",
                        br#"{"sha":"22cff091926dddd2ca54bde7d7693aba92c2c7e9"}"#,
                    ),
                    1 => reply(
                        &mut stream,
                        "302 Found",
                        &format!("Location: http://127.0.0.1:{port}/codeload.zip\r\n"),
                        b"",
                    ),
                    _ => reply(
                        &mut stream,
                        "200 OK",
                        "Content-Type: application/zip\r\n",
                        &archive,
                    ),
                }
            }
            requests
        });
        let data = data_dir("bundle-archive");
        let mut phases = Vec::new();
        let result = install_github_package_with_endpoints_and_progress(
            &data,
            "https://github.com/owner/repo",
            &endpoints(port),
            &mut |phase, _| phases.push(phase.to_string()),
        )
        .unwrap();
        let InstalledPackage::Bundle(bundle) = result else {
            panic!("expected bundle");
        };
        assert_eq!(bundle.skill_names, vec!["alpha", "beta"]);
        assert_eq!(bundle.support_paths, vec!["_shared"]);
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /repos/owner/repo/commits/HEAD "));
        assert!(requests[1].starts_with(
            "GET /repos/owner/repo/zipball/22cff091926dddd2ca54bde7d7693aba92c2c7e9 "
        ));
        assert!(requests[2].starts_with("GET /codeload.zip "));
        assert!(requests.iter().all(|request| !request.contains("raw")));
        assert_eq!(
            phases,
            [
                "source_resolution",
                "recovery",
                "download",
                "download",
                "validation",
                "commit"
            ]
        );
        std::fs::remove_dir_all(data.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn unbounded_codeload_falls_back_to_tree_and_raw_then_reuses_archive_validation() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let commit = "a".repeat(40);
        let skill = b"---\nname: repo\ndescription: fallback\n---\n".to_vec();
        let expected_size = skill.len();
        let worker_commit = commit.clone();
        let worker = thread::spawn(move || {
            let (mut zipball, _) = listener.accept().unwrap();
            let zipball_request = read_request(&mut zipball);
            assert!(zipball_request
                .contains(&format!("GET /repos/owner/repo/zipball/{worker_commit} ")));
            reply(
                &mut zipball,
                "302 Found",
                &format!("Location: http://127.0.0.1:{port}/archive\r\n"),
                b"{}",
            );

            let (mut archive, _) = listener.accept().unwrap();
            let archive_request = read_request(&mut archive);
            assert!(archive_request.contains("GET /archive "));
            archive
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n",
                )
                .unwrap();

            let (mut tree, _) = listener.accept().unwrap();
            let tree_request = read_request(&mut tree);
            assert!(tree_request.contains(&format!(
                "GET /repos/owner/repo/git/trees/{worker_commit}?recursive=1 "
            )));
            let tree_body = serde_json::to_vec(&serde_json::json!({
                "truncated": false,
                "tree": [{
                    "path": "SKILL.md",
                    "mode": "100644",
                    "type": "blob",
                    "size": expected_size
                }]
            }))
            .unwrap();
            reply(
                &mut tree,
                "200 OK",
                "Content-Type: application/json\r\n",
                &tree_body,
            );

            let (mut raw, _) = listener.accept().unwrap();
            let raw_request = read_request(&mut raw);
            assert!(raw_request.contains(&format!("GET /owner/repo/{worker_commit}/SKILL.md ")));
            reply(&mut raw, "200 OK", "Content-Type: text/plain\r\n", &skill);
        });
        let source = GithubPackageSource {
            owner: "owner".into(),
            repo: "repo".into(),
            reference: commit.clone(),
            path: String::new(),
        };
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(5))
            .redirect(Policy::none())
            .build()
            .unwrap();
        let mut messages = Vec::new();
        let archive = download_repository_archive(
            &client,
            &source,
            &commit,
            &endpoints(port),
            Instant::now() + Duration::from_secs(5),
            &mut |phase, message| messages.push((phase.to_string(), message.to_string())),
        )
        .unwrap();
        assert!(matches!(
            package_or_bundle_from_github_archive(&archive, "", "repo").unwrap(),
            ValidatedArchive::Skill(_)
        ));
        assert!(messages.iter().any(|(phase, message)| {
            phase == "download_fallback" && message.contains("tree/raw")
        }));
        worker.join().unwrap();
    }

    #[test]
    #[ignore = "explicit full-size Nature archive through mock GitHub transport"]
    fn full_nature_archive_uses_github_flow_and_retry_skips_archive() {
        let archive_path = std::env::var_os("CSSWITCH_LOCAL_NATURE_BUNDLE_ARCHIVE")
            .map(PathBuf::from)
            .expect("set CSSWITCH_LOCAL_NATURE_BUNDLE_ARCHIVE");
        let archive = std::fs::read(archive_path).unwrap();
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                requests.push(request.lines().next().unwrap_or("").to_string());
                match index {
                    0 | 3 => reply(
                        &mut stream,
                        "200 OK",
                        "Content-Type: application/json\r\n",
                        br#"{"sha":"22cff091926dddd2ca54bde7d7693aba92c2c7e9"}"#,
                    ),
                    1 => reply(
                        &mut stream,
                        "302 Found",
                        &format!("Location: http://127.0.0.1:{port}/nature.zip\r\n"),
                        b"",
                    ),
                    _ => reply(
                        &mut stream,
                        "200 OK",
                        "Content-Type: application/zip\r\n",
                        &archive,
                    ),
                }
            }
            requests
        });
        let data = data_dir("full-nature-mock");
        let url = "https://github.com/Yuan1z0825/nature-skills";
        let first = install_github_package_with_endpoints(&data, url, &endpoints(port)).unwrap();
        let InstalledPackage::Bundle(first) = first else {
            panic!("expected Nature bundle");
        };
        assert_eq!(first.skill_names.len(), 17);
        assert!(first.support_paths.iter().any(|path| path == "_shared"));
        let repeated = install_github_package_with_endpoints(&data, url, &endpoints(port)).unwrap();
        let InstalledPackage::Bundle(repeated) = repeated else {
            panic!("expected recovered Nature bundle");
        };
        assert_eq!(repeated.action, crate::InstallAction::ReusedVerified);
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("/zipball/"))
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("GET /nature.zip "))
                .count(),
            1
        );
        std::fs::remove_dir_all(data.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn rejects_repo_root_query_and_encoded_slash_ref() {
        assert!(parse_github_source("https://github.com/a/b").is_err());
        assert!(parse_github_source("https://github.com/a/b/tree/main/x?tab=readme").is_err());
        let error =
            parse_github_source("https://github.com/a/b/tree/feature%2Fx/skill").unwrap_err();
        assert_eq!(error.code, "SOURCE_REF_REQUIRES_COMMIT_SHA");
    }

    #[test]
    fn named_ref_uses_one_commit_api_one_tree_api_and_target_file_get() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let commit = "a".repeat(40);
        let commit_for_worker = commit.clone();
        let worker = thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                let path = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_string();
                captured.lock().unwrap().push(path.clone());
                if path.contains("/commits/main") {
                    reply(
                        &mut stream,
                        "200 OK",
                        "Content-Type: application/json\r\n",
                        format!(r#"{{"sha":"{commit_for_worker}"}}"#).as_bytes(),
                    );
                } else if path.contains("/git/trees/") {
                    reply(
                        &mut stream,
                        "200 OK",
                        "Content-Type: application/json\r\n",
                        format!(
                            r#"{{"sha":"{commit_for_worker}","truncated":false,"tree":[{{"path":"skills/demo","mode":"040000","type":"tree"}},{{"path":"skills/demo/SKILL.md","mode":"100644","type":"blob","size":6}}]}}"#
                        )
                        .as_bytes(),
                    );
                } else {
                    assert_eq!(
                        path,
                        format!("/owner/repo/{commit_for_worker}/skills/demo/SKILL.md")
                    );
                    reply(
                        &mut stream,
                        "200 OK",
                        "Content-Type: text/plain\r\n",
                        b"# Demo",
                    );
                }
            }
        });
        let data = data_dir("named");
        let result = install_github_skill_with_endpoints(
            &data,
            "https://github.com/owner/repo/tree/main/skills/demo",
            &endpoints(port),
        )
        .unwrap();
        worker.join().unwrap();
        assert_eq!(result.resolved_commit_sha.as_deref(), Some(commit.as_str()));
        let retried = install_github_skill_with_endpoints(
            &data,
            "https://github.com/owner/repo/tree/main/skills/demo",
            &endpoints(port),
        )
        .unwrap();
        assert_eq!(retried.action, crate::InstallAction::ReusedVerified);
        assert_eq!(
            requests.lock().unwrap().as_slice(),
            [
                "/repos/owner/repo/commits/main",
                format!("/repos/owner/repo/git/trees/{commit}?recursive=1").as_str(),
                format!("/owner/repo/{commit}/skills/demo/SKILL.md").as_str(),
            ]
        );
        std::fs::remove_dir_all(data.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn commit_sha_skips_commit_api_and_rejects_truncated_tree() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let commit = "b".repeat(40);
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with(&format!(
                "GET /repos/owner/repo/git/trees/{commit}?recursive=1 "
            )));
            reply(
                &mut stream,
                "200 OK",
                "Content-Type: application/json\r\n",
                br#"{"truncated":true,"tree":[]}"#,
            );
        });
        let data = data_dir("sha");
        let error = install_github_skill_with_endpoints(
            &data,
            &format!(
                "https://github.com/owner/repo/tree/{}/skills/demo",
                "b".repeat(40)
            ),
            &endpoints(port),
        )
        .unwrap_err();
        worker.join().unwrap();
        assert_eq!(error.code, "GITHUB_TREE_TRUNCATED");
        std::fs::remove_dir_all(data.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn rate_limit_headers_classify_403_without_token_fallback() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET /repos/owner/repo/commits/main "));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            reply(
                &mut stream,
                "403 Forbidden",
                "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 9999999999\r\n",
                b"{}",
            );
        });
        let data = data_dir("rate-limit");
        let error = install_github_skill_with_endpoints(
            &data,
            "https://github.com/owner/repo/tree/main/skills/demo",
            &endpoints(port),
        )
        .unwrap_err();
        worker.join().unwrap();
        assert_eq!(error.code, "GITHUB_RATE_LIMITED");
        assert!(error.retryable);
        std::fs::remove_dir_all(data.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn timeout_is_not_collapsed_into_generic_network_error() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let client = Client::builder()
            .timeout(Duration::from_millis(10))
            .build()
            .unwrap();
        let problem = client
            .get(format!("http://127.0.0.1:{port}/slow"))
            .send()
            .unwrap_err();
        let error = github_transport_error(problem, "download", "timed out");
        assert_eq!(error.code, "GITHUB_TIMEOUT");
        assert!(error.retryable);
        worker.join().unwrap();
    }

    #[test]
    fn response_reader_enforces_wall_clock_deadline_across_slow_chunks() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\na")
                .unwrap();
            thread::sleep(Duration::from_millis(50));
            let _ = stream.write_all(b"b");
        });
        let client = Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let response = client
            .get(format!("http://127.0.0.1:{port}/slow-chunks"))
            .send()
            .unwrap();
        let error = read_response_limited(
            response,
            16,
            "download",
            Instant::now() + Duration::from_millis(10),
        )
        .unwrap_err();
        assert_eq!(error.code, "GITHUB_TIMEOUT");
        assert!(error.retryable);
        worker.join().unwrap();
    }

    #[test]
    fn response_reader_reports_incremental_archive_bytes() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let body = vec![b'x'; 5 * 1024 * 1024];
        let expected_len = body.len();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            reply(
                &mut stream,
                "200 OK",
                "Content-Type: application/zip\r\n",
                &body,
            );
        });
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let response = client
            .get(format!("http://127.0.0.1:{port}/archive"))
            .send()
            .unwrap();
        let mut messages = Vec::new();
        let bytes = read_response_limited_with_progress(
            response,
            expected_len + 1,
            "download",
            Instant::now() + Duration::from_secs(5),
            &mut |phase, message| messages.push((phase.to_string(), message.to_string())),
        )
        .unwrap();
        assert_eq!(bytes.len(), expected_len);
        assert!(messages
            .iter()
            .any(|(phase, message)| phase == "download" && message.contains("4.0 / 5.0 MiB")));
        assert!(messages
            .last()
            .is_some_and(|(_, message)| message.contains("下载完成：5.0 MiB")));
        worker.join().unwrap();
    }

    #[test]
    fn codeload_download_resumes_after_midstream_disconnect() {
        let Some(listener) = bind_loopback() else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let mut body = vec![b'x'; 2 * 1024 * 1024];
        body[0] = b'P';
        body[1] = b'K';
        let expected = body.clone();
        let first_connection_bytes = 512 * 1024;
        let worker = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let first_request = read_request(&mut first);
            assert!(first_request
                .to_ascii_lowercase()
                .contains("accept-encoding: identity"));
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            first.write_all(header.as_bytes()).unwrap();
            first.write_all(&body[..first_connection_bytes]).unwrap();
            drop(first);

            let (mut resumed, _) = listener.accept().unwrap();
            let resumed_request = read_request(&mut resumed);
            let range = resumed_request
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("range: bytes=")
                        .and_then(|value| value.strip_suffix('-'))
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .expect("resume request range");
            assert_eq!(range, first_connection_bytes);
            let headers = format!(
                "Content-Type: application/zip\r\nContent-Range: bytes {range}-{}/{}\r\n",
                body.len() - 1,
                body.len()
            );
            reply(
                &mut resumed,
                "206 Partial Content",
                &headers,
                &body[range..],
            );
        });
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let location = reqwest::Url::parse(&format!("http://127.0.0.1:{port}/archive")).unwrap();
        let mut messages = Vec::new();
        let bytes = download_codeload_archive(
            &client,
            &location,
            Instant::now() + Duration::from_secs(5),
            &mut |phase, message| messages.push((phase.to_string(), message.to_string())),
        )
        .unwrap();
        assert_eq!(bytes, expected);
        assert!(messages
            .iter()
            .any(|(phase, message)| phase == "download" && message.contains("尝试续传")));
        worker.join().unwrap();
    }

    #[test]
    fn content_range_must_match_resume_offset_and_total() {
        assert_eq!(parse_content_range("bytes 10-19/20"), Some((10, Some(20))));
        assert_eq!(parse_content_range("bytes 10-19/*"), Some((10, None)));
        assert_eq!(parse_content_range("bytes 20-19/30"), None);
        assert_eq!(parse_content_range("bytes 10-20/20"), None);
        assert_eq!(parse_content_range("items 10-19/20"), None);
    }

    #[test]
    fn complete_zip_is_accepted_after_transport_trailer_failure() {
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("repo/SKILL.md", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"---\nname: demo\n---\n").unwrap();
        let archive = writer.finish().unwrap().into_inner();
        assert!(complete_zip_archive(&archive));
        assert!(!complete_zip_archive(&archive[..archive.len() - 4]));
        assert!(!complete_zip_archive(b"PK-not-a-zip"));
    }

    #[test]
    #[ignore = "explicit public GitHub subtree smoke; temporary data-dir only"]
    fn public_commit_subtree_download_smoke() {
        assert_eq!(
            std::env::var("CSSWITCH_PUBLIC_GITHUB_SKILL_SMOKE").as_deref(),
            Ok("1"),
            "must explicitly enable public GitHub smoke"
        );
        let data = data_dir("public-style-guide");
        let result = install_github_skill(
            &data,
            "https://github.com/michaelboeding/skills/tree/84abf02d42612ab0b94a54de1a1a454ae25dd131/skills/style-guide",
        )
        .unwrap();
        assert_eq!(result.skill_name, "style-guide");
        assert_eq!(
            result.resolved_commit_sha.as_deref(),
            Some("84abf02d42612ab0b94a54de1a1a454ae25dd131")
        );
        let installed = data.join("orgs/org-test/skills/style-guide");
        assert!(installed.join("SKILL.md").is_file());
        assert!(installed.join(".import-origin").is_file());
        assert_eq!(
            std::fs::read_dir(&installed).unwrap().count(),
            2,
            "only target SKILL.md and CSSwitch marker should be committed"
        );
        std::fs::remove_dir_all(data.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    #[ignore = "explicit fixed Nature bundle acceptance; temporary data-dir only"]
    fn public_nature_bundle_archive_smoke() {
        assert_eq!(
            std::env::var("CSSWITCH_PUBLIC_NATURE_BUNDLE_SMOKE").as_deref(),
            Ok("1"),
            "must explicitly enable fixed Nature bundle smoke"
        );
        let data = data_dir("public-nature-bundle");
        let url = "https://github.com/Yuan1z0825/nature-skills/tree/22cff091926dddd2ca54bde7d7693aba92c2c7e9";
        let result = install_github_package(&data, url).unwrap();
        let InstalledPackage::Bundle(bundle) = result else {
            panic!("expected Nature bundle");
        };
        assert_eq!(
            bundle.resolved_commit_sha.as_deref(),
            Some("22cff091926dddd2ca54bde7d7693aba92c2c7e9")
        );
        assert!(bundle.skill_names.len() > 1);
        assert!(bundle.support_paths.iter().any(|path| path == "_shared"));
        let skills = data.join("orgs/org-test/skills");
        assert!(skills.join("_shared").is_dir());
        for skill_name in &bundle.skill_names {
            assert!(skills.join(skill_name).join("SKILL.md").is_file());
            assert!(skills.join(skill_name).join(".import-origin").is_file());
        }
        let repeated = install_github_package(&data, url).unwrap();
        let InstalledPackage::Bundle(repeated) = repeated else {
            panic!("expected repeated Nature bundle");
        };
        assert_eq!(repeated.action, crate::InstallAction::ReusedVerified);
        std::fs::remove_dir_all(data.parent().unwrap().parent().unwrap()).unwrap();
    }
}
