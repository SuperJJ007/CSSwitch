import json
import os
import pathlib
import socket
import subprocess
import tempfile
import time
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_GATEWAY = ROOT / "desktop/gateway/target/debug/csswitch-gateway"


def gateway_bin():
    override = os.environ.get("CSSWITCH_GATEWAY_BIN")
    return pathlib.Path(override) if override else DEFAULT_GATEWAY


class ExternalSkillInstallBridge(unittest.TestCase):
    BRIDGE_TOKEN = "0123456789abcdef" * 4

    def mcp_env(self):
        handle = tempfile.NamedTemporaryFile(
            mode="w", prefix="csswitch-skill-bridge-key-", dir="/private/tmp", delete=False
        )
        try:
            handle.write(self.BRIDGE_TOKEN + "\n")
            handle.close()
            os.chmod(handle.name, 0o600)
        except Exception:
            handle.close()
            pathlib.Path(handle.name).unlink(missing_ok=True)
            raise
        self.addCleanup(pathlib.Path(handle.name).unlink, missing_ok=True)
        env = os.environ.copy()
        env["CSSWITCH_SKILL_BRIDGE_KEY_FILE"] = handle.name
        return env

    def call_tool(self, binary, bridge_dir, arguments, tool_name="install_external_skill"):
        request = {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": tool_name, "arguments": arguments},
        }
        result = subprocess.run(
            [str(binary), "skill-install-mcp", "--bridge-dir", str(bridge_dir)],
            input=json.dumps(request) + "\n",
            text=True,
            capture_output=True,
            env=self.mcp_env(),
            timeout=90,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)["result"]["structuredContent"]

    def wait_response(self, path):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if path.is_file():
                return json.loads(path.read_text())
            time.sleep(0.02)
        self.fail(f"timed out waiting for {path.name}")

    def test_gateway_bridge_rejects_unsigned_and_replayed_requests(self):
        binary = gateway_bin()
        if not binary.is_file():
            self.skipTest("csswitch-gateway binary not built")
        with tempfile.TemporaryDirectory(prefix="csswitch-bridge-host-", dir="/private/tmp") as raw:
            root = pathlib.Path(raw)
            bridge_dir = root / "CSSwitch-Skill-Bridge-test"
            data_dir = root / "science"
            skills_dir = data_dir / "orgs" / "org-test" / "skills"
            bridge_dir.mkdir(mode=0o700)
            skills_dir.mkdir(parents=True)
            (data_dir / "active-org.json").write_text('{"org_uuid":"org-test"}\n')
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
                port = probe.getsockname()[1]
            env = os.environ.copy()
            env.update(
                {
                    "DEEPSEEK_API_KEY": "test-only-key",
                    "CSSWITCH_SKILL_DATA_DIR": str(data_dir),
                    "CSSWITCH_SKILL_BRIDGE_DIR": str(bridge_dir),
                    "CSSWITCH_SKILL_BRIDGE_TOKEN": self.BRIDGE_TOKEN,
                }
            )
            process = subprocess.Popen(
                [str(binary), "--provider", "deepseek", "--port", str(port)],
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                            break
                    except OSError:
                        if process.poll() is not None:
                            self.fail(process.stderr.read())
                        time.sleep(0.02)
                else:
                    self.fail("gateway did not bind loopback")

                unsigned_id = "a" * 32
                unsigned = bridge_dir / f"{unsigned_id}.request.json"
                unsigned.write_text(
                    json.dumps(
                        {
                            "operation": "uninstall",
                            "arguments": {"skill_name": "missing"},
                        }
                    )
                )
                os.chmod(unsigned, 0o600)
                rejected = self.wait_response(
                    bridge_dir / f"{unsigned_id}.response.json"
                )
                self.assertEqual(rejected["status"], "REQUEST_FAILED")

                generated = self.call_tool(
                    binary,
                    bridge_dir,
                    {"skill_name": "missing"},
                    "uninstall_external_skill",
                )
                request_name = generated["request"]["filename"]
                response_name = generated["request"]["response_filename"]
                request_path = bridge_dir / request_name
                response_path = bridge_dir / response_name
                request_body = json.dumps(generated["request"]["payload"])
                request_path.write_text(request_body)
                os.chmod(request_path, 0o600)
                handled = self.wait_response(response_path)
                self.assertEqual(handled["status"], "UNINSTALL_FAILED")

                response_path.unlink()
                request_path.write_text(request_body)
                os.chmod(request_path, 0o600)
                replayed = self.wait_response(response_path)
                self.assertEqual(replayed["status"], "REQUEST_FAILED")
            finally:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
                if process.stderr is not None:
                    process.stderr.close()

    def test_stdio_mcp_all_mode_keeps_compatibility_and_name_only_needs_url(self):
        binary = gateway_bin()
        if not binary.is_file():
            self.skipTest("csswitch-gateway binary not built")
        with tempfile.TemporaryDirectory(prefix="CSSwitch-Skill-Bridge-mcp-", dir="/private/tmp") as raw:
            bridge_dir = pathlib.Path(raw)
            requests = [
                {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}},
                {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {
                        "name": "install_external_skill",
                        "arguments": {"skill_name": "pdf"},
                    },
                },
            ]
            result = subprocess.run(
                [str(binary), "skill-install-mcp", "--bridge-dir", str(bridge_dir)],
                input="".join(json.dumps(item) + "\n" for item in requests),
                text=True,
                capture_output=True,
                env=self.mcp_env(),
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stderr, "")
            responses = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual([item["id"] for item in responses], [1, 2, 3])
            tools = responses[1]["result"]["tools"]
            self.assertEqual(
                [tool["name"] for tool in tools],
                ["install_external_skill", "uninstall_external_skill"],
            )
            description = tools[0]["description"]
            self.assertIn("host.skills.edit", description)
            self.assertIn("host.skills.publish", description)
            payload = responses[2]["result"]["structuredContent"]
            self.assertEqual(payload["status"], "NEED_SOURCE_URL")
            self.assertFalse(payload["directory_commit"])
            self.assertEqual(list(bridge_dir.iterdir()), [])

    def test_uninstall_request_uses_the_same_host_bridge_and_never_calls_science_delete(self):
        binary = gateway_bin()
        if not binary.is_file():
            self.skipTest("csswitch-gateway binary not built")
        with tempfile.TemporaryDirectory(prefix="CSSwitch-Skill-Bridge-uninstall-", dir="/private/tmp") as raw:
            bridge_dir = pathlib.Path(raw)
            request = {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "uninstall_external_skill",
                    "arguments": {"skill_name": "internal-comms"},
                },
            }
            result = subprocess.run(
                [str(binary), "skill-install-mcp", "--bridge-dir", str(bridge_dir)],
                input=json.dumps(request) + "\n",
                text=True,
                capture_output=True,
                env=self.mcp_env(),
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(result.stdout)["result"]["structuredContent"]
            self.assertEqual(payload["status"], "HOST_ACCESS_REQUIRED")
            self.assertEqual(payload["request"]["payload"]["operation"], "uninstall")
            self.assertEqual(
                payload["request"]["payload"]["arguments"],
                {"skill_name": "internal-comms"},
            )
            self.assertIn("host.skills.edit/publish", payload["message"])
            self.assertEqual(list(bridge_dir.iterdir()), [])

    def test_scoped_uninstaller_connector_exposes_only_uninstall(self):
        binary = gateway_bin()
        if not binary.is_file():
            self.skipTest("csswitch-gateway binary not built")
        with tempfile.TemporaryDirectory(prefix="CSSwitch-Skill-Bridge-scoped-", dir="/private/tmp") as raw:
            bridge_dir = pathlib.Path(raw)
            requests = [
                {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
                {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
            ]
            result = subprocess.run(
                [
                    str(binary),
                    "skill-install-mcp",
                    "--bridge-dir",
                    str(bridge_dir),
                    "--tool-mode",
                    "uninstall",
                ],
                input="".join(json.dumps(item) + "\n" for item in requests),
                text=True,
                capture_output=True,
                env=self.mcp_env(),
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            responses = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual(
                responses[0]["result"]["serverInfo"]["name"],
                "csswitch-skill-uninstaller",
            )
            self.assertEqual(
                [tool["name"] for tool in responses[1]["result"]["tools"]],
                ["uninstall_external_skill"],
            )

    def test_url_request_requires_official_host_access_without_direct_write(self):
        binary = gateway_bin()
        if not binary.is_file():
            self.skipTest("csswitch-gateway binary not built")
        with tempfile.TemporaryDirectory(prefix="CSSwitch-Skill-Bridge-url-", dir="/private/tmp") as raw:
            bridge_dir = pathlib.Path(raw)
            request = {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "install_external_skill",
                    "arguments": {"source_url": "https://example.com/not-a-skill"},
                },
            }
            result = subprocess.run(
                [str(binary), "skill-install-mcp", "--bridge-dir", str(bridge_dir)],
                input=json.dumps(request) + "\n",
                text=True,
                capture_output=True,
                env=self.mcp_env(),
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            response = json.loads(result.stdout)
            payload = response["result"]["structuredContent"]
            self.assertEqual(payload["status"], "HOST_ACCESS_REQUIRED")
            self.assertFalse(payload["directory_commit"])
            self.assertEqual(payload["bridge_dir"], str(bridge_dir))
            self.assertEqual(list(bridge_dir.iterdir()), [])

    def test_science_startup_registration_is_best_effort_and_prelaunch(self):
        session = (ROOT / "desktop/src-tauri/src/runtime/sandbox_session.rs").read_text()
        registration = session.index("register_before_science_start(")
        launch = session.index('let status = Command::new("zsh")')
        self.assertLess(registration, launch)
        self.assertIn("失败只降级该工具，绝不阻断 Science 启动", session)
        self.assertNotIn("register_before_science_start(&app, &auth_dir)?", session)

    def test_route_is_ensured_and_scoped_connectors_are_registered(self):
        route = (
            ROOT / "desktop/src-tauri/src/runtime/external_skill_route.rs"
        ).read_text()
        bridge = (
            ROOT / "desktop/src-tauri/src/runtime/skill_install_bridge.rs"
        ).read_text()
        self.assertIn("ensure_route_skill(data_dir)?", bridge)
        self.assertNotIn("retire_route_skill(data_dir)?", bridge)
        self.assertIn("csswitch-system-bridge", route)
        self.assertIn("route_skill_matches(&target)?", route)
        self.assertIn("rename_no_replace(&temp, &target)?", route)
        self.assertIn('\"--tool-mode\", \"install\"', bridge)
        self.assertIn('\"--tool-mode\", \"uninstall\"', bridge)

    def test_route_forces_dynamic_connector_uninstall_without_science_or_shell_fallback(self):
        route = (
            ROOT
            / "desktop/src-tauri/resources/skills/csswitch-external-skill-tools/SKILL.md"
        ).read_text()
        self.assertIn("mcp-csswitch-skill-installer", route)
        self.assertIn("mcp-csswitch-skill-uninstaller", route)
        self.assertIn('"csswitch-skill-uninstaller"', route)
        self.assertIn("Never call `host.skills.delete`", route)
        self.assertIn("manual filesystem deletion", route)
        self.assertIn("There is no\n   default or hard-coded Skill name", route)
        self.assertNotIn("internal-comms", route)

    def test_route_attachment_uses_fixed_loopback_control_plane_and_fresh_ui_url(self):
        gateway = (ROOT / "desktop/gateway/src/science_control.rs").read_text()
        bridge = (
            ROOT / "desktop/src-tauri/src/runtime/skill_install_bridge.rs"
        ).read_text()
        session = (ROOT / "desktop/src-tauri/src/runtime/sandbox_session.rs").read_text()
        self.assertIn('ROUTE_SKILL_NAME: &str = "csswitch-external-skill-tools"', gateway)
        self.assertIn('matches!(url.host_str(), Some("127.0.0.1" | "localhost"))', gateway)
        self.assertIn('/api/agents/OPERON/skills', gateway)
        self.assertIn('x-operon-csrf', gateway)
        self.assertIn('CSSWITCH_SCIENCE_CONTROL_URL', bridge)
        self.assertNotIn('.arg(control_url)', bridge)
        self.assertIn('let control_url = sandbox_url(sport, &running_runtime);', session)
        self.assertIn('let control_url = sandbox_url(sport, &launch_runtime);', session)
        self.assertIn('attach_route_best_effort(&app, installer, &control_url)', session)
        self.assertIn('// the browser receives a fresh URL below.', session)

    def test_skill_installer_targets_active_org_and_never_version_runtime(self):
        installer = (ROOT / "desktop/gateway/src/skill_install.rs").read_text()
        route = (
            ROOT / "desktop/src-tauri/src/runtime/external_skill_route.rs"
        ).read_text()
        for source in (installer, route):
            self.assertIn('data_dir.join("orgs")', source)
            self.assertIn('join("skills")', source)
            self.assertNotIn('data_dir.join("runtime")', source)
            self.assertNotIn('join("runtime").join', source)

    def test_tool_results_require_native_agent_attach_and_detach(self):
        gateway = (ROOT / "desktop/gateway/src/skill_install.rs").read_text()
        self.assertIn("FILES_COMMITTED_ATTACH_REQUIRED", gateway)
        self.assertIn("host.agents.attach_skill", gateway)
        self.assertIn("QUARANTINED_DETACH_REQUIRED", gateway)
        self.assertIn("host.agents.detach_skill", gateway)

if __name__ == "__main__":
    unittest.main()
