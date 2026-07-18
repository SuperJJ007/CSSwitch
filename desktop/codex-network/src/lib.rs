use std::collections::BTreeMap;
use std::env;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Host;

pub const ROUTE_ENV: &str = "CSSWITCH_CODEX_NETWORK_ROUTE_V1";
const MAX_PROXY_URL_BYTES: usize = 2_048;
const MAX_NO_PROXY_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexNetworkMode {
    #[default]
    Auto,
    Custom,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct CodexNetworkSettings {
    pub mode: CodexNetworkMode,
    pub proxy_url: String,
    /// 前向兼容透传；runtime 只解释已知字段。
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteSource {
    Direct,
    EnvHttps,
    EnvAll,
    Custom,
}

impl RouteSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::EnvHttps => "env_https",
            Self::EnvAll => "env_all",
            Self::Custom => "custom",
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedCodexNetworkRoute {
    pub source: RouteSource,
    pub proxy_scheme: Option<String>,
    pub proxy_url: Option<String>,
    pub no_proxy: Option<String>,
    pub fingerprint: String,
}

impl fmt::Debug for ResolvedCodexNetworkRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedCodexNetworkRoute")
            .field("source", &self.source)
            .field("proxy_scheme", &self.proxy_scheme)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteErrorKind {
    ProxyConfigInvalid,
    EncodedRouteInvalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouteError {
    pub kind: RouteErrorKind,
    detail: &'static str,
}

impl RouteError {
    fn proxy(detail: &'static str) -> Self {
        Self {
            kind: RouteErrorKind::ProxyConfigInvalid,
            detail,
        }
    }

    fn encoded(detail: &'static str) -> Self {
        Self {
            kind: RouteErrorKind::EncodedRouteInvalid,
            detail,
        }
    }

    pub fn code(self) -> &'static str {
        match self.kind {
            RouteErrorKind::ProxyConfigInvalid => "proxy_config_invalid",
            RouteErrorKind::EncodedRouteInvalid => "encoded_route_invalid",
        }
    }
}

impl fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.detail)
    }
}

impl std::error::Error for RouteError {}

#[derive(Clone, Default)]
pub struct EnvironmentSnapshot {
    pub https_proxy: Option<String>,
    pub https_proxy_lower: Option<String>,
    pub all_proxy: Option<String>,
    pub all_proxy_lower: Option<String>,
    pub no_proxy: Option<String>,
    pub no_proxy_lower: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct RouteEnvelope {
    schema_version: u32,
    route: ResolvedCodexNetworkRoute,
}

pub fn resolve_from_process(
    settings: &CodexNetworkSettings,
) -> Result<ResolvedCodexNetworkRoute, RouteError> {
    resolve(settings, &read_environment()?)
}

pub fn direct_route() -> ResolvedCodexNetworkRoute {
    build_route(RouteSource::Direct, None, None, None)
}

pub fn resolve(
    settings: &CodexNetworkSettings,
    environment: &EnvironmentSnapshot,
) -> Result<ResolvedCodexNetworkRoute, RouteError> {
    match settings.mode {
        CodexNetworkMode::Custom => {
            let (scheme, canonical) = validate_proxy_url(&settings.proxy_url)?;
            Ok(build_route(
                RouteSource::Custom,
                Some(scheme),
                Some(canonical),
                None,
            ))
        }
        CodexNetworkMode::Auto => {
            let selected = [
                (RouteSource::EnvHttps, environment.https_proxy.as_deref()),
                (
                    RouteSource::EnvHttps,
                    environment.https_proxy_lower.as_deref(),
                ),
                (RouteSource::EnvAll, environment.all_proxy.as_deref()),
                (RouteSource::EnvAll, environment.all_proxy_lower.as_deref()),
            ]
            .into_iter()
            .find_map(|(source, value)| {
                value.filter(|value| !value.is_empty()).map(|v| (source, v))
            });
            let Some((source, proxy_url)) = selected else {
                return Ok(direct_route());
            };
            let (scheme, canonical) = validate_proxy_url(proxy_url)?;
            let no_proxy = environment
                .no_proxy
                .as_deref()
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    environment
                        .no_proxy_lower
                        .as_deref()
                        .filter(|value| !value.is_empty())
                })
                .map(validate_no_proxy)
                .transpose()?;
            Ok(build_route(source, Some(scheme), Some(canonical), no_proxy))
        }
    }
}

pub fn encode_route(route: &ResolvedCodexNetworkRoute) -> Result<String, RouteError> {
    validate_decoded_route(route)?;
    serde_json::to_string(&RouteEnvelope {
        schema_version: 1,
        route: route.clone(),
    })
    .map_err(|_| RouteError::encoded("Codex 网络路由编码失败。"))
}

