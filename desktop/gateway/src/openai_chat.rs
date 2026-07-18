use serde_json::{json, Map, Value};

pub fn clamp_qwen_max_tokens(value: Option<u64>) -> Option<u64> {
    value.map(|v| v.min(8192))
}

pub fn map_tool_choice(tool_choice: Option<&Value>, tools: Option<&Value>) -> Option<Value> {
    let tc = tool_choice?.as_object()?;
    match tc.get("type").and_then(Value::as_str) {
        Some("auto") => Some(Value::String("auto".to_string())),
        Some("none") => Some(Value::String("none".to_string())),
        Some("tool") => tc
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        Some("any") => {
            let names: Vec<&str> = tools
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .collect();
            if names.len() == 1 {
                Some(json!({"type": "function", "function": {"name": names[0]}}))
            } else {
                Some(Value::String("required".to_string()))
            }
        }
        _ => None,
    }
}

fn json_dumps_python_style(value: &Value) -> String {
    let compact = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let mut out = String::with_capacity(compact.len() + 8);
    let mut in_string = false;
    let mut escaped = false;
    for ch in compact.chars() {
        out.push(ch);
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
        } else if ch == ':' || ch == ',' {
            out.push(' ');
        }
    }
    out
}

fn tool_result_content(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => json_dumps_python_style(other),
        None => String::new(),
    }
}

pub fn anthropic_to_openai(req: &Value, target_model: &str) -> Result<Value, String> {
    anthropic_to_openai_with_model(req, target_model.to_string(), Some(8192))
}

pub fn anthropic_to_openai_custom(req: &Value, target_model: &str) -> Result<Value, String> {
    anthropic_to_openai_with_model(req, target_model.to_string(), None)
}

fn anthropic_to_openai_with_model(
    req: &Value,
    target_model: String,
    max_token_cap: Option<u64>,
) -> Result<Value, String> {
    let obj = req
        .as_object()
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    if !obj.get("messages").map(Value::is_array).unwrap_or(false) {
        return Err("request body must be a JSON object with a 'messages' array".to_string());
    }

    let mut msgs = Vec::new();
    if let Some(system) = obj.get("system") {
        let sys_prompt = match system {
            Value::Array(items) => items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            Value::String(s) => s.clone(),
            _ => String::new(),
        };
        if !sys_prompt.is_empty() {
            msgs.push(json!({"role": "system", "content": sys_prompt}));
        }
    }

    for message in obj.get("messages").and_then(Value::as_array).unwrap() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let content = message.get("content").unwrap_or(&Value::Null);
        if let Some(text) = content.as_str() {
            msgs.push(json!({"role": role, "content": text}));
            continue;
        }

        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        for block in content.as_array().into_iter().flatten() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    text_parts.push(block.get("text").and_then(Value::as_str).unwrap_or(""));
                }
                Some("tool_use") => {
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    tool_calls.push(json!({
                        "id": block.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": {
                            "name": block.get("name").cloned().unwrap_or(Value::Null),
                            "arguments": json_dumps_python_style(&input)
                        }
                    }));
                }
                Some("tool_result") => {
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": block.get("tool_use_id").cloned().unwrap_or(Value::Null),
                        "content": tool_result_content(block.get("content"))
                    }));
                }
                _ => {}
            }
        }
        let joined_text = text_parts.join("");
        if role == "assistant" && !tool_calls.is_empty() {
            msgs.push(json!({
                "role": "assistant",
                "content": if joined_text.is_empty() { Value::Null } else { Value::String(joined_text) },
                "tool_calls": tool_calls
            }));
        } else if !tool_results.is_empty() {
            msgs.extend(tool_results);
            if !joined_text.is_empty() {
                msgs.push(json!({"role": role, "content": joined_text}));
            }
        } else {
            msgs.push(json!({"role": role, "content": joined_text}));
        }
    }

    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(target_model));
    out.insert("messages".to_string(), Value::Array(msgs));
    out.insert("stream".to_string(), Value::Bool(false));
    if let Some(max_tokens) = obj.get("max_tokens").and_then(Value::as_u64) {
        let value = max_token_cap
            .map(|cap| max_tokens.min(cap))
            .unwrap_or(max_tokens);
        out.insert(
            "max_tokens".to_string(),
            Value::Number(serde_json::Number::from(value)),
        );
    }
    if let Some(temperature) = obj.get("temperature") {
        if !temperature.is_null() {
            out.insert("temperature".to_string(), temperature.clone());
        }
    }
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        if !tools.is_empty() {
            let mapped: Vec<Value> = tools
                .iter()
                .filter_map(|tool| {
                    let name = tool.get("name").and_then(Value::as_str)?;
                    Some(json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": tool.get("description").and_then(Value::as_str).unwrap_or(""),
                            "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({}))
                        }
                    }))
                })
                .collect();
            if !mapped.is_empty() {
                out.insert("tools".to_string(), Value::Array(mapped));
            }
        }
    }
    if let Some(mapped) = map_tool_choice(obj.get("tool_choice"), obj.get("tools")) {
        out.insert("tool_choice".to_string(), mapped);
    }
    if let Some(stop) = obj.get("stop_sequences") {
        out.insert("stop".to_string(), stop.clone());
    }
    if let Some(top_p) = obj.get("top_p") {
        if !top_p.is_null() {
            out.insert("top_p".to_string(), top_p.clone());
        }
    }
    Ok(Value::Object(out))
}

