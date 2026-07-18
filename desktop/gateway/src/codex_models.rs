use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::{Client, Response};
use reqwest::header::{
    HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_LENGTH, ETAG, IF_NONE_MATCH, USER_AGENT,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::codex_auth::InferenceSecrets;
use crate::codex_network::CodexHttpClientFactory;
use crate::config::UPSTREAM_UA;
use crate::provider_contracts::CodexRuntimeContract;

const MODELS_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/models";
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CACHE_FILE: &str = "codex-models-cache.v3.json";
const CACHE_EPOCH_FILE: &str = "codex-models-cache-epoch.v3.json";
const CACHE_LOCK_FILE: &str = "codex-models-cache.v3.lock";
const CACHE_VERSION: u32 = 3;
#[cfg(test)]
const NORMAL_TTL_SECONDS: u64 = 5 * 60;
#[cfg(test)]
const STALE_TTL_SECONDS: u64 = 24 * 60 * 60;
#[cfg(test)]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(500)];
const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MODELS_BODY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MODELS: usize = 512;
const SCIENCE_MODEL_PREFIX: &str = "claude-csswitch-codex-";
const SCIENCE_DISPLAY_PREFIX: &str = "Codex / ";
const MAX_SCIENCE_MODEL_ID_BYTES: usize = 256;
const MAX_MODEL_ID_BYTES: usize = MAX_SCIENCE_MODEL_ID_BYTES - SCIENCE_MODEL_PREFIX.len();
const MAX_SCIENCE_DISPLAY_NAME_BYTES: usize = 512;
const MAX_DISPLAY_NAME_BYTES: usize = MAX_SCIENCE_DISPLAY_NAME_BYTES - SCIENCE_DISPLAY_PREFIX.len();
const MAX_ETAG_BYTES: usize = 512;
const MAX_REASONING_LEVELS: usize = 32;
const MAX_REASONING_EFFORT_BYTES: usize = 32;
const CREATED_AT: &str = "2026-01-01T00:00:00Z";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CachedModel {
    id: String,
    display_name: String,
    priority: i32,
    default_reasoning_effort: Option<String>,
    supported_reasoning_efforts: Vec<String>,
    supports_reasoning_summary: bool,
    supports_parallel_tool_calls: bool,
    use_responses_lite: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelsCacheFile {
    version: u32,
    auth_epoch: String,
    auth_generation: u64,
    account_hash: String,
    fetched_at: u64,
    etag: Option<String>,
    models: Vec<CachedModel>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelsCacheEpoch {
    version: u32,
    auth_epoch: String,
    auth_generation: u64,
    account_hash: String,
    nonce: String,
    invalidated: bool,
}

impl ModelsCacheEpoch {
    fn new(identity: &CatalogIdentity, invalidated: bool) -> Result<Self, CodexModelsError> {
        let mut random = [0_u8; 16];
        getrandom::getrandom(&mut random)
            .map_err(|_| CodexModelsError::cache("Codex model cache random failed"))?;
        Ok(Self {
            version: CACHE_VERSION,
            auth_epoch: identity.auth_epoch.clone(),
            auth_generation: identity.auth_generation,
            account_hash: identity.account_hash.clone(),
            nonce: random.iter().map(|byte| format!("{byte:02x}")).collect(),
            invalidated,
        })
    }

    fn matches(&self, identity: &CatalogIdentity) -> bool {
        self.version == CACHE_VERSION
            && self.auth_epoch == identity.auth_epoch
            && self.auth_generation == identity.auth_generation
            && self.account_hash == identity.account_hash
    }

    fn validate(&self) -> Result<(), CodexModelsError> {
        if self.version != CACHE_VERSION
            || self.auth_epoch.len() != 32
            || !self.auth_epoch.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.account_hash.len() != 32
            || !self
                .account_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.nonce.len() != 32
            || !self.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(CodexModelsError::cache(
                "Codex model cache epoch is invalid",
            ));
        }
        Ok(())
    }
}

impl ModelsCacheFile {
    fn matches(&self, identity: &CatalogIdentity) -> bool {
        self.version == CACHE_VERSION
            && self.auth_epoch == identity.auth_epoch
            && self.auth_generation == identity.auth_generation
            && self.account_hash == identity.account_hash
    }

    fn age_at(&self, now: u64) -> Option<u64> {
        now.checked_sub(self.fetched_at)
    }

    fn validate(&self) -> Result<(), CodexModelsError> {
        if self.version != CACHE_VERSION
            || self.auth_epoch.len() != 32
            || !self.auth_epoch.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.account_hash.len() != 32
            || !self
                .account_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.models.len() > MAX_MODELS
            || self.etag.as_deref().is_some_and(|etag| {
                etag.len() > MAX_ETAG_BYTES || etag.chars().any(char::is_control)
            })
        {
            return Err(CodexModelsError::cache("Codex model cache is invalid"));
        }
        let mut ids = HashSet::new();
        for model in &self.models {
            validate_model(model)?;
            if !ids.insert(model.id.as_str()) {
                return Err(CodexModelsError::cache("Codex model cache is invalid"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
struct OfficialModel {
    slug: String,
    display_name: String,
    visibility: String,
    priority: i32,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<OfficialReasoningLevel>,
    #[serde(default, deserialize_with = "deserialize_optional_bool")]
    supports_reasoning_summaries: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_bool")]
    supports_reasoning_summary_parameter: Option<bool>,
    #[serde(default)]
    supports_parallel_tool_calls: bool,
    #[serde(default)]
    use_responses_lite: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct OfficialReasoningLevel {
    effort: String,
}

impl OfficialModel {
    fn supports_reasoning_summary(&self) -> bool {
        self.supports_reasoning_summaries
            .or(self.supports_reasoning_summary_parameter)
            .unwrap_or(true)
    }
}

fn deserialize_optional_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    bool::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CatalogIdentity {
    auth_epoch: String,
    auth_generation: u64,
    account_hash: String,
}

impl CatalogIdentity {
    fn from_secrets(secrets: &InferenceSecrets) -> Self {
        Self {
            auth_epoch: secrets.auth_epoch().to_string(),
            auth_generation: secrets.auth_generation(),
            account_hash: secrets.account_hash().to_string(),
        }
    }
}

#[derive(Default)]
struct CatalogState {
    invalidated: HashSet<CatalogIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CatalogSource {
    Live,
    FreshCache,
    RevalidatedCache,
    StaleCache,
}

impl CatalogSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::FreshCache => "fresh-cache",
            Self::RevalidatedCache => "revalidated-cache",
            Self::StaleCache => "stale-cache",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CodexModelsSnapshot {
    models: Vec<CachedModel>,
    source: CatalogSource,
    age_seconds: u64,
    published_generation: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolvedCodexModel<'a> {
    model: &'a CachedModel,
}

impl ResolvedCodexModel<'_> {
    pub(crate) fn raw_id(&self) -> &str {
        &self.model.id
    }

    pub(crate) fn default_reasoning_effort(&self) -> Option<&str> {
        self.model.default_reasoning_effort.as_deref()
    }

    pub(crate) fn supports_reasoning_summary(&self) -> bool {
        self.model.supports_reasoning_summary
    }

    pub(crate) fn supports_parallel_tool_calls(&self) -> bool {
        self.model.supports_parallel_tool_calls
    }

    pub(crate) fn use_responses_lite(&self) -> bool {
        self.model.use_responses_lite
    }
}

impl CodexModelsSnapshot {
    #[cfg(test)]
    pub(crate) fn contains_raw(&self, model: &str) -> bool {
        self.models.iter().any(|candidate| candidate.id == model)
    }

    pub(crate) fn resolve_science_model<'a>(
        &'a self,
        model: &str,
    ) -> Option<ResolvedCodexModel<'a>> {
        let raw_id = model.strip_prefix(SCIENCE_MODEL_PREFIX)?;
        self.models
            .iter()
            .find(|candidate| candidate.id == raw_id)
            .map(|candidate| ResolvedCodexModel { model: candidate })
    }

    pub(crate) fn source(&self) -> CatalogSource {
        self.source
    }

    pub(crate) fn age_seconds(&self) -> u64 {
        self.age_seconds
    }

    pub(crate) fn response_body(&self) -> Value {
        let data: Vec<Value> = self
            .models
            .iter()
            .map(|model| {
                let science_id = science_model_alias(&model.id);
                json!({
                    "type": "model",
                    "id": science_id,
                    "display_name": format!("{SCIENCE_DISPLAY_PREFIX}{}", model.display_name),
                    "supports_tools": true,
                    "created_at": CREATED_AT,
                })
            })
            .collect();
        json!({
            "data": data,
            "has_more": false,
            "first_id": data.first().and_then(|model| model.get("id")).cloned().unwrap_or(Value::Null),
            "last_id": data.last().and_then(|model| model.get("id")).cloned().unwrap_or(Value::Null),
            "diagnostics": {
                "source": self.source.as_str(),
                "stale": self.source == CatalogSource::StaleCache,
                "age_seconds": self.age_seconds,
                "generation": self.published_generation,
            },
        })
    }
}

fn science_model_alias(raw_id: &str) -> String {
    format!("{SCIENCE_MODEL_PREFIX}{raw_id}")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CodexModelsError {
    pub(crate) status: u16,
    pub(crate) upstream_status: Option<u16>,
    pub(crate) error_kind: &'static str,
    pub(crate) detail: &'static str,
}

impl CodexModelsError {
    fn network() -> Self {
        Self {
            status: 502,
            upstream_status: None,
            error_kind: "network",
            detail: "Codex model catalog is temporarily unavailable",
        }
    }

    fn protocol(detail: &'static str) -> Self {
        Self {
            status: 502,
            upstream_status: None,
            error_kind: "protocol",
            detail,
        }
    }

    fn cache(detail: &'static str) -> Self {
        Self {
            status: 500,
            upstream_status: None,
            error_kind: "cache",
            detail,
        }
    }

    fn invalidated() -> Self {
        Self {
            status: 503,
            upstream_status: None,
            error_kind: "cache_invalidated",
            detail: "Codex model catalog changed while the request was in progress",
        }
    }

    fn upstream(status: u16) -> Self {
        Self {
            status: if matches!(status, 401 | 403 | 408 | 429) {
                status
            } else {
                502
            },
            upstream_status: Some(status),
            error_kind: "upstream",
            detail: "Codex model catalog request was rejected",
        }
    }
}

impl fmt::Display for CodexModelsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.detail)
    }
}

impl std::error::Error for CodexModelsError {}

pub(crate) struct CodexModelCatalog {
    client: Client,
    endpoint: String,
    state_root: PathBuf,
    retry_delays: [Duration; 2],
    normal_ttl_seconds: u64,
    stale_ttl_seconds: u64,
    state: Mutex<CatalogState>,
    published: Mutex<Option<(CatalogIdentity, CodexModelsSnapshot)>>,
    published_generation: AtomicU64,
}

#[derive(Clone, Copy)]
struct CatalogPolicy {
    retry_delays: [Duration; 2],
    connect_timeout: Duration,
    request_timeout: Duration,
    normal_ttl_seconds: u64,
    stale_ttl_seconds: u64,
}

impl fmt::Debug for CodexModelCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexModelCatalog")
            .field("endpoint", &self.endpoint)
            .field("state_root", &self.state_root)
            .finish_non_exhaustive()
    }
}

impl CodexModelCatalog {
    pub(crate) fn production(
        state_root: PathBuf,
        contract: &CodexRuntimeContract,
    ) -> Result<Self, CodexModelsError> {
        let factory =
            CodexHttpClientFactory::from_environment().map_err(|_| CodexModelsError::network())?;
        Self::new_with_factory(
            format!(
                "{MODELS_ENDPOINT}?client_version={}",
                contract.model_catalog_client_version
            ),
            state_root,
            CatalogPolicy {
                retry_delays: RETRY_DELAYS,
                connect_timeout: contract.connect_timeout,
                request_timeout: contract.request_timeout,
                normal_ttl_seconds: contract.normal_ttl_seconds,
                stale_ttl_seconds: contract.stale_ttl_seconds,
            },
            &factory,
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        endpoint: String,
        state_root: PathBuf,
    ) -> Result<Self, CodexModelsError> {
        Self::new_with_factory(
            endpoint,
            state_root,
            CatalogPolicy {
                retry_delays: [Duration::ZERO; 2],
                connect_timeout: CONNECT_TIMEOUT,
                request_timeout: REQUEST_TIMEOUT,
                normal_ttl_seconds: NORMAL_TTL_SECONDS,
                stale_ttl_seconds: STALE_TTL_SECONDS,
            },
            &CodexHttpClientFactory::direct_for_test(),
        )
    }

    fn new_with_factory(
        endpoint: String,
        state_root: PathBuf,
        policy: CatalogPolicy,
        factory: &CodexHttpClientFactory,
    ) -> Result<Self, CodexModelsError> {
        let client = factory
            .blocking_builder()
            .map_err(|_| CodexModelsError::network())?
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0)
            .connect_timeout(policy.connect_timeout)
            .timeout(policy.request_timeout)
            .build()
            .map_err(|_| CodexModelsError::network())?;
        Ok(Self {
            client,
            endpoint,
            state_root,
            retry_delays: policy.retry_delays,
            normal_ttl_seconds: policy.normal_ttl_seconds,
            stale_ttl_seconds: policy.stale_ttl_seconds,
            state: Mutex::new(CatalogState::default()),
            published: Mutex::new(None),
            published_generation: AtomicU64::new(0),
        })
    }

    /// Return one immutable, process-published snapshot for both `/v1/models`
    /// and inference. `list` re-reads the shared cache epoch before consulting
    /// the network, so an explicit refresh performed by the desktop's isolated
    /// scratch gateway becomes visible here. A fully validated candidate is
    /// swapped under one lock; unchanged catalogs keep their generation.
    pub(crate) fn published_snapshot(
        &self,
        secrets: &InferenceSecrets,
    ) -> Result<CodexModelsSnapshot, CodexModelsError> {
        let identity = CatalogIdentity::from_secrets(secrets);
        let mut published = self
            .published
            .lock()
            .map_err(|_| CodexModelsError::cache("Codex published catalog lock is unavailable"))?;
        // Keep candidate selection and publication under one process-local
        // lock. Otherwise an older fresh-cache reader can resume after a newer
        // reader and publish the old catalog with a larger generation.
        let candidate = self.list(secrets)?;
        self.publish_candidate(&identity, candidate, &mut published)
    }

    /// Explicit UI discovery path. It bypasses the normal fresh-cache TTL, but
    /// does not invalidate or remove the last good cache before the new
    /// response has been fully validated and atomically committed.
    pub(crate) fn refresh_published_snapshot(
        &self,
        secrets: &InferenceSecrets,
    ) -> Result<CodexModelsSnapshot, CodexModelsError> {
        let identity = CatalogIdentity::from_secrets(secrets);
        let mut published = self
            .published
            .lock()
            .map_err(|_| CodexModelsError::cache("Codex published catalog lock is unavailable"))?;
        let candidate = self.list_at_mode(secrets, unix_time_seconds(), true)?;
        self.publish_candidate(&identity, candidate, &mut published)
    }

    fn publish_candidate(
        &self,
        identity: &CatalogIdentity,
        mut candidate: CodexModelsSnapshot,
        published: &mut Option<(CatalogIdentity, CodexModelsSnapshot)>,
    ) -> Result<CodexModelsSnapshot, CodexModelsError> {
        if let Some((_, snapshot)) = published
            .as_ref()
            .filter(|(published_identity, _)| published_identity == identity)
            .filter(|(_, snapshot)| snapshot.models == candidate.models)
        {
            return Ok(snapshot.clone());
        }
        candidate.published_generation = self
            .published_generation
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        *published = Some((identity.clone(), candidate.clone()));
        Ok(candidate)
    }

    pub(crate) fn list(
        &self,
        secrets: &InferenceSecrets,
    ) -> Result<CodexModelsSnapshot, CodexModelsError> {
        self.list_at(secrets, unix_time_seconds())
    }

    fn list_at(
        &self,
        secrets: &InferenceSecrets,
        now: u64,
    ) -> Result<CodexModelsSnapshot, CodexModelsError> {
        self.list_at_mode(secrets, now, false)
    }

    fn list_at_mode(
        &self,
        secrets: &InferenceSecrets,
        now: u64,
        force_refresh: bool,
    ) -> Result<CodexModelsSnapshot, CodexModelsError> {
        let identity = CatalogIdentity::from_secrets(secrets);
        let mut state = self
            .state
            .lock()
            .map_err(|_| CodexModelsError::cache("Codex model cache lock is unavailable"))?;
        let (marker_at_start, cached_at_start) =
            self.with_cache_lock(|| Ok((self.load_cache_epoch()?, self.load_cache()?)))?;
        let marker_nonce_at_start = marker_at_start.as_ref().map(|marker| marker.nonce.clone());
        let invalidated = state.invalidated.contains(&identity)
            || marker_at_start
                .as_ref()
                .is_some_and(|marker| marker.matches(&identity) && marker.invalidated);
        let cached = (!invalidated)
            .then_some(cached_at_start)
            .flatten()
            .filter(|cache| cache.matches(&identity))
            .filter(|cache| cache.age_at(now).is_some());
        if !force_refresh {
            if let Some(cache) = cached.as_ref() {
                if cache
                    .age_at(now)
                    .is_some_and(|age| age <= self.normal_ttl_seconds)
                {
                    return Ok(snapshot_from_cache(cache, CatalogSource::FreshCache, now));
                }
            }
        }
        let stale = cached.filter(|cache| {
            cache
                .age_at(now)
                .is_some_and(|age| age <= self.stale_ttl_seconds)
        });
        match self.fetch_with_retries(secrets, stale.as_ref()) {
            Ok(FetchResult::Live { models, etag }) => {
                let cache = ModelsCacheFile {
                    version: CACHE_VERSION,
                    auth_epoch: identity.auth_epoch.clone(),
                    auth_generation: identity.auth_generation,
                    account_hash: identity.account_hash.clone(),
                    fetched_at: now,
                    etag,
                    models,
                };
                self.commit_cache_if_current(&identity, marker_nonce_at_start.as_deref(), &cache)?;
                state.invalidated.remove(&identity);
                Ok(snapshot_from_cache(&cache, CatalogSource::Live, now))
            }
            Ok(FetchResult::NotModified) => {
                let mut cache = stale.ok_or_else(|| {
                    CodexModelsError::protocol("Codex model catalog returned an invalid 304")
                })?;
                cache.fetched_at = now;
                self.commit_cache_if_current(&identity, marker_nonce_at_start.as_deref(), &cache)?;
                state.invalidated.remove(&identity);
                Ok(snapshot_from_cache(
                    &cache,
                    CatalogSource::RevalidatedCache,
                    now,
                ))
            }
            Err(error) if matches!(error.upstream_status, Some(401 | 403)) => {
                state.invalidated.insert(identity.clone());
                let _ = self.persist_invalidation(&identity);
                Err(error)
            }
            Err(error) if stale_eligible(&error) => {
                self.ensure_marker_unchanged(marker_nonce_at_start.as_deref())?;
                stale
                    .as_ref()
                    .map(|cache| snapshot_from_cache(cache, CatalogSource::StaleCache, now))
                    .ok_or(error)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn invalidate_identity(
        &self,
        auth_epoch: &str,
        auth_generation: u64,
        account_hash: &str,
    ) {
        if let Ok(mut published) = self.published.lock() {
            if published.as_ref().is_some_and(|(identity, _)| {
                identity.auth_epoch == auth_epoch
                    && identity.auth_generation == auth_generation
                    && identity.account_hash == account_hash
            }) {
                *published = None;
            }
        }
        if let Ok(mut state) = self.state.lock() {
            let identity = CatalogIdentity {
                auth_epoch: auth_epoch.to_string(),
                auth_generation,
                account_hash: account_hash.to_string(),
            };
            state.invalidated.insert(identity.clone());
            let _ = self.persist_invalidation(&identity);
        }
    }

    fn commit_cache_if_current(
        &self,
        identity: &CatalogIdentity,
        marker_nonce_at_start: Option<&str>,
        cache: &ModelsCacheFile,
    ) -> Result<(), CodexModelsError> {
        self.with_cache_lock(|| {
            let marker_now = self.load_cache_epoch()?;
            let marker_nonce_now = marker_now.as_ref().map(|marker| marker.nonce.as_str());
            if marker_nonce_now != marker_nonce_at_start {
                return Err(CodexModelsError::invalidated());
            }
            let next_epoch = ModelsCacheEpoch::new(identity, false)?;
            self.commit_cache_epoch(&next_epoch)?;
            self.commit_cache(cache)?;
            Ok(())
        })
    }

    fn ensure_marker_unchanged(
        &self,
        marker_nonce_at_start: Option<&str>,
    ) -> Result<(), CodexModelsError> {
        self.with_cache_lock(|| {
            let marker_now = self.load_cache_epoch()?;
            let marker_nonce_now = marker_now.as_ref().map(|marker| marker.nonce.as_str());
            if marker_nonce_now == marker_nonce_at_start {
                Ok(())
            } else {
                Err(CodexModelsError::invalidated())
            }
        })
    }

    fn persist_invalidation(&self, identity: &CatalogIdentity) -> Result<(), CodexModelsError> {
        let marker = ModelsCacheEpoch::new(identity, true)?;
        self.with_cache_lock(|| {
            self.commit_cache_epoch(&marker)?;
            self.remove_cache()
        })
    }

    fn fetch_with_retries(
        &self,
        secrets: &InferenceSecrets,
        cached: Option<&ModelsCacheFile>,
    ) -> Result<FetchResult, CodexModelsError> {
        for attempt in 0..=self.retry_delays.len() {
            match self.fetch_once(secrets, cached.and_then(|cache| cache.etag.as_deref())) {
                Ok(result) => return Ok(result),
                Err(error) if retryable(&error) && attempt < self.retry_delays.len() => {
                    thread::sleep(self.retry_delays[attempt]);
                }
                Err(error) => return Err(error),
            }
        }
        Err(CodexModelsError::network())
    }

    fn fetch_once(
        &self,
        secrets: &InferenceSecrets,
        etag: Option<&str>,
    ) -> Result<FetchResult, CodexModelsError> {
        let authorization = Zeroizing::new(format!("Bearer {}", secrets.access_token()));
        let mut authorization_header =
            HeaderValue::from_str(&authorization).map_err(|_| CodexModelsError::upstream(401))?;
        authorization_header.set_sensitive(true);
        let mut account_header = HeaderValue::from_str(secrets.account_id())
            .map_err(|_| CodexModelsError::upstream(401))?;
        account_header.set_sensitive(true);
        let mut request = self
            .client
            .get(&self.endpoint)
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, UPSTREAM_UA)
            .header("originator", CODEX_ORIGINATOR)
            .header("ChatGPT-Account-ID", account_header)
            .header(AUTHORIZATION, authorization_header);
        if let Some(etag) = etag {
            let value = HeaderValue::from_str(etag)
                .map_err(|_| CodexModelsError::cache("Codex model cache ETag is invalid"))?;
            request = request.header(IF_NONE_MATCH, value);
        }
        let response = request.send().map_err(|_| CodexModelsError::network())?;
        let status = response.status().as_u16();
        if status == 304 {
            return Ok(FetchResult::NotModified);
        }
        if !response.status().is_success() {
            return Err(CodexModelsError::upstream(status));
        }
        parse_live_response(response)
    }

    fn cache_path(&self) -> PathBuf {
        self.state_root.join(CACHE_FILE)
    }

    fn epoch_path(&self) -> PathBuf {
        self.state_root.join(CACHE_EPOCH_FILE)
    }

    fn lock_path(&self) -> PathBuf {
        self.state_root.join(CACHE_LOCK_FILE)
    }

    fn with_cache_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, CodexModelsError>,
    ) -> Result<T, CodexModelsError> {
        let _lock = self.acquire_cache_lock()?;
        operation()
    }

    fn acquire_cache_lock(&self) -> Result<CacheFileLock, CodexModelsError> {
        ensure_private_root(&self.state_root)?;
        let path = self.lock_path();
        reject_unsafe_target(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(path)
            .map_err(|_| CodexModelsError::cache("Codex model cache lock is unavailable"))?;
        validate_private_file(&file)?;
        #[cfg(unix)]
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return Err(CodexModelsError::cache(
                    "Codex model cache lock is unavailable",
                ));
            }
        }
        Ok(CacheFileLock(file))
    }

    fn load_cache(&self) -> Result<Option<ModelsCacheFile>, CodexModelsError> {
        if !validate_private_root(&self.state_root)? {
            return Ok(None);
        }
        let path = self.cache_path();
        reject_unsafe_target(&path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(CodexModelsError::cache("Codex model cache is unavailable")),
        };
        validate_private_file(&file)?;
        let mut bytes = Vec::new();
        file.take(MAX_CACHE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| CodexModelsError::cache("Codex model cache is unavailable"))?;
        if bytes.len() as u64 > MAX_CACHE_BYTES {
            return Err(CodexModelsError::cache("Codex model cache is invalid"));
        }
        let cache: ModelsCacheFile = serde_json::from_slice(&bytes)
            .map_err(|_| CodexModelsError::cache("Codex model cache is invalid"))?;
        cache.validate()?;
        Ok(Some(cache))
    }

    fn load_cache_epoch(&self) -> Result<Option<ModelsCacheEpoch>, CodexModelsError> {
        if !validate_private_root(&self.state_root)? {
            return Ok(None);
        }
        let path = self.epoch_path();
        reject_unsafe_target(&path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(CodexModelsError::cache(
                    "Codex model cache epoch is unavailable",
                ));
            }
        };
        validate_private_file(&file)?;
        let mut bytes = Vec::new();
        file.take(MAX_CACHE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| CodexModelsError::cache("Codex model cache epoch is unavailable"))?;
        if bytes.len() as u64 > MAX_CACHE_BYTES {
            return Err(CodexModelsError::cache(
                "Codex model cache epoch is invalid",
            ));
        }
        let marker: ModelsCacheEpoch = serde_json::from_slice(&bytes)
            .map_err(|_| CodexModelsError::cache("Codex model cache epoch is invalid"))?;
        marker.validate()?;
        Ok(Some(marker))
    }

    fn commit_cache(&self, cache: &ModelsCacheFile) -> Result<(), CodexModelsError> {
        cache.validate()?;
        ensure_private_root(&self.state_root)?;
        let target = self.cache_path();
        reject_unsafe_target(&target)?;
        let bytes = serde_json::to_vec(cache)
            .map_err(|_| CodexModelsError::cache("Codex model cache encoding failed"))?;
        let mut random = [0_u8; 8];
        getrandom::getrandom(&mut random)
            .map_err(|_| CodexModelsError::cache("Codex model cache random failed"))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let temp = self
            .state_root
            .join(format!(".{CACHE_FILE}.tmp-{}-{suffix}", std::process::id()));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = options
                .open(&temp)
                .map_err(|_| CodexModelsError::cache("Codex model cache write failed"))?;
            file.write_all(&bytes)
                .map_err(|_| CodexModelsError::cache("Codex model cache write failed"))?;
            file.sync_all()
                .map_err(|_| CodexModelsError::cache("Codex model cache sync failed"))?;
            fs::rename(&temp, &target)
                .map_err(|_| CodexModelsError::cache("Codex model cache publish failed"))?;
            File::open(&self.state_root)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| CodexModelsError::cache("Codex model cache sync failed"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn commit_cache_epoch(&self, marker: &ModelsCacheEpoch) -> Result<(), CodexModelsError> {
        marker.validate()?;
        ensure_private_root(&self.state_root)?;
        let target = self.epoch_path();
        reject_unsafe_target(&target)?;
        let bytes = serde_json::to_vec(marker)
            .map_err(|_| CodexModelsError::cache("Codex model cache epoch encoding failed"))?;
        let mut random = [0_u8; 8];
        getrandom::getrandom(&mut random)
            .map_err(|_| CodexModelsError::cache("Codex model cache random failed"))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let temp = self.state_root.join(format!(
            ".{CACHE_EPOCH_FILE}.tmp-{}-{suffix}",
            std::process::id()
        ));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = options
                .open(&temp)
                .map_err(|_| CodexModelsError::cache("Codex model cache epoch write failed"))?;
            file.write_all(&bytes)
                .map_err(|_| CodexModelsError::cache("Codex model cache epoch write failed"))?;
            file.sync_all()
                .map_err(|_| CodexModelsError::cache("Codex model cache epoch sync failed"))?;
            fs::rename(&temp, &target)
                .map_err(|_| CodexModelsError::cache("Codex model cache epoch publish failed"))?;
            File::open(&self.state_root)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| CodexModelsError::cache("Codex model cache epoch sync failed"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn remove_cache(&self) -> Result<(), CodexModelsError> {
        let target = self.cache_path();
        reject_unsafe_target(&target)?;
        match fs::remove_file(&target) {
            Ok(()) => File::open(&self.state_root)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| CodexModelsError::cache("Codex model cache sync failed")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(CodexModelsError::cache(
                "Codex model cache invalidation failed",
            )),
        }
    }
}

struct CacheFileLock(File);

impl Drop for CacheFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

enum FetchResult {
    Live {
        models: Vec<CachedModel>,
        etag: Option<String>,
    },
    NotModified,
}

fn parse_live_response(mut response: Response) -> Result<FetchResult, CodexModelsError> {
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_MODELS_BODY_BYTES)
    {
        return Err(CodexModelsError::protocol(
            "Codex model catalog response is too large",
        ));
    }
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= MAX_ETAG_BYTES && !value.chars().any(char::is_control))
        .map(str::to_string);
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(MAX_MODELS_BODY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CodexModelsError::network())?;
    if bytes.len() as u64 > MAX_MODELS_BODY_BYTES {
        return Err(CodexModelsError::protocol(
            "Codex model catalog response is too large",
        ));
    }
    let official = parse_official_models(&bytes)?;
    if official.len() > MAX_MODELS {
        return Err(CodexModelsError::protocol(
            "Codex model catalog has too many entries",
        ));
    }
    let mut models: Vec<CachedModel> = official
        .into_iter()
        .filter(|model| model.visibility == "list")
        .map(|model| {
            let supports_reasoning_summary = model.supports_reasoning_summary();
            CachedModel {
                id: model.slug,
                display_name: model.display_name,
                priority: model.priority,
                default_reasoning_effort: model.default_reasoning_level,
                supported_reasoning_efforts: model
                    .supported_reasoning_levels
                    .into_iter()
                    .map(|level| level.effort)
                    .collect(),
                supports_reasoning_summary,
                supports_parallel_tool_calls: model.supports_parallel_tool_calls,
                use_responses_lite: model.use_responses_lite,
            }
        })
        .collect();
    if models.is_empty() {
        return Err(CodexModelsError::protocol(
            "Codex model catalog contains no visible models",
        ));
    }
    models.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.id.cmp(&right.id))
    });
    let cache = ModelsCacheFile {
        version: CACHE_VERSION,
        auth_epoch: "0".repeat(32),
        auth_generation: 0,
        account_hash: "0".repeat(32),
        fetched_at: 0,
        etag: etag.clone(),
        models: models.clone(),
    };
    cache
        .validate()
        .map_err(|_| CodexModelsError::protocol("Codex model catalog metadata is invalid"))?;
    Ok(FetchResult::Live { models, etag })
}

fn parse_official_models(bytes: &[u8]) -> Result<Vec<OfficialModel>, CodexModelsError> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|_| CodexModelsError::protocol("Codex model catalog response is not JSON"))?;
    let entries = root
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CodexModelsError::protocol("Codex model catalog response has no models array")
        })?;
    entries
        .iter()
        .map(|entry| {
            serde_json::from_value(entry.clone())
                .map_err(|_| CodexModelsError::protocol(official_model_schema_error(entry)))
        })
        .collect()
}

