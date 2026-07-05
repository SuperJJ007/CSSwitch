import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import test from "node:test";

const html = readFileSync(new URL("../desktop/src/index.html", import.meta.url), "utf8");
const main = readFileSync(new URL("../desktop/src/main.js", import.meta.url), "utf8");
const remoteCommands = readFileSync(new URL("../desktop/src-tauri/src/remote_commands.rs", import.meta.url), "utf8");
const remoteSsh = readFileSync(new URL("../desktop/src-tauri/src/remote/ssh.rs", import.meta.url), "utf8");
const helperCommands = readFileSync(new URL("../desktop/src-tauri/src/cli/commands.rs", import.meta.url), "utf8");
const libTauri = readFileSync(new URL("../desktop/src-tauri/src/lib_tauri.rs", import.meta.url), "utf8");
const tauriConf = readFileSync(new URL("../desktop/src-tauri/tauri.conf.json", import.meta.url), "utf8");
const buildWorkflow = readFileSync(new URL("../.github/workflows/build.yml", import.meta.url), "utf8");
const tauriBuildRs = readFileSync(new URL("../desktop/src-tauri/build.rs", import.meta.url), "utf8");
const crossTomlUrl = new URL("../desktop/src-tauri/Cross.toml", import.meta.url);
const crossToml = existsSync(crossTomlUrl) ? readFileSync(crossTomlUrl, "utf8") : "";

function remoteStartProxyBody() {
  const m = remoteCommands.match(/pub fn remote_start_proxy[\s\S]*?\n}\n\n\/\/\/ 停止远程代理/);
  assert.ok(m, "remote_start_proxy body should be discoverable");
  return m[0];
}

function workflowJob(name) {
  const m = buildWorkflow.match(new RegExp(`\\n  ${name}:\\n[\\s\\S]*?(?=\\n  [a-zA-Z0-9_-]+:\\n|\\n$)`));
  assert.ok(m, `${name} job should exist`);
  return m[0];
}