pub fn openai_to_anthropic(resp: &Value, model_id: &str) -> Value {
    let choice = resp
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .unwrap_or(&Value::Null);
    let msg = choice.get("message").unwrap_or(&Value::Null);
    let mut blocks = Vec::new();
    if let Some(content) = msg.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            blocks.push(json!({"type": "text", "text": content}));
        }
    }
    for tc in msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let function = tc.get("function").unwrap_or(&Value::Null);
        let args = function
            .get("arguments")
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .unwrap_or_else(|| json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": tc.get("id").cloned().unwrap_or(Value::Null),
            "name": function.get("name").cloned().unwrap_or(Value::Null),
            "input": args
        }));
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    let stop_reason = match choice.get("finish_reason").and_then(Value::as_str) {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        _ => "end_turn",
    };
    let usage = resp.get("usage").unwrap_or(&Value::Null);
    json!({
        "id": resp.get("id").and_then(Value::as_str).unwrap_or("msg_proxy"),
        "type": "message",
        "role": "assistant",
        "model": model_id,
        "content": blocks,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0)
        }
    })
}

pub fn replay_as_sse_events(aresp: &Value) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    let usage = aresp
        .get("usage")
        .cloned()
        .unwrap_or_else(|| json!({"input_tokens": 0, "output_tokens": 0}));
    events.push((
        "message_start".to_string(),
        json!({"type": "message_start", "message": {
            "id": aresp.get("id").and_then(Value::as_str).unwrap_or("msg_proxy"),
            "type": "message",
            "role": "assistant",
            "model": aresp.get("model").cloned().unwrap_or(Value::Null),
            "content": [],
            "stop_reason": null,
            "stop_sequence": null,
            "usage": usage
        }}),
    ));
    events.push(("ping".to_string(), json!({"type": "ping"})));

    let default_blocks = vec![json!({"type": "text", "text": ""})];
    let blocks = aresp
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&default_blocks);
    for (idx, block) in blocks.iter().enumerate() {
        let index = Value::Number(serde_json::Number::from(idx));
        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            events.push((
                "content_block_start".to_string(),
                json!({"type": "content_block_start", "index": index, "content_block": {
                    "type": "tool_use",
                    "id": block.get("id").cloned().unwrap_or(Value::Null),
                    "name": block.get("name").cloned().unwrap_or(Value::Null),
                    "input": {}
                }}),
            ));
            events.push((
                "content_block_delta".to_string(),
                json!({"type": "content_block_delta", "index": idx, "delta": {
                    "type": "input_json_delta",
                    "partial_json": json_dumps_python_style(block.get("input").unwrap_or(&json!({})))
                }}),
            ));
        } else {
            events.push((
                "content_block_start".to_string(),
                json!({"type": "content_block_start", "index": idx, "content_block": {
                    "type": "text",
                    "text": ""
                }}),
            ));
            events.push((
                "content_block_delta".to_string(),
                json!({"type": "content_block_delta", "index": idx, "delta": {
                    "type": "text_delta",
                    "text": block.get("text").and_then(Value::as_str).unwrap_or("")
                }}),
            ));
        }
        events.push((
            "content_block_stop".to_string(),
            json!({"type": "content_block_stop", "index": idx}),
        ));
    }
    events.push((
        "message_delta".to_string(),
        json!({"type": "message_delta", "delta": {
            "stop_reason": aresp.get("stop_reason").and_then(Value::as_str).unwrap_or("end_turn"),
            "stop_sequence": null
        }, "usage": {
            "output_tokens": aresp.get("usage").and_then(|u| u.get("output_tokens")).and_then(Value::as_u64).unwrap_or(0)
        }}),
    ));
    events.push(("message_stop".to_string(), json!({"type": "message_stop"})));
    events
}

