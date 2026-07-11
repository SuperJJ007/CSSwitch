import http.client
import json
import os
import select
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock as mocklib

import test.provider_mock_scenarios as scenario_module
from test.provider_mock_scenarios import (
    ACTION_TYPES,
    DEFAULT_MANIFEST,
    EvidenceStore,
    RunningScenarioMock,
    ScenarioControlPlane,
    ScenarioError,
    load_manifest,
    read_control_token_fd,
    read_secrets_fd,
    scenario_from_steps,
    start_manifest_scenario,
    start_owned_tcp_echo,
    start_scenario,
)


SAFE_TMP = os.path.realpath(tempfile.gettempdir())
MODULE_PATH = Path(__file__).with_name("provider_mock_scenarios.py")


def request(mock, method, path, body=None, headers=None, timeout=4):
    conn = http.client.HTTPConnection(mock.host, mock.port, timeout=timeout)
    encoded = body
    if isinstance(body, (dict, list)):
        encoded = json.dumps(body).encode("utf-8")
        headers = dict(headers or {})
        headers.setdefault("Content-Type", "application/json")
    conn.request(method, path, body=encoded, headers=headers or {})
    response = conn.getresponse()
    raw = response.read()
    result = (response.status, response.getheader("Content-Type"), raw)
    conn.close()
    return result


def wait_for_request_count(mock, count, timeout=2):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if len(mock.result()["requests"]) >= count:
            return True
        time.sleep(0.01)
    return False


def control_request(socket_path, token, command, **fields):
    payload = {"token": token, "command": command, **fields}
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(4)
    client.connect(str(socket_path))
    client.sendall(json.dumps(payload).encode("utf-8") + b"\n")
    client.shutdown(socket.SHUT_WR)
    data = b""
    while b"\n" not in data:
        chunk = client.recv(65_536)
        if not chunk:
            break
        data += chunk
    client.close()
    return json.loads(data.split(b"\n", 1)[0])


def pipe_with(payload):
    read_fd, write_fd = os.pipe()
    try:
        os.write(write_fd, payload)
    finally:
        os.close(write_fd)
    return read_fd


def set_rst_on_close(sock):
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))


