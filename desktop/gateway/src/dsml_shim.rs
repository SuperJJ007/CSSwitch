use regex::Regex;
use serde_json::{json, Map, Number, Value};
use std::sync::OnceLock;

const DSML_MARKERS: [&[u8]; 2] = ["｜DSML｜".as_bytes(), "｜｜DSML｜｜".as_bytes()];

#[derive(Debug, Default)]
pub struct DsmlDetector {
    pub found: bool,
    tail: Vec<u8>,
}

impl DsmlDetector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, data: &[u8]) {
        if self.found || data.is_empty() {
            return;
        }
        let mut buf = Vec::with_capacity(self.tail.len() + data.len());
        buf.extend_from_slice(&self.tail);
        buf.extend_from_slice(data);
        if DSML_MARKERS
            .iter()
            .any(|marker| buf.windows(marker.len()).any(|window| window == *marker))
        {
            self.found = true;
            self.tail.clear();
            return;
        }
        let keep = DSML_MARKERS
            .iter()
            .map(|marker| marker.len())
            .max()
            .unwrap_or(1)
            - 1;
        if buf.len() > keep {
            self.tail = buf[buf.len() - keep..].to_vec();
        } else {
            self.tail = buf;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ToolCall {
    name: String,
    input: Value,
}

#[derive(Debug, Clone, PartialEq)]
enum Segment {
    Text(String),
    ToolUse(ToolCall),
}

fn toolcalls_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?s)<[｜]{1,2}DSML[｜]{1,2}(?:tool_calls|function_calls)>(.*?)</[｜]{1,2}DSML[｜]{1,2}(?:tool_calls|function_calls)>"#,
        )
        .unwrap()
    })
}

fn invoke_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?s)<[｜]{1,2}DSML[｜]{1,2}invoke\s+name="([^"]+)"\s*>(.*?)</[｜]{1,2}DSML[｜]{1,2}invoke>"#,
        )
        .unwrap()
    })
}

fn param_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?s)<[｜]{1,2}DSML[｜]{1,2}parameter\s+name="([^"]+)"(?:\s+string="(true|false)")?\s*>(.*?)</[｜]{1,2}DSML[｜]{1,2}parameter>"#,
        )
        .unwrap()
    })
}

