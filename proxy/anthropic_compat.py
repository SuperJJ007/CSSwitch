"""Anthropic 透传路径「兼容层」（S1a）：暴露三入口给薄骨架。内部调 provider_policy + dsml_shim。

依赖方向：骨架 → 本模块 → provider_policy；本模块不反向 import csswitch_proxy（无循环依赖）。
三入口无状态可序列化 + nonce 可注入 + ProviderState 显式传参 → 为 S1b 跨语言接缝铺路。
"""
import json
from dataclasses import dataclass

import dsml_shim
import provider_policy

_EMPTY_OBJECT_SCHEMA = {"type": "object", "properties": {}}


@dataclass
class Ctx:
    """transform_request 产出、传给 rewrite_nonstream / make_stream_rewriter 的请求级上下文。"""
    src_model: str
    target_model: str
    known_tools: dict
    nonce: str
    shim_mode: str
    provider: str


def _normalize_relay_input_schema(schema):
    if not isinstance(schema, dict) or not schema:
        return dict(_EMPTY_OBJECT_SCHEMA)

    out = dict(schema)
    props = out.get("properties")
    typ = out.get("type")

    if typ is None and isinstance(props, dict):
        out["type"] = "object"
    elif typ != "object":
        out = dict(_EMPTY_OBJECT_SCHEMA)
        props = out["properties"]

    if not isinstance(out.get("properties"), dict):
        out["properties"] = {}
    if "required" in out and not isinstance(out["required"], list):
        out.pop("required", None)
    return out


def _degrade_missing_tool_choice(upstream):
    choice = upstream.get("tool_choice")
    if not (isinstance(choice, dict) and choice.get("type") == "tool"):
        return
    names = {t.get("name") for t in upstream.get("tools") or [] if isinstance(t, dict)}
    if choice.get("name") not in names:
        upstream["tool_choice"] = {"type": "auto"}


def _normalize_relay_tools(upstream):
    """Normalize Anthropic-compatible relay tool schemas before outbound.

    Some Anthropic-compatible relay providers reject Claude Science's empty or loose
    ``input_schema`` values with a provider-side 400. Keep this limited to relay
    passthrough; OpenAI/Qwen/Responses conversions have their own mapping rules.
    """
    tools = upstream.get("tools")
    if not isinstance(tools, list):
        if "tools" in upstream:
            upstream.pop("tools", None)
            _degrade_missing_tool_choice(upstream)
        return

    normalized = []
    for tool in tools:
        if not isinstance(tool, dict) or not tool.get("name"):
            continue
        clean = dict(tool)
        clean["input_schema"] = _normalize_relay_input_schema(tool.get("input_schema"))
        normalized.append(clean)
    if normalized:
        upstream["tools"] = normalized
    else:
        upstream.pop("tools", None)
    _degrade_missing_tool_choice(upstream)


def _filter_upstream_tools(upstream, target_model, provider):
    """Provider-specific tool compatibility before sending to upstream.

    Kimi's Anthropic endpoint treats a tool named ``web_search`` as its own server tool and
    streams ``server_tool_use`` / ``web_search_tool_result`` blocks. The local client path
    expects ordinary client tools, so those server-tool blocks make the stream
    retry. Keep the original known_tools in ctx, but do not advertise this one tool upstream.
    """
    if provider != "relay":
        return
    _normalize_relay_tools(upstream)
    if "kimi" in (target_model or "").lower():
        tools = upstream.get("tools")
        if isinstance(tools, list):
            filtered = [t for t in tools if not (isinstance(t, dict) and t.get("name") == "web_search")]
            if len(filtered) != len(tools):
                if filtered:
                    upstream["tools"] = filtered
                else:
                    upstream.pop("tools", None)
                _degrade_missing_tool_choice(upstream)


def transform_request(body, state):
    """(body, ProviderState) -> (upstream_body, Ctx)。纯函数：无网络、无全局读取。
    等价于旧 _handle_anthropic 的 :695-702 + :714-718。"""
    src = body.get("model", "?")
    target = provider_policy.resolve_model(src, state)
    upstream = dict(body)
    upstream["model"] = target
    if upstream.get("max_tokens"):
        upstream["max_tokens"] = provider_policy.clamp_max_tokens(
            upstream["max_tokens"], target, state)
    provider_policy.normalize_thinking(upstream, state.prov_name, state.relay_thinking)
    _filter_upstream_tools(upstream, target, state.prov_name)
    known_tools = {t["name"]: (t.get("input_schema") or {})
                   for t in (body.get("tools") or [])
                   if isinstance(t, dict) and t.get("name")}
    ctx = Ctx(src_model=src, target_model=target, known_tools=known_tools,
              nonce=state.nonce_factory(), shim_mode=state.shim_mode, provider=state.prov_name)
    return upstream, ctx


def _shim_on(ctx):
    return ctx.shim_mode in ("detect", "rewrite") and bool(ctx.known_tools)


def rewrite_nonstream(body_bytes, ctx):
    """(body_bytes, Ctx) -> (body_bytes, stats)。等价于旧 :771-780。
    off / 无工具：(原 bytes, {})；detect：(原 bytes, {"found": bool})；
    rewrite：(改写 bytes, {"rewritten": bool})。"""
    if not _shim_on(ctx):
        return body_bytes, {}
    if ctx.shim_mode == "rewrite":
        new = dsml_shim.rewrite_nonstream_body(body_bytes, ctx.known_tools, nonce=ctx.nonce)
        return new, {"rewritten": new != body_bytes}
    det = dsml_shim.DsmlDetector()
    det.feed(body_bytes)
    return body_bytes, {"found": det.found}


