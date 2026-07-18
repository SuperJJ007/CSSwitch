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
        state_check = session.index("let (remembered_runtime, confirmed_stopped)")
        self.assertLess(session.index("config::load_from(&dir)"), state_check)
        self.assertNotIn("ensure_proxy(", session[:state_check])

        runtime_selection = session.index("let launch_runtime: ScienceRuntimeIdentity")
        self.assertGreater(runtime_selection, state_check)
        launch_check = session.index("if !launch.is_file()")
        normal_proxy = session.index("let (pport, secret, proxy_action) =", state_check)
        self.assertGreater(normal_proxy, launch_check)

    def test_launcher_never_clones_or_implicitly_selects_data_dir_runtime(self):
        launch = (ROOT / "scripts/launch-virtual-sandbox.sh").read_text()
        selection = launch.split('BIN_SOURCE="backend-selected runtime"', 1)[1].split(
            "# Use a keychain scoped", 1
        )[0]
        self.assertIn('BIN="$APP_BIN"', selection)
        self.assertNotIn('BIN="$DATA_DIR/bin/claude-science"', launch)
        self.assertNotIn("for asset in bin conda runtime seed-assets", launch)
        self.assertNotIn("cp -Rc", launch)
        self.assertIn("CSSWITCH_PROXY_URL", launch)
        self.assertIn("--proxy-url", launch)
        self.assertIn("path_contains_symlink", launch)

        stop = (ROOT / "scripts/stop-science-sandbox.sh").read_text()
        self.assertNotIn('BIN="$DATA_DIR/bin/claude-science"', stop)
        self.assertIn("path_contains_symlink", stop)

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
        self.assertNotIn("localStorage", one_click)
        self.assertIn('THEME_STORAGE_KEY = "csswitch-theme"', js)
        self.assertIn("runtime_choice: Option<String>", runtime)
        self.assertIn("choice == Some(CACHED_ONCE_CHOICE)", science)
        self.assertIn("fn safe_science_version(path: &Path)", science)
        self.assertIn("version_cache.version(app_bin)", science)
        self.assertIn('"cached_choice_required"', science)

    def test_manual_science_open_refreshes_url_and_has_visible_feedback(self):
        js = (ROOT / "desktop/src/main.js").read_text()
        runtime = (ROOT / "desktop/src-tauri/src/commands/runtime.rs").read_text()
        system = (ROOT / "desktop/src-tauri/src/runtime/system.rs").read_text()

        handler = js.split("async function openBrowser()", 1)[1].split(
            "async function runDoctor", 1
        )[0]
        self.assertIn("if (busy || browserOpenInFlight) return", handler)
        self.assertIn("browserOpenInFlight = true", handler)
        self.assertIn("browserOpenInFlight = false", handler)
        self.assertIn("syncOpenBrowserControl()", handler)
        self.assertIn("正在获取新的 Science 地址", handler)
        self.assertIn("已向默认浏览器发出打开 Science 的请求", handler)
        self.assertIn('result.status === "error"', handler)
        self.assertIn("setBrowserFallback(result.fallback_url)", handler)
        self.assertIn('setMsg("打开浏览器失败："', handler)

        control = js.split("function syncOpenBrowserControl()", 1)[1].split(
            "function syncActivationControls", 1
        )[0]
        self.assertIn("busy || browserOpenInFlight", control)
        self.assertIn('browserOpenInFlight ? "打开中…" : "浏览器打开"', control)

        command = runtime.split("fn open_url_inner", 1)[1].split(
            "pub(crate) async fn quit_app", 1
        )[0]
        self.assertIn("sandbox_listener_matches_runtime", command)
        self.assertIn("sandbox_url(sandbox_port, &runtime)", command)
        self.assertNotIn("st.sandbox_url.clone()", command)
        self.assertIn("manual_open_result(url.clone(), open_in_browser(&url))", command)
        self.assertIn("CSSWITCH_FAKE_OPEN_FAIL_ONCE_FILE", runtime)
        self.assertIn('failed_open["fallback_url"]', runtime)
        self.assertIn('ACCEPTANCE_OPEN_BIN_ENV: &str = "CSSWITCH_ACCEPTANCE_OPEN_BIN"', system)
        self.assertIn('TEST_OPEN_BIN_ENV: &str = "CSSWITCH_TEST_OPEN_BIN"', system)
        self.assertIn('return Ok(PathBuf::from("/usr/bin/open"))', system)
        self.assertIn("if !path.is_absolute()", system)
        matrix = (ROOT / "test/installed_provider_matrix.py").read_text()
        self.assertIn('"CSSWITCH_ACCEPTANCE_OPEN_BIN": str(self.bin_dir / "open")', matrix)

    def test_simple_model_inputs_and_one_click_failures_are_visible_and_structured(self):
        js = (ROOT / "desktop/src/main.js").read_text()
        session = (ROOT / "desktop/src-tauri/src/runtime/sandbox_session.rs").read_text()
        runtime = (ROOT / "desktop/src-tauri/src/commands/runtime.rs").read_text()
        lifecycle = (ROOT / "desktop/src-tauri/src/runtime/proxy_lifecycle.rs").read_text()
        lib = (ROOT / "desktop/src-tauri/src/lib.rs").read_text()

        submission = js.split("function catalogSubmission(kind)", 1)[1].split(
            "function catalogRolesChanged", 1
        )[0]
        for field in (
            "default_model: editor.model.value",
            "quality_model: editor.quality.value",
            "fast_model: editor.fast.value",
            "fable_model: editor.fable.value",
        ):
            self.assertIn(field, submission)
        self.assertNotIn("async function applyPresetSync", js)
        self.assertNotIn("function applyFetchResult", js)
        self.assertIn("function catalogRolesChanged(kind)", js)
        self.assertIn('catalogRolesChanged("wizard")', js)
        self.assertIn('catalogRolesChanged("connection")', js)

        one_click = js.split("async function runOneClick", 1)[1].split(
            "async function importLocalSkill", 1
        )[0]
        self.assertLess(
            one_click.index('r && r.status === "error"'),
            one_click.index('const message = r.msg ||'),
        )
        self.assertIn("setBusy(false)", one_click)
        self.assertIn("setBrowserFallback(r.fallback_url)", one_click)

        gateway_ready = session.index("verify_gateway_model_catalog(pport, &secret, active_profile)?")
        science_spawn = session.index('Command::new("zsh")', gateway_ready)
        self.assertLess(gateway_ready, science_spawn)
        for stage in ("start_gateway", "start_science", "verify_science_catalog"):
            self.assertIn(f'"{stage}"', session)

        self.assertIn("recover_interrupted_gateway(&app, &state)?", runtime)
        self.assertIn("stop_managed_gateway_on_port", lifecycle)
        self.assertIn('health.intent == "formal"', lifecycle)
        self.assertIn("journal.previous_gateway.as_ref()", lifecycle)
        self.assertIn("current == initial_for_probe", lifecycle)
        boot = lib.split("LaunchPath::BootScience", 1)[1].split("// ---------- 入口", 1)[0]
        self.assertIn("boot_result_error(&value)", boot)
        self.assertIn("Some(message) => mark_boot_failed", boot)

    def test_science_runtime_identity_is_reused_for_serve_status_url_and_stop(self):
        session = (ROOT / "desktop/src-tauri/src/runtime/sandbox_session.rs").read_text()
        science = (ROOT / "desktop/src-tauri/src/runtime/science.rs").read_text()
        runtime = (ROOT / "desktop/src-tauri/src/commands/runtime.rs").read_text()
        self.assertIn('.env("SCIENCE_BIN", &launch_runtime.path)', session)
        self.assertIn('.env("CSSWITCH_PROXY_URL", &proxy_url)', session)
        self.assertNotIn('.arg(&proxy_url)', session)
        self.assertIn("st.science_runtime = Some(launch_runtime.clone())", session)
        self.assertIn("probe_known_runtime(sport, &runtime)", session)
        self.assertIn("sandbox_listener_matches_runtime(sport, &launch_runtime)", session)
        self.assertIn("sandbox_url(sport, &launch_runtime)", session)
        self.assertIn('.env("SCIENCE_BIN", &runtime.path)', science)
        self.assertIn('"source": runtime.source.code()', runtime)

    def test_system_ssh_bridge_is_opt_in_and_replaces_tunnel_entry(self):
        js = (ROOT / "desktop/src/main.js").read_text()
        html = (ROOT / "desktop/src/index.html").read_text()
        launch = (ROOT / "scripts/launch-virtual-sandbox.sh").read_text()
        wrapper = (ROOT / "scripts/ssh-bridge/ssh").read_text()
        session = (ROOT / "desktop/src-tauri/src/runtime/sandbox_session.rs").read_text()
        runtime = (ROOT / "desktop/src-tauri/src/commands/runtime.rs").read_text()

        self.assertNotIn("ssh_tunnel_info", js + runtime)
        self.assertNotIn("生成 SSH 访问命令", html)
        self.assertIn("reuseSystemSsh", js + html)
        self.assertIn("reuse_system_ssh", js + runtime)
        self.assertIn('CSSWITCH_REUSE_SYSTEM_SSH', launch + session)
        self.assertIn('CSSWITCH_SYSTEM_SSH_CONFIG', launch + wrapper)
        self.assertIn('exec /usr/bin/ssh -F "$config" "$@"', wrapper)
        self.assertNotIn("ln -s", launch)
        self.assertNotIn("cp -R", launch)

    def test_explicit_exit_revokes_the_managed_science_target(self):
        lib = (ROOT / "desktop/src-tauri/src/lib.rs").read_text()
        runtime = (ROOT / "desktop/src-tauri/src/commands/runtime.rs").read_text()
        js = (ROOT / "desktop/src/main.js").read_text()

        cleanup = lib.split("fn cleanup_for_exit", 1)[1].split(
            "fn mark_boot_failed", 1
        )[0]
        self.assertLess(cleanup.index("stop_sandbox("), cleanup.index("st.stop_proxy()"))
        quit_command = runtime.split("pub(crate) async fn quit_app", 1)[1].split(
            "#[cfg(test)]", 1
        )[0]
        self.assertLess(
            quit_command.index("stop_all_inner_cmd"), quit_command.index("exit_app.exit(0)")
        )
        quit_handler = js.split('els.quitBtn.addEventListener("click"', 1)[1].split(
            "\n  });", 1
        )[0]
        self.assertNotIn("ssh_tunnel_info", quit_handler)
        self.assertIn('setMsg("退出失败："', quit_handler)

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
