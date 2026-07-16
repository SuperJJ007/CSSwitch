//! UI 模板注册表：只保存名称、默认地址、图标和内置模型等展示元数据。
//! adapter、鉴权、模型策略、transport、thinking 等启动 capability 的唯一来源是
//! `catalog/provider-contracts.v1.json`；前端 list_templates 由两者合并后铺 UI。

#[derive(Clone)]
pub struct Template {
    pub id: &'static str,
    pub name: &'static str,
    pub category: &'static str, // official | cn_official | custom | experimental
    pub api_format: &'static str, // anthropic | openai_chat | openai_responses | gemini_native
    pub base_url: &'static str, // 默认；空=用户填
    pub base_url_editable: bool,
    pub builtin_models: &'static [&'static str],
    pub website_url: &'static str,
    pub icon: &'static str,
    pub icon_color: &'static str,
}

pub fn all() -> &'static [Template] {
    TEMPLATES
}

pub fn by_id(id: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.id == id)
}

/// 旧固定槽 id → 新 template_id（迁移用）。未知/遗留裸 relay → custom。
pub fn template_id_for_legacy_slot(slot: &str) -> &'static str {
    match slot {
        "deepseek" => "deepseek",
        "qwen" => "qwen",
        "relay-glm" => "glm",
        "relay-xiaomi" => "xiaomi",
        "relay-siliconflow" => "siliconflow",
        "relay-openrouter" => "openrouter",
        _ => "custom",
    }
}