class ProviderMockScenarioTests(unittest.TestCase):
    def setUp(self):
        self.mocks = []

    def tearDown(self):
        for mock in reversed(self.mocks):
            try:
                mock.stop()
            except (RuntimeError, ScenarioError):
                pass

    def start(self, name, steps, phases=None, secrets=None, evidence=None):
        mock = start_scenario(
            scenario_from_steps(name, steps, phases=phases),
            secrets=secrets,
            evidence=evidence,
        )
        self.mocks.append(mock)
        return mock

    def test_manifest_has_repeatable_provider_contract_and_fault_matrix(self):
        scenarios = load_manifest(DEFAULT_MANIFEST)
        provider_names = {
            "installed_deepseek_matrix",
            "installed_qwen_matrix",
            "installed_openai_chat_matrix",
            "installed_openai_responses_matrix",
            "installed_relay_matrix",
        }
        self.assertTrue(provider_names.issubset(scenarios))
        expected_phases = ("discovery", "scratch", "formal", "reuse", "restart")
        expected_paths = {
            "installed_deepseek_matrix": "/deepseek/v1/messages",
            "installed_qwen_matrix": "/qwen/v1/chat/completions",
            "installed_openai_chat_matrix": "/openai/v1/chat/completions",
            "installed_openai_responses_matrix": "/responses/v1/responses",
            "installed_relay_matrix": "/relay/v1/messages",
        }
        for name in provider_names:
            scenario = scenarios[name]
            self.assertEqual(scenario.phases, expected_phases)
            reuse = [step for step in scenario.steps if step.phase == "reuse"]
            self.assertEqual(len(reuse), 2, name)
            self.assertTrue(all(step.path == expected_paths[name] for step in reuse))

        deepseek = scenarios["installed_deepseek_matrix"].steps[0]
        self.assertEqual(deepseek.checks["body"]["equals"]["/model"], "deepseek-v4-pro")
        self.assertEqual(deepseek.checks["body"]["equals"]["/max_tokens"], 1)
        self.assertNotIn("/thinking/type", deepseek.checks["body"]["equals"])
        self.assertEqual(deepseek.checks["headers"]["anthropic-version"]["equals"], "2023-06-01")
        deepseek_formal = scenarios["installed_deepseek_matrix"].steps[1]
        self.assertEqual(deepseek_formal.checks["body"]["equals"]["/max_tokens"], 65536)
        self.assertEqual(deepseek_formal.checks["body"]["equals"]["/thinking/type"], "adaptive")

        qwen_tool = scenarios["installed_qwen_matrix"].steps[2]
        self.assertEqual(qwen_tool.checks["body"]["equals"]["/max_tokens"], 8192)
        self.assertEqual(qwen_tool.checks["body"]["equals"]["/messages/2/tool_call_id"], "toolu_1")

        custom = scenarios["installed_openai_chat_matrix"].steps[1]
        self.assertEqual(custom.checks["body"]["equals"]["/model"], "glm-4.5")
        self.assertEqual(custom.checks["body"]["equals"]["/max_tokens"], 1)
        custom_formal = scenarios["installed_openai_chat_matrix"].steps[2]
        self.assertEqual(custom_formal.checks["body"]["equals"]["/max_tokens"], 1000000)

        responses = scenarios["installed_openai_responses_matrix"].steps[2]
        self.assertEqual(responses.checks["body"]["equals"]["/max_output_tokens"], 65536)
        self.assertEqual(responses.checks["body"]["equals"]["/input/1/type"], "function_call")
        self.assertEqual(responses.checks["body"]["equals"]["/input/2/type"], "function_call_output")
        self.assertEqual(responses.checks["body"]["equals"]["/input/2/call_id"], "toolu_1")

        relay = scenarios["installed_relay_matrix"]
        kimi = next(step for step in relay.steps if step.step_id == "relay-formal-kimi-thinking-filter")
        self.assertEqual(kimi.checks["body"]["equals"]["/thinking/budget_tokens"], 1024)
        self.assertIn("/tools/2", kimi.checks["body"]["absent"])
        self.assertIn("/tool_choice", kimi.checks["body"]["absent"])

        for prefix in ("deepseek", "qwen", "openai_chat", "openai_responses", "relay"):
            statuses = scenarios[f"{prefix}_status_matrix"]
            self.assertEqual(
                [step.action["status"] for step in statuses.steps],
                [401, 403, 429, 503],
            )
        retry = scenarios["retry_message_drop3_then_success"].steps[0]
        self.assertEqual(retry.action["drops"], 3)
        keepalive = scenarios["keepalive_stream_delay_1_2s_then_anthropic_sse"].steps[0]
        self.assertGreater(keepalive.action["seconds"], 1)
        self.assertEqual(keepalive.action["then"]["type"], "anthropic_sse")

    def test_manifest_and_nested_schema_are_strict(self):
        with DEFAULT_MANIFEST.open("r", encoding="utf-8") as handle:
            base = json.load(handle)
        with tempfile.TemporaryDirectory(dir=SAFE_TMP) as root:
            root_path = Path(root)

            def rejected(mutator, message):
                value = json.loads(json.dumps(base))
                mutator(value)
                path = root_path / f"case-{time.monotonic_ns()}.json"
                path.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaisesRegex(ScenarioError, message):
                    load_manifest(path)

            rejected(lambda value: value.__setitem__("extra", True), "unknown keys")
            rejected(lambda value: value.__setitem__("version", True), "schema/version")
            rejected(lambda value: value.__setitem__("actions", value["actions"][:-1]), "action catalog")
            rejected(
                lambda value: value["scenarios"]["stalled_message"].__setitem__("extra", True),
                "unknown keys",
            )
            rejected(
                lambda value: value["scenarios"]["stalled_message"]["steps"][0]["action"].__setitem__("extra", True),
                "unknown keys",
            )

        invalid_actions = [
            {"type": "models_json", "models": []},
            {"type": "truncated", "missing_bytes": 0},
            {"type": "status", "status": 199},
            {"type": "anthropic_sse", "chunk_delay": -1},
            {"type": "delay", "seconds": 1, "then": {"type": "status", "extra": True}},
        ]
        for action in invalid_actions:
            with self.subTest(action=action):
                with self.assertRaises(ScenarioError):
                    scenario_from_steps(
                        "invalid",
                        [{"id": "bad", "phase": "bad", "method": "GET", "path": "/", "action": action}],
                    )
        with self.assertRaisesRegex(ScenarioError, "sensitive header"):
            scenario_from_steps(
                "literal-secret",
                [{
                    "id": "bad",
                    "phase": "bad",
                    "method": "GET",
                    "path": "/",
                    "checks": {"headers": {"authorization": {"equals": "Bearer embedded"}}},
                    "action": "status",
                }],
            )

    def test_initial_phase_is_unarmed_and_unexpected_request_is_terminal(self):
        mock = self.start(
            "unarmed",
            [{"id": "one", "phase": "formal", "method": "GET", "path": "/expected", "action": "status"}],
        )
        self.assertIsNone(mock.ready()["phase"])
        secret_path = "/wrong/path-secret-never-record"
        self.assertEqual(request(mock, "GET", secret_path)[0], 409)
        result = mock.result()
        self.assertEqual(result["failures"][0]["kind"], "phase_not_entered")
        self.assertNotIn(secret_path, json.dumps(result, sort_keys=True))
        self.assertFalse(result["protocol_complete"])

    def test_empty_discovery_phase_and_ordered_keepalive_require_explicit_entry(self):
        mock = self.start(
            "phase-order",
            [
                {"id": "scratch", "phase": "scratch", "method": "GET", "path": "/scratch", "action": "status"},
                {"id": "formal", "phase": "formal", "method": "POST", "path": "/formal", "checks": {"body": {"json": True}}, "action": "anthropic_json"},
            ],
            phases=["discovery", "scratch", "formal"],
        )
        mock.enter_phase("discovery")
        self.assertEqual(mock.result()["active_phase"], "discovery")
        mock.enter_phase("scratch")
        conn = http.client.HTTPConnection(mock.host, mock.port, timeout=3)
        conn.request("GET", "/scratch")
        first = conn.getresponse()
        self.assertEqual(first.status, 200)
        first.read()
        mock.enter_phase("formal")
        body = b"{}"
        conn.request("POST", "/formal", body=body, headers={"Content-Type": "application/json"})
        second = conn.getresponse()
        self.assertEqual(second.status, 200)
        self.assertEqual(json.loads(second.read())["type"], "message")
        conn.close()
        before_stop = mock.result()
        self.assertTrue(before_stop["protocol_complete"])
        self.assertFalse(before_stop["final_ok"])
        after_stop = mock.stop()
        self.assertTrue(after_stop["final_ok"])
        self.assertFalse(after_stop["server_thread_alive"])

    def test_secret_matchers_are_exact_and_results_only_retain_booleans(self):
        fake_key = "fake-provider-key-never-record-731"
        wrong_key = "wrong-provider-key-never-record-499"
        steps = [{
            "id": "secret",
            "phase": "formal",
            "method": "POST",
            "path": "/secret",
            "checks": {
                "headers": {
                    "authorization": {"bearer_secret": "provider_key"},
                    "x-api-key": {"equals_secret": "provider_key"},
                },
                "body": {"json": True, "required": ["/messages"]},
            },
            "action": "anthropic_json",
        }]
        with self.assertRaisesRegex(ScenarioError, "missing in-memory secrets"):
            start_scenario(scenario_from_steps("missing", steps))

        mock = self.start("exact-secret", steps, secrets={"provider_key": fake_key})
        mock.enter_phase("formal")
        status, _, _ = request(
            mock,
            "POST",
            "/secret",
            {"messages": [], "ignored_secret": "body-secret-never-record"},
            {"Authorization": f"Bearer {fake_key}", "X-Api-Key": fake_key},
        )
        self.assertEqual(status, 200)
        result = mock.stop()
        encoded = json.dumps(result, sort_keys=True)
        self.assertNotIn(fake_key, encoded)
        self.assertNotIn("body-secret-never-record", encoded)
        self.assertEqual(
            result["requests"][0]["request"]["header_matches"],
            {"authorization": True, "x-api-key": True},
        )

        failed = self.start("wrong-secret", steps, secrets={"provider_key": fake_key})
        failed.enter_phase("formal")
        self.assertEqual(
            request(
                failed,
                "POST",
                "/secret",
                {"messages": []},
                {"Authorization": f"Bearer {wrong_key}", "X-Api-Key": wrong_key},
            )[0],
            422,
        )
        failed_result = failed.stop()
        failed_encoded = json.dumps(failed_result, sort_keys=True)
        self.assertNotIn(fake_key, failed_encoded)
        self.assertNotIn(wrong_key, failed_encoded)

    def test_control_token_and_secret_payloads_reject_regular_file_fds(self):
        with tempfile.TemporaryDirectory(dir=SAFE_TMP) as root:
            token_path = Path(root) / "token"
            token_path.write_text("regular-file-token-value", encoding="utf-8")
            token_fd = os.open(token_path, os.O_RDONLY)
            with self.assertRaisesRegex(ScenarioError, "anonymous pipe/socket"):
                read_control_token_fd(token_fd)
            secret_path = Path(root) / "secrets"
            secret_path.write_text('{"provider_key":"value"}', encoding="utf-8")
            secret_fd = os.open(secret_path, os.O_RDONLY)
            with self.assertRaisesRegex(ScenarioError, "anonymous pipe/socket"):
                read_secrets_fd(secret_fd)

    def test_evidence_directory_and_atomic_targets_reject_unsafe_state(self):
        with tempfile.TemporaryDirectory(dir=SAFE_TMP) as root:
            root_path = Path(root)
            os.chmod(root_path, 0o700)

            store = EvidenceStore.create(root_path / "safe-evidence")
            store.write_ready({"safe": True})
            store.append_hit({"header_match": True})
            store.write_result({"final_ok": True})
            for name in ("ready.json", "hits.jsonl", "result.json"):
                info = os.lstat(store.path / name)
                self.assertTrue(stat.S_ISREG(info.st_mode))
                self.assertEqual(stat.S_IMODE(info.st_mode), 0o600)
            self.assertEqual(stat.S_IMODE(os.lstat(store.path).st_mode), 0o700)
            store.close()

            old = EvidenceStore.create(root_path / "old-target")
            old_ready = old.path / "ready.json"
            old_ready.write_text("do-not-truncate", encoding="utf-8")
            os.chmod(old_ready, 0o644)
            with self.assertRaisesRegex(ScenarioError, "pre-existing"):
                old.write_ready({"safe": True})
            self.assertEqual(old_ready.read_text(encoding="utf-8"), "do-not-truncate")
            self.assertEqual(stat.S_IMODE(os.lstat(old_ready).st_mode), 0o644)
            old.close()

            victim = root_path / "victim"
            victim.write_text("victim-safe", encoding="utf-8")
            symlink_store = EvidenceStore.create(root_path / "symlink-target")
            os.symlink(victim, symlink_store.path / "ready.json")
            with self.assertRaisesRegex(ScenarioError, "pre-existing"):
                symlink_store.write_ready({"safe": True})
            self.assertEqual(victim.read_text(encoding="utf-8"), "victim-safe")
            symlink_store.close()

            fifo_store = EvidenceStore.create(root_path / "fifo-target")
            os.mkfifo(fifo_store.path / "ready.json", 0o600)
            with self.assertRaisesRegex(ScenarioError, "pre-existing"):
                fifo_store.write_ready({"safe": True})
            fifo_store.close()

            actual_parent = root_path / "actual-parent"
            actual_parent.mkdir(mode=0o700)
            linked_parent = root_path / "linked-parent"
            os.symlink(actual_parent, linked_parent)
            with self.assertRaisesRegex(ScenarioError, "symlink"):
                EvidenceStore.create(linked_parent / "evidence")

    def test_ready_hits_and_final_result_are_durable_redacted_evidence(self):
        fake_key = "evidence-fake-key-never-record"
        with tempfile.TemporaryDirectory(dir=SAFE_TMP) as root:
            root_path = Path(root)
            os.chmod(root_path, 0o700)
            evidence = EvidenceStore.create(root_path / "evidence")
            mock = self.start(
                "evidence",
                [{
                    "id": "hit",
                    "phase": "formal",
                    "method": "GET",
                    "path": "/hit",
                    "checks": {"headers": {"x-api-key": {"equals_secret": "provider_key"}}},
                    "action": "status",
                }],
                secrets={"provider_key": fake_key},
                evidence=evidence,
            )
            mock.write_ready_evidence()
            mock.enter_phase("formal")
            self.assertEqual(request(mock, "GET", "/hit", headers={"X-Api-Key": fake_key})[0], 200)
            hits_before_stop = (evidence.path / "hits.jsonl").read_text(encoding="utf-8")
            self.assertIn('"header_matches":{"x-api-key":true}', hits_before_stop)
            self.assertNotIn(fake_key, hits_before_stop)
            final = mock.stop()
            self.assertTrue(final["final_ok"])
            result_file = json.loads((evidence.path / "result.json").read_text(encoding="utf-8"))
            self.assertTrue(result_file["final_ok"])
            scan = "".join(
                (evidence.path / name).read_text(encoding="utf-8")
                for name in ("ready.json", "hits.jsonl", "result.json")
            )
            self.assertNotIn(fake_key, scan)
            evidence.close()

    def test_real_cli_two_phase_control_secret_fd_and_crash_resilient_hits(self):
        token = "control-token-never-record-88231"
        fake_key = "cli-fake-provider-key-never-record-9102"
        with tempfile.TemporaryDirectory(dir=SAFE_TMP) as root:
            root_path = Path(root)
            os.chmod(root_path, 0o700)
            manifest_path = root_path / "two-phase.json"
            manifest = {
                "schema": "csswitch.provider-mock-scenarios",
                "version": 1,
                "actions": sorted(ACTION_TYPES),
                "scenarios": {
                    "two-phase": {
                        "phases": ["one", "two"],
                        "steps": [
                            {
                                "id": "one",
                                "phase": "one",
                                "method": "GET",
                                "path": "/one",
                                "checks": {"headers": {"authorization": {"bearer_secret": "provider_key"}, "x-api-key": {"equals_secret": "provider_key"}}},
                                "action": "status",
                            },
                            {
                                "id": "two",
                                "phase": "two",
                                "method": "GET",
                                "path": "/two",
                                "checks": {"headers": {"authorization": {"bearer_secret": "provider_key"}, "x-api-key": {"equals_secret": "provider_key"}}},
                                "action": "status",
                            },
                        ],
                    }
                },
            }
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            evidence_path = root_path / "evidence"
            token_fd = pipe_with(token.encode("utf-8"))
            secrets_fd = pipe_with(json.dumps({"provider_key": fake_key}).encode("utf-8"))
            argv = [
                sys.executable,
                str(MODULE_PATH),
                "--manifest",
                str(manifest_path),
                "--scenario",
                "two-phase",
                "--evidence-dir",
                str(evidence_path),
                "--control-token-fd",
                str(token_fd),
                "--secrets-fd",
                str(secrets_fd),
            ]
            self.assertNotIn(token, argv)
            self.assertNotIn(fake_key, argv)
            proc = subprocess.Popen(
                argv,
                pass_fds=(token_fd, secrets_fd),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            os.close(token_fd)
            os.close(secrets_fd)
            stdout = ""
            stderr = ""
            try:
                readable, _, _ = select.select([proc.stdout], [], [], 5)
                self.assertTrue(readable, "CLI did not emit ready JSON")
                ready_line = proc.stdout.readline()
                ready = json.loads(ready_line)
                self.assertIsNone(ready["phase"])
                self.assertEqual(ready["control_socket"], "control.sock")
                self.assertFalse(ready["executable_verified_by_driver"])
                self.assertFalse(ready["port_closed_verified_by_driver"])
                control_path = evidence_path / ready["control_socket"]
                control_info = os.lstat(control_path)
                self.assertTrue(stat.S_ISSOCK(control_info.st_mode))
                self.assertEqual(stat.S_IMODE(control_info.st_mode), 0o600)

                unauthorized = control_request(control_path, "wrong-token-value-123", "status")
                self.assertEqual(unauthorized, {"ok": False, "error": "unauthorized"})
                self.assertNotIn(token, json.dumps(unauthorized))

                entered = control_request(control_path, token, "enter_phase", phase="one")
                self.assertTrue(entered["ok"])
                headers = {"Authorization": f"Bearer {fake_key}", "X-Api-Key": fake_key}
                conn = http.client.HTTPConnection(ready["host"], ready["port"], timeout=3)
                conn.request("GET", "/one", headers=headers)
                response = conn.getresponse()
                self.assertEqual(response.status, 200)
                response.read()
                conn.close()

                hits_now = (evidence_path / "hits.jsonl").read_text(encoding="utf-8")
                self.assertEqual(len(hits_now.splitlines()), 1)
                self.assertNotIn(fake_key, hits_now)
                self.assertNotIn(token, hits_now)

                status = control_request(control_path, token, "status")
                self.assertTrue(status["ok"])
                self.assertFalse(status["status"]["protocol_complete"])
                entered = control_request(control_path, token, "enter_phase", phase="two")
                self.assertTrue(entered["ok"])
                self.assertEqual(request(type("M", (), {"host": ready["host"], "port": ready["port"]})(), "GET", "/two", headers=headers)[0], 200)
                waited = control_request(control_path, token, "wait", timeout_ms=2000)
                self.assertTrue(waited["completed"])
                self.assertTrue(waited["status"]["protocol_complete"])
                self.assertFalse(waited["status"]["final_ok"])
                stopped = control_request(control_path, token, "stop")
                self.assertEqual(stopped, {"ok": True, "accepted": True})
                stdout, stderr = proc.communicate(timeout=6)
                stdout = ready_line + stdout
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        extra_out, extra_err = proc.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        extra_out, extra_err = proc.communicate(timeout=5)
                    stdout += extra_out
                    stderr += extra_err
            self.assertEqual(proc.returncode, 0, stderr)
            final = json.loads((evidence_path / "result.json").read_text(encoding="utf-8"))
            self.assertTrue(final["final_ok"])
            self.assertFalse(final["server_thread_alive"])
            self.assertFalse(control_path.exists())
            for name in ("ready.json", "hits.jsonl", "result.json"):
                self.assertEqual(stat.S_IMODE(os.lstat(evidence_path / name).st_mode), 0o600)
            self.assertEqual(stat.S_IMODE(os.lstat(evidence_path).st_mode), 0o700)
            scan = manifest_path.read_text(encoding="utf-8") + stdout + stderr
            scan += "".join(
                (evidence_path / name).read_text(encoding="utf-8")
                for name in ("ready.json", "hits.jsonl", "result.json")
            )
            self.assertNotIn(token, scan)
            self.assertNotIn(fake_key, scan)
            with self.assertRaises(OSError):
                socket.create_connection((ready["host"], ready["port"]), timeout=0.2)

    def test_cli_forced_crash_keeps_fsynced_redacted_hits_without_final_result(self):
        token = "crash-control-token-never-record-44"
        with tempfile.TemporaryDirectory(dir=SAFE_TMP) as root:
            root_path = Path(root)
            os.chmod(root_path, 0o700)
            evidence_path = root_path / "evidence"
            token_fd = pipe_with(token.encode("utf-8"))
            argv = [
                sys.executable,
                str(MODULE_PATH),
                "--scenario",
                "status_get_post",
                "--evidence-dir",
                str(evidence_path),
                "--control-token-fd",
                str(token_fd),
            ]
            proc = subprocess.Popen(
                argv,
                pass_fds=(token_fd,),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            os.close(token_fd)
            try:
                readable, _, _ = select.select([proc.stdout], [], [], 5)
                self.assertTrue(readable)
                ready_line = proc.stdout.readline()
                ready = json.loads(ready_line)
                control_path = evidence_path / ready["control_socket"]
                self.assertTrue(
                    control_request(control_path, token, "enter_phase", phase="status")["ok"]
                )
                self.assertEqual(request(type("M", (), {"host": ready["host"], "port": ready["port"]})(), "GET", "/status/200")[0], 200)
                hits_path = evidence_path / "hits.jsonl"
                deadline = time.monotonic() + 2
                while time.monotonic() < deadline and not hits_path.read_text(encoding="utf-8"):
                    time.sleep(0.01)
                proc.kill()
                stdout, stderr = proc.communicate(timeout=5)
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate(timeout=5)
            self.assertNotEqual(proc.returncode, 0)
            self.assertTrue((evidence_path / "ready.json").is_file())
            self.assertTrue((evidence_path / "hits.jsonl").read_text(encoding="utf-8").strip())
            self.assertFalse((evidence_path / "result.json").exists())
            self.assertEqual(stat.S_IMODE(os.lstat(evidence_path / "hits.jsonl").st_mode), 0o600)
            scan = ready_line + stdout + stderr
            scan += (evidence_path / "ready.json").read_text(encoding="utf-8")
            scan += (evidence_path / "hits.jsonl").read_text(encoding="utf-8")
            self.assertNotIn(token, scan)

    def test_cli_partial_start_failures_reverse_cleanup_owned_resources(self):
        original_mock_start = RunningScenarioMock.start

        for failure_point in ("control-start", "ready-write", "ready-stdout"):
            with self.subTest(failure_point=failure_point):
                with tempfile.TemporaryDirectory(dir=SAFE_TMP) as root:
                    root_path = Path(root)
                    os.chmod(root_path, 0o700)
                    evidence_path = root_path / "evidence"
                    token_fd = pipe_with(
                        f"startup-token-{failure_point}-12345".encode("utf-8")
                    )
                    argv = [
                        "--scenario",
                        "status_get_post",
                        "--evidence-dir",
                        str(evidence_path),
                        "--control-token-fd",
                        str(token_fd),
                    ]
                    started_mocks = []

                    def recording_start(instance, *args, **kwargs):
                        value = original_mock_start(instance, *args, **kwargs)
                        started_mocks.append(instance)
                        return value

                    patches = [
                        mocklib.patch.object(
                            RunningScenarioMock, "start", new=recording_start
                        )
                    ]
                    if failure_point == "control-start":
                        patches.append(
                            mocklib.patch.object(
                                ScenarioControlPlane,
                                "start",
                                side_effect=RuntimeError("injected control start failure"),
                            )
                        )
                    elif failure_point == "ready-write":
                        patches.append(
                            mocklib.patch.object(
                                RunningScenarioMock,
                                "write_ready_evidence",
                                side_effect=RuntimeError("injected ready write failure"),
                            )
                        )
                    else:
                        calls = {"count": 0}

                        def fail_first_print(*_args, **_kwargs):
                            calls["count"] += 1
                            if calls["count"] == 1:
                                raise BrokenPipeError("injected ready stdout failure")

                        patches.append(mocklib.patch("builtins.print", new=fail_first_print))

                    entered = []
                    try:
                        for patcher in patches:
                            patcher.start()
                            entered.append(patcher)
                        if failure_point != "ready-stdout":
                            print_patch = mocklib.patch("builtins.print")
                            print_patch.start()
                            entered.append(print_patch)
                        with self.assertRaises((RuntimeError, BrokenPipeError)):
                            scenario_module._main(argv)
                    finally:
                        for patcher in reversed(entered):
                            patcher.stop()

                    self.assertEqual(len(started_mocks), 1)
                    owned = started_mocks[0]
                    self.assertFalse(owned.thread_alive)
                    with self.assertRaises(OSError):
                        socket.create_connection((owned.host, owned.port), timeout=0.2)
                    self.assertFalse(os.path.lexists(evidence_path / "control.sock"))
                    result = json.loads(
                        (evidence_path / "result.json").read_text(encoding="utf-8")
                    )
                    self.assertFalse(result["final_ok"])
                    self.assertFalse(result["ok"])
                    self.assertIn(
                        "cli_start_or_cleanup_failed",
                        [failure["kind"] for failure in result["failures"]],
                    )
                    live_owned_threads = [
                        thread.name
                        for thread in threading.enumerate()
                        if thread.name.startswith("provider-mock-")
                    ]
                    self.assertEqual(live_owned_threads, [])

    def test_cli_control_stop_failure_cannot_publish_green_result(self):
        token = "control-stop-failure-token-12345"
        with tempfile.TemporaryDirectory(dir=SAFE_TMP) as root:
            root_path = Path(root)
            os.chmod(root_path, 0o700)
            evidence_path = root_path / "evidence"
            token_fd = pipe_with(token.encode("utf-8"))
            argv = [
                "--scenario",
                "status_get_post",
                "--evidence-dir",
                str(evidence_path),
                "--control-token-fd",
                str(token_fd),
            ]
            original_control_start = ScenarioControlPlane.start
            original_control_stop = ScenarioControlPlane.stop

            def start_complete_scenario(instance):
                value = original_control_start(instance)
                instance.mock.enter_phase("status")
                self.assertEqual(request(instance.mock, "GET", "/status/200")[0], 200)
                self.assertEqual(
                    request(instance.mock, "POST", "/status/503", {})[0], 503
                )
                self.assertTrue(instance.mock.result()["protocol_complete"])
                instance._stop_requested.set()
                return value

            def stop_then_fail(instance, *args, **kwargs):
                original_control_stop(instance, *args, **kwargs)
                self.assertFalse(
                    (evidence_path / "result.json").exists(),
                    "result must not be published before control cleanup returns",
                )
                raise RuntimeError("injected control stop failure")

            with mocklib.patch.object(
                ScenarioControlPlane, "start", new=start_complete_scenario
            ), mocklib.patch.object(
                ScenarioControlPlane, "stop", new=stop_then_fail
            ), mocklib.patch("builtins.print"):
                with self.assertRaisesRegex(RuntimeError, "control stop failure"):
                    scenario_module._main(argv)
            result = json.loads(
                (evidence_path / "result.json").read_text(encoding="utf-8")
            )
            self.assertTrue(result["protocol_complete"])
            self.assertTrue(result["stopped"])
            self.assertFalse(result["server_thread_alive"])
            self.assertFalse(result["final_ok"])
            self.assertFalse(result["ok"])
            self.assertIn(
                "cli_start_or_cleanup_failed",
                [failure["kind"] for failure in result["failures"]],
            )
            self.assertFalse(os.path.lexists(evidence_path / "control.sock"))
            self.assertEqual(
                [
                    thread.name
                    for thread in threading.enumerate()
                    if thread.name.startswith("provider-mock-")
                ],
                [],
            )

    def test_drop_three_then_success_uses_explicit_retry_outcomes(self):
        mock = self.start(
            "retry",
            [{
                "id": "retry",
                "phase": "formal",
                "method": "GET",
                "path": "/retry",
                "action": {"type": "drop_then_success", "drops": 3, "then": "status"},
            }],
        )
        mock.enter_phase("formal")
        for index in range(3):
            with self.assertRaises((http.client.RemoteDisconnected, ConnectionResetError)):
                request(mock, "GET", "/retry")
            self.assertTrue(wait_for_request_count(mock, index + 1))
        self.assertEqual(request(mock, "GET", "/retry")[0], 200)
        result = mock.stop()
        self.assertEqual(
            [item["outcome"] for item in result["requests"]],
            ["RETRY", "RETRY", "RETRY", "CONSUMED"],
        )
        self.assertTrue(result["final_ok"])

    def test_normal_json_rst_before_headers_is_terminal_failed_not_consumed(self):
        mock = self.start(
            "json-rst",
            [{
                "id": "json",
                "phase": "formal",
                "method": "POST",
                "path": "/json",
                "action": {"type": "delay", "seconds": 0.2, "then": "anthropic_json"},
            }],
        )
        mock.enter_phase("formal")
        client = socket.create_connection((mock.host, mock.port), timeout=2)
        client.sendall(b"POST /json HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n")
        set_rst_on_close(client)
        client.close()
        self.assertTrue(mock.wait_complete(2))
        result = mock.stop()
        self.assertFalse(result["protocol_complete"])
        self.assertEqual(result["requests"][0]["outcome"], "FAILED")
        self.assertFalse(result["requests"][0]["consumed"])
        self.assertEqual(result["failures"][0]["kind"], "response_write_failed")

    def test_drop_then_success_write_failure_does_not_consume_step(self):
        mock = self.start(
            "retry-write-failure",
            [{
                "id": "retry",
                "phase": "formal",
                "method": "POST",
                "path": "/retry",
                "action": {
                    "type": "drop_then_success",
                    "drops": 1,
                    "then": {"type": "delay", "seconds": 0.2, "then": "anthropic_json"},
                },
            }],
        )
        mock.enter_phase("formal")
        with self.assertRaises((http.client.RemoteDisconnected, ConnectionResetError)):
            request(mock, "POST", "/retry")
        self.assertTrue(wait_for_request_count(mock, 1))
        client = socket.create_connection((mock.host, mock.port), timeout=2)
        client.sendall(b"POST /retry HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n")
        set_rst_on_close(client)
        client.close()
        self.assertTrue(mock.wait_complete(2))
        result = mock.stop()
        self.assertEqual(
            [item["outcome"] for item in result["requests"]], ["RETRY", "FAILED"]
        )
        self.assertFalse(result["requests"][1]["consumed"])
        self.assertFalse(result["protocol_complete"])

    def test_sse_midstream_rst_is_terminal_failed_not_consumed(self):
        mock = self.start(
            "sse-rst",
            [{
                "id": "sse",
                "phase": "formal",
                "method": "POST",
                "path": "/sse",
                "action": {"type": "anthropic_sse", "chunk_delay": 0.2},
            }],
        )
        mock.enter_phase("formal")
        client = socket.create_connection((mock.host, mock.port), timeout=2)
        client.sendall(b"POST /sse HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n")
        received = b""
        while b"event: message_start" not in received:
            received += client.recv(4096)
        set_rst_on_close(client)
        client.close()
        self.assertTrue(mock.wait_complete(3))
        result = mock.stop()
        self.assertFalse(result["protocol_complete"])
        self.assertEqual(result["requests"][0]["outcome"], "FAILED")
        self.assertEqual(result["failures"][0]["kind"], "response_write_failed")

    def test_explicit_truncated_and_drop_actions_are_expected_drops(self):
        mock = self.start(
            "expected-drops",
            [
                {"id": "truncated", "phase": "fault", "method": "GET", "path": "/truncated", "action": "truncated"},
                {"id": "drop", "phase": "fault", "method": "GET", "path": "/drop", "action": "drop_before_headers"},
            ],
        )
        mock.enter_phase("fault")
        conn = http.client.HTTPConnection(mock.host, mock.port, timeout=3)
        conn.request("GET", "/truncated")
        response = conn.getresponse()
        with self.assertRaises(http.client.IncompleteRead):
            response.read()
        conn.close()
        with self.assertRaises((http.client.RemoteDisconnected, ConnectionResetError)):
            request(mock, "GET", "/drop")
        result = mock.stop()
        self.assertEqual([item["outcome"] for item in result["requests"]], ["EXPECTED_DROP", "EXPECTED_DROP"])
        self.assertTrue(result["final_ok"])

    def test_response_actions_have_complete_content_types_and_tool_shapes(self):
        action_names = [
            ("anthropic", "anthropic_json"),
            ("sse", "anthropic_sse"),
            ("dsml", "dsml"),
            ("chat", "openai_chat_text_tool"),
            ("responses", "openai_responses_text_tool"),
            ("kimi", "kimi_sse"),
            ("models", "models_json"),
        ]
        mock = self.start(
            "responses",
            [{"id": name, "phase": "actions", "method": "GET", "path": f"/{name}", "action": action} for name, action in action_names],
        )
        mock.enter_phase("actions")
        responses = {name: request(mock, "GET", f"/{name}") for name, _ in action_names}
        anthropic = json.loads(responses["anthropic"][2])
        self.assertEqual(responses["anthropic"][1], "application/json")
        self.assertEqual(anthropic["type"], "message")
        self.assertEqual(anthropic["role"], "assistant")
        self.assertEqual(anthropic["stop_reason"], "end_turn")

        self.assertEqual(responses["sse"][1], "text/event-stream")
        self.assertTrue(responses["sse"][2].endswith(b'event: message_stop\ndata: {"type":"message_stop"}\n\n'))
        self.assertIn("DSML", responses["dsml"][2].decode("utf-8"))

        chat = json.loads(responses["chat"][2])
        message = chat["choices"][0]["message"]
        self.assertEqual(message["role"], "assistant")
        self.assertEqual(message["tool_calls"][0]["type"], "function")
        self.assertEqual(json.loads(message["tool_calls"][0]["function"]["arguments"])["query"], "mock-query")
        self.assertEqual(chat["usage"]["total_tokens"], 5)

        openai_response = json.loads(responses["responses"][2])
        function_call = next(item for item in openai_response["output"] if item["type"] == "function_call")
        self.assertEqual(function_call["call_id"], "call_mock_1")
        self.assertEqual(json.loads(function_call["arguments"])["query"], "mock-query")
        self.assertEqual(openai_response["usage"]["total_tokens"], 5)

        self.assertTrue(responses["kimi"][2].endswith(b'data: {"type":"message_stop"}\n\n'))
        self.assertIn(b"server_tool_use", responses["kimi"][2])
        models = json.loads(responses["models"][2])
        self.assertEqual(models["object"], "list")
        self.assertEqual(models["data"][0]["object"], "model")
        self.assertTrue(mock.stop()["final_ok"])

    def test_stop_unblocks_stall_and_final_ok_remains_false_when_incomplete(self):
        mock = self.start(
            "stall",
            [{"id": "stall", "phase": "formal", "method": "GET", "path": "/stall", "action": {"type": "stall", "seconds": 30, "then": "status"}}],
        )
        mock.enter_phase("formal")
        started = threading.Event()

        def caller():
            conn = http.client.HTTPConnection(mock.host, mock.port, timeout=3)
            try:
                conn.request("GET", "/stall")
                started.set()
                response = conn.getresponse()
                response.read()
            except (ConnectionError, OSError, http.client.HTTPException):
                pass
            finally:
                conn.close()

        client = threading.Thread(target=caller, daemon=False)
        client.start()
        self.assertTrue(started.wait(1))
        deadline = time.monotonic() + 1
        while time.monotonic() < deadline:
            with mock._lock:
                if mock._inflight:
                    break
            time.sleep(0.01)
        result = mock.stop(timeout=3)
        client.join(3)
        self.assertFalse(client.is_alive())
        self.assertTrue(result["stopped"])
        self.assertFalse(result["server_thread_alive"])
        self.assertFalse(result["protocol_complete"])
        self.assertFalse(result["final_ok"])
        self.assertEqual(result["requests"][0]["outcome"], "RETRY")

    def test_owned_tcp_echo_supports_connect_roundtrip_without_recording_payload(self):
        echo = start_owned_tcp_echo()
        port = echo.port
        self.assertNotEqual(port, 8765)
        payload = b"owned-connect-round-trip"
        client = socket.create_connection((echo.host, echo.port), timeout=2)
        client.sendall(payload)
        self.assertEqual(client.recv(len(payload)), payload)
        client.close()
        result = echo.stop()
        self.assertTrue(result["final_ok"])
        self.assertGreaterEqual(result["round_trips"], 1)
        self.assertNotIn(payload.decode("ascii"), json.dumps(result))
        with self.assertRaises(OSError):
            socket.create_connection(("127.0.0.1", port), timeout=0.2)

    def test_siliconflow_proxy_absolute_target_and_evil_suffix_are_distinct(self):
        fake_key = "siliconflow-fake-key-never-record"
        cases = [
            (
                "relay_siliconflow_proxy_positive",
                "http://api.siliconflow.cn/v1/messages",
                "api.siliconflow.cn",
                {"type": "any"},
            ),
            (
                "relay_siliconflow_proxy_evil_suffix",
                "http://api.siliconflow.cn.evil/v1/messages",
                "api.siliconflow.cn.evil",
                {"type": "tool", "name": "lookup"},
            ),
        ]
        for name, target, host, tool_choice in cases:
            with self.subTest(name=name):
                mock = start_manifest_scenario(name, secrets={"provider_key": fake_key})
                self.mocks.append(mock)
                mock.enter_phase("formal")
                body = {"model": "mock", "messages": [], "tools": [{"name": "lookup"}], "tool_choice": tool_choice}
                status, _, _ = request(
                    mock,
                    "POST",
                    target,
                    body,
                    {"Host": host, "Authorization": f"Bearer {fake_key}", "X-Api-Key": fake_key},
                )
                self.assertEqual(status, 200)
                result = mock.stop()
                self.assertTrue(result["final_ok"])
                self.assertTrue(result["requests"][0]["path_match"])
                self.assertTrue(all(result["requests"][0]["request"]["header_matches"].values()))
                self.assertNotIn(fake_key, json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    unittest.main()
