//! 中转站预设表：Rust 后端单一来源（spec §4.1）。前端 get_relay_presets 取一次铺 UI，
//! 不在前端复制常量。base_url 只读（自定义走 relay-custom）；requires_model_override 决定
//! 是否允许「留空透传」（不认 claude-* 的家 = true，必须选模型）。

/// 一个中转站预设。所有字段静态常量（非密钥），可整表回显给前端。
#[derive(Clone)]
pub struct RelayPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub base_url_editable: bool,
    pub requires_model_override: bool,
    pub builtin_models: &'static [&'static str],
}

pub fn all() -> &'static [RelayPreset] {
    PRESETS
}

/// 按 id 查预设。
pub fn by_id(id: &str) -> Option<&'static RelayPreset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// 按 base_url 匹配内置预设（归一化去尾 `/` 精确比对），返回预设 id。
/// 空 base_url（relay-custom）不参与匹配 → None。供遗留 provider=relay 迁移用。
pub fn match_base_url(url: &str) -> Option<&'static str> {
    let norm = url.trim().trim_end_matches('/');
    if norm.is_empty() {
        return None;
    }
    PRESETS
        .iter()
        .find(|p| !p.base_url.is_empty() && p.base_url.trim_end_matches('/') == norm)
        .map(|p| p.id)
}

/// 遗留 provider=relay 迁移（防御项，幂等，修评审 P1-2 防御化）。
/// PR #4 用单槽 `providers["relay"]` + `provider="relay"`；多槽落地后把它迁到 `relay-<preset>`：
///   - 有旧 `relay` 槽 → 按 base_url match_base_url（不中→relay-custom）搬 key/base_url/model，删旧槽。
///   - provider=="relay" → 改成迁移后的目标 id；无槽可搬则落 deepseek（不留非法 provider）。
/// 改动了返回 true。已是多槽 / 非 relay 配置 → 不动，返回 false。
pub fn migrate_legacy_relay(cfg: &mut crate::config::Config) -> bool {
    let mut changed = false;
    let target = if let Some(slot) = cfg.providers.remove("relay") {
        let id = match_base_url(&slot.base_url).unwrap_or("relay-custom");
        cfg.providers.insert(id.to_string(), slot);
        changed = true;
        Some(id.to_string())
    } else {
        None
    };
    if cfg.provider == "relay" {
        cfg.provider = target.unwrap_or_else(|| "deepseek".to_string());
        changed = true;
    }
    changed
}

