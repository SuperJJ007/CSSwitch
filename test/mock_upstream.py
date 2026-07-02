"""测试用假上游：记账命中次数，按 mode 返回不同响应。
mode="json"：返回一份最小 Anthropic 非流式消息体。"""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def start_mock(mode="json"):
    hits = []

    class M(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_POST(self):
            n = int(self.headers.get("Content-Length") or 0)
            self.rfile.read(n)
            hits.append(self.path)
            if mode == "json":
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

    srv = ThreadingHTTPServer(("127.0.0.1", 0), M)
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{port}/up", hits, srv.shutdown
