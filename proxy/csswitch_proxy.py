#!/usr/bin/env python3
"""CSSwitch 代理：把 Claude Science 的推理转发到第三方模型（provider 可切）。

Providers:
  deepseek (默认)：https://api.deepseek.com/anthropic —— DeepSeek 原生 Anthropic 端点，
                   代理只做「透传 + 改模型名 + 换鉴权头 + max_tokens 夹取 + 连接重试」，
                   thinking/tool_use 全部原生保真（不翻译协议）。
  qwen           ：DashScope compatible-mode —— Anthropic↔OpenAI 双向翻译（流式以 SSE 回放保真 tool_use）。
  openai-custom  ：任意 OpenAI Chat Completions 兼容端点，base_url + token + model。
                   代理拼 /chat/completions 与 /models，并复用 qwen 的 Anthropic↔OpenAI 翻译链。
  openai-responses：任意 OpenAI Responses 兼容端点，base_url + token + model。
                   代理拼 /responses 与 /models，并做 Anthropic↔Responses 翻译。
  relay          ：任意「中转站」（Anthropic 兼容端点，base_url + token）。原生透传、【不重映射模型】，
                   /v1/models 回源直拉让 Science 选择器自动铺满中转站真实模型。base_url 经
                   CSSWITCH_RELAY_BASE_URL 提供、token 经 CSSWITCH_RELAY_KEY 提供。

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
import queue
import re
import select
import socket
import sys
import threading
import time
import urllib.request
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import dsml_shim
import model_discovery
import openai_chat_compat
import provider_policy
import responses_compat
import anthropic_compat

# DSML 兜底 shim 的运行模式：off（默认，字节级透传）/ detect（透传 + 遥测）/ rewrite（改写）。
# 由 __main__ 依 shim_mode(PROV_NAME, PROV) 覆写（读环境变量 CSSWITCH_TOOLUSE_SHIM）。
SHIM_MODE = "off"

# ---------- provider 注册表 ----------
PROVIDERS = {
    "deepseek": {
        "mode": "anthropic",
        "dsml_capable": True,   # 只有 DeepSeek 打开 DSML 兜底 shim（relay 需显式确认）
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
    "openai-custom": {
        "mode": "openai",
        "api_format": "openai_chat",
        "url": None,
        "models_url": None,
        "key_env": "CSSWITCH_OPENAI_KEY",
        "auth_style": "bearer",
        "force_model_override": True,
        "models": [],
        "model_map": {},
        "model_caps": {},
        "default_cap": None,
        "default_model": "",
    },
    "openai-responses": {
        "mode": "openai",
        "api_format": "openai_responses",
        "url": None,
        "models_url": None,
        "key_env": "CSSWITCH_OPENAI_KEY",
        "auth_style": "bearer",
        "force_model_override": True,
        "models": [],
        "model_map": {},
        "model_caps": {},
        # DashScope Responses rejects values above 65536; generic Responses-compatible
        # endpoints commonly accept this as a safe ceiling. Chat Completions custom
        # intentionally stays unclamped.
        "default_cap": 65536,
        "default_model": "",
    },
    "relay": {
        # 「中转站」：任意 Anthropic 兼容端点（base_url + token）。原生透传、【不重映射模型】
        # ——中转站原生认 claude-* 名。上游 url / models_url 在 __main__ 里按 CSSWITCH_RELAY_BASE_URL
        # 装配（base + /v1/messages、base + /v1/models）。
        "mode": "anthropic",       # 复用原生透传 handler（流式/非流式/重试都现成）
        "url": None,               # __main__ 装配
        "models_url": None,        # __main__ 装配；存在即 /v1/models 回源直拉
        "key_env": "CSSWITCH_RELAY_KEY",
        "passthrough": True,       # resolve_model 原样透传模型名（不映射）
        "force_model_override": True,
        "auth_style": "both",      # 同时带 x-api-key + Authorization: Bearer（最大兼容各家中转站）
        "models": [],              # 回源拉取，静态为空
        "model_map": {},
        "model_caps": {},
        "default_cap": None,       # 不夹 max_tokens：尊重中转站真实（claude 原生）上限
        # 空名兜底：Science 硬编码的默认推理模型 id（中转站基本都提供）。
        "default_model": "claude-opus-4-8",
    },
}

PROV = None      # 当前 provider 配置（dict），运行时设定
KEY = None       # 当前 provider 的 key，只驻内存
LOG = None
PROV_NAME = None  # 运行时设定；模块被 import 做测试时也要有定义，避免 handler NameError
AUTH_SECRET = None  # 未设则不启用鉴权（保持旧行为）
# relay 模式：最近一次 /v1/models 回源拉到的上游模型 id 列表。resolve_model 用它把
# Science 发来的裸 id（如标题 agent 的 claude-haiku-4-5）贴合到中转站真实 id
# （如 claude-haiku-4-5-20251001）。首拉前为空 → 纯透传。
RELAY_MODELS = []
# 强制模型 override：面板选了模型时，代理无条件把所有请求模型改成它（relay 覆盖透传；
# openai-custom/openai-responses 覆盖 Science 发来的 claude-* 壳）。由 CSSWITCH_RELAY_MODEL 或
# CSSWITCH_OPENAI_MODEL 环境变量在 __main__ 里装配；留空 → None。
RELAY_FORCE_MODEL = None
# relay thinking 策略（来自模板 thinking_policy）：由 CSSWITCH_RELAY_THINKING 在 __main__ 装配。
# None/"adaptive" → auto→adaptive（现状，如 MiniMax）；"enabled" → 强制 enabled（如 Kimi）。
RELAY_THINKING = None
# 出站 User-Agent：部分中转站的 WAF 把默认的 "Python-urllib/x.y" 判为 bot 直接 403
# （byteswarm 实测），故所有上游请求统一带一个非 bot 的 UA。
UPSTREAM_UA = "CSSwitch/0.2 (+https://github.com/SuperJJ007/CSSwitch)"

# ---------- #3: targeted fast-fail（沙箱「Switching organization」卡死修复） ----------
# 沙箱 Science 启动时会对 claude.ai/api/oauth/profile 发【阻塞式】请求解析组织；
# 在到不了 claude.ai 的网络上超时重试 → UI 卡在 "Switching organization"。
# 起沙箱时把 http(s)_proxy 指向本代理（见 launch-virtual-sandbox.sh），do_CONNECT
# 对下列 Anthropic 域名的 CONNECT 立即短路，其余域名正常隧道透传（保留装包 / MCP 等外联）。
# 推理仍走 127.0.0.1（no_proxy 直连本地）。
#
# 【为何回 401 而非 403】operon 的 claudeAiFetch 读的是 CONNECT 的状态码：
#   - 401 Unauthorized = 「你没登录」→ operon 打日志 `treating as logged-out` 并秒过（实测/根因确认）。
#   - 403 Forbidden    = 「登录了但没权限」→ operon 当成组织/权限问题【反复重试】→ 一直卡
#     "Switching organization"（v0.1.4 实机复现：server 日志固定 `/api/oauth/profile → 403`，
#     无 `treating as logged-out`）。
# 虚拟登录本就该表现为「未登录」，故用 401。
_BLOCKED_SUFFIXES = ("anthropic.com", "claude.ai", "claude.com")


def _is_blocked_host(host):
    h = host.lower().rstrip(".")
    return any(h == s or h.endswith("." + s) for s in _BLOCKED_SUFFIXES)


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


def _provider_state(areq):
    """从模块全局一次性组装 ProviderState（骨架侧），传给 compat / policy。
    nonce_factory 捕获 areq，保留旧 id(areq) 派生（字节级等价）。"""
    return provider_policy.ProviderState(
        policy=provider_policy.policy_from_prov(PROV),
        prov_name=PROV_NAME,
        relay_force_model=RELAY_FORCE_MODEL,
        relay_models=RELAY_MODELS,
        relay_thinking=RELAY_THINKING,
        shim_mode=SHIM_MODE,
        nonce_factory=lambda: f"{id(areq) & 0xffffff:x}",
    )


def http_post(url, data, headers, attempts=4, timeout=300):
    """POST 上游；重试覆盖【连接 + 完整读体】（含 SSL EOF、握手超时、对端断开、IncompleteRead），
    对服务端明确响应（HTTPError，如 400）不重试。返回 (body_bytes, content_type)。"""
    headers = {"User-Agent": UPSTREAM_UA, **headers}
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
    """打开上游流式连接并预读首行（把「200 但立刻空体」这种抖动也纳入重试）。
    返回 (resp, first_chunk, content_type)；首字节到手后不再重试。"""
    headers = {"User-Agent": UPSTREAM_UA, **headers}
    for i in range(attempts):
        req = urllib.request.Request(url, data=data, headers=headers)
        try:
            r = urllib.request.urlopen(req, timeout=timeout)
            first = r.readline(65536)
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


def _open_stream_with_keepalive(write_chunk, url, data, headers):
    """等待上游首帧时持续给下游发 SSE 注释心跳。

    下游主请求可能带大量工具定义；部分上游首帧 TTFT 较长时，如果下游在这段
    时间完全收不到 body 字节，会先断开并重试。注释帧是合法 SSE，客户端应忽略内容，
    但能证明连接仍活着。"""
    q = queue.Queue(maxsize=1)

    def _open():
        try:
            q.put(("ok", open_stream(url, data, headers)))
        except BaseException as e:
            q.put(("err", e))

    threading.Thread(target=_open, daemon=True).start()
    keepalive = b": csswitch-keepalive\n\n"
    while True:
        try:
            kind, payload = q.get(timeout=1.0)
            if kind == "err":
                raise payload
            return payload
        except queue.Empty:
            write_chunk(keepalive)


def http_get_json(url, headers, attempts=3, timeout=30):
    """GET 上游并解析 JSON（relay 回源拉 /v1/models 用）。连接抖动重试，服务端明确响应不重试。"""
    headers = {"User-Agent": UPSTREAM_UA, **headers}
    for i in range(attempts):
        req = urllib.request.Request(url, headers=headers, method="GET")
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return json.loads(r.read())
        except urllib.error.HTTPError:
            raise
        except Exception as e:
            if i < attempts - 1:
                log(f"  ~ 上游连接抖动，重试 {i + 1}/{attempts - 1}: {e}")
                time.sleep(0.6 * (i + 1))
                continue
            raise


def normalize_openai_base(base):
    """OpenAI 兼容端点存 base root。用户若误填到 /chat/completions 或 /models，
    这里收敛回 root，避免后续双拼。"""
    b = (base or "").strip().rstrip("/")
    for suffix in ("/v1/chat/completions", "/chat/completions",
                   "/v1/responses", "/responses", "/v1/models", "/models"):
        if b.endswith(suffix):
            b = b[: -len(suffix)].rstrip("/")
    return b


def _ends_with_version_segment(base):
    return bool(re.search(r"/v\d+(?:\.\d+)?$", base))


def openai_endpoint(base, suffix):
    root = normalize_openai_base(base)
    if not _ends_with_version_segment(root):
        root = root + "/v1"
    return root + suffix


def _upstream_auth_headers():
    """上游鉴权头：按当前 provider 的 auth_style 装 x-api-key / bearer / both。
    deepseek 未设 → 默认 x-api-key（保持原状）；relay = both。"""
    style = PROV.get("auth_style", "x-api-key")
    h = {}
    if style in ("x-api-key", "both"):
        h["x-api-key"] = KEY
    if style in ("bearer", "both"):
        h["Authorization"] = f"Bearer {KEY}"
    return h


def fetch_relay_models():
    """回源拉上游模型列表，归一化成 Science 可消费的模型列表。relay 会刷新
    RELAY_MODELS 缓存（供 resolve_model 贴合）；openai-custom 仅用于模型发现。返回 list（可空）。"""
    global RELAY_MODELS
    murl = PROV.get("models_url")
    if not murl:
        return []
    headers = dict(_upstream_auth_headers())
    if PROV_NAME == "relay":
        headers["anthropic-version"] = "2023-06-01"
    raw = http_get_json(murl, headers)
    out, ids = model_discovery.normalize_models_response(raw)
    if ids and PROV_NAME == "relay":
        RELAY_MODELS = ids
    return out


def build_models_response():
    """装配 /v1/models 响应，返回 (状态码, body dict)。协议锁定（修评审 P2-2）：
      - relay/openai-custom 回源成功 → (200, {data:[…含 supports_tools…]})。
      - 回源 HTTPError → (上游同状态码, {error_kind:"upstream", upstream_status, message})，
        绝不吞成 200+静态（否则掩盖坏 key）。builtin 兜底交 Rust 命令决定。
      - 网络异常 → (502, {error_kind:"network", upstream_status:None, message})。
      - 非 relay（无 models_url，deepseek/qwen）→ (200, {静态选择器列表})，行为不变。"""
    if PROV.get("models_url"):
        if RELAY_FORCE_MODEL:
            # force（Science 常驻代理）：只返回一个壳，Science 主列表显示真实模型名。
            # 出站由 resolve_model 的 force 分支覆盖，无需 model_map。app 的 fetch_models
            # 不设 RELAY_FORCE_MODEL，故仍走下面回源拿真实 id 供用户选（两个消费者切分）。
            log(f"GET /v1/models -> {PROV_NAME}(force 借壳): {RELAY_FORCE_MODEL}")
            return model_discovery.force_shell_response(RELAY_FORCE_MODEL)
        try:
            data = fetch_relay_models()
            log(f"GET /v1/models -> {PROV_NAME}(回源): {len(data)} 个模型")
            return model_discovery.live_models_response(data)
        except urllib.error.HTTPError as e:
            detail = ""
            try:
                detail = e.read().decode("utf-8", "replace")[:200]
            except Exception:
                pass
            log(f"GET /v1/models -> {PROV_NAME} 回源 HTTP {e.code}（保留状态码，不回静态）")
            return e.code, {"error_kind": "upstream", "upstream_status": e.code,
                            "message": f"upstream {e.code}: {detail}"}
        except Exception as e:
            log(f"GET /v1/models -> {PROV_NAME} 回源网络异常，本地回 502: {e}")
            return 502, {"error_kind": "network", "upstream_status": None, "message": str(e)}
    # 非 relay：静态选择器列表（deepseek/qwen）。
    return model_discovery.static_models_response(PROV["models"])


# ---------- Anthropic -> OpenAI 翻译（qwen 路径） ----------
def anthropic_to_openai(req):
    return openai_chat_compat.anthropic_to_openai(req, _provider_state(req))


def map_tool_choice(tc, tools):
    return openai_chat_compat.map_tool_choice(tc, tools)


def map_responses_tool_choice(tc, tools):
    return responses_compat.map_tool_choice(tc, tools)


def responses_max_output_tokens(req, model, state, has_tools):
    return responses_compat.max_output_tokens(req, model, state, has_tools, _is_dashscope_responses())


def _is_dashscope_responses():
    return responses_compat.is_dashscope_responses(PROV)


def normalize_responses_tool_parameters(schema):
    return responses_compat.normalize_tool_parameters(schema)


def map_responses_tools(tools):
    return responses_compat.map_tools(tools, _is_dashscope_responses())


def anthropic_to_openai_responses(req):
    return responses_compat.anthropic_to_openai(
        req,
        _provider_state(req),
        _is_dashscope_responses(),
    )


def openai_to_anthropic(resp, model_id):
    return openai_chat_compat.openai_to_anthropic(resp, model_id)


def _responses_output_text(item):
    return responses_compat.output_text(item)


def openai_responses_to_anthropic(resp, model_id):
    return responses_compat.openai_to_anthropic(resp, model_id)


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
        if self.close_connection:
            # 主动关闭连接时显式告知客户端，避免其在已关闭的 socket 上复用连接。
            self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def _sse(self, event, data):
        chunk = f"event: {event}\ndata: {json.dumps(data, ensure_ascii=False)}\n\n".encode()
        self.wfile.write(hex(len(chunk))[2:].encode() + b"\r\n" + chunk + b"\r\n")

    def _sse_error_and_terminate(self, msg):
        frame = ("event: error\ndata: " + json.dumps(
            {"type": "error", "error": {"type": "api_error", "message": msg}},
            ensure_ascii=False) + "\n\n").encode()
        self.wfile.write(hex(len(frame))[2:].encode() + b"\r\n" + frame + b"\r\n")
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def _auth_ok(self):
        if not AUTH_SECRET:
            return True
        prefix = "/" + AUTH_SECRET
        if self.path == prefix or self.path.startswith(prefix + "/"):
            self.path = self.path[len(prefix):] or "/"
            return True
        # 鉴权失败时请求体（POST）尚未读取，若保持长连接，服务端下一轮会从残留
        # body 中间开始解析下一个请求，产出的畸形 400 错误页会把残留字节和下一条
        # 请求行拼在一起回显给客户端，可能带出路径里的 secret。这里主动关连接
        # 阻断该复用路径；_send_json 会据 close_connection 追加 Connection: close。
        self.close_connection = True
        self._send_json(403, {"type": "error", "error": {
            "type": "permission_error", "message": "forbidden"}})
        return False

    def do_GET(self):
        if not self._auth_ok():
            return
        if self.path.startswith("/v1/models"):
            code, body = build_models_response()
            self._send_json(code, body)
        elif self.path.startswith("/health"):
            self._send_json(200, {"status": "ok", "provider": PROV_NAME})
        else:
            self._send_json(404, {"type": "error", "error": {"type": "not_found_error", "message": self.path}})

    def do_POST(self):
        if not self._auth_ok():
            return
        # Content-Length 解析放在保护内：畸形头（如 "oops" / 负数）应回规范 400，
        # 不能让 int() 抛 ValueError 击穿 handler、给客户端一个空响应。
        try:
            n = int(self.headers.get("Content-Length") or 0)
            if n < 0:
                raise ValueError("negative length")
        except (ValueError, TypeError):
            self._send_json(400, {"type": "error", "error": {
                "type": "invalid_request_error", "message": "invalid Content-Length"}})
            return
        raw = self.rfile.read(n) if n else b"{}"
        if not self.path.startswith("/v1/messages"):
            self._send_json(404, {"type": "error", "error": {"type": "not_found_error", "message": self.path}})
            return
        try:
            areq = json.loads(raw)
        except Exception as e:
            self._send_json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": str(e)}})
            return
        # 结构校验（修 P1 GPT 复审）：顶层必须是对象且 messages 是数组，否则回规范 400。
        # 否则 []/"hello"/{"messages":null} 会在下游 .get / 迭代处抛 AttributeError/TypeError，
        # 击穿线程 → 客户端拿到空响应而非 400。
        if not isinstance(areq, dict) or not isinstance(areq.get("messages"), list):
            self._send_json(400, {"type": "error", "error": {
                "type": "invalid_request_error",
                "message": "request body must be a JSON object with a 'messages' array"}})
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

    # ---- HTTP CONNECT 隧道：Anthropic 域名 fast-fail、其余透传（修 #3） ----
    def do_CONNECT(self):
        # operon 用 https_proxy 走到这里；self.path 形如 "host:port"。
        # 【为何不走 _auth_ok】CONNECT 把目标放在请求行、没有可嵌 path-secret 的位置，
        # operon 的 https_proxy 也带不上 secret。此处不鉴权的实际风险面很小：
        #   - 只监听回环（127.0.0.1），本机进程本就能自行外连，隧道不给它任何新能力；
        #   - 隧道是裸 TCP 转发，不注入上游 key、不经推理端点（那两条仍受 secret 保护）。
        #   即 path-secret 真正守护的边界（第三方 key + 推理端点）未被削弱。
        # 进一步收紧可让 launch 把 secret 放进 https_proxy 的 userinfo 再校验
        # Proxy-Authorization，但需先实测 operon 是否会带该头（否则误伤透传），留待整链联调。
        target = self.path
        host = target.rsplit(":", 1)[0].strip("[]").lower()
        if _is_blocked_host(host):
            # 401（未登录）而非 403（禁止）：让 operon 判 logged-out 秒过，而非当组织问题反复重试。
            log(f"CONNECT {target} -> 401 未登录（Anthropic 域名 fast-fail）")
            self._connect_reply(401)
            return
        try:
            port = int(target.rsplit(":", 1)[1])
        except (ValueError, IndexError):
            self._connect_reply(400)
            return
        try:
            upstream = socket.create_connection((host, port), timeout=10)
        except Exception as e:
            log(f"CONNECT {target} -> 502 上游连不上: {e}")
            self._connect_reply(502)
            return
        self.send_response(200, "Connection Established")
        self.end_headers()
        try:
            self.wfile.flush()
        except Exception:
            pass
        log(f"CONNECT {target} -> 隧道建立，透传")
        try:
            self._tunnel(self.connection, upstream)
        finally:
            try:
                upstream.close()
            except Exception:
                pass
        self.close_connection = True

    def _connect_reply(self, code):
        """CONNECT 的短响应（拒绝/错误）：空体 + 主动关连接。"""
        self.send_response(code)
        self.send_header("Content-Length", "0")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

    @staticmethod
    def _tunnel(client, upstream):
        """在两个已连接 socket 间双向搬字节，直到任一侧 EOF / 出错。"""
        socks = [client, upstream]
        while True:
            try:
                r, _, _ = select.select(socks, [], [])
            except Exception:
                return
            for s in r:
                other = upstream if s is client else client
                try:
                    data = s.recv(65536)
                except Exception:
                    return
                if not data:  # 对端 EOF
                    return
                try:
                    other.sendall(data)
                except Exception:
                    return

    # ---- DeepSeek：Anthropic 原生透传（改模型名+换鉴权+夹 max_tokens+重试） ----
    def _handle_anthropic(self, areq):
        state = _provider_state(areq)
        upstream_body, ctx = anthropic_compat.transform_request(areq, state)
        stream = bool(upstream_body.get("stream"))
        n_tools = len(upstream_body.get("tools") or [])
        log(f"POST /v1/messages  {ctx.src_model}->{ctx.target_model} stream={stream} "
            f"tools={n_tools} msgs={len(upstream_body.get('messages') or [])}  "
            f"(入站鉴权已剥离, 直连 {PROV_NAME})")
        # 鉴权头按 provider 的 auth_style（deepseek x-api-key / relay both）。KEY 只驻内存、不入日志。
        headers = {"content-type": "application/json", "anthropic-version": "2023-06-01"}
        headers.update(_upstream_auth_headers())
        data = json.dumps(upstream_body).encode()
        headers_sent = False
        try:
            if stream:
                # Deliberate stream tradeoff: open the downstream SSE response before
                # upstream TTFT, then send comment keepalives while waiting. This keeps
                # the client from retrying long tool-heavy requests, but upstream
                # HTTP errors after this point must be represented as SSE error frames
                # because the HTTP status line is already committed.
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                self.wfile.flush()
                headers_sent = True

                def _wc(b):
                    if b:
                        self.wfile.write(hex(len(b))[2:].encode() + b"\r\n" + b + b"\r\n")
                        self.wfile.flush()

                r, first, _ct = _open_stream_with_keepalive(_wc, PROV["url"], data, headers)
                with r:
                    # off / 无工具 → None（骨架直接透传，零开销）；detect / rewrite → 统一 filter。
                    f = anthropic_compat.make_stream_rewriter(ctx)
                    # 第一帧同样要过 filter（状态机 / 检测器必须从第 0 字节按序看到全部上游数据）。
                    if f is not None:
                        _wc(f.feed(first))
                    else:
                        _wc(first)
                    while True:
                        try:
                            chunk = r.readline(65536)
                        except Exception as e:
                            log(f"  !! 流中断（头已发），SSE error 收尾: {e}")
                            self._sse_error_and_terminate(str(e))
                            return
                        if not chunk:
                            break
                        if f is not None:
                            _wc(f.feed(chunk))
                        else:
                            _wc(chunk)
                    if f is not None:
                        _wc(f.finalize())
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                st = f.stats() if f is not None else {}
                if st.get("synthesized"):
                    log(f"  <- {PROV_NAME} 流式 DSML 改写 OK（合成 tool_use×{st['tool_n']}）")
                elif st.get("found"):
                    log(f"  <- {PROV_NAME} 流式透传 OK（!! detect：本响应含 DSML 泄漏，未改写）")
                else:
                    log(f"  <- {PROV_NAME} 流式透传 OK")
            else:
                body_bytes, ct = http_post(PROV["url"], data, headers)
                body_bytes, stats = anthropic_compat.rewrite_nonstream(body_bytes, ctx)
                if stats.get("rewritten"):
                    log(f"  <- {PROV_NAME} 非流式 DSML 改写 OK（展开 tool_use）")
                elif stats.get("found"):
                    log(f"  !! detect：非流式响应含 DSML 泄漏，未改写")
                self.send_response(200)
                self.send_header("Content-Type", ct)
                self.send_header("Content-Length", str(len(body_bytes)))
                self.end_headers()
                headers_sent = True
                self.wfile.write(body_bytes)
                if "rewritten" not in stats:
                    log(f"  <- {PROV_NAME} 非流式透传 OK")
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", "replace")[:400]
            log(f"  !! 上游 HTTP {e.code}: {detail}")
            if not headers_sent:
                # 上游鉴权 / 额度类状态码原样透传（401/403/429），其余归一化 502。
                code = e.code if e.code in (401, 403, 429) else 502
                self._send_json(code, {"type": "error", "error": {
                    "type": "api_error", "message": f"upstream {e.code}: {detail}"}})
            else:
                try:
                    self._sse_error_and_terminate(f"upstream {e.code}: {detail}")
                except Exception:
                    pass
        except Exception as e:
            log(f"  !! 代理异常: {e}")
            if headers_sent:
                try:
                    self._sse_error_and_terminate(str(e))
                except Exception:
                    pass
            else:
                self._send_json(502, {"type": "error", "error": {
                    "type": "api_error", "message": str(e)}})

    # ---- Qwen：翻译到 OpenAI，非流式取全再按需 SSE 回放 ----
    def _handle_openai(self, areq):
        model_id = areq.get("model", "claude-sonnet-5")
        stream = bool(areq.get("stream"))
        if PROV.get("api_format") == "openai_responses":
            oreq = anthropic_to_openai_responses(areq)
            msg_count = len(oreq.get("input") or [])
        else:
            oreq = anthropic_to_openai(areq)
            msg_count = len(oreq.get("messages") or [])
        n_tools = len(oreq.get("tools", []))
        log(f"POST /v1/messages  {model_id}->{oreq['model']} stream={stream} tools={n_tools} "
            f"msgs={msg_count}  (入站鉴权已剥离, {PROV_NAME})")
        headers = {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}
        data = json.dumps(oreq).encode()
        try:
            raw, _ct = http_post(PROV["url"], data, headers)
            oresp = json.loads(raw)
            if PROV.get("api_format") == "openai_responses":
                aresp = openai_responses_to_anthropic(oresp, model_id)
            else:
                aresp = openai_to_anthropic(oresp, model_id)
            if stream:
                self._replay_as_sse(aresp)
            else:
                self._send_json(200, aresp)
            log(f"  <- {PROV_NAME} OK (blocks={len(aresp['content'])} stop={aresp['stop_reason']})")
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", "replace")[:400]
            log(f"  !! 上游 HTTP {e.code}: {detail}")
            # 修 P2（GPT 复审）：OpenAI 翻译路径（qwen 等）同样保留上游 401/403/429，
            # 别一律归一化 502——否则 verify_key 无法准确提示「key 无效」。其余仍归 502。
            code = e.code if e.code in (401, 403, 429) else 502
            self._send_json(code, {"type": "error", "error": {"type": "api_error",
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
    ap.add_argument("--relay-base", default=None,
                    help="relay provider 的中转站 base_url（也可用环境变量 CSSWITCH_RELAY_BASE_URL）")
    ap.add_argument("--openai-base", default=None,
                    help="openai-custom provider 的 OpenAI base root（也可用环境变量 CSSWITCH_OPENAI_BASE_URL）")
    args = ap.parse_args()
    PROV_NAME = args.provider
    PROV = PROVIDERS[PROV_NAME]
    LOG = args.log
    KEY = load_key(PROV, args)
    AUTH_SECRET = os.environ.get("CSSWITCH_AUTH_TOKEN") or args.auth_token
    # relay：按中转站 base_url 装配上游端点（base + /v1/messages、base + /v1/models）。
    if PROV_NAME == "relay":
        base = (os.environ.get("CSSWITCH_RELAY_BASE_URL") or args.relay_base or "").strip().rstrip("/")
        if not base or not re.match(r"^https?://", base):
            print("relay 需要中转站 base_url（http(s)://…）。用 --relay-base 或环境变量 "
                  "CSSWITCH_RELAY_BASE_URL 提供。", file=sys.stderr)
            sys.exit(1)
        PROV = dict(PROV)
        PROV["url"] = base + "/v1/messages"
        PROV["models_url"] = base + "/v1/models"
        forced = (os.environ.get("CSSWITCH_RELAY_MODEL") or "").strip()
        if forced:
            RELAY_FORCE_MODEL = forced
        RELAY_THINKING = (os.environ.get("CSSWITCH_RELAY_THINKING") or "").strip() or None
    elif PROV_NAME in ("openai-custom", "openai-responses"):
        base = normalize_openai_base(os.environ.get("CSSWITCH_OPENAI_BASE_URL") or args.openai_base or "")
        if not base or not re.match(r"^https?://", base):
            print(f"{PROV_NAME} 需要 OpenAI base root（http(s)://…）。用 --openai-base 或环境变量 "
                  "CSSWITCH_OPENAI_BASE_URL 提供。", file=sys.stderr)
            sys.exit(1)
        forced = (os.environ.get("CSSWITCH_OPENAI_MODEL") or "").strip()
        PROV = dict(PROV)
        suffix = "/responses" if PROV.get("api_format") == "openai_responses" else "/chat/completions"
        PROV["url"] = openai_endpoint(base, suffix)
        PROV["models_url"] = openai_endpoint(base, "/models")
        # 模型发现 scratch 只需要 /models，不能要求 CSSWITCH_OPENAI_MODEL；
        # 正式推理由 Rust 侧 relay_missing_model + Message 探针保证 model 必填。
        if forced:
            PROV["default_model"] = forced
            RELAY_FORCE_MODEL = forced
    _up = os.environ.get("CSSWITCH_UPSTREAM_URL")
    if _up:
        PROV = dict(PROV)
        PROV["url"] = _up
    if not KEY:
        print(f"找不到 {PROV['key_env']}。用环境变量或 --env-file <路径> 提供。", file=sys.stderr)
        sys.exit(1)
    # DSML 兜底 shim 模式（默认 off；relay 恒 off；deepseek 且 dsml_capable 才读环境变量）。
    SHIM_MODE = dsml_shim.shim_mode(PROV_NAME, PROV)
    log(f"CSSwitch 代理启动 127.0.0.1:{args.port}  provider={PROV_NAME}  "
        f"key=已加载(未显示)  上游={PROV['url']}  dsml_shim={SHIM_MODE}")
    # 绑定重试：上次会话遗留的孤儿代理可能还占着端口（app 侧会主动清，但退干净需一点时间）。
    # 重试 ~3s 等端口释放，避免一次绑不上就直接失败（Errno 48）。
    srv = None
    for attempt in range(10):
        try:
            srv = ThreadingHTTPServer(("127.0.0.1", args.port), H)
            break
        except OSError as e:
            if attempt == 9:
                print(f"[csswitch] 端口 {args.port} 无法绑定：{e}。"
                      f"可能被占用（结束占用进程，或在面板「高级」里换个端口）。",
                      file=sys.stderr, flush=True)
                sys.exit(2)
            time.sleep(0.3)
    srv.serve_forever()
