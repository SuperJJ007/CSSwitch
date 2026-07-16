use std::collections::{HashMap, HashSet};
use std::fmt;

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

type HmacSha256 = Hmac<Sha256>;

pub const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_REASONING_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SIGNATURE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_NONSTREAM_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_TOOL_CALLS: usize = 256;
pub const MAX_OUTPUT_ITEMS: usize = 1024;

const SIGNATURE_PREFIX: &str = "csswitch.codex-thinking.v1";
const SIGNATURE_PURPOSE: &str = "codex-reasoning";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolErrorKind {
    InvalidRequest,
    InvalidSignature,
    Bounds,
    Upstream,
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolError {
    pub kind: ProtocolErrorKind,
    pub detail: &'static str,
}

impl ProtocolError {
    fn invalid(detail: &'static str) -> Self {
        Self {
            kind: ProtocolErrorKind::InvalidRequest,
            detail,
        }
    }

    fn signature(detail: &'static str) -> Self {
        Self {
            kind: ProtocolErrorKind::InvalidSignature,
            detail,
        }
    }

    fn bounds(detail: &'static str) -> Self {
        Self {
            kind: ProtocolErrorKind::Bounds,
            detail,
        }
    }

    fn upstream(detail: &'static str) -> Self {
        Self {
            kind: ProtocolErrorKind::Upstream,
            detail,
        }
    }

    fn incomplete(detail: &'static str) -> Self {
        Self {
            kind: ProtocolErrorKind::Incomplete,
            detail,
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.detail)
    }
}

impl std::error::Error for ProtocolError {}

pub struct ThinkingSigner {
    key: [u8; 32],
}

impl ThinkingSigner {
    pub fn new(key: &[u8]) -> Result<Self, ProtocolError> {
        let key: [u8; 32] = key
            .try_into()
            .map_err(|_| ProtocolError::signature("thinking key is invalid"))?;
        Ok(Self { key })
    }

    pub fn seal(
        &self,
        auth_epoch: &str,
        account_hash: &str,
        item_id: &str,
        tool_id: Option<&str>,
        encrypted_content: &str,
    ) -> Result<String, ProtocolError> {
        validate_signature_identity(auth_epoch, account_hash, item_id, tool_id)?;
        if encrypted_content.len() > MAX_SIGNATURE_BYTES {
            return Err(ProtocolError::bounds("thinking signature is too large"));
        }
        let digest = hex_digest(encrypted_content.as_bytes());
        let payload = SignaturePayload {
            version: 1,
            purpose: SIGNATURE_PURPOSE.to_string(),
            auth_epoch: auth_epoch.to_string(),
            account_hash: account_hash.to_string(),
            item_id: item_id.to_string(),
            tool_id: tool_id.map(ToString::to_string),
            encrypted_digest: digest,
            encrypted_content: encrypted_content.to_string(),
        };
        let payload_bytes = Zeroizing::new(
            serde_json::to_vec(&payload)
                .map_err(|_| ProtocolError::signature("thinking signature encoding failed"))?,
        );
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&payload_bytes);
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| ProtocolError::signature("thinking signature key failed"))?;
        mac.update(SIGNATURE_PREFIX.as_bytes());
        mac.update(b".");
        mac.update(encoded.as_bytes());
        let tag =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        let signature = format!("{SIGNATURE_PREFIX}.{encoded}.{tag}");
        if signature.len() > MAX_SIGNATURE_BYTES {
            return Err(ProtocolError::bounds("thinking signature is too large"));
        }
        Ok(signature)
    }

    pub fn open(
        &self,
        signature: &str,
        expected_auth_epoch: &str,
        expected_account_hash: &str,
        expected_tool_id: Option<&str>,
    ) -> Result<VerifiedReasoning, ProtocolError> {
        if signature.len() > MAX_SIGNATURE_BYTES {
            return Err(ProtocolError::bounds("thinking signature is too large"));
        }
        let mut pieces = signature.split('.');
        let prefix = pieces.next();
        let product = pieces.next();
        let version = pieces.next();
        let encoded = pieces.next();
        let tag = pieces.next();
        if prefix != Some("csswitch")
            || product != Some("codex-thinking")
            || version != Some("v1")
            || encoded.is_none()
            || tag.is_none()
            || pieces.next().is_some()
        {
            return Err(ProtocolError::signature("thinking signature is invalid"));
        }
        let encoded = encoded.unwrap_or_default();
        let tag = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(tag.unwrap_or_default())
            .map_err(|_| ProtocolError::signature("thinking signature is invalid"))?;
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| ProtocolError::signature("thinking signature key failed"))?;
        mac.update(SIGNATURE_PREFIX.as_bytes());
        mac.update(b".");
        mac.update(encoded.as_bytes());
        mac.verify_slice(&tag)
            .map_err(|_| ProtocolError::signature("thinking signature is invalid"))?;
        let payload_bytes = Zeroizing::new(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| ProtocolError::signature("thinking signature is invalid"))?,
        );
        let mut payload: SignaturePayload = serde_json::from_slice(&payload_bytes)
            .map_err(|_| ProtocolError::signature("thinking signature is invalid"))?;
        if payload.version != 1
            || payload.purpose != SIGNATURE_PURPOSE
            || payload.auth_epoch != expected_auth_epoch
            || payload.account_hash != expected_account_hash
            || payload.tool_id.as_deref() != expected_tool_id
            || payload.encrypted_digest != hex_digest(payload.encrypted_content.as_bytes())
        {
            return Err(ProtocolError::signature(
                "thinking signature context changed",
            ));
        }
        validate_signature_identity(
            &payload.auth_epoch,
            &payload.account_hash,
            &payload.item_id,
            payload.tool_id.as_deref(),
        )?;
        Ok(VerifiedReasoning {
            item_id: std::mem::take(&mut payload.item_id),
            encrypted_content: Zeroizing::new(std::mem::take(&mut payload.encrypted_content)),
        })
    }
}

impl Drop for ThinkingSigner {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

impl fmt::Debug for ThinkingSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThinkingSigner")
            .field("has_key", &true)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignaturePayload {
    version: u32,
    purpose: String,
    auth_epoch: String,
    account_hash: String,
    item_id: String,
    tool_id: Option<String>,
    encrypted_digest: String,
    encrypted_content: String,
}

impl Drop for SignaturePayload {
    fn drop(&mut self) {
        self.auth_epoch.zeroize();
        self.account_hash.zeroize();
        self.item_id.zeroize();
        if let Some(tool_id) = self.tool_id.as_mut() {
            tool_id.zeroize();
        }
        self.encrypted_digest.zeroize();
        self.encrypted_content.zeroize();
    }
}

pub struct VerifiedReasoning {
    pub item_id: String,
    pub encrypted_content: Zeroizing<String>,
}

impl fmt::Debug for VerifiedReasoning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedReasoning")
            .field("item_id", &self.item_id)
            .field("has_encrypted_content", &!self.encrypted_content.is_empty())
            .finish()
    }
}

fn validate_signature_identity(
    auth_epoch: &str,
    account_hash: &str,
    item_id: &str,
    tool_id: Option<&str>,
) -> Result<(), ProtocolError> {
    if auth_epoch.len() != 32
        || account_hash.len() != 32
        || !is_lower_hex(auth_epoch)
        || !is_lower_hex(account_hash)
        || item_id.is_empty()
        || item_id.len() > 512
        || tool_id.is_some_and(|value| value.is_empty() || value.len() > 512)
    {
        return Err(ProtocolError::signature(
            "thinking signature identity is invalid",
        ));
    }
    Ok(())
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub struct RequestContext<'a> {
    pub target_model: &'a str,
    pub auth_epoch: &'a str,
    pub account_hash: &'a str,
    pub reasoning_effort: Option<&'a str>,
    pub supports_reasoning_summary: bool,
    pub supports_parallel_tool_calls: bool,
}

