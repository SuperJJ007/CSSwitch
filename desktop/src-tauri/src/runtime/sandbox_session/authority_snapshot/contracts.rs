#[allow(clippy::useless_conversion)]
pub(super) fn inode_u64(value: libc::ino_t) -> Option<u64> {
    u64::try_from(value).ok()
}

pub(super) struct AuthorityTreeSnapshot {
    pub(super) scope: AuthoritySnapshotScope,
    pub(super) source: PathBuf,
    pub(super) backup: PathBuf,
    pub(super) existed: bool,
    pub(super) source_parent: Option<std::fs::File>,
    pub(super) source_name: Option<std::ffi::CString>,
    pub(super) backup_identity: Option<(u64, u64, libc::mode_t)>,
    pub(super) backup_parent: Option<std::fs::File>,
    pub(super) backup_name: Option<std::ffi::CString>,
}

pub(super) struct AuthorityDirectoryStream(*mut libc::DIR);

impl Drop for AuthorityDirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

pub(super) const MAX_AUTHORITY_SNAPSHOT_ENTRIES: usize = 131_072;
pub(super) const MAX_AUTHORITY_SNAPSHOT_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(super) const MAX_AUTHORITY_SNAPSHOT_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub(super) const MAX_AUTHORITY_FULL_COPY_FILE_BYTES: u64 = 128 * 1024 * 1024;
pub(super) const MAX_AUTHORITY_FULL_COPY_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
pub(super) const SCIENCE_OWNED_OPAQUE_ROOTS: [&str; 5] =
    ["conda", "runtime", "seed-assets", "r-libs", "sbx-bind-src"];
pub(crate) const SCIENCE_PROTECTED_AUTHORITY_ENTRIES: [&str; 10] = [
    "encryption.key",
    ".oauth-tokens",
    "active-org.json",
    ".key-backups",
    "auth-owner.lock",
    "config.toml",
    "csswitch-ssh-bridge.v1.json",
    "mcp",
    ".csswitch-route-state.json",
    "orgs",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AuthoritySnapshotScope {
    ScienceData,
    SandboxState,
    CsswitchRuntime,
    ManagedReceipt,
    #[default]
    Test,
}

impl AuthoritySnapshotScope {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::ScienceData => "science_data",
            Self::SandboxState => "sandbox_state",
            Self::CsswitchRuntime => "csswitch_runtime",
            Self::ManagedReceipt => "managed_receipt",
            Self::Test => "test",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum AuthoritySnapshotCategory {
    CondaCache,
    ScienceRuntime,
    Skills,
    OrgState,
    #[default]
    Other,
}

impl AuthoritySnapshotCategory {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::CondaCache => "conda_cache",
            Self::ScienceRuntime => "science_runtime",
            Self::Skills => "skills",
            Self::OrgState => "org_state",
            Self::Other => "other",
        }
    }
}
