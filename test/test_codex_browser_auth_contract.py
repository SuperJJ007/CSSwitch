import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class CodexBrowserAuthContractTest(unittest.TestCase):
    def test_packaged_ui_has_one_browser_login_entry(self):
        html = (ROOT / "desktop/src/index.html").read_text()
        js = (ROOT / "desktop/src/main.js").read_text()

        self.assertIn('id="codexLoginBtn"', html)
        self.assertIn("浏览器登录 Codex", html)
        self.assertNotIn("codexDeviceLoginBtn", html + js)
        self.assertNotIn("codexBrowserLoginBtn", html + js)
        self.assertNotIn("设备码登录", html + js)
        self.assertIn('call("codex_auth_start")', js)
        self.assertNotIn('call("codex_auth_start", { method', js)

    def test_desktop_and_gateway_have_no_device_dispatch(self):
        desktop = (ROOT / "desktop/src-tauri/src/commands/codex.rs").read_text()
        gateway = (ROOT / "desktop/gateway/src/codex_auth/login_async.rs").read_text()

        self.assertNotIn("LoginDevice", desktop)
        self.assertNotIn("run_device_login", gateway)
        self.assertNotIn("AsyncLoginMethod", gateway)
        self.assertNotIn("deviceauth/usercode", gateway)
        self.assertNotIn("deviceauth/token", gateway)

    def test_profile_repair_is_explicit_and_does_not_restart_oauth(self):
        html = (ROOT / "desktop/src/index.html").read_text()
        js = (ROOT / "desktop/src/main.js").read_text()
        tauri = (ROOT / "desktop/src-tauri/src/lib.rs").read_text()

        self.assertIn('id="codexRepairProfileBtn"', html)
        self.assertIn("无需重新登录", html)
        self.assertIn('call("codex_ensure_profile")', js)
        self.assertIn("profile_ensure_failed", js)
        self.assertIn("commands::codex::codex_ensure_profile", tauri)
        dom_ready = js.split('window.addEventListener("DOMContentLoaded"', 1)[1]
        self.assertNotIn("refreshCodexAuthStatus", dom_ready)
        self.assertIn("refreshCodexAuthStatus({ quiet: true })", js)

    def test_preview_and_ui_use_the_three_account_catalog_display_names(self):
        js = (ROOT / "desktop/src/main.js").read_text()
        for slug, display_name in (
            ("gpt-5.6-sol", "Codex / GPT-5.6-Sol"),
            ("gpt-5.6-terra", "Codex / GPT-5.6-Terra"),
            ("gpt-5.6-luna", "Codex / GPT-5.6-Luna"),
        ):
            self.assertIn("claude-csswitch-codex-" + slug, js)
            self.assertIn(display_name, js)
        self.assertNotIn("claude-csswitch-codex-gpt-5.6-codex", js)

    def test_model_labels_preserve_gateway_contract_and_escape_html(self):
        js = (ROOT / "desktop/src/main.js").read_text()

        self.assertIn('new TextEncoder().encode(value).length <= 512', js)
        self.assertIn(r'/[\u0000-\u001f\u007f-\u009f]/', js)
        self.assertNotIn('model.display_name.trim()', js)
        self.assertIn('return "显示名不可用 · " + String((model && model.id) || "")', js)
        self.assertNotIn('return "Codex / " + raw', js)
        self.assertIn('escapeHtml(codexModelLabel(m))', js)

    def test_relogin_message_matches_active_codex_runtime_state(self):
        js = (ROOT / "desktop/src/main.js").read_text()

        self.assertIn('active && isCodexSource(active)', js)
        self.assertIn('受管 Science/Gateway 已停止且未自动重启；请点击“一键开始”', js)
        self.assertIn('下一步可在“我的配置”中设为当前', js)

    def test_ui_defers_codex_preflight_to_one_typed_backend_operation(self):
        js = (ROOT / "desktop/src/main.js").read_text()
        protocol = (ROOT / "desktop/src/codex-auth-protocol.js").read_text()
        diagnostics = (ROOT / "desktop/src-tauri/src/commands/diagnostics.rs").read_text()

        self.assertNotIn("function requireCodexAuth", js)
        self.assertNotIn("await requireCodexAuth", js)
        self.assertIn("parseCodexAuthCommandError", js + protocol)
        for code in ("codex_login_required", "codex_auth_unavailable", "codex_auth_busy"):
            self.assertIn(code, protocol)
        dom_ready = js.split('window.addEventListener("DOMContentLoaded"', 1)[1]
        self.assertNotIn("refreshCodexAuthStatus", dom_ready)
        self.assertNotIn("codex_auth_status", diagnostics)

    def test_acceptance_build_isolates_config_and_auth_state_roots(self):
        desktop_config = (ROOT / "desktop/src-tauri/src/config.rs").read_text()
        gateway_auth = (ROOT / "desktop/gateway/src/codex_auth/mod.rs").read_text()
        gateway_cli = (ROOT / "desktop/gateway/src/codex_auth/cli.rs").read_text()
        gateway_config = (ROOT / "desktop/gateway/src/config.rs").read_text()
        gateway_storage = (ROOT / "desktop/gateway/src/codex_auth/storage.rs").read_text()
        gateway_manifest = (ROOT / "desktop/gateway/Cargo.toml").read_text()
        build_rs = (ROOT / "desktop/src-tauri/build.rs").read_text()

        self.assertIn('CONFIG_DIR_NAME: &str = ".csswitch-acceptance"', desktop_config)
        self.assertIn('CODEX_STATE_DIR_NAME: &str = ".csswitch-acceptance"', gateway_auth)
        self.assertIn('super::state_root_from_home(&home)', gateway_cli)
        self.assertIn('join(crate::codex_auth::CODEX_STATE_DIR_NAME)', gateway_config)
        self.assertIn('CONFIG_DIR_NAME: &str = ".csswitch"', desktop_config)
        self.assertIn('CODEX_STATE_DIR_NAME: &str = ".csswitch"', gateway_auth)
        self.assertIn("CSSwitch builds cannot skip Gateway staging", build_rs)
        self.assertIn('OAUTH_SECRET_FILE: &str = "codex-oauth.v1.json"', gateway_storage)
        self.assertIn('THINKING_SECRET_FILE: &str = "codex-thinking.v1.json"', gateway_storage)
        self.assertNotIn("security-framework", gateway_manifest)
        self.assertNotIn("CSSWITCH_SIGNING_TEAM_ID", build_rs)
        self.assertFalse((ROOT / "desktop/src-tauri/src/code_identity.rs").exists())
        self.assertFalse((ROOT / "desktop/gateway/src/code_identity.rs").exists())


if __name__ == "__main__":
    unittest.main()
