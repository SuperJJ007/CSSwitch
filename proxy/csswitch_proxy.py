#!/usr/bin/env python3
"""CSSwitch 代理：把 Claude Science 的推理转发到第三方模型（provider 可切）。

Providers:
  deepseek (默认)：https://api.deepseek.com/anthropic —— DeepSeek 原生 Anthropic 端点，
                   代理只做「透传 + 改模型名 + 换鉴权头 + max_tokens 夹取 + 连接重试」，
                   thinking/tool_use 全部原生保真（不翻译协议）。
  qwen           ：DashScope compatible-mode —— Anthropic↔OpenAI 双向翻译（流式以 SSE 回放保真 tool_use）。

安全约束：
  - 入站 Authorization / x-api-key（Science 带来的 OAuth Bearer）一律剥离，不记录、不转发。
  - 上游只用本地环境变量里的 provider key，值只驻内存，不打印、不写日志。
  - 只监听回环地址；除所选 provider 端点外不外连。

用法：
  DEEPSEEK_API_KEY=... python3 csswitch_proxy.py --provider deepseek --port 18991
  DASHSCOPE_API_KEY=... python3 csswitch_proxy.py --provider qwen --port 18991
"""
import argparse
import json
import os
import re
import sys
import time
import urllib.request
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# ---------- provider 注册表 ----------
PROVIDERS = {
    "deepseek": {
        "mode": "anthropic",
        "url": "https://api.deepseek.com/anthropic/v1/messages",
        "key_env": "DEEPSEEK_API_KEY",
        # 选择器里展示的可选模型。
        # 注意：Science 模型面板对可选项有两道硬规则（二进制 s0/ZjO/XjO/hB_）：
        #   1) id 必须以 claude- 开头（s0）；
        #   2) 只有 id 形如 claude-{opus|sonnet|haiku}-<数字...>（family+纯数字版本）才进【主列表】，
        #      每个 family 只留一个；其余一律塞进「More models」折叠区（overflow:true）。
        # 因此这里【借用】Science 认可的主列表 id（opus/haiku），显示名仍写 DeepSeek，
        # 由 model_map 映射回真实 DeepSeek id。这样两个模型都直接平铺在选择器里，无需展开 More models。
        #   claude-opus-4-8  → 显示「DeepSeek V4 Pro」  （tier0，且是 Science 的默认模型 id）
        #   claude-haiku-4-5 → 显示「DeepSeek V4 Flash」（tier2）
        "models": [
            ("claude-opus-4-8", "DeepSeek V4 Pro"),
            ("claude-haiku-4-5", "DeepSeek V4 Flash"),
        ],
        "model_map": {
            # 选择器里选中的 / Science 硬编码的 claude-*（标题用 haiku、正式推理用 opus）→ 真实 deepseek id
            "claude-opus-4-8": "deepseek-v4-pro",
            "claude-sonnet-5": "deepseek-v4-flash",
            "claude-sonnet-4-6": "deepseek-v4-flash",
            "claude-haiku-4-5": "deepseek-v4-flash",
        },
        # 每模型输出上限。provisional：待 §12.3 拉官方模型列表核对真实上限后校准。
        "model_caps": {
            "deepseek-v4-pro": 65536,
            "deepseek-v4-flash": 32768,
        },
        "default_cap": 8192,
        "default_model": "deepseek-v4-flash",
    },
    "qwen": {
        "mode": "openai",
        "url": "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
        "key_env": "DASHSCOPE_API_KEY",
        "models": [
            ("qwen-max", "Qwen Max"),
            ("qwen-plus", "Qwen Plus"),
            ("qwen-turbo", "Qwen Turbo"),
        ],
        "model_map": {
            "claude-opus-4-8": "qwen-max",
            "claude-sonnet-5": "qwen-plus",
            "claude-sonnet-4-6": "qwen-plus",
            "claude-haiku-4-5": "qwen-turbo",
        },
        # provisional：待核对 DashScope 各模型真实上限。
        "model_caps": {
            "qwen-max": 8192,
            "qwen-plus": 8192,
            "qwen-turbo": 8192,
        },
        "default_cap": 8192,
        "default_model": "qwen-plus",
    },
}

