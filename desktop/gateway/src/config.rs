#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub provider: String,
    pub port: u16,
    pub auth_secret: Option<String>,
    pub api_key: Option<String>,
    pub upstream_url: String,
    pub models_url: Option<String>,
    pub forced_model: Option<String>,
    pub relay_thinking: Option<String>,
    pub shim_mode: String,
    /// CSSwitch-owned auth state root. Present only for Codex; OAuth secrets
    /// themselves remain in CSSwitch private auth files.
    pub codex_state_root: Option<std::path::PathBuf>,
    pub(crate) codex_contract: Option<crate::provider_contracts::CodexRuntimeContract>,
    /// Opaque per-spawn identity supplied by the Tauri process manager.
    /// Standalone invocations may leave it empty, but managed launches always set it.
    pub launch_id: String,
    /// CSSwitch-managed Science data-dir used only by the authenticated local
    /// external-Skill install endpoint. Standalone gateways leave it unset.
    pub skill_data_dir: Option<std::path::PathBuf>,
    pub skill_bridge_dir: Option<std::path::PathBuf>,
    /// Per-proxy HMAC key for the user-confirmed Skill filesystem bridge.
    /// It is supplied only through the child environment and is never returned
    /// from Gateway health or inference responses.
    pub skill_bridge_token: Option<String>,
    /// Verified Science runtime identity used by the local Skill attach control
    /// plane. A gateway without this context still serves inference traffic but
    /// does not install Skills.
    pub science_host_context: Option<csswitch_skill_install_core::ScienceHostContext>,
}

pub const UPSTREAM_UA: &str = "CSSwitch/0.2 (+https://github.com/SuperJJ007/CSSwitch)";
pub const DEFAULT_UPSTREAM_URL: &str = "https://api.deepseek.com/anthropic/v1/messages";
pub const DEFAULT_QWEN_UPSTREAM_URL: &str =
    "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions";
pub const DEFAULT_CODEX_UPSTREAM_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

pub const DEEPSEEK_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-8", "DeepSeek V4 Pro"),
    ("claude-haiku-4-5", "DeepSeek V4 Flash"),
];

pub const QWEN_MODELS: &[(&str, &str)] = &[
    ("qwen3.7-max", "Qwen 3.7 Max"),
    ("qwen-plus-latest", "Qwen Plus Latest"),
    ("qwen-turbo", "Qwen Turbo"),
];

pub fn shim_mode(raw: Option<&str>) -> &'static str {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "detect" => "detect",
        "rewrite" => "rewrite",
        _ => "off",
    }
}

/// Canonical DSML mode shared by config parsing and runtime identity.
/// Only DeepSeek is DSML-capable; every other provider is fail-safe `off`,
/// even when the parent environment contains a stale or polluted value.
pub fn canonical_shim_mode(provider: &str, raw: Option<&str>) -> &'static str {
    if provider == "deepseek" {
        shim_mode(raw)
    } else {
        "off"
    }
}

pub fn provider_supported(provider: &str, shim: &str) -> bool {
    match provider {
        "deepseek" => matches!(shim, "off" | "detect" | "rewrite"),
        "qwen" | "openai-custom" | "openai-responses" | "relay" | "codex" => shim == "off",
        _ => false,
    }
}

pub fn normalize_openai_base(base: &str) -> String {
    let mut out = base.trim().trim_end_matches('/').to_string();
    for suffix in [
        "/v1/chat/completions",
        "/chat/completions",
        "/v1/responses",
        "/responses",
        "/v1/models",
        "/models",
    ] {
        if out.ends_with(suffix) {
            let keep = out.len() - suffix.len();
            out.truncate(keep);
            while out.ends_with('/') {
                out.pop();
            }
            break;
        }
    }
    out
}