pub fn decode_route(value: &str) -> Result<ResolvedCodexNetworkRoute, RouteError> {
    if value.len() > 8_192 || value.chars().any(char::is_control) {
        return Err(RouteError::encoded("Codex 网络路由环境值非法。"));
    }
    let envelope: RouteEnvelope = serde_json::from_str(value)
        .map_err(|_| RouteError::encoded("Codex 网络路由环境值无法解析。"))?;
    if envelope.schema_version != 1 {
        return Err(RouteError::encoded("Codex 网络路由版本不受支持。"));
    }
    validate_decoded_route(&envelope.route)?;
    Ok(envelope.route)
}

pub fn route_from_process_env() -> Result<ResolvedCodexNetworkRoute, RouteError> {
    match env::var_os(ROUTE_ENV) {
        Some(value) => value
            .to_str()
            .ok_or_else(|| RouteError::encoded("Codex 网络路由环境值不是 UTF-8。"))
            .and_then(decode_route),
        None => resolve_from_process(&CodexNetworkSettings::default()),
    }
}

fn read_environment() -> Result<EnvironmentSnapshot, RouteError> {
    fn read(name: &str) -> Result<Option<String>, RouteError> {
        env::var_os(name)
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| RouteError::proxy("Codex 代理环境变量不是 UTF-8。"))
            })
            .transpose()
    }
    Ok(EnvironmentSnapshot {
        https_proxy: read("HTTPS_PROXY")?,
        https_proxy_lower: read("https_proxy")?,
        all_proxy: read("ALL_PROXY")?,
        all_proxy_lower: read("all_proxy")?,
        no_proxy: read("NO_PROXY")?,
        no_proxy_lower: read("no_proxy")?,
    })
}