PROV = None      # 当前 provider 配置（dict），运行时设定
KEY = None       # 当前 provider 的 key，只驻内存
LOG = None
PROV_NAME = None  # 运行时设定；模块被 import 做测试时也要有定义，避免 handler NameError
AUTH_SECRET = None  # 未设则不启用鉴权（保持旧行为）
_DATE_SUFFIX = re.compile(r"-\d{8}$")


def log(msg):
    line = f"[{time.strftime('%H:%M:%S')}] {msg}"
    print(line, flush=True)
    if LOG:
        with open(LOG, "a") as f:
            f.write(line + "\n")


def load_key(prov, args):
    env = prov["key_env"]
    if os.environ.get(env):
        return os.environ[env].strip()
    if args.env_file and os.path.isfile(args.env_file):
        for raw in open(args.env_file):
            raw = raw.strip()
            if not raw or raw.startswith("#") or "=" not in raw:
                continue
            k, v = raw.split("=", 1)
            if k.strip() == env:
                return v.strip().strip('"').strip("'")
    return None


def resolve_model(name):
    """把 Science 传来的模型名解析成当前 provider 的目标模型。
    优先：选择器里选中的 provider 原生名 > 显式映射 > 去日期后缀 > 前缀匹配 > 默认。"""
    if not name:
        return PROV["default_model"]
    mm = PROV["model_map"]
    if name in mm:          # 先查映射（覆盖伪 claude- 前缀的选择器 id 和 Science 硬编码 claude-*）
        return mm[name]
    ids = {m[0] for m in PROV["models"]}
    if name in ids:         # provider 原生 id（如 qwen-max）直接用
        return name
    stripped = _DATE_SUFFIX.sub("", name)
    if stripped in mm:
        return mm[stripped]
    for k, v in mm.items():
        if name.startswith(k) or stripped.startswith(k):
            return v
    return PROV["default_model"]


def clamp_max_tokens(v, model=None):
    if not v:
        return v
    caps = PROV.get("model_caps") or {}
    cap = caps.get(model, PROV.get("default_cap"))
    if cap:
        return min(int(v), cap)
    return v


def http_post(url, data, headers, attempts=4, timeout=300):
    """POST 上游；重试覆盖【连接 + 完整读体】（含 SSL EOF、握手超时、对端断开、IncompleteRead），
    对服务端明确响应（HTTPError，如 400）不重试。返回 (body_bytes, content_type)。"""
    for i in range(attempts):
        req = urllib.request.Request(url, data=data, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.read(), r.headers.get("Content-Type", "application/json")
        except urllib.error.HTTPError:
            raise
        except Exception as e:
            if i < attempts - 1:
                log(f"  ~ 上游连接抖动，重试 {i + 1}/{attempts - 1}: {e}")
                time.sleep(0.8 * (i + 1))
                continue
            raise


def open_stream(url, data, headers, attempts=4, timeout=300):
    """打开上游流式连接并预读首块（把「200 但立刻空体」这种抖动也纳入重试）。
    返回 (resp, first_chunk, content_type)；首字节到手后不再重试。"""
    for i in range(attempts):
        req = urllib.request.Request(url, data=data, headers=headers)
        try:
            r = urllib.request.urlopen(req, timeout=timeout)
            first = r.read(4096)
            if not first:
                r.close()
                raise ConnectionError("上游 200 但立刻空体")
            return r, first, r.headers.get("Content-Type", "application/json")
        except urllib.error.HTTPError:
            raise
        except Exception as e:
            if i < attempts - 1:
                log(f"  ~ 上游连接抖动，重试 {i + 1}/{attempts - 1}: {e}")
                time.sleep(0.8 * (i + 1))
                continue
            raise


# ---------- Anthropic -> OpenAI 翻译（qwen 路径） ----------
def anthropic_to_openai(req):
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
                    "id": blk.get("id"), "type": "function",
                    "function": {"name": blk.get("name"),
                                 "arguments": json.dumps(blk.get("input", {}), ensure_ascii=False)},
                })
            elif t == "tool_result":
                c = blk.get("content")
                if isinstance(c, list):
                    c = "".join(x.get("text", "") for x in c if isinstance(x, dict))
                tool_results.append({"role": "tool", "tool_call_id": blk.get("tool_use_id"),
                                     "content": c if isinstance(c, str) else json.dumps(c, ensure_ascii=False)})
        if role == "assistant" and tool_calls:
            msgs.append({"role": "assistant", "content": "".join(text_parts) or None, "tool_calls": tool_calls})
        elif tool_results:
            msgs.extend(tool_results)
            if text_parts:
                msgs.append({"role": role, "content": "".join(text_parts)})
        else:
            msgs.append({"role": role, "content": "".join(text_parts)})
    out = {"model": resolve_model(req.get("model")), "messages": msgs, "stream": False}
    if req.get("max_tokens"):
        out["max_tokens"] = clamp_max_tokens(req["max_tokens"], out["model"])
    if req.get("temperature") is not None:
        out["temperature"] = req["temperature"]
    if req.get("tools"):
        out["tools"] = [{"type": "function",
                         "function": {"name": t["name"], "description": t.get("description", ""),
                                      "parameters": t.get("input_schema", {})}}
                        for t in req["tools"] if t.get("name")]
    tcm = map_tool_choice(req.get("tool_choice"), req.get("tools"))
    if tcm is not None:
        out["tool_choice"] = tcm
    if req.get("stop_sequences"):
        out["stop"] = req["stop_sequences"]
    if req.get("top_p") is not None:
        out["top_p"] = req["top_p"]
    return out


