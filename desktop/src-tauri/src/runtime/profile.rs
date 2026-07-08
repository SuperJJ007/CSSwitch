use std::path::Path;

use serde_json::json;

use crate::runtime::provider::{
    adapter_for_profile, assert_format_supported, is_native_adapter, is_openai_adapter,
    reject_openai_custom_anthropic_base, relay_missing_model,
};
use crate::{config, scratch, templates};

/// 判断模型 id 是否会平铺进 Science 选择器主列表（claude-{opus|sonnet|haiku}-<数字…>）。
/// 仅用于「获取模型」结果排序（主列表项排前），非鉴权路径。
pub(crate) fn is_main_list_model(id: &str) -> bool {
    for fam in ["claude-opus-", "claude-sonnet-", "claude-haiku-"] {
        if let Some(rest) = id.strip_prefix(fam) {
            return rest
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false);
        }
    }
    false
}

fn build_capabilities(
    adapter: &str,
    api_format: &str,
    model_required: bool,
    thinking_policy: &str,
) -> serde_json::Value {
    let model_discovery = if is_native_adapter(adapter) {
        "builtin_static"
    } else if is_openai_adapter(adapter) || matches!(api_format, "openai_chat" | "openai_responses")
    {
        "openai_models_or_manual"
    } else {
        "anthropic_models_or_manual"
    };
    let tools_hint = match api_format {
        "openai_chat" | "openai_responses" => "translated",
        "anthropic" if is_native_adapter(adapter) => "native",
        "anthropic" => "passthrough",
        _ => "unknown",
    };
    json!({
        "base_url_required": !is_native_adapter(adapter),
        "model_required": model_required,
        "model_discovery": model_discovery,
        "supports_thinking_policy": !thinking_policy.is_empty(),
        "thinking_policy": thinking_policy,
        "supports_tools_hint": tools_hint,
    })
}

pub(crate) fn template_capabilities(t: &templates::Template) -> serde_json::Value {
    build_capabilities(
        t.adapter,
        t.api_format,
        t.requires_model_override,
        t.thinking_policy,
    )
}

pub(crate) fn profile_capabilities(p: &config::Profile) -> serde_json::Value {
    match templates::by_id(&p.template_id) {
        Some(t) => {
            let api_format = if p.api_format.trim().is_empty() {
                t.api_format
            } else {
                &p.api_format
            };
            build_capabilities(
                t.adapter,
                api_format,
                t.requires_model_override,
                t.thinking_policy,
            )
        }
        None => {
            let api_format = if p.api_format.trim().is_empty() {
                "anthropic"
            } else {
                &p.api_format
            };
            build_capabilities("relay", api_format, true, "")
        }
    }
}

/// 组装 get_config 返回体：profiles 的 key 只回掩码，全 key 绝不出后端。
pub(crate) fn build_get_config(dir: &Path) -> Result<serde_json::Value, String> {
    let cfg = config::load_from(dir).map_err(|e| e.to_string())?;
    // 一次性迁移提示（#9 甲）：读出后立即清盘，避免每次 get_config 重复提示。
    let notice = cfg.pending_notice.clone();
    if notice.is_some() {
        config::update(dir, |c| c.pending_notice = None).map_err(|e| e.to_string())?;
    }
    let profiles: Vec<serde_json::Value> = cfg
        .profiles
        .iter()
        .map(|p| {
            let key_masked = config::mask(&p.api_key);
            json!({
                "id": p.id, "name": p.name, "template_id": p.template_id, "category": p.category,
                "api_format": p.api_format, "base_url": p.base_url, "model": p.model,
                "key": key_masked.clone(), "has_key": !p.api_key.is_empty(), "key_masked": key_masked,
                "capabilities": profile_capabilities(p), "icon": p.icon, "icon_color": p.icon_color,
                "website_url": p.website_url, "sort_index": p.sort_index, "notes": p.notes,
            })
        })
        .collect();
    Ok(json!({
        "schema_version": cfg.schema_version, "active_id": cfg.active_id, "profiles": profiles,
        "templates": build_list_templates(), "proxy_port": cfg.proxy_port,
        "sandbox_port": cfg.sandbox_port, "mode": cfg.mode, "pending_notice": notice,
    }))
}