fn validate_proxy_url(value: &str) -> Result<(String, String), RouteError> {
    if value.is_empty() || value.len() > MAX_PROXY_URL_BYTES || value.chars().any(char::is_control)
    {
        return Err(RouteError::proxy(
            "Codex 代理 URL 为空、过长或包含控制字符。",
        ));
    }
    let parsed =
        url::Url::parse(value).map_err(|_| RouteError::proxy("Codex 代理 URL 无法解析。"))?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h") {
        return Err(RouteError::proxy("Codex 代理 scheme 不受支持。"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(RouteError::proxy("Codex 代理不支持用户名或密码。"));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() || !matches!(parsed.path(), "" | "/")
    {
        return Err(RouteError::proxy("Codex 代理 URL 只允许根路径。"));
    }
    let authority = value
        .split_once("://")
        .map(|(_, suffix)| suffix.split(['/', '?', '#']).next().unwrap_or_default())
        .ok_or_else(|| RouteError::proxy("Codex 代理 URL 缺少 authority。"))?;
    if authority.contains('@') {
        return Err(RouteError::proxy("Codex 代理不支持 userinfo。"));
    }
    let (authority_host, port_text) = authority
        .rsplit_once(':')
        .ok_or_else(|| RouteError::proxy("Codex 代理 URL 必须包含显式端口。"))?;
    if authority_host.is_empty()
        || port_text.is_empty()
        || !port_text.chars().all(|ch| ch.is_ascii_digit())
    {
        return Err(RouteError::proxy("Codex 代理 URL 必须包含显式端口。"));
    }
    let port = port_text
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| RouteError::proxy("Codex 代理端口非法。"))?;
    let host = match parsed
        .host()
        .ok_or_else(|| RouteError::proxy("Codex 代理 URL 缺少 host。"))?
    {
        Host::Domain(domain) => domain.to_string(),
        Host::Ipv4(address) => address.to_string(),
        Host::Ipv6(address) => format!("[{address}]"),
    };
    Ok((scheme.clone(), format!("{scheme}://{host}:{port}/")))
}

fn validate_no_proxy(value: &str) -> Result<String, RouteError> {
    if value.len() > MAX_NO_PROXY_BYTES || value.chars().any(char::is_control) {
        return Err(RouteError::proxy("NO_PROXY 过长或包含控制字符。"));
    }
    let entries = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    if entries.iter().any(|entry| entry.len() > 512) {
        return Err(RouteError::proxy("NO_PROXY 条目过长。"));
    }
    Ok(entries.join(","))
}

fn build_route(
    source: RouteSource,
    proxy_scheme: Option<String>,
    proxy_url: Option<String>,
    no_proxy: Option<String>,
) -> ResolvedCodexNetworkRoute {
    let fingerprint = fingerprint(
        source,
        proxy_scheme.as_deref(),
        proxy_url.as_deref(),
        no_proxy.as_deref(),
    );
    ResolvedCodexNetworkRoute {
        source,
        proxy_scheme,
        proxy_url,
        no_proxy,
        fingerprint,
    }
}

fn fingerprint(
    source: RouteSource,
    proxy_scheme: Option<&str>,
    proxy_url: Option<&str>,
    no_proxy: Option<&str>,
) -> String {
    let mut digest = Sha256::new();
    for part in [
        source.as_str(),
        proxy_scheme.unwrap_or_default(),
        proxy_url.unwrap_or_default(),
        no_proxy.unwrap_or_default(),
    ] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn validate_decoded_route(route: &ResolvedCodexNetworkRoute) -> Result<(), RouteError> {
    match route.source {
        RouteSource::Direct => {
            if route.proxy_scheme.is_some() || route.proxy_url.is_some() || route.no_proxy.is_some()
            {
                return Err(RouteError::encoded("直接网络路由包含了代理字段。"));
            }
        }
        RouteSource::Custom => {
            if route.no_proxy.is_some() {
                return Err(RouteError::encoded("自定义代理路由不得包含 NO_PROXY。"));
            }
            validate_decoded_proxy(route)?;
        }
        RouteSource::EnvHttps | RouteSource::EnvAll => {
            validate_decoded_proxy(route)?;
            if let Some(no_proxy) = &route.no_proxy {
                if validate_no_proxy(no_proxy)
                    .map_err(|_| RouteError::encoded("Codex 网络路由中的 NO_PROXY 非法。"))?
                    != *no_proxy
                {
                    return Err(RouteError::encoded(
                        "Codex 网络路由中的 NO_PROXY 未规范化。",
                    ));
                }
            }
        }
    }
    let expected = fingerprint(
        route.source,
        route.proxy_scheme.as_deref(),
        route.proxy_url.as_deref(),
        route.no_proxy.as_deref(),
    );
    if expected != route.fingerprint {
        return Err(RouteError::encoded("Codex 网络路由指纹不匹配。"));
    }
    Ok(())
}

fn validate_decoded_proxy(route: &ResolvedCodexNetworkRoute) -> Result<(), RouteError> {
    let proxy_url = route
        .proxy_url
        .as_deref()
        .ok_or_else(|| RouteError::encoded("代理网络路由缺少 URL。"))?;
    let (scheme, canonical) = validate_proxy_url(proxy_url)
        .map_err(|_| RouteError::encoded("代理网络路由 URL 非法。"))?;
    if route.proxy_scheme.as_deref() != Some(scheme.as_str()) || canonical != proxy_url {
        return Err(RouteError::encoded("代理网络路由未规范化。"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto() -> CodexNetworkSettings {
        CodexNetworkSettings::default()
    }

    #[test]
    fn auto_uses_only_https_then_all_proxy_and_normalizes_no_proxy() {
        let route = resolve(
            &auto(),
            &EnvironmentSnapshot {
                https_proxy: Some("http://proxy.example:8080".into()),
                https_proxy_lower: Some("http://ignored.example:8081".into()),
                all_proxy: Some("socks5h://ignored.example:1080".into()),
                no_proxy: Some(" localhost, 127.0.0.1 ".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(route.source, RouteSource::EnvHttps);
        assert_eq!(route.proxy_scheme.as_deref(), Some("http"));
        assert_eq!(
            route.proxy_url.as_deref(),
            Some("http://proxy.example:8080/")
        );
        assert_eq!(route.no_proxy.as_deref(), Some("localhost,127.0.0.1"));
    }

    #[test]
    fn custom_ignores_no_proxy_and_requires_explicit_port() {
        let route = resolve(
            &CodexNetworkSettings {
                mode: CodexNetworkMode::Custom,
                proxy_url: "socks5h://localhost:1080/".into(),
                ..Default::default()
            },
            &EnvironmentSnapshot {
                no_proxy: Some("*".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(route.source, RouteSource::Custom);
        assert_eq!(route.no_proxy, None);
        assert!(resolve(
            &CodexNetworkSettings {
                mode: CodexNetworkMode::Custom,
                proxy_url: "http://localhost".into(),
                ..Default::default()
            },
            &EnvironmentSnapshot::default(),
        )
        .is_err());
    }

    #[test]
    fn rejects_credentials_paths_queries_and_unsupported_schemes() {
        for value in [
            "http://user:pass@localhost:8080/",
            "http://localhost:8080/path",
            "http://localhost:8080/?x=1",
            "ftp://localhost:2121/",
        ] {
            assert!(validate_proxy_url(value).is_err(), "{value}");
        }
    }

    #[test]
    fn encoded_route_is_bounded_and_fingerprint_protected() {
        let route = resolve(
            &CodexNetworkSettings {
                mode: CodexNetworkMode::Custom,
                proxy_url: "https://proxy.example:8443".into(),
                ..Default::default()
            },
            &EnvironmentSnapshot::default(),
        )
        .unwrap();
        let encoded = encode_route(&route).unwrap();
        assert_eq!(decode_route(&encoded).unwrap(), route);
        let tampered = encoded.replace("8443", "8444");
        assert_eq!(
            decode_route(&tampered).unwrap_err().kind,
            RouteErrorKind::EncodedRouteInvalid
        );
        let mut invalid = route;
        invalid.fingerprint = "00".repeat(32);
        assert_eq!(
            encode_route(&invalid).unwrap_err().kind,
            RouteErrorKind::EncodedRouteInvalid
        );
    }

    #[test]
    fn direct_route_has_stable_identity() {
        assert_eq!(direct_route(), direct_route());
        assert_eq!(direct_route().source, RouteSource::Direct);
    }
}