def map_tool_choice(tc, tools):
    """把 Anthropic tool_choice 译成 OpenAI 兼容取值。
    any 不做通用映射：单工具直接指定该函数（等效强制且不依赖 required）；
    多工具退 "required"（DashScope 若不支持会以上游错误显式暴露，不静默退化）。"""
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
        blocks.append({"type": "tool_use", "id": tc.get("id"), "name": fn.get("name"), "input": args})
    fr = choice.get("finish_reason")
    stop = {"stop": "end_turn", "length": "max_tokens", "tool_calls": "tool_use"}.get(fr, "end_turn")
    usage = resp.get("usage", {})
    return {
        "id": resp.get("id", "msg_proxy"), "type": "message", "role": "assistant", "model": model_id,
        "content": blocks or [{"type": "text", "text": ""}],
        "stop_reason": stop, "stop_sequence": None,
        "usage": {"input_tokens": usage.get("prompt_tokens", 0),
                  "output_tokens": usage.get("completion_tokens", 0)},
    }


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "csswitch-proxy"

    def log_message(self, *a):
        pass

    def _send_json(self, code, obj):
        body = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _sse(self, event, data):
        chunk = f"event: {event}\ndata: {json.dumps(data, ensure_ascii=False)}\n\n".encode()
        self.wfile.write(hex(len(chunk))[2:].encode() + b"\r\n" + chunk + b"\r\n")

    def _auth_ok(self):
        if not AUTH_SECRET:
            return True
        prefix = "/" + AUTH_SECRET
        if self.path == prefix or self.path.startswith(prefix + "/"):
            self.path = self.path[len(prefix):] or "/"
            return True
        self._send_json(403, {"type": "error", "error": {
            "type": "permission_error", "message": "forbidden"}})
        return False

    def do_GET(self):
        if not self._auth_ok():
            return
        if self.path.startswith("/v1/models"):
            data = [{"type": "model", "id": mid, "display_name": disp,
                     "created_at": "2026-01-01T00:00:00Z"} for mid, disp in PROV["models"]]
            log(f"GET /v1/models -> {PROV_NAME}: {', '.join(m[0] for m in PROV['models'])}")
            self._send_json(200, {"data": data, "has_more": False,
                                  "first_id": data[0]["id"], "last_id": data[-1]["id"]})
        elif self.path.startswith("/health"):
            self._send_json(200, {"status": "ok", "provider": PROV_NAME})
        else:
            self._send_json(404, {"type": "error", "error": {"type": "not_found_error", "message": self.path}})

    def do_POST(self):
        if not self._auth_ok():
            return
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b"{}"
        if not self.path.startswith("/v1/messages"):
            self._send_json(404, {"type": "error", "error": {"type": "not_found_error", "message": self.path}})
            return
        try:
            areq = json.loads(raw)
        except Exception as e:
            self._send_json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": str(e)}})
            return
        _dd = os.environ.get("PROXY_DUMP_REQ")
        if _dd:
            try:
                with open(os.path.join(_dd, f"req_{areq.get('model','x')}_{len(raw)}.json"), "w") as _f:
                    json.dump({"model": areq.get("model"), "thinking": areq.get("thinking"),
                               "tool_choice": areq.get("tool_choice"),
                               "n_tools": len(areq.get("tools") or [])}, _f, ensure_ascii=False, indent=2)
            except Exception:
                pass
        if PROV["mode"] == "anthropic":
            self._handle_anthropic(areq)
        else:
            self._handle_openai(areq)

    # ---- DeepSeek：Anthropic 原生透传（改模型名+换鉴权+夹 max_tokens+重试） ----
    def _handle_anthropic(self, areq):
        src = areq.get("model", "?")
        target = resolve_model(src)
        body = dict(areq)
        body["model"] = target
        if body.get("max_tokens"):
            body["max_tokens"] = clamp_max_tokens(body["max_tokens"], target)
        # DeepSeek 的 thinking 归一化：
        #  - 强制 tool_choice（any/tool，如标题/verdict 生成）：必须显式关 thinking。
        #    注意 DeepSeek flash 默认 thinking 开，即使请求里 thinking=null 也会与强制工具冲突，故无条件置 disabled。
        #  - 否则若 thinking.type=="auto"（Science 发的）→ "adaptive"（DeepSeek 只认 adaptive/enabled/disabled）。
        tc = body.get("tool_choice")
        forcing = isinstance(tc, dict) and tc.get("type") in ("any", "tool")
        if forcing:
            body["thinking"] = {"type": "disabled"}
        else:
            th = body.get("thinking")
            if isinstance(th, dict) and th.get("type") == "auto":
                th = dict(th)
                th["type"] = "adaptive"
                body["thinking"] = th
        stream = bool(body.get("stream"))
        n_tools = len(body.get("tools") or [])
        log(f"POST /v1/messages  {src}->{target} stream={stream} tools={n_tools} "
            f"msgs={len(body.get('messages') or [])}  (入站鉴权已剥离, 直连 {PROV_NAME})")
        headers = {"x-api-key": KEY, "content-type": "application/json", "anthropic-version": "2023-06-01"}
        data = json.dumps(body).encode()
        try:
            if stream:
                r, first, ct = open_stream(PROV["url"], data, headers)
                with r:
                    self.send_response(200)
                    self.send_header("Content-Type", ct)
                    self.send_header("Cache-Control", "no-cache")
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    self.wfile.write(hex(len(first))[2:].encode() + b"\r\n" + first + b"\r\n")
                    while True:
                        chunk = r.read(4096)
                        if not chunk:
                            break
                        self.wfile.write(hex(len(chunk))[2:].encode() + b"\r\n" + chunk + b"\r\n")
                    self.wfile.write(b"0\r\n\r\n")
                log(f"  <- {PROV_NAME} 流式透传 OK")
            else:
                body_bytes, ct = http_post(PROV["url"], data, headers)
                self.send_response(200)
                self.send_header("Content-Type", ct)
                self.send_header("Content-Length", str(len(body_bytes)))
                self.end_headers()
                self.wfile.write(body_bytes)
                log(f"  <- {PROV_NAME} 非流式透传 OK")
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", "replace")[:400]
            log(f"  !! 上游 HTTP {e.code}: {detail}")
            self._send_json(502, {"type": "error", "error": {"type": "api_error",
                                  "message": f"upstream {e.code}: {detail}"}})
        except Exception as e:
            log(f"  !! 代理异常: {e}")
            self._send_json(502, {"type": "error", "error": {"type": "api_error", "message": str(e)}})

    # ---- Qwen：翻译到 OpenAI，非流式取全再按需 SSE 回放 ----
    def _handle_openai(self, areq):
        model_id = areq.get("model", "claude-sonnet-5")
        stream = bool(areq.get("stream"))
        oreq = anthropic_to_openai(areq)
        n_tools = len(oreq.get("tools", []))
        log(f"POST /v1/messages  {model_id}->{oreq['model']} stream={stream} tools={n_tools} "
            f"msgs={len(oreq['messages'])}  (入站鉴权已剥离, {PROV_NAME})")
        headers = {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}
        data = json.dumps(oreq).encode()
        try:
            raw, _ct = http_post(PROV["url"], data, headers)
            oresp = json.loads(raw)
            aresp = openai_to_anthropic(oresp, model_id)
            if stream:
                self._replay_as_sse(aresp)
            else:
                self._send_json(200, aresp)
            log(f"  <- {PROV_NAME} OK (blocks={len(aresp['content'])} stop={aresp['stop_reason']})")
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", "replace")[:400]
            log(f"  !! 上游 HTTP {e.code}: {detail}")
            self._send_json(502, {"type": "error", "error": {"type": "api_error",
                                  "message": f"upstream {e.code}: {detail}"}})
        except Exception as e:
            log(f"  !! 代理异常: {e}")
            self._send_json(502, {"type": "error", "error": {"type": "api_error", "message": str(e)}})

    def _replay_as_sse(self, aresp):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        blocks = aresp.get("content") or [{"type": "text", "text": ""}]
        self._sse("message_start", {"type": "message_start", "message": {
            "id": aresp.get("id", "msg_proxy"), "type": "message", "role": "assistant",
            "model": aresp.get("model"), "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": aresp.get("usage", {"input_tokens": 0, "output_tokens": 0})}})
        self._sse("ping", {"type": "ping"})
        for idx, blk in enumerate(blocks):
            if blk.get("type") == "tool_use":
                self._sse("content_block_start", {"type": "content_block_start", "index": idx,
                          "content_block": {"type": "tool_use", "id": blk.get("id"),
                                            "name": blk.get("name"), "input": {}}})
                self._sse("content_block_delta", {"type": "content_block_delta", "index": idx,
                          "delta": {"type": "input_json_delta",
                                    "partial_json": json.dumps(blk.get("input", {}), ensure_ascii=False)}})
            else:
                self._sse("content_block_start", {"type": "content_block_start", "index": idx,
                          "content_block": {"type": "text", "text": ""}})
                self._sse("content_block_delta", {"type": "content_block_delta", "index": idx,
                          "delta": {"type": "text_delta", "text": blk.get("text", "")}})
            self._sse("content_block_stop", {"type": "content_block_stop", "index": idx})
        self._sse("message_delta", {"type": "message_delta",
                  "delta": {"stop_reason": aresp.get("stop_reason", "end_turn"), "stop_sequence": None},
                  "usage": {"output_tokens": aresp.get("usage", {}).get("output_tokens", 0)}})
        self._sse("message_stop", {"type": "message_stop"})
        self.wfile.write(b"0\r\n\r\n")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--provider", default=os.environ.get("CSSWITCH_PROVIDER", "deepseek"),
                    choices=list(PROVIDERS.keys()))
    ap.add_argument("--port", type=int, default=18991)
    ap.add_argument("--env-file", default=None)
    ap.add_argument("--log", default=None)
    ap.add_argument("--auth-token", default=None)
    args = ap.parse_args()
    PROV_NAME = args.provider
    PROV = PROVIDERS[PROV_NAME]
    LOG = args.log
    KEY = load_key(PROV, args)
    AUTH_SECRET = os.environ.get("CSSWITCH_AUTH_TOKEN") or args.auth_token
    _up = os.environ.get("CSSWITCH_UPSTREAM_URL")
    if _up:
        PROV = dict(PROV)
        PROV["url"] = _up
    if not KEY:
        print(f"找不到 {PROV['key_env']}。用环境变量或 --env-file <路径> 提供。", file=sys.stderr)
        sys.exit(1)
    log(f"CSSwitch 代理启动 127.0.0.1:{args.port}  provider={PROV_NAME}  "
        f"key=已加载(未显示)  上游={PROV['url']}")
    ThreadingHTTPServer(("127.0.0.1", args.port), H).serve_forever()