fn official_model_schema_error(entry: &Value) -> &'static str {
    let Some(object) = entry.as_object() else {
        return "Codex model catalog contains a non-object entry";
    };
    for field in ["slug", "display_name", "visibility"] {
        if !object.get(field).is_some_and(Value::is_string) {
            return match field {
                "slug" => "Codex model catalog slug field is invalid",
                "display_name" => "Codex model catalog display_name field is invalid",
                _ => "Codex model catalog visibility field is invalid",
            };
        }
    }
    if object
        .get("priority")
        .and_then(Value::as_i64)
        .is_none_or(|priority| i32::try_from(priority).is_err())
    {
        return "Codex model catalog priority field is invalid";
    }
    if object
        .get("default_reasoning_level")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return "Codex model catalog default reasoning field is invalid";
    }
    if let Some(levels) = object.get("supported_reasoning_levels") {
        let Some(levels) = levels.as_array() else {
            return "Codex model catalog reasoning levels field is invalid";
        };
        if levels.iter().any(|level| {
            !level
                .as_object()
                .and_then(|object| object.get("effort"))
                .is_some_and(Value::is_string)
        }) {
            return "Codex model catalog reasoning level entry is invalid";
        }
    }
    for field in [
        "supports_reasoning_summaries",
        "supports_reasoning_summary_parameter",
        "supports_parallel_tool_calls",
        "use_responses_lite",
    ] {
        if object.get(field).is_some_and(|value| !value.is_boolean()) {
            return "Codex model catalog capability field is invalid";
        }
    }
    "Codex model catalog entry schema is incompatible"
}

