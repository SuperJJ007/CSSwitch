"""代理级端到端：证明 DSML 兜底 shim 已真正接进 csswitch_proxy（Codex P0）。

启动【真实代理】子进程，用 CSSWITCH_UPSTREAM_URL 把上游指到一个会吐 DSML 纯文本
工具调用的假上游，再对代理发真实 HTTP。断言：
  - rewrite 模式：流式/非流式里泄漏成文本的 DSML 被还原成真正的 tool_use 块，
    stop_reason 被覆写成 tool_use；
  - off  模式（默认）：字节级透传，DSML 仍是文本、无 tool_use（零回归）。

隔离层测试：只打代理 ↔ 假上游，全程不碰 Science（守铁律 4）。
"""
import json
import os
import socket
import subprocess
import sys
import threading
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
from _capability import loopback_available

HERE = os.path.dirname(__file__)
PROXY = os.path.join(HERE, "..", "proxy", "csswitch_proxy.py")
SEC = "dsmltok"
P2 = "｜｜"   # 双全角竖线（issue #8 实测泄漏形态）

# 一段「模型本想调用 web_search、却泄漏成纯文本」的 DSML（与 test_dsml_shim 的 wrap_typed 同构）。
DSML_TEXT = (
    "<" + P2 + "DSML" + P2 + "tool_calls> "
    "<" + P2 + "DSML" + P2 + 'invoke name="web_search">'
    "<" + P2 + "DSML" + P2 + 'parameter name="query" string="true">GSE207177</' + P2 + "DSML" + P2 + "parameter>"
    "</" + P2 + "DSML" + P2 + "invoke> "
    "</" + P2 + "DSML" + P2 + "tool_calls>")


def _sse(event, obj):
    return f"event: {event}\ndata: {json.dumps(obj, ensure_ascii=False)}\n\n"


