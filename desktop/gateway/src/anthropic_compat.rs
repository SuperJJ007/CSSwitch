use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

const RELAY_DEFAULT_MODEL: &str = "claude-opus-4-8";
const RULE_PROVIDER_RELAY_FORCE_MODEL_SHELL: &str = "provider.relay.force-model-shell";
const RULE_PROVIDER_KIMI_RELAY_THINKING_ENABLED: &str = "provider.kimi.relay-thinking-enabled";
const RULE_TOOL_RELAY_INPUT_SCHEMA_NORMALIZE: &str = "tool.relay.input-schema-normalize";
const RULE_TOOL_KIMI_WEB_SEARCH_SERVER_TOOL_FILTER: &str =
    "tool.kimi.web_search.server-tool-filter";
const RULE_TOOL_SILICONFLOW_FORCED_NAMED_TO_ANY: &str = "tool.siliconflow.forced-named-to-any";
const SILICONFLOW_API_HOSTS: [&str; 2] = ["api.siliconflow.cn", "api.siliconflow.com"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicMetadata {
    pub target_model: String,
    pub rule_ids: Vec<String>,
}

#[derive(Debug, Default)]
pub struct KimiServerToolFilter {
    buf: Vec<u8>,
    skip: HashSet<i64>,
    index_map: HashMap<i64, i64>,
    next_index: i64,
    dropped: usize,
}

impl KimiServerToolFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((frame, sep_len, rest)) = split_frame(&self.buf) {
            let sep = self.buf[frame.len()..frame.len() + sep_len].to_vec();
            let rewritten = self.rewrite_frame(&frame, &sep);
            out.extend_from_slice(&rewritten);
            self.buf = rest;
        }
        out
    }

    pub fn finalize(&mut self) -> Vec<u8> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let frame = std::mem::take(&mut self.buf);
        self.rewrite_frame(&frame, b"\n\n")
    }

    pub fn dropped(&self) -> usize {
        self.dropped
    }

    fn mapped_index(&mut self, idx: i64) -> i64 {
        if let Some(mapped) = self.index_map.get(&idx) {
            return *mapped;
        }
        let mapped = self.next_index;
        self.next_index += 1;
        self.index_map.insert(idx, mapped);
        mapped
    }

    fn rewrite_frame(&mut self, frame: &[u8], sep: &[u8]) -> Vec<u8> {
        let (event, data) = event_and_data(frame);
        if data.is_empty() {
            return passthrough(frame, sep);
        }
        let Ok(mut obj) = serde_json::from_slice::<Value>(&data) else {
            return passthrough(frame, sep);
        };
        let Some(kind) = obj.get("type").and_then(Value::as_str) else {
            return passthrough(frame, sep);
        };
        if kind == "content_block_start" {
            let Some(idx) = obj.get("index").and_then(Value::as_i64) else {
                return passthrough(frame, sep);
            };
            let block_type = obj
                .get("content_block")
                .and_then(Value::as_object)
                .and_then(|block| block.get("type"))
                .and_then(Value::as_str);
            if matches!(
                block_type,
                Some("server_tool_use" | "web_search_tool_result")
            ) {
                self.skip.insert(idx);
                self.dropped += 1;
                return Vec::new();
            }
            if let Some(obj_map) = obj.as_object_mut() {
                obj_map.insert(
                    "index".to_string(),
                    Value::Number(self.mapped_index(idx).into()),
                );
            }
            return render_sse(event.as_deref(), &obj);
        }
        if kind == "content_block_delta" || kind == "content_block_stop" {
            let Some(idx) = obj.get("index").and_then(Value::as_i64) else {
                return passthrough(frame, sep);
            };
            if self.skip.contains(&idx) {
                return Vec::new();
            }
            if let Some(mapped) = self.index_map.get(&idx).copied() {
                if let Some(obj_map) = obj.as_object_mut() {
                    obj_map.insert("index".to_string(), Value::Number(mapped.into()));
                }
                return render_sse(event.as_deref(), &obj);
            }
        }
        passthrough(frame, sep)
    }
}

