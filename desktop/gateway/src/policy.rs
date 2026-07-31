use serde_json::Value;

pub fn clamp_max_tokens(value: Option<u64>, model: &str) -> Option<u64> {
    let cap = match model {
        "deepseek-v4-pro" => 65_536,
        "deepseek-v4-flash" => 32_768,
        _ => 8_192,
    };
    value.map(|v| v.min(cap))
}

pub fn normalize_thinking(body: &mut Value) {
    let forcing = body
        .get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|tc| tc.get("type"))
        .and_then(Value::as_str)
        .map(|t| t == "any" || t == "tool")
        .unwrap_or(false);
    if forcing {
        body["thinking"] = serde_json::json!({"type": "disabled"});
        return;
    }
    // Claude Science sends "auto"; DeepSeek's Anthropic endpoint handles it
    // natively — no provider-specific remapping needed.
}

/// DeepSeek thinking mode has two distinct rules for multi-turn history:
/// - No tool calls in the assistant turn: reasoning_content is optional and
///   ignored by the API. We strip it for consistency with Claude Science.
/// - Tool calls present: reasoning_content MUST be echoed back in ALL
///   subsequent turns, otherwise the API returns 400.
fn strip_thinking_from_history(messages: &mut Value) {
    let Some(arr) = messages.as_array_mut() else {
        return;
    };
    for msg in arr.iter_mut() {
        let Some(role) = msg.get("role").and_then(Value::as_str) else {
            continue;
        };
        if role != "assistant" {
            continue;
        }
        let Some(content) = msg.get("content") else {
            continue;
        };
        let Some(items) = content.as_array() else {
            continue;
        };

        // This assistant turn involved tool use → preserve thinking
        let has_tool_use = items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"));

        if has_tool_use {
            continue;
        }

        // No tool use → strip thinking (API ignores it anyway)
        let Some(items_mut) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        items_mut.retain(|item| item.get("type").and_then(Value::as_str) != Some("thinking"));
    }
}

pub fn transform_request(mut body: Value, target_model: &str) -> Result<Vec<u8>, String> {
    let obj = body
        .as_object_mut()
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    if !obj.get("messages").map(Value::is_array).unwrap_or(false) {
        return Err("request body must be a JSON object with a 'messages' array".to_string());
    }
    obj.insert("model".to_string(), Value::String(target_model.to_string()));
    if let Some(max_tokens) = obj.get("max_tokens").and_then(Value::as_u64) {
        obj.insert(
            "max_tokens".to_string(),
            Value::Number(serde_json::Number::from(
                clamp_max_tokens(Some(max_tokens), target_model).unwrap_or(max_tokens),
            )),
        );
    }
    normalize_thinking(&mut body);
    strip_thinking_from_history(&mut body["messages"]);
    serde_json::to_vec(&body).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{clamp_max_tokens, strip_thinking_from_history, transform_request};
    use serde_json::json;

    #[test]
    fn clamps_deepseek_max_tokens() {
        assert_eq!(
            clamp_max_tokens(Some(100_000), "deepseek-v4-pro"),
            Some(65_536)
        );
        assert_eq!(clamp_max_tokens(Some(500), "deepseek-v4-pro"), Some(500));
    }

    #[test]
    fn transform_maps_model_and_normalizes_thinking() {
        let raw = json!({
            "model": "claude-opus-4-8",
            "max_tokens": 100000,
            "thinking": {"type": "auto"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let bytes = transform_request(raw, "deepseek-v4-pro").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["model"], "deepseek-v4-pro");
        assert_eq!(v["max_tokens"], 65536);
        assert_eq!(v["thinking"]["type"], "auto");
    }

    #[test]
    fn forced_tool_choice_disables_thinking() {
        let raw = json!({
            "model": "claude-opus-4-8",
            "tool_choice": {"type": "any"},
            "thinking": {"type": "auto"},
            "messages": []
        });
        let bytes = transform_request(raw, "deepseek-v4-pro").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["thinking"]["type"], "disabled");
    }

    #[test]
    fn strips_thinking_from_assistant_messages_in_history() {
        let mut messages = json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "let me think...", "signature": "sig1"},
                {"type": "text", "text": "hi there"}
            ]},
            {"role": "user", "content": "more"}
        ]);
        strip_thinking_from_history(&mut messages);
        let arr = messages.as_array().unwrap();
        // user messages unchanged
        assert_eq!(arr[0]["content"], "hello");
        // assistant: thinking removed, text kept
        let content = arr[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        // last user unchanged
        assert_eq!(arr[2]["content"], "more");
    }

    #[test]
    fn strips_thinking_from_multi_turn_request() {
        let raw = json!({
            "model": "claude-sonnet-5",
            "max_tokens": 1000,
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm...", "signature": "abc"},
                    {"type": "text", "text": "hi"}
                ]},
                {"role": "user", "content": "what else?"}
            ]
        });
        let bytes = transform_request(raw, "deepseek-v4-flash").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        let content = msgs[1]["content"].as_array().unwrap();
        // No tool_use → thinking stripped
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn preserves_thinking_when_assistant_has_tool_use() {
        // With tool_use → thinking MUST be preserved per DeepSeek docs
        let mut messages = json!([
            {"role": "user", "content": "what's the weather?"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "I need to call the weather tool", "signature": "sig"},
                {"type": "tool_use", "id": "tool_1", "name": "get_weather", "input": {}}
            ]},
            {"role": "user", "content": "and tomorrow?"}
        ]);
        strip_thinking_from_history(&mut messages);
        let arr = messages.as_array().unwrap();
        let content = arr[1]["content"].as_array().unwrap();
        // tool_use present → thinking preserved
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[1]["type"], "tool_use");
    }
}