def _build_sse():
    frames = "".join([
        _sse("message_start", {"type": "message_start", "message": {
            "id": "msg_up", "type": "message", "role": "assistant", "model": "up",
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        _sse("content_block_start", {"type": "content_block_start", "index": 0,
             "content_block": {"type": "text", "text": ""}}),
        _sse("content_block_delta", {"type": "content_block_delta", "index": 0,
             "delta": {"type": "text_delta", "text": DSML_TEXT}}),
        _sse("content_block_stop", {"type": "content_block_stop", "index": 0}),
        _sse("message_delta", {"type": "message_delta",
             "delta": {"stop_reason": "end_turn", "stop_sequence": None},
             "usage": {"output_tokens": 5}}),
        _sse("message_stop", {"type": "message_stop"}),
    ])
    return frames.encode("utf-8")


def _build_json():
    return json.dumps({
        "id": "msg_up", "type": "message", "role": "assistant", "model": "up",
        "content": [{"type": "text", "text": DSML_TEXT}],
        "stop_reason": "end_turn", "stop_sequence": None,
        "usage": {"input_tokens": 1, "output_tokens": 5},
    }, ensure_ascii=False).encode("utf-8")


def start_dsml_upstream():
    """假上游：按请求体里的 stream 标志，返回含 DSML 文本的 SSE 或非流式 JSON。"""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(5)
    port = srv.getsockname()[1]

    def serve():
        while True:
            try:
                c, _ = srv.accept()
            except OSError:
                return
            req = c.recv(65536)
            is_stream = b'"stream": true' in req or b'"stream":true' in req
            if is_stream:
                sse = _build_sse()
                head = ("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n"
                        f"Content-Length: {len(sse)}\r\nConnection: close\r\n\r\n").encode()
                c.sendall(head + sse)
            else:
                j = _build_json()
                head = ("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                        f"Content-Length: {len(j)}\r\nConnection: close\r\n\r\n").encode()
                c.sendall(head + j)
            c.close()

    threading.Thread(target=serve, daemon=True).start()
    return f"http://127.0.0.1:{port}/up", srv


def raw_post(host, port, path, body):
    s = socket.create_connection((host, port), timeout=5)
    req = (f"POST {path} HTTP/1.1\r\nHost: {host}\r\n"
           f"Content-Type: application/json\r\nContent-Length: {len(body)}\r\n"
           f"Connection: close\r\n\r\n").encode() + body
    s.sendall(req)
    chunks = []
    while True:
        d = s.recv(65536)
        if not d:
            break
        chunks.append(d)
    s.close()
    return b"".join(chunks)


def dechunk(raw):
    """把代理的 chunked 响应体解开成裸 body；非 chunked 原样返回 body。"""
    head, _, rest = raw.partition(b"\r\n\r\n")
    if b"chunked" not in head.lower():
        return rest
    out, buf = b"", rest
    while buf:
        line, _, buf = buf.partition(b"\r\n")
        try:
            size = int(line.strip() or b"0", 16)
        except ValueError:
            break
        if size == 0:
            break
        out += buf[:size]
        buf = buf[size + 2:]      # 跳过分块尾随的 \r\n
    return out


def _req_body(stream):
    return json.dumps({
        "model": "claude-opus-4-8", "max_tokens": 100, "stream": stream,
        "tools": [{"name": "web_search",
                   "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}}],
        "messages": [{"role": "user", "content": "find GSE207177"}],
    }).encode("utf-8")


def launch_proxy(port, shim_mode, upstream):
    env = dict(os.environ, DEEPSEEK_API_KEY="fake", CSSWITCH_UPSTREAM_URL=upstream)
    if shim_mode is None:
        env.pop("CSSWITCH_TOOLUSE_SHIM", None)
    else:
        env["CSSWITCH_TOOLUSE_SHIM"] = shim_mode
    proc = subprocess.Popen(
        [sys.executable, PROXY, "--provider", "deepseek", "--port", str(port), "--auth-token", SEC],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    # 探活：等 /health 起来
    for _ in range(50):
        try:
            r = raw_post("127.0.0.1", port, f"/{SEC}/v1/messages", b'{"messages":[]}')
            if r:
                break
        except OSError:
            time.sleep(0.1)
    time.sleep(0.3)
    return proc


@unittest.skipUnless(loopback_available(), "env-blocked: loopback bind/connect not permitted")
class DsmlRewriteWired(unittest.TestCase):
    """rewrite 模式：DSML 泄漏被还原成真正的 tool_use（P0 已接通的核心证明）。"""

    @classmethod
    def setUpClass(cls):
        cls.up_url, cls.up_sock = start_dsml_upstream()
        cls.port = 18979  # S0 全局唯一端口：DsmlRewriteWired
        cls.proc = launch_proxy(cls.port, "rewrite", cls.up_url)

    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate()
        cls.proc.wait(timeout=5)
        cls.up_sock.close()

    def test_streaming_dsml_becomes_tool_use(self):
        raw = raw_post("127.0.0.1", self.port, f"/{SEC}/v1/messages", _req_body(True))
        body = dechunk(raw).decode("utf-8", "replace")
        self.assertIn("200", raw.split(b"\r\n", 1)[0].decode())
        self.assertIn('"type": "tool_use"', body)          # 合成了 tool_use 块
        self.assertIn('"name": "web_search"', body)
        self.assertIn("GSE207177", body)                    # 参数保真
        self.assertIn('"stop_reason": "tool_use"', body)    # stop_reason 被覆写
        self.assertNotIn("DSML", body)                      # 原始泄漏标记已被消化

    def test_nonstreaming_dsml_becomes_tool_use(self):
        raw = raw_post("127.0.0.1", self.port, f"/{SEC}/v1/messages", _req_body(False))
        body = dechunk(raw)
        obj = json.loads(body)
        kinds = [b.get("type") for b in obj["content"]]
        self.assertIn("tool_use", kinds)
        tu = next(b for b in obj["content"] if b["type"] == "tool_use")
        self.assertEqual(tu["name"], "web_search")
        self.assertEqual(tu["input"], {"query": "GSE207177"})
        self.assertEqual(obj["stop_reason"], "tool_use")


@unittest.skipUnless(loopback_available(), "env-blocked: loopback bind/connect not permitted")
class DsmlOffIsVerbatim(unittest.TestCase):
    """off 模式（默认）：字节级透传，DSML 仍是文本、无 tool_use（零回归护栏）。"""

    @classmethod
    def setUpClass(cls):
        cls.up_url, cls.up_sock = start_dsml_upstream()
        cls.port = 18980  # S0 全局唯一端口：DsmlOffIsVerbatim
        cls.proc = launch_proxy(cls.port, None, cls.up_url)   # 不设 env → 默认 off

    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate()
        cls.proc.wait(timeout=5)
        cls.up_sock.close()

    def test_streaming_passes_dsml_verbatim(self):
        raw = raw_post("127.0.0.1", self.port, f"/{SEC}/v1/messages", _req_body(True))
        body = dechunk(raw).decode("utf-8", "replace")
        self.assertIn("DSML", body)                          # 原样保留泄漏文本
        self.assertNotIn('"type": "tool_use"', body)         # 未改写

    def test_nonstreaming_passes_dsml_verbatim(self):
        raw = raw_post("127.0.0.1", self.port, f"/{SEC}/v1/messages", _req_body(False))
        obj = json.loads(dechunk(raw))
        self.assertEqual([b["type"] for b in obj["content"]], ["text"])
        self.assertIn("DSML", obj["content"][0]["text"])


if __name__ == "__main__":
    unittest.main()
