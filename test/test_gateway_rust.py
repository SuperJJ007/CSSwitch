import http.client
import hashlib
import json
import os
import pathlib
import re
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_GATEWAY_BIN = ROOT / "desktop" / "gateway" / "target" / "debug" / "csswitch-gateway"
STAGED_GATEWAY_DIR = ROOT / "desktop" / "src-tauri" / "binaries"


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    if port == 8765:
        return free_port()
    return port


def gateway_bin():
    raw = os.environ.get("CSSWITCH_GATEWAY_BIN")
    if raw and pathlib.Path(raw).is_file():
        return pathlib.Path(raw)
    if DEFAULT_GATEWAY_BIN.is_file():
        return DEFAULT_GATEWAY_BIN
    for path in sorted(STAGED_GATEWAY_DIR.glob("csswitch-gateway-*")):
        if path.is_file():
            return path
    return None


def static_catalog_fingerprint(catalog):
    digest = hashlib.sha256()
    digest.update(b"csswitch-static-catalog-fp-v1\0")

    def number(value):
        digest.update(int(value).to_bytes(4, "big"))

    def text(value):
        encoded = str(value).encode()
        number(len(encoded))
        digest.update(encoded)

    number(catalog["schema_version"])
    text(catalog["adapter"])
    text(catalog["default_selector_id"])
    number(len(catalog["routes"]))
    for route in catalog["routes"]:
        text(route["selector_id"])
        text(route["display_name"])
        text(route["upstream_model"])
        digest.update(bytes([{None: 0, False: 1, True: 2}[route.get("supports_tools")]]))
    for role in ("sonnet", "opus", "haiku", "fable"):
        text(catalog["role_bindings"][role])
    number(len(catalog["legacy_aliases"]))
    for legacy in catalog["legacy_aliases"]:
        text(legacy["alias"])
        text(legacy["selector_id"])
    return digest.hexdigest()


def recv_http_head(sock):
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data += chunk
    return data


def recv_http_all(sock):
    chunks = []
    while True:
        try:
            chunk = sock.recv(65536)
        except ConnectionResetError:
            break
        if not chunk:
            break
        chunks.append(chunk)
    return b"".join(chunks)


def recv_http_request(sock):
    sock.settimeout(2)
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(65536)
        if not chunk:
            return data
        data += chunk
    head, separator, body = data.partition(b"\r\n\r\n")
    content_length = 0
    for line in head.split(b"\r\n")[1:]:
        name, _, value = line.partition(b":")
        if name.strip().lower() == b"content-length":
            content_length = int(value.strip())
            break
    while len(body) < content_length:
        chunk = sock.recv(65536)
        if not chunk:
            break
        body += chunk
    return head + separator + body


def parse_raw_response(raw):
    head, _, body = raw.partition(b"\r\n\r\n")
    lines = head.split(b"\r\n")
    status = int(lines[0].split()[1])
    headers = {}
    for line in lines[1:]:
        key, _, value = line.partition(b":")
        if key:
            headers[key.strip().lower().decode()] = value.strip().decode()
    return status, headers, body


def assert_error_shape(testcase, body, error_type):
    parsed = json.loads(body)
    testcase.assertEqual(parsed["type"], "error")
    testcase.assertEqual(parsed["error"]["type"], error_type)
    testcase.assertIsInstance(parsed["error"]["message"], str)
    return parsed


def assert_dsml_tool_ids(testcase, ids):
    testcase.assertEqual(len(ids), 2)
    matches = [
        re.fullmatch(r"toolu_dsml_(?P<nonce>[0-9a-f]+)_(?P<index>[1-9][0-9]*)", tool_id)
        for tool_id in ids
    ]
    testcase.assertTrue(all(matches), ids)
    testcase.assertEqual([int(match.group("index")) for match in matches], [1, 2])
    nonces = {match.group("nonce") for match in matches}
    testcase.assertEqual(len(nonces), 1)
    return nonces.pop()


class MockUpstream(ThreadingHTTPServer):
    allow_reuse_address = True

    def __init__(
        self,
        response_body,
        content_type="application/json",
        status=200,
        response_delay=0,
    ):
        self.requests = []
        self.response_body = response_body
        self.content_type = content_type
        self.status = status
        self.response_delay = response_delay
        super().__init__(("127.0.0.1", free_port()), MockHandler)


class MockHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _write_payload(self):
        if getattr(self.server, "drop_response", False):
            self.close_connection = True
            try:
                self.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.connection.close()
            return
        payload = self.server.response_body
        if self.server.response_delay:
            time.sleep(self.server.response_delay)
        self.send_response(self.server.status)
        self.send_header("content-type", self.server.content_type)
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length)
        self.server.requests.append(
            {
                "path": self.path,
                "headers": {k.lower(): v for k, v in self.headers.items()},
                "body": body,
            }
        )
        self._write_payload()

    def do_GET(self):
        self.server.requests.append(
            {
                "method": "GET",
                "path": self.path,
                "headers": {k.lower(): v for k, v in self.headers.items()},
                "body": b"",
            }
        )
        self._write_payload()

    def log_message(self, *_args):
        pass


class EchoServer:
    def __init__(self):
        self.port = free_port()
        self.ready = threading.Event()
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()
        self.ready.wait(2)

    def _serve(self):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as srv:
            srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            srv.bind(("127.0.0.1", self.port))
            srv.listen(1)
            self.ready.set()
            conn, _ = srv.accept()
            with conn:
                data = conn.recv(4096)
                conn.sendall(data)


class RawUpstream:
    def __init__(self, handler):
        self.port = free_port()
        self.handler = handler
        self.ready = threading.Event()
        self.closed = False
        self.workers = []
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()
        self.ready.wait(2)

    def _serve(self):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as srv:
            srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            srv.bind(("127.0.0.1", self.port))
            srv.listen(5)
            self.ready.set()
            while not self.closed:
                try:
                    conn, _ = srv.accept()
                except OSError:
                    return
                if self.closed:
                    conn.close()
                    return
                worker = threading.Thread(target=self.handler, args=(conn,), daemon=True)
                self.workers.append(worker)
                worker.start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}/anthropic/v1/messages"

    def close(self):
        self.closed = True
        try:
            with socket.create_connection(("127.0.0.1", self.port), timeout=0.2):
                pass
        except OSError:
            pass
        self.thread.join(timeout=2)
        if self.thread.is_alive():
            raise AssertionError("raw upstream listener thread did not exit")
        for worker in self.workers:
            worker.join(timeout=2)
            if worker.is_alive():
                raise AssertionError("raw upstream worker thread did not exit")


def delayed_stream_handler(conn):
    with conn:
        conn.recv(65536)
        time.sleep(1.2)
        head = (
            "HTTP/1.1 200 OK\r\n"
            "Content-Type: text/event-stream\r\n"
            "Transfer-Encoding: chunked\r\n\r\n"
        )
        first = b"event: message_start\n"
        conn.sendall(head.encode() + b"15\r\n" + first + b"\r\n0\r\n\r\n")


def dropping_stream_handler(conn):
    with conn:
        conn.recv(65536)
        payload = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n"
        head = (
            "HTTP/1.1 200 OK\r\n"
            "Content-Type: text/event-stream\r\n"
            "Transfer-Encoding: chunked\r\n\r\n"
        )
        try:
            conn.sendall(
                head.encode() + hex(len(payload))[2:].encode() + b"\r\n" + payload + b"\r\n"
            )
            conn.sendall(b"1f4\r\n0123456789")
        except BrokenPipeError:
            pass


def dropping_stream_handler_with_payload(payload):
    def handler(conn):
        with conn:
            conn.recv(65536)
            head = (
                "HTTP/1.1 200 OK\r\n"
                "Content-Type: text/event-stream\r\n"
                "Transfer-Encoding: chunked\r\n\r\n"
            )
            try:
                conn.sendall(
                    head.encode()
                    + hex(len(payload))[2:].encode()
                    + b"\r\n"
                    + payload
                    + b"\r\n"
                )
                conn.sendall(b"1f4\r\n0123456789")
            except BrokenPipeError:
                pass

    return handler