pub fn validate_request_body_size(length: usize) -> Result<(), ProtocolError> {
    if length > MAX_REQUEST_BYTES {
        Err(ProtocolError::bounds("request is too large"))
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct ToolHistory {
    calls: HashSet<String>,
    results: HashSet<String>,
}

pub fn translate_anthropic_request(
    request: &Value,
    context: &RequestContext<'_>,
    signer: &ThinkingSigner,
) -> Result<Value, ProtocolError> {
    if context.target_model.trim().is_empty() || context.target_model.len() > 256 {
        return Err(ProtocolError::invalid("model is invalid"));
    }
    validate_signature_identity(context.auth_epoch, context.account_hash, "request", None)?;
    let object = request
        .as_object()
        .ok_or_else(|| ProtocolError::invalid("request must be an object"))?;
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::invalid("messages must be an array"))?;
    let mut input = Vec::new();
    let mut text_bytes = 0_usize;
    let mut reasoning_bytes = 0_usize;
    let mut tool_history = ToolHistory::default();
    for message in messages {
        translate_message(
            message,
            context,
            signer,
            &mut input,
            &mut text_bytes,
            &mut reasoning_bytes,
            &mut tool_history,
        )?;
    }
    let tools = translate_tools(object.get("tools"), &mut text_bytes)?;
    let has_tools = !tools.is_empty();
    let tool_choice = translate_tool_choice(object.get("tool_choice"), &tools)?;
    let instructions = translate_system(object.get("system"), &mut text_bytes)?;
    let reasoning_disabled = object
        .get("thinking")
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        == Some("disabled");
    let reasoning = if reasoning_disabled {
        Some(Value::Null)
    } else {
        let mut policy = serde_json::Map::new();
        if let Some(effort) = context.reasoning_effort {
            policy.insert("effort".into(), Value::String(effort.to_string()));
        }
        if context.supports_reasoning_summary {
            policy.insert("summary".into(), Value::String("auto".into()));
        }
        (!policy.is_empty()).then_some(Value::Object(policy))
    };
    let parallel_tool_calls =
        context.supports_parallel_tool_calls && tool_choice.allow_parallel && has_tools;
    let mut translated = json!({
        "model": context.target_model,
        "instructions": instructions,
        "input": input,
        "tools": Value::Array(tools),
        "tool_choice": tool_choice.value,
        "parallel_tool_calls": parallel_tool_calls,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
    });
    if !has_tools {
        translated
            .as_object_mut()
            .expect("translated request is an object")
            .remove("tools");
        translated
            .as_object_mut()
            .expect("translated request is an object")
            .remove("parallel_tool_calls");
    }
    if let Some(reasoning) = reasoning {
        translated
            .as_object_mut()
            .expect("translated request is an object")
            .insert("reasoning".into(), reasoning);
    }
    if instructions.is_empty() {
        translated
            .as_object_mut()
            .expect("translated request is an object")
            .remove("instructions");
    }
    let encoded_len = serde_json::to_vec(&translated)
        .map_err(|_| ProtocolError::invalid("request encoding failed"))?
        .len();
    if encoded_len > MAX_REQUEST_BYTES {
        return Err(ProtocolError::bounds("request is too large"));
    }
    Ok(translated)
}

fn translate_message(
    message: &Value,
    context: &RequestContext<'_>,
    signer: &ThinkingSigner,
    input: &mut Vec<Value>,
    text_bytes: &mut usize,
    reasoning_bytes: &mut usize,
    tool_history: &mut ToolHistory,
) -> Result<(), ProtocolError> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::invalid("message role is missing"))?;
    if !matches!(role, "user" | "assistant") {
        return Err(ProtocolError::invalid("message role is unsupported"));
    }
    let content = message
        .get("content")
        .ok_or_else(|| ProtocolError::invalid("message content is missing"))?;
    if let Some(text) = content.as_str() {
        add_bounded(text_bytes, text.len(), MAX_TEXT_BYTES, "text is too large")?;
        input.push(message_item(role, text));
        return Ok(());
    }
    let blocks = content
        .as_array()
        .ok_or_else(|| ProtocolError::invalid("message content is invalid"))?;
    let mut ordinary = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| ProtocolError::invalid("content block type is missing"))?;
        match block_type {
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                add_bounded(text_bytes, text.len(), MAX_TEXT_BYTES, "text is too large")?;
                ordinary.push(if role == "assistant" {
                    json!({"type": "output_text", "text": text})
                } else {
                    json!({"type": "input_text", "text": text})
                });
            }
            "image" if role == "user" => {
                let source = block
                    .get("source")
                    .and_then(Value::as_object)
                    .ok_or_else(|| ProtocolError::invalid("image source is invalid"))?;
                if source.get("type").and_then(Value::as_str) != Some("base64") {
                    return Err(ProtocolError::invalid("image source type is unsupported"));
                }
                let media_type = source
                    .get("media_type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProtocolError::invalid("image media type is missing"))?;
                let data = source
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProtocolError::invalid("image data is missing"))?;
                if !matches!(
                    media_type,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) {
                    return Err(ProtocolError::invalid("image media type is unsupported"));
                }
                ordinary.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{data}"),
                    "detail": "high",
                }));
            }
            "tool_use" if role == "assistant" => {
                flush_message(role, &mut ordinary, input);
                let id = required_short_string(block, "id", "tool id is invalid")?;
                let name = required_short_string(block, "name", "tool name is invalid")?;
                if !tool_history.calls.insert(id.to_string()) {
                    return Err(ProtocolError::invalid("tool id is duplicated"));
                }
                let empty_input = Value::Object(Map::new());
                let tool_input = block.get("input").unwrap_or(&empty_input);
                if !tool_input.is_object() {
                    return Err(ProtocolError::invalid("tool arguments must be an object"));
                }
                let arguments = serde_json::to_string(tool_input)
                    .map_err(|_| ProtocolError::invalid("tool arguments are invalid"))?;
                if arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
                    return Err(ProtocolError::bounds("tool arguments are too large"));
                }
                input.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
            "tool_result" if role == "user" => {
                flush_message(role, &mut ordinary, input);
                let id = required_short_string(block, "tool_use_id", "tool result id is invalid")?;
                if !tool_history.calls.contains(id) {
                    return Err(ProtocolError::invalid("tool result has no matching call"));
                }
                if !tool_history.results.insert(id.to_string()) {
                    return Err(ProtocolError::invalid("tool result is duplicated"));
                }
                let output = tool_result_output(block.get("content"), text_bytes)?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": id,
                    "output": output,
                }));
            }
            "thinking" if role == "assistant" => {
                flush_message(role, &mut ordinary, input);
                let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                add_bounded(
                    reasoning_bytes,
                    thinking.len(),
                    MAX_REASONING_BYTES,
                    "reasoning is too large",
                )?;
                let signature = block
                    .get("signature")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProtocolError::signature("thinking signature is missing"))?;
                let bound_tool_id = blocks.get(index + 1).and_then(|candidate| {
                    (candidate.get("type").and_then(Value::as_str) == Some("tool_use"))
                        .then(|| candidate.get("id").and_then(Value::as_str))
                        .flatten()
                });
                let verified = signer.open(
                    signature,
                    context.auth_epoch,
                    context.account_hash,
                    bound_tool_id,
                )?;
                add_bounded(
                    reasoning_bytes,
                    verified.encrypted_content.len(),
                    MAX_REASONING_BYTES,
                    "encrypted reasoning is too large",
                )?;
                input.push(json!({
                    "type": "reasoning",
                    "id": verified.item_id,
                    "summary": [{"type": "summary_text", "text": thinking}],
                    "content": Value::Null,
                    "encrypted_content": verified.encrypted_content.as_str(),
                }));
            }
            _ => return Err(ProtocolError::invalid("content block is unsupported")),
        }
    }
    flush_message(role, &mut ordinary, input);
    Ok(())
}

fn message_item(role: &str, text: &str) -> Value {
    let kind = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]})
}

fn flush_message(role: &str, content: &mut Vec<Value>, input: &mut Vec<Value>) {
    if !content.is_empty() {
        input.push(json!({"type": "message", "role": role, "content": std::mem::take(content)}));
    }
}

fn required_short_string<'a>(
    object: &'a Value,
    field: &str,
    detail: &'static str,
) -> Result<&'a str, ProtocolError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .ok_or_else(|| ProtocolError::invalid(detail))
}

