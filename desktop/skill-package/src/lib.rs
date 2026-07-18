mod archive;
mod bundle;
mod github;
mod install;
mod listing;
mod science;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use bundle::{
    find_bundle_for_skill, quarantine_bundle, BundleCommit, BundleMemberCommit, BundleUninstall,
    BundleUninstallCommit,
};
pub use github::{
    install_github_package, install_github_package_with_progress, install_github_skill,
    parse_github_package_source, parse_github_source, GithubPackageSource, GithubSource,
};
pub use install::{
    active_org, install_local_package, install_local_skill, verify_csswitch_import_origin,
    InstallAction, InstallCommit, InstalledPackage, LocalArchiveInput,
};
pub use listing::{
    inspect_active_org_skills, InstalledSkillSource, InstalledSkillSummary,
    SkillFilesystemSnapshot, SkillListWarning, MAX_LISTED_SKILLS, MAX_SKILL_FRONTMATTER_BYTES,
};
pub use science::{
    attach_skill, read_agent_skill_names, update_agent_skills, verify_attach_control_ready,
    AttachError, AttachResult, BatchSkillUpdate,
};

pub const SCHEMA_VERSION: u64 = 2;
pub const AGENT_NAME: &str = "OPERON";
pub const IMPORT_ORIGIN_FILE: &str = ".import-origin";
pub const CATALOG_STAMP_FILE: &str = ".catalog_stamp";
pub const CSSWITCH_MARKETPLACE: &str = "csswitch-local-bridge";
pub const MAX_ARCHIVE_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_ARCHIVE_ENTRIES: usize = 10_000;
pub const MAX_FILES: usize = 512;
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_BUNDLE_FILES: usize = 2_000;
pub const MAX_BUNDLE_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PATH_BYTES: usize = 1_024;
pub const MAX_PATH_DEPTH: usize = 32;
pub const MAX_IMPORT_ORIGIN_BYTES: usize = 16 * 1024;
pub const GITHUB_BUNDLE_OPERATION_TIMEOUT_SECONDS: u64 = 1_800;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScienceExecutableFingerprint {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub mode: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScienceHostContext {
    pub binary: PathBuf,
    pub version: String,
    pub fingerprint: ScienceExecutableFingerprint,
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub sandbox_port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Github,
    LocalZip,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::LocalZip => "local_zip",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyEvidence {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub pattern: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PackageFile {
    pub path: PathBuf,
    pub content: Vec<u8>,
    pub executable: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ValidatedPackage {
    pub skill_name: String,
    pub files: Vec<PackageFile>,
    pub content_sha256: String,
}

#[derive(Clone, Debug)]
pub(crate) struct BundleMember {
    pub skill_name: String,
    pub member_path: PathBuf,
    pub content_sha256: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ValidatedBundle {
    pub bundle_name: String,
    pub collection_path: String,
    pub files: Vec<PackageFile>,
    pub members: Vec<BundleMember>,
    pub support_paths: Vec<String>,
    pub content_sha256: String,
}

#[derive(Clone, Debug)]
pub(crate) enum ValidatedArchive {
    Skill(ValidatedPackage),
    Bundle(ValidatedBundle),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallError {
    pub code: String,
    pub message: String,
    pub phase: String,
    pub retryable: bool,
    pub directory_commit: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<DependencyEvidence>,
}

impl InstallError {
    pub fn new(code: &str, message: impl Into<String>, phase: &str) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            phase: phase.to_string(),
            retryable: false,
            directory_commit: false,
            evidence: Vec::new(),
        }
    }

    pub fn retryable(mut self, value: bool) -> Self {
        self.retryable = value;
        self
    }

    pub fn with_evidence(mut self, evidence: Vec<DependencyEvidence>) -> Self {
        self.evidence = evidence;
        self
    }
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for InstallError {}

pub(crate) fn error(code: &str, message: impl Into<String>, phase: &str) -> InstallError {
    InstallError::new(code, message, phase)
}
