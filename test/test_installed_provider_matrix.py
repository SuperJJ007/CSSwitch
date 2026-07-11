import json
import os
import plistlib
import socket
import stat
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock as mocklib

from test.installed_provider_matrix import (
    CASE_DEFINITIONS,
    CONTROLLER_SCHEMA,
    EXPECTED_BUNDLE_ID,
    EXPECTED_EXECUTABLE,
    FAKE_API_KEY,
    FIXED_PATH_SECRET,
    ControllerError,
    InProcessScenarioControl,
    InstalledProviderSession,
    ProcessInspector,
    ProcessRecord,
    SubprocessScenarioControl,
    UnixScenarioControlClient,
    _dispatch,
    _safe_json_write,
    _scrub_error,
    build_case_scenario,
)


class FakeInspector:
    def __init__(self):
        self.records = []
        self.executables = {}
        self.listeners = set()
        self.alive = set()
        self.accept_all_listeners = False

    def process_table(self):
        return list(self.records)

    def executable_for_pid(self, pid):
        return self.executables.get(pid)

    def listener_owned(self, pid, port):
        return self.accept_all_listeners or (pid, port) in self.listeners

    def pid_alive(self, pid):
        return pid in self.alive

    def children(self, parent_pid):
        return [record for record in self.records if record.ppid == parent_pid]


class InstalledProviderMatrixTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="installed-provider-controller-test.")
        self.base = Path(self.temp.name).resolve(strict=True)
        self.app_bundle = self._make_fake_bundle()

    def tearDown(self):
        self.temp.cleanup()

    def _make_fake_bundle(self):
        bundle = self.base / "CSSwitch.app"
        macos = bundle / "Contents/MacOS"
        macos.mkdir(parents=True)
        info = {
            "CFBundleIdentifier": EXPECTED_BUNDLE_ID,
            "CFBundleExecutable": EXPECTED_EXECUTABLE,
            "CFBundleShortVersionString": "0.4.0-test",
        }
        with (bundle / "Contents/Info.plist").open("wb") as handle:
            plistlib.dump(info, handle)
        for name in ("desktop", "csswitch-gateway"):
            path = macos / name
            path.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            path.chmod(0o700)
        return bundle

    def _session(self, case="deepseek-off", inspector=None):
        root = self.base / f"root-{case}-{len(list(self.base.glob('root-*')))}"
        return InstalledProviderSession(
            case,
            root=root,
            app_bundle=self.app_bundle,
            allow_test_bundle=True,
            inspector=inspector or FakeInspector(),
            scenario_control=InProcessScenarioControl(
                build_case_scenario(CASE_DEFINITIONS[case])
            ),
        )

    def test_required_case_catalog_and_composed_phase_contract(self):
        self.assertTrue(
            {
                "deepseek-off",
                "deepseek-detect",
                "deepseek-rewrite",
                "qwen-chat",
                "custom-chat",
                "responses",
                "relay-force",
                "kimi",
                "siliconflow",
            }.issubset(CASE_DEFINITIONS)
        )
        for case in CASE_DEFINITIONS.values():
            scenario = build_case_scenario(case)
            phases = [step.phase for step in scenario.steps]
            self.assertEqual(phases.count("scratch"), 1, case.case_id)
            self.assertEqual(phases.count("formal"), 1, case.case_id)
            self.assertEqual(phases.count("reuse"), 2, case.case_id)
            self.assertEqual(phases.count("restart"), 1, case.case_id)
            self.assertEqual(phases.count("discovery"), 0 if case.base_kind == "native" else 1)
            self.assertTrue(all("8765" not in step.path for step in scenario.steps))
        silicon = build_case_scenario(CASE_DEFINITIONS["siliconflow"])
        self.assertEqual(silicon.steps[0].path, "http://api.siliconflow.cn/v1/models")
        self.assertTrue(
            all(
                step.path == "http://api.siliconflow.cn/v1/messages"
                for step in silicon.steps[1:]
            )
        )

    def test_workspace_config_and_wrappers_are_private_and_redacted_from_plan(self):
        with self._session("custom-chat") as session:
            plan = session.prepare_dry_run()
            config = json.loads(session.config_path.read_text(encoding="utf-8"))
            self.assertEqual(config["schema_version"], 2)
            self.assertEqual(config["active_id"], "")
            self.assertEqual(config["secret"], FIXED_PATH_SECRET)
            self.assertEqual(config["profiles"][0]["api_key"], FAKE_API_KEY)
            self.assertNotEqual(config["proxy_port"], 8765)
            self.assertNotEqual(config["sandbox_port"], 8765)
            self.assertNotEqual(config["proxy_port"], config["sandbox_port"])
            for directory in (
                session.root,
                session.home,
                session.csswitch_dir,
                session.evidence,
                session.tmp,
                session.bin_dir,
            ):
                self.assertFalse(directory.is_symlink())
                self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)
            self.assertEqual(stat.S_IMODE(session.config_path.stat().st_mode), 0o600)
            for wrapper in (
                session.fake_science,
                session.bin_dir / "open",
                session.bin_dir / "security",
                session.bin_dir / "python3",
            ):
                self.assertFalse(wrapper.is_symlink())
                self.assertEqual(stat.S_IMODE(wrapper.stat().st_mode), 0o700)
            encoded = json.dumps(plan, sort_keys=True)
            self.assertNotIn(FIXED_PATH_SECRET, encoded)
            self.assertNotIn(FAKE_API_KEY, encoded)
            self.assertTrue(plan["dry_run"])
            self.assertTrue(plan["do_not_execute"])

    def test_launchservices_plan_is_exact_and_controller_never_executes_it(self):
        with self._session("relay-force") as session:
            plan = session.prepare_dry_run()
            argv = plan["launch_argv"]
            self.assertEqual(argv[:4], ["/usr/bin/open", "-n", "-F", "-g"])
            self.assertEqual(argv[-1], str(self.app_bundle))
            self.assertIn("--env", argv)
            self.assertNotIn("--args", argv)
            self.assertFalse(plan["controller_launches_app"])
            self.assertFalse(plan["launch_allowed"])
            self.assertTrue(plan["preflight_would_allow_launch"])
            self.assertEqual(
                session._launch_environment(plan["mock_base_url"])["CSSWITCH_UPSTREAM_URL"],
                plan["mock_base_url"],
            )
            encoded = json.dumps(argv)
            self.assertNotIn(FIXED_PATH_SECRET, encoded)
            self.assertNotIn(FAKE_API_KEY, encoded)
            with self.assertRaisesRegex(ControllerError, "unknown controller operation"):
                _dispatch(session, {"op": "launch"})

    def test_siliconflow_proxy_mapping_is_present_but_launch_is_fail_closed(self):
        with self._session("siliconflow") as session:
            plan = session.prepare_dry_run()
            self.assertFalse(plan["launch_allowed"])
            self.assertTrue(plan["preflight_would_allow_launch"])
            self.assertIsNotNone(plan["launch_argv"])
            self.assertEqual(plan["blockers"], [])
            env = session._launch_environment(plan["mock_base_url"])
            self.assertEqual(env["HTTP_PROXY"], plan["mock_base_url"])
            self.assertEqual(env["http_proxy"], plan["mock_base_url"])
            self.assertEqual(env["NO_PROXY"], "")
            self.assertEqual(env["no_proxy"], "")
            self.assertEqual(env["CSSWITCH_UPSTREAM_URL"], plan["mock_base_url"])

    def test_same_bundle_and_unknown_same_named_process_block_launch_without_signal(self):
        inspector = FakeInspector()
        inspector.records = [ProcessRecord(101, 1, "desktop")]
        inspector.executables[101] = self.app_bundle / "Contents/MacOS/desktop"
        with self._session("deepseek-off", inspector) as session:
            plan = session.prepare_dry_run()
            self.assertFalse(plan["launch_allowed"])
            self.assertIn("same_bundle_process_running", plan["blockers"])
            self.assertNotIn(101, inspector.alive)

        inspector = FakeInspector()
        inspector.records = [ProcessRecord(202, 1, "desktop")]
        with self._session("deepseek-off", inspector) as session:
            matches = session.same_bundle_processes()
            self.assertEqual(matches[0]["identity"], "unknown")
            self.assertIn("same_bundle_process_running", session.preflight_blockers())

        bad_bundle = self.base / "Unreadable.app"
        bad_macos = bad_bundle / "Contents/MacOS"
        bad_macos.mkdir(parents=True)
        bad_executable = bad_macos / "desktop"
        bad_executable.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        bad_executable.chmod(0o700)
        (bad_bundle / "Contents/Info.plist").write_bytes(b"not-a-plist")
        inspector = FakeInspector()
        inspector.records = [ProcessRecord(303, 1, "desktop")]
        inspector.executables[303] = bad_executable
        with self._session("deepseek-off", inspector) as session:
            matches = session.same_bundle_processes()
            self.assertEqual(matches[0]["identity"], "unknown_bundle")
            self.assertIn("same_bundle_process_running", session.preflight_blockers())

    def test_mock_start_uses_dynamic_owned_listener_and_native_discovery_is_no_hit(self):
        inspector = FakeInspector()
        inspector.accept_all_listeners = True
        inspector.executables[os.getpid()] = Path(sys.executable)
        with self._session("deepseek-off", inspector) as session:
            ready = session.start_mock()
            self.assertNotIn(ready["port"], {8765, session.proxy_port, session.sandbox_port})
            self.assertTrue(ready["listener_verified"])
            started = session.enter_phase("discovery")
            self.assertFalse(started["mock_request_expected"])
            finished = session.finish_phase("discovery")
            self.assertTrue(finished["ok"])
            result = session.stop_mock()
            self.assertTrue(result["stopped"])
            self.assertFalse(result["server_thread_alive"])

    def test_config_diff_reports_paths_only_and_enforces_allowlist(self):
        with self._session("responses") as session:
            session.prepare_dry_run()
            config = json.loads(session.config_path.read_text(encoding="utf-8"))
            config["active_id"] = config["profiles"][0]["id"]
            _safe_json_write(session.config_path, config)
            accepted = session.inspect_config(["/active_id"])
            self.assertTrue(accepted["ok"])
            self.assertEqual(accepted["changed_paths"], ["/active_id"])
            config["secret"] = "unexpected-test-value"
            _safe_json_write(session.config_path, config)
            rejected = session.inspect_config(["/active_id"])
            self.assertFalse(rejected["ok"])
            self.assertEqual(rejected["unexpected_paths"], ["/secret"])
            encoded = json.dumps(rejected)
            self.assertNotIn("unexpected-test-value", encoded)
            self.assertNotIn(FIXED_PATH_SECRET, encoded)

    def test_log_scan_outputs_counts_only_and_python_tripwire_is_a_hard_failure(self):
        with self._session("qwen-chat") as session:
            session.prepare_dry_run()
            logs = session.csswitch_dir / "logs"
            logs.mkdir(mode=0o700)
            (logs / "proxy.log").write_text("safe gateway log\n", encoding="utf-8")
            clean = session.scan_logs()
            self.assertTrue(clean["ok"])
            self.assertEqual(clean["sensitive_log_match_count"], 0)
            (logs / "proxy.log").write_text(FAKE_API_KEY, encoding="utf-8")
            session._python_tripwire.write_text("python3-invoked\n", encoding="utf-8")
            dirty = session.scan_logs()
            self.assertFalse(dirty["ok"])
            self.assertGreater(dirty["sensitive_log_match_count"], 0)
            encoded = json.dumps(dirty)
            self.assertNotIn(FAKE_API_KEY, encoded)

    def test_health_and_formal_observations_never_return_raw_secret_key_or_body(self):
        with self._session("responses") as session:
            session.prepare_dry_run()
            captured = {}

            def fake_request(method, path, **kwargs):
                if method == "GET":
                    if path == "/health" or path == "/wrong-installed-secret/health":
                        return 403, {"content-type": "application/json"}, b"{}"
                    return (
                        200,
                        {"content-type": "application/json"},
                        json.dumps(
                            {
                                "gateway": "rust",
                                "provider": "openai-responses",
                                "shim": "off",
                                "launch_id": "launch-safe-1",
                            }
                        ).encode(),
                    )
                captured["body"] = kwargs["body"]
                return (
                    200,
                    {"content-type": "application/json"},
                    b'{"type":"message","content":[]}',
                )

            session._http_request = fake_request
            health = session.inspect_health()
            self.assertTrue(health["ok"])
            formal = session.send_formal()
            self.assertEqual(formal["status"], 200)
            self.assertIn(b'"tools"', captured["body"])
            self.assertNotIn("body", formal)
            for value in (health, formal):
                encoded = json.dumps(value)
                self.assertNotIn(FIXED_PATH_SECRET, encoded)
                self.assertNotIn(FAKE_API_KEY, encoded)

    def test_exact_app_sidecar_listener_and_reuse_restart_records(self):
        inspector = FakeInspector()
        app_exe = self.app_bundle / "Contents/MacOS/desktop"
        gateway_exe = self.app_bundle / "Contents/MacOS/csswitch-gateway"
        inspector.records = [
            ProcessRecord(301, 1, "desktop"),
            ProcessRecord(302, 301, "csswitch-gateway"),
        ]
        inspector.executables = {301: app_exe, 302: gateway_exe}
        inspector.alive = {301, 302}
        with self._session("relay-force", inspector) as session:
            inspector.listeners.add((302, session.proxy_port))
            observed = session.observe_app(timeout_seconds=0.05)
            self.assertEqual(observed["pid"], 301)
            self.assertEqual(session.inspect_sidecar()["pid"], 302)
            self.assertEqual(session.inspect_children()["python_direct_children"], 0)
            session._runtime_records["first"] = {"pid": 302, "launch_id": "launch-a"}
            session._runtime_records["reused"] = {"pid": 302, "launch_id": "launch-a"}
            session._runtime_records["restarted"] = {"pid": 303, "launch_id": "launch-b"}
            self.assertTrue(session.compare_runtime("first", "reused", "reuse")["ok"])
            self.assertTrue(session.compare_runtime("first", "restarted", "restart")["ok"])

    def test_cleanup_probes_only_owned_dynamic_ports_and_does_not_signal(self):
        inspector = FakeInspector()
        with self._session("custom-chat", inspector) as session:
            session.prepare_dry_run()
            result = session.verify_cleanup()
            self.assertTrue(result["ok"])
            self.assertEqual(set(result["ports_closed"]), {"proxy", "sandbox"})
            self.assertNotIn(8765, (session.proxy_port, session.sandbox_port))

    def test_cleanup_accepts_stopped_fake_science_empty_state_but_rejects_partial_state(self):
        inspector = FakeInspector()
        with self._session("custom-chat", inspector) as session:
            session.prepare_dry_run()
            state = (
                session.csswitch_dir
                / "sandbox/home/.claude-science/csswitch-installed-fake-science"
            )
            state.mkdir(parents=True, mode=0o700)
            self.assertTrue(session.verify_cleanup()["fake_science_state_valid"])

            (state / "pid").write_text("701\n", encoding="utf-8")
            result = session.verify_cleanup()
            self.assertFalse(result["fake_science_state_valid"])
            self.assertFalse(result["ok"])

    def test_destroy_workspace_requires_closed_owned_state_and_rejects_symlinks(self):
        inspector = FakeInspector()
        session = self._session("custom-chat", inspector)
        session.prepare_dry_run()
        root = session.root
        inspector.alive.add(701)
        inspector.executables[701] = self.app_bundle / "Contents/MacOS/desktop"
        session._app_pid = 701
        with self.assertRaisesRegex(ControllerError, "owned cleanup"):
            session.destroy_workspace()
        self.assertTrue(root.exists())
        inspector.alive.clear()
        link = root / "refuse-link"
        link.symlink_to("/private/tmp")
        with self.assertRaisesRegex(ControllerError, "symlink"):
            session.destroy_workspace()
        link.unlink()
        self.assertEqual(session.destroy_workspace(), {"root_removed": True})
        self.assertFalse(root.exists())
        session.close()

    def test_destroy_workspace_refuses_a_live_owned_port(self):
        session = self._session("deepseek-off")
        session.prepare_dry_run()
        session._proxy_reservation.release()
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", session.proxy_port))
            listener.listen(1)
            with self.assertRaisesRegex(ControllerError, "owned cleanup"):
                session.destroy_workspace()
        finally:
            listener.close()
        self.assertTrue(session.destroy_workspace()["root_removed"])
        session.close()

    def test_sanitized_summary_exports_outside_root_without_sensitive_values(self):
        export_temp = tempfile.TemporaryDirectory(prefix="csim-export.", dir="/private/tmp")
        self.addCleanup(export_temp.cleanup)
        export_parent = Path(export_temp.name)
        export_parent.chmod(0o700)
        destination = export_parent / "summary.json"
        session = self._session("responses")
        try:
            session.prepare_dry_run()
            session.scan_logs()
            exported = session.export_sanitized_summary(destination)
            self.assertEqual(exported["destination"], str(destination))
            raw = destination.read_text(encoding="utf-8")
            self.assertNotIn(FIXED_PATH_SECRET, raw)
            self.assertNotIn(FAKE_API_KEY, raw)
            value = json.loads(raw)
            self.assertEqual(value["case"], "responses")
            self.assertEqual(stat.S_IMODE(destination.stat().st_mode), 0o600)
            with self.assertRaisesRegex(ControllerError, "outside repo"):
                session.export_sanitized_summary(session.root / "summary.json")
            self.assertTrue(session.destroy_workspace()["root_removed"])
        finally:
            session.close()

    def test_external_cli_control_uses_private_evidence_and_exits_via_socket(self):
        external_temp = tempfile.TemporaryDirectory(prefix="csim-ext.", dir="/private/tmp")
        self.addCleanup(external_temp.cleanup)
        parent = Path(external_temp.name)
        parent.chmod(0o700)
        control = SubprocessScenarioControl(
            build_case_scenario(CASE_DEFINITIONS["deepseek-off"]),
            parent,
        )
        ready = control.start()
        self.assertEqual(ready["phase"], None)
        self.assertNotIn("token", json.dumps(ready))
        control.enter_phase("discovery")
        self.assertEqual(control.status()["active_phase"], "discovery")
        result = control.stop()
        self.assertTrue(result["stopped"])
        self.assertFalse(result["final_ok"])
        self.assertEqual(result["owned_process_exit_code"], 1)
        self.assertFalse(control.process_alive)
        for evidence_file in control.evidence_dir.glob("*.json"):
            self.assertNotIn("token", evidence_file.read_text(encoding="utf-8"))

    def test_external_cli_ready_failure_terminates_only_owned_subprocess(self):
        external_temp = tempfile.TemporaryDirectory(prefix="csim-start-fail.", dir="/private/tmp")
        self.addCleanup(external_temp.cleanup)
        parent = Path(external_temp.name)
        parent.chmod(0o700)
        control = SubprocessScenarioControl(
            build_case_scenario(CASE_DEFINITIONS["deepseek-off"]),
            parent,
        )
        with mocklib.patch.object(
            control,
            "_read_ready",
            side_effect=ControllerError("injected ready failure"),
        ):
            with self.assertRaisesRegex(ControllerError, "injected ready failure"):
                control.start()
        self.assertIsNotNone(control.process_pid)
        self.assertFalse(control.process_alive)

    def test_installed_bundle_identity_rejects_symlinked_components(self):
        real_contents = self.base / "real-contents"
        real_macos = real_contents / "MacOS"
        real_macos.mkdir(parents=True)
        info = {
            "CFBundleIdentifier": EXPECTED_BUNDLE_ID,
            "CFBundleExecutable": EXPECTED_EXECUTABLE,
        }
        with (real_contents / "Info.plist").open("wb") as handle:
            plistlib.dump(info, handle)
        for name in ("desktop", "csswitch-gateway"):
            path = real_macos / name
            path.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            path.chmod(0o700)
        linked_bundle = self.base / "Linked.app"
        linked_bundle.mkdir()
        (linked_bundle / "Contents").symlink_to(real_contents, target_is_directory=True)
        with self.assertRaisesRegex(ControllerError, "traverses a symlink"):
            InstalledProviderSession(
                "deepseek-off",
                root=self.base / "linked-root",
                app_bundle=linked_bundle,
                allow_test_bundle=True,
                inspector=FakeInspector(),
            )

    def test_default_session_mock_is_external_and_identity_checked(self):
        external_temp = tempfile.TemporaryDirectory(prefix="csim-session.", dir="/private/tmp")
        self.addCleanup(external_temp.cleanup)
        root = Path(external_temp.name) / "owned"
        session = InstalledProviderSession(
            "deepseek-off",
            root=root,
            app_bundle=self.app_bundle,
            allow_test_bundle=True,
            inspector=ProcessInspector(),
        )
        try:
            self.assertIsInstance(session._mock, SubprocessScenarioControl)
            ready = session.start_mock()
            self.assertTrue(ready["listener_verified"])
            expected = ProcessInspector().executable_for_pid(os.getpid())
            self.assertEqual(ready["owned_executable"], str(expected))
            session.enter_phase("discovery")
            session.finish_phase("discovery")
            stopped = session.stop_mock()
            self.assertTrue(stopped["stopped"])
            self.assertTrue(session.verify_cleanup()["ok"])
            self.assertTrue(session.destroy_workspace()["root_removed"])
        finally:
            session.close()

    def test_control_error_scrubbing_and_json_controller_schema(self):
        message = f"bad {FIXED_PATH_SECRET} and {FAKE_API_KEY}"
        scrubbed = _scrub_error(message)
        self.assertNotIn(FIXED_PATH_SECRET, scrubbed)
        self.assertNotIn(FAKE_API_KEY, scrubbed)
        self.assertIn("<redacted>", scrubbed)
        self.assertEqual(CONTROLLER_SCHEMA, "csswitch.installed-provider-controller.v1")

    def test_unix_control_client_keeps_token_in_memory_and_uses_reviewed_commands(self):
        socket_temp = tempfile.TemporaryDirectory(prefix="csim-sock.", dir="/private/tmp")
        self.addCleanup(socket_temp.cleanup)
        evidence = Path(socket_temp.name) / "e"
        evidence.mkdir(mode=0o700)
        evidence.chmod(0o700)
        socket_path = evidence / "control.sock"
        token = "controller-token-never-persist"
        received = []
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(str(socket_path))
        socket_path.chmod(0o600)
        server.listen(4)

        def serve():
            while True:
                conn, _ = server.accept()
                with conn:
                    raw = b""
                    while not raw.endswith(b"\n"):
                        raw += conn.recv(4096)
                    request = json.loads(raw)
                    received.append({key: value for key, value in request.items() if key != "token"})
                    self.assertEqual(request["token"], token)
                    command = request["command"]
                    if command == "status":
                        response = {"ok": True, "status": {"requests": [], "failures": []}}
                    elif command == "enter_phase":
                        response = {"ok": True, "status": {"requests": [], "failures": []}}
                    elif command == "wait":
                        response = {"ok": True, "completed": False, "status": {}}
                    else:
                        response = {"ok": True, "accepted": True}
                    conn.sendall(json.dumps(response).encode() + b"\n")
                    if command == "stop":
                        _safe_json_write(
                            evidence / "result.json",
                            {"final_ok": True, "stopped": True, "requests": [], "failures": []},
                        )
                        break
            server.close()

        thread = threading.Thread(target=serve)
        thread.start()
        ready = {
            "control_socket": "control.sock",
            "base_url": "http://127.0.0.1:32123",
            "port": 32123,
            "owned_pid": 999,
        }
        client = UnixScenarioControlClient(evidence, ready, token)
        self.assertNotIn(token, json.dumps(client.start()))
        client.enter_phase("scratch")
        self.assertEqual(client.status()["requests"], [])
        self.assertFalse(client.wait(0.01))
        self.assertTrue(client.stop()["final_ok"])
        thread.join(timeout=2)
        self.assertFalse(thread.is_alive())
        self.assertEqual(
            [request["command"] for request in received],
            ["enter_phase", "status", "wait", "stop"],
        )
        self.assertFalse((evidence / "ready-token.json").exists())


if __name__ == "__main__":
    unittest.main()
