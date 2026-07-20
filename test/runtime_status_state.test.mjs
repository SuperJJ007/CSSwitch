import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  RUNTIME_STATUS_LABELS,
  aggregateRuntimeStatus,
  normalizeRuntimeLight,
} from "../desktop/src/runtime-status-state.js";

test("Codex 无独立 upstream 时只聚合适用层", () => {
  assert.equal(aggregateRuntimeStatus({ proxy: "green", sandbox: "green", upstream: "gray" }), "green");
  assert.equal(aggregateRuntimeStatus({ proxy: "amber", sandbox: "amber", upstream: "gray" }), "amber");
});

test("官方视图仍聚合第三方状态且不把打开请求当作健康证明", () => {
  assert.equal(
    aggregateRuntimeStatus(
      { proxy: "green", sandbox: "green", upstream: "gray" },
      { mode: "official", officialState: "green" },
    ),
    "green",
  );
  assert.equal(
    aggregateRuntimeStatus(
      { proxy: "red", sandbox: "green", upstream: "gray" },
      { mode: "official", officialState: "green" },
    ),
    "red",
  );
});

test("官方与第三方并存 UI 保留 managed 状态和显式停止边界", () => {
  const html = readFileSync(new URL("../desktop/src/index.html", import.meta.url), "utf8");
  const js = readFileSync(new URL("../desktop/src/main.js", import.meta.url), "utf8");
  const runtime = readFileSync(
    new URL("../desktop/src-tauri/src/commands/runtime.rs", import.meta.url),
    "utf8",
  );
  const statusList = html.match(/<div class="status-list([^\"]*)">/)?.[1] || "";
  assert.doesNotMatch(statusList, /tp-only/);
  assert.match(html, /id="stopBtn">停止第三方实例</);
  assert.match(html, /官方实例可与第三方隔离实例同时运行/);
  assert.doesNotMatch(js, /第三方代理\/沙箱已停|停止第三方代理\/沙箱并保存模式/);

  const setMode = runtime.slice(runtime.indexOf("fn set_mode_inner("), runtime.indexOf("/// 官方模式"));
  assert.doesNotMatch(setMode, /stop_sandbox_state|stop_proxy|bump_generation/);
  assert.match(setMode, /lifecycle\.with_serialized/);
});

test("未知状态保持中性，明确失败才变红", () => {
  assert.equal(normalizeRuntimeLight("not-a-status"), "unknown");
  assert.equal(aggregateRuntimeStatus({ proxy: "green", sandbox: "green" }), "gray");
  assert.equal(aggregateRuntimeStatus({ proxy: "red", sandbox: "green", upstream: "gray" }), "red");
  assert.equal(RUNTIME_STATUS_LABELS.unknown, "状态未知");
});

test("运行反馈统一显示在右上角且不会触发页面滚动", () => {
  const css = readFileSync(new URL("../desktop/src/styles.css", import.meta.url), "utf8");
  const js = readFileSync(new URL("../desktop/src/main.js", import.meta.url), "utf8");
  const feedbackRule = css.match(/\.feedback\s*\{([^}]+)\}/)?.[1] || "";
  assert.match(feedbackRule, /top:\s*18px/);
  assert.match(feedbackRule, /right:\s*22px/);
  assert.match(feedbackRule, /bottom:\s*auto/);
  assert.doesNotMatch(feedbackRule, /bottom:\s*18px/);
  assert.match(css, /\.feedback\s*\{\s*position:\s*fixed;\s*top:\s*62px;\s*right:\s*12px;\s*bottom:\s*auto;/);
  const setMsg = js.slice(js.indexOf("function setMsg("), js.indexOf("function setBrowserFallback("));
  assert.doesNotMatch(setMsg, /scrollIntoView/);
});

test("运行时窗口重设与 Tauri 默认尺寸保持一致", () => {
  const js = readFileSync(new URL("../desktop/src/main.js", import.meta.url), "utf8");
  const tauri = JSON.parse(readFileSync(new URL("../desktop/src-tauri/tauri.conf.json", import.meta.url), "utf8"));
  const testTauri = JSON.parse(readFileSync(new URL("./tauri.real-machine.conf.json", import.meta.url), "utf8"));
  const mainWindow = tauri.app.windows.find((item) => item.label === "main");
  const testWindow = testTauri.app.windows.find((item) => item.label === "main");
  assert.deepEqual([mainWindow.width, mainWindow.height], [920, 650.5]);
  assert.deepEqual([testWindow.width, testWindow.height], [920, 650.5]);

  const configureWindow = js.slice(
    js.indexOf("async function configureDesktopWindow()"),
    js.indexOf("function renderCurrentSummary()"),
  );
  assert.match(configureWindow, /setMinSize\(new LogicalSize\(760, 520\)\)/);
  assert.match(configureWindow, /setSize\(new LogicalSize\(920, 650\.5\)\)/);
  assert.doesNotMatch(configureWindow, /setSize\(new LogicalSize\(920, 600\)\)/);
});