fn split_frame(buf: &[u8]) -> Option<(Vec<u8>, usize, Vec<u8>)> {
    let lf = buf.windows(2).position(|window| window == b"\n\n");
    let crlf = buf.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (None, None) => None,
        (Some(i), None) => Some((buf[..i].to_vec(), 2, buf[i + 2..].to_vec())),
        (None, Some(i)) => Some((buf[..i].to_vec(), 4, buf[i + 4..].to_vec())),
        (Some(lf_i), Some(crlf_i)) if lf_i <= crlf_i => {
            Some((buf[..lf_i].to_vec(), 2, buf[lf_i + 2..].to_vec()))
        }
        (Some(_), Some(crlf_i)) => Some((buf[..crlf_i].to_vec(), 4, buf[crlf_i + 4..].to_vec())),
    }
}

fn event_and_data(frame: &[u8]) -> (Option<String>, Vec<u8>) {
    let normalized = String::from_utf8_lossy(frame).replace("\r\n", "\n");
    let mut event = None;
    let mut data = Vec::new();
    for line in normalized.split('\n') {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim_start().as_bytes().to_vec());
        }
    }
    (event, data.join(b"\n".as_slice()))
}

fn render_sse(event: Option<&str>, obj: &Value) -> Vec<u8> {
    let data = serde_json::to_vec(obj).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = Vec::new();
    if let Some(event) = event {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(event.as_bytes());
        out.extend_from_slice(b"\n");
    }
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(&data);
    out.extend_from_slice(b"\n\n");
    out
}

fn passthrough(frame: &[u8], sep: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + sep.len());
    out.extend_from_slice(frame);
    out.extend_from_slice(sep);
    out
}

fn append_rule_id(rule_ids: &mut Vec<String>, rule_id: &str) {
    if !rule_ids.iter().any(|existing| existing == rule_id) {
        rule_ids.push(rule_id.to_string());
    }
}

fn is_siliconflow_anthropic_endpoint(endpoint: &str) -> bool {
    let raw_authority = endpoint
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    if raw_authority.contains('@') {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = if let Some(without_dot) = host.strip_suffix('.') {
        if without_dot.ends_with('.') {
            return false;
        }
        without_dot
    } else {
        host
    };
    SILICONFLOW_API_HOSTS
        .iter()
        .any(|official| host.eq_ignore_ascii_case(official))
}

pub fn resolve_relay_model(
    name: Option<&str>,
    forced_model: Option<&str>,
    relay_models: &[String],
) -> String {
    if let Some(model) = forced_model.filter(|model| !model.is_empty()) {
        return model.to_string();
    }
    let name = name.unwrap_or("");
    if name.is_empty() {
        return RELAY_DEFAULT_MODEL.to_string();
    }
    if relay_models.is_empty() || relay_models.iter().any(|model| model == name) {
        return name.to_string();
    }
    relay_models
        .iter()
        .find(|model| model.starts_with(&format!("{name}-")) || *model == name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

fn enabled_budget(max_tokens: Option<u64>) -> u64 {
    let default = 1024;
    match max_tokens {
        Some(value) if value > 0 => default.min(value.saturating_sub(1)).max(1),
        _ => default,
    }
}

fn is_forced_tool_choice(body: &Value) -> bool {
    body.get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        .map(|kind| kind == "any" || kind == "tool")
        .unwrap_or(false)
}

fn normalize_relay_thinking(body: &mut Value, relay_thinking: Option<&str>) {
    if relay_thinking == Some("enabled") {
        if is_forced_tool_choice(body) {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("tool_choice");
            }
        }
        let already_enabled = body
            .get("thinking")
            .and_then(Value::as_object)
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            == Some("enabled");
        if !already_enabled {
            let budget = enabled_budget(body.get("max_tokens").and_then(Value::as_u64));
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }
        return;
    }

    if body
        .get("thinking")
        .and_then(Value::as_object)
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        == Some("auto")
    {
        if let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) {
            thinking.insert("type".to_string(), Value::String("adaptive".to_string()));
        }
    }
}

fn normalize_relay_input_schema(schema: Option<&Value>) -> Value {
    let Some(Value::Object(obj)) = schema else {
        return json!({"type": "object", "properties": {}});
    };
    if obj.is_empty() {
        return json!({"type": "object", "properties": {}});
    }
    let mut out = obj.clone();
    let has_properties = out.get("properties").map(Value::is_object).unwrap_or(false);
    match out.get("type").and_then(Value::as_str) {
        None if has_properties => {
            out.insert("type".to_string(), Value::String("object".to_string()));
        }
        Some("object") => {}
        _ => return json!({"type": "object", "properties": {}}),
    }
    if !out.get("properties").map(Value::is_object).unwrap_or(false) {
        out.insert("properties".to_string(), json!({}));
    }
    if out.get("required").map(Value::is_array) == Some(false) {
        out.remove("required");
    }
    Value::Object(out)
}

fn degrade_missing_tool_choice(body: &mut Value) {
    let Some(choice) = body.get("tool_choice").and_then(Value::as_object) else {
        return;
    };
    if choice.get("type").and_then(Value::as_str) != Some("tool") {
        return;
    }
    let choice_name = choice.get("name").and_then(Value::as_str).unwrap_or("");
    let exists = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|tool| tool.get("name").and_then(Value::as_str) == Some(choice_name));
    if !exists {
        body["tool_choice"] = json!({"type": "auto"});
    }
}