fn tool_result_output(
    content: Option<&Value>,
    text_bytes: &mut usize,
) -> Result<Value, ProtocolError> {
    match content {
        Some(Value::String(text)) => {
            add_bounded(text_bytes, text.len(), MAX_TEXT_BYTES, "text is too large")?;
            Ok(Value::String(text.clone()))
        }
        Some(Value::Array(blocks)) => {
            let mut output = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        add_bounded(text_bytes, text.len(), MAX_TEXT_BYTES, "text is too large")?;
                        output.push(json!({"type": "input_text", "text": text}));
                    }
                    Some("image") => {
                        let source =
                            block
                                .get("source")
                                .and_then(Value::as_object)
                                .ok_or_else(|| {
                                    ProtocolError::invalid("tool result image source is invalid")
                                })?;
                        let media_type = source
                            .get("media_type")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                ProtocolError::invalid("tool result image media type is missing")
                            })?;
                        let data = source.get("data").and_then(Value::as_str).ok_or_else(|| {
                            ProtocolError::invalid("tool result image data is missing")
                        })?;
                        if source.get("type").and_then(Value::as_str) != Some("base64") {
                            return Err(ProtocolError::invalid(
                                "tool result image source is unsupported",
                            ));
                        }
                        if !matches!(
                            media_type,
                            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                        ) {
                            return Err(ProtocolError::invalid(
                                "tool result image media type is unsupported",
                            ));
                        }
                        output.push(json!({
                            "type": "input_image",
                            "image_url": format!("data:{media_type};base64,{data}"),
                        }));
                    }
                    _ => return Err(ProtocolError::invalid("tool result block is unsupported")),
                }
            }
            Ok(Value::Array(output))
        }
        Some(other) => {
            let text = serde_json::to_string(other)
                .map_err(|_| ProtocolError::invalid("tool result is invalid"))?;
            add_bounded(text_bytes, text.len(), MAX_TEXT_BYTES, "text is too large")?;
            Ok(Value::String(text))
        }
        None => Ok(Value::String(String::new())),
    }
}

fn translate_tools(
    tools: Option<&Value>,
    text_bytes: &mut usize,
) -> Result<Vec<Value>, ProtocolError> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| ProtocolError::invalid("tools must be an array"))?;
    if tools.len() > MAX_TOOL_CALLS {
        return Err(ProtocolError::bounds("too many tools"));
    }
    tools
        .iter()
        .map(|tool| {
            let name = required_short_string(tool, "name", "tool name is invalid")?;
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            add_bounded(
                text_bytes,
                description.len(),
                MAX_TEXT_BYTES,
                "text is too large",
            )?;
            let parameters = tool
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Ok(json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            }))
        })
        .collect()
}

struct ToolChoiceTranslation {
    value: Value,
    allow_parallel: bool,
}

fn translate_tool_choice(
    choice: Option<&Value>,
    tools: &[Value],
) -> Result<ToolChoiceTranslation, ProtocolError> {
    let has_tools = !tools.is_empty();
    let kind = choice
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("type").and_then(Value::as_str))
        })
        .unwrap_or("auto");
    let disable_parallel = match choice.and_then(|value| value.get("disable_parallel_tool_use")) {
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return Err(ProtocolError::invalid(
                "tool choice parallel policy is invalid",
            ))
        }
        None => false,
    };
    let value = match kind {
        "auto" if has_tools => Value::String("auto".into()),
        "auto" | "none" => Value::String("none".into()),
        "any" if has_tools => Value::String("required".into()),
        "any" => return Err(ProtocolError::invalid("required tool choice has no tools")),
        "tool" if has_tools => {
            let object = choice
                .and_then(Value::as_object)
                .ok_or_else(|| ProtocolError::invalid("forced tool choice is invalid"))?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && name.len() <= 512)
                .ok_or_else(|| ProtocolError::invalid("forced tool name is invalid"))?;
            if !tools
                .iter()
                .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            {
                return Err(ProtocolError::invalid("forced tool is not declared"));
            }
            json!({"type": "function", "name": name})
        }
        "tool" => return Err(ProtocolError::invalid("forced tool choice has no tools")),
        _ => return Err(ProtocolError::invalid("tool choice is invalid")),
    };
    Ok(ToolChoiceTranslation {
        value,
        allow_parallel: !disable_parallel,
    })
}

fn translate_system(
    system: Option<&Value>,
    text_bytes: &mut usize,
) -> Result<String, ProtocolError> {
    let mut parts = Vec::new();
    match system {
        None => {}
        Some(Value::String(text)) => parts.push(text.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(ProtocolError::invalid("system block is unsupported"));
                }
                parts.push(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                );
            }
        }
        Some(_) => return Err(ProtocolError::invalid("system is invalid")),
    }
    let instructions = parts.join("\n");
    add_bounded(
        text_bytes,
        instructions.len(),
        MAX_TEXT_BYTES,
        "text is too large",
    )?;
    Ok(instructions)
}

fn add_bounded(
    total: &mut usize,
    amount: usize,
    limit: usize,
    detail: &'static str,
) -> Result<(), ProtocolError> {
    *total = total
        .checked_add(amount)
        .ok_or_else(|| ProtocolError::bounds(detail))?;
    if *total > limit {
        return Err(ProtocolError::bounds(detail));
    }
    Ok(())
}

pub struct SseDecoder {
    pending: Vec<u8>,
    event: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            event: Vec::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Value>, ProtocolError> {
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let (event, consumed) = self.feed_next(&bytes[offset..])?;
            offset += consumed;
            if let Some(event) = event {
                out.push(event);
            }
        }
        Ok(out)
    }

    pub fn feed_next(&mut self, bytes: &[u8]) -> Result<(Option<Value>, usize), ProtocolError> {
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' {
                let mut line = std::mem::take(&mut self.pending);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if let Some(event) = self.process_line(&line)? {
                    return Ok((Some(event), index + 1));
                }
            } else {
                self.pending.push(*byte);
            }
            if self.pending.len().saturating_add(self.event.len()) > MAX_EVENT_BYTES {
                return Err(ProtocolError::bounds("upstream event is too large"));
            }
        }
        Ok((None, bytes.len()))
    }

    pub fn finish(&mut self) -> Result<Vec<Value>, ProtocolError> {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            if let Some(event) = self.process_line(&line)? {
                return Ok(vec![event]);
            }
        }
        if self.event.is_empty() {
            return Ok(Vec::new());
        }
        let event = parse_sse_event(&self.event)?;
        self.event.clear();
        Ok(vec![event])
    }

    fn process_line(&mut self, line: &[u8]) -> Result<Option<Value>, ProtocolError> {
        if line.is_empty() {
            if self.event.is_empty() {
                return Ok(None);
            }
            let event = parse_sse_event(&self.event)?;
            self.event.clear();
            return Ok(Some(event));
        }
        if line.starts_with(b":") {
            return Ok(None);
        }
        if self
            .event
            .len()
            .saturating_add(line.len())
            .saturating_add(1)
            > MAX_EVENT_BYTES
        {
            return Err(ProtocolError::bounds("upstream event is too large"));
        }
        self.event.extend_from_slice(line);
        self.event.push(b'\n');
        Ok(None)
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_sse_event(raw: &[u8]) -> Result<Value, ProtocolError> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| ProtocolError::upstream("upstream SSE is not UTF-8"))?;
    let mut data = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if data.is_empty() {
        return Err(ProtocolError::upstream("upstream SSE event has no data"));
    }
    serde_json::from_str(&data.join("\n"))
        .map_err(|_| ProtocolError::upstream("upstream SSE JSON is invalid"))
}

pub struct AnthropicEvent {
    pub event: &'static str,
    pub data: Value,
}

impl fmt::Debug for AnthropicEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicEvent")
            .field("event", &self.event)
            .finish_non_exhaustive()
    }
}

struct ToolState {
    index: Option<usize>,
    call_id: String,
    name: String,
    arguments: String,
    closed: bool,
}

struct TextState {
    index: usize,
    text: String,
    closed: bool,
}

struct ReasoningState {
    index: usize,
    text: String,
    encrypted_content: Option<Zeroizing<String>>,
    signature: Option<String>,
    done: bool,
    closed: bool,
}

enum BlockKey {
    Reasoning(String),
    Text(String),
    Tool(String),
}

#[derive(PartialEq, Eq)]
enum ActiveBlock {
    Reasoning(String),
    Text(String),
    Tool(String),
}

pub struct ResponsesReducer<'a> {
    model: String,
    auth_epoch: &'a str,
    account_hash: &'a str,
    signer: &'a ThinkingSigner,
    message_id: String,
    created: bool,
    started: bool,
    completed: bool,
    failed: bool,
    next_index: usize,
    texts: HashMap<String, TextState>,
    reasoning: HashMap<String, ReasoningState>,
    tools: HashMap<String, ToolState>,
    call_ids: HashMap<String, String>,
    block_order: Vec<BlockKey>,
    active_block: Option<ActiveBlock>,
    pending_reasoning: Option<String>,
    output_items: HashMap<String, String>,
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: &'static str,
    total_text_bytes: usize,
    total_reasoning_bytes: usize,
    stored_bytes: usize,
    estimated_nonstream_bytes: usize,
}