fn snapshot_from_cache(
    cache: &ModelsCacheFile,
    source: CatalogSource,
    now: u64,
) -> CodexModelsSnapshot {
    CodexModelsSnapshot {
        models: cache.models.clone(),
        source,
        age_seconds: cache.age_at(now).unwrap_or_default(),
        published_generation: 0,
    }
}

fn validate_model(model: &CachedModel) -> Result<(), CodexModelsError> {
    if model.id.is_empty()
        || model.id.len() > MAX_MODEL_ID_BYTES
        || !model.id.is_ascii()
        || !model
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        || model.display_name.is_empty()
        || model.display_name.len() > MAX_DISPLAY_NAME_BYTES
        || model.display_name.chars().any(char::is_control)
        || model.supported_reasoning_efforts.len() > MAX_REASONING_LEVELS
    {
        return Err(CodexModelsError::cache("Codex model cache is invalid"));
    }
    let mut efforts = HashSet::new();
    for effort in &model.supported_reasoning_efforts {
        if !valid_reasoning_effort(effort) || !efforts.insert(effort.as_str()) {
            return Err(CodexModelsError::cache("Codex model cache is invalid"));
        }
    }
    if let Some(default) = model.default_reasoning_effort.as_deref() {
        if !valid_reasoning_effort(default)
            || (!model.supported_reasoning_efforts.is_empty() && !efforts.contains(default))
        {
            return Err(CodexModelsError::cache("Codex model cache is invalid"));
        }
    }
    Ok(())
}