fn normalize_relay_tools(body: &mut Value, rule_ids: &mut Vec<String>) {
    let Some(tools) = body.get("tools") else {
        return;
    };
    let Some(tool_items) = tools.as_array() else {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
        }
        degrade_missing_tool_choice(body);
        return;
    };

    let mut normalized = Vec::new();
    for tool in tool_items {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mut clean = match tool {
            Value::Object(obj) => obj.clone(),
            _ => Map::new(),
        };
        clean.insert(
            "input_schema".to_string(),
            normalize_relay_input_schema(tool.get("input_schema")),
        );
        normalized.push(Value::Object(clean));
    }
    append_rule_id(rule_ids, RULE_TOOL_RELAY_INPUT_SCHEMA_NORMALIZE);
    if normalized.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
        }
    } else {
        body["tools"] = Value::Array(normalized);
    }
    degrade_missing_tool_choice(body);
}

fn filter_kimi_server_tools(body: &mut Value, target_model: &str, rule_ids: &mut Vec<String>) {
    normalize_relay_tools(body, rule_ids);
    if !target_model.to_ascii_lowercase().contains("kimi") {
        return;
    }
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return;
    };
    let filtered: Vec<Value> = tools
        .iter()
        .filter(|tool| tool.get("name").and_then(Value::as_str) != Some("web_search"))
        .cloned()
        .collect();
    if filtered.len() == tools.len() {
        return;
    }
    append_rule_id(rule_ids, RULE_TOOL_KIMI_WEB_SEARCH_SERVER_TOOL_FILTER);
    if filtered.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
        }
    } else {
        body["tools"] = Value::Array(filtered);
    }
    degrade_missing_tool_choice(body);
}

fn apply_siliconflow_tool_choice_compat(
    body: &mut Value,
    upstream_url: &str,
    rule_ids: &mut Vec<String>,
) {
    if !is_siliconflow_anthropic_endpoint(upstream_url) {
        return;
    }
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| !tools.is_empty())
        .unwrap_or(false);
    if !has_tools {
        return;
    }
    let is_forced_named = body
        .get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        == Some("tool");
    if !is_forced_named {
        return;
    }
    body["tool_choice"] = json!({"type": "any"});
    append_rule_id(rule_ids, RULE_TOOL_SILICONFLOW_FORCED_NAMED_TO_ANY);
}