class RustGatewayLoopback(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.bin = gateway_bin()
        if cls.bin is None:
            raise unittest.SkipTest("csswitch-gateway binary not built")

    def start_gateway(
        self,
        upstream_url=None,
        secret="secret",
        provider="deepseek",
        openai_base_url=None,
        openai_model=None,
        relay_thinking=None,
        shim_mode="off",
        launch_id="loopback-launch",
        port=None,
        env_overrides=None,
        gateway_intent="formal",
        static_catalog=None,
    ):
        port = port or free_port()
        self.assertNotEqual(port, 8765)
        env = os.environ.copy()
        env.update(
            {
                "CSSWITCH_AUTH_TOKEN": secret,
                "CSSWITCH_TOOLUSE_SHIM": shim_mode,
                "CSSWITCH_LAUNCH_ID": launch_id,
                "CSSWITCH_GATEWAY_INTENT": gateway_intent,
            }
        )
        if gateway_intent == "formal":
            if static_catalog is None:
                target = openai_model or "claude-opus-4-8"
                if provider == "deepseek":
                    routes = [
                        ("claude-opus-4-8", "DeepSeek Pro", "deepseek-v4-pro", True),
                        ("claude-haiku-4-5", "DeepSeek Flash", "deepseek-v4-flash", True),
                    ]
                    roles = {"sonnet": routes[0][0], "opus": routes[0][0], "haiku": routes[1][0], "fable": routes[0][0]}
                elif provider == "qwen":
                    routes = [
                        ("claude-csswitch-qwen-max-222222222222", "Qwen Max", "qwen3.7-max", True),
                        ("claude-csswitch-qwen-plus-111111111111", "Qwen Plus", "qwen-plus-latest", True),
                        ("claude-csswitch-qwen-turbo-333333333333", "Qwen Turbo", "qwen-turbo", True),
                    ]
                    roles = {"sonnet": routes[1][0], "opus": routes[0][0], "haiku": routes[2][0], "fable": routes[0][0]}
                else:
                    routes = [("claude-opus-4-8", target, target, None)]
                    roles = {name: routes[0][0] for name in ("sonnet", "opus", "haiku", "fable")}
                static_catalog = {
                    "schema_version": 1,
                    "adapter": provider,
                    "default_selector_id": routes[0][0],
                    "routes": [
                        {"selector_id": selector, "display_name": display, "upstream_model": upstream, "supports_tools": tools}
                        for selector, display, upstream, tools in routes
                    ],
                    "role_bindings": roles,
                    "legacy_aliases": [],
                }
            static_catalog = dict(static_catalog)
            static_catalog["catalog_fp"] = static_catalog_fingerprint(static_catalog)
            env["CSSWITCH_STATIC_MODEL_CATALOG_V1"] = json.dumps(
                static_catalog, separators=(",", ":"), sort_keys=True
            )
        else:
            env.pop("CSSWITCH_STATIC_MODEL_CATALOG_V1", None)
        if provider == "qwen":
            env["DASHSCOPE_API_KEY"] = "fake-qwen-key"
            env.pop("DEEPSEEK_API_KEY", None)
            env.pop("CSSWITCH_OPENAI_KEY", None)
            env.pop("CSSWITCH_RELAY_KEY", None)
            env.pop("CSSWITCH_RELAY_BASE_URL", None)
            env.pop("CSSWITCH_RELAY_MODEL", None)
            env.pop("CSSWITCH_RELAY_THINKING", None)
        elif provider in ("openai-custom", "openai-responses"):
            env["CSSWITCH_OPENAI_KEY"] = "fake-openai-key"
            env["CSSWITCH_OPENAI_BASE_URL"] = openai_base_url or "http://127.0.0.1:1/up"
            if openai_model:
                env["CSSWITCH_OPENAI_MODEL"] = openai_model
            else:
                env.pop("CSSWITCH_OPENAI_MODEL", None)
            env.pop("DEEPSEEK_API_KEY", None)
            env.pop("DASHSCOPE_API_KEY", None)
            env.pop("CSSWITCH_RELAY_KEY", None)
            env.pop("CSSWITCH_RELAY_BASE_URL", None)
            env.pop("CSSWITCH_RELAY_MODEL", None)
            env.pop("CSSWITCH_RELAY_THINKING", None)
        elif provider == "relay":
            env["CSSWITCH_RELAY_KEY"] = "fake-relay-key"
            env["CSSWITCH_RELAY_BASE_URL"] = openai_base_url or "http://127.0.0.1:1/up"
            if openai_model:
                env["CSSWITCH_RELAY_MODEL"] = openai_model
            else:
                env.pop("CSSWITCH_RELAY_MODEL", None)
            if relay_thinking:
                env["CSSWITCH_RELAY_THINKING"] = relay_thinking
            else:
                env.pop("CSSWITCH_RELAY_THINKING", None)
            env.pop("DEEPSEEK_API_KEY", None)
            env.pop("DASHSCOPE_API_KEY", None)
            env.pop("CSSWITCH_OPENAI_KEY", None)
        else:
            env["DEEPSEEK_API_KEY"] = "fake-deepseek-key"
            env.pop("DASHSCOPE_API_KEY", None)
            env.pop("CSSWITCH_OPENAI_KEY", None)
            env.pop("CSSWITCH_RELAY_KEY", None)
            env.pop("CSSWITCH_RELAY_BASE_URL", None)
            env.pop("CSSWITCH_RELAY_MODEL", None)
            env.pop("CSSWITCH_RELAY_THINKING", None)
        if upstream_url:
            env["CSSWITCH_UPSTREAM_URL"] = upstream_url
        else:
            env.pop("CSSWITCH_UPSTREAM_URL", None)
        for key, value in (env_overrides or {}).items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        proc = subprocess.Popen(
            [
                str(self.bin),
                "--provider",
                provider,
                "--port",
                str(port),
                "--auth-token",
                "cli-secret-should-lose",
            ],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        deadline = time.time() + 5
        while time.time() < deadline:
            try:
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=0.2)
                conn.request("GET", f"/{secret}/health")
                resp = conn.getresponse()
                body = json.loads(resp.read())
                conn.close()
                if (
                    resp.status == 200
                    and body.get("gateway") == "rust"
                    and body.get("provider") == provider
                    and body.get("shim") == (shim_mode if provider == "deepseek" else "off")
                    and body.get("launch_id") == launch_id
                ):
                    return proc, port
            except OSError:
                pass
            if proc.poll() is not None:
                break
            time.sleep(0.05)
        proc.terminate()
        stderr = ""
        try:
            _, stderr = proc.communicate(timeout=1)
        except Exception:
            proc.kill()
        raise RuntimeError(f"gateway did not become healthy: {stderr}")

    def stop_gateway(self, proc):
        proc.terminate()
        try:
            stdout, stderr = proc.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            stdout, stderr = proc.communicate(timeout=3)
        return stdout, stderr

    def start_current_gateway(self, *args, env_overrides=None, **kwargs):
        raw = os.environ.get("CSSWITCH_GATEWAY_BIN")
        self.assertTrue(
            raw,
            "current-source contract tests require CSSWITCH_GATEWAY_BIN to be explicitly pinned",
        )
        pinned = pathlib.Path(raw)
        self.assertTrue(pinned.is_file(), f"pinned gateway binary not found: {pinned}")
        self.assertEqual(
            pinned.resolve(),
            self.bin.resolve(),
            "test process must use the explicitly pinned current-source gateway binary",
        )
        isolated_env = {
            "HTTP_PROXY": None,
            "HTTPS_PROXY": None,
            "ALL_PROXY": None,
            "http_proxy": None,
            "https_proxy": None,
            "all_proxy": None,
            "NO_PROXY": "*",
            "no_proxy": "*",
        }
        isolated_env.update(env_overrides or {})
        kwargs["env_overrides"] = isolated_env
        return self.start_gateway(*args, **kwargs)

    def raw_request(self, port, request):
        with socket.create_connection(("127.0.0.1", port), timeout=5) as sock:
            sock.sendall(request)
            return recv_http_all(sock)

    def raw_post_until(self, port, body, needle, timeout=1.1):
        with socket.create_connection(("127.0.0.1", port), timeout=5) as sock:
            sock.settimeout(timeout)
            request = (
                "POST /secret/v1/messages HTTP/1.1\r\n"
                "Host: 127.0.0.1\r\n"
                "Content-Type: application/json\r\n"
                f"Content-Length: {len(body)}\r\n"
                "Connection: close\r\n\r\n"
            ).encode() + body
            started = time.monotonic()
            sock.sendall(request)
            chunks = []
            try:
                while needle not in b"".join(chunks):
                    chunk = sock.recv(65536)
                    if not chunk:
                        break
                    chunks.append(chunk)
            except socket.timeout:
                pass
            return b"".join(chunks), time.monotonic() - started

    def test_auth_and_models(self):
        proc, port = self.start_gateway()
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/v1/models")
            forbidden = conn.getresponse()
            self.assertEqual(forbidden.status, 403)
            forbidden_body = json.loads(forbidden.read())
            conn.close()
            self.assertEqual(forbidden_body["type"], "error")
            self.assertEqual(forbidden_body["error"]["type"], "permission_error")

            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(body["first_id"], "claude-opus-4-8")
            self.assertEqual(body["last_id"], "claude-haiku-4-5")

            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/health")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(body["status"], "ok")
            self.assertEqual(body["gateway"], "rust")
            self.assertEqual(body["provider"], "deepseek")
            self.assertEqual(body["shim"], "off")
            self.assertEqual(body["launch_id"], "loopback-launch")
            self.assertEqual(body["intent"], "formal")
            self.assertRegex(body["catalog_fp"], r"^[0-9a-f]{64}$")
        finally:
            self.stop_gateway(proc)

    def test_models_response_replaces_default_placeholder_with_upstream_name(self):
        selector = "claude-csswitch-relay-real-model-123456789abc"
        catalog = {
            "schema_version": 1,
            "adapter": "relay",
            "default_selector_id": selector,
            "routes": [{
                "selector_id": selector,
                "display_name": "default",
                "upstream_model": "vendor/real-model-name",
                "supports_tools": None,
            }],
            "role_bindings": {role: selector for role in ("sonnet", "opus", "haiku", "fable")},
            "legacy_aliases": [],
        }
        proc, port = self.start_current_gateway(provider="relay", static_catalog=catalog)
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/v1/models")
            response = conn.getresponse()
            body = json.loads(response.read())
            conn.close()
            self.assertEqual(response.status, 200)
            self.assertEqual(body["data"][0]["display_name"], "vendor/real-model-name")
            self.assertNotEqual(body["data"][0]["display_name"].lower(), "default")
        finally:
            self.stop_gateway(proc)

    def test_formal_five_model_whitelist_routes_exact_upstream_ids(self):
        upstream = MockUpstream(json.dumps({
            "id": "msg_exact", "type": "message", "role": "assistant",
            "model": "ignored", "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1},
        }).encode())
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        routes = [
            (f"claude-csswitch-relay-model-{index}-00000000000{index}", f"Model {index}", f"vendor/model-{index}", True)
            for index in range(1, 6)
        ]
        catalog = {
            "schema_version": 1,
            "adapter": "relay",
            "default_selector_id": routes[0][0],
            "routes": [
                {"selector_id": selector, "display_name": display, "upstream_model": target, "supports_tools": tools}
                for selector, display, target, tools in routes
            ],
            "role_bindings": {"sonnet": routes[0][0], "opus": routes[1][0], "haiku": routes[4][0], "fable": routes[1][0]},
            "legacy_aliases": [],
        }
        proc, port = self.start_gateway(
            provider="relay",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            static_catalog=catalog,
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/v1/models")
            response = conn.getresponse()
            body = json.loads(response.read())
            conn.close()
            self.assertEqual(response.status, 200)
            self.assertEqual([item["id"] for item in body["data"]], [item[0] for item in routes])

            for selector, _, target, _ in routes:
                payload = json.dumps({
                    "model": selector,
                    "max_tokens": 1,
                    "messages": [{"role": "user", "content": "ping"}],
                }).encode()
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
                conn.request("POST", "/secret/v1/messages", body=payload, headers={"content-type": "application/json"})
                response = conn.getresponse()
                response.read()
                conn.close()
                self.assertEqual(response.status, 200)
                self.assertEqual(json.loads(upstream.requests[-1]["body"])["model"], target)
            self.assertEqual(len(upstream.requests), 5)
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_qwen_rejects_old_codex_alias_before_upstream(self):
        upstream = MockUpstream(b'{"unexpected":true}')
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="qwen",
            upstream_url=f"http://127.0.0.1:{upstream.server_port}/compatible-mode/v1/chat/completions",
        )
        try:
            payload = json.dumps({
                "model": "claude-csswitch-codex-gpt-5.6-sol",
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "ping"}],
            }).encode()
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("POST", "/secret/v1/messages", body=payload, headers={"content-type": "application/json"})
            response = conn.getresponse()
            body = json.loads(response.read())
            conn.close()
            self.assertEqual(response.status, 400)
            self.assertEqual(body["error"]["type"], "route_unknown")
            self.assertEqual(upstream.requests, [])
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_bind_failure_cannot_adopt_old_launch_identity(self):
        old_proc, port = self.start_gateway(launch_id="old-launch")
        try:
            with self.assertRaisesRegex(RuntimeError, "gateway did not become healthy"):
                self.start_gateway(port=port, launch_id="new-launch")

            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/health")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(body["launch_id"], "old-launch")
            self.assertIsNone(old_proc.poll(), "old managed gateway must not be killed")
        finally:
            self.stop_gateway(old_proc)

    def test_nonstream_maps_request_and_preserves_content_length(self):
        upstream = MockUpstream(b'{"id":"msg_mock","type":"message"}')
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=f"http://127.0.0.1:{upstream.server_port}/anthropic/v1/messages"
        )
        try:
            request = {
                "model": "claude-opus-4-8",
                "max_tokens": 100000,
                "thinking": {"type": "auto"},
                "messages": [{"role": "user", "content": "hi"}],
            }
            raw = json.dumps(request).encode()
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=raw,
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = resp.read()
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(resp.getheader("content-length"), str(len(body)))
            self.assertEqual(body, b'{"id":"msg_mock","type":"message"}')

            self.assertEqual(len(upstream.requests), 1)
            req = upstream.requests[0]
            self.assertEqual(req["path"], "/anthropic/v1/messages")
            self.assertEqual(req["headers"]["x-api-key"], "fake-deepseek-key")
            self.assertEqual(req["headers"]["anthropic-version"], "2023-06-01")
            mapped = json.loads(req["body"])
            self.assertEqual(mapped["model"], "deepseek-v4-pro")
            self.assertEqual(mapped["max_tokens"], 65536)
            self.assertEqual(mapped["thinking"]["type"], "adaptive")
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_deepseek_dsml_rewrite_nonstream_opt_in(self):
        dsml = (
            "<｜｜DSML｜｜tool_calls> "
            '<｜｜DSML｜｜invoke name="web_search">'
            '<｜｜DSML｜｜parameter name="query" string="true">first</｜｜DSML｜｜parameter>'
            "</｜｜DSML｜｜invoke> "
            '<｜｜DSML｜｜invoke name="web_search">'
            '<｜｜DSML｜｜parameter name="query" string="true">second</｜｜DSML｜｜parameter>'
            "</｜｜DSML｜｜invoke> </｜｜DSML｜｜tool_calls>"
        )
        upstream = MockUpstream(
            json.dumps(
                {
                    "id": "msg_dsml",
                    "type": "message",
                    "role": "assistant",
                    "model": "deepseek-v4-pro",
                    "content": [{"type": "text", "text": "A" + dsml + "B"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 2},
                },
                ensure_ascii=False,
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=f"http://127.0.0.1:{upstream.server_port}/anthropic/v1/messages",
            shim_mode="rewrite",
        )
        try:
            request = {
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "web_search", "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}}],
            }
            bodies = []
            for _ in range(2):
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                conn.request(
                    "POST",
                    "/secret/v1/messages",
                    body=json.dumps(request).encode(),
                    headers={"content-type": "application/json"},
                )
                resp = conn.getresponse()
                raw = resp.read()
                body = json.loads(raw)
                conn.close()
                self.assertEqual(resp.status, 200)
                self.assertEqual(resp.getheader("content-length"), str(len(raw)))
                bodies.append(body)

            request_nonces = []
            for body in bodies:
                tool_uses = [block for block in body["content"] if block["type"] == "tool_use"]
                self.assertEqual([block["name"] for block in tool_uses], ["web_search", "web_search"])
                self.assertEqual(
                    [block["input"] for block in tool_uses],
                    [{"query": "first"}, {"query": "second"}],
                )
                request_nonces.append(
                    assert_dsml_tool_ids(self, [block["id"] for block in tool_uses])
                )
                self.assertEqual(body["stop_reason"], "tool_use")
            self.assertEqual(len(set(request_nonces)), 2)
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_deepseek_dsml_detect_nonstream_passes_through(self):
        dsml = (
            "<｜｜DSML｜｜tool_calls> "
            '<｜｜DSML｜｜invoke name="web_search">'
            '<｜｜DSML｜｜parameter name="query" string="true">GSE207177</｜｜DSML｜｜parameter>'
            "</｜｜DSML｜｜invoke> </｜｜DSML｜｜tool_calls>"
        )
        upstream = MockUpstream(
            json.dumps(
                {
                    "id": "msg_dsml",
                    "type": "message",
                    "role": "assistant",
                    "model": "deepseek-v4-pro",
                    "content": [{"type": "text", "text": dsml}],
                    "stop_reason": "end_turn",
                },
                ensure_ascii=False,
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=f"http://127.0.0.1:{upstream.server_port}/anthropic/v1/messages",
            shim_mode="detect",
        )
        try:
            request = {
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "web_search", "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}}],
            }
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual([block["type"] for block in body["content"]], ["text"])
            self.assertIn("DSML", body["content"][0]["text"])
            self.assertEqual(body["stop_reason"], "end_turn")
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_deepseek_dsml_detect_stream_cross_chunk_passes_through(self):
        dsml = (
            "<｜｜DSML｜｜tool_calls> "
            '<｜｜DSML｜｜invoke name="web_search">'
            '<｜｜DSML｜｜parameter name="query" string="true">GSE207177</｜｜DSML｜｜parameter>'
            "</｜｜DSML｜｜invoke> </｜｜DSML｜｜tool_calls>"
        )
        payload = b"".join(
            [
                b'event: message_start\ndata: {"type":"message_start","message":{"id":"m_detect","type":"message","role":"assistant","model":"deepseek-v4-pro","content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}\n\n',
                b'event: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n',
                (
                    "event: content_block_delta\ndata: "
                    + json.dumps(
                        {
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {"type": "text_delta", "text": "before " + dsml + " after"},
                        },
                        ensure_ascii=False,
                        separators=(",", ":"),
                    )
                    + "\n\n"
                ).encode(),
                b'event: content_block_stop\ndata: {"type":"content_block_stop","index":0}\n\n',
                b'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}\n\n',
                b'event: message_stop\ndata: {"type":"message_stop"}\n\n',
            ]
        )
        marker = "<｜｜DSML｜｜tool_calls>".encode()
        marker_at = payload.index(marker)
        split_at = marker_at + len("<｜".encode())
        attempts = []
        release_second_chunk = threading.Event()

        def detect_stream_handler(conn):
            with conn:
                attempts.append(recv_http_request(conn))
                head = (
                    "HTTP/1.1 200 OK\r\n"
                    "Content-Type: text/event-stream\r\n"
                    "Transfer-Encoding: chunked\r\n\r\n"
                )
                conn.sendall(head.encode())
                try:
                    first, second = payload[:split_at], payload[split_at:]
                    conn.sendall(
                        hex(len(first))[2:].encode() + b"\r\n" + first + b"\r\n"
                    )
                    if not release_second_chunk.wait(3):
                        return
                    conn.sendall(
                        hex(len(second))[2:].encode() + b"\r\n" + second + b"\r\n"
                    )
                    conn.sendall(b"0\r\n\r\n")
                except BrokenPipeError:
                    pass

        upstream = RawUpstream(detect_stream_handler)
        try:
            with tempfile.TemporaryDirectory(prefix="csswitch-current-detect-") as temp_home:
                proc, port = self.start_current_gateway(
                    provider="deepseek",
                    upstream_url=upstream.url,
                    shim_mode="detect",
                    env_overrides={"HOME": temp_home},
                )
                try:
                    request = {
                        "model": "claude-opus-4-8",
                        "stream": True,
                        "messages": [{"role": "user", "content": "hi"}],
                        "tools": [
                            {
                                "name": "web_search",
                                "input_schema": {
                                    "type": "object",
                                    "properties": {"query": {"type": "string"}},
                                },
                            }
                        ],
                    }
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    conn.request(
                        "POST",
                        "/secret/v1/messages",
                        body=json.dumps(request).encode(),
                        headers={"content-type": "application/json"},
                    )
                    resp = conn.getresponse()
                    first_body = resp.read(split_at)
                    release_second_chunk.set()
                    body = first_body + resp.read()
                    conn.close()
                    self.assertEqual(resp.status, 200)
                    self.assertEqual(resp.getheader("transfer-encoding"), "chunked")
                    self.assertEqual(first_body, payload[:split_at])
                    self.assertEqual(body, payload)
                    self.assertIn(marker, body)
                    self.assertNotIn(b'"type":"tool_use"', body)
                    self.assertEqual(len(attempts), 1)
                finally:
                    _stdout, captured_stderr = self.stop_gateway(proc)
        finally:
            upstream.close()
        detect_log = "deepseek stream DSML detect found=true"
        self.assertEqual(
            captured_stderr.splitlines().count(detect_log),
            1,
            captured_stderr,
        )
        self.assertNotIn("deepseek stream DSML rewrite", captured_stderr)

    def test_deepseek_dsml_rewrite_stream_opt_in(self):
        dsml = (
            "<｜｜DSML｜｜tool_calls> "
            '<｜｜DSML｜｜invoke name="web_search">'
            '<｜｜DSML｜｜parameter name="query" string="true">first</｜｜DSML｜｜parameter>'
            "</｜｜DSML｜｜invoke> "
            '<｜｜DSML｜｜invoke name="web_search">'
            '<｜｜DSML｜｜parameter name="query" string="true">second</｜｜DSML｜｜parameter>'
            "</｜｜DSML｜｜invoke> </｜｜DSML｜｜tool_calls>"
        )
        payload = b"".join([
            b'event: message_start\ndata: {"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"deepseek-v4-pro","content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}\n\n',
            b'event: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n',
            ("event: content_block_delta\ndata: " + json.dumps({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "A" + dsml + "B"}}, ensure_ascii=False) + "\n\n").encode(),
            b'event: content_block_stop\ndata: {"type":"content_block_stop","index":0}\n\n',
            b'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}\n\n',
            b'event: message_stop\ndata: {"type":"message_stop"}\n\n',
        ])

        def dsml_stream_handler(conn):
            with conn:
                conn.recv(65536)
                head = (
                    "HTTP/1.1 200 OK\r\n"
                    "Content-Type: text/event-stream\r\n"
                    "Transfer-Encoding: chunked\r\n\r\n"
                )
                conn.sendall(head.encode())
                try:
                    for chunk in (payload[:180], payload[180:360], payload[360:]):
                        conn.sendall(hex(len(chunk))[2:].encode() + b"\r\n" + chunk + b"\r\n")
                    conn.sendall(b"0\r\n\r\n")
                except BrokenPipeError:
                    pass

        upstream = RawUpstream(dsml_stream_handler)
        proc, port = self.start_gateway(
            provider="deepseek",
            upstream_url=upstream.url,
            shim_mode="rewrite",
        )
        try:
            request = {
                "model": "claude-opus-4-8",
                "stream": True,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "web_search", "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}}],
            }
            request_nonces = []
            request_body = json.dumps(request).encode()
            for _ in range(2):
                raw = self.raw_request(
                    port,
                    (
                        b"POST /secret/v1/messages HTTP/1.1\r\n"
                        b"Host: 127.0.0.1\r\n"
                        b"Content-Type: application/json\r\n"
                        + f"Content-Length: {len(request_body)}\r\n".encode()
                        + b"Connection: close\r\n\r\n"
                        + request_body
                    ),
                )
                status, headers, body = parse_raw_response(raw)
                self.assertEqual(status, 200)
                self.assertEqual(headers["transfer-encoding"], "chunked")
                self.assertEqual(body.count(b'"type":"tool_use"'), 2)
                self.assertEqual(body.count(b'"name":"web_search"'), 2)
                self.assertIn(b'"stop_reason":"tool_use"', body)
                self.assertNotIn("DSML".encode(), body)
                ids = [tool_id.decode() for tool_id in re.findall(rb'"id":"(toolu_dsml_[0-9a-f]+_[0-9]+)"', body)]
                request_nonces.append(assert_dsml_tool_ids(self, ids))
            self.assertEqual(len(set(request_nonces)), 2)
        finally:
            self.stop_gateway(proc)
            upstream.close()

    def test_qwen_models_and_nonstream_translation(self):
        upstream = MockUpstream(
            json.dumps(
                {
                    "id": "chatcmpl_1",
                    "choices": [
                        {
                            "message": {
                                "content": "answer",
                                "tool_calls": [
                                    {
                                        "id": "call_2",
                                        "type": "function",
                                        "function": {
                                            "name": "grade",
                                            "arguments": "{\"score\":1}",
                                        },
                                    }
                                ],
                            },
                            "finish_reason": "tool_calls",
                        }
                    ],
                    "usage": {"prompt_tokens": 7, "completion_tokens": 8},
                }
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=(
                f"http://127.0.0.1:{upstream.server_port}"
                "/compatible-mode/v1/chat/completions"
            ),
            provider="qwen",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/v1/models")
            models_resp = conn.getresponse()
            models_body = json.loads(models_resp.read())
            conn.close()
            self.assertEqual(models_resp.status, 200)
            self.assertEqual(models_body["first_id"], "claude-csswitch-qwen-max-222222222222")
            self.assertEqual(models_body["last_id"], "claude-csswitch-qwen-turbo-333333333333")

            request = {
                "model": "claude-haiku-4-5-20250514",
                "max_tokens": 100000,
                "messages": [
                    {"role": "user", "content": "hi"},
                    {
                        "role": "assistant",
                        "content": [
                            {"type": "text", "text": "checking"},
                            {
                                "type": "tool_use",
                                "id": "toolu_1",
                                "name": "lookup",
                                "input": {"q": "x"},
                            },
                        ],
                    },
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": "toolu_1",
                                "content": [{"type": "text", "text": "found"}],
                            }
                        ],
                    },
                ],
                "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
                "tool_choice": {"type": "any"},
            }
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(body["content"][0], {"type": "text", "text": "answer"})
            self.assertEqual(body["content"][1]["type"], "tool_use")
            self.assertEqual(body["content"][1]["input"], {"score": 1})
            self.assertEqual(body["stop_reason"], "tool_use")
            self.assertEqual(body["usage"], {"input_tokens": 7, "output_tokens": 8})

            self.assertEqual(len(upstream.requests), 1)
            req = upstream.requests[0]
            self.assertEqual(req["path"], "/compatible-mode/v1/chat/completions")
            self.assertEqual(req["headers"]["authorization"], "Bearer fake-qwen-key")
            self.assertNotIn("x-api-key", req["headers"])
            self.assertNotIn("anthropic-version", req["headers"])
            mapped = json.loads(req["body"])
            self.assertEqual(mapped["model"], "qwen-turbo")
            self.assertEqual(mapped["max_tokens"], 8192)
            self.assertEqual(mapped["messages"][1]["tool_calls"][0]["function"]["name"], "lookup")
            self.assertEqual(mapped["messages"][2], {
                "role": "tool",
                "tool_call_id": "toolu_1",
                "content": "found",
            })
            self.assertEqual(
                mapped["tool_choice"],
                {"type": "function", "function": {"name": "lookup"}},
            )
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_qwen_stream_replays_anthropic_sse(self):
        upstream = MockUpstream(
            json.dumps(
                {
                    "id": "chatcmpl_stream",
                    "choices": [
                        {
                            "message": {"content": "streamed"},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 2, "completion_tokens": 3},
                }
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=(
                f"http://127.0.0.1:{upstream.server_port}"
                "/compatible-mode/v1/chat/completions"
            ),
            provider="qwen",
        )
        try:
            request = {
                "model": "claude-opus-4-8",
                "stream": True,
                "messages": [{"role": "user", "content": "hi"}],
            }
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = resp.read()
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(resp.getheader("transfer-encoding"), "chunked")
            self.assertIn(b"event: message_start", body)
            self.assertIn(b"event: content_block_delta", body)
            self.assertIn(b'"text":"streamed"', body)
            self.assertIn(b"event: message_stop", body)
            mapped = json.loads(upstream.requests[0]["body"])
            self.assertFalse(mapped["stream"], "qwen Rust path should fetch full body then replay SSE")
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_qwen_nonstream_upstream_errors_match_python_shape(self):
        upstream = MockUpstream(b'{"error":"bad key"}', status=401)
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=(
                f"http://127.0.0.1:{upstream.server_port}"
                "/compatible-mode/v1/chat/completions"
            ),
            provider="qwen",
        )
        try:
            request_body = (
                b'{"model":"claude-opus-4-8",'
                b'"messages":[{"role":"user","content":"hi"}]}'
            )
            raw = self.raw_request(
                port,
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Type: application/json\r\n"
                    + f"Content-Length: {len(request_body)}\r\n".encode()
                    + b"Connection: close\r\n\r\n"
                    + request_body
                ),
            )
            status, headers, body = parse_raw_response(raw)
            self.assertEqual(status, 401)
            self.assertEqual(int(headers["content-length"]), len(body))
            parsed = assert_error_shape(self, body, "api_error")
            self.assertIn("upstream 401", parsed["error"]["message"])
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_custom_openai_forced_model_shell_and_nonstream_translation(self):
        upstream = MockUpstream(
            json.dumps(
                {
                    "id": "chatcmpl_custom",
                    "choices": [
                        {
                            "message": {"content": "custom ok"},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 4, "completion_tokens": 5},
                }
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="openai-custom",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            openai_model="glm-4.5",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/v1/models")
            models_resp = conn.getresponse()
            models_body = json.loads(models_resp.read())
            conn.close()
            self.assertEqual(models_resp.status, 200)
            self.assertEqual(models_body["data"], [{
                "type": "model",
                "id": "claude-opus-4-8",
                "display_name": "glm-4.5",
                "supports_tools": None,
                "created_at": "2026-01-01T00:00:00Z",
            }])
            self.assertEqual(upstream.requests, [], "forced model shell should not hit /models")

            request = {
                "model": "claude-opus-4-8",
                "max_tokens": 1000000,
                "messages": [{"role": "user", "content": "hi"}],
            }
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(body["content"], [{"type": "text", "text": "custom ok"}])
            self.assertEqual(body["usage"], {"input_tokens": 4, "output_tokens": 5})

            self.assertEqual(len(upstream.requests), 1)
            req = upstream.requests[0]
            self.assertEqual(req["path"], "/up/v1/chat/completions")
            self.assertEqual(req["headers"]["authorization"], "Bearer fake-openai-key")
            self.assertNotIn("x-api-key", req["headers"])
            self.assertNotIn("anthropic-version", req["headers"])
            mapped = json.loads(req["body"])
            self.assertEqual(mapped["model"], "glm-4.5")
            self.assertEqual(mapped["max_tokens"], 1000000)
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_openai_responses_forced_model_shell_and_nonstream_translation(self):
        upstream = MockUpstream(
            json.dumps(
                {
                    "id": "resp_custom",
                    "status": "completed",
                    "output": [
                        {
                            "type": "message",
                            "content": [{"type": "output_text", "text": "responses ok"}],
                        }
                    ],
                    "usage": {"input_tokens": 2, "output_tokens": 3},
                }
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="openai-responses",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            openai_model="gpt-5.2",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/v1/models")
            models_resp = conn.getresponse()
            models_body = json.loads(models_resp.read())
            conn.close()
            self.assertEqual(models_resp.status, 200)
            self.assertEqual(models_body["data"], [{
                "type": "model",
                "id": "claude-opus-4-8",
                "display_name": "gpt-5.2",
                "supports_tools": None,
                "created_at": "2026-01-01T00:00:00Z",
            }])
            self.assertEqual(upstream.requests, [], "forced model shell should not hit /models")

            request = {
                "model": "claude-opus-4-8",
                "max_tokens": 999999,
                "messages": [{"role": "user", "content": "hi"}],
            }
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(body["content"], [{"type": "text", "text": "responses ok"}])
            self.assertEqual(body["usage"], {"input_tokens": 2, "output_tokens": 3})

            self.assertEqual(len(upstream.requests), 1)
            req = upstream.requests[0]
            self.assertEqual(req["path"], "/up/v1/responses")
            self.assertEqual(req["headers"]["authorization"], "Bearer fake-openai-key")
            self.assertNotIn("x-api-key", req["headers"])
            self.assertNotIn("anthropic-version", req["headers"])
            mapped = json.loads(req["body"])
            self.assertEqual(mapped["model"], "gpt-5.2")
            self.assertEqual(mapped["max_output_tokens"], 65536)
            self.assertNotIn("/up/v1/chat/completions", [r["path"] for r in upstream.requests])
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_openai_responses_tools_and_tool_result_full_loopback(self):
        upstream = MockUpstream(
            json.dumps(
                {
                    "id": "resp_tools",
                    "status": "completed",
                    "output": [
                        {
                            "type": "message",
                            "content": [
                                {"type": "output_text", "text": "next lookup"}
                            ],
                        },
                        {
                            "type": "function_call",
                            "call_id": "call_next",
                            "name": "lookup",
                            "arguments": '{"q":"next"}',
                        },
                    ],
                    "usage": {"input_tokens": 12, "output_tokens": 7},
                }
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix="csswitch-current-responses-") as temp_home:
                proc, port = self.start_current_gateway(
                    provider="openai-responses",
                    openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
                    openai_model="gpt-5.2",
                    env_overrides={"HOME": temp_home},
                )
                try:
                    request = {
                        "model": "claude-opus-4-8",
                        "system": [{"type": "text", "text": "be brief"}],
                        "max_tokens": 999999,
                        "messages": [
                            {"role": "user", "content": "hi"},
                            {
                                "role": "assistant",
                                "content": [
                                    {"type": "text", "text": "checking"},
                                    {
                                        "type": "tool_use",
                                        "id": "call_previous",
                                        "name": "lookup",
                                        "input": {"q": "past"},
                                    },
                                ],
                            },
                            {
                                "role": "user",
                                "content": [
                                    {
                                        "type": "tool_result",
                                        "tool_use_id": "call_previous",
                                        "content": [
                                            {"type": "text", "text": "found"}
                                        ],
                                    }
                                ],
                            },
                        ],
                        "tools": [
                            {
                                "name": "lookup",
                                "description": "search records",
                                "input_schema": {
                                    "properties": {"q": {"type": "string"}},
                                    "required": ["q"],
                                },
                            }
                        ],
                        "tool_choice": {"type": "any"},
                    }
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    conn.request(
                        "POST",
                        "/secret/v1/messages",
                        body=json.dumps(request).encode(),
                        headers={"content-type": "application/json"},
                    )
                    resp = conn.getresponse()
                    raw_body = resp.read()
                    conn.close()
                    body = json.loads(raw_body)
                    self.assertEqual(resp.status, 200)
                    self.assertEqual(resp.getheader("content-length"), str(len(raw_body)))
                    self.assertEqual(
                        body["content"],
                        [
                            {"type": "text", "text": "next lookup"},
                            {
                                "type": "tool_use",
                                "id": "call_next",
                                "name": "lookup",
                                "input": {"q": "next"},
                            },
                        ],
                    )
                    self.assertEqual(body["stop_reason"], "tool_use")
                    self.assertEqual(body["usage"], {"input_tokens": 12, "output_tokens": 7})

                    self.assertEqual(len(upstream.requests), 1)
                    captured = upstream.requests[0]
                    self.assertEqual(captured["path"], "/up/v1/responses")
                    self.assertEqual(
                        captured["headers"]["authorization"],
                        "Bearer fake-openai-key",
                    )
                    mapped = json.loads(captured["body"])
                    self.assertEqual(mapped["model"], "gpt-5.2")
                    self.assertFalse(mapped["stream"])
                    self.assertEqual(mapped["instructions"], "be brief")
                    self.assertEqual(mapped["max_output_tokens"], 65536)
                    self.assertEqual(
                        mapped["input"],
                        [
                            {"role": "user", "content": "hi"},
                            {"role": "assistant", "content": "checking"},
                            {
                                "type": "function_call",
                                "call_id": "call_previous",
                                "name": "lookup",
                                "arguments": '{"q": "past"}',
                            },
                            {
                                "type": "function_call_output",
                                "call_id": "call_previous",
                                "output": "found",
                            },
                        ],
                    )
                    self.assertEqual(
                        mapped["tools"],
                        [
                            {
                                "type": "function",
                                "name": "lookup",
                                "description": "search records",
                                "parameters": {
                                    "type": "object",
                                    "properties": {"q": {"type": "string"}},
                                    "required": ["q"],
                                },
                            }
                        ],
                    )
                    self.assertEqual(mapped["tool_choice"], "auto")
                finally:
                    self.stop_gateway(proc)
        finally:
            upstream.shutdown()
            upstream.server_close()

    def test_openai_responses_error_mapping_and_http_statuses_do_not_retry(self):
        upstream = MockUpstream(b'{}', status=401)
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        request_body = json.dumps(
            {
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
            }
        ).encode()
        try:
            with tempfile.TemporaryDirectory(prefix="csswitch-current-responses-errors-") as temp_home:
                proc, port = self.start_current_gateway(
                    provider="openai-responses",
                    openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
                    openai_model="gpt-5.2",
                    env_overrides={"HOME": temp_home},
                )
                try:
                    cases = [
                        (401, 401),
                        (403, 403),
                        (429, 429),
                        (500, 502),
                        (502, 502),
                        (503, 502),
                    ]
                    for upstream_status, expected_status in cases:
                        with self.subTest(upstream_status=upstream_status):
                            upstream.status = upstream_status
                            upstream.response_body = json.dumps(
                                {
                                    "error": {
                                        "type": "provider_error",
                                        "message": f"mock {upstream_status}",
                                    }
                                }
                            ).encode()
                            before = len(upstream.requests)
                            conn = http.client.HTTPConnection(
                                "127.0.0.1", port, timeout=5
                            )
                            conn.request(
                                "POST",
                                "/secret/v1/messages",
                                body=request_body,
                                headers={"content-type": "application/json"},
                            )
                            resp = conn.getresponse()
                            raw_body = resp.read()
                            conn.close()
                            self.assertEqual(resp.status, expected_status)
                            self.assertEqual(
                                resp.getheader("content-length"), str(len(raw_body))
                            )
                            parsed = assert_error_shape(self, raw_body, "api_error")
                            self.assertIn(
                                f"upstream {upstream_status}",
                                parsed["error"]["message"],
                            )
                            self.assertEqual(
                                len(upstream.requests),
                                before + 1,
                                "an upstream HTTP status must not be retried",
                            )
                            self.assertEqual(
                                upstream.requests[-1]["path"], "/up/v1/responses"
                            )

                    upstream.status = 200
                    upstream.response_body = b"not-json"
                    before = len(upstream.requests)
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    conn.request(
                        "POST",
                        "/secret/v1/messages",
                        body=request_body,
                        headers={"content-type": "application/json"},
                    )
                    resp = conn.getresponse()
                    raw_body = resp.read()
                    conn.close()
                    self.assertEqual(resp.status, 502)
                    parsed = assert_error_shape(self, raw_body, "api_error")
                    self.assertTrue(parsed["error"]["message"])
                    self.assertEqual(
                        len(upstream.requests),
                        before + 1,
                        "a malformed HTTP 200 payload is a protocol error, not a retryable transport error",
                    )
                finally:
                    self.stop_gateway(proc)
        finally:
            upstream.shutdown()
            upstream.server_close()

    def test_openai_responses_connection_drop_retries_exactly_four_times(self):
        attempts = []

        def always_drop_handler(conn):
            with conn:
                attempts.append(recv_http_request(conn))
                try:
                    conn.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

        upstream = RawUpstream(always_drop_handler)
        request_body = json.dumps(
            {
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
            }
        ).encode()
        try:
            with tempfile.TemporaryDirectory(prefix="csswitch-current-responses-drop-") as temp_home:
                proc, port = self.start_current_gateway(
                    provider="openai-responses",
                    openai_base_url=f"http://127.0.0.1:{upstream.port}/up",
                    openai_model="gpt-5.2",
                    env_overrides={"HOME": temp_home},
                )
                try:
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
                    conn.request(
                        "POST",
                        "/secret/v1/messages",
                        body=request_body,
                        headers={"content-type": "application/json"},
                    )
                    resp = conn.getresponse()
                    raw_body = resp.read()
                    conn.close()
                    self.assertEqual(resp.status, 502)
                    parsed = assert_error_shape(self, raw_body, "api_error")
                    self.assertTrue(parsed["error"]["message"])
                    self.assertEqual(len(attempts), 4)
                    self.assertTrue(all(attempts))
                finally:
                    self.stop_gateway(proc)
        finally:
            upstream.close()

    def test_openai_responses_drop_then_success_retries_exactly_once(self):
        attempts = []
        payload = json.dumps(
            {
                "id": "resp_after_retry",
                "status": "completed",
                "output": [
                    {
                        "type": "message",
                        "content": [
                            {"type": "output_text", "text": "retry succeeded"}
                        ],
                    }
                ],
                "usage": {"input_tokens": 1, "output_tokens": 2},
            }
        ).encode()

        def drop_then_success_handler(conn):
            with conn:
                attempts.append(recv_http_request(conn))
                if len(attempts) == 1:
                    try:
                        conn.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                    return
                head = (
                    "HTTP/1.1 200 OK\r\n"
                    "Content-Type: application/json\r\n"
                    f"Content-Length: {len(payload)}\r\n"
                    "Connection: close\r\n\r\n"
                )
                conn.sendall(head.encode() + payload)

        upstream = RawUpstream(drop_then_success_handler)
        request_body = json.dumps(
            {
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
            }
        ).encode()
        try:
            with tempfile.TemporaryDirectory(prefix="csswitch-current-responses-retry-") as temp_home:
                proc, port = self.start_current_gateway(
                    provider="openai-responses",
                    openai_base_url=f"http://127.0.0.1:{upstream.port}/up",
                    openai_model="gpt-5.2",
                    env_overrides={"HOME": temp_home},
                )
                try:
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    conn.request(
                        "POST",
                        "/secret/v1/messages",
                        body=request_body,
                        headers={"content-type": "application/json"},
                    )
                    resp = conn.getresponse()
                    body = json.loads(resp.read())
                    conn.close()
                    self.assertEqual(resp.status, 200)
                    self.assertEqual(
                        body["content"],
                        [{"type": "text", "text": "retry succeeded"}],
                    )
                    self.assertEqual(len(attempts), 2)
                    self.assertTrue(all(attempts))
                finally:
                    self.stop_gateway(proc)
        finally:
            upstream.close()

    def test_openai_responses_three_drops_then_success_uses_fourth_attempt(self):
        attempts = []
        payload = json.dumps(
            {
                "id": "resp_after_three_retries",
                "status": "completed",
                "output": [
                    {
                        "type": "message",
                        "content": [
                            {"type": "output_text", "text": "fourth attempt succeeded"}
                        ],
                    }
                ],
                "usage": {"input_tokens": 1, "output_tokens": 2},
            }
        ).encode()

        def three_drops_then_success_handler(conn):
            with conn:
                attempts.append(recv_http_request(conn))
                if len(attempts) <= 3:
                    try:
                        conn.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                    return
                head = (
                    "HTTP/1.1 200 OK\r\n"
                    "Content-Type: application/json\r\n"
                    f"Content-Length: {len(payload)}\r\n"
                    "Connection: close\r\n\r\n"
                )
                conn.sendall(head.encode() + payload)

        upstream = RawUpstream(three_drops_then_success_handler)
        request_body = json.dumps(
            {
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
            }
        ).encode()
        try:
            with tempfile.TemporaryDirectory(prefix="csswitch-current-responses-retry4-") as temp_home:
                proc, port = self.start_current_gateway(
                    provider="openai-responses",
                    openai_base_url=f"http://127.0.0.1:{upstream.port}/up",
                    openai_model="gpt-5.2",
                    env_overrides={"HOME": temp_home},
                )
                try:
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
                    conn.request(
                        "POST",
                        "/secret/v1/messages",
                        body=request_body,
                        headers={"content-type": "application/json"},
                    )
                    resp = conn.getresponse()
                    body = json.loads(resp.read())
                    conn.close()
                    self.assertEqual(resp.status, 200)
                    self.assertEqual(
                        body["content"],
                        [{"type": "text", "text": "fourth attempt succeeded"}],
                    )
                    self.assertEqual(len(attempts), 4)
                    self.assertTrue(all(attempts))
                finally:
                    self.stop_gateway(proc)
        finally:
            upstream.close()

    def test_dashscope_responses_uppercase_host_rules_and_safe_metadata_logs(self):
        forward_proxy = MockUpstream(
            json.dumps(
                {
                    "id": "resp_dashscope",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "content": [{"type": "output_text", "text": "dashscope ok"}],
                    }],
                    "usage": {"input_tokens": 2, "output_tokens": 3},
                }
            ).encode()
        )
        thread = threading.Thread(target=forward_proxy.serve_forever, daemon=True)
        thread.start()
        temp_home = tempfile.TemporaryDirectory(prefix="csswitch-dashscope-responses-")
        proxy_url = f"http://127.0.0.1:{forward_proxy.server_port}"
        proc = None
        stdout = ""
        stderr = ""
        try:
            proc, port = self.start_gateway(
                provider="openai-responses",
                openai_base_url="http://DASHSCOPE.ALIYUNCS.COM/compatible-mode/v1",
                openai_model="gpt-5.2",
                env_overrides={
                    "HOME": temp_home.name,
                    "HTTP_PROXY": proxy_url,
                    "http_proxy": proxy_url,
                    "HTTPS_PROXY": None,
                    "https_proxy": None,
                    "ALL_PROXY": None,
                    "all_proxy": None,
                    "NO_PROXY": "",
                    "no_proxy": "",
                },
            )
            request = {
                "model": "claude-opus-4-8",
                "max_tokens": 999999,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [
                    {"name": "web_search", "input_schema": {"type": "object"}},
                    {"name": "lookup", "input_schema": {"type": "object"}},
                ],
            }
            for stream in (False, True):
                body = dict(request, stream=stream)
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                conn.request(
                    "POST",
                    "/secret/v1/messages",
                    body=json.dumps(body).encode(),
                    headers={"content-type": "application/json"},
                )
                resp = conn.getresponse()
                response_body = resp.read()
                conn.close()
                self.assertEqual(resp.status, 200, response_body[:300])
                if stream:
                    self.assertIn(b"event: message_stop", response_body)
                else:
                    self.assertEqual(
                        json.loads(response_body)["content"],
                        [{"type": "text", "text": "dashscope ok"}],
                    )
        finally:
            if proc is not None:
                proc.terminate()
                try:
                    stdout, stderr = proc.communicate(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    stdout, stderr = proc.communicate(timeout=5)
            forward_proxy.shutdown()
            thread.join(timeout=2)
            forward_proxy.server_close()
            temp_home.cleanup()

        self.assertEqual(len(forward_proxy.requests), 2)
        for captured in forward_proxy.requests:
            self.assertEqual(
                captured["path"].lower(),
                "http://dashscope.aliyuncs.com/compatible-mode/v1/responses",
            )
            mapped = json.loads(captured["body"])
            self.assertEqual([tool["name"] for tool in mapped["tools"]], ["lookup"])
            self.assertEqual(mapped["max_output_tokens"], 8192)

        log_line = (
            "POST /v1/messages provider=openai-responses target=gpt-5.2 "
            "stream={} input=1 tools=1 "
            "rules=tool.dashscope.responses.web_search-drop,"
            "provider.dashscope.responses-tools-cap"
        )
        self.assertEqual(stderr.count(log_line.format("false")), 1, stderr)
        self.assertEqual(stderr.count(log_line.format("true")), 1, stderr)
        combined_logs = (stdout + stderr).lower()
        self.assertNotIn("fake-openai-key", combined_logs)
        self.assertNotIn("dashscope.aliyuncs.com", combined_logs)

    def test_custom_openai_models_discovery_without_forced_model(self):
        upstream = MockUpstream(
            json.dumps(
                {
                    "data": [
                        {"id": "glm-4.5", "supported_parameters": ["tools"]},
                        {"id": "glm-lite", "supported_parameters": ["temperature"]},
                    ]
                }
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="openai-custom",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            gateway_intent="scratch-models",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual([m["id"] for m in body["data"]], ["glm-4.5", "glm-lite"])
            self.assertEqual(body["data"][0]["supports_tools"], True)
            self.assertEqual(body["data"][1]["supports_tools"], False)
            self.assertEqual(upstream.requests[0]["path"], "/up/v1/models")
            self.assertEqual(upstream.requests[0]["headers"]["authorization"], "Bearer fake-openai-key")
            self.assertNotIn("x-api-key", upstream.requests[0]["headers"])
            self.assertNotIn("anthropic-version", upstream.requests[0]["headers"])
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_openai_responses_models_discovery_without_forced_model(self):
        upstream = MockUpstream(
            json.dumps(
                {
                    "data": [
                        {"id": "gpt-5.2", "supported_parameters": ["tools"]},
                        {"id": "gpt-lite", "supported_parameters": ["temperature"]},
                    ]
                }
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="openai-responses",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            gateway_intent="scratch-models",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual([m["id"] for m in body["data"]], ["gpt-5.2", "gpt-lite"])
            self.assertEqual(body["data"][0]["supports_tools"], True)
            self.assertEqual(body["data"][1]["supports_tools"], False)
            self.assertEqual(upstream.requests[0]["path"], "/up/v1/models")
            self.assertEqual(upstream.requests[0]["headers"]["authorization"], "Bearer fake-openai-key")
            self.assertNotIn("x-api-key", upstream.requests[0]["headers"])
            self.assertNotIn("anthropic-version", upstream.requests[0]["headers"])
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_relay_forced_model_shell_and_nonstream_passthrough(self):
        upstream = MockUpstream(
            json.dumps(
                {
                    "id": "msg_relay",
                    "type": "message",
                    "role": "assistant",
                    "model": "MiniMax-M2",
                    "content": [{"type": "text", "text": "relay ok"}],
                    "stop_reason": "end_turn",
                    "stop_sequence": None,
                    "usage": {"input_tokens": 4, "output_tokens": 5},
                }
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="relay",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            openai_model="MiniMax-M2",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/v1/models")
            models_resp = conn.getresponse()
            models_body = json.loads(models_resp.read())
            conn.close()
            self.assertEqual(models_resp.status, 200)
            self.assertEqual(models_body["data"], [{
                "type": "model",
                "id": "claude-opus-4-8",
                "display_name": "MiniMax-M2",
                "supports_tools": None,
                "created_at": "2026-01-01T00:00:00Z",
            }])
            self.assertEqual(upstream.requests, [], "forced relay shell should not hit /models")

            request = {
                "model": "claude-opus-4-8",
                "max_tokens": 1000000,
                "thinking": {"type": "auto"},
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [
                    {"name": "lookup", "description": "lookup", "input_schema": {"properties": {"q": {"type": "string"}}}},
                    {"name": "empty", "input_schema": []},
                ],
            }
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(body["content"], [{"type": "text", "text": "relay ok"}])
            self.assertEqual(body["usage"], {"input_tokens": 4, "output_tokens": 5})

            self.assertEqual(len(upstream.requests), 1)
            req = upstream.requests[0]
            self.assertEqual(req["path"], "/up/v1/messages")
            self.assertEqual(req["headers"]["authorization"], "Bearer fake-relay-key")
            self.assertEqual(req["headers"]["x-api-key"], "fake-relay-key")
            self.assertEqual(req["headers"]["anthropic-version"], "2023-06-01")
            mapped = json.loads(req["body"])
            self.assertEqual(mapped["model"], "MiniMax-M2")
            self.assertEqual(mapped["max_tokens"], 1000000)
            self.assertEqual(mapped["thinking"]["type"], "adaptive")
            schemas = {tool["name"]: tool["input_schema"] for tool in mapped["tools"]}
            self.assertEqual(schemas["lookup"]["type"], "object")
            self.assertEqual(schemas["empty"], {"type": "object", "properties": {}})
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_relay_models_discovery_without_forced_model(self):
        upstream = MockUpstream(
            json.dumps(
                {
                    "data": [
                        {"id": "glm-5.2", "supported_parameters": ["tools"]},
                        {"id": "glm-lite", "supported_parameters": ["temperature"]},
                    ]
                }
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="relay",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            gateway_intent="scratch-models",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual([m["id"] for m in body["data"]], ["glm-5.2", "glm-lite"])
            self.assertEqual(body["data"][0]["supports_tools"], True)
            self.assertEqual(body["data"][1]["supports_tools"], False)
            self.assertEqual(upstream.requests[0]["path"], "/up/v1/models")
            self.assertEqual(upstream.requests[0]["headers"]["authorization"], "Bearer fake-relay-key")
            self.assertEqual(upstream.requests[0]["headers"]["x-api-key"], "fake-relay-key")
            self.assertEqual(upstream.requests[0]["headers"]["anthropic-version"], "2023-06-01")
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_relay_models_cache_snaps_messages_and_preserves_last_good_value(self):
        discovered = "claude-haiku-4-5-20251001"
        upstream = MockUpstream(
            json.dumps(
                {
                    "data": [
                        {"id": discovered, "supported_parameters": ["tools"]},
                        {"id": "other-model"},
                    ]
                }
            ).encode()
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="relay",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            gateway_intent="scratch-models",
        )
        message_response = json.dumps(
            {
                "id": "msg_cache",
                "type": "message",
                "role": "assistant",
                "model": discovered,
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "stop_sequence": None,
                "usage": {"input_tokens": 1, "output_tokens": 1},
            }
        ).encode()
        request = {
            "model": "claude-haiku-4-5",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}],
        }

        def get_models():
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            return resp.status, body

        def post_message():
            upstream.response_body = message_response
            upstream.status = 200
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            resp.read()
            conn.close()
            self.assertEqual(resp.status, 400)

        try:
            status, body = get_models()
            self.assertEqual(status, 200)
            self.assertEqual([m["id"] for m in body["data"]], [discovered, "other-model"])
            post_message()

            upstream.response_body = b'{"data":[{"id":""}]}'
            upstream.status = 200
            status, body = get_models()
            self.assertEqual(status, 200)
            self.assertEqual(body["data"], [])
            post_message()

            upstream.response_body = b'not-json'
            upstream.status = 200
            status, body = get_models()
            self.assertEqual(status, 502)
            self.assertEqual(body["error_kind"], "protocol")
            self.assertIsNone(body["upstream_status"])
            post_message()

            upstream.response_body = b'{"error":"temporary"}'
            upstream.status = 500
            status, body = get_models()
            self.assertEqual(status, 500)
            self.assertEqual(body["error_kind"], "upstream")
            self.assertEqual(body["upstream_status"], 500)
            post_message()

            upstream.drop_response = True
            status, body = get_models()
            self.assertEqual(status, 502)
            self.assertEqual(body["error_kind"], "network")
            self.assertIsNone(body["upstream_status"])
            upstream.drop_response = False
            post_message()

            posts = [request for request in upstream.requests if request.get("method") != "GET"]
            self.assertEqual(len(posts), 0, "scratch-models must reject inference before upstream")
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_relay_models_http_statuses_preserve_truthful_error_shape(self):
        for upstream_status in (401, 403, 500):
            with self.subTest(upstream_status=upstream_status):
                upstream = MockUpstream(b'{"error":"models failure"}', status=upstream_status)
                thread = threading.Thread(target=upstream.serve_forever, daemon=True)
                thread.start()
                proc, port = self.start_gateway(
                    provider="relay",
                    openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
                    gateway_intent="scratch-models",
                )
                try:
                    raw = self.raw_request(
                        port,
                        b"GET /secret/v1/models HTTP/1.1\r\n"
                        b"Host: 127.0.0.1\r\n"
                        b"Connection: close\r\n\r\n",
                    )
                    status, headers, raw_body = parse_raw_response(raw)
                    body = json.loads(raw_body)
                    self.assertEqual(status, upstream_status)
                    self.assertEqual(int(headers["content-length"]), len(raw_body))
                    self.assertEqual(body["error_kind"], "upstream")
                    self.assertEqual(body["upstream_status"], upstream_status)
                    self.assertIsInstance(body["message"], str)
                    self.assertNotIn("data", body)
                    if upstream_status == 500:
                        self.assertEqual(
                            raw.split(b"\r\n", 1)[0],
                            b"HTTP/1.1 500 Internal Server Error",
                        )
                finally:
                    self.stop_gateway(proc)
                    upstream.shutdown()
                    upstream.server_close()

    def test_relay_models_invalid_json_is_protocol_shaped_502(self):
        upstream = MockUpstream(b'not-json')
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="relay",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            gateway_intent="scratch-models",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 502)
            self.assertEqual(body["error_kind"], "protocol")
            self.assertIsNone(body["upstream_status"])
            self.assertIn("JSON parse failed", body["message"])
            self.assertNotIn("data", body)
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_relay_models_network_failure_is_network_shaped_502(self):
        unavailable_port = free_port()
        proc, port = self.start_gateway(
            provider="relay",
            openai_base_url=f"http://127.0.0.1:{unavailable_port}/up",
            gateway_intent="scratch-models",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 502)
            self.assertEqual(body["error_kind"], "network")
            self.assertIsNone(body["upstream_status"])
            self.assertIsInstance(body["message"], str)
            self.assertNotIn("data", body)
        finally:
            self.stop_gateway(proc)

    def test_relay_models_discovery_retries_network_failure(self):
        attempts = []

        def flaky_get_handler(conn):
            with conn:
                conn.recv(65536)
                attempts.append(time.time())
                if len(attempts) == 1:
                    return
                payload = b'{"data":[{"id":"glm-5.2","supported_parameters":["tools"]}]}'
                head = (
                    "HTTP/1.1 200 OK\r\n"
                    "Content-Type: application/json\r\n"
                    f"Content-Length: {len(payload)}\r\n\r\n"
                )
                conn.sendall(head.encode() + payload)

        upstream = RawUpstream(flaky_get_handler)
        proc, port = self.start_gateway(
            provider="relay",
            openai_base_url=f"http://127.0.0.1:{upstream.port}/up",
            gateway_intent="scratch-models",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual([m["id"] for m in body["data"]], ["glm-5.2"])
            self.assertEqual(len(attempts), 2)
        finally:
            self.stop_gateway(proc)
            upstream.close()

    def test_relay_kimi_stream_filters_server_tool_blocks(self):
        payload = b"".join([
            b'event: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n',
            b'event: content_block_stop\ndata: {"type":"content_block_stop","index":0}\n\n',
            b'event: content_block_start\ndata: {"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","name":"web_search"}}\n\n',
            b'event: content_block_delta\ndata: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta"}}\n\n',
            b'event: content_block_stop\ndata: {"type":"content_block_stop","index":1}\n\n',
            b'event: content_block_start\ndata: {"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","content":[]}}\n\n',
            b'event: content_block_stop\ndata: {"type":"content_block_stop","index":2}\n\n',
            b'event: content_block_start\ndata: {"type":"content_block_start","index":3,"content_block":{"type":"thinking","thinking":"","signature":""}}\n\n',
            b'event: content_block_stop\ndata: {"type":"content_block_stop","index":3}\n\n',
            b'event: content_block_start\ndata: {"type":"content_block_start","index":4,"content_block":{"type":"text","text":""}}\n\n',
            b'event: content_block_delta\ndata: {"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"OK"}}\n\n',
        ])

        def kimi_stream_handler(conn):
            with conn:
                conn.recv(65536)
                head = (
                    "HTTP/1.1 200 OK\r\n"
                    "Content-Type: text/event-stream\r\n"
                    "Transfer-Encoding: chunked\r\n\r\n"
                )
                conn.sendall(head.encode())
                try:
                    for chunk in (payload[:300], payload[300:]):
                        conn.sendall(hex(len(chunk))[2:].encode() + b"\r\n" + chunk + b"\r\n")
                    conn.sendall(b"0\r\n\r\n")
                except BrokenPipeError:
                    pass

        upstream = RawUpstream(kimi_stream_handler)
        proc, port = self.start_gateway(
            provider="relay",
            upstream_url=upstream.url,
            openai_base_url=f"http://127.0.0.1:{upstream.port}/up",
            openai_model="kimi-k2.7-code",
        )
        try:
            request = {
                "model": "claude-opus-4-8",
                "stream": True,
                "messages": [{"role": "user", "content": "hi"}],
            }
            raw = self.raw_request(
                port,
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Type: application/json\r\n"
                    + f"Content-Length: {len(json.dumps(request).encode())}\r\n".encode()
                    + b"Connection: close\r\n\r\n"
                    + json.dumps(request).encode()
                ),
            )
            status, headers, body = parse_raw_response(raw)
            self.assertEqual(status, 200)
            self.assertEqual(headers["transfer-encoding"], "chunked")
            self.assertNotIn(b"server_tool_use", body)
            self.assertNotIn(b"web_search_tool_result", body)
            self.assertIn(b'"type":"thinking","thinking":"","signature":""', body)
            self.assertIn(b'"index":1', body)
            self.assertIn(b'"index":2', body)
            self.assertIn(b'"text":"OK"', body)
        finally:
            self.stop_gateway(proc)
            upstream.close()

    def test_custom_openai_models_preserves_upstream_error(self):
        upstream = MockUpstream(b'{"error":"bad key"}', status=401)
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="openai-custom",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            gateway_intent="scratch-models",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 401)
            self.assertEqual(body["error_kind"], "upstream")
            self.assertEqual(body["upstream_status"], 401)
            self.assertNotIn("data", body)
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_custom_openai_models_preserves_upstream_502_as_upstream(self):
        upstream = MockUpstream(b'{"error":"upstream down"}', status=502)
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            provider="openai-custom",
            openai_base_url=f"http://127.0.0.1:{upstream.server_port}/up",
            gateway_intent="scratch-models",
        )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 502)
            self.assertEqual(body["error_kind"], "upstream")
            self.assertEqual(body["upstream_status"], 502)
            self.assertNotIn("data", body)
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_deepseek_formal_upstream_403_is_preserved_without_retry(self):
        upstream = MockUpstream(
            b'{"type":"error","error":{"type":"permission_error","message":"forbidden"}}',
            status=403,
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix="csswitch-current-deepseek-403-") as temp_home:
                proc, port = self.start_current_gateway(
                    provider="deepseek",
                    upstream_url=(
                        f"http://127.0.0.1:{upstream.server_port}"
                        "/anthropic/v1/messages"
                    ),
                    env_overrides={"HOME": temp_home},
                )
                try:
                    request_body = json.dumps(
                        {
                            "model": "claude-opus-4-8",
                            "messages": [{"role": "user", "content": "hi"}],
                        }
                    ).encode()
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    conn.request(
                        "POST",
                        "/secret/v1/messages",
                        body=request_body,
                        headers={"content-type": "application/json"},
                    )
                    resp = conn.getresponse()
                    raw_body = resp.read()
                    conn.close()
                    self.assertEqual(resp.status, 403)
                    self.assertEqual(resp.getheader("content-length"), str(len(raw_body)))
                    parsed = assert_error_shape(self, raw_body, "api_error")
                    self.assertIn("upstream 403", parsed["error"]["message"])
                    self.assertEqual(len(upstream.requests), 1)
                    captured = upstream.requests[0]
                    self.assertEqual(captured["path"], "/anthropic/v1/messages")
                    self.assertEqual(
                        captured["headers"]["x-api-key"], "fake-deepseek-key"
                    )
                finally:
                    self.stop_gateway(proc)
        finally:
            upstream.shutdown()
            upstream.server_close()

    def test_nonstream_upstream_errors_match_python_shape(self):
        cases = [
            (401, 401),
            (429, 429),
            (500, 502),
        ]
        upstream_body = (
            b'{"type":"error","error":{"type":"authentication_error",'
            b'"message":"mock upstream error"}}'
        )
        for upstream_status, expected_status in cases:
            with self.subTest(upstream_status=upstream_status):
                upstream = MockUpstream(upstream_body, status=upstream_status)
                thread = threading.Thread(target=upstream.serve_forever, daemon=True)
                thread.start()
                proc, port = self.start_gateway(
                    upstream_url=(
                        f"http://127.0.0.1:{upstream.server_port}"
                        "/anthropic/v1/messages"
                    )
                )
                try:
                    request_body = (
                        b'{"model":"claude-opus-4-8",'
                        b'"messages":[{"role":"user","content":"hi"}]}'
                    )
                    raw = self.raw_request(
                        port,
                        (
                            b"POST /secret/v1/messages HTTP/1.1\r\n"
                            b"Host: 127.0.0.1\r\n"
                            b"Content-Type: application/json\r\n"
                            + f"Content-Length: {len(request_body)}\r\n".encode()
                            + b"Connection: close\r\n\r\n"
                            + request_body
                        ),
                    )
                    status, headers, body = parse_raw_response(raw)
                    self.assertEqual(status, expected_status)
                    self.assertEqual(int(headers["content-length"]), len(body))
                    parsed = assert_error_shape(self, body, "api_error")
                    self.assertIn(f"upstream {upstream_status}", parsed["error"]["message"])
                finally:
                    self.stop_gateway(proc)
                    upstream.shutdown()
                    upstream.server_close()

    def test_malformed_requests_match_python_error_types(self):
        proc, port = self.start_gateway()
        try:
            cases = [
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: nope\r\n"
                    b"Connection: close\r\n\r\n",
                    400,
                    "invalid_request_error",
                    "invalid Content-Length",
                ),
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: -1\r\n"
                    b"Connection: close\r\n\r\n",
                    400,
                    "invalid_request_error",
                    "invalid Content-Length",
                ),
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Connection: close\r\n\r\n",
                    400,
                    "route_unknown",
                    "model selector is required",
                ),
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: 0\r\n"
                    b"Connection: close\r\n\r\n",
                    400,
                    "route_unknown",
                    "model selector is required",
                ),
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: 2\r\n"
                    b"Connection: close\r\n\r\n[]",
                    400,
                    "invalid_request_error",
                    "request body must be a JSON object",
                ),
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: 17\r\n"
                    b"Connection: close\r\n\r\n{\"messages\":null}",
                    400,
                    "route_unknown",
                    "model selector is required",
                ),
                (
                    b"POST /secret/v1/unknown HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: 2\r\n"
                    b"Connection: close\r\n\r\n{}",
                    404,
                    "not_found_error",
                    "/v1/unknown",
                ),
                (
                    b"GET /secret/nope HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Connection: close\r\n\r\n",
                    404,
                    "not_found_error",
                    "/nope",
                ),
            ]
            for raw_request, expected_status, error_type, message_part in cases:
                with self.subTest(error_type=error_type, message_part=message_part):
                    status, headers, body = parse_raw_response(
                        self.raw_request(port, raw_request)
                    )
                    self.assertEqual(status, expected_status)
                    self.assertEqual(int(headers["content-length"]), len(body))
                    parsed = assert_error_shape(self, body, error_type)
                    self.assertIn(message_part, parsed["error"]["message"])
        finally:
            self.stop_gateway(proc)

    def test_stream_passthrough_dechunks_same_payload(self):
        payload = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n"
        upstream = MockUpstream(payload, content_type="text/event-stream")
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=f"http://127.0.0.1:{upstream.server_port}/anthropic/v1/messages"
        )
        try:
            request = {
                "model": "claude-haiku-4-5",
                "stream": True,
                "messages": [{"role": "user", "content": "hi"}],
            }
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = resp.read()
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(resp.getheader("transfer-encoding"), "chunked")
            self.assertEqual(body, payload)
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_stream_keepalive_opens_before_upstream_first_byte(self):
        upstream = RawUpstream(delayed_stream_handler)
        proc, port = self.start_gateway(upstream_url=upstream.url)
        try:
            body = (
                b'{"model":"claude-opus-4-8","max_tokens":10,"stream":true,'
                b'"messages":[{"role":"user","content":"hi"}]}'
            )
            raw, elapsed = self.raw_post_until(port, body, b": csswitch-keepalive")
            self.assertIn(b"HTTP/1.1 200", raw)
            self.assertIn(b"content-type: text/event-stream", raw)
            self.assertIn(b": csswitch-keepalive", raw)
            self.assertLess(elapsed, 1.1)
        finally:
            self.stop_gateway(proc)
            upstream.close()

    def test_stream_upstream_status_after_headers_is_sse_error(self):
        upstream = MockUpstream(
            b'{"error":"bad key"}',
            content_type="application/json",
            status=401,
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=f"http://127.0.0.1:{upstream.server_port}/anthropic/v1/messages"
        )
        try:
            body = (
                b'{"model":"claude-opus-4-8","max_tokens":10,"stream":true,'
                b'"messages":[{"role":"user","content":"hi"}]}'
            )
            raw = self.raw_request(
                port,
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Type: application/json\r\n"
                    + f"Content-Length: {len(body)}\r\n".encode()
                    + b"Connection: close\r\n\r\n"
                    + body
                ),
            )
            head, _, tail = raw.partition(b"\r\n\r\n")
            self.assertIn(b"HTTP/1.1 200", head)
            self.assertIn(b"content-type: text/event-stream", head)
            self.assertIn(b"event: error", tail)
            self.assertIn(b'"type":"api_error"', tail)
            self.assertIn(b"upstream 401", tail)
            self.assertTrue(raw.rstrip().endswith(b"0"))
            self.assertNotIn(b"HTTP/1.1 401", raw)
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_stream_midstream_truncation_ends_with_sse_error(self):
        upstream = RawUpstream(dropping_stream_handler)
        proc, port = self.start_gateway(upstream_url=upstream.url)
        try:
            body = (
                b'{"model":"claude-opus-4-8","max_tokens":10,"stream":true,'
                b'"messages":[{"role":"user","content":"hi"}]}'
            )
            raw = self.raw_request(
                port,
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Type: application/json\r\n"
                    + f"Content-Length: {len(body)}\r\n".encode()
                    + b"Connection: close\r\n\r\n"
                    + body
                ),
            )
            head, _, tail = raw.partition(b"\r\n\r\n")
            self.assertIn(b"HTTP/1.1 200", head)
            self.assertIn(b"event: content_block_delta", tail)
            self.assertIn(b"event: error", tail)
            self.assertIn(b'"type":"api_error"', tail)
            self.assertTrue(raw.rstrip().endswith(b"0"))
        finally:
            self.stop_gateway(proc)
            upstream.close()

    def test_kimi_filtered_truncation_does_not_finalize_after_sse_error(self):
        payload = b"".join([
            b'event: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n',
            b'event: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"BUFFERED_KIMI"}}',
        ])
        upstream = RawUpstream(dropping_stream_handler_with_payload(payload))
        proc, port = self.start_gateway(
            provider="relay",
            upstream_url=upstream.url,
            openai_base_url=f"http://127.0.0.1:{upstream.port}/up",
            openai_model="kimi-k2.7-code",
        )
        try:
            request = {
                "model": "claude-opus-4-8",
                "stream": True,
                "messages": [{"role": "user", "content": "hi"}],
            }
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = resp.read()
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertIn(b"event: error", body)
            self.assertIn(b'"type":"api_error"', body)
            self.assertNotIn(b"BUFFERED_KIMI", body)
            self.assertEqual(body.count(b"event: error"), 1)
            self.assertEqual(body[body.index(b"event: error"):].count(b"event:"), 1)
            self.assertTrue(body.endswith(b"\n\n"))
        finally:
            self.stop_gateway(proc)
            upstream.close()

    def test_dsml_filtered_truncation_does_not_synthesize_after_sse_error(self):
        leak = (
            "<｜｜DSML｜｜tool_calls>"
            "<｜｜DSML｜｜invoke name=\"web_search\">"
            "<｜｜DSML｜｜parameter name=\"query\" string=\"true\">cats"
            "</｜｜DSML｜｜parameter>"
            "</｜｜DSML｜｜invoke>"
            "</｜｜DSML｜｜tool_calls>"
        )
        payload = b"".join([
            b'event: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n',
            (
                "event: content_block_delta\ndata: "
                + json.dumps({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": leak},
                }, ensure_ascii=False, separators=(",", ":"))
            ).encode(),
        ])
        upstream = RawUpstream(dropping_stream_handler_with_payload(payload))
        proc, port = self.start_gateway(
            upstream_url=upstream.url,
            shim_mode="rewrite",
        )
        try:
            request = {
                "model": "claude-opus-4-8",
                "stream": True,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{
                    "name": "web_search",
                    "input_schema": {
                        "type": "object",
                        "properties": {"query": {"type": "string"}},
                        "required": ["query"],
                    },
                }],
            }
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = resp.read()
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertIn(b"event: error", body)
            self.assertIn(b'"type":"api_error"', body)
            self.assertNotIn(b"tool_use", body)
            self.assertNotIn(b"toolu_dsml_", body)
            self.assertEqual(body.count(b"event: error"), 1)
            self.assertEqual(body[body.index(b"event: error"):].count(b"event:"), 1)
            self.assertTrue(body.endswith(b"\n\n"))
        finally:
            self.stop_gateway(proc)
            upstream.close()

    def test_connect_status_contract_and_local_tunnel(self):
        proc, port = self.start_gateway()
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
                sock.sendall(b"CONNECT claude.ai:443 HTTP/1.1\r\nhost: claude.ai:443\r\n\r\n")
                head = recv_http_head(sock)
            self.assertEqual(head.split(b"\r\n", 1)[0], b"HTTP/1.1 401 Unauthorized")

            with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
                sock.sendall(b"CONNECT missing-port HTTP/1.1\r\nhost: missing-port\r\n\r\n")
                head = recv_http_head(sock)
            self.assertEqual(head.split(b"\r\n", 1)[0], b"HTTP/1.1 400 Bad Request")

            with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
                sock.sendall(b"CONNECT example.test:not-a-port HTTP/1.1\r\nhost: example.test:not-a-port\r\n\r\n")
                head = recv_http_head(sock)
            self.assertEqual(head.split(b"\r\n", 1)[0], b"HTTP/1.1 400 Bad Request")

            with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
                sock.sendall(b"CONNECT []:443 HTTP/1.1\r\nhost: []:443\r\n\r\n")
                head = recv_http_head(sock)
            self.assertEqual(head.split(b"\r\n", 1)[0], b"HTTP/1.1 400 Bad Request")

            with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
                sock.sendall(b"CONNECT 127.0.0.1:0 HTTP/1.1\r\nhost: 127.0.0.1:0\r\n\r\n")
                head = recv_http_head(sock)
            self.assertEqual(head.split(b"\r\n", 1)[0], b"HTTP/1.1 502 Bad Gateway")

            echo = EchoServer()
            with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
                target = f"CONNECT 127.0.0.1:{echo.port} HTTP/1.1\r\nhost: 127.0.0.1:{echo.port}\r\n\r\n"
                sock.sendall(target.encode())
                data = recv_http_head(sock)
                self.assertEqual(data.split(b"\r\n", 1)[0], b"HTTP/1.1 200 Connection Established")
                sock.sendall(b"ping")
                self.assertEqual(sock.recv(4), b"ping")
        finally:
            self.stop_gateway(proc)


if __name__ == "__main__":
    unittest.main()