impl<'a> ResponsesReducer<'a> {
    pub fn new(
        model: impl Into<String>,
        auth_epoch: &'a str,
        account_hash: &'a str,
        signer: &'a ThinkingSigner,
    ) -> Self {
        Self {
            model: model.into(),
            auth_epoch,
            account_hash,
            signer,
            message_id: "msg_codex".into(),
            created: false,
            started: false,
            completed: false,
            failed: false,
            next_index: 0,
            texts: HashMap::new(),
            reasoning: HashMap::new(),
            tools: HashMap::new(),
            call_ids: HashMap::new(),
            block_order: Vec::new(),
            active_block: None,
            pending_reasoning: None,
            output_items: HashMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            stop_reason: "end_turn",
            total_text_bytes: 0,
            total_reasoning_bytes: 0,
            stored_bytes: 0,
            estimated_nonstream_bytes: 512,
        }
    }

    pub fn apply(&mut self, event: Value) -> Result<Vec<AnthropicEvent>, ProtocolError> {
        if self.completed || self.failed {
            return Err(ProtocolError::upstream(
                "event arrived after terminal response",
            ));
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| ProtocolError::upstream("upstream event type is missing"))?;
        if kind != "response.created" && !self.created {
            return Err(ProtocolError::upstream(
                "response.created must be the first event",
            ));
        }
        let mut out = Vec::new();
        match kind {
            "response.created" => {
                if self.created || self.started {
                    return Err(ProtocolError::upstream(
                        "response.created arrived more than once",
                    ));
                }
                self.created = true;
                if let Some(id) = event
                    .get("response")
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
                {
                    if id.is_empty() || id.len() > 512 {
                        return Err(ProtocolError::upstream("response id is invalid"));
                    }
                    self.message_id = id.to_string();
                }
                self.read_usage(event.get("response"));
                self.ensure_started(&mut out);
            }
            "response.output_text.delta" => {
                self.ensure_started(&mut out);
                let item_id = required_event_id(&event)?;
                self.claim_output_item(item_id, "message")?;
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                self.append_text(item_id, delta, &mut out)?;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.ensure_started(&mut out);
                let item_id = required_event_id(&event)?;
                self.claim_output_item(item_id, "reasoning")?;
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                self.append_reasoning(item_id, delta, &mut out)?;
            }
            "response.output_item.added" => {
                self.ensure_started(&mut out);
                let item = event
                    .get("item")
                    .ok_or_else(|| ProtocolError::upstream("output item is missing"))?;
                self.note_output_item(item)?;
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    self.register_tool(item, &mut out)?;
                }
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                self.ensure_started(&mut out);
                let item_id = required_event_id(&event)?;
                self.claim_output_item(item_id, "function_call")?;
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                self.append_tool(
                    item_id,
                    event.get("call_id").and_then(Value::as_str),
                    delta,
                    &mut out,
                )?;
            }
            "response.output_item.done" => {
                self.ensure_started(&mut out);
                let item = event
                    .get("item")
                    .ok_or_else(|| ProtocolError::upstream("output item is missing"))?;
                self.note_output_item(item)?;
                match item.get("type").and_then(Value::as_str) {
                    Some("reasoning") => self.finish_reasoning(item, &mut out)?,
                    Some("function_call") => self.finish_tool(item, &mut out)?,
                    Some("message") => self.finish_message_item(item, &mut out)?,
                    _ => {}
                }
            }
            "response.completed" => {
                self.finish_terminal(event.get("response"), "end_turn", &mut out)?;
            }
            "response.incomplete" => {
                let response = event.get("response");
                let reason = response
                    .and_then(|value| value.get("incomplete_details"))
                    .and_then(|value| value.get("reason"))
                    .and_then(Value::as_str);
                if reason == Some("max_output_tokens") {
                    self.finish_terminal(response, "max_tokens", &mut out)?;
                } else {
                    self.failed = true;
                    return Err(ProtocolError::upstream("upstream response was incomplete"));
                }
            }
            "response.failed" => {
                self.failed = true;
                return Err(ProtocolError::upstream("upstream response failed"));
            }
            "error" => {
                self.failed = true;
                return Err(ProtocolError::upstream("upstream stream error"));
            }
            "response.reasoning_summary_text.done"
            | "response.reasoning_summary_part.added"
            | "response.output_text.done" => {}
            _ => {}
        }
        Ok(out)
    }

    pub fn finish_stream(&self) -> Result<(), ProtocolError> {
        if self.completed {
            Ok(())
        } else {
            Err(ProtocolError::incomplete(
                "upstream stream ended before completion",
            ))
        }
    }

    pub fn is_complete(&self) -> bool {
        self.completed
    }

    pub fn nonstream_response(mut self) -> Result<Value, ProtocolError> {
        self.finish_stream()?;
        if self.estimated_nonstream_bytes > MAX_NONSTREAM_BYTES {
            return Err(ProtocolError::bounds("nonstream response is too large"));
        }
        let mut content = Vec::with_capacity(self.block_order.len());
        for block in std::mem::take(&mut self.block_order) {
            match block {
                BlockKey::Reasoning(key) => {
                    let mut state = self
                        .reasoning
                        .remove(&key)
                        .ok_or_else(|| ProtocolError::upstream("reasoning state is missing"))?;
                    let signature = state
                        .signature
                        .take()
                        .ok_or_else(|| ProtocolError::upstream("reasoning signature is missing"))?;
                    content.push(json!({
                        "type": "thinking",
                        "thinking": std::mem::take(&mut state.text),
                        "signature": signature,
                    }));
                }
                BlockKey::Text(key) => {
                    let mut state = self
                        .texts
                        .remove(&key)
                        .ok_or_else(|| ProtocolError::upstream("text state is missing"))?;
                    content.push(json!({"type": "text", "text": std::mem::take(&mut state.text)}));
                }
                BlockKey::Tool(key) => {
                    let mut tool = self
                        .tools
                        .remove(&key)
                        .ok_or_else(|| ProtocolError::upstream("tool state is missing"))?;
                    let parsed = serde_json::from_str::<Value>(&tool.arguments)
                        .map_err(|_| ProtocolError::upstream("tool arguments are invalid"))?;
                    content.push(json!({
                        "type": "tool_use",
                        "id": std::mem::take(&mut tool.call_id),
                        "name": std::mem::take(&mut tool.name),
                        "input": parsed,
                    }));
                }
            }
        }
        let response = json!({
            "id": self.message_id,
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": content,
            "stop_reason": self.stop_reason,
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": self.input_tokens, "output_tokens": self.output_tokens},
        });
        if serde_json::to_vec(&response)
            .map_err(|_| ProtocolError::upstream("response encoding failed"))?
            .len()
            > MAX_NONSTREAM_BYTES
        {
            return Err(ProtocolError::bounds("nonstream response is too large"));
        }
        Ok(response)
    }

    fn ensure_started(&mut self, out: &mut Vec<AnthropicEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(AnthropicEvent {
            event: "message_start",
            data: json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": self.input_tokens, "output_tokens": 0},
                },
            }),
        });
    }

    fn append_text(
        &mut self,
        item_id: &str,
        delta: &str,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        self.close_pending_reasoning(None, out)?;
        if !self.texts.contains_key(item_id) {
            self.require_no_active_block()?;
            self.reserve_identity(item_id, 160)?;
            let index = self.allocate_index();
            self.texts.insert(
                item_id.to_string(),
                TextState {
                    index,
                    text: String::new(),
                    closed: false,
                },
            );
            self.block_order.push(BlockKey::Text(item_id.to_string()));
            self.active_block = Some(ActiveBlock::Text(item_id.to_string()));
            out.push(AnthropicEvent {
                event: "content_block_start",
                data: json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
            });
        }
        if self.active_block != Some(ActiveBlock::Text(item_id.to_string())) {
            return Err(ProtocolError::upstream("text blocks overlap"));
        }
        if self.texts.get(item_id).is_none_or(|state| state.closed) {
            return Err(ProtocolError::upstream(
                "text delta arrived after item completed",
            ));
        }
        self.reserve_text(delta)?;
        let state = self.texts.get_mut(item_id).expect("text state inserted");
        state.text.push_str(delta);
        if !delta.is_empty() {
            out.push(AnthropicEvent {
                event: "content_block_delta",
                data: json!({"type": "content_block_delta", "index": state.index, "delta": {"type": "text_delta", "text": delta}}),
            });
        }
        Ok(())
    }

    fn finish_text(
        &mut self,
        item_id: &str,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        let state = self
            .texts
            .get_mut(item_id)
            .ok_or_else(|| ProtocolError::upstream("text state is missing"))?;
        if state.closed || self.active_block != Some(ActiveBlock::Text(item_id.to_string())) {
            return Err(ProtocolError::upstream("text item lifecycle is invalid"));
        }
        out.push(AnthropicEvent {
            event: "content_block_stop",
            data: json!({"type": "content_block_stop", "index": state.index}),
        });
        state.closed = true;
        self.active_block = None;
        Ok(())
    }

    fn append_reasoning(
        &mut self,
        item_id: &str,
        delta: &str,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        if self
            .pending_reasoning
            .as_deref()
            .is_some_and(|key| key != item_id)
        {
            self.close_pending_reasoning(None, out)?;
        }
        if !self.reasoning.contains_key(item_id) {
            self.require_no_active_block()?;
            self.reserve_identity(item_id, 192)?;
            let index = self.allocate_index();
            self.reasoning.insert(
                item_id.to_string(),
                ReasoningState {
                    index,
                    text: String::new(),
                    encrypted_content: None,
                    signature: None,
                    done: false,
                    closed: false,
                },
            );
            self.block_order
                .push(BlockKey::Reasoning(item_id.to_string()));
            self.active_block = Some(ActiveBlock::Reasoning(item_id.to_string()));
            out.push(AnthropicEvent {
                event: "content_block_start",
                data: json!({"type": "content_block_start", "index": index, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            });
        }
        if self.active_block != Some(ActiveBlock::Reasoning(item_id.to_string())) {
            return Err(ProtocolError::upstream("reasoning blocks overlap"));
        }
        let state = self
            .reasoning
            .get(item_id)
            .expect("reasoning state inserted");
        if state.done || state.closed {
            return Err(ProtocolError::upstream(
                "reasoning delta arrived after item completed",
            ));
        }
        self.reserve_reasoning(delta)?;
        let state = self
            .reasoning
            .get_mut(item_id)
            .expect("reasoning state inserted");
        state.text.push_str(delta);
        if !delta.is_empty() {
            out.push(AnthropicEvent {
                event: "content_block_delta",
                data: json!({"type": "content_block_delta", "index": state.index, "delta": {"type": "thinking_delta", "thinking": delta}}),
            });
        }
        Ok(())
    }

    fn register_tool(
        &mut self,
        item: &Value,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        let item_id = item_id(item)?;
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or(item_id);
        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
        if call_id.is_empty() || name.is_empty() || call_id.len() > 512 || name.len() > 512 {
            return Err(ProtocolError::upstream("tool identity is invalid"));
        }
        if let Some(state) = self.tools.get(item_id) {
            if state.call_id != call_id || state.name != name || state.closed {
                return Err(ProtocolError::upstream("tool identity changed"));
            }
            return Ok(());
        }
        if self.tools.len() >= MAX_TOOL_CALLS {
            return Err(ProtocolError::bounds("too many tool calls"));
        }
        if self
            .call_ids
            .get(call_id)
            .is_some_and(|claimed| claimed != item_id)
        {
            return Err(ProtocolError::upstream("tool call id is duplicated"));
        }
        self.close_pending_reasoning(Some(call_id), out)?;
        self.reserve_identity(item_id, 192)?;
        self.reserve_identity(call_id, 0)?;
        self.reserve_identity(name, 0)?;
        self.call_ids
            .insert(call_id.to_string(), item_id.to_string());
        self.tools.insert(
            item_id.to_string(),
            ToolState {
                index: None,
                call_id: call_id.to_string(),
                name: name.to_string(),
                arguments: String::new(),
                closed: false,
            },
        );
        Ok(())
    }

    fn start_registered_tool(
        &mut self,
        item_id: &str,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        if self
            .tools
            .get(item_id)
            .and_then(|state| state.index)
            .is_some()
        {
            if self.active_block != Some(ActiveBlock::Tool(item_id.to_string())) {
                return Err(ProtocolError::upstream("tool blocks overlap"));
            }
            return Ok(());
        }
        self.require_no_active_block()?;
        let index = self.allocate_index();
        let state = self
            .tools
            .get_mut(item_id)
            .ok_or_else(|| ProtocolError::upstream("tool item is missing"))?;
        state.index = Some(index);
        let call_id = state.call_id.clone();
        let name = state.name.clone();
        self.block_order.push(BlockKey::Tool(item_id.to_string()));
        self.active_block = Some(ActiveBlock::Tool(item_id.to_string()));
        out.push(AnthropicEvent {
            event: "content_block_start",
            data: json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": call_id, "name": name, "input": {}},
            }),
        });
        Ok(())
    }

    fn append_tool(
        &mut self,
        item_id: &str,
        call_id: Option<&str>,
        delta: &str,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        let state = self.tools.get(item_id).ok_or_else(|| {
            ProtocolError::upstream("tool argument delta arrived before tool item")
        })?;
        if call_id.is_some_and(|value| value != state.call_id) {
            return Err(ProtocolError::upstream("tool call id changed"));
        }
        if state.closed {
            return Err(ProtocolError::upstream(
                "tool delta arrived after item completed",
            ));
        }
        self.start_registered_tool(item_id, out)?;
        let current_len = self
            .tools
            .get(item_id)
            .map(|state| state.arguments.len())
            .unwrap_or(0);
        if current_len.saturating_add(delta.len()) > MAX_TOOL_ARGUMENT_BYTES {
            return Err(ProtocolError::bounds("tool arguments are too large"));
        }
        self.reserve_payload(delta.len(), delta.len())?;
        let state = self.tools.get_mut(item_id).expect("tool state registered");
        state.arguments.push_str(delta);
        if !delta.is_empty() {
            out.push(AnthropicEvent {
                event: "content_block_delta",
                data: json!({"type": "content_block_delta", "index": state.index.expect("tool started"), "delta": {"type": "input_json_delta", "partial_json": delta}}),
            });
        }
        Ok(())
    }

    fn finish_reasoning(
        &mut self,
        item: &Value,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        let item_id = item_id(item)?;
        let summary = item
            .get("summary")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        if !self.reasoning.contains_key(item_id) {
            self.append_reasoning(item_id, &summary, out)?;
        } else {
            let current = self
                .reasoning
                .get(item_id)
                .map(|state| state.text.as_str())
                .unwrap_or("");
            if current.is_empty() && !summary.is_empty() {
                self.append_reasoning(item_id, &summary, out)?;
            } else if !summary.is_empty() && current != summary {
                return Err(ProtocolError::upstream(
                    "reasoning deltas do not match completed item",
                ));
            }
        }
        let encrypted = item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .ok_or_else(|| ProtocolError::upstream("encrypted reasoning is missing"))?;
        let state = self
            .reasoning
            .get(item_id)
            .ok_or_else(|| ProtocolError::upstream("reasoning state is missing"))?;
        if state.done || state.closed {
            return Err(ProtocolError::upstream("reasoning item completed twice"));
        }
        self.reserve_reasoning_opaque(encrypted)?;
        let state = self
            .reasoning
            .get_mut(item_id)
            .expect("reasoning state inserted");
        state.encrypted_content = Some(Zeroizing::new(encrypted.to_string()));
        state.done = true;
        self.pending_reasoning = Some(item_id.to_string());
        Ok(())
    }

    fn close_pending_reasoning(
        &mut self,
        tool_id: Option<&str>,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        let Some(item_id) = self.pending_reasoning.take() else {
            return Ok(());
        };
        if self.active_block != Some(ActiveBlock::Reasoning(item_id.clone())) {
            return Err(ProtocolError::upstream(
                "reasoning item lifecycle is invalid",
            ));
        }
        let encrypted = self
            .reasoning
            .get_mut(&item_id)
            .and_then(|state| state.encrypted_content.take())
            .ok_or_else(|| ProtocolError::upstream("encrypted reasoning is missing"))?;
        let signature = self.signer.seal(
            self.auth_epoch,
            self.account_hash,
            &item_id,
            tool_id,
            &encrypted,
        )?;
        self.release_stored(encrypted.len());
        self.reserve_payload(signature.len(), escaped_json_len(&signature))?;
        let state = self
            .reasoning
            .get_mut(&item_id)
            .ok_or_else(|| ProtocolError::upstream("reasoning state is missing"))?;
        out.push(AnthropicEvent {
            event: "content_block_delta",
            data: json!({"type": "content_block_delta", "index": state.index, "delta": {"type": "signature_delta", "signature": signature.clone()}}),
        });
        out.push(AnthropicEvent {
            event: "content_block_stop",
            data: json!({"type": "content_block_stop", "index": state.index}),
        });
        state.signature = Some(signature);
        state.closed = true;
        self.active_block = None;
        Ok(())
    }

    fn finish_tool(
        &mut self,
        item: &Value,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        let item_id = item_id(item)?;
        self.register_tool(item, out)?;
        self.start_registered_tool(item_id, out)?;
        let complete = item.get("arguments").and_then(Value::as_str).unwrap_or("");
        let current = self
            .tools
            .get(item_id)
            .map(|state| state.arguments.as_str())
            .unwrap_or("");
        if current.is_empty() && !complete.is_empty() {
            self.append_tool(
                item_id,
                item.get("call_id").and_then(Value::as_str),
                complete,
                out,
            )?;
        } else if !complete.is_empty() && current != complete {
            return Err(ProtocolError::upstream(
                "tool argument deltas do not match completed item",
            ));
        }
        if self
            .tools
            .get(item_id)
            .is_some_and(|state| state.arguments.is_empty())
        {
            self.reserve_payload(2, 2)?;
            self.tools
                .get_mut(item_id)
                .expect("tool registered")
                .arguments
                .push_str("{}");
        }
        let state = self.tools.get_mut(item_id).expect("tool state inserted");
        if !serde_json::from_str::<Value>(&state.arguments)
            .map_err(|_| ProtocolError::upstream("tool arguments are invalid"))?
            .is_object()
        {
            return Err(ProtocolError::upstream("tool arguments must be an object"));
        }
        if state.closed || self.active_block != Some(ActiveBlock::Tool(item_id.to_string())) {
            return Err(ProtocolError::upstream("tool item lifecycle is invalid"));
        }
        out.push(AnthropicEvent {
            event: "content_block_stop",
            data: json!({"type": "content_block_stop", "index": state.index.expect("tool started")}),
        });
        state.closed = true;
        self.active_block = None;
        Ok(())
    }

    fn finish_message_item(
        &mut self,
        item: &Value,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        let item_id = item_id(item)?;
        let complete = item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        if !self.texts.contains_key(item_id) {
            self.append_text(item_id, &complete, out)?;
        } else {
            let current = self
                .texts
                .get(item_id)
                .map(|state| state.text.as_str())
                .unwrap_or("");
            if current != complete {
                return Err(ProtocolError::upstream(
                    "text deltas do not match completed item",
                ));
            }
        }
        self.finish_text(item_id, out)
    }

    fn finish_terminal(
        &mut self,
        response: Option<&Value>,
        default_reason: &'static str,
        out: &mut Vec<AnthropicEvent>,
    ) -> Result<(), ProtocolError> {
        self.ensure_started(out);
        self.close_pending_reasoning(None, out)?;
        if self.active_block.is_some()
            || self.texts.values().any(|state| !state.closed)
            || self.reasoning.values().any(|state| !state.closed)
            || self.tools.values().any(|state| !state.closed)
        {
            return Err(ProtocolError::upstream(
                "upstream completed with an open output item",
            ));
        }
        if self.block_order.is_empty() {
            let id = "empty-output";
            self.claim_output_item(id, "message")?;
            self.append_text(id, "", out)?;
            self.finish_text(id, out)?;
        }
        self.read_usage(response);
        self.completed = true;
        self.stop_reason = if default_reason == "max_tokens" {
            "max_tokens"
        } else if self.tools.is_empty() {
            "end_turn"
        } else {
            "tool_use"
        };
        out.push(AnthropicEvent {
            event: "message_delta",
            data: json!({
                "type": "message_delta",
                "delta": {"stop_reason": self.stop_reason, "stop_sequence": Value::Null},
                "usage": {"input_tokens": self.input_tokens, "output_tokens": self.output_tokens},
            }),
        });
        out.push(AnthropicEvent {
            event: "message_stop",
            data: json!({"type": "message_stop"}),
        });
        Ok(())
    }

    fn note_output_item(&mut self, item: &Value) -> Result<(), ProtocolError> {
        let id = item
            .get("id")
            .or_else(|| item.get("call_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                serde_json::to_vec(item)
                    .map(|bytes| format!("anonymous:{}", hex_digest(&bytes)))
                    .unwrap_or_else(|_| "anonymous:invalid".to_string())
            });
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 64)
            .ok_or_else(|| ProtocolError::upstream("output item type is invalid"))?;
        self.claim_output_item(&id, kind)
    }

    fn claim_output_item(&mut self, id: &str, kind: &str) -> Result<(), ProtocolError> {
        if id.is_empty() || id.len() > 512 {
            return Err(ProtocolError::upstream("upstream item id is invalid"));
        }
        if self
            .output_items
            .get(id)
            .is_some_and(|claimed| claimed != kind)
        {
            return Err(ProtocolError::upstream("output item type changed"));
        }
        self.output_items
            .entry(id.to_string())
            .or_insert_with(|| kind.to_string());
        if self.output_items.len() > MAX_OUTPUT_ITEMS {
            return Err(ProtocolError::bounds("too many output items"));
        }
        Ok(())
    }

    fn require_no_active_block(&self) -> Result<(), ProtocolError> {
        if self.active_block.is_none() {
            Ok(())
        } else {
            Err(ProtocolError::upstream("output blocks overlap"))
        }
    }

    fn reserve_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        add_bounded(
            &mut self.total_text_bytes,
            text.len(),
            MAX_TEXT_BYTES,
            "text is too large",
        )?;
        self.reserve_payload(text.len(), escaped_json_len(text))
    }

    fn reserve_reasoning(&mut self, text: &str) -> Result<(), ProtocolError> {
        add_bounded(
            &mut self.total_reasoning_bytes,
            text.len(),
            MAX_REASONING_BYTES,
            "reasoning is too large",
        )?;
        self.reserve_payload(text.len(), escaped_json_len(text))
    }

    fn reserve_reasoning_opaque(&mut self, encrypted: &str) -> Result<(), ProtocolError> {
        add_bounded(
            &mut self.total_reasoning_bytes,
            encrypted.len(),
            MAX_REASONING_BYTES,
            "reasoning is too large",
        )?;
        self.reserve_stored(encrypted.len())
    }

    fn reserve_identity(&mut self, value: &str, overhead: usize) -> Result<(), ProtocolError> {
        self.reserve_payload(
            value.len().saturating_add(overhead),
            escaped_json_len(value).saturating_add(overhead),
        )
    }

    fn reserve_payload(
        &mut self,
        stored: usize,
        estimated_json: usize,
    ) -> Result<(), ProtocolError> {
        self.reserve_stored(stored)?;
        self.estimated_nonstream_bytes = self
            .estimated_nonstream_bytes
            .checked_add(estimated_json)
            .ok_or_else(|| ProtocolError::bounds("nonstream response is too large"))?;
        if self.estimated_nonstream_bytes > MAX_NONSTREAM_BYTES {
            return Err(ProtocolError::bounds("nonstream response is too large"));
        }
        Ok(())
    }

    fn reserve_stored(&mut self, amount: usize) -> Result<(), ProtocolError> {
        self.stored_bytes = self
            .stored_bytes
            .checked_add(amount)
            .ok_or_else(|| ProtocolError::bounds("response aggregate is too large"))?;
        if self.stored_bytes > MAX_NONSTREAM_BYTES {
            return Err(ProtocolError::bounds("response aggregate is too large"));
        }
        Ok(())
    }

    fn release_stored(&mut self, amount: usize) {
        self.stored_bytes = self.stored_bytes.saturating_sub(amount);
    }

    fn read_usage(&mut self, response: Option<&Value>) {
        let usage = response.and_then(|value| value.get("usage"));
        if let Some(input) = usage
            .and_then(|value| value.get("input_tokens"))
            .and_then(Value::as_u64)
        {
            self.input_tokens = input;
        }
        if let Some(output) = usage
            .and_then(|value| value.get("output_tokens"))
            .and_then(Value::as_u64)
        {
            self.output_tokens = output;
        }
    }

    fn allocate_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }
}