fn schema_properties(schema: Option<&Value>) -> Map<String, Value> {
    schema
        .and_then(|schema| schema.get("properties"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn coerce_param(string_attr: Option<&str>, raw: &str, prop_schema: Option<&Value>) -> Value {
    if string_attr == Some("true") {
        return Value::String(raw.to_string());
    }
    let typ = prop_schema
        .and_then(|schema| schema.get("type"))
        .and_then(Value::as_str);
    match typ {
        Some("string") => return Value::String(raw.to_string()),
        Some("integer") => {
            if let Ok(value) = raw.parse::<i64>() {
                return Value::Number(value.into());
            }
        }
        Some("number") => {
            if let Ok(value) = raw.parse::<f64>() {
                if let Some(number) = Number::from_f64(value) {
                    return Value::Number(number);
                }
            }
        }
        Some("boolean") => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => return Value::Bool(true),
            "false" | "0" | "no" => return Value::Bool(false),
            _ => return Value::String(raw.to_string()),
        },
        Some("object") | Some("array") => {
            if let Ok(value) = serde_json::from_str::<Value>(raw) {
                return value;
            }
        }
        _ => {}
    }
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn type_ok(value: &Value, typ: Option<&str>) -> bool {
    match typ {
        None => true,
        Some("string") => value.is_string(),
        Some("integer") => {
            value.as_i64().is_some()
                || value
                    .as_str()
                    .map(|text| text.trim().parse::<i64>().is_ok())
                    .unwrap_or(false)
        }
        Some("number") => {
            value.as_f64().is_some()
                || value
                    .as_str()
                    .map(|text| text.trim().parse::<f64>().is_ok())
                    .unwrap_or(false)
        }
        Some("boolean") => {
            value.is_boolean()
                || value
                    .as_str()
                    .map(|text| {
                        matches!(
                            text.trim().to_ascii_lowercase().as_str(),
                            "true" | "false" | "1" | "0" | "yes" | "no"
                        )
                    })
                    .unwrap_or(false)
        }
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        _ => true,
    }
}

fn validate_input(input: &Map<String, Value>, schema: Option<&Value>) -> bool {
    let schema = schema.unwrap_or(&Value::Null);
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for field in required.iter().filter_map(Value::as_str) {
            if !input.contains_key(field) {
                return false;
            }
        }
    }
    let props = schema_properties(Some(schema));
    for (key, value) in input {
        let typ = props
            .get(key)
            .and_then(|prop| prop.get("type"))
            .and_then(Value::as_str);
        if props.contains_key(key) && !type_ok(value, typ) {
            return false;
        }
    }
    true
}

fn parse_invoke(name: &str, body: &str, known_tools: &Map<String, Value>) -> Option<ToolCall> {
    let schema = known_tools.get(name);
    let props = schema_properties(schema);
    let mut input = Map::new();
    for caps in param_re().captures_iter(body) {
        let pname = caps.get(1)?.as_str();
        let string_attr = caps.get(2).map(|m| m.as_str());
        let raw = caps.get(3)?.as_str();
        input.insert(
            pname.to_string(),
            coerce_param(string_attr, raw, props.get(pname)),
        );
    }
    if input.len() == 1 {
        let only = input.keys().next().cloned().unwrap_or_default();
        if (only == "arguments" || only == "input") && !props.contains_key(&only) {
            if let Some(value) = input.remove(&only) {
                let parsed = value
                    .as_str()
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    .unwrap_or(value);
                if let Value::Object(obj) = parsed {
                    input = obj;
                }
            }
        }
    }
    if !validate_input(&input, schema) {
        return None;
    }
    Some(ToolCall {
        name: name.to_string(),
        input: Value::Object(input),
    })
}

fn parse_dsml_tool_calls(wrapper_region: &str, known_tools: &Map<String, Value>) -> Vec<ToolCall> {
    let mut out = Vec::new();
    for wrapper in toolcalls_re().captures_iter(wrapper_region) {
        let Some(inner) = wrapper.get(1).map(|m| m.as_str()) else {
            return Vec::new();
        };
        let invokes: Vec<_> = invoke_re().captures_iter(inner).collect();
        if invokes.is_empty() {
            return Vec::new();
        }
        for invoke in invokes {
            let Some(name) = invoke.get(1).map(|m| m.as_str()) else {
                return Vec::new();
            };
            let Some(body) = invoke.get(2).map(|m| m.as_str()) else {
                return Vec::new();
            };
            if !known_tools.contains_key(name) {
                return Vec::new();
            }
            let Some(call) = parse_invoke(name, body, known_tools) else {
                return Vec::new();
            };
            out.push(call);
        }
    }
    out
}

fn segment_dsml_text(text: &str, known_tools: &Map<String, Value>) -> Vec<Segment> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut segments = Vec::new();
    let mut pos = 0;
    for wrapper in toolcalls_re().find_iter(text) {
        let calls = parse_dsml_tool_calls(wrapper.as_str(), known_tools);
        if calls.is_empty() {
            continue;
        }
        if wrapper.start() > pos {
            segments.push(Segment::Text(text[pos..wrapper.start()].to_string()));
        }
        segments.extend(calls.into_iter().map(Segment::ToolUse));
        pos = wrapper.end();
    }
    if pos < text.len() {
        segments.push(Segment::Text(text[pos..].to_string()));
    }
    if segments.is_empty() {
        return vec![Segment::Text(text.to_string())];
    }
    segments
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
    Pass,
    Capture,
}

#[derive(Debug, Clone)]
pub struct DsmlStreamRewriter {
    known_tools: Map<String, Value>,
    nonce: String,
    pending_bytes: Vec<u8>,
    frame_buf: String,
    next_out: usize,
    cur_out: Option<usize>,
    cur_type: Option<String>,
    pub synthesized: bool,
    pub tool_n: usize,
    state: StreamState,
    scan_buf: String,
    cap_buf: String,
}

impl DsmlStreamRewriter {
    const MAX_OPEN: usize = "<｜｜DSML｜｜function_calls>".len();
    const CAP: usize = 256 * 1024;

    pub fn new(known_tools: Map<String, Value>, nonce: &str) -> Self {
        Self {
            known_tools,
            nonce: if nonce.is_empty() {
                "x".to_string()
            } else {
                nonce.to_string()
            },
            pending_bytes: Vec::new(),
            frame_buf: String::new(),
            next_out: 0,
            cur_out: None,
            cur_type: None,
            synthesized: false,
            tool_n: 0,
            state: StreamState::Pass,
            scan_buf: String::new(),
            cap_buf: String::new(),
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        self.push_utf8(data, false);
        self.drain_frames(false)
    }

    pub fn finalize(&mut self) -> Vec<u8> {
        self.push_utf8(&[], true);
        let mut out = self.drain_frames(true);
        out.extend(self.finalize_text());
        out
    }

    fn push_utf8(&mut self, data: &[u8], final_flush: bool) {
        self.pending_bytes.extend_from_slice(data);
        loop {
            match std::str::from_utf8(&self.pending_bytes) {
                Ok(text) => {
                    self.frame_buf.push_str(text);
                    self.pending_bytes.clear();
                    return;
                }
                Err(e) if e.valid_up_to() > 0 => {
                    let valid_up_to = e.valid_up_to();
                    let text = std::str::from_utf8(&self.pending_bytes[..valid_up_to]).unwrap();
                    self.frame_buf.push_str(text);
                    self.pending_bytes = self.pending_bytes[valid_up_to..].to_vec();
                }
                Err(e) if final_flush => {
                    let valid_up_to = e.valid_up_to();
                    if valid_up_to > 0 {
                        let text = std::str::from_utf8(&self.pending_bytes[..valid_up_to]).unwrap();
                        self.frame_buf.push_str(text);
                    }
                    if valid_up_to < self.pending_bytes.len() {
                        self.frame_buf
                            .push_str(&String::from_utf8_lossy(&self.pending_bytes[valid_up_to..]));
                    }
                    self.pending_bytes.clear();
                    return;
                }
                Err(_) => return,
            }
        }
    }

    fn drain_frames(&mut self, flush_tail: bool) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some((idx, sep)) = next_frame_boundary(&self.frame_buf) {
            let frame = self.frame_buf[..idx].to_string();
            self.frame_buf = self.frame_buf[idx + sep..].to_string();
            out.extend(self.handle_frame(&frame));
        }
        if flush_tail && !self.frame_buf.trim().is_empty() {
            let frame = std::mem::take(&mut self.frame_buf);
            out.extend(self.handle_frame(&frame));
        }
        out
    }

    fn handle_frame(&mut self, frame: &str) -> Vec<u8> {
        let (event, obj) = parse_sse_frame(frame);
        let Some(mut obj) = obj else {
            return raw_frame(frame);
        };
        let Some(kind) = obj.get("type").and_then(Value::as_str) else {
            return raw_frame(frame);
        };
        match kind {
            "content_block_start" => {
                self.cur_type = obj
                    .get("content_block")
                    .and_then(Value::as_object)
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let idx = self.next_out;
                self.next_out += 1;
                self.cur_out = Some(idx);
                obj["index"] = Value::Number(idx.into());
                emit_sse(event.as_deref().unwrap_or("content_block_start"), &obj)
            }
            "content_block_delta" => {
                let delta_type = obj
                    .get("delta")
                    .and_then(Value::as_object)
                    .and_then(|delta| delta.get("type"))
                    .and_then(Value::as_str);
                if self.cur_type.as_deref() == Some("text") && delta_type == Some("text_delta") {
                    let text = obj
                        .get("delta")
                        .and_then(Value::as_object)
                        .and_then(|delta| delta.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    return self.on_text_delta(text);
                }
                if let Some(cur_out) = self.cur_out {
                    obj["index"] = Value::Number(cur_out.into());
                }
                emit_sse(event.as_deref().unwrap_or("content_block_delta"), &obj)
            }
            "content_block_stop" => self.on_block_stop(),
            "message_delta" => {
                let mut out = self.flush_pending();
                out.extend(self.on_message_delta(obj, event.as_deref().unwrap_or("message_delta")));
                out
            }
            "message_stop" => {
                let mut out = self.flush_pending();
                out.extend(raw_frame(frame));
                out
            }
            _ => raw_frame(frame),
        }
    }

    fn on_text_delta(&mut self, text: &str) -> Vec<u8> {
        match self.state {
            StreamState::Pass => {
                self.scan_buf.push_str(text);
                self.pass_scan()
            }
            StreamState::Capture => {
                self.cap_buf.push_str(text);
                self.capture_scan()
            }
        }
    }

    fn pass_scan(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(found) = open_re().find(&self.scan_buf) {
            let start = found.start();
            let before = self.scan_buf[..start].to_string();
            let cap = self.scan_buf[start..].to_string();
            if !before.is_empty() {
                out.extend(self.text_delta(&before));
            }
            if let Some(cur_out) = self.cur_out.take() {
                out.extend(emit_sse(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": cur_out}),
                ));
            }
            self.cap_buf = cap;
            self.scan_buf.clear();
            self.state = StreamState::Capture;
            out.extend(self.capture_scan());
            return out;
        }
        let keep = Self::MAX_OPEN - 1;
        let char_count = self.scan_buf.chars().count();
        if char_count > keep {
            let emit_chars = char_count - keep;
            let split_at = self
                .scan_buf
                .char_indices()
                .nth(emit_chars)
                .map(|(idx, _)| idx)
                .unwrap_or(self.scan_buf.len());
            let emit = self.scan_buf[..split_at].to_string();
            self.scan_buf = self.scan_buf[split_at..].to_string();
            if !emit.is_empty() {
                out.extend(self.text_delta(&emit));
            }
        }
        out
    }

    fn capture_scan(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(found) = toolcalls_re().find(&self.cap_buf) {
            let region = found.as_str().to_string();
            let end = found.end();
            let rest = self.cap_buf[end..].to_string();
            let calls = parse_dsml_tool_calls(&region, &self.known_tools);
            if calls.is_empty() {
                out.extend(self.text_as_new_block(&region));
            } else {
                for call in calls {
                    out.extend(self.tool_use_events(&call));
                }
                self.synthesized = true;
            }
            self.cap_buf.clear();
            self.state = StreamState::Pass;
            self.cur_out = None;
            if !rest.is_empty() {
                self.scan_buf = rest;
                out.extend(self.pass_scan());
            }
            return out;
        }
        if self.cap_buf.len() > Self::CAP {
            let text = std::mem::take(&mut self.cap_buf);
            out.extend(self.text_as_new_block(&text));
            self.state = StreamState::Pass;
            self.cur_out = None;
        }
        out
    }

    fn finalize_text(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.state == StreamState::Capture && !self.cap_buf.is_empty() {
            let text = std::mem::take(&mut self.cap_buf);
            out.extend(self.text_as_new_block(&text));
            self.state = StreamState::Pass;
        }
        if !self.scan_buf.is_empty() {
            let text = std::mem::take(&mut self.scan_buf);
            out.extend(self.text_delta(&text));
        }
        if let Some(cur_out) = self.cur_out.take() {
            out.extend(emit_sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": cur_out}),
            ));
        }
        out
    }

    fn on_block_stop(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.state == StreamState::Capture {
            if !self.cap_buf.is_empty() {
                let text = std::mem::take(&mut self.cap_buf);
                out.extend(self.text_as_new_block(&text));
            }
            self.state = StreamState::Pass;
        } else if !self.scan_buf.is_empty() {
            if let Some(cur_out) = self.cur_out {
                let text = std::mem::take(&mut self.scan_buf);
                out.extend(emit_sse(
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": cur_out, "delta": {"type": "text_delta", "text": text}}),
                ));
            }
        }
        if let Some(cur_out) = self.cur_out.take() {
            out.extend(emit_sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": cur_out}),
            ));
        }
        out
    }

    fn flush_pending(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.state == StreamState::Capture && !self.cap_buf.is_empty() {
            let text = std::mem::take(&mut self.cap_buf);
            out.extend(self.text_as_new_block(&text));
            self.state = StreamState::Pass;
        } else if !self.scan_buf.is_empty() {
            let text = std::mem::take(&mut self.scan_buf);
            out.extend(self.text_delta(&text));
        }
        if let Some(cur_out) = self.cur_out.take() {
            out.extend(emit_sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": cur_out}),
            ));
        }
        out
    }

    fn text_delta(&mut self, text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        if self.cur_out.is_none() {
            out.extend(self.open_text_block());
        }
        let cur_out = self.cur_out.unwrap_or(0);
        out.extend(emit_sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": cur_out, "delta": {"type": "text_delta", "text": text}}),
        ));
        out
    }

    fn open_text_block(&mut self) -> Vec<u8> {
        let idx = self.next_out;
        self.next_out += 1;
        self.cur_out = Some(idx);
        self.cur_type = Some("text".to_string());
        emit_sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": idx, "content_block": {"type": "text", "text": ""}}),
        )
    }

    fn text_as_new_block(&mut self, text: &str) -> Vec<u8> {
        let mut out = self.text_delta(text);
        if let Some(cur_out) = self.cur_out.take() {
            out.extend(emit_sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": cur_out}),
            ));
        }
        out
    }

    fn tool_use_events(&mut self, call: &ToolCall) -> Vec<u8> {
        let idx = self.next_out;
        self.next_out += 1;
        self.tool_n += 1;
        let tool_id = format!("toolu_dsml_{}_{}", self.nonce, self.tool_n);
        let mut out = Vec::new();
        out.extend(emit_sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": idx, "content_block": {"type": "tool_use", "id": tool_id, "name": call.name, "input": {}}}),
        ));
        out.extend(emit_sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": idx, "delta": {"type": "input_json_delta", "partial_json": serde_json::to_string(&call.input).unwrap_or_else(|_| "{}".to_string())}}),
        ));
        out.extend(emit_sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": idx}),
        ));
        out
    }

    fn on_message_delta(&self, mut obj: Value, event: &str) -> Vec<u8> {
        if self.synthesized {
            if let Some(delta) = obj.get_mut("delta").and_then(Value::as_object_mut) {
                let should_override = match delta.get("stop_reason") {
                    None | Some(Value::Null) => true,
                    Some(Value::String(reason)) => reason == "end_turn" || reason == "stop",
                    _ => false,
                };
                if should_override {
                    delta.insert(
                        "stop_reason".to_string(),
                        Value::String("tool_use".to_string()),
                    );
                }
            }
        }
        emit_sse(event, &obj)
    }
}