static TEMPLATES: &[Template] = &[
    Template {
        id: "deepseek",
        name: "DeepSeek",
        category: "cn_official",
        api_format: "anthropic",
        base_url: "https://api.deepseek.com/anthropic",
        base_url_editable: false,
        builtin_models: &["claude-opus-4-8", "claude-haiku-4-5"],
        website_url: "https://platform.deepseek.com",
        icon: "deepseek",
        icon_color: "#1E88E5",
    },
    Template {
        id: "glm",
        name: "智谱 GLM",
        category: "cn_official",
        api_format: "anthropic",
        base_url: "https://open.bigmodel.cn/api/anthropic",
        base_url_editable: true,
        builtin_models: &["glm-5.2", "glm-4.7", "glm-4.6", "glm-4.5-air"], // 官方核定 2026-07-04：旗舰 glm-5.2
        website_url: "https://open.bigmodel.cn",
        icon: "glm",
        icon_color: "#2E6BE6",
    },
    Template {
        id: "xiaomi",
        name: "小米 MiMo",
        category: "cn_official",
        api_format: "anthropic",
        base_url: "https://api.xiaomimimo.com/anthropic",
        base_url_editable: true,
        builtin_models: &["mimo-v2.5-pro"],
        website_url: "https://xiaomimimo.com",
        icon: "xiaomi",
        icon_color: "#FF6900",
    },
    Template {
        id: "siliconflow",
        name: "硅基流动",
        category: "cn_official",
        api_format: "anthropic",
        base_url: "https://api.siliconflow.cn",
        base_url_editable: true,
        builtin_models: &[
            "deepseek-ai/DeepSeek-V4-Pro",
            "deepseek-ai/DeepSeek-V4-Flash",
            "deepseek-ai/DeepSeek-V3.2",
            "zai-org/GLM-5.2",
        ], // 官方核定 2026-07-04；真机证实 api.siliconflow.cn/v1/messages 返回 Anthropic 200（relay/anthropic 配置正确，无需翻译）
        website_url: "https://siliconflow.cn",
        icon: "siliconflow",
        icon_color: "#7C3AED",
    },
    Template {
        id: "kimi",
        name: "Kimi（Moonshot）",
        category: "cn_official",
        api_format: "anthropic",
        base_url: "https://api.moonshot.cn/anthropic", // 国际站可改 api.moonshot.ai/anthropic
        base_url_editable: true,
        builtin_models: &["kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6"], // 官方核定 2026-07-04
        website_url: "https://platform.moonshot.cn",
        icon: "kimi",
        icon_color: "#16182F",
    },
    Template {
        id: "minimax",
        name: "MiniMax",
        category: "cn_official",
        api_format: "anthropic",
        base_url: "https://api.minimaxi.com/anthropic", // 国内站（真机验证：key 有效 + /v1/models 实时发现 200）；国际站改 api.minimax.io
        base_url_editable: true,
        builtin_models: &["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed"], // 官方核定 2026-07-04：旗舰 M3（2026-06-01 GA）
        website_url: "https://platform.minimaxi.com",
        icon: "minimax",
        icon_color: "#E1341E",
    },
    Template {
        id: "openrouter",
        name: "OpenRouter",
        category: "custom",
        api_format: "anthropic",
        base_url: "https://openrouter.ai/api",
        base_url_editable: true,
        builtin_models: &[
            "anthropic/claude-sonnet-5",
            "anthropic/claude-opus-4.8",
            "anthropic/claude-opus-4.8-fast",
        ], // 官方核定 2026-07-04：补非 2x 价的 opus-4.8
        website_url: "https://openrouter.ai",
        icon: "openrouter",
        icon_color: "#6467F2",
    },
    Template {
        id: "qwen",
        name: "通义千问",
        category: "cn_official",
        api_format: "openai_chat",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        base_url_editable: false,
        builtin_models: &["qwen3.7-max", "qwen-plus-latest", "qwen-turbo"],
        website_url: "https://dashscope.aliyun.com",
        icon: "qwen",
        icon_color: "#615CED",
    },
    Template {
        id: "codex",
        name: "Codex（实验）",
        category: "experimental",
        api_format: "openai_responses",
        base_url: "",
        base_url_editable: false,
        builtin_models: &[],
        website_url: "https://developers.openai.com/codex/",
        icon: "custom",
        icon_color: "#111827",
    },
    Template {
        id: "custom-openai",
        name: "自定义 OpenAI",
        category: "custom",
        api_format: "openai_chat",
        base_url: "",
        base_url_editable: true,
        builtin_models: &[],
        website_url: "",
        icon: "custom",
        icon_color: "#2563EB",
    },
    Template {
        id: "custom-openai-responses",
        name: "自定义 OpenAI Responses",
        category: "custom",
        api_format: "openai_responses",
        base_url: "",
        base_url_editable: true,
        builtin_models: &[],
        website_url: "",
        icon: "custom",
        icon_color: "#0F766E",
    },
    Template {
        id: "custom",
        name: "自定义 Anthropic",
        category: "custom",
        api_format: "anthropic",
        base_url: "",
        base_url_editable: true,
        builtin_models: &[],
        website_url: "",
        icon: "custom",
        icon_color: "#6B7280",
    },
];

/// 遗留 provider=relay 单槽迁移（幂等）：在「旧 slot map + 旧 provider 指针」上把
/// 裸 `relay` 槽按 base_url 归位到 `relay-<preset>`。A4 迁移前先跑。返回是否改动。
pub fn migrate_legacy_relay(
    providers: &mut std::collections::BTreeMap<String, crate::config_legacy::ProviderCfgV1>,
    provider: &mut String,
) -> bool {
    let mut changed = false;
    let target = if let Some(slot) = providers.remove("relay") {
        let id = match_base_url(&slot.base_url).unwrap_or("relay-custom");
        providers.insert(id.to_string(), slot);
        changed = true;
        Some(id.to_string())
    } else {
        None
    };
    if provider == "relay" {
        *provider = target.unwrap_or_else(|| "deepseek".to_string());
        changed = true;
    }
    changed
}

