"""OpenAI Responses compatibility helpers.

The functions here are pure protocol transforms. Provider runtime state is
passed in by csswitch_proxy instead of read from process globals.
"""

import json

import provider_policy


def map_tool_choice(tc, tools):
    has_tools = bool(tools)
    if isinstance(tc, str):
        t = tc
    elif isinstance(tc, dict):
        t = tc.get("type")
    else:
        t = None
    if t == "auto":
        return "auto"
    if t == "none":
        return "none"
    if has_tools:
        return "auto"
    return None


def max_output_tokens(req, model, state, has_tools, is_dashscope=False):
    value = provider_policy.clamp_max_tokens(req.get("max_tokens"), model, state)
    if not value:
        return value
    if has_tools and is_dashscope:
        return min(int(value), 8192)
    return value


def is_dashscope_responses(prov):
    return (
        prov.get("api_format") == "openai_responses"
        and "dashscope.aliyuncs.com" in (prov.get("url") or "")
    )


def normalize_tool_parameters(schema):
    if not isinstance(schema, dict):
        return {"type": "object", "properties": {}}
    out = dict(schema)
    if "properties" in out and not out.get("type"):
        out["type"] = "object"
    if out.get("type") != "object":
        return {"type": "object", "properties": {}}
    if not isinstance(out.get("properties"), dict):
        out["properties"] = {}
    return out


def map_tools(tools, is_dashscope=False):
    out = []
    for t in tools or []:
        name = t.get("name")
        if not name:
            continue
        if is_dashscope and name == "web_search":
            continue
        out.append({
            "type": "function",
            "name": name,
            "description": t.get("description", ""),
            "parameters": normalize_tool_parameters(t.get("input_schema", {})),
        })
    return out


def _as_text(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "".join(x.get("text", "") for x in value if isinstance(x, dict))
    if value is None:
        return ""
    return json.dumps(value, ensure_ascii=False)


def anthropic_to_openai(req, state, is_dashscope=False):
    sys_prompt = req.get("system")
    if isinstance(sys_prompt, list):
        sys_prompt = "\n".join(b.get("text", "") for b in sys_prompt if isinstance(b, dict))

    items = []
    for m in req.get("messages", []):
        role = m.get("role")
        content = m.get("content")
        if isinstance(content, str):
            items.append({"role": role, "content": content})
            continue

        text_parts = []
        for blk in content or []:
            t = blk.get("type")
            if t == "text":
                text_parts.append(blk.get("text", ""))
            elif t == "tool_use":
                if text_parts:
                    items.append({"role": role, "content": "".join(text_parts)})
                    text_parts = []
                items.append({
                    "type": "function_call",
                    "call_id": blk.get("id"),
                    "name": blk.get("name"),
                    "arguments": json.dumps(blk.get("input", {}), ensure_ascii=False),
                })
            elif t == "tool_result":
                if text_parts:
                    items.append({"role": role, "content": "".join(text_parts)})
                    text_parts = []
                items.append({
                    "type": "function_call_output",
                    "call_id": blk.get("tool_use_id"),
                    "output": _as_text(blk.get("content")),
                })
        if text_parts:
            items.append({"role": role, "content": "".join(text_parts)})

    out = {
        "model": provider_policy.resolve_model(req.get("model"), state),
        "input": items,
        "stream": False,
    }
    if sys_prompt:
        out["instructions"] = sys_prompt
    tools = map_tools(req.get("tools"), is_dashscope)
    token_limit = max_output_tokens(req, out["model"], state, bool(tools), is_dashscope)
    if token_limit:
        out["max_output_tokens"] = token_limit
    if req.get("temperature") is not None:
        out["temperature"] = req["temperature"]
    if req.get("top_p") is not None:
        out["top_p"] = req["top_p"]
    if tools:
        out["tools"] = tools
    tcm = map_tool_choice(req.get("tool_choice"), tools)
    if tcm is not None:
        out["tool_choice"] = tcm
    return out


def output_text(item):
    parts = []
    for c in item.get("content") or []:
        if not isinstance(c, dict):
            continue
        if c.get("type") in ("output_text", "text"):
            parts.append(c.get("text", ""))
    return "".join(parts)


def openai_to_anthropic(resp, model_id):
    blocks = []
    for item in resp.get("output") or []:
        if not isinstance(item, dict):
            continue
        t = item.get("type")
        if t == "message":
            text = output_text(item)
            if text:
                blocks.append({"type": "text", "text": text})
        elif t == "function_call":
            raw_args = item.get("arguments") or "{}"
            try:
                args = json.loads(raw_args)
            except Exception:
                args = {}
            blocks.append({
                "type": "tool_use",
                "id": item.get("call_id") or item.get("id"),
                "name": item.get("name"),
                "input": args,
            })
    if not blocks and resp.get("output_text"):
        blocks.append({"type": "text", "text": resp.get("output_text", "")})
    usage = resp.get("usage", {})
    stop = "tool_use" if any(b.get("type") == "tool_use" for b in blocks) else "end_turn"
    if resp.get("status") == "incomplete":
        stop = "max_tokens"
    return {
        "id": resp.get("id", "msg_proxy"),
        "type": "message",
        "role": "assistant",
        "model": model_id,
        "content": blocks or [{"type": "text", "text": ""}],
        "stop_reason": stop,
        "stop_sequence": None,
        "usage": {
            "input_tokens": usage.get("input_tokens", 0),
            "output_tokens": usage.get("output_tokens", 0),
        },
    }