/// 模板注册表交前端铺 UI（单一来源，前端不复制常量）。
pub(crate) fn build_list_templates() -> Vec<serde_json::Value> {
    templates::all()
        .iter()
        .map(|t| {
            json!({
                "id": t.id, "name": t.name, "category": t.category, "api_format": t.api_format,
                "adapter": t.adapter, "base_url": t.base_url, "base_url_editable": t.base_url_editable,
                "requires_model_override": t.requires_model_override,
                "builtin_models": t.builtin_models, "icon": t.icon, "icon_color": t.icon_color,
                "website_url": t.website_url, "capabilities": template_capabilities(t),
            })
        })
        .collect()
}

pub(crate) fn create_profile_inner(
    dir: &Path,
    template_id: &str,
    name: &str,
    key: Option<&str>,
    base_url_override: Option<&str>,
    model: Option<&str>,
) -> Result<String, String> {
    let tpl = templates::by_id(template_id).ok_or_else(|| format!("未知模板：{template_id}"))?;
    let id = config::new_id();
    let base_url = base_url_override
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| tpl.base_url.to_string());
    let p = config::Profile {
        id: id.clone(),
        name: name.to_string(),
        template_id: template_id.to_string(),
        category: tpl.category.to_string(),
        api_format: tpl.api_format.to_string(),
        base_url,
        api_key: key.unwrap_or("").to_string(),
        model: model.unwrap_or("").to_string(),
        website_url: Some(tpl.website_url.to_string()),
        icon: Some(tpl.icon.to_string()),
        icon_color: Some(tpl.icon_color.to_string()),
        sort_index: Some(config::now_ms()),
        created_at: Some(config::now_ms()),
        notes: None,
    };
    assert_format_supported(&p)?; // custom 选了不支持格式则拒
    let adapter = adapter_for_profile(&p);
    reject_openai_custom_anthropic_base(adapter, &p.base_url)?;
    // 守卫（修 #9 P1-a）：relay/自定义端点必须带 model（force 前提）。
    if relay_missing_model(adapter, &p.model) {
        return Err("中转 / 自定义端点必须选择或填写一个模型，未创建。".to_string());
    }
    config::update(dir, |c| c.profiles.push(p)).map_err(|e| e.to_string())?;
    Ok(id)
}