fn open_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<[｜]{1,2}DSML[｜]{1,2}(?:tool_calls|function_calls)>"#).unwrap()
    })
}

fn next_frame_boundary(buf: &str) -> Option<(usize, usize)> {
    match (buf.find("\n\n"), buf.find("\r\n\r\n")) {
        (None, None) => None,
        (Some(i), None) => Some((i, 2)),
        (None, Some(i)) => Some((i, 4)),
        (Some(lf), Some(crlf)) if lf <= crlf => Some((lf, 2)),
        (Some(_), Some(crlf)) => Some((crlf, 4)),
    }
}

fn parse_sse_frame(frame: &str) -> (Option<String>, Option<Value>) {
    let mut event = None;
    let mut data_lines = Vec::new();
    for raw_line in frame.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
        }
    }
    if data_lines.is_empty() {
        return (event, None);
    }
    let data = data_lines.join("\n");
    (event, serde_json::from_str(&data).ok())
}

fn emit_sse(event: &str, obj: &Value) -> Vec<u8> {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(obj).unwrap_or_else(|_| "{}".to_string())
    )
    .into_bytes()
}

fn raw_frame(frame: &str) -> Vec<u8> {
    format!("{frame}\n\n").into_bytes()
}

pub fn rewrite_nonstream_body(
    body_bytes: &[u8],
    known_tools: &Map<String, Value>,
    nonce: &str,
) -> Vec<u8> {
    let nonce = if nonce.is_empty() { "x" } else { nonce };
    let Ok(mut obj) = serde_json::from_slice::<Value>(body_bytes) else {
        return body_bytes.to_vec();
    };
    let Some(content) = obj.get("content").and_then(Value::as_array) else {
        return body_bytes.to_vec();
    };
    let mut changed = false;
    let mut next_tool = 0;
    let mut new_content = Vec::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                let segments = segment_dsml_text(text, known_tools);
                if segments
                    .iter()
                    .any(|segment| matches!(segment, Segment::ToolUse(_)))
                {
                    changed = true;
                    for segment in segments {
                        match segment {
                            Segment::Text(text) => {
                                new_content.push(json!({"type": "text", "text": text}));
                            }
                            Segment::ToolUse(call) => {
                                next_tool += 1;
                                new_content.push(json!({
                                    "type": "tool_use",
                                    "id": format!("toolu_dsml_{nonce}_{next_tool}"),
                                    "name": call.name,
                                    "input": call.input,
                                }));
                            }
                        }
                    }
                    continue;
                }
            }
        }
        new_content.push(block.clone());
    }
    if !changed {
        return body_bytes.to_vec();
    }
    obj["content"] = Value::Array(new_content);
    let should_override_stop = match obj.get("stop_reason") {
        None | Some(Value::Null) => true,
        Some(Value::String(reason)) => reason == "end_turn" || reason == "stop",
        _ => false,
    };
    if should_override_stop {
        obj["stop_reason"] = Value::String("tool_use".to_string());
    }
    serde_json::to_vec(&obj).unwrap_or_else(|_| body_bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::{
        parse_dsml_tool_calls, parse_sse_frame, rewrite_nonstream_body, segment_dsml_text,
        DsmlDetector, DsmlStreamRewriter, Segment,
    };
    use serde_json::{json, Map, Value};

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../test/golden/dsml_nonstream.json")).unwrap()
    }

    fn tools(key: &str) -> Map<String, Value> {
        fixture()[key].as_object().unwrap().clone()
    }

    fn response(text: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "id": "m",
            "type": "message",
            "role": "assistant",
            "model": "deepseek-v4-pro",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2},
        }))
        .unwrap()
    }

    fn wrap(pipe: &str, tool: &str, params: &[(&str, &str, &str)]) -> String {
        let params = params
            .iter()
            .map(|(name, attr, value)| {
                format!(
                    "<{pipe}DSML{pipe}parameter name=\"{name}\"{attr}>{value}</{pipe}DSML{pipe}parameter>"
                )
            })
            .collect::<String>();
        format!(
            "<{pipe}DSML{pipe}tool_calls> <{pipe}DSML{pipe}invoke name=\"{tool}\">{params}</{pipe}DSML{pipe}invoke> </{pipe}DSML{pipe}tool_calls>"
        )
    }

    fn sse(event: &str, obj: Value) -> String {
        format!(
            "event: {event}\ndata: {}\n\n",
            serde_json::to_string(&obj).unwrap()
        )
    }

    fn parse_sse(raw: &[u8]) -> Vec<(String, Value)> {
        let text = String::from_utf8(raw.to_vec()).unwrap();
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < text.len() {
            let Some((idx, sep)) = super::next_frame_boundary(&text[pos..]) else {
                break;
            };
            let frame = &text[pos..pos + idx];
            let (event, data) = parse_sse_frame(frame);
            if let (Some(event), Some(data)) = (event, data) {
                out.push((event, data));
            }
            pos += idx + sep;
        }
        out
    }

    fn run_stream(
        raw: &str,
        chunk: usize,
        known_tools: Map<String, Value>,
    ) -> Vec<(String, Value)> {
        let mut rewriter = DsmlStreamRewriter::new(known_tools, "t");
        let mut out = Vec::new();
        for chunk in raw.as_bytes().chunks(chunk) {
            out.extend(rewriter.feed(chunk));
        }
        out.extend(rewriter.finalize());
        parse_sse(&out)
    }

    fn dsml_text_stream(query: &str, pre: &str, post: &str) -> String {
        let leak = format!(
            "{pre}<｜｜DSML｜｜tool_calls> <｜｜DSML｜｜invoke name=\"web_search\"><｜｜DSML｜｜parameter name=\"query\" string=\"true\">{query}</｜｜DSML｜｜parameter> </｜｜DSML｜｜invoke> </｜｜DSML｜｜tool_calls>{post}"
        );
        [
            sse("message_start", json!({"type": "message_start", "message": {"id": "m", "type": "message", "role": "assistant", "model": "deepseek-v4-pro", "content": [], "stop_reason": null, "usage": {"input_tokens": 1, "output_tokens": 0}}})),
            sse("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
            sse("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": leak}})),
            sse("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
            sse("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}})),
            sse("message_stop", json!({"type": "message_stop"})),
        ].join("")
    }

    #[test]
    fn detector_flags_marker_across_chunks_without_rewrite() {
        let mut detector = DsmlDetector::new();
        detector.feed(b"some text ");
        detector.feed("<｜".as_bytes());
        assert!(!detector.found);
        detector.feed("｜DSML｜｜tool_calls>".as_bytes());
        assert!(detector.found);
    }

    #[test]
    fn detector_clean_response_not_flagged() {
        let mut detector = DsmlDetector::new();
        detector.feed(b"just a normal answer, no tool markers at all");
        assert!(!detector.found);
    }

    #[test]
    fn detector_single_pipe_marker_split_byte_by_byte() {
        let mut detector = DsmlDetector::new();
        for byte in "｜DSML｜".as_bytes() {
            detector.feed(&[*byte]);
        }
        assert!(detector.found);
    }

    #[test]
    fn stream_no_dsml_preserves_semantics_and_utf8_splits() {
        let text = "a｜b｜｜c";
        let raw = [
            sse("message_start", json!({"type": "message_start", "message": {"id": "m", "type": "message", "role": "assistant", "model": "deepseek-v4-pro", "content": [], "stop_reason": null, "usage": {"input_tokens": 1, "output_tokens": 0}}})),
            sse("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}})),
            sse("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "let me think"}})),
            sse("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
            sse("content_block_start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}})),
            sse("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": text}})),
            sse("content_block_stop", json!({"type": "content_block_stop", "index": 1})),
            sse("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}})),
            sse("message_stop", json!({"type": "message_stop"})),
        ]
        .join("");
        let events = run_stream(&raw, 1, Map::new());
        let kinds = events
            .iter()
            .map(|(event, _)| event.as_str())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"message_start"));
        assert!(kinds.contains(&"message_stop"));
        let thinking = events
            .iter()
            .filter_map(|(_, data)| data["delta"].get("thinking").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(thinking, vec!["let me think"]);
        let text_out = events
            .iter()
            .filter_map(|(_, data)| data["delta"].get("text").and_then(Value::as_str))
            .collect::<String>();
        assert_eq!(text_out, text);
        let stop = events
            .iter()
            .find(|(event, _)| event == "message_delta")
            .unwrap()
            .1["delta"]["stop_reason"]
            .as_str()
            .unwrap();
        assert_eq!(stop, "end_turn");
    }

    #[test]
    fn stream_long_non_ascii_without_dsml_does_not_panic() {
        let text = "实验结果｜".repeat(32);
        let raw = [
            sse("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
            sse("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}})),
            sse("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ]
        .join("");
        let events = run_stream(&raw, 11, Map::new());
        let text_out = events
            .iter()
            .filter_map(|(_, data)| data["delta"].get("text").and_then(Value::as_str))
            .collect::<String>();
        assert_eq!(text_out, text);
    }

    #[test]
    fn stream_dsml_becomes_tool_use_across_chunk_sizes() {
        for chunk in [1, 3, 7, 4096] {
            let events = run_stream(
                &dsml_text_stream("GSE207177", "", ""),
                chunk,
                tools("web_search_schema"),
            );
            let tool_uses = events
                .iter()
                .filter_map(|(event, data)| {
                    if event == "content_block_start"
                        && data["content_block"]["type"].as_str() == Some("tool_use")
                    {
                        Some(data["content_block"].clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(tool_uses.len(), 1, "chunk={chunk}");
            assert_eq!(tool_uses[0]["name"], "web_search");
            let partial_json = events
                .iter()
                .find_map(|(event, data)| {
                    if event == "content_block_delta"
                        && data["delta"]["type"].as_str() == Some("input_json_delta")
                    {
                        data["delta"]["partial_json"].as_str()
                    } else {
                        None
                    }
                })
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(partial_json).unwrap(),
                json!({"query": "GSE207177"})
            );
            let stop = events
                .iter()
                .find(|(event, _)| event == "message_delta")
                .unwrap()
                .1["delta"]["stop_reason"]
                .as_str()
                .unwrap();
            assert_eq!(stop, "tool_use");
        }
    }

    #[test]
    fn stream_preserves_pre_post_and_unknown_or_incomplete_as_text() {
        let events = run_stream(
            &dsml_text_stream("q", "before ", " after"),
            3,
            tools("web_search_schema"),
        );
        let texts = events
            .iter()
            .filter_map(|(_, data)| data["delta"].get("text").and_then(Value::as_str))
            .collect::<String>();
        assert!(texts.contains("before "));
        assert!(texts.contains(" after"));
        assert!(!texts.contains("DSML"));

        let unknown = dsml_text_stream("q", "", "").replace("web_search", "evil_exec");
        let events = run_stream(&unknown, 5, tools("web_search_schema"));
        assert!(!events
            .iter()
            .any(|(_, data)| data["content_block"]["type"] == "tool_use"));
        let texts = events
            .iter()
            .filter_map(|(_, data)| data["delta"].get("text").and_then(Value::as_str))
            .collect::<String>();
        assert!(texts.contains("DSML"));

        let incomplete = [
            sse("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
            sse("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "<｜｜DSML｜｜tool_calls> <｜｜DSML｜｜invoke name=\"web_search\""}})),
            sse("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
            sse("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {}})),
            sse("message_stop", json!({"type": "message_stop"})),
        ].join("");
        let events = run_stream(&incomplete, 5, tools("web_search_schema"));
        let texts = events
            .iter()
            .filter_map(|(_, data)| data["delta"].get("text").and_then(Value::as_str))
            .collect::<String>();
        assert!(texts.contains("DSML"));
        let stop = events
            .iter()
            .find(|(event, _)| event == "message_delta")
            .unwrap()
            .1["delta"]["stop_reason"]
            .as_str()
            .unwrap();
        assert_eq!(stop, "end_turn");
    }

    #[test]
    fn stream_close_tag_boundary_and_missing_stop_do_not_emit_null_indexes() {
        let block = "<｜｜DSML｜｜tool_calls> <｜｜DSML｜｜invoke name=\"web_search\"><｜｜DSML｜｜parameter name=\"query\" string=\"true\">q</｜｜DSML｜｜parameter> </｜｜DSML｜｜invoke> </｜｜DSML｜｜tool_calls>";
        let raw = [
            sse("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
            sse("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": block}})),
            sse("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "done searching"}})),
            sse("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
            sse("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {}})),
            sse("message_stop", json!({"type": "message_stop"})),
        ].join("");
        let events = run_stream(&raw, 4096, tools("web_search_schema"));
        assert!(events
            .iter()
            .any(|(_, data)| data["content_block"]["type"].as_str() == Some("tool_use")));
        for (event, data) in &events {
            if event.starts_with("content_block") {
                assert!(!data.get("index").unwrap_or(&Value::Null).is_null());
            }
        }
        let texts = events
            .iter()
            .filter_map(|(_, data)| data["delta"].get("text").and_then(Value::as_str))
            .collect::<String>();
        assert!(texts.contains("done searching"));

        let raw = [
            sse("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
            sse("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {}})),
            sse("message_stop", json!({"type": "message_stop"})),
        ].join("");
        let events = run_stream(&raw, 4096, Map::new());
        let kinds = events
            .iter()
            .map(|(event, _)| event.as_str())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"content_block_stop"));
        assert!(
            kinds
                .iter()
                .position(|event| *event == "content_block_stop")
                .unwrap()
                < kinds
                    .iter()
                    .position(|event| *event == "message_stop")
                    .unwrap()
        );
    }

    #[test]
    fn segment_rewrites_known_tool_and_preserves_interleaving() {
        let fixture = fixture();
        let text = fixture["wrapped_text"].as_str().unwrap();
        let segments = segment_dsml_text(text, &tools("web_search_schema"));
        assert_eq!(segments.len(), 3);
        assert!(matches!(&segments[0], Segment::Text(text) if text == "A"));
        assert!(matches!(&segments[2], Segment::Text(text) if text == "B"));
        let Segment::ToolUse(call) = &segments[1] else {
            panic!("expected tool segment");
        };
        assert_eq!(call.name, "web_search");
        assert_eq!(call.input, json!({"query": "q"}));
    }

    #[test]
    fn issue8_multiple_wrappers_and_function_calls_alias_match_python() {
        let pipe = "｜｜";
        let q1 = "site:https://www.ncbi.nlm.nih.gov/geo/ \"GSE207177\"";
        let q2 = "\"GSE207177\" AND (\"sepsis\" OR \"heart\")";
        let q3 = "https://www.ncbi.nlm.nih.gov/geo/query/acc.cgi?acc=GSE207177";
        let block1 = format!(
            "<{pipe}DSML{pipe}tool_calls> {} {} </{pipe}DSML{pipe}tool_calls>",
            wrap(pipe, "web_search", &[("query", " string=\"true\"", q1)]),
            wrap(pipe, "web_search", &[("query", " string=\"true\"", q2)])
        );
        let block2 = format!(
            "<{pipe}DSML{pipe}function_calls> <{pipe}DSML{pipe}invoke name=\"web_search\"><{pipe}DSML{pipe}parameter name=\"query\" string=\"true\">{q3}</{pipe}DSML{pipe}parameter></{pipe}DSML{pipe}invoke> </{pipe}DSML{pipe}function_calls>"
        );
        let joined = block1 + &block2;
        let segments = segment_dsml_text(&joined, &tools("web_search_schema"));
        let queries = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::ToolUse(call) => call.input.get("query").and_then(Value::as_str),
                Segment::Text(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(queries, vec![q1, q2, q3]);
    }

    #[test]
    fn single_pipe_tool_calls_alias_matches_python() {
        let pipe = "｜";
        let block = format!(
            "<{pipe}DSML{pipe}tool_calls> <{pipe}DSML{pipe}invoke name=\"web_search\"><{pipe}DSML{pipe}parameter name=\"query\" string=\"true\">x</{pipe}DSML{pipe}parameter></{pipe}DSML{pipe}invoke> </{pipe}DSML{pipe}tool_calls>"
        );
        let segments = segment_dsml_text(&block, &tools("web_search_schema"));
        let Segment::ToolUse(call) = &segments[0] else {
            panic!("expected single-pipe tool segment");
        };
        assert_eq!(call.name, "web_search");
        assert_eq!(call.input, json!({"query": "x"}));
    }

    #[test]
    fn unknown_tool_keeps_whole_block_as_text() {
        let fixture = fixture();
        let text = fixture["unknown_tool_text"].as_str().unwrap();
        assert_eq!(
            segment_dsml_text(text, &tools("web_search_schema")),
            vec![Segment::Text(text.to_string())]
        );
    }

    #[test]
    fn mixed_known_unknown_whole_block_stays_text() {
        let pipe = "｜｜";
        let block = format!(
            "<{pipe}DSML{pipe}tool_calls> <{pipe}DSML{pipe}invoke name=\"web_search\"><{pipe}DSML{pipe}parameter name=\"query\" string=\"true\">x</{pipe}DSML{pipe}parameter></{pipe}DSML{pipe}invoke> <{pipe}DSML{pipe}invoke name=\"evil\"><{pipe}DSML{pipe}parameter name=\"cmd\" string=\"true\">rm -rf /</{pipe}DSML{pipe}parameter></{pipe}DSML{pipe}invoke> </{pipe}DSML{pipe}tool_calls>"
        );
        assert_eq!(
            segment_dsml_text(&block, &tools("web_search_schema")),
            vec![Segment::Text(block)]
        );
    }

    #[test]
    fn typed_params_match_python_contract() {
        let fixture = fixture();
        let calls = parse_dsml_tool_calls(
            fixture["typed_call"].as_str().unwrap(),
            &tools("numeric_schema"),
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input, fixture["expected_typed_input"]);
    }

    #[test]
    fn wrapper_unwrap_and_arguments_real_field_match_python() {
        let mut wrap_tool = Map::new();
        wrap_tool.insert(
            "do".to_string(),
            json!({"type": "object", "properties": {"x": {"type": "integer"}, "y": {"type": "string"}}}),
        );
        let block = wrap(
            "｜｜",
            "do",
            &[("arguments", " string=\"false\"", "{\"x\":1,\"y\":\"hi\"}")],
        );
        let calls = parse_dsml_tool_calls(&block, &wrap_tool);
        assert_eq!(calls[0].input, json!({"x": 1, "y": "hi"}));

        let mut arguments_field = Map::new();
        arguments_field.insert(
            "run".to_string(),
            json!({"type": "object", "properties": {"arguments": {"type": "string"}}}),
        );
        let block = wrap("｜｜", "run", &[("arguments", " string=\"true\"", "hello")]);
        let calls = parse_dsml_tool_calls(&block, &arguments_field);
        assert_eq!(calls[0].input, json!({"arguments": "hello"}));
    }

    #[test]
    fn required_type_mismatch_and_illegal_boolean_void_whole_block() {
        let mut required_tool = Map::new();
        required_tool.insert(
            "do".to_string(),
            json!({"type": "object", "properties": {"x": {"type": "integer"}}, "required": ["x"]}),
        );
        assert!(parse_dsml_tool_calls(
            &wrap("｜｜", "do", &[("y", " string=\"false\"", "1")]),
            &required_tool,
        )
        .is_empty());
        assert!(parse_dsml_tool_calls(
            &wrap("｜｜", "do", &[("x", " string=\"true\"", "not-an-int")]),
            &required_tool,
        )
        .is_empty());

        let mut bool_tool = Map::new();
        bool_tool.insert(
            "setflag".to_string(),
            json!({"type": "object", "properties": {"flag": {"type": "boolean"}}, "required": ["flag"]}),
        );
        for bad in ["maybe", "garbage", "2", "TrueFalseMaybe"] {
            assert!(parse_dsml_tool_calls(
                &wrap("｜｜", "setflag", &[("flag", "", bad)]),
                &bool_tool,
            )
            .is_empty());
        }
        for (raw, want) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("yes", true),
            ("false", false),
            ("0", false),
        ] {
            let calls =
                parse_dsml_tool_calls(&wrap("｜｜", "setflag", &[("flag", "", raw)]), &bool_tool);
            assert_eq!(calls[0].input, json!({"flag": want}));
        }
    }

    #[test]
    fn rewrite_nonstream_injects_stable_tool_use_and_stop_reason() {
        let fixture = fixture();
        let raw = response(fixture["wrapped_text"].as_str().unwrap());
        let out = rewrite_nonstream_body(&raw, &tools("web_search_schema"), "fixed");
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        let content = parsed["content"].as_array().unwrap();
        assert_eq!(
            content
                .iter()
                .map(|block| block["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["text", "tool_use", "text"]
        );
        assert_eq!(content[1]["id"], "toolu_dsml_fixed_1");
        assert_eq!(content[1]["name"], "web_search");
        assert_eq!(content[1]["input"], json!({"query": "q"}));
        assert_eq!(parsed["stop_reason"], "tool_use");
    }

    #[test]
    fn rewrite_nonstream_does_not_override_non_end_stop_reason() {
        let fixture = fixture();
        let mut raw: Value =
            serde_json::from_slice(&response(fixture["wrapped_text"].as_str().unwrap())).unwrap();
        raw["stop_reason"] = Value::String("max_tokens".to_string());
        let out = rewrite_nonstream_body(
            &serde_json::to_vec(&raw).unwrap(),
            &tools("web_search_schema"),
            "fixed",
        );
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["stop_reason"], "max_tokens");
        assert!(parsed["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["type"] == "tool_use"));
    }

    #[test]
    fn clean_or_unknown_or_bad_json_returns_verbatim_bytes() {
        let clean = br#"{"id":"m","type":"message","content":[{"type":"text","text":"caf\u00e9 \u4f60\u597d"}],"stop_reason":"end_turn"}"#;
        assert_eq!(
            rewrite_nonstream_body(clean, &tools("web_search_schema"), "t"),
            clean
        );
        assert_eq!(
            rewrite_nonstream_body(
                &response(fixture()["unknown_tool_text"].as_str().unwrap()),
                &tools("web_search_schema"),
                "t"
            ),
            response(fixture()["unknown_tool_text"].as_str().unwrap())
        );
        assert_eq!(
            rewrite_nonstream_body(b"not json", &tools("web_search_schema"), "t"),
            b"not json"
        );
    }
}