fn ends_with_version_segment(base: &str) -> bool {
    let Some(last) = base.rsplit('/').next() else {
        return false;
    };
    let Some(version) = last.strip_prefix('v') else {
        return false;
    };
    !version.is_empty()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

pub fn openai_endpoint(base: &str, suffix: &str) -> String {
    let mut root = normalize_openai_base(base);
    if !ends_with_version_segment(&root) {
        root.push_str("/v1");
    }
    root.push_str(suffix);
    root
}

fn upstream_url_for(
    provider: &str,
    default_upstream: String,
    override_raw: Option<String>,
) -> String {
    if matches!(provider, "deepseek" | "qwen") {
        override_raw
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(default_upstream)
    } else {
        default_upstream
    }
}

impl GatewayConfig {
    pub fn from_env_args(args: Vec<String>) -> Result<Self, String> {
        let mut provider = "deepseek".to_string();
        let mut port: Option<u16> = None;
        let mut auth_token_arg: Option<String> = None;

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--provider" => {
                    i += 1;
                    provider = args.get(i).ok_or("--provider 缺少值")?.clone();
                }
                "--port" => {
                    i += 1;
                    let raw = args.get(i).ok_or("--port 缺少值")?;
                    port = Some(raw.parse().map_err(|_| format!("非法端口：{raw}"))?);
                }
                "--auth-token" => {
                    i += 1;
                    auth_token_arg = Some(args.get(i).ok_or("--auth-token 缺少值")?.clone());
                }
                other => return Err(format!("未知参数：{other}")),
            }
            i += 1;
        }

        let shim = canonical_shim_mode(
            &provider,
            std::env::var("CSSWITCH_TOOLUSE_SHIM").ok().as_deref(),
        );
        if !provider_supported(&provider, shim) {
            return Err(format!(
                "只支持 deepseek + shim off/detect/rewrite 或 qwen/openai-custom/openai-responses/relay/codex + shim off（provider={provider}, shim={shim}）"
            ));
        }

        let key_env = match provider.as_str() {
            "qwen" => "DASHSCOPE_API_KEY",
            "openai-custom" | "openai-responses" => "CSSWITCH_OPENAI_KEY",
            "relay" => "CSSWITCH_RELAY_KEY",
            _ => "DEEPSEEK_API_KEY",
        };
        let api_key = if provider == "codex" {
            None
        } else {
            Some(
                std::env::var(key_env)
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| format!("缺少 {key_env}"))?,
            )
        };
        let auth_secret = std::env::var("CSSWITCH_AUTH_TOKEN")
            .ok()
            .filter(|v| !v.is_empty())
            .or(auth_token_arg)
            .filter(|v| !v.is_empty());
        let mut models_url = None;
        let mut forced_model = None;
        let mut relay_thinking = None;
        let default_upstream = if provider == "openai-custom" || provider == "openai-responses" {
            let base = std::env::var("CSSWITCH_OPENAI_BASE_URL")
                .ok()
                .map(|v| normalize_openai_base(&v))
                .filter(|v| {
                    !v.is_empty() && (v.starts_with("http://") || v.starts_with("https://"))
                })
                .ok_or_else(|| format!("{provider} 需要 CSSWITCH_OPENAI_BASE_URL=http(s)://..."))?;
            models_url = Some(openai_endpoint(&base, "/models"));
            forced_model = std::env::var("CSSWITCH_OPENAI_MODEL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
            let suffix = if provider == "openai-responses" {
                "/responses"
            } else {
                "/chat/completions"
            };
            openai_endpoint(&base, suffix)
        } else if provider == "relay" {
            let base = std::env::var("CSSWITCH_RELAY_BASE_URL")
                .ok()
                .map(|v| v.trim().trim_end_matches('/').to_string())
                .filter(|v| {
                    !v.is_empty() && (v.starts_with("http://") || v.starts_with("https://"))
                })
                .ok_or("relay 需要 CSSWITCH_RELAY_BASE_URL=http(s)://...")?;
            models_url = Some(format!("{base}/v1/models"));
            forced_model = std::env::var("CSSWITCH_RELAY_MODEL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
            relay_thinking = std::env::var("CSSWITCH_RELAY_THINKING")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
            format!("{base}/v1/messages")
        } else if provider == "qwen" {
            DEFAULT_QWEN_UPSTREAM_URL.to_string()
        } else if provider == "codex" {
            DEFAULT_CODEX_UPSTREAM_URL.to_string()
        } else {
            DEFAULT_UPSTREAM_URL.to_string()
        };
        let upstream_url = upstream_url_for(
            &provider,
            default_upstream,
            std::env::var("CSSWITCH_UPSTREAM_URL").ok(),
        );
        let launch_id = std::env::var("CSSWITCH_LAUNCH_ID")
            .unwrap_or_default()
            .trim()
            .to_string();
        let codex_state_root = if provider == "codex" {
            Some(
                std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .ok_or("codex provider 需要绝对 HOME")?
                    .join(crate::codex_auth::CODEX_STATE_DIR_NAME),
            )
        } else {
            None
        };
        let codex_contract = if provider == "codex" {
            let contract = crate::provider_contracts::load_codex_runtime_contract()?;
            crate::provider_contracts::validate_managed_identity(
                &contract,
                std::env::var("CSSWITCH_PROVIDER_CONTRACT_ID")
                    .ok()
                    .as_deref(),
                std::env::var("CSSWITCH_PROVIDER_CONTRACT_DIGEST")
                    .ok()
                    .as_deref(),
            )?;
            Some(contract)
        } else {
            None
        };
        let skill_data_dir = std::env::var_os("CSSWITCH_SKILL_DATA_DIR")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute());
        let skill_bridge_dir = std::env::var_os("CSSWITCH_SKILL_BRIDGE_DIR")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute());
        let skill_bridge_token =
            std::env::var("CSSWITCH_SKILL_BRIDGE_TOKEN")
                .ok()
                .filter(|value| {
                    value.len() == 64
                        && value.chars().all(|character| character.is_ascii_hexdigit())
                });
        let science_host_context = std::env::var("CSSWITCH_SCIENCE_HOST_CONTEXT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                serde_json::from_str::<csswitch_skill_install_core::ScienceHostContext>(&value)
                    .map_err(|_| "CSSWITCH_SCIENCE_HOST_CONTEXT 不是合法的 Science host context")
            })
            .transpose()?;
        if science_host_context
            .as_ref()
            .zip(skill_data_dir.as_ref())
            .is_some_and(|(context, data_dir)| &context.data_dir != data_dir)
        {
            return Err("Science host context 与 Skill data-dir 不一致".into());
        }
        Ok(Self {
            provider,
            port: port.ok_or("--port 必填")?,
            auth_secret,
            api_key,
            upstream_url,
            models_url,
            forced_model,
            relay_thinking,
            shim_mode: shim.to_string(),
            codex_state_root,
            codex_contract,
            launch_id,
            skill_data_dir,
            skill_bridge_dir,
            skill_bridge_token,
            science_host_context,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_shim_mode, normalize_openai_base, openai_endpoint, provider_supported, shim_mode,
        upstream_url_for,
    };

    #[test]
    fn shim_mode_parses_deepseek_off_contract() {
        assert_eq!(shim_mode(None), "off");
        assert_eq!(shim_mode(Some("Detect")), "detect");
        assert_eq!(shim_mode(Some(" Rewrite ")), "rewrite");
        assert_eq!(shim_mode(Some("bad")), "off");
    }

    #[test]
    fn canonical_shim_mode_is_deepseek_only_and_fail_safe() {
        for (raw, expected) in [
            (None, "off"),
            (Some(" DETECT "), "detect"),
            (Some(" Rewrite "), "rewrite"),
            (Some("unknown"), "off"),
        ] {
            assert_eq!(canonical_shim_mode("deepseek", raw), expected);
        }
        for provider in [
            "qwen",
            "openai-custom",
            "openai-responses",
            "relay",
            "codex",
            "unknown",
        ] {
            assert_eq!(canonical_shim_mode(provider, Some(" Rewrite ")), "off");
            assert_eq!(canonical_shim_mode(provider, Some("DETECT")), "off");
        }
    }

    #[test]
    fn provider_support_matrix_accepts_only_canonical_shims() {
        assert!(provider_supported("deepseek", "off"));
        assert!(provider_supported("deepseek", "detect"));
        assert!(provider_supported("deepseek", "rewrite"));
        assert!(provider_supported("qwen", "off"));
        assert!(!provider_supported("qwen", "detect"));
        assert!(provider_supported("openai-custom", "off"));
        assert!(provider_supported("openai-responses", "off"));
        assert!(provider_supported("relay", "off"));
        assert!(!provider_supported("relay", "rewrite"));
        assert!(provider_supported("codex", "off"));
        assert!(!provider_supported("codex", "rewrite"));
    }

    #[test]
    fn openai_base_normalization_matches_python_proxy_contract() {
        let root = "https://open.bigmodel.cn/api/paas/v4";
        assert_eq!(
            normalize_openai_base(&format!("{root}/chat/completions")),
            root
        );
        assert_eq!(normalize_openai_base(&format!("{root}/responses")), root);
        assert_eq!(normalize_openai_base(&format!("{root}/models")), root);
        assert_eq!(openai_endpoint(root, "/models"), format!("{root}/models"));
        assert_eq!(
            openai_endpoint("https://api.siliconflow.cn", "/chat/completions"),
            "https://api.siliconflow.cn/v1/chat/completions"
        );
    }

    #[test]
    fn upstream_override_is_native_only() {
        let poison = Some("http://127.0.0.1:1/poison".to_string());
        assert_eq!(
            upstream_url_for(
                "deepseek",
                "https://default/deepseek".to_string(),
                poison.clone()
            ),
            "http://127.0.0.1:1/poison"
        );
        assert_eq!(
            upstream_url_for("qwen", "https://default/qwen".to_string(), poison.clone()),
            "http://127.0.0.1:1/poison"
        );
        assert_eq!(
            upstream_url_for(
                "openai-custom",
                "http://candidate/v1/chat/completions".to_string(),
                poison.clone()
            ),
            "http://candidate/v1/chat/completions"
        );
        assert_eq!(
            upstream_url_for(
                "openai-responses",
                "http://candidate/v1/responses".to_string(),
                poison.clone()
            ),
            "http://candidate/v1/responses"
        );
        assert_eq!(
            upstream_url_for("relay", "http://candidate/v1/messages".to_string(), poison),
            "http://candidate/v1/messages"
        );
        assert_eq!(
            upstream_url_for(
                "codex",
                super::DEFAULT_CODEX_UPSTREAM_URL.to_string(),
                Some("http://127.0.0.1:1/poison".to_string())
            ),
            super::DEFAULT_CODEX_UPSTREAM_URL
        );
    }
}