fn valid_reasoning_effort(effort: &str) -> bool {
    !effort.is_empty()
        && effort.len() <= MAX_REASONING_EFFORT_BYTES
        && effort.is_ascii()
        && effort
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn retryable(error: &CodexModelsError) -> bool {
    error.error_kind == "network" || matches!(error.upstream_status, Some(408 | 429 | 500..=599))
}

fn stale_eligible(error: &CodexModelsError) -> bool {
    retryable(error)
}

fn unix_time_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn ensure_private_root(root: &Path) -> Result<(), CodexModelsError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(CodexModelsError::cache("Codex model cache root is unsafe")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(root)
                .map_err(|_| CodexModelsError::cache("Codex model cache root is unavailable"))?;
        }
        Err(_) => {
            return Err(CodexModelsError::cache(
                "Codex model cache root is unavailable",
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .map_err(|_| CodexModelsError::cache("Codex model cache root is unavailable"))?;
        let metadata = fs::symlink_metadata(root)
            .map_err(|_| CodexModelsError::cache("Codex model cache root is unavailable"))?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(CodexModelsError::cache("Codex model cache root is unsafe"));
        }
    }
    Ok(())
}

fn validate_private_root(root: &Path) -> Result<bool, CodexModelsError> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => {
            return Err(CodexModelsError::cache(
                "Codex model cache root is unavailable",
            ));
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(CodexModelsError::cache("Codex model cache root is unsafe"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o700
        {
            return Err(CodexModelsError::cache("Codex model cache root is unsafe"));
        }
    }
    Ok(true)
}

fn reject_unsafe_target(path: &Path) -> Result<(), CodexModelsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(CodexModelsError::cache("Codex model cache path is unsafe")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(CodexModelsError::cache(
            "Codex model cache path is unavailable",
        )),
    }
}

fn validate_private_file(file: &File) -> Result<(), CodexModelsError> {
    let metadata = file
        .metadata()
        .map_err(|_| CodexModelsError::cache("Codex model cache is unavailable"))?;
    if !metadata.is_file() || metadata.len() > MAX_CACHE_BYTES {
        return Err(CodexModelsError::cache("Codex model cache is invalid"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(CodexModelsError::cache("Codex model cache is unsafe"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use serde_json::{json, Value};

    use super::{
        parse_official_models, science_model_alias, CatalogSource, CodexModelCatalog,
        ModelsCacheFile, CACHE_EPOCH_FILE, CACHE_FILE,
    };
    use crate::codex_auth::InferenceSecrets;

    fn bind_loopback() -> TcpListener {
        loop {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            if listener.local_addr().unwrap().port() != 8765 {
                return listener;
            }
        }
    }

    fn private_root() -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let mut random = [0_u8; 8];
        getrandom::getrandom(&mut random).unwrap();
        let suffix = u64::from_ne_bytes(random);
        let root = std::env::temp_dir().join(format!(
            "csswitch-codex-models-{}-{}",
            std::process::id(),
            suffix
        ));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return request;
            }
        }
    }

    fn response(status: &str, body: &[u8], extra_headers: &str) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra_headers}connection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn model_body(ids: &[&str]) -> Vec<u8> {
        let models: Vec<Value> = ids
            .iter()
            .enumerate()
            .map(|(priority, id)| {
                json!({
                    "slug": id,
                    "display_name": format!("Display {id}"),
                    "visibility": "list",
                    "supported_in_api": true,
                    "priority": priority,
                    "default_reasoning_level": "medium",
                    "supported_reasoning_levels": [{"effort": "medium", "description": "default"}],
                    "supports_reasoning_summary_parameter": true,
                    "supports_parallel_tool_calls": true,
                    "use_responses_lite": false,
                })
            })
            .collect();
        serde_json::to_vec(&json!({"models": models})).unwrap()
    }

    #[test]
    fn official_catalog_parse_errors_are_bounded_and_field_typed() {
        let not_json = parse_official_models(b"not-json").unwrap_err();
        assert_eq!(not_json.detail, "Codex model catalog response is not JSON");

        let missing_models = parse_official_models(br#"{"data":[]}"#).unwrap_err();
        assert_eq!(
            missing_models.detail,
            "Codex model catalog response has no models array"
        );

        let invalid_display = serde_json::to_vec(&json!({"models": [{
            "slug": "gpt-test",
            "display_name": null,
            "visibility": "list",
            "priority": 1
        }]}))
        .unwrap();
        let invalid_display = parse_official_models(&invalid_display).unwrap_err();
        assert_eq!(
            invalid_display.detail,
            "Codex model catalog display_name field is invalid"
        );

        let invalid_levels = serde_json::to_vec(&json!({"models": [{
            "slug": "gpt-test",
            "display_name": "GPT Test",
            "visibility": "list",
            "priority": 1,
            "supported_reasoning_levels": null
        }]}))
        .unwrap();
        let invalid_levels = parse_official_models(&invalid_levels).unwrap_err();
        assert_eq!(
            invalid_levels.detail,
            "Codex model catalog reasoning levels field is invalid"
        );

        for field in [
            "supports_reasoning_summaries",
            "supports_reasoning_summary_parameter",
        ] {
            for invalid in [Value::Null, json!("true"), json!(1)] {
                let mut entry = json!({
                    "slug": "gpt-test",
                    "display_name": "GPT Test",
                    "visibility": "list",
                    "priority": 1
                });
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert(field.to_string(), invalid);
                let body = serde_json::to_vec(&json!({"models": [entry]})).unwrap();
                let error = parse_official_models(&body).unwrap_err();
                assert_eq!(
                    error.detail,
                    "Codex model catalog capability field is invalid"
                );
            }
        }
    }

    #[test]
    fn official_summary_capability_prefers_canonical_then_legacy_then_default() {
        let entry = |slug: &str, capabilities: Value| {
            let mut entry = json!({
                "slug": slug,
                "display_name": slug,
                "visibility": "list",
                "priority": 1
            });
            entry.as_object_mut().unwrap().extend(
                capabilities
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
            entry
        };
        let body = serde_json::to_vec(&json!({"models": [
            entry("canonical-only", json!({"supports_reasoning_summaries": false})),
            entry("legacy-only", json!({"supports_reasoning_summary_parameter": false})),
            entry("canonical-true", json!({
                "supports_reasoning_summaries": true,
                "supports_reasoning_summary_parameter": false
            })),
            entry("canonical-false", json!({
                "supports_reasoning_summaries": false,
                "supports_reasoning_summary_parameter": true
            })),
            entry("missing", json!({}))
        ]}))
        .unwrap();

        let models = parse_official_models(&body).unwrap();
        let normalized: Vec<(&str, bool)> = models
            .iter()
            .map(|model| (model.slug.as_str(), model.supports_reasoning_summary()))
            .collect();
        assert_eq!(
            normalized,
            [
                ("canonical-only", false),
                ("legacy-only", false),
                ("canonical-true", true),
                ("canonical-false", false),
                ("missing", true),
            ]
        );
    }

    type MockModelsServer = (
        String,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<Vec<u8>>>>,
        thread::JoinHandle<()>,
    );

    fn serve_responses(responses: Vec<Vec<u8>>) -> MockModelsServer {
        let listener = bind_loopback();
        let address = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let count_for_server = Arc::clone(&count);
        let requests_for_server = Arc::clone(&requests);
        let server = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                requests_for_server
                    .lock()
                    .unwrap()
                    .push(read_request(&mut stream));
                count_for_server.fetch_add(1, Ordering::SeqCst);
                stream.write_all(&response).unwrap();
                stream.flush().unwrap();
            }
        });
        (format!("http://{address}/models"), count, requests, server)
    }

    fn secrets() -> InferenceSecrets {
        InferenceSecrets::for_test("access-secret", "account-secret")
    }

    fn published_generation(snapshot: &super::CodexModelsSnapshot) -> u64 {
        snapshot.response_body()["diagnostics"]["generation"]
            .as_u64()
            .unwrap()
    }

    #[test]
    fn explicit_refresh_replaces_shared_snapshot_and_failures_keep_last_generation() {
        let root = private_root();
        let (endpoint, _count, _requests, server) =
            serve_responses(vec![response("200 OK", &model_body(&["gpt-old"]), "")]);
        let formal = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        let first = formal.published_snapshot(&secrets()).unwrap();
        assert!(first.contains_raw("gpt-old"));
        assert_eq!(published_generation(&first), 1);
        server.join().unwrap();

        let (endpoint, _count, _requests, server) =
            serve_responses(vec![response("200 OK", &model_body(&["gpt-new"]), "")]);
        let scratch = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        let refreshed = scratch.refresh_published_snapshot(&secrets()).unwrap();
        assert!(refreshed.contains_raw("gpt-new"));
        server.join().unwrap();

        let second = formal.published_snapshot(&secrets()).unwrap();
        assert!(second.contains_raw("gpt-new"));
        assert!(!second.contains_raw("gpt-old"));
        assert_eq!(published_generation(&second), 2);

        let (endpoint, _count, _requests, server) = serve_responses(vec![
            response("503 Service Unavailable", b"", ""),
            response("503 Service Unavailable", b"", ""),
            response("503 Service Unavailable", b"", ""),
        ]);
        let failed_scratch = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        let stale = failed_scratch
            .refresh_published_snapshot(&secrets())
            .unwrap();
        assert_eq!(stale.source(), CatalogSource::StaleCache);
        assert!(stale.contains_raw("gpt-new"));
        server.join().unwrap();

        let unchanged = formal.published_snapshot(&secrets()).unwrap();
        assert!(unchanged.contains_raw("gpt-new"));
        assert_eq!(published_generation(&unchanged), 2);

        let (endpoint, _count, _requests, server) =
            serve_responses(vec![response("200 OK", &model_body(&[]), "")]);
        let empty_scratch = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        let error = empty_scratch
            .refresh_published_snapshot(&secrets())
            .unwrap_err();
        assert_eq!(error.error_kind, "protocol");
        server.join().unwrap();
        let still_unchanged = formal.published_snapshot(&secrets()).unwrap();
        assert!(still_unchanged.contains_raw("gpt-new"));
        assert_eq!(published_generation(&still_unchanged), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn late_refresh_cannot_publish_over_a_newer_cross_process_cache_epoch() {
        let root = private_root();
        let listener = bind_loopback();
        let address = listener.local_addr().unwrap();
        let (late_started_tx, late_started_rx) = std::sync::mpsc::channel();
        let (release_late_tx, release_late_rx) = std::sync::mpsc::channel();
        let formal_server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            read_request(&mut first);
            first
                .write_all(&response("200 OK", &model_body(&["gpt-old"]), ""))
                .unwrap();
            first.flush().unwrap();

            let (mut late, _) = listener.accept().unwrap();
            read_request(&mut late);
            late_started_tx.send(()).unwrap();
            release_late_rx.recv().unwrap();
            late.write_all(&response("200 OK", &model_body(&["gpt-late-old"]), ""))
                .unwrap();
            late.flush().unwrap();
        });
        let formal = Arc::new(
            CodexModelCatalog::for_test(format!("http://{address}/models"), root.clone()).unwrap(),
        );
        let initial = formal.published_snapshot(&secrets()).unwrap();
        assert_eq!(published_generation(&initial), 1);

        let formal_for_late = Arc::clone(&formal);
        let late = thread::spawn(move || formal_for_late.refresh_published_snapshot(&secrets()));
        late_started_rx.recv().unwrap();

        let (endpoint, _count, _requests, external_server) =
            serve_responses(vec![response("200 OK", &model_body(&["gpt-new"]), "")]);
        CodexModelCatalog::for_test(endpoint, root.clone())
            .unwrap()
            .refresh_published_snapshot(&secrets())
            .unwrap();
        external_server.join().unwrap();

        release_late_tx.send(()).unwrap();
        let error = late.join().unwrap().unwrap_err();
        assert_eq!(error.error_kind, "cache_invalidated");
        formal_server.join().unwrap();

        let current = formal.published_snapshot(&secrets()).unwrap();
        assert!(current.contains_raw("gpt-new"));
        assert!(!current.contains_raw("gpt-late-old"));
        assert_eq!(published_generation(&current), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn live_catalog_uses_official_headers_filters_and_caches() {
        let body = serde_json::to_vec(&json!({"models": [
            {"slug":"hidden","display_name":"Hidden","visibility":"hide","supported_in_api":true,"priority":0,"default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"}],"supports_reasoning_summaries":false,"supports_parallel_tool_calls":false},
            {"slug":"chatgpt-only","display_name":"ChatGPT Only","visibility":"list","supported_in_api":false,"priority":1,"default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"}],"supports_reasoning_summaries":false,"supports_parallel_tool_calls":false},
            {"slug":"gpt-b","display_name":"B","visibility":"list","supported_in_api":true,"priority":3,"default_reasoning_level":"high","supported_reasoning_levels":[{"effort":"medium"},{"effort":"high"}],"supports_reasoning_summaries":true,"supports_parallel_tool_calls":true,"use_responses_lite":true},
            {"slug":"gpt-a","display_name":"A","visibility":"list","supported_in_api":true,"priority":2,"default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"medium"}],"supports_reasoning_summaries":true,"supports_parallel_tool_calls":false}
        ]})).unwrap();
        let (endpoint, count, requests, server) =
            serve_responses(vec![response("200 OK", &body, "etag: \"catalog-v1\"\r\n")]);
        let root = private_root();
        let catalog = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        let snapshot = catalog.list_at(&secrets(), 10_000).unwrap();
        assert_eq!(snapshot.source(), CatalogSource::Live);
        assert!(snapshot.contains_raw("gpt-a"));
        assert!(snapshot.contains_raw("gpt-b"));
        assert!(snapshot.contains_raw("chatgpt-only"));
        assert!(!snapshot.contains_raw("hidden"));
        assert_eq!(
            snapshot.response_body()["first_id"],
            "claude-csswitch-codex-chatgpt-only"
        );
        let chatgpt_only = snapshot
            .resolve_science_model("claude-csswitch-codex-chatgpt-only")
            .unwrap();
        assert!(!chatgpt_only.supports_reasoning_summary());
        assert_eq!(
            snapshot.response_body()["data"][0]["display_name"],
            "Codex / ChatGPT Only"
        );
        let gpt_a = snapshot
            .resolve_science_model("claude-csswitch-codex-gpt-a")
            .unwrap();
        assert_eq!(gpt_a.raw_id(), "gpt-a");
        assert_eq!(gpt_a.default_reasoning_effort(), Some("medium"));
        assert!(gpt_a.supports_reasoning_summary());
        assert!(!gpt_a.supports_parallel_tool_calls());
        assert!(snapshot.resolve_science_model("gpt-a").is_none());
        assert!(snapshot
            .resolve_science_model("claude-csswitch-codex-hidden")
            .is_none());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let request = String::from_utf8_lossy(&requests.lock().unwrap()[0]).to_ascii_lowercase();
        assert!(request.contains("authorization: bearer access-secret"));
        assert!(request.contains("chatgpt-account-id: account-secret"));
        assert!(request.contains("originator: codex_cli_rs"));
        server.join().unwrap();

        let cache: ModelsCacheFile =
            serde_json::from_slice(&fs::read(root.join(CACHE_FILE)).unwrap()).unwrap();
        assert_eq!(cache.etag.as_deref(), Some("\"catalog-v1\""));
        let cached_b = cache
            .models
            .iter()
            .find(|model| model.id == "gpt-b")
            .unwrap();
        assert_eq!(cached_b.default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(cached_b.supported_reasoning_efforts, ["medium", "high"]);
        assert!(cached_b.supports_reasoning_summary);
        assert!(cached_b.supports_parallel_tool_calls);
        assert!(cached_b.use_responses_lite);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.join(CACHE_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let text = serde_json::to_string(&cache).unwrap();
        assert!(!text.contains("access-secret"));
        assert!(!text.contains("account-secret"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_official_summary_capability_uses_pinned_true_default() {
        let body = serde_json::to_vec(&json!({"models": [{
            "slug": "gpt-defaults",
            "display_name": "Defaults",
            "visibility": "list",
            "supported_in_api": false,
            "priority": 1
        }]}))
        .unwrap();
        let (endpoint, _count, _requests, server) =
            serve_responses(vec![response("200 OK", &body, "")]);
        let root = private_root();
        let snapshot = CodexModelCatalog::for_test(endpoint, root.clone())
            .unwrap()
            .list_at(&secrets(), 10_000)
            .unwrap();
        let model = snapshot
            .resolve_science_model("claude-csswitch-codex-gpt-defaults")
            .unwrap();
        assert_eq!(model.default_reasoning_effort(), None);
        assert!(model.supports_reasoning_summary());
        assert!(!model.supports_parallel_tool_calls());
        let cache: ModelsCacheFile =
            serde_json::from_slice(&fs::read(root.join(CACHE_FILE)).unwrap()).unwrap();
        assert!(cache.models[0].supports_reasoning_summary);
        assert!(!cache.models[0].supports_parallel_tool_calls);
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn gpt_5_6_visible_models_keep_priority_display_alias_and_reverse_mapping() {
        let body = serde_json::to_vec(&json!({"models": [
            {
                "slug": "gpt-5.6-luna",
                "display_name": "GPT-5.6-Luna",
                "visibility": "list",
                "priority": 3,
                "supports_reasoning_summaries": true,
                "use_responses_lite": true
            },
            {
                "slug": "gpt-5.6-hidden",
                "display_name": "GPT-5.6-Hidden",
                "visibility": "hide",
                "priority": 0,
                "supports_reasoning_summaries": true
            },
            {
                "slug": "gpt-5.6-sol",
                "display_name": "GPT-5.6-Sol",
                "visibility": "list",
                "priority": 1,
                "supports_reasoning_summaries": true,
                "use_responses_lite": true
            },
            {
                "slug": "gpt-5.6-terra",
                "display_name": "GPT-5.6-Terra",
                "visibility": "list",
                "priority": 2,
                "supports_reasoning_summaries": true,
                "use_responses_lite": true
            }
        ]}))
        .unwrap();
        let (endpoint, _count, _requests, server) =
            serve_responses(vec![response("200 OK", &body, "")]);
        let root = private_root();
        let snapshot = CodexModelCatalog::for_test(endpoint, root.clone())
            .unwrap()
            .list_at(&secrets(), 10_000)
            .unwrap();
        server.join().unwrap();

        let response = snapshot.response_body();
        let data = response["data"].as_array().unwrap();
        let ids: Vec<&str> = data
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "claude-csswitch-codex-gpt-5.6-sol",
                "claude-csswitch-codex-gpt-5.6-terra",
                "claude-csswitch-codex-gpt-5.6-luna",
            ]
        );
        assert_eq!(data[0]["display_name"], "Codex / GPT-5.6-Sol");
        assert_eq!(data[1]["display_name"], "Codex / GPT-5.6-Terra");
        assert_eq!(data[2]["display_name"], "Codex / GPT-5.6-Luna");
        for raw_id in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let alias = science_model_alias(raw_id);
            assert_eq!(
                snapshot.resolve_science_model(&alias).unwrap().raw_id(),
                raw_id
            );
            assert!(snapshot
                .resolve_science_model(&alias)
                .unwrap()
                .use_responses_lite());
        }
        assert!(snapshot
            .resolve_science_model("claude-csswitch-codex-gpt-5.6-hidden")
            .is_none());
        assert!(snapshot
            .resolve_science_model("claude-csswitch-codex-gpt-5.6-absent")
            .is_none());
        assert!(snapshot
            .resolve_science_model("claude-csswitch-codex-gpt-5.6-terra")
            .is_some_and(|model| model.supports_reasoning_summary()));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn production_catalog_query_uses_explicit_codex_compat_version() {
        let root = private_root();
        let contract = crate::provider_contracts::load_codex_runtime_contract().unwrap();
        let catalog = CodexModelCatalog::production(root.clone(), &contract).unwrap();
        assert_eq!(
            catalog.endpoint,
            "https://chatgpt.com/backend-api/codex/models?client_version=0.144.4"
        );
        assert!(!catalog.endpoint.ends_with(env!("CARGO_PKG_VERSION")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fresh_cache_avoids_network_and_identity_mismatch_never_reuses_it() {
        let root = private_root();
        let (endpoint, _count, _requests, server) =
            serve_responses(vec![response("200 OK", &model_body(&["gpt-one"]), "")]);
        let catalog = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        catalog.list_at(&secrets(), 20_000).unwrap();
        server.join().unwrap();

        let no_network =
            CodexModelCatalog::for_test("http://127.0.0.1:1/models".into(), root.clone()).unwrap();
        let fresh = no_network.list_at(&secrets(), 20_100).unwrap();
        assert_eq!(fresh.source(), CatalogSource::FreshCache);

        let other = InferenceSecrets::for_test("other-token", "other-account");
        assert!(no_network.list_at(&other, 20_100).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_cache_revalidates_with_etag_and_network_failure_falls_back() {
        let root = private_root();
        let (endpoint, _count, _requests, server) = serve_responses(vec![response(
            "200 OK",
            &model_body(&["gpt-cached"]),
            "etag: cached-tag\r\n",
        )]);
        let catalog = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        catalog.list_at(&secrets(), 30_000).unwrap();
        server.join().unwrap();

        let (endpoint, count, requests, server) =
            serve_responses(vec![response("304 Not Modified", b"", "")]);
        let catalog = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        let revalidated = catalog.list_at(&secrets(), 30_301).unwrap();
        assert_eq!(revalidated.source(), CatalogSource::RevalidatedCache);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(String::from_utf8_lossy(&requests.lock().unwrap()[0])
            .to_ascii_lowercase()
            .contains("if-none-match: cached-tag"));
        server.join().unwrap();

        let no_network =
            CodexModelCatalog::for_test("http://127.0.0.1:1/models".into(), root.clone()).unwrap();
        let stale = no_network.list_at(&secrets(), 30_602).unwrap();
        assert_eq!(stale.source(), CatalogSource::StaleCache);
        assert_eq!(stale.age_seconds(), 301);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retry_policy_is_bounded_and_auth_never_uses_stale_cache() {
        let root = private_root();
        let (endpoint, _count, _requests, server) =
            serve_responses(vec![response("200 OK", &model_body(&["gpt-old"]), "")]);
        CodexModelCatalog::for_test(endpoint, root.clone())
            .unwrap()
            .list_at(&secrets(), 40_000)
            .unwrap();
        server.join().unwrap();

        let (endpoint, count, _requests, server) = serve_responses(vec![
            response("503 Service Unavailable", b"", ""),
            response("429 Too Many Requests", b"", ""),
            response("503 Service Unavailable", b"", ""),
        ]);
        let catalog = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        assert_eq!(
            catalog.list_at(&secrets(), 40_301).unwrap().source(),
            CatalogSource::StaleCache
        );
        assert_eq!(count.load(Ordering::SeqCst), 3);
        server.join().unwrap();

        let (endpoint, count, _requests, server) =
            serve_responses(vec![response("401 Unauthorized", b"", "")]);
        let catalog = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        let error = catalog.list_at(&secrets(), 40_302).unwrap_err();
        assert_eq!(error.status, 401);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(!root.join(CACHE_FILE).exists());
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cross_process_cache_epoch_blocks_aba_late_commit_and_restart_reuse() {
        let root = private_root();
        let (endpoint, _count, _requests, server) =
            serve_responses(vec![response("200 OK", &model_body(&["gpt-old"]), "")]);
        CodexModelCatalog::for_test(endpoint, root.clone())
            .unwrap()
            .list_at(&secrets(), 45_000)
            .unwrap();
        server.join().unwrap();

        let listener = bind_loopback();
        let address = listener.local_addr().unwrap();
        let (request_started_tx, request_started_rx) = std::sync::mpsc::channel();
        let (release_response_tx, release_response_rx) = std::sync::mpsc::channel();
        let late_server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            request_started_tx.send(()).unwrap();
            release_response_rx.recv().unwrap();
            stream
                .write_all(&response("200 OK", &model_body(&["gpt-racy"]), ""))
                .unwrap();
            stream.flush().unwrap();
        });

        let root_for_in_flight = root.clone();
        let in_flight = thread::spawn(move || {
            CodexModelCatalog::for_test(format!("http://{address}/models"), root_for_in_flight)
                .unwrap()
                .list_at(&secrets(), 45_301)
        });
        request_started_rx.recv().unwrap();

        let current = secrets();
        let invalidator =
            CodexModelCatalog::for_test("http://127.0.0.1:1/models".into(), root.clone()).unwrap();
        invalidator.invalidate_identity(
            current.auth_epoch(),
            current.auth_generation(),
            current.account_hash(),
        );
        assert!(!root.join(CACHE_FILE).exists());
        assert!(root.join(CACHE_EPOCH_FILE).exists());

        let restarted =
            CodexModelCatalog::for_test("http://127.0.0.1:1/models".into(), root.clone()).unwrap();
        assert!(restarted.list_at(&secrets(), 45_302).is_err());
        assert!(!root.join(CACHE_FILE).exists());

        let (endpoint, _count, _requests, recovery_server) =
            serve_responses(vec![response("200 OK", &model_body(&["gpt-new"]), "")]);
        let recovered = CodexModelCatalog::for_test(endpoint, root.clone())
            .unwrap()
            .list_at(&secrets(), 45_303)
            .unwrap();
        recovery_server.join().unwrap();
        assert_eq!(recovered.source(), CatalogSource::Live);
        assert!(recovered.contains_raw("gpt-new"));
        assert!(root.join(CACHE_FILE).exists());
        assert!(root.join(CACHE_EPOCH_FILE).exists());

        let restarted_after_recovery =
            CodexModelCatalog::for_test("http://127.0.0.1:1/models".into(), root.clone()).unwrap();
        assert_eq!(
            restarted_after_recovery
                .list_at(&secrets(), 45_304)
                .unwrap()
                .source(),
            CatalogSource::FreshCache
        );

        release_response_tx.send(()).unwrap();
        let race_error = in_flight.join().unwrap().unwrap_err();
        assert_eq!(race_error.error_kind, "cache_invalidated");
        late_server.join().unwrap();
        let cache: ModelsCacheFile =
            serde_json::from_slice(&fs::read(root.join(CACHE_FILE)).unwrap()).unwrap();
        assert_eq!(cache.models[0].id, "gpt-new");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn expired_or_protocol_invalid_catalog_never_uses_stale_cache() {
        let root = private_root();
        let (endpoint, _count, _requests, server) =
            serve_responses(vec![response("200 OK", &model_body(&["gpt-old"]), "")]);
        CodexModelCatalog::for_test(endpoint, root.clone())
            .unwrap()
            .list_at(&secrets(), 50_000)
            .unwrap();
        server.join().unwrap();

        let expired =
            CodexModelCatalog::for_test("http://127.0.0.1:1/models".into(), root.clone()).unwrap();
        assert!(expired
            .list_at(&secrets(), 50_000 + super::STALE_TTL_SECONDS + 1)
            .is_err());

        let (endpoint, count, _requests, server) =
            serve_responses(vec![response("200 OK", b"not-json", "")]);
        let invalid = CodexModelCatalog::for_test(endpoint, root.clone()).unwrap();
        let error = invalid.list_at(&secrets(), 50_301).unwrap_err();
        assert_eq!(error.error_kind, "protocol");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn cache_symlink_is_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = private_root();
        let outside = root.with_extension("outside");
        fs::write(&outside, b"outside-safe").unwrap();
        symlink(&outside, root.join(CACHE_FILE)).unwrap();
        let catalog =
            CodexModelCatalog::for_test("http://127.0.0.1:1/models".into(), root.clone()).unwrap();
        let error = catalog.list_at(&secrets(), 60_000).unwrap_err();
        assert_eq!(error.error_kind, "cache");
        assert_eq!(fs::read(&outside).unwrap(), b"outside-safe");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(outside);
    }
}