test("desktop profile UI script matches the v2 profile HTML", () => {
  assert.match(html, /id="profileList"/);
  assert.match(html, /id="newBtn"/);
  assert.match(html, /id="wizSec"/);

  assert.doesNotMatch(main, /els\.(provider|keyInput|saveKeyBtn)\b/);
  assert.doesNotMatch(main, /save_provider_key/);

  for (const command of [
    "create_profile",
    "update_profile_metadata",
    "update_profile_connection",
    "clear_profile_key",
    "delete_profile",
    "set_active_profile",
  ]) {
    assert.match(main, new RegExp(`["']${command}["']`));
  }

  assert.match(main, /newBtn\.addEventListener\(["']click["']/);
});

test("remote server modal uses its own list instead of the local profile list", () => {
  assert.match(html, /id="remoteProfileList"/);
  assert.match(main, /getElementById\(["']remoteProfileList["']\)/);
  assert.doesNotMatch(main, /const\s+list\s*=\s*document\.getElementById\(["']profileList["']\)/);
});

test("remote start uploads the active local profile before starting helper proxy", () => {
  assert.match(remoteCommands, /remote_active_config_for_start/);
  assert.match(remoteCommands, /config::load_from\(&config::default_dir\(\)\)/);
  assert.match(remoteCommands, /"config"\.to_string\(\),\s*"set"\.to_string\(\)/s);
  assert.match(remoteCommands, /serde_json::to_string\(&remote_cfg\)/);
});

test("remote start stops stale helper proxy before starting with the new secret", () => {
  assert.match(
    remoteStartProxyBody(),
    /"proxy"\.to_string\(\),\s*"stop"\.to_string\(\)[\s\S]*"proxy"\.to_string\(\),\s*"start"\.to_string\(\)/,
  );
});

test("remote one-click frontend calls the full remote stack command", () => {
  const body = main.match(/async function remoteOneClick\(\) \{[\s\S]*?\n\}/);
  assert.ok(body, "remoteOneClick body should be discoverable");
  assert.match(body[0], /call\(["']remote_one_click["']/);
  assert.match(body[0], /proxyPort/);
  assert.match(body[0], /sandboxPort/);
  assert.doesNotMatch(body[0], /call\(["']remote_start_proxy["']/);
});

test("remote profile test prepares helper instead of only checking health", () => {
  const body = main.match(/async function testProfileConnection\(\) \{[\s\S]*?\n\}/);
  assert.ok(body, "testProfileConnection body should be discoverable");
  assert.match(body[0], /call\(["']remote_prepare_helper["']/);
  assert.doesNotMatch(body[0], /call\(["']remote_check_health["']/);
});

test("remote profile save prepares helper before saving the server", () => {
  const body = main.match(/async function saveProfile\(\) \{[\s\S]*?\n\}/);
  assert.ok(body, "saveProfile body should be discoverable");
  assert.match(body[0], /call\(["']remote_prepare_helper["']/);
  assert.match(body[0], /call\(["']remote_save_profile["']/);
  assert.ok(
    body[0].indexOf('call("remote_prepare_helper"') < body[0].indexOf('call("remote_save_profile"'),
    "save should prepare helper before persisting the remote server",
  );
});

test("remote one-click does not repeat helper preparation", () => {
  const body = main.match(/async function remoteOneClick\(\) \{[\s\S]*?\n\}/);
  assert.ok(body, "remoteOneClick body should be discoverable");
  assert.doesNotMatch(body[0], /remote_prepare_helper/);
});

test("backend exposes explicit remote helper preparation command", () => {
  assert.match(remoteCommands, /pub fn remote_prepare_helper/);
  assert.match(main, /case "remote_prepare_helper"/);
});

test("remote helper preparation installs only when health is not ready and falls back to bundled upload", () => {
  const m = remoteCommands.match(/pub fn remote_prepare_helper[\s\S]*?\n}\n\n\/\/ ============================================================================/);
  assert.ok(m, "remote_prepare_helper body should be discoverable");
  const body = m[0];
  assert.match(body, /helper_ready_for_profile/);
  assert.match(body, /install_helper_from_github/);
  assert.match(body, /install_helper_from_bundle/);
  assert.ok(
    body.indexOf("install_helper_from_github") < body.indexOf("install_helper_from_bundle"),
    "GitHub release install should be attempted before bundled upload fallback",
  );
});

test("remote bundled helper upload uses SSH stdin and installs atomically", () => {
  assert.match(remoteSsh, /pub fn install_helper_from_stdin/);
  assert.match(remoteSsh, /stdin\(Stdio::piped\(\)\)/);
  assert.match(remoteSsh, /cat > "\$TMP"/);
  assert.match(remoteSsh, /chmod \+x "\$TMP"/);
  assert.match(remoteSsh, /mv "\$TMP" "\$HELPER_PATH"/);
});

test("remote github helper installer extracts browser download url after matching asset name", () => {
  assert.match(remoteSsh, /BINARY_NAME="csswitch-helper-\$\{\{OS\}\}-\$\{\{ARCH\}\}"/);
  assert.match(remoteSsh, /browser_download_url/);
  assert.match(remoteSsh, /awk -v name=/);
});

test("desktop bundle and release workflow provide linux helper assets for upload fallback", () => {
  assert.match(tauriConf, /helper-assets/);
  assert.match(buildWorkflow, /csswitch-helper-linux-\$\{\{ matrix\.asset_arch \}\}/);
  assert.match(buildWorkflow, /helper-assets/);
});

test("desktop release workflow uses the Tauri v2 bundles argument", () => {
  assert.doesNotMatch(buildWorkflow, /--bundler\b/);
  assert.match(buildWorkflow, /npx tauri build --target \$\{\{ matrix\.target \}\} --bundles nsis/);
  assert.match(buildWorkflow, /npx tauri build --target aarch64-apple-darwin --bundles dmg/);
});

test("desktop workflow publishes Windows installers for x64 and arm64", () => {
  const body = workflowJob("build-windows");
  assert.match(body, /fail-fast: false/);
  assert.match(body, /target: x86_64-pc-windows-msvc[\s\S]*artifact: CSSwitch-Windows-x64/);
  assert.match(body, /target: aarch64-pc-windows-msvc[\s\S]*artifact: CSSwitch-Windows-arm64/);
  assert.match(body, /targets: \$\{\{ matrix\.target \}\}/);
  assert.match(body, /npx tauri build --target \$\{\{ matrix\.target \}\} --bundles nsis/);
  assert.match(body, /name: \$\{\{ matrix\.artifact \}\}/);
  assert.match(body, /target\/\$\{\{ matrix\.target \}\}\/release\/bundle\/nsis\/\*\.exe/);
});

test("desktop workflow keeps macOS packaging aligned with upstream arm64 DMG releases", () => {
  const body = workflowJob("build-macos");
  assert.match(body, /runs-on: macos-15/);
  assert.match(body, /targets: aarch64-apple-darwin/);
  assert.match(body, /npx tauri build --target aarch64-apple-darwin --bundles dmg/);
  assert.match(body, /name: CSSwitch-macOS-arm64/);
  assert.match(body, /target\/aarch64-apple-darwin\/release\/bundle\/dmg\/\*\.dmg/);
  assert.doesNotMatch(body, /macos-15-intel/);
  assert.doesNotMatch(body, /x86_64-apple-darwin/);
  assert.doesNotMatch(body, /universal-apple-darwin/);
});

test("release job uploads all public release assets", () => {
  const body = workflowJob("release");
  assert.match(body, /CSSwitch-Windows-\*\/\*\.exe/);
  assert.match(body, /CSSwitch-macOS-arm64\/\*\.dmg/);
  assert.match(body, /csswitch-helper-\*\/\*/);
});

test("linux helper release workflow uses cross for musl target builds", () => {
  const body = workflowJob("build-helper");
  assert.match(body, /fail-fast: false/);
  assert.match(body, /taiki-e\/install-action@v2/);
  assert.match(body, /tool: cross/);
  assert.match(body, /CSSWITCH_BUNDLED_PROXY_DIR: \$\{\{ github\.workspace \}\}\/proxy/);
  assert.match(body, /cross build --bin csswitch-helper --no-default-features --release --target \$\{\{ matrix\.target \}\}/);
  assert.doesNotMatch(body, /cargo build --bin csswitch-helper --no-default-features --release --target \$\{\{ matrix\.target \}\}/);
  assert.doesNotMatch(body, /apt-get install -y musl-tools/);
});

test("linux helper cross build mounts the bundled proxy directory into the container", () => {
  assert.match(crossToml, /\[build\.env\]/);
  assert.match(crossToml, /volumes\s*=\s*\[[\s\S]*"CSSWITCH_BUNDLED_PROXY_DIR"[\s\S]*\]/);
  assert.match(crossToml, /passthrough\s*=\s*\[[\s\S]*"CSSWITCH_BUNDLED_PROXY_DIR"[\s\S]*\]/);
  assert.match(tauriBuildRs, /CSSWITCH_BUNDLED_PROXY_DIR/);
});

test("tauri build script validates bundled proxy resources with a clear error", () => {
  assert.match(tauriBuildRs, /fn require_bundled_proxy_file/);
  assert.match(tauriBuildRs, /csswitch_proxy\.py/);
  assert.match(tauriBuildRs, /dsml_shim\.py/);
  assert.match(tauriBuildRs, /\.is_file\(\)/);
  assert.match(tauriBuildRs, /panic!\([\s\S]*CSSWITCH_BUNDLED_PROXY_DIR/);
});

test("linux test workflow installs Tauri system dependencies before cargo tests", () => {
  const body = workflowJob("test");
  assert.match(body, /Install Linux desktop dependencies/);
  assert.ok(
    body.indexOf("Install Linux desktop dependencies") < body.indexOf("Run Tests"),
    "Linux desktop dependencies should be installed before Rust tests compile Tauri crates",
  );
  for (const pkg of [
    "libwebkit2gtk-4.1-dev",
    "build-essential",
    "curl",
    "wget",
    "file",
    "libxdo-dev",
    "libssl-dev",
    "libayatana-appindicator3-dev",
    "librsvg2-dev",
  ]) {
    assert.match(body, new RegExp(pkg.replaceAll(".", "\\.")));
  }
});

test("macOS desktop builds bundle the linux helper assets too", () => {
  const body = workflowJob("build-macos");
  assert.match(body, /needs: build-helper/);
  assert.match(body, /Download Linux Helper Assets/);
  assert.match(body, /pattern: csswitch-helper-linux-\*/);
  assert.match(body, /path: desktop\/src-tauri\/helper-assets/);
  assert.match(body, /npx tauri build --target aarch64-apple-darwin --bundles dmg/);
  assert.match(buildWorkflow, /CSSwitch-macOS-arm64/);
});

test("macOS one-click command keeps app and state names available under cfg", () => {
  const m = libTauri.match(/fn one_click_login\([\s\S]*?\n}\n\n\/\/\/ 从/);
  assert.ok(m, "one_click_login body should be discoverable");
  const body = m[0];
  assert.match(body, /app: tauri::AppHandle/);
  assert.match(body, /state: State<'_, Mutex<AppState>>/);
  assert.doesNotMatch(body, /_app: tauri::AppHandle/);
  assert.doesNotMatch(body, /_state: State<'_, Mutex<AppState>>/);
  assert.match(body, /ensure_proxy\(&app, &state\)/);
  assert.match(body, /stop_sandbox_inner\(&app, &mut st\)/);
  assert.match(body, /asset_root\(&app\)/);
});

test("remote one-click backend starts proxy and sandbox and returns access info", () => {
  const m = remoteCommands.match(/pub fn remote_one_click[\s\S]*?\n}\n\n\/\/ ==========================================================================/);
  assert.ok(m, "remote_one_click body should be discoverable");
  const body = m[0];
  assert.match(body, /remote_active_config_for_start/);
  assert.match(body, /"proxy"\.to_string\(\),\s*"stop"\.to_string\(\)[\s\S]*"proxy"\.to_string\(\),\s*"start"\.to_string\(\)/);
  assert.match(body, /"sandbox"\.to_string\(\),\s*"stop"\.to_string\(\)[\s\S]*"sandbox"\.to_string\(\),\s*"start"\.to_string\(\)/);
  assert.match(body, /proxy_url/);
  assert.match(body, /tunnel_hint/);
  assert.match(body, /local_url/);
});

test("remote one-click keeps the requested proxy port instead of drifting", () => {
  const m = remoteCommands.match(/pub fn remote_one_click[\s\S]*?\n}\n\n\/\/ ==========================================================================/);
  assert.ok(m, "remote_one_click body should be discoverable");
  const body = m[0];
  assert.doesNotMatch(body, /for candidate_proxy_port in proxy_port\.\.=proxy_port\.saturating_add\(20\)/);
  assert.doesNotMatch(body, /selected_proxy_port/);
  assert.match(body, /"proxy_port": proxy_port/);
  assert.match(body, /启动远程代理失败/);
});

test("remote one-click returns the fresh Science URL from sandbox start", () => {
  const m = remoteCommands.match(/pub fn remote_one_click[\s\S]*?\n}\n\n\/\/ ==========================================================================/);
  assert.ok(m, "remote_one_click body should be discoverable");
  const body = m[0];
  assert.match(body, /sandbox_result\["url"\]\s*\.as_str\(\)/);
  assert.match(body, /"local_url": local_url/);
});

test("remote helper status reports the configured sandbox state", () => {
  assert.match(helperCommands, /fn sandbox_is_running/);
  assert.match(helperCommands, /fn get_configured_sandbox_port/);
  assert.match(helperCommands, /"sandbox_running": sandbox_is_running\(\)/);
});

test("remote helper sandbox stop is idempotent before requiring Science", () => {
  const m = helperCommands.match(/pub fn cmd_sandbox_stop[\s\S]*?\n}\n\n\/\/\/ `logs/);
  assert.ok(m, "cmd_sandbox_stop body should be discoverable");
  const body = m[0];
  assert.match(body, /if !sandbox_is_running\(\)[\s\S]*CliEnvelope::ok/);
  assert.match(body, /find_cmd\("claude-science"\)/);
  assert.ok(
    body.indexOf("if !sandbox_is_running()") < body.indexOf('find_cmd("claude-science")'),
    "not-running sandbox should return ok before requiring the binary",
  );
});

test("remote helper sandbox start returns a fresh claude-science url", () => {
  const m = helperCommands.match(/pub fn cmd_sandbox_start[\s\S]*?\n}\n\n\/\/\/ `sandbox stop/);
  assert.ok(m, "cmd_sandbox_start body should be discoverable");
  const body = m[0];
  assert.match(body, /sandbox_fresh_url/);
  assert.match(body, /\.args\(\["url", "--data-dir"\]\)/);
  assert.match(body, /"url": url/);
});

test("remote helper clears stale Science processes and waits for a usable sandbox url", () => {
  const m = helperCommands.match(/pub fn cmd_sandbox_start[\s\S]*?\n}\n\n\/\/\/ `sandbox stop/);
  assert.ok(m, "cmd_sandbox_start body should be discoverable");
  const body = m[0];
  assert.match(helperCommands, /fn wait_for_sandbox_ready/);
  assert.match(helperCommands, /fn terminate_sandbox_processes/);
  assert.match(helperCommands, /fn matching_sandbox_pids/);
  assert.ok(
    body.indexOf("terminate_sandbox_processes") < body.indexOf(".spawn()"),
    "sandbox start must clear stale same-data-dir Science processes before spawning",
  );
  assert.match(helperCommands, /let url = sandbox_fresh_url/);
  assert.match(helperCommands, /http_health\(port, None/);
  assert.match(body, /sandbox\.log/);
  assert.doesNotMatch(
    body,
    /stdout\(std::process::Stdio::null\(\)\)[\s\S]*stderr\(std::process::Stdio::null\(\)\)[\s\S]*\.spawn\(\)/,
    "serve startup output should not be discarded while diagnosing daemon startup failures",
  );
});

test("remote helper sandbox start binds Science to loopback for SSH tunnel access", () => {
  const m = helperCommands.match(/pub fn cmd_sandbox_start[\s\S]*?\n}\n\n\/\/\/ `sandbox stop/);
  assert.ok(m, "cmd_sandbox_start body should be discoverable");
  const body = m[0];
  assert.match(body, /\.arg\("--host"\)\s*\.arg\("127\.0\.0\.1"\)/);
  assert.doesNotMatch(body, /\.arg\("0\.0\.0\.0"\)/);
});

test("remote helper carries and installs the managed proxy script", () => {
  assert.match(
    helperCommands,
    /include_str!\(concat!\([\s\S]*env!\("CSSWITCH_BUNDLED_PROXY_DIR"\)[\s\S]*"\/csswitch_proxy\.py"[\s\S]*\)\)/,
  );
  assert.match(
    helperCommands,
    /include_str!\(concat!\([\s\S]*env!\("CSSWITCH_BUNDLED_PROXY_DIR"\)[\s\S]*"\/dsml_shim\.py"[\s\S]*\)\)/,
  );
  assert.match(helperCommands, /fn ensure_managed_proxy_script\(\) -> Result<PathBuf, String>/);
  assert.match(helperCommands, /"~\/\.csswitch\/proxy\/csswitch_proxy\.py"/);
  assert.match(helperCommands, /dsml_shim\.py/);
  assert.match(helperCommands, /BUNDLED_PROXY/);
});

test("remote helper prefers the self-healed managed proxy bundle after explicit overrides", () => {
  const m = helperCommands.match(/fn proxy_script_path[\s\S]*?\n}\n\n\/\/ ==========/);
  assert.ok(m, "proxy_script_path body should be discoverable");
  const body = m[0];
  assert.ok(
    body.indexOf('std::env::var("CSSWITCH_PROXY_DIR")') < body.indexOf("ensure_managed_proxy_script()"),
    "explicit CSSWITCH_PROXY_DIR override should remain first",
  );
  assert.doesNotMatch(body, /current_exe\(\)[\s\S]*ensure_managed_proxy_script\(\)/);
});

test("remote helper searches user-local binary directories for Science", () => {
  const m = helperCommands.match(/fn find_cmd[\s\S]*?\n}/);
  assert.ok(m, "find_cmd body should be discoverable");
  const body = m[0];
  assert.match(body, /\.local"\)\.join\("bin"\)/);
  assert.match(body, /miniconda3"\)\.join\("bin"\)/);
  assert.match(body, /anaconda3"\)\.join\("bin"\)/);
});

test("remote helper injects relay profile connection fields into proxy env", () => {
  assert.match(helperCommands, /fn proxy_launch_from_config/);
  assert.match(helperCommands, /"CSSWITCH_RELAY_KEY"/);
  assert.match(helperCommands, /"CSSWITCH_RELAY_BASE_URL"/);
  assert.match(helperCommands, /"CSSWITCH_RELAY_MODEL"/);
  assert.match(helperCommands, /"CSSWITCH_RELAY_THINKING"/);
  assert.doesNotMatch(helperCommands, /_ => "DEEPSEEK_API_KEY"/);
});

test("remote helper clears an unhealthy proxy port before spawning a replacement", () => {
  assert.match(helperCommands, /fn clear_unhealthy_proxy_port/);
  assert.match(helperCommands, /fn stop_recorded_proxy/);
  assert.match(helperCommands, /pid_looks_like_recorded_proxy/);
  assert.match(helperCommands, /stop_recorded_proxy\(port\)/);
  assert.match(helperCommands, /clear_unhealthy_proxy_port\(port\)/);
  assert.match(helperCommands, /port_in_use/);
});