class _RewriteFilter:
    """rewrite 模式的流式 filter：包 DsmlStreamRewriter，暴露统一 feed/finalize/stats。"""

    def __init__(self, known_tools, nonce):
        self._rw = dsml_shim.DsmlStreamRewriter(known_tools, nonce=nonce)

    def feed(self, chunk):
        return self._rw.feed(chunk)

    def finalize(self):
        return self._rw.finalize()

    def stats(self):
        return {"synthesized": self._rw.synthesized, "tool_n": self._rw.tool_n}


class _DetectFilter:
    """detect 模式的流式 filter：原样透传 + 内部记 stats。"""

    def __init__(self):
        self._det = dsml_shim.DsmlDetector()

    def feed(self, chunk):
        self._det.feed(chunk)
        return chunk

    def finalize(self):
        return b""

    def stats(self):
        return {"found": self._det.found}


class _KimiServerToolFilter:
    """Drop Kimi server-tool SSE blocks that the local client cannot consume.

    Kimi may emit Anthropic server-tool blocks (currently web search) even when CSSwitch
    does not advertise that tool upstream. The client-tool path expects ordinary
    content blocks with contiguous indexes, so we remove those blocks and compact indexes.
    """

    _DROP_TYPES = {"server_tool_use", "web_search_tool_result"}

    def __init__(self):
        self._buf = b""
        self._skip = set()
        self._index_map = {}
        self._next_index = 0
        self._dropped = 0

    @staticmethod
    def _split_frame(buf):
        candidates = [(buf.find(b"\n\n"), 2), (buf.find(b"\r\n\r\n"), 4)]
        candidates = [(i, n) for i, n in candidates if i >= 0]
        if not candidates:
            return None, None, buf
        i, n = min(candidates, key=lambda x: x[0])
        return buf[:i], buf[i:i + n], buf[i + n:]

    @staticmethod
    def _event_and_data(frame):
        event = None
        data = []
        for line in frame.replace(b"\r\n", b"\n").split(b"\n"):
            if line.startswith(b"event:"):
                event = line.split(b":", 1)[1].strip()
            elif line.startswith(b"data:"):
                data.append(line.split(b":", 1)[1].lstrip())
        return event, b"\n".join(data)

    @staticmethod
    def _render(event, obj):
        data = json.dumps(obj, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        if event:
            return b"event: " + event + b"\n" + b"data: " + data + b"\n\n"
        return b"data: " + data + b"\n\n"

    def _mapped_index(self, idx):
        if idx not in self._index_map:
            self._index_map[idx] = self._next_index
            self._next_index += 1
        return self._index_map[idx]

    def _rewrite_frame(self, frame, sep):
        event, data = self._event_and_data(frame)
        if not data:
            return frame + sep
        try:
            obj = json.loads(data.decode("utf-8"))
        except Exception:
            return frame + sep
        if not isinstance(obj, dict):
            return frame + sep

        t = obj.get("type")
        if t == "content_block_start":
            idx = obj.get("index")
            block = obj.get("content_block") if isinstance(obj.get("content_block"), dict) else {}
            if block.get("type") in self._DROP_TYPES:
                self._skip.add(idx)
                self._dropped += 1
                return b""
            obj = dict(obj)
            obj["index"] = self._mapped_index(idx)
            return self._render(event, obj)
        if t in ("content_block_delta", "content_block_stop"):
            idx = obj.get("index")
            if idx in self._skip:
                return b""
            if idx in self._index_map:
                obj = dict(obj)
                obj["index"] = self._index_map[idx]
                return self._render(event, obj)
        return frame + sep

    def feed(self, chunk):
        self._buf += chunk
        out = []
        while True:
            frame, sep, rest = self._split_frame(self._buf)
            if frame is None:
                break
            self._buf = rest
            out.append(self._rewrite_frame(frame, sep))
        return b"".join(out)

    def finalize(self):
        if not self._buf:
            return b""
        frame = self._buf
        self._buf = b""
        return self._rewrite_frame(frame, b"\n\n")

    def stats(self):
        return {"dropped_kimi_server_tool_blocks": self._dropped}


class _PipelineFilter:
    def __init__(self, filters):
        self._filters = filters

    def feed(self, chunk):
        out = chunk
        for f in self._filters:
            out = f.feed(out)
        return out

    def finalize(self):
        out = b""
        for f in self._filters:
            out = f.feed(out) + f.finalize()
        return out

    def stats(self):
        out = {}
        for f in self._filters:
            out.update(f.stats())
        return out


def make_stream_rewriter(ctx):
    """(Ctx) -> stream_filter | None。off / 无工具 → None（骨架直接透传，零开销）。
    filter 统一接口：feed(chunk)->bytes / finalize()->bytes / stats()。等价于旧 :735-737。"""
    filters = []
    if ctx.provider == "relay" and "kimi" in (ctx.target_model or "").lower():
        filters.append(_KimiServerToolFilter())
    if _shim_on(ctx):
        if ctx.shim_mode == "rewrite":
            filters.append(_RewriteFilter(ctx.known_tools, ctx.nonce))
        else:
            filters.append(_DetectFilter())
    if not filters:
        return None
    if len(filters) == 1:
        return filters[0]
    return _PipelineFilter(filters)