#[cfg(test)]
mod tests {
    use super::{
        anthropic_to_openai, anthropic_to_openai_custom, map_tool_choice, openai_to_anthropic,
        replay_as_sse_events,
    };
    use serde_json::{json, Value};

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../test/golden/qwen_openai_chat.json")).unwrap()
    }

    #[test]
    fn qwen_transform_matches_python_fixture() {
        let fx = fixture();
        let got = anthropic_to_openai(&fx["request"], "qwen-turbo").unwrap();
        assert_eq!(got, fx["openai_request"]);
    }

    #[test]
    fn qwen_response_mapping_matches_python_fixture() {
        let fx = fixture();
        let got = openai_to_anthropic(&fx["openai_response"], "claude-haiku-4-5");
        assert_eq!(got, fx["anthropic_response"]);
    }

    #[test]
    fn qwen_tool_choice_contract_matches_python() {
        assert_eq!(
            map_tool_choice(
                Some(&json!({"type": "any"})),
                Some(&json!([{"name": "only"}]))
            ),
            Some(json!({"type": "function", "function": {"name": "only"}}))
        );
        assert_eq!(
            map_tool_choice(
                Some(&json!({"type": "any"})),
                Some(&json!([{"name": "a"}, {"name": "b"}]))
            ),
            Some(json!("required"))
        );
        assert_eq!(
            map_tool_choice(Some(&json!({"type": "none"})), Some(&json!([]))),
            Some(json!("none"))
        );
    }

    #[test]
    fn qwen_token_cap_preserves_resolved_target() {
        let got = anthropic_to_openai(
            &json!({
                "model": "claude-opus-4-8",
                "max_tokens": 100000,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "qwen3.7-max",
        )
        .unwrap();
        assert_eq!(got["model"], "qwen3.7-max");
        assert_eq!(got["max_tokens"], 8192);
    }

    #[test]
    fn custom_openai_forces_model_without_generic_token_clamp() {
        let got = anthropic_to_openai_custom(
            &json!({
                "model": "claude-opus-4-8",
                "max_tokens": 1000000,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "glm-4.5",
        )
        .unwrap();
        assert_eq!(got["model"], "glm-4.5");
        assert_eq!(got["max_tokens"], 1000000);
    }

    #[test]
    fn qwen_sse_replay_events_match_python_sequence_shape() {
        let fx = fixture();
        let events = replay_as_sse_events(&fx["anthropic_response"]);
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "ping",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[3].1["delta"]["text"], "answer");
        assert_eq!(events[5].1["content_block"]["type"], "tool_use");
        assert_eq!(events[6].1["delta"]["type"], "input_json_delta");
        assert_eq!(events[8].1["delta"]["stop_reason"], "tool_use");
        assert_eq!(events[8].1["usage"]["output_tokens"], 8);
    }
}