static PRESETS: &[RelayPreset] = &[
    RelayPreset {
        id: "relay-xiaomi",
        name: "小米 MiMo",
        base_url: "https://api.xiaomimimo.com/anthropic",
        base_url_editable: false,
        requires_model_override: true, // 无 /v1/models + 不认 claude-*，必须选模型
        builtin_models: &["mimo-v2.5-pro"],
    },
    RelayPreset {
        id: "relay-glm",
        name: "智谱 GLM",
        base_url: "https://open.bigmodel.cn/api/anthropic",
        base_url_editable: false,
        requires_model_override: false, // 有 /v1/models + 认 claude-*（含裸名），可透传
        builtin_models: &["glm-4.6", "glm-5", "glm-4.5-air"],
    },
    RelayPreset {
        id: "relay-siliconflow",
        name: "硅基流动",
        base_url: "https://api.siliconflow.cn",
        base_url_editable: false,
        requires_model_override: true, // 有 models 但不认 claude-*，必须选模型
        builtin_models: &["deepseek-ai/DeepSeek-V3", "zai-org/GLM-5.2"],
    },
    RelayPreset {
        id: "relay-openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api",
        base_url_editable: false,
        requires_model_override: false, // 有 /v1/models + 认裸/前缀 claude-*，可透传
        builtin_models: &[
            "anthropic/claude-sonnet-5",
            "anthropic/claude-opus-4.8-fast",
        ],
    },
    RelayPreset {
        id: "relay-custom",
        name: "自定义",
        base_url: "",
        base_url_editable: true,
        requires_model_override: true, // 未实测兼容前默认要求选模型（高级用户前端可显式解锁）
        builtin_models: &[],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProviderCfg};

    fn cfg_with_relay(base_url: &str) -> Config {
        let mut c = Config {
            provider: "relay".into(),
            ..Default::default()
        };
        c.providers.insert(
            "relay".into(),
            ProviderCfg {
                key: "legacy_key".into(),
                base_url: base_url.into(),
                model: String::new(),
            },
        );
        c
    }

    #[test]
    fn migrate_known_base_url_moves_to_matched_preset() {
        let mut c = cfg_with_relay("https://open.bigmodel.cn/api/anthropic");
        let changed = migrate_legacy_relay(&mut c);
        assert!(changed);
        assert_eq!(c.provider, "relay-glm");
        assert!(!c.providers.contains_key("relay"), "旧 relay 槽应删除");
        assert_eq!(c.key_for("relay-glm").as_deref(), Some("legacy_key"));
        assert_eq!(
            c.base_url_for("relay-glm").as_deref(),
            Some("https://open.bigmodel.cn/api/anthropic")
        );
    }

    #[test]
    fn migrate_unknown_base_url_falls_to_custom() {
        let mut c = cfg_with_relay("https://unknown.example/relay");
        assert!(migrate_legacy_relay(&mut c));
        assert_eq!(c.provider, "relay-custom");
        assert_eq!(c.key_for("relay-custom").as_deref(), Some("legacy_key"));
    }

    #[test]
    fn migrate_is_idempotent_and_noop_on_new_config() {
        // 已是多槽（relay-glm）→ 不动，返回 false。
        let mut c = Config {
            provider: "relay-glm".into(),
            ..Default::default()
        };
        c.providers.insert(
            "relay-glm".into(),
            ProviderCfg {
                key: "k".into(),
                base_url: "https://open.bigmodel.cn/api/anthropic".into(),
                model: "glm-4.6".into(),
            },
        );
        assert!(!migrate_legacy_relay(&mut c));
        assert_eq!(c.provider, "relay-glm");
        // 非 relay 配置也不动。
        let mut d = Config::default();
        assert!(!migrate_legacy_relay(&mut d));
        assert_eq!(d.provider, "deepseek");
    }

    #[test]
    fn migrate_provider_relay_without_slot_falls_to_deepseek() {
        // provider=relay 但无 relay 槽（异常残留）→ 落 deepseek，别留非法 provider。
        let mut c = Config {
            provider: "relay".into(),
            ..Default::default()
        };
        assert!(migrate_legacy_relay(&mut c));
        assert_eq!(c.provider, "deepseek");
    }

    #[test]
    fn table_has_five_presets_with_expected_ids() {
        let ids: Vec<&str> = all().iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            vec![
                "relay-xiaomi",
                "relay-glm",
                "relay-siliconflow",
                "relay-openrouter",
                "relay-custom"
            ]
        );
    }

    #[test]
    fn requires_model_override_matches_spec() {
        // 不认 claude-* / 无 models 的家必须选模型；GLM/OpenRouter 可透传；custom 默认 true。
        assert!(by_id("relay-xiaomi").unwrap().requires_model_override);
        assert!(by_id("relay-siliconflow").unwrap().requires_model_override);
        assert!(!by_id("relay-glm").unwrap().requires_model_override);
        assert!(!by_id("relay-openrouter").unwrap().requires_model_override);
        assert!(by_id("relay-custom").unwrap().requires_model_override);
    }

    #[test]
    fn custom_has_empty_editable_base_url() {
        let c = by_id("relay-custom").unwrap();
        assert_eq!(c.base_url, "");
        assert!(c.base_url_editable);
    }

    #[test]
    fn match_base_url_finds_preset_and_ignores_trailing_slash() {
        assert_eq!(
            match_base_url("https://open.bigmodel.cn/api/anthropic/"),
            Some("relay-glm")
        );
        assert_eq!(
            match_base_url("https://api.xiaomimimo.com/anthropic"),
            Some("relay-xiaomi")
        );
        assert_eq!(match_base_url("https://unknown.example/relay"), None);
        assert_eq!(match_base_url(""), None);
    }

    #[test]
    fn builtin_models_present_for_known_presets() {
        assert!(by_id("relay-xiaomi")
            .unwrap()
            .builtin_models
            .contains(&"mimo-v2.5-pro"));
        assert!(by_id("relay-custom").unwrap().builtin_models.is_empty());
    }
}
