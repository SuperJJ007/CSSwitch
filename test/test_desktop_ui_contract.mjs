import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const html = readFileSync(new URL("../desktop/src/index.html", import.meta.url), "utf8");
const main = readFileSync(new URL("../desktop/src/main.js", import.meta.url), "utf8");
const remoteCommands = readFileSync(new URL("../desktop/src-tauri/src/remote_commands.rs", import.meta.url), "utf8");
const helperCommands = readFileSync(new URL("../desktop/src-tauri/src/cli/commands.rs", import.meta.url), "utf8");

function remoteStartProxyBody() {
  const m = remoteCommands.match(/pub fn remote_start_proxy[\s\S]*?\n}\n\n\/\/\/ 停止远程代理/);
  assert.ok(m, "remote_start_proxy body should be discoverable");
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

test("remote one-click retries the proxy port when the requested one is occupied", () => {
  const m = remoteCommands.match(/pub fn remote_one_click[\s\S]*?\n}\n\n\/\/ ==========================================================================/);
  assert.ok(m, "remote_one_click body should be discoverable");
  const body = m[0];
  assert.match(body, /for candidate_proxy_port in proxy_port\.\.=proxy_port\.saturating_add\(20\)/);
  assert.match(body, /e\.code == "port_in_use"/);
  assert.match(body, /selected_proxy_port/);
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
  assert.match(helperCommands, /clear_unhealthy_proxy_port\(port\)/);
  assert.match(helperCommands, /port_in_use/);
});
