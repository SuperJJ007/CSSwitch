import json
import os
import pathlib
import stat
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]


class SkillRuntimeBoundary(unittest.TestCase):
    def test_production_startup_has_no_skill_manager_dependency(self):
        session = (ROOT / "desktop/src-tauri/src/runtime/sandbox_session.rs").read_text()
        for forbidden in (
            "skill_manager",
            "commands::skills",
            ".claude/skills",
            "scan_and_reconcile",
            "CSSWITCH_RECONCILED_DATA_DIR",
            "STORE_CONFLICT",
            "LIMIT_EXCEEDED",
        ):
            self.assertNotIn(forbidden, session)

        lib = (ROOT / "desktop/src-tauri/src/lib.rs").read_text()
        command_block = lib.split("tauri::generate_handler![", 1)[1].split("])", 1)[0]
        self.assertNotIn("commands::skills", command_block)
        self.assertNotIn("mod skill_manager;", lib)

        commands = (ROOT / "desktop/src-tauri/src/commands/mod.rs").read_text()
        self.assertNotIn("mod skills;", commands)

        catalog = json.loads((ROOT / "catalog/capabilities.v1.json").read_text())
        self.assertEqual(catalog["skills"], [])

    def test_gateway_starts_only_after_config_and_science_state_prechecks(self):
        session = (ROOT / "desktop/src-tauri/src/runtime/sandbox_session.rs").read_text()
        state_check = session.index("let remembered_runtime =")
        self.assertLess(session.index("config::load_from(&dir)"), state_check)
        self.assertNotIn("ensure_proxy(", session[:state_check])

        runtime_selection = session.index("let launch_runtime: ScienceRuntimeIdentity")
        self.assertGreater(runtime_selection, state_check)
        launch_check = session.index("if !launch.is_file()")
        normal_proxy = session.index(
            "let (pport, secret, proxy_action) = ensure_proxy", state_check
        )
        self.assertGreater(normal_proxy, launch_check)

    def test_launcher_never_clones_or_implicitly_selects_data_dir_runtime(self):
        launch = (ROOT / "scripts/launch-virtual-sandbox.sh").read_text()
        selection = launch.split("# 优先级：", 1)[1].split(
            "# Use a keychain scoped", 1
        )[0]
        self.assertIn('BIN="$APP_BIN"', selection)
        self.assertNotIn('BIN="$DATA_DIR/bin/claude-science"', launch)
        self.assertNotIn("for asset in bin conda runtime seed-assets", launch)
        self.assertNotIn("cp -Rc", launch)
        self.assertIn("本脚本绝不自动选择它", selection)

        stop = (ROOT / "scripts/stop-science-sandbox.sh").read_text()
        self.assertNotIn('BIN="$DATA_DIR/bin/claude-science"', stop)

    def test_fresh_data_dir_initializes_without_reading_real_science_home(self):
        with tempfile.TemporaryDirectory(
            prefix="csswitch-runtime-init-", dir="/private/tmp"
        ) as raw_tmp:
            tmp = pathlib.Path(raw_tmp)
            outer_home = tmp / "outer-home"
            real_science = outer_home / ".claude-science"
            real_science.mkdir(parents=True)
            (real_science / "must-not-copy").write_text("private")

            sandbox_home = tmp / "sandbox-home"
            bin_dir = tmp / "bin"
            bin_dir.mkdir()
            security = bin_dir / "security"
            security.write_text("#!/bin/sh\nexit 0\n")
            security.chmod(0o700)
            marker = tmp / "science-invocation.txt"
            science = tmp / "fake-claude-science"
            science.write_text(
                "#!/bin/sh\n"
                "mkdir -p \"$HOME/.claude-science\"\n"
                "printf 'HOME=%s\\nARGS=%s\\n' \"$HOME\" \"$*\" > \"$CSSWITCH_TEST_MARKER\"\n"
                "exit 0\n"
            )
            science.chmod(0o700)
            env = os.environ.copy()
            env.update(
                {
                    "HOME": str(outer_home),
                    "SANDBOX_HOME": str(sandbox_home),
                    "SCIENCE_BIN": str(science),
                    "CSSWITCH_TEST_MARKER": str(marker),
                    "PATH": f"{bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
                }
            )

            real_science.chmod(0)
            try:
                result = subprocess.run(
                    [
                        str(ROOT / "scripts/launch-virtual-sandbox.sh"),
                        "--port",
                        "19942",
                        "--proxy-url",
                        "http://127.0.0.1:19941/test-secret",
                        "--skip-oauth-forge",
                    ],
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=15,
                    check=False,
                )
            finally:
                real_science.chmod(stat.S_IRWXU)

            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn(f"HOME={sandbox_home}", marker.read_text())
            data_dir = sandbox_home / ".claude-science"
            self.assertTrue(data_dir.is_dir())
            self.assertFalse((data_dir / "must-not-copy").exists())
            self.assertFalse((data_dir / "bin").exists())

    def test_ui_cache_authorization_is_explicit_and_not_persisted(self):
        html = (ROOT / "desktop/src/index.html").read_text()
        js = (ROOT / "desktop/src/main.js").read_text()
        runtime = (ROOT / "desktop/src-tauri/src/commands/runtime.rs").read_text()
        science = (ROOT / "desktop/src-tauri/src/runtime/science.rs").read_text()

        for element_id in (
            "runtimeChoiceSec",
            "runtimeChoiceText",
            "runtimeUseCacheBtn",
            "runtimeDownloadBtn",
            "runtimeChoiceCancelBtn",
        ):
            self.assertIn(f'id="{element_id}"', html)
        one_click = js.split("async function oneClick()", 1)[1].split(
            "async function openScienceDownload", 1
        )[0]
        self.assertLess(
            one_click.index('call("science_runtime_preflight")'),
            one_click.index("runOneClick(null)"),
        )
        self.assertIn('runOneClick("cached_once")', js)
        self.assertIn("此选择不会保存", js)
        self.assertNotIn("localStorage", js)
        self.assertIn("runtime_choice: Option<String>", runtime)
        self.assertIn("choice == Some(CACHED_ONCE_CHOICE)", science)
        self.assertIn("if is_executable_file(app_bin)", science)

    def test_science_runtime_identity_is_reused_for_serve_status_url_and_stop(self):
        session = (ROOT / "desktop/src-tauri/src/runtime/sandbox_session.rs").read_text()
        science = (ROOT / "desktop/src-tauri/src/runtime/science.rs").read_text()
        runtime = (ROOT / "desktop/src-tauri/src/commands/runtime.rs").read_text()
        self.assertIn('.env("SCIENCE_BIN", &launch_runtime.path)', session)
        self.assertIn("st.science_runtime = Some(launch_runtime.clone())", session)
        self.assertIn("sandbox_running_ours(sport, &launch_runtime)", session)
        self.assertIn("sandbox_url(sport, &launch_runtime)", session)
        self.assertIn('.env("SCIENCE_BIN", &runtime.path)', science)
        self.assertIn('"source": runtime.source.code()', runtime)

    def test_launcher_ignores_large_external_tree_and_broken_legacy_store(self):
        with tempfile.TemporaryDirectory(
            prefix="csswitch-skill-boundary-", dir="/private/tmp"
        ) as raw_tmp:
            tmp = pathlib.Path(raw_tmp)
            outer_home = tmp / "outer-home"
            external = outer_home / ".claude" / "skills"
            legacy_store = outer_home / ".csswitch" / "skills"
            external.mkdir(parents=True)
            legacy_store.mkdir(parents=True)
            for index in range(300):
                skill = external / f"skill-{index:03d}"
                skill.mkdir()
                (skill / "SKILL.md").write_text(
                    f"---\nname: skill-{index:03d}\ndescription: boundary probe\n---\n"
                )
            (legacy_store / "inventory.v1.json").write_text("{broken")

            bin_dir = tmp / "bin"
            bin_dir.mkdir()
            security = bin_dir / "security"
            security.write_text("#!/bin/sh\nexit 0\n")
            security.chmod(0o700)

            marker = tmp / "science-invocation.txt"
            science = tmp / "fake-claude-science"
            science.write_text(
                "#!/bin/sh\n"
                "printf 'HOME=%s\\n' \"$HOME\" > \"$CSSWITCH_TEST_MARKER\"\n"
                "printf 'ARGS=%s\\n' \"$*\" >> \"$CSSWITCH_TEST_MARKER\"\n"
                "exit 0\n"
            )
            science.chmod(0o700)

            sandbox_home = tmp / "sandbox-home"
            data_dir = sandbox_home / ".claude-science"
            existing_skill = data_dir / "orgs" / "org-v043" / "skills" / "existing-skill"
            existing_skill.mkdir(parents=True)
            existing_skill_bytes = (
                b"---\nname: existing-skill\ndescription: v0.4.3 upgrade probe\n---\n"
            )
            (existing_skill / "SKILL.md").write_bytes(existing_skill_bytes)
            active_org_bytes = b'{"org_uuid":"org-v043"}\n'
            (data_dir / "active-org.json").write_bytes(active_org_bytes)
            env = os.environ.copy()
            env.update(
                {
                    "HOME": str(outer_home),
                    "SANDBOX_HOME": str(sandbox_home),
                    "SCIENCE_BIN": str(science),
                    "CSSWITCH_TEST_MARKER": str(marker),
                    "PATH": f"{bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
                }
            )

            external.chmod(0)
            try:
                result = subprocess.run(
                    [
                        str(ROOT / "scripts/launch-virtual-sandbox.sh"),
                        "--port",
                        "19932",
                        "--proxy-url",
                        "http://127.0.0.1:19931/test-secret",
                        "--skip-oauth-forge",
                    ],
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=15,
                    check=False,
                )
            finally:
                external.chmod(stat.S_IRWXU)

            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            invocation = marker.read_text()
            self.assertIn(f"HOME={sandbox_home}", invocation)
            self.assertIn(
                f"--data-dir {sandbox_home / '.claude-science'}", invocation
            )
            self.assertEqual(
                (existing_skill / "SKILL.md").read_bytes(), existing_skill_bytes
            )
            self.assertEqual(
                (data_dir / "active-org.json").read_bytes(), active_org_bytes
            )
            self.assertNotIn("LIMIT_EXCEEDED", result.stdout + result.stderr)
            self.assertNotIn("STORE_CONFLICT", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
