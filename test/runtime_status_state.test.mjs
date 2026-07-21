import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  RUNTIME_STATUS_LABELS,
  aggregateRuntimeStatus,
  normalizeRuntimeLight,
  scienceRuntimeStatusText,
} from "../desktop/src/runtime-status-state.js";

test("Codex 无独立 upstream 时只聚合适用层", () => {
  assert.equal(aggregateRuntimeStatus({ proxy: "green", sandbox: "green", upstream: "gray" }), "green");
  assert.equal(aggregateRuntimeStatus({ proxy: "amber", sandbox: "amber", upstream: "gray" }), "amber");
});

test("官方模式不把打开请求当作健康证明", () => {
  assert.equal(aggregateRuntimeStatus({}, { mode: "official", officialState: "gray" }), "gray");
});

test("未知状态保持中性，明确失败才变红", () => {
  assert.equal(normalizeRuntimeLight("not-a-status"), "unknown");
  assert.equal(aggregateRuntimeStatus({ proxy: "green", sandbox: "green" }), "gray");
  assert.equal(aggregateRuntimeStatus({ proxy: "red", sandbox: "green", upstream: "gray" }), "red");
  assert.equal(RUNTIME_STATUS_LABELS.unknown, "状态未知");
});

test("隔离 Science 只展示 runtime 来源且不声称自己检查更新", () => {
  assert.equal(
    scienceRuntimeStatusText({
      runtime: { source: "official_downloaded", version: "claude-science 0.1.15" },
    }),
    "官方已下载 Runtime · claude-science 0.1.15；隔离实例仅复用程序文件。",
  );
  assert.match(
    scienceRuntimeStatusText({
      runtime: { source: "installed_app", version: "claude-science 0.1.0" },
    }),
    /App 内置备用 Runtime.*官方下载 Runtime 当前不可用/,
  );
  assert.match(scienceRuntimeStatusText(null), /隔离实例不登录或检查更新/);
  assert.doesNotMatch(scienceRuntimeStatusText(null), /已是最新|up to date/i);
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