/// 旧「relay-<preset>」槽 id ↔ base_url（迁移遗留裸 relay 用）。空 base_url → None。
fn match_base_url(url: &str) -> Option<&'static str> {
    let norm = url.trim().trim_end_matches('/');
    if norm.is_empty() {
        return None;
    }
    [
        ("relay-glm", "https://open.bigmodel.cn/api/anthropic"),
        ("relay-xiaomi", "https://api.xiaomimimo.com/anthropic"),
        ("relay-siliconflow", "https://api.siliconflow.cn"),
        ("relay-openrouter", "https://openrouter.ai/api"),
    ]
    .iter()
    .find(|(_, b)| *b == norm)
    .map(|(id, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_legacy::ProviderCfgV1;
    use std::collections::BTreeMap;

    #[test]
    fn table_has_twelve_templates_including_experimental_codex() {
        let ids: Vec<&str> = all().iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            vec![
                "deepseek",
                "glm",
                "xiaomi",
                "siliconflow",
                "kimi",
                "minimax",
                "openrouter",
                "qwen",
                "codex",
                "custom-openai",
                "custom-openai-responses",
                "custom"
            ]
        );
    }

    #[test]
    fn provider_contract_owns_adapter_mapping() {
        assert_eq!(
            crate::provider_contracts::contract_for("deepseek", "anthropic")
                .unwrap()
                .adapter,
            "deepseek"
        );
        assert_eq!(
            crate::provider_contracts::contract_for("custom-openai-responses", "openai_responses")
                .unwrap()
                .adapter,
            "openai-responses"
        );
        assert!(crate::provider_contracts::contract_for("unknown-xyz", "anthropic").is_err());
    }

    #[test]
    fn api_format_reflects_real_protocol() {
        assert_eq!(by_id("deepseek").unwrap().api_format, "anthropic");
        assert_eq!(by_id("glm").unwrap().api_format, "anthropic");
        assert_eq!(by_id("kimi").unwrap().api_format, "anthropic");
        assert_eq!(by_id("minimax").unwrap().api_format, "anthropic");
        assert_eq!(by_id("qwen").unwrap().api_format, "openai_chat");
        assert_eq!(by_id("codex").unwrap().api_format, "openai_responses");
        assert_eq!(by_id("custom-openai").unwrap().api_format, "openai_chat");
        assert_eq!(
            by_id("custom-openai-responses").unwrap().api_format,
            "openai_responses"
        );
        assert_eq!(by_id("custom").unwrap().api_format, "anthropic");
    }

    #[test]
    fn requires_model_override_matches_capability() {
        for (id, api_format) in [
            ("xiaomi", "anthropic"),
            ("siliconflow", "anthropic"),
            ("kimi", "anthropic"),
            ("minimax", "anthropic"),
            ("glm", "anthropic"),
            ("custom-openai", "openai_chat"),
            ("custom-openai-responses", "openai_responses"),
            ("openrouter", "anthropic"),
            ("custom", "anthropic"),
        ] {
            assert_eq!(
                crate::provider_contracts::contract_for(id, api_format)
                    .unwrap()
                    .default_model_policy,
                crate::provider_contracts::ModelPolicy::RequiredFixed
            );
        }
        // 旗舰默认 = builtin_models 首项（官方核定，2026-07-04）
        assert_eq!(by_id("glm").unwrap().builtin_models[0], "glm-5.2");
        assert_eq!(by_id("minimax").unwrap().builtin_models[0], "MiniMax-M3");
        assert_eq!(
            by_id("openrouter").unwrap().builtin_models[0],
            "anthropic/claude-sonnet-5"
        );
    }

    #[test]
    fn thinking_policy_per_provider() {
        assert_eq!(
            crate::provider_contracts::contract_for("kimi", "anthropic")
                .unwrap()
                .thinking_policy,
            "enabled"
        );
        assert_eq!(
            crate::provider_contracts::contract_for("glm", "anthropic")
                .unwrap()
                .thinking_policy,
            "adaptive"
        );
        assert_eq!(
            crate::provider_contracts::contract_for("deepseek", "anthropic")
                .unwrap()
                .thinking_policy,
            ""
        );
    }

    #[test]
    fn legacy_slot_maps_to_template_id() {
        assert_eq!(template_id_for_legacy_slot("deepseek"), "deepseek");
        assert_eq!(template_id_for_legacy_slot("qwen"), "qwen");
        assert_eq!(template_id_for_legacy_slot("relay-glm"), "glm");
        assert_eq!(template_id_for_legacy_slot("relay-xiaomi"), "xiaomi");
        assert_eq!(
            template_id_for_legacy_slot("relay-siliconflow"),
            "siliconflow"
        );
        assert_eq!(
            template_id_for_legacy_slot("relay-openrouter"),
            "openrouter"
        );
        assert_eq!(template_id_for_legacy_slot("relay-custom"), "custom");
        assert_eq!(template_id_for_legacy_slot("relay"), "custom"); // 遗留裸 relay 兜底
        assert_eq!(template_id_for_legacy_slot("weird"), "custom");
    }

    #[test]
    fn custom_has_empty_editable_base_url() {
        let c = by_id("custom").unwrap();
        assert_eq!(c.base_url, "");
        assert!(c.base_url_editable);
    }

    #[test]
    fn base_url_editable_matrix() {
        // relay 家族预设：地址可编辑（预填官方默认，允许改到 token 套餐 / 区域端点）。
        // 源自用户反馈：小米 MiMo token plan 走 token-plan-cn.xiaomimimo.com/anthropic，
        // 与内置 api.xiaomimimo.com 不同 host，锁死地址 → 上游 401。
        for id in [
            "glm",
            "xiaomi",
            "siliconflow",
            "kimi",
            "minimax",
            "openrouter",
            "custom-openai",
            "custom-openai-responses",
            "custom",
        ] {
            assert!(
                by_id(id).unwrap().base_url_editable,
                "{id} 的 base_url 应可编辑"
            );
        }
        // native adapter（deepseek/qwen）上游 URL 在 gateway 中固定，运行时不吃自定义
        // base_url，故保持只读，避免「能填但不生效」的假象。
        for id in ["deepseek", "qwen"] {
            assert!(
                !by_id(id).unwrap().base_url_editable,
                "{id} 是原生 adapter，base_url 应只读"
            );
        }
    }

    fn slot(base_url: &str) -> ProviderCfgV1 {
        ProviderCfgV1 {
            key: "legacy_key".into(),
            base_url: base_url.into(),
            model: String::new(),
        }
    }

    #[test]
    fn migrate_known_base_url_moves_to_matched_preset() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "relay".to_string(),
            slot("https://open.bigmodel.cn/api/anthropic"),
        );
        let mut provider = "relay".to_string();
        assert!(migrate_legacy_relay(&mut providers, &mut provider));
        assert_eq!(provider, "relay-glm");
        assert!(!providers.contains_key("relay"), "旧 relay 槽应删除");
        assert_eq!(providers.get("relay-glm").unwrap().key, "legacy_key");
    }

    #[test]
    fn migrate_unknown_base_url_falls_to_custom() {
        let mut providers = BTreeMap::new();
        providers.insert("relay".to_string(), slot("https://unknown.example/relay"));
        let mut provider = "relay".to_string();
        assert!(migrate_legacy_relay(&mut providers, &mut provider));
        assert_eq!(provider, "relay-custom");
        assert_eq!(providers.get("relay-custom").unwrap().key, "legacy_key");
    }

    #[test]
    fn migrate_provider_relay_without_slot_falls_to_deepseek() {
        let mut providers: BTreeMap<String, ProviderCfgV1> = BTreeMap::new();
        let mut provider = "relay".to_string();
        assert!(migrate_legacy_relay(&mut providers, &mut provider));
        assert_eq!(provider, "deepseek");
    }

    #[test]
    fn migrate_is_noop_on_new_config() {
        let mut providers: BTreeMap<String, ProviderCfgV1> = BTreeMap::new();
        providers.insert(
            "relay-glm".to_string(),
            slot("https://open.bigmodel.cn/api/anthropic"),
        );
        let mut provider = "relay-glm".to_string();
        assert!(!migrate_legacy_relay(&mut providers, &mut provider));
        assert_eq!(provider, "relay-glm");
    }
}