fn escaped_json_len(value: &str) -> usize {
    value
        .chars()
        .map(|character| match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
            character if character <= '\u{1f}' => 6,
            character => character.len_utf8(),
        })
        .sum()
}

fn required_event_id(event: &Value) -> Result<&str, ProtocolError> {
    event
        .get("item_id")
        .or_else(|| event.get("call_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .ok_or_else(|| ProtocolError::upstream("upstream item id is invalid"))
}

fn item_id(item: &Value) -> Result<&str, ProtocolError> {
    item.get("id")
        .or_else(|| item.get("call_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .ok_or_else(|| ProtocolError::upstream("output item id is invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: &str = "00112233445566778899aabbccddeeff";
    const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";

    fn signer() -> ThinkingSigner {
        ThinkingSigner::new(&[7_u8; 32]).unwrap()
    }

    fn context<'a>() -> RequestContext<'a> {
        RequestContext {
            target_model: "gpt-5.6-codex",
            auth_epoch: EPOCH,
            account_hash: ACCOUNT,
            reasoning_effort: Some("high"),
            supports_reasoning_summary: true,
            supports_parallel_tool_calls: true,
        }
    }

    #[test]
    fn signature_round_trip_and_context_binding() {
        let signer = signer();
        let signature = signer
            .seal(EPOCH, ACCOUNT, "rs_1", Some("call_1"), "opaque-ciphertext")
            .unwrap();
        let opened = signer
            .open(&signature, EPOCH, ACCOUNT, Some("call_1"))
            .unwrap();
        assert_eq!(opened.item_id, "rs_1");
        assert_eq!(opened.encrypted_content.as_str(), "opaque-ciphertext");
        assert!(matches!(
            signer.open(&signature, EPOCH, ACCOUNT, Some("call_2")),
            Err(ProtocolError {
                kind: ProtocolErrorKind::InvalidSignature,
                detail: "thinking signature context changed"
            })
        ));
        let mut tampered = signature.into_bytes();
        let index = tampered.len() / 2;
        tampered[index] = if tampered[index] == b'a' { b'b' } else { b'a' };
        assert!(signer
            .open(
                std::str::from_utf8(&tampered).unwrap(),
                EPOCH,
                ACCOUNT,
                Some("call_1")
            )
            .is_err());
    }

    #[test]
    fn translator_preserves_history_images_tools_and_signed_reasoning() {
        let signer = signer();
        let signature = signer
            .seal(EPOCH, ACCOUNT, "rs_1", Some("call_1"), "encrypted-1")
            .unwrap();
        let request = json!({
            "model": "claude-opus-4-8",
            "system": [{"type": "text", "text": "be precise", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "plan", "signature": signature},
                    {"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "x"}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "ok"}]},
                {"role": "assistant", "content": "done"}
            ],
            "tools": [{"name": "read", "description": "read a file", "input_schema": {"type": "object"}}],
            "thinking": {"type": "adaptive"},
            "stream": false
        });
        let translated = translate_anthropic_request(&request, &context(), &signer).unwrap();
        assert_eq!(translated["model"], "gpt-5.6-codex");
        assert_eq!(translated["store"], false);
        assert_eq!(translated["stream"], true);
        assert_eq!(translated["instructions"], "be precise");
        assert!(translated.get("previous_response_id").is_none());
        assert_eq!(
            translated["include"],
            json!(["reasoning.encrypted_content"])
        );
        assert_eq!(translated["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(translated["input"][1]["type"], "reasoning");
        assert_eq!(translated["input"][1]["encrypted_content"], "encrypted-1");
        assert_eq!(translated["input"][2]["type"], "function_call");
        assert_eq!(translated["input"][3]["type"], "function_call_output");
    }

    #[test]
    fn translator_rejects_unsigned_or_foreign_reasoning() {
        let signer = signer();
        let foreign = signer
            .seal(
                EPOCH,
                "fedcba9876543210fedcba9876543210",
                "rs_1",
                None,
                "secret",
            )
            .unwrap();
        for signature in ["not-signed".to_string(), foreign] {
            let request = json!({
                "messages": [{"role": "assistant", "content": [{
                    "type": "thinking", "thinking": "x", "signature": signature
                }]}]
            });
            assert!(translate_anthropic_request(&request, &context(), &signer).is_err());
        }
    }

    #[test]
    fn translator_maps_named_tool_choice_and_model_capabilities() {
        let request = json!({
            "messages": [{"role": "user", "content": "use the exact tool"}],
            "tools": [
                {"name": "read", "input_schema": {"type": "object"}},
                {"name": "write", "input_schema": {"type": "object"}}
            ],
            "tool_choice": {"type": "tool", "name": "read", "disable_parallel_tool_use": true}
        });
        let translated = translate_anthropic_request(&request, &context(), &signer()).unwrap();
        assert_eq!(
            translated["tool_choice"],
            json!({"type": "function", "name": "read"})
        );
        assert_eq!(translated["parallel_tool_calls"], false);
        assert_eq!(
            translated["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );

        let no_summary_or_parallel = RequestContext {
            supports_reasoning_summary: false,
            supports_parallel_tool_calls: false,
            ..context()
        };
        let mut request = request;
        request["tool_choice"] = json!({"type": "auto"});
        let translated =
            translate_anthropic_request(&request, &no_summary_or_parallel, &signer()).unwrap();
        assert_eq!(translated["parallel_tool_calls"], false);
        assert_eq!(translated["reasoning"], json!({"effort": "high"}));
    }

    #[test]
    fn translator_rejects_unknown_named_tool_before_transport() {
        let request = json!({
            "messages": [{"role": "user", "content": "use it"}],
            "tools": [{"name": "read", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "missing"}
        });
        assert!(matches!(
            translate_anthropic_request(&request, &context(), &signer()),
            Err(ProtocolError {
                kind: ProtocolErrorKind::InvalidRequest,
                detail: "forced tool is not declared"
            })
        ));
    }

    #[test]
    fn sse_decoder_handles_chunking_and_rejects_oversize_event() {
        let mut decoder = SseDecoder::new();
        assert!(decoder
            .feed(b"event: response.created\nda")
            .unwrap()
            .is_empty());
        let events = decoder
            .feed(b"ta: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "response.created");
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed(&vec![b'x'; MAX_EVENT_BYTES + 1]).is_err());
    }

    #[test]
    fn reducer_stream_and_nonstream_share_one_state_machine() {
        let signer = signer();
        let mut reducer = ResponsesReducer::new("gpt-5.6-codex", EPOCH, ACCOUNT, &signer);
        let fixtures = vec![
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "summary_index": 0, "delta": "plan"}),
            json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "plan"}], "encrypted_content": "cipher"}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "hello"}),
            json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]}}),
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "call_id": "call_1", "delta": "{\"path\":"}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "call_id": "call_1", "delta": "\"x\"}"}),
            json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"x\"}"}}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "usage": {"input_tokens": 5, "output_tokens": 7}}}),
        ];
        let mut kinds = Vec::new();
        let mut signature = None;
        for fixture in fixtures {
            for event in reducer.apply(fixture).unwrap() {
                kinds.push(event.event);
                if event.data["delta"]["type"] == "signature_delta" {
                    signature = event.data["delta"]["signature"]
                        .as_str()
                        .map(ToString::to_string);
                }
            }
        }
        reducer.finish_stream().unwrap();
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == "message_start")
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == "content_block_start")
                .count(),
            3
        );
        assert!(signature.unwrap().starts_with(SIGNATURE_PREFIX));
        let response = reducer.nonstream_response().unwrap();
        assert_eq!(response["id"], "resp_1");
        assert_eq!(response["stop_reason"], "tool_use");
        assert_eq!(
            response["usage"],
            json!({"input_tokens": 5, "output_tokens": 7})
        );
        assert_eq!(
            response["content"][1],
            json!({"type": "text", "text": "hello"})
        );
        assert_eq!(response["content"][2]["input"], json!({"path": "x"}));
        let replay_signature = response["content"][0]["signature"].as_str().unwrap();
        let replay = signer.open(replay_signature, EPOCH, ACCOUNT, None).unwrap();
        assert_eq!(replay.item_id, "rs_1");
        assert_eq!(replay.encrypted_content.as_str(), "cipher");
    }

    #[test]
    fn reducer_rejects_disconnect_duplicate_terminal_and_mismatched_arguments() {
        let signer = signer();
        let mut reducer = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        reducer
            .apply(json!({"type": "response.created", "response": {"id": "r"}}))
            .unwrap();
        assert_eq!(
            reducer.finish_stream(),
            Err(ProtocolError::incomplete(
                "upstream stream ended before completion"
            ))
        );
        reducer
            .apply(json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc", "call_id": "c", "name": "t"}}))
            .unwrap();
        reducer
            .apply(json!({"type": "response.function_call_arguments.delta", "item_id": "fc", "delta": "{}"}))
            .unwrap();
        assert!(reducer
            .apply(json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc", "call_id": "c", "name": "t", "arguments": "{\"different\":true}"}}))
            .is_err());
    }

    #[test]
    fn request_body_gate_and_tool_history_are_strict() {
        assert!(validate_request_body_size(MAX_REQUEST_BYTES).is_ok());
        assert!(validate_request_body_size(MAX_REQUEST_BYTES + 1).is_err());

        let signer = signer();
        let orphan = json!({
            "messages": [{"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "missing", "content": "x"
            }]}]
        });
        assert!(translate_anthropic_request(&orphan, &context(), &signer).is_err());

        let duplicate_call = json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "a", "input": {}},
                {"type": "tool_use", "id": "call_1", "name": "b", "input": {}}
            ]}]
        });
        assert!(translate_anthropic_request(&duplicate_call, &context(), &signer).is_err());

        let duplicate_result = json!({
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "a", "input": {}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "one"},
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "two"}
                ]}
            ]
        });
        assert!(translate_anthropic_request(&duplicate_result, &context(), &signer).is_err());
    }

    #[test]
    fn request_tool_argument_limit_is_per_call_not_whole_history() {
        let signer = signer();
        let payload = "x".repeat(5 * 1024 * 1024);
        let request = json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "a", "input": {"payload": payload}},
                {"type": "tool_use", "id": "call_2", "name": "b", "input": {"payload": payload}}
            ]}]
        });
        let translated = translate_anthropic_request(&request, &context(), &signer).unwrap();
        assert_eq!(translated["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn thinking_requires_the_exact_adjacent_tool_binding() {
        let signer = signer();
        let unbound = signer
            .seal(EPOCH, ACCOUNT, "rs_1", None, "encrypted")
            .unwrap();
        let wrong_shape = json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "plan", "signature": unbound},
                {"type": "tool_use", "id": "call_1", "name": "read", "input": {}}
            ]}]
        });
        assert!(translate_anthropic_request(&wrong_shape, &context(), &signer).is_err());

        let bound = signer
            .seal(EPOCH, ACCOUNT, "rs_1", Some("call_1"), "encrypted")
            .unwrap();
        let correct = json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "plan", "signature": bound},
                {"type": "tool_use", "id": "call_1", "name": "read", "input": {}}
            ]}]
        });
        assert!(translate_anthropic_request(&correct, &context(), &signer).is_ok());
    }

    #[test]
    fn sse_decoder_allows_many_small_events_in_one_large_feed() {
        let mut decoder = SseDecoder::new();
        let line = b": keepalive\n";
        let repeats = MAX_EVENT_BYTES / line.len() + 1024;
        let mut chunk = Vec::with_capacity(repeats * line.len());
        for _ in 0..repeats {
            chunk.extend_from_slice(line);
        }
        assert!(chunk.len() > MAX_EVENT_BYTES);
        assert!(decoder.feed(&chunk).unwrap().is_empty());
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn reducer_binds_reasoning_to_tool_and_closes_blocks_in_order() {
        let signer = signer();
        let mut reducer = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        let fixtures = vec![
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs", "delta": "plan"}),
            json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs", "summary": [{"type": "summary_text", "text": "plan"}], "encrypted_content": "cipher"}}),
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc", "call_id": "call", "name": "read"}}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc", "call_id": "call", "delta": "{}"}),
            json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc", "call_id": "call", "name": "read", "arguments": "{}"}}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 11, "output_tokens": 3}}}),
        ];
        let mut active = false;
        let mut signature = None;
        let mut terminal_usage = None;
        for fixture in fixtures {
            for event in reducer.apply(fixture).unwrap() {
                if event.event == "content_block_start" {
                    assert!(!active, "content blocks must never nest");
                    active = true;
                } else if event.event == "content_block_stop" {
                    assert!(active, "content block stop must match a start");
                    active = false;
                }
                if event.data["delta"]["type"] == "signature_delta" {
                    signature = event.data["delta"]["signature"]
                        .as_str()
                        .map(ToString::to_string);
                }
                if event.event == "message_delta" {
                    terminal_usage = Some(event.data["usage"].clone());
                }
            }
        }
        assert!(!active);
        let signature = signature.unwrap();
        assert!(signer
            .open(&signature, EPOCH, ACCOUNT, Some("call"))
            .is_ok());
        assert!(signer.open(&signature, EPOCH, ACCOUNT, None).is_err());
        assert_eq!(
            terminal_usage.unwrap(),
            json!({"input_tokens": 11, "output_tokens": 3})
        );
    }

    #[test]
    fn reducer_supports_multiple_text_items_without_cross_item_merge() {
        let signer = signer();
        let mut reducer = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        let fixtures = vec![
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "before"}),
            json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": "before"}]}}),
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc", "call_id": "call", "name": "read"}}),
            json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc", "call_id": "call", "name": "read", "arguments": "{}"}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_2", "delta": "after"}),
            json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_2", "content": [{"type": "output_text", "text": "after"}]}}),
            json!({"type": "response.completed", "response": {"usage": {}}}),
        ];
        let mut active = false;
        for fixture in fixtures {
            for event in reducer.apply(fixture).unwrap() {
                match event.event {
                    "content_block_start" => {
                        assert!(!active);
                        active = true;
                    }
                    "content_block_stop" => {
                        assert!(active);
                        active = false;
                    }
                    _ => {}
                }
            }
        }
        assert!(!active);
        let response = reducer.nonstream_response().unwrap();
        assert_eq!(
            response["content"][0],
            json!({"type": "text", "text": "before"})
        );
        assert_eq!(response["content"][1]["type"], "tool_use");
        assert_eq!(
            response["content"][2],
            json!({"type": "text", "text": "after"})
        );
    }

    #[test]
    fn reducer_rejects_duplicate_call_ids_identity_changes_and_delta_item_overflow() {
        let signer = signer();
        let mut reducer = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        reducer
            .apply(json!({"type": "response.created", "response": {"id": "r"}}))
            .unwrap();
        reducer
            .apply(json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "same", "name": "a"}}))
            .unwrap();
        assert!(reducer
            .apply(json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_2", "call_id": "same", "name": "b"}}))
            .is_err());

        let mut reducer = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        reducer
            .apply(json!({"type": "response.created", "response": {"id": "r"}}))
            .unwrap();
        reducer
            .apply(json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc", "call_id": "call", "name": "a"}}))
            .unwrap();
        assert!(reducer
            .apply(json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc", "call_id": "changed", "name": "a", "arguments": "{}"}}))
            .is_err());

        let mut reducer = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        reducer
            .apply(json!({"type": "response.created", "response": {"id": "r"}}))
            .unwrap();
        for index in 0..MAX_OUTPUT_ITEMS {
            reducer
                .apply(json!({"type": "response.output_item.added", "item": {"type": "other", "id": format!("item_{index}")}}))
                .unwrap();
        }
        assert!(reducer
            .apply(json!({"type": "response.reasoning_summary_text.delta", "item_id": "overflow", "delta": "x"}))
            .is_err());
    }

    #[test]
    fn reducer_enforces_cumulative_reasoning_and_aggregate_before_terminal() {
        let signer = signer();
        let mut reducer = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        reducer
            .apply(json!({"type": "response.created", "response": {"id": "r"}}))
            .unwrap();
        let first = "a".repeat(8 * 1024 * 1024);
        reducer
            .apply(json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": first}))
            .unwrap();
        reducer
            .apply(json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": first}], "encrypted_content": "x"}}))
            .unwrap();
        let second = "b".repeat(8 * 1024 * 1024);
        assert!(reducer
            .apply(json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_2", "delta": second}))
            .is_err());

        let mut reducer = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        reducer
            .reserve_payload(MAX_NONSTREAM_BYTES - 512, MAX_NONSTREAM_BYTES - 512)
            .unwrap();
        assert!(reducer.reserve_payload(1, 1).is_err());
    }

    #[test]
    fn reducer_requires_one_created_event_and_binds_item_ids_to_types() {
        let signer = signer();
        let mut late = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        assert!(late
            .apply(json!({"type": "response.output_text.delta", "item_id": "m", "delta": "x"}))
            .is_err());

        let mut duplicate = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        duplicate
            .apply(json!({"type": "response.created", "response": {"id": "r"}}))
            .unwrap();
        assert!(duplicate
            .apply(json!({"type": "response.created", "response": {"id": "other"}}))
            .is_err());

        let mut changed_type = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        changed_type
            .apply(json!({"type": "response.created", "response": {"id": "r"}}))
            .unwrap();
        changed_type
            .apply(json!({"type": "response.output_item.done", "item": {"type": "message", "id": "same", "content": [{"type": "output_text", "text": "x"}]}}))
            .unwrap();
        assert!(changed_type
            .apply(json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "same", "call_id": "call", "name": "read"}}))
            .is_err());
    }

    #[test]
    fn reducer_only_maps_token_limit_incomplete_to_max_tokens() {
        let signer = signer();
        let mut filtered = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        filtered
            .apply(json!({"type": "response.created", "response": {"id": "r"}}))
            .unwrap();
        assert!(filtered
            .apply(json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "content_filter"}}}))
            .is_err());

        let mut limited = ResponsesReducer::new("gpt", EPOCH, ACCOUNT, &signer);
        limited
            .apply(json!({"type": "response.created", "response": {"id": "r"}}))
            .unwrap();
        limited
            .apply(json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 1, "output_tokens": 2}}}))
            .unwrap();
        assert_eq!(
            limited.nonstream_response().unwrap()["stop_reason"],
            "max_tokens"
        );
    }
}