pub fn transform_relay_request(
    mut body: Value,
    forced_model: Option<&str>,
    relay_models: &[String],
    relay_thinking: Option<&str>,
    upstream_url: &str,
) -> Result<(Value, AnthropicMetadata), String> {
    let obj = body
        .as_object_mut()
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    if !obj.get("messages").map(Value::is_array).unwrap_or(false) {
        return Err("request body must be a JSON object with a 'messages' array".to_string());
    }

    let target_model = resolve_relay_model(
        obj.get("model").and_then(Value::as_str),
        forced_model,
        relay_models,
    );
    let mut rule_ids = Vec::new();
    if forced_model.filter(|model| !model.is_empty()).is_some() {
        append_rule_id(&mut rule_ids, RULE_PROVIDER_RELAY_FORCE_MODEL_SHELL);
    }
    if relay_thinking == Some("enabled") && target_model.to_ascii_lowercase().contains("kimi") {
        append_rule_id(&mut rule_ids, RULE_PROVIDER_KIMI_RELAY_THINKING_ENABLED);
    }
    obj.insert("model".to_string(), Value::String(target_model.clone()));
    normalize_relay_thinking(&mut body, relay_thinking);
    filter_kimi_server_tools(&mut body, &target_model, &mut rule_ids);
    apply_siliconflow_tool_choice_compat(&mut body, upstream_url, &mut rule_ids);
    Ok((
        body,
        AnthropicMetadata {
            target_model,
            rule_ids,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        is_siliconflow_anthropic_endpoint, resolve_relay_model, transform_relay_request,
        KimiServerToolFilter,
    };
    use serde_json::{json, Value};

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../test/golden/relay_anthropic.json")).unwrap()
    }

    #[test]
    fn siliconflow_endpoint_matching_is_exact_and_url_parsed() {
        for endpoint in [
            "https://api.siliconflow.cn",
            "https://API.SILICONFLOW.CN/v1/messages",
            "http://api.siliconflow.com./anthropic/v1/messages",
        ] {
            assert!(is_siliconflow_anthropic_endpoint(endpoint), "{endpoint}");
        }
        for endpoint in [
            "ftp://api.siliconflow.cn/v1/messages",
            "https://sub.api.siliconflow.cn/v1/messages",
            "https://api.siliconflow.cn.evil/v1/messages",
            "https://api.siliconflow.com.evil/v1/messages",
            "https://api.siliconflow.cn@evil.example/v1/messages",
            "https://evil@api.siliconflow.cn/v1/messages",
            "https://@api.siliconflow.cn/v1/messages",
            "https://:pass@api.siliconflow.cn/v1/messages",
            "https://user:@api.siliconflow.cn/v1/messages",
            "https://api.siliconflow.cn../v1/messages",
            "https://evil.example/api.siliconflow.cn/v1/messages",
            "not a url api.siliconflow.cn",
        ] {
            assert!(!is_siliconflow_anthropic_endpoint(endpoint), "{endpoint}");
        }
    }

    #[test]
    fn siliconflow_tool_choice_fixture_matrix_matches_python() {
        let fixture = fixture();
        let cases = fixture["siliconflow_tool_choice_cases"].as_array().unwrap();
        for case in cases {
            let (mapped, metadata) = transform_relay_request(
                case["request"].clone(),
                None,
                &[],
                None,
                case["endpoint"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(mapped, case["mapped"], "{}", case["name"]);
            let expected_rules: Vec<String> =
                serde_json::from_value(case["rule_ids"].clone()).unwrap();
            assert_eq!(metadata.rule_ids, expected_rules, "{}", case["name"]);
        }
    }

    #[test]
    fn relay_snaps_bare_model_and_preserves_max_tokens() {
        let fixture = fixture();
        let relay_models = vec![
            "claude-haiku-4-5-20251001".to_string(),
            "claude-opus-4-8".to_string(),
        ];
        let (mapped, metadata) = transform_relay_request(
            fixture["plain_request"].clone(),
            None,
            &relay_models,
            None,
            "",
        )
        .unwrap();
        assert_eq!(mapped, fixture["plain_mapped"]);
        assert_eq!(metadata.target_model, fixture["plain_target_model"]);
        assert_eq!(metadata.rule_ids, Vec::<String>::new());
    }

    #[test]
    fn relay_force_model_overrides_shell() {
        let fixture = fixture();
        let (mapped, metadata) = transform_relay_request(
            fixture["force_request"].clone(),
            Some("MiniMax-M2"),
            &[],
            None,
            "",
        )
        .unwrap();
        assert_eq!(mapped, fixture["force_mapped"]);
        assert_eq!(metadata.target_model, fixture["force_target_model"]);
        assert_eq!(metadata.rule_ids, vec!["provider.relay.force-model-shell"]);
    }

    #[test]
    fn relay_kimi_thinking_and_tool_quirks_match_python_fixture() {
        let fixture = fixture();
        let (mapped, metadata) = transform_relay_request(
            fixture["kimi_request"].clone(),
            Some("kimi-k2.7-code"),
            &[],
            Some("enabled"),
            "",
        )
        .unwrap();
        assert_eq!(mapped, fixture["kimi_mapped"]);
        assert_eq!(metadata.target_model, fixture["kimi_target_model"]);
        assert_eq!(
            metadata.rule_ids,
            vec![
                "provider.relay.force-model-shell".to_string(),
                "provider.kimi.relay-thinking-enabled".to_string(),
                "tool.relay.input-schema-normalize".to_string(),
                "tool.kimi.web_search.server-tool-filter".to_string(),
            ]
        );
    }

    #[test]
    fn relay_kimi_removed_tool_choice_degrades_to_auto_without_enabled_thinking() {
        let (mapped, _metadata) = transform_relay_request(
            json!({
                "model": "claude-opus-4-8",
                "messages": [],
                "tool_choice": {"type": "tool", "name": "web_search"},
                "tools": [{"name": "web_search", "input_schema": {"type": "object"}}],
            }),
            Some("kimi-k2.7-code"),
            &[],
            None,
            "",
        )
        .unwrap();
        assert!(mapped.get("tools").is_none());
        assert_eq!(mapped["tool_choice"], json!({"type": "auto"}));
    }

    #[test]
    fn resolve_relay_model_defaults_and_passthrough() {
        assert_eq!(resolve_relay_model(None, None, &[]), "claude-opus-4-8");
        assert_eq!(
            resolve_relay_model(Some("claude-opus-4-8"), None, &[]),
            "claude-opus-4-8"
        );
        assert_eq!(
            resolve_relay_model(Some("claude-opus-4-8"), Some("glm-5.2"), &[]),
            "glm-5.2"
        );
        let models = vec![
            "claude-haiku-4-5-20251001".to_string(),
            "claude-haiku-4-5".to_string(),
            "claude-haiku-4-5-20251111".to_string(),
        ];
        assert_eq!(
            resolve_relay_model(Some("claude-haiku-4-5"), None, &models),
            "claude-haiku-4-5",
            "an exact live ID wins over earlier prefix matches"
        );
        assert_eq!(
            resolve_relay_model(Some("claude-haiku"), None, &models),
            "claude-haiku-4-5-20251001",
            "otherwise the first requested-name prefix wins"
        );
    }

    #[test]
    fn kimi_stream_filter_drops_server_tool_blocks_and_compacts_indexes() {
        let sse = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"name\":\"web_search\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"web_search_tool_result\",\"content\":[]}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":4,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n"
        );
        let mut filter = KimiServerToolFilter::new();
        let midpoint = sse.len() / 2;
        let mut out = filter.feed(&sse.as_bytes()[..midpoint]);
        out.extend(filter.feed(&sse.as_bytes()[midpoint..]));
        out.extend(filter.finalize());
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("server_tool_use"));
        assert!(!text.contains("web_search_tool_result"));
        assert!(text.contains("\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\""));
        assert!(text.contains("\"index\":1"));
        assert!(text.contains("\"index\":2"));
        assert!(text.contains("\"text\":\"OK\""));
        assert_eq!(filter.dropped(), 2);
    }
}