pub(crate) fn update_profile_metadata_inner(
    dir: &Path,
    id: &str,
    name: &str,
    notes: Option<&str>,
) -> Result<(), String> {
    // 未命中 id → Err（不静默 Ok，修 MP-1 Minor [4]）。
    if config::load_from(dir)
        .map_err(|e| e.to_string())?
        .profile_by_id(id)
        .is_none()
    {
        return Err(format!("找不到 profile：{id}"));
    }
    config::update(dir, |c| {
        if let Some(p) = c.profile_by_id_mut(id) {
            p.name = name.to_string();
            p.notes = notes.map(str::to_string);
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn clear_profile_key_inner(dir: &Path, id: &str) -> Result<(), String> {
    config::update(dir, |c| {
        if let Some(p) = c.profile_by_id_mut(id) {
            p.api_key.clear();
        }
    })
    .map_err(|e| e.to_string())?;
    config::drop_rolling_backup(dir); // 清 key 后净化滚动备份，旧明文不可从 .bak 恢复
    Ok(())
}

pub(crate) fn delete_profile_inner(dir: &Path, id: &str) -> Result<(), String> {
    config::update(dir, |c| {
        c.profiles.retain(|p| p.id != id);
        if c.active_id == id {
            c.active_id.clear(); // 删 active → 置空
        }
    })
    .map_err(|e| e.to_string())?;
    config::drop_rolling_backup(dir);
    Ok(())
}

pub(crate) fn update_profile_connection_inner(
    dir: &Path,
    id: &str,
    base_url: Option<&str>,
    api_format: Option<&str>,
    model: Option<&str>,
    key: Option<&str>,
) -> Result<(), String> {
    if let Some(fmt) = api_format {
        let probe = config::Profile {
            api_format: fmt.to_string(),
            ..Default::default()
        };
        assert_format_supported(&probe)?;
    }
    // 未命中 id → Err（不静默 Ok，修 MP-1 Minor [4]）。
    if config::load_from(dir)
        .map_err(|e| e.to_string())?
        .profile_by_id(id)
        .is_none()
    {
        return Err(format!("找不到 profile：{id}"));
    }
    config::write_rolling_backup(dir).ok(); // 覆盖前留底
    config::update(dir, |c| {
        if let Some(p) = c.profile_by_id_mut(id) {
            if let Some(u) = base_url {
                p.base_url = u.to_string();
            }
            if let Some(f) = api_format {
                p.api_format = f.to_string();
            }
            if let Some(m) = model {
                p.model = m.to_string();
            }
            if let Some(k) = key {
                if !k.is_empty() {
                    p.api_key = k.to_string(); // 空=不改（留占位不覆盖已存 key）
                }
            }
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 非 active 连接编辑的上游校验裁决（纯函数，P2-d）：
/// - `Ok(true)`  上游明确接受(200)，已校验；
/// - `Ok(false)` 无法确认(429/5xx/无响应)，best-effort 落盘、标记「未校验」（激活时会再验）；
/// - `Err(hint)` 上游明确拒绝(401/403/400/404/422)，拦下不落盘。
///
/// 选「如实标记后保存」：不因网络抖动/上游繁忙挡住保存，但也绝不假称已校验。
pub(crate) fn nonactive_probe_verdict(outcome: &scratch::ProbeOutcome) -> Result<bool, String> {
    match outcome {
        scratch::ProbeOutcome::Ok => Ok(true),
        scratch::ProbeOutcome::Auth(code) => {
            Err(format!("上游拒绝（{code}），key/权限有误，连接未保存。"))
        }
        scratch::ProbeOutcome::ModelError(code) => Err(format!(
            "上游拒绝该模型（{code}），连接未保存。请换一个模型或核对 base_url。"
        )),
        // 无法确认（405/429/5xx/无响应）：落盘但标记未校验，激活时再验。
        // Unsupported(405) 并入此类：save 走 Message 探测，405 罕见（端点/base_url 异常），保守标未校验（与旧行为一致）。
        scratch::ProbeOutcome::Ambiguous(_)
        | scratch::ProbeOutcome::NoResponse
        | scratch::ProbeOutcome::Unsupported(_) => Ok(false),
    }
}

/// active 连接编辑的内存候选值（validate-before-persist 用）：不改的字段为 None。
/// 校验时把它套到旧 profile 的克隆上做 scratch/起正式；提交成功时用**同一套** [`ConnectionEdit::apply`]
/// 逻辑连同 active_id 一起落盘，杜绝「先落盘后校验」导致的「盘新运行旧」（P1-4）。
#[derive(Default)]
pub(crate) struct ConnectionEdit {
    base_url: Option<String>,
    api_format: Option<String>,
    model: Option<String>,
    key: Option<String>,
}

impl ConnectionEdit {
    pub(crate) fn new(
        base_url: Option<String>,
        api_format: Option<String>,
        model: Option<String>,
        key: Option<String>,
    ) -> Self {
        Self {
            base_url,
            api_format,
            model,
            key,
        }
    }

    /// 把非空编辑值套到目标 profile（内存候选与落盘共用同一逻辑）。
    /// 语义与 `update_profile_connection_inner` 一致：None=不改；key 为空串=不改（留占位不覆盖已存 key）。
    pub(crate) fn apply(&self, p: &mut config::Profile) {
        if let Some(u) = &self.base_url {
            p.base_url = u.clone();
        }
        if let Some(f) = &self.api_format {
            p.api_format = f.clone();
        }
        if let Some(m) = &self.model {
            p.model = m.clone();
        }
        if let Some(k) = &self.key {
            if !k.is_empty() {
                p.api_key = k.clone();
            }
        }
    }
}

/// live 探测结果（id + 能力）∪ builtin，去重（按 id）+ 排序（true>null>false，主列表 id 微调靠前）。
pub(crate) fn merge_and_sort_models(
    live: Vec<(String, Option<bool>)>,
    builtin: &[&str],
) -> Vec<serde_json::Value> {
    let mut seen = std::collections::BTreeSet::new();
    let mut merged: Vec<(String, Option<bool>)> = Vec::new();
    for (id, st) in live {
        if seen.insert(id.clone()) {
            merged.push((id, st));
        }
    }
    for b in builtin {
        if seen.insert(b.to_string()) {
            merged.push((b.to_string(), None));
        }
    }
    merged.sort_by_key(|(id, st)| {
        let cap = match st {
            Some(true) => 0u8,
            None => 1,
            Some(false) => 2,
        };
        let main = if is_main_list_model(id) { 0u8 } else { 1 };
        (cap, main)
    });
    merged
        .into_iter()
        .map(|(id, st)| json!({ "id": id, "supports_tools": st }))
        .collect()
}

/// 探测类型选择（纯函数，修真机 P1）：
/// - 原生 adapter（deepseek/qwen）的 `/v1/models` 是【静态列表、不回源】，探不出坏 key，故一律用
///   Message 探测（打 `/v1/messages` 会真发上游，坏 key → 401）。
/// - relay：留空用 Models（`/v1/models` 回源即可验端点+鉴权）；选了具体模型用 Message 验该模型。
pub(crate) fn probe_kind_for(adapter: &str, model: &str) -> scratch::ProbeKind {
    if is_native_adapter(adapter) {
        return scratch::ProbeKind::Message; // native /v1/models 静态，只有 Message 打上游能验 key。
    }
    probe_kind_for_model(model)
}

/// 选了模型 → 验具体模型（POST /v1/messages）；留空 → 验端点+鉴权（GET /v1/models）。
pub(crate) fn probe_kind_for_model(model: &str) -> scratch::ProbeKind {
    if model.trim().is_empty() {
        scratch::ProbeKind::Models
    } else {
        scratch::ProbeKind::Message
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_get_config, build_list_templates, clear_profile_key_inner, create_profile_inner,
        delete_profile_inner, is_main_list_model, merge_and_sort_models, nonactive_probe_verdict,
        probe_kind_for, probe_kind_for_model, profile_capabilities, template_capabilities,
        update_profile_connection_inner, update_profile_metadata_inner, ConnectionEdit,
    };
    use crate::config;

    /// 每个测试用独立临时 `.csswitch` 目录（进程 id + 线程 id + 随机后缀），互不干扰。
    fn tmpdir_profile() -> std::path::PathBuf {
        let base =
            std::env::temp_dir().join(format!("csswitch-profile-test-{}", std::process::id()));
        let d = base.join(format!(
            "{:?}-{}",
            std::thread::current().id(),
            config::new_id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join(".csswitch")
    }

    // ---------- P2-d: 非 active「如实标记后保存」裁决（明确拒绝才拦；200=已校验；含糊/无响应=落盘但未校验） ----------
    #[test]
    fn nonactive_probe_verdict_maps_outcomes() {
        use crate::scratch::ProbeOutcome;
        assert!(
            nonactive_probe_verdict(&ProbeOutcome::Auth(401))
                .unwrap_err()
                .contains("401"),
            "401 明确鉴权失败 → 拦下不落盘"
        );
        assert!(
            nonactive_probe_verdict(&ProbeOutcome::ModelError(404))
                .unwrap_err()
                .contains("404"),
            "404 模型不被接受 → 拦下不落盘"
        );
        assert_eq!(
            nonactive_probe_verdict(&ProbeOutcome::Ok),
            Ok(true),
            "200 → 落盘且【已校验】"
        );
        assert_eq!(
            nonactive_probe_verdict(&ProbeOutcome::Ambiguous(Some(429))),
            Ok(false),
            "含糊(429) → best-effort 落盘但【未校验】"
        );
        assert_eq!(
            nonactive_probe_verdict(&ProbeOutcome::NoResponse),
            Ok(false),
            "无响应 → best-effort 落盘但【未校验】"
        );
    }

    // ---------- MP-2 fix [1]: 连接编辑 validate-before-persist 的字段应用逻辑（内存/落盘共用） ----------
    #[test]
    fn connection_edit_apply_only_changes_provided_fields() {
        use crate::config::Profile;
        let mut p = Profile {
            base_url: "old-url".into(),
            api_format: "anthropic".into(),
            model: "old-model".into(),
            api_key: "old-key".into(),
            ..Default::default()
        };
        let edit = ConnectionEdit::new(
            Some("new-url".into()),
            None, // None = 不改
            Some("new-model".into()),
            Some(String::new()), // 空 key = 不改（留占位不覆盖已存 key）
        );
        edit.apply(&mut p);
        assert_eq!(p.base_url, "new-url");
        assert_eq!(p.api_format, "anthropic", "None 字段不改");
        assert_eq!(p.model, "new-model");
        assert_eq!(p.api_key, "old-key", "空 key 不覆盖已存 key");

        // 非空 key 覆盖；其余 None 不动。
        let edit2 = ConnectionEdit::new(None, None, None, Some("new-key".into()));
        edit2.apply(&mut p);
        assert_eq!(p.api_key, "new-key", "非空 key 覆盖");
        assert_eq!(p.base_url, "new-url", "None 字段不改");
        assert_eq!(p.model, "new-model", "None 字段不改");
    }

    // ---------- B4: profile CRUD *_inner ----------
    #[test]
    fn create_profile_from_template_prefills() {
        let d = tmpdir_profile();
        let id =
            create_profile_inner(&d, "glm", "我的 GLM", Some("gk"), None, Some("glm-5.2")).unwrap();
        let cfg = config::load_from(&d).unwrap();
        let p = cfg.profile_by_id(&id).unwrap();
        assert_eq!(p.template_id, "glm");
        assert_eq!(p.name, "我的 GLM");
        assert_eq!(p.api_format, "anthropic");
        assert_eq!(p.base_url, "https://open.bigmodel.cn/api/anthropic");
        assert_eq!(p.api_key, "gk");
        assert_eq!(cfg.active_id, "", "新建不自动生效");
    }

    #[test]
    fn create_relay_without_model_is_rejected() {
        // 修 #9 P1-a：后端命令层直接创建 relay/自定义端点空 model 也被拦（不变量不可绕过）。
        let d = tmpdir_profile();
        let e = create_profile_inner(&d, "glm", "GLM", Some("gk"), None, None);
        assert!(e.is_err(), "relay 空 model 应拒绝创建");
        assert!(e.unwrap_err().contains("模型"));
        // native 不受约束（model 可空）。
        assert!(create_profile_inner(&d, "deepseek", "DS", Some("gk"), None, None).is_ok());
    }

    #[test]
    fn update_metadata_does_not_touch_key() {
        let d = tmpdir_profile();
        let id =
            create_profile_inner(&d, "glm", "GLM", Some("secret9"), None, Some("glm-5.2")).unwrap();
        update_profile_metadata_inner(&d, &id, "改名", Some("备注")).unwrap();
        let cfg = config::load_from(&d).unwrap();
        let p = cfg.profile_by_id(&id).unwrap();
        assert_eq!(p.name, "改名");
        assert_eq!(p.notes.as_deref(), Some("备注"));
        assert_eq!(p.api_key, "secret9", "元数据编辑不动 key");
    }

    #[test]
    fn clear_key_empties_key_and_drops_backup() {
        let d = tmpdir_profile();
        let id = create_profile_inner(&d, "glm", "GLM", Some("secretTAIL"), None, Some("glm-5.2"))
            .unwrap();
        config::write_rolling_backup(&d).ok();
        clear_profile_key_inner(&d, &id).unwrap();
        let cfg = config::load_from(&d).unwrap();
        assert_eq!(cfg.profile_by_id(&id).unwrap().api_key, "");
        assert!(!d.join("config.json.bak").exists(), "清 key 后净化滚动备份");
    }

    #[test]
    fn delete_active_clears_active() {
        let d = tmpdir_profile();
        let id = create_profile_inner(&d, "glm", "GLM", Some("k"), None, Some("glm-5.2")).unwrap();
        config::update(&d, |c| c.active_id = id.clone()).unwrap();
        delete_profile_inner(&d, &id).unwrap();
        let cfg = config::load_from(&d).unwrap();
        assert!(cfg.profile_by_id(&id).is_none());
        assert_eq!(cfg.active_id, "", "删 active → 置空");
    }

    #[test]
    fn update_connection_rejects_unsupported_format() {
        let d = tmpdir_profile();
        let id =
            create_profile_inner(&d, "custom", "C", None, Some("https://x/y"), Some("m")).unwrap();
        let e = update_profile_connection_inner(
            &d,
            &id,
            Some("https://x/y"),
            Some("gemini_native"),
            None,
            None,
        );
        assert!(e.is_err());
    }

    // ---------- MP-2 Minor [4]: 未命中 id → Err（不静默 Ok） ----------
    #[test]
    fn update_metadata_unknown_id_errors() {
        let d = tmpdir_profile();
        create_profile_inner(&d, "glm", "GLM", Some("k"), None, Some("glm-5.2")).unwrap();
        let e = update_profile_metadata_inner(&d, "no-such-id", "改名", None);
        assert!(e.is_err(), "未命中 id 应报错，而非静默成功");
        assert!(e.unwrap_err().contains("找不到 profile"));
    }

    #[test]
    fn update_connection_unknown_id_errors() {
        let d = tmpdir_profile();
        create_profile_inner(&d, "glm", "GLM", Some("k"), None, Some("glm-5.2")).unwrap();
        let e = update_profile_connection_inner(
            &d,
            "no-such-id",
            Some("https://x/y"),
            None,
            None,
            None,
        );
        assert!(e.is_err(), "未命中 id 应报错，而非静默成功");
        assert!(e.unwrap_err().contains("找不到 profile"));
    }

    // ---------- B5: build_get_config / build_list_templates ----------
    #[test]
    fn get_config_masks_keys_and_lists_profiles() {
        let d = tmpdir_profile();
        let id = create_profile_inner(
            &d,
            "glm",
            "GLM",
            Some("sk-longsecret9999"),
            None,
            Some("glm-5.2"),
        )
        .unwrap();
        let v = build_get_config(&d).unwrap();
        assert_eq!(v["schema_version"], 2);
        let arr = v["profiles"].as_array().unwrap();
        let p = arr.iter().find(|p| p["id"] == id).unwrap();
        assert!(p["key"].as_str().unwrap().ends_with("9999"));
        assert!(
            !p["key"].as_str().unwrap().contains("longsecret"),
            "只回掩码"
        );
        assert!(
            p.get("api_key").is_none() || p["api_key"].is_null(),
            "全 key 不出后端"
        );
        assert_eq!(p["has_key"], true);
        assert_eq!(p["key_masked"], p["key"], "保留旧 key 字段并补新掩码字段");
        assert_eq!(p["capabilities"]["model_required"], true);
        assert_eq!(
            p["capabilities"]["model_discovery"],
            "anthropic_models_or_manual"
        );
    }

    #[test]
    fn get_config_returns_notes_so_rename_does_not_wipe_them() {
        // M1 回归：build_get_config 必须回传 notes，否则前端读到空、下次改名把备注静默清掉。
        let d = tmpdir_profile();
        let id = create_profile_inner(&d, "glm", "GLM", Some("k"), None, Some("glm-5.2")).unwrap();
        update_profile_metadata_inner(&d, &id, "GLM", Some("我的备注")).unwrap();
        let v = build_get_config(&d).unwrap();
        let p = v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap();
        assert_eq!(p["notes"], "我的备注", "notes 必须随 get_config 回传");
    }

    #[test]
    fn list_templates_has_eleven() {
        let v = build_list_templates();
        assert_eq!(v.len(), 11);
        assert!(v.iter().any(|t| t["id"] == "custom"));
        assert!(v.iter().any(|t| t["id"] == "custom-openai"));
        assert!(v.iter().any(|t| t["id"] == "custom-openai-responses"));
        assert!(v.iter().any(|t| t["id"] == "kimi"));
        assert!(v.iter().any(|t| t["id"] == "minimax"));
        let qwen = v.iter().find(|t| t["id"] == "qwen").unwrap();
        assert_eq!(qwen["capabilities"]["model_discovery"], "builtin_static");
        assert_eq!(qwen["capabilities"]["supports_tools_hint"], "translated");
        let custom = v.iter().find(|t| t["id"] == "custom-openai").unwrap();
        assert_eq!(
            custom["capabilities"]["model_discovery"],
            "openai_models_or_manual"
        );
        assert_eq!(custom["capabilities"]["base_url_required"], true);
    }

    #[test]
    fn capabilities_are_derived_from_template_contract() {
        let ds = template_capabilities(crate::templates::by_id("deepseek").unwrap());
        assert_eq!(ds["base_url_required"], false);
        assert_eq!(ds["model_required"], false);
        assert_eq!(ds["model_discovery"], "builtin_static");

        let relay = template_capabilities(crate::templates::by_id("glm").unwrap());
        assert_eq!(relay["base_url_required"], true);
        assert_eq!(relay["model_required"], true);
        assert_eq!(relay["model_discovery"], "anthropic_models_or_manual");
        assert_eq!(relay["thinking_policy"], "adaptive");

        let p = config::Profile {
            template_id: "custom-openai-responses".into(),
            ..Default::default()
        };
        assert_eq!(
            profile_capabilities(&p)["model_discovery"],
            "openai_models_or_manual"
        );
    }

    #[test]
    fn profile_capabilities_follow_profile_api_format_when_present() {
        let p = config::Profile {
            template_id: "custom".into(),
            api_format: "openai_responses".into(),
            ..Default::default()
        };
        let caps = profile_capabilities(&p);
        assert_eq!(caps["model_discovery"], "openai_models_or_manual");
        assert_eq!(caps["supports_tools_hint"], "translated");
    }

    #[test]
    fn main_list_model_matches_family_plus_digit() {
        assert!(is_main_list_model("claude-opus-4-8"));
        assert!(is_main_list_model("claude-sonnet-5"));
        assert!(is_main_list_model("claude-haiku-4-5-20251001"));
        assert!(!is_main_list_model("claude-3-5-sonnet-20241022"));
        assert!(!is_main_list_model("claude-fable-5"));
        assert!(!is_main_list_model("gpt-4o"));
    }

    #[test]
    fn merge_and_sort_prefers_tools_then_dedupes_builtin() {
        let live = vec![
            ("m-notools".to_string(), Some(false)),
            ("m-tools".to_string(), Some(true)),
            ("m-unknown".to_string(), None),
        ];
        let out = merge_and_sort_models(live, &["m-tools", "m-builtin-only"]);
        let ids: Vec<String> = out
            .iter()
            .map(|v| v.get("id").unwrap().as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids[0], "m-tools");
        assert!(ids.contains(&"m-builtin-only".to_string()));
        assert_eq!(ids.iter().filter(|i| *i == "m-tools").count(), 1, "去重");
        assert_eq!(ids.last().unwrap(), "m-notools");
    }

    #[test]
    fn probe_kind_picks_message_when_model_set() {
        assert!(matches!(
            probe_kind_for_model("mimo-v2.5-pro"),
            crate::scratch::ProbeKind::Message
        ));
        assert!(matches!(
            probe_kind_for_model(""),
            crate::scratch::ProbeKind::Models
        ));
    }

    // ---------- 修真机 P1：native adapter 上游校验（GPT 验收报告 RM-06） ----------

    #[test]
    fn native_probe_uses_message_since_native_models_is_static() {
        // native 的 /v1/models 是静态列表、探不出坏 key，故一律用 Message（打上游 /v1/messages）。
        assert!(matches!(
            probe_kind_for("deepseek", ""),
            crate::scratch::ProbeKind::Message
        ));
        assert!(matches!(
            probe_kind_for("qwen", ""),
            crate::scratch::ProbeKind::Message
        ));
        // relay：空 model 用 Models（/v1/models 回源即验鉴权）；选了 model 用 Message 验该模型。
        assert!(matches!(
            probe_kind_for("relay", ""),
            crate::scratch::ProbeKind::Models
        ));
        assert!(matches!(
            probe_kind_for("relay", "m1"),
            crate::scratch::ProbeKind::Message
        ));
    }
}
