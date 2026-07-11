"""测试用假上游：记账命中次数，按 mode 返回不同响应。
mode="json"：返回一份最小 Anthropic 非流式消息体。
mode="status:NNN"：返回 HTTP NNN + 一小段错误 JSON（测代理对上游错误码的处理，如 P3 401/403 透传）。
mode="openai_responses"：返回一份最小 OpenAI Responses 响应体。"""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from _loopback_ports import bind_http_server


def start_mock(mode="json"):
    hits = []

    class M(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_POST(self):
            n = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(n)
            hits.append(self.path)
            if mode == "openai_responses":
                try:
                    req = json.loads(raw or b"{}")
                except Exception:
                    req = {}
                body = json.dumps({
                    "id": "resp_mock",
                    "object": "response",
                    "status": "completed",
                    "model": req.get("model", "mock"),
                    "output": [{
                        "id": "msg_mock",
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "ok-responses"}],
                    }],
                    "usage": {"input_tokens": 2, "output_tokens": 3},
                }).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            elif mode == "json":
                body = json.dumps({
                    "id": "msg_mock", "type": "message", "role": "assistant",
                    "model": "mock", "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn", "stop_sequence": None,
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                }).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            elif mode.startswith("status:"):
                try:
                    code = int(mode.split(":", 1)[1])
                except ValueError:
                    code = 500
                body = json.dumps({"type": "error", "error": {
                    "type": "authentication_error", "message": "mock upstream error"}}).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        def do_GET(self):
            hits.append(self.path)
            if mode == "openai_models" and self.path.endswith("/models"):
                body = json.dumps({"data": [{"id": "glm-4.5"}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            self.send_response(404)
            self.end_headers()

    srv = bind_http_server(ThreadingHTTPServer, M)
    port = srv.server_address[1]
    thread = threading.Thread(target=srv.serve_forever, daemon=True)
    thread.start()

    def stop():
        srv.shutdown()
        srv.server_close()
        thread.join(timeout=5)

    return f"http://127.0.0.1:{port}/up", hits, stop
