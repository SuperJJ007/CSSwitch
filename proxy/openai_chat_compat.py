"""OpenAI Chat Completions compatibility helpers."""

import json

import provider_policy


def map_tool_choice(tc, tools):
    if not isinstance(tc, dict):
        return None
    t = tc.get("type")
    if t == "auto":
        return "auto"
    if t == "none":
        return "none"
    if t == "tool" and tc.get("name"):
        return {"type": "function", "function": {"name": tc["name"]}}
    if t == "any":
        names = [x["name"] for x in (tools or []) if x.get("name")]
        if len(names) == 1:
            return {"type": "function", "function": {"name": names[0]}}
        return "required"
    return None


def anthropic_to_openai(req, state):
    msgs = []
    sys_prompt = req.get("system")
    if isinstance(sys_prompt, list):
        sys_prompt = "\n".join(b.get("text", "") for b in sys_prompt if isinstance(b, dict))
    if sys_prompt:
        msgs.append({"role": "system", "content": sys_prompt})
    for m in req.get("messages", []):
        role = m.get("role")
        content = m.get("content")
        if isinstance(content, str):
            msgs.append({"role": role, "content": content})
            continue
        text_parts, tool_calls, tool_results = [], [], []
        for blk in content or []:
            t = blk.get("type")
            if t == "text":
                text_parts.append(blk.get("text", ""))
            elif t == "tool_use":
                tool_calls.append({
                    "id": blk.get("id"),
                    "type": "function",
                    "function": {
                        "name": blk.get("name"),
                        "arguments": json.dumps(blk.get("input", {}), ensure_ascii=False),
                    },
                })
            elif t == "tool_result":
                c = blk.get("content")
                if isinstance(c, list):
                    c = "".join(x.get("text", "") for x in c if isinstance(x, dict))
                tool_results.append({
                    "role": "tool",
                    "tool_call_id": blk.get("tool_use_id"),
                    "content": c if isinstance(c, str) else json.dumps(c, ensure_ascii=False),
                })
        if role == "assistant" and tool_calls:
            msgs.append({
                "role": "assistant",
                "content": "".join(text_parts) or None,
                "tool_calls": tool_calls,
            })
        elif tool_results:
            msgs.extend(tool_results)
            if text_parts:
                msgs.append({"role": role, "content": "".join(text_parts)})
        else:
            msgs.append({"role": role, "content": "".join(text_parts)})
    out = {
        "model": provider_policy.resolve_model(req.get("model"), state),
        "messages": msgs,
        "stream": False,
    }
    if req.get("max_tokens"):
        out["max_tokens"] = provider_policy.clamp_max_tokens(
            req["max_tokens"],
            out["model"],
            state,
        )
    if req.get("temperature") is not None:
        out["temperature"] = req["temperature"]
    if req.get("tools"):
        out["tools"] = [{
            "type": "function",
            "function": {
                "name": t["name"],
                "description": t.get("description", ""),
                "parameters": t.get("input_schema", {}),
            },
        } for t in req["tools"] if t.get("name")]
    tcm = map_tool_choice(req.get("tool_choice"), req.get("tools"))
    if tcm is not None:
        out["tool_choice"] = tcm
    if req.get("stop_sequences"):
        out["stop"] = req["stop_sequences"]
    if req.get("top_p") is not None:
        out["top_p"] = req["top_p"]
    return out


def openai_to_anthropic(resp, model_id):
    choice = (resp.get("choices") or [{}])[0]
    msg = choice.get("message", {})
    blocks = []
    if msg.get("content"):
        blocks.append({"type": "text", "text": msg["content"]})
    for tc in msg.get("tool_calls") or []:
        fn = tc.get("function", {})
        try:
            args = json.loads(fn.get("arguments") or "{}")
        except Exception:
            args = {}
        blocks.append({
            "type": "tool_use",
            "id": tc.get("id"),
            "name": fn.get("name"),
            "input": args,
        })
    fr = choice.get("finish_reason")
    stop = {"stop": "end_turn", "length": "max_tokens", "tool_calls": "tool_use"}.get(
        fr,
        "end_turn",
    )
    usage = resp.get("usage", {})
    return {
        "id": resp.get("id", "msg_proxy"),
        "type": "message",
        "role": "assistant",
        "model": model_id,
        "content": blocks or [{"type": "text", "text": ""}],
        "stop_reason": stop,
        "stop_sequence": None,
        "usage": {
            "input_tokens": usage.get("prompt_tokens", 0),
            "output_tokens": usage.get("completion_tokens", 0),
        },
    }
