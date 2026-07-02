# Phase 0 加固 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修掉 GPT 两轮评审确认的 8 个加固项（代理 + 伪造器 + 启动/停止脚本），并把回归测试从失效的 `qwen_proxy.py` 目标迁到当前主实现 `csswitch_proxy.py`，让 `unittest`/`node --test`/bash 三套测试都能发现并全绿。

**Architecture:** 只加固现有 Python/Node/shell，不引入 Tauri，不改架构。代理保持 stdlib-only；伪造器保持 Node 单文件；脚本保持 zsh。所有改动都先写失败测试再实现（TDD）。为可测性增加三处受控开关（`CSSWITCH_AUTH_TOKEN`/`--auth-token`、`CSSWITCH_UPSTREAM_URL`、脚本 `--dry-run` 与 `SCIENCE_BIN`/`SANDBOX_HOME` 覆盖），都带默认值、对生产行为零影响。

**Tech Stack:** Python 3（conda，stdlib only：`http.server`/`urllib`/`unittest`）、Node 22（`node:test`、`node:crypto`）、zsh + bash 测试脚本。

## Global Constraints

以下为 spec 的全项目约束，每个 Task 都隐含遵守：

- 铁律：绝不读写真实 `~/.claude-science` 及其登录文件；绝不用端口 8765；沙箱一律独立 HOME + 独立 data-dir + 独立端口。本 Phase 全部测试只在临时目录与回环高位端口上跑，不启动 Science、不碰 OAuth、不碰 CC Switch。
- 本地 git only：Phase 0 可 `git init` 本地仓库并频繁提交，但**不加 remote、不 push、不上传 GitHub**（上传属 Phase 2，时机由用户定）。
- Python 用 conda 环境，避免系统 3.9；代理本体 stdlib-only，两者皆可跑，测试统一用 conda `python3`。
- 中文写作：注释与文档不使用任何破折号（——、—、--），不使用「不是……而是……」句型（命令行参数里的 `--flag` 是字面语法，不受此约束）。
- 代理安全不变量：入站 Authorization / x-api-key 一律剥离不转发；provider key 只驻内存、不打印、不写日志；只监听回环。
- 可测性开关必须有生产默认值：未设 `CSSWITCH_AUTH_TOKEN` 时不启用鉴权（保持旧行为）；未设 `CSSWITCH_UPSTREAM_URL` 时用注册表里的官方地址；脚本不带 `--dry-run` 时行为不变。

---

## 文件结构

改动与新增：

- Modify `proxy/csswitch_proxy.py`
  - 新增 `map_tool_choice()`（7.4）
  - 改 `clamp_max_tokens()` 为按模型查表（7.5），PROVIDERS 增 `model_caps`/`default_cap`
  - `anthropic_to_openai()` 补 tool_choice/stop/top_p（7.4）
  - 新增 `AUTH_SECRET` 全局 + `_auth_ok()` 路径 secret 校验（7.2）
  - `_handle_anthropic()` 加 `headers_sent` 守卫 + `_sse_error_and_terminate()`（7.3）
  - 启动增 `--auth-token` 与 `CSSWITCH_AUTH_TOKEN`、`CSSWITCH_UPSTREAM_URL`；模块顶补 `PROV_NAME=None`、`AUTH_SECRET=None`
- Modify `scripts/make-virtual-oauth.mjs`（7.1）：realpath 目录护栏、叶子文件 lstat 拒链接、临时文件+原子 rename
- Modify `scripts/stop-science-sandbox.sh`（7.6）：按退出码如实报告、`SCIENCE_BIN`/`SANDBOX_HOME` 覆盖
- Modify `scripts/launch-virtual-sandbox.sh` 与 `scripts/launch-science-sandbox.sh`（7.7）：端口整数归一化、data-dir realpath 比、`--dry-run`
- Delete `test/proxy_e2e_test.py`（打的是被取代的 qwen_proxy.py，`unittest discover` 命名不匹配发现 0 个）
- Create `test/test_proxy_units.py`（7.4/7.5 纯函数）
- Create `test/test_proxy_auth.py`（7.2 集成，子进程 + mock 上游）
- Create `test/test_proxy_stream.py`（7.3 集成，raw socket + 断流 mock）
- Create `test/mock_upstream.py`（测试共享：可记账、可断流的假上游）
- Create `test/test_make_virtual_oauth.mjs`（7.1，`node --test`）
- Create `test/test_scripts.sh`（7.6/7.7，bash 断言）

---

## Task 0: 本地 git 初始化与测试基线

**Files:**
- Create: （无源码文件；建立本地仓库与首个基线提交）

**Interfaces:**
- Produces: 一个本地 git 仓库，后续每个 Task 的 commit 步骤有落点。

- [ ] **Step 1: 确认不在既有仓库内、且 .gitignore 已挡敏感物**

Run: `git -C /Users/superjj/ccproj/CSswitch rev-parse --is-inside-work-tree 2>&1; cat /Users/superjj/ccproj/CSswitch/.gitignore`
Expected: 第一行报 `not a git repository`（确认当前无仓库）；`.gitignore` 含 `.sandbox/`、`*.env`、`*token*`、`*secret*`。

- [ ] **Step 2: 本地初始化（不加 remote、不 push）**

```bash
cd /Users/superjj/ccproj/CSswitch
git init
git add -A
git status --short | head
```

- [ ] **Step 3: 基线提交前再扫一遍不入库敏感物**

Run: `cd /Users/superjj/ccproj/CSswitch && git ls-files | grep -E '\.sandbox/|\.env$|oauth-tokens|encryption\.key' || echo CLEAN`
Expected: 输出 `CLEAN`（.sandbox 等未被纳入暂存）。

- [ ] **Step 4: 首个基线提交**

```bash
cd /Users/superjj/ccproj/CSswitch
git commit -m "chore: baseline before Phase 0 hardening

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```
Expected: 提交成功，返回 commit hash。

---

## Task 1: [7.4] Qwen 翻译补 tool_choice / stop / top_p

**Files:**
- Modify: `proxy/csswitch_proxy.py`（新增 `map_tool_choice`；`anthropic_to_openai` 尾部补字段）
- Delete: `test/proxy_e2e_test.py`
- Create: `test/test_proxy_units.py`

**Interfaces:**
- Produces:
  - `map_tool_choice(tc: dict | None, tools: list | None) -> str | dict | None`
  - `anthropic_to_openai(req: dict) -> dict` 的返回值在有 `tool_choice`/`stop_sequences`/`top_p` 时分别带 `tool_choice`/`stop`/`top_p`。

- [ ] **Step 1: 删掉打错对象的旧测试**

```bash
cd /Users/superjj/ccproj/CSswitch
git rm test/proxy_e2e_test.py
```

- [ ] **Step 2: 写失败测试 `test/test_proxy_units.py`**

```python
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "proxy"))
import csswitch_proxy as cs


class ToolChoiceMapping(unittest.TestCase):
    def setUp(self):
        cs.PROV = cs.PROVIDERS["qwen"]

    def test_tool_named_maps_to_function(self):
        out = cs.map_tool_choice({"type": "tool", "name": "grade"}, [{"name": "grade"}])
        self.assertEqual(out, {"type": "function", "function": {"name": "grade"}})

    def test_any_single_tool_names_that_function(self):
        out = cs.map_tool_choice({"type": "any"}, [{"name": "only"}])
        self.assertEqual(out, {"type": "function", "function": {"name": "only"}})

    def test_any_multi_tool_falls_back_to_required(self):
        out = cs.map_tool_choice({"type": "any"}, [{"name": "a"}, {"name": "b"}])
        self.assertEqual(out, "required")

    def test_auto_and_none_passthrough(self):
        self.assertEqual(cs.map_tool_choice({"type": "auto"}, []), "auto")
        self.assertEqual(cs.map_tool_choice({"type": "none"}, []), "none")

    def test_missing_returns_none(self):
        self.assertIsNone(cs.map_tool_choice(None, []))

    def test_translation_carries_tool_choice_stop_top_p(self):
        req = {
            "model": "claude-haiku-4-5",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "grade", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "grade"},
            "stop_sequences": ["STOP"],
            "top_p": 0.5,
        }
        out = cs.anthropic_to_openai(req)
        self.assertEqual(out["tool_choice"], {"type": "function", "function": {"name": "grade"}})
        self.assertEqual(out["stop"], ["STOP"])
        self.assertEqual(out["top_p"], 0.5)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 3: 运行，确认失败**

Run: `cd /Users/superjj/ccproj/CSswitch && python3 -m unittest test.test_proxy_units -v`
Expected: FAIL，`AttributeError: module 'csswitch_proxy' has no attribute 'map_tool_choice'`。

- [ ] **Step 4: 在 `proxy/csswitch_proxy.py` 新增 `map_tool_choice`（放在 `openai_to_anthropic` 之前）**

```python
def map_tool_choice(tc, tools):
    """把 Anthropic tool_choice 译成 OpenAI 兼容取值。
    any 不做通用映射：单工具直接指定该函数（等效强制且不依赖 required）；
    多工具退 "required"（DashScope 若不支持会以上游错误显式暴露，不静默退化）。"""
    if not isinstance(tc, dict):
        return None
    t = tc.get("type")
    if t == "auto":
        return "auto"
    if t == "none":
        return "none"
    if t == "tool" and tc.get("name"):
        return {"type": "function", "function": {"name": tc["name"]}}
    if t == "any":
        names = [x["name"] for x in (tools or []) if x.get("name")]
        if len(names) == 1:
            return {"type": "function", "function": {"name": names[0]}}
        return "required"
    return None
```

- [ ] **Step 5: 在 `anthropic_to_openai` 的 `return out` 之前补三字段**

在现有 tools 组装块之后、`return out` 之前插入：

```python
    tcm = map_tool_choice(req.get("tool_choice"), req.get("tools"))
    if tcm is not None:
        out["tool_choice"] = tcm
    if req.get("stop_sequences"):
        out["stop"] = req["stop_sequences"]
    if req.get("top_p") is not None:
        out["top_p"] = req["top_p"]
```

- [ ] **Step 6: 运行，确认通过**

Run: `cd /Users/superjj/ccproj/CSswitch && python3 -m unittest test.test_proxy_units -v`
Expected: PASS（6 个用例全绿）。

- [ ] **Step 7: 确认 unittest 能发现（原为 0 个）**

Run: `cd /Users/superjj/ccproj/CSswitch && python3 -m unittest discover -s test -p 'test_*.py' -v 2>&1 | tail -3`
Expected: 报告 `Ran N tests`（N ≥ 6），OK。

- [ ] **Step 8: Commit**

```bash
cd /Users/superjj/ccproj/CSswitch
git add proxy/csswitch_proxy.py test/test_proxy_units.py
git commit -m "fix(proxy): translate tool_choice/stop/top_p for qwen path (7.4)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 2: [7.5] max_tokens 按模型查表，不按 provider 一刀切

**Files:**
- Modify: `proxy/csswitch_proxy.py`（PROVIDERS 增 `model_caps`/`default_cap`，改 `clamp_max_tokens` 签名与两处调用）
- Modify: `test/test_proxy_units.py`（增一组用例）

**Interfaces:**
- Produces: `clamp_max_tokens(v: int | None, model: str | None = None) -> int | None`，cap 取自 `PROV["model_caps"][model]`，缺则 `PROV["default_cap"]`。

- [ ] **Step 1: 追加失败测试到 `test/test_proxy_units.py`**

在文件末尾 `if __name__` 之前插入：

```python
class MaxTokensPerModel(unittest.TestCase):
    def setUp(self):
        cs.PROV = cs.PROVIDERS["deepseek"]

    def test_cap_uses_target_model_entry(self):
        self.assertEqual(cs.clamp_max_tokens(100000, "deepseek-v4-pro"), 65536)
        self.assertEqual(cs.clamp_max_tokens(100000, "deepseek-v4-flash"), 32768)

    def test_unknown_model_uses_default_cap(self):
        self.assertEqual(cs.clamp_max_tokens(100000, "who-knows"), 8192)

    def test_not_clamped_below_request(self):
        self.assertEqual(cs.clamp_max_tokens(500, "deepseek-v4-pro"), 500)

    def test_none_passthrough(self):
        self.assertIsNone(cs.clamp_max_tokens(None, "deepseek-v4-pro"))

    def test_qwen_per_model(self):
        cs.PROV = cs.PROVIDERS["qwen"]
        self.assertEqual(cs.clamp_max_tokens(100000, "qwen-max"), 8192)
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd /Users/superjj/ccproj/CSswitch && python3 -m unittest test.test_proxy_units.MaxTokensPerModel -v`
Expected: FAIL（`clamp_max_tokens()` 目前只接受 1 个位置参数，或断言值不符）。

- [ ] **Step 3: PROVIDERS 里把 `max_tokens_cap` 换成 `model_caps` + `default_cap`**

deepseek 块：删除 `"max_tokens_cap": 8192,`，在 `"model_map"` 之后加：

```python
        # 每模型输出上限。provisional：待 §12.3 拉官方模型列表核对真实上限后校准。
        "model_caps": {
            "deepseek-v4-pro": 65536,
            "deepseek-v4-flash": 32768,
        },
        "default_cap": 8192,
```

qwen 块：删除 `"max_tokens_cap": 8192,`，在 `"model_map"` 之后加：

```python
        # provisional：待核对 DashScope 各模型真实上限。
        "model_caps": {
            "qwen-max": 8192,
            "qwen-plus": 8192,
            "qwen-turbo": 8192,
        },
        "default_cap": 8192,
```

- [ ] **Step 4: 改 `clamp_max_tokens` 签名为按模型查表**

替换现有函数：

```python
def clamp_max_tokens(v, model=None):
    if not v:
        return v
    caps = PROV.get("model_caps") or {}
    cap = caps.get(model, PROV.get("default_cap"))
    if cap:
        return min(int(v), cap)
    return v
```

- [ ] **Step 5: 更新两处调用点传入解析后的目标模型**

`anthropic_to_openai` 内（原 `out["max_tokens"] = clamp_max_tokens(req["max_tokens"])`）：

```python
    if req.get("max_tokens"):
        out["max_tokens"] = clamp_max_tokens(req["max_tokens"], out["model"])
```

`_handle_anthropic` 内（原 `body["max_tokens"] = clamp_max_tokens(body["max_tokens"])`）：

```python
        if body.get("max_tokens"):
            body["max_tokens"] = clamp_max_tokens(body["max_tokens"], target)
```

- [ ] **Step 6: 运行，确认全组通过**

Run: `cd /Users/superjj/ccproj/CSswitch && python3 -m unittest test.test_proxy_units -v`
Expected: PASS（Task 1 + Task 2 共 11 个用例全绿）。

- [ ] **Step 7: Commit**

```bash
cd /Users/superjj/ccproj/CSswitch
git add proxy/csswitch_proxy.py test/test_proxy_units.py
git commit -m "fix(proxy): per-model max_tokens cap instead of per-provider (7.5)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 3: [7.2] 代理 path secret 鉴权 + 可测上游覆盖

**Files:**
- Modify: `proxy/csswitch_proxy.py`（模块顶 `PROV_NAME=None`/`AUTH_SECRET=None`；`_auth_ok()`；do_GET/do_POST 首行校验；启动读 `--auth-token`/env 与 `CSSWITCH_UPSTREAM_URL`）
- Create: `test/mock_upstream.py`
- Create: `test/test_proxy_auth.py`

**Interfaces:**
- Consumes: `csswitch_proxy` 作为子进程启动（`--provider deepseek --port P --auth-token SEC`），env `DEEPSEEK_API_KEY`、`CSSWITCH_UPSTREAM_URL`。
- Produces:
  - `H._auth_ok(self) -> bool`：无 `AUTH_SECRET` 时恒 True；有则要求 `/<secret>/...` 前缀，命中则把 `self.path` 剥去前缀返回 True，否则发 403 返回 False。
  - `test/mock_upstream.py` 提供 `start_mock(mode="json") -> (base_url, hits, stop)`，`hits` 是可读的命中计数列表，`stop()` 关服务。

- [ ] **Step 1: 写共享 mock 上游 `test/mock_upstream.py`**

```python
"""测试用假上游：记账命中次数，按 mode 返回不同响应。
mode="json"：返回一份最小 Anthropic 非流式消息体。"""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def start_mock(mode="json"):
    hits = []

    class M(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_POST(self):
            n = int(self.headers.get("Content-Length") or 0)
            self.rfile.read(n)
            hits.append(self.path)
            if mode == "json":
                body = json.dumps({
                    "id": "msg_mock", "type": "message", "role": "assistant",
                    "model": "mock", "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn", "stop_sequence": None,
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                }).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

    srv = ThreadingHTTPServer(("127.0.0.1", 0), M)
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{port}/up", hits, srv.shutdown
```

- [ ] **Step 2: 写失败测试 `test/test_proxy_auth.py`**

```python
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request

HERE = os.path.dirname(__file__)
sys.path.insert(0, HERE)
from mock_upstream import start_mock

PROXY = os.path.join(HERE, "..", "proxy", "csswitch_proxy.py")
SEC = "s3cr3t-test-token"


def _req(url, method="GET", body=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(url, data=data, method=method,
                              headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(r, timeout=5) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


class ProxyAuth(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.up_url, cls.hits, cls.stop_up = start_mock("json")
        port = 18990
        cls.base = f"http://127.0.0.1:{port}"
        cls.logf = os.path.join(tempfile.gettempdir(), f"csswitch-auth-{port}.log")
        open(cls.logf, "w").close()
        env = dict(os.environ, DEEPSEEK_API_KEY="fake-key",
                   CSSWITCH_UPSTREAM_URL=cls.up_url)
        cls.proc = subprocess.Popen(
            [sys.executable, PROXY, "--provider", "deepseek",
             "--port", str(port), "--auth-token", SEC, "--log", cls.logf],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(50):
            try:
                s, _b = _req(f"{cls.base}/{SEC}/health")
                if s == 200:
                    break
            except Exception:
                pass
            time.sleep(0.1)

    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate()
        cls.proc.wait(timeout=5)
        cls.stop_up()

    def test_health_with_secret_ok(self):
        s, _b = _req(f"{self.base}/{SEC}/health")
        self.assertEqual(s, 200)

    def test_health_without_secret_forbidden(self):
        s, _b = _req(f"{self.base}/health")
        self.assertEqual(s, 403)

    def test_messages_without_secret_forbidden_and_upstream_untouched(self):
        before = len(self.hits)
        s, _b = _req(f"{self.base}/v1/messages", "POST",
                     {"model": "claude-opus-4-8", "max_tokens": 10,
                      "messages": [{"role": "user", "content": "hi"}]})
        self.assertEqual(s, 403)
        self.assertEqual(len(self.hits), before)

    def test_messages_with_secret_forwarded(self):
        before = len(self.hits)
        s, b = _req(f"{self.base}/{SEC}/v1/messages", "POST",
                    {"model": "claude-opus-4-8", "max_tokens": 10,
                     "messages": [{"role": "user", "content": "hi"}]})
        self.assertEqual(s, 200)
        self.assertEqual(len(self.hits), before + 1)
        self.assertEqual(json.loads(b)["content"][0]["text"], "ok")

    def test_secret_not_in_log(self):
        _s, _b = _req(f"{self.base}/{SEC}/health")
        with open(self.logf) as f:
            self.assertNotIn(SEC, f.read())


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 3: 运行，确认失败**

Run: `cd /Users/superjj/ccproj/CSswitch && python3 -m unittest test.test_proxy_auth -v`
Expected: FAIL（当前无鉴权：无 secret 也返回非 403；且不认 `CSSWITCH_UPSTREAM_URL`/`--auth-token`，启动或断言失败）。

- [ ] **Step 4: 模块顶补两个全局（放在 `LOG = None` 附近）**

```python
PROV_NAME = None  # 运行时设定；模块被 import 做测试时也要有定义，避免 handler NameError
AUTH_SECRET = None  # 未设则不启用鉴权（保持旧行为）
```

- [ ] **Step 5: 在 `H` 类里新增 `_auth_ok`（放在 `_sse` 之后）**

```python
    def _auth_ok(self):
        if not AUTH_SECRET:
            return True
        prefix = "/" + AUTH_SECRET
        if self.path == prefix or self.path.startswith(prefix + "/"):
            self.path = self.path[len(prefix):] or "/"
            return True
        self._send_json(403, {"type": "error", "error": {
            "type": "permission_error", "message": "forbidden"}})
        return False
```

- [ ] **Step 6: do_GET / do_POST 首行加校验**

`do_GET` 第一行、`do_POST` 第一行分别加：

```python
        if not self._auth_ok():
            return
```

- [ ] **Step 7: 启动段读 secret 与上游覆盖**

在 `KEY = load_key(PROV, args)` 之后、`if not KEY:` 之前加：

```python
    AUTH_SECRET = os.environ.get("CSSWITCH_AUTH_TOKEN") or args.auth_token
    _up = os.environ.get("CSSWITCH_UPSTREAM_URL")
    if _up:
        PROV = dict(PROV)
        PROV["url"] = _up
```

并给 argparse 增参数（与 `--log` 同级）：

```python
    ap.add_argument("--auth-token", default=None)
```

注意：`AUTH_SECRET`、`PROV` 在 `__main__` 段赋值即模块级全局；handler 引用同名全局，天然可见。secret 只用于路径比对，不进任何 `log()`。

- [ ] **Step 8: 运行，确认通过（含 secret 不落日志断言）**

Run: `cd /Users/superjj/ccproj/CSswitch && python3 -m unittest test.test_proxy_auth -v`
Expected: PASS（5 个用例全绿，含 `test_secret_not_in_log`）。

- [ ] **Step 9: Commit**

```bash
cd /Users/superjj/ccproj/CSswitch
git add proxy/csswitch_proxy.py test/mock_upstream.py test/test_proxy_auth.py
git commit -m "feat(proxy): path-secret auth + upstream override for tests (7.2)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 4: [7.3] 流中断后不再写坏响应

**Files:**
- Modify: `proxy/csswitch_proxy.py`（`_handle_anthropic` 加 `headers_sent` 守卫与流内读异常处理；新增 `_sse_error_and_terminate`）
- Create: `test/test_proxy_stream.py`（raw socket 客户端 + 断流 raw mock）

**Interfaces:**
- Produces: `H._sse_error_and_terminate(self, msg: str)`：把一个 Anthropic `event: error` SSE 帧作为 chunk 写出，再写终止块 `0\r\n\r\n`。
- 行为契约：一旦流式响应头已发出，任何上游读异常都走 SSE error 收尾，绝不再调用 `_send_json`（避免把 502 JSON 拼进 chunked 流）。

- [ ] **Step 1: 写失败测试 `test/test_proxy_stream.py`**

```python
import os
import socket
import subprocess
import sys
import threading
import time
import unittest

HERE = os.path.dirname(__file__)
PROXY = os.path.join(HERE, "..", "proxy", "csswitch_proxy.py")
SEC = "streamtok"


def start_dropping_upstream():
    """假上游：声明 Content-Length 很大，但只发少量含 SSE 帧的字节后立刻断开，
    逼客户端（代理）在流中途 read 抛 IncompleteRead。"""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(5)
    port = srv.getsockname()[1]

    def serve():
        while True:
            try:
                c, _ = srv.accept()
            except OSError:
                return
            c.recv(65536)
            head = ("HTTP/1.1 200 OK\r\n"
                    "Content-Type: text/event-stream\r\n"
                    "Content-Length: 100000\r\n\r\n")
            body = ("event: content_block_delta\n"
                    "data: {\"type\":\"content_block_delta\"}\n\n")
            c.sendall((head + body).encode())
            c.close()  # 远小于 100000，触发 IncompleteRead

    threading.Thread(target=serve, daemon=True).start()
    return f"http://127.0.0.1:{port}/up", srv


def raw_post(host, port, path, body):
    s = socket.create_connection((host, port), timeout=5)
    req = (f"POST {path} HTTP/1.1\r\nHost: {host}\r\n"
           f"Content-Type: application/json\r\nContent-Length: {len(body)}\r\n"
           f"Connection: close\r\n\r\n").encode() + body
    s.sendall(req)
    chunks = []
    while True:
        d = s.recv(65536)
        if not d:
            break
        chunks.append(d)
    s.close()
    return b"".join(chunks)


class StreamInterruption(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.up_url, cls.up_sock = start_dropping_upstream()
        cls.port = 18992
        env = dict(os.environ, DEEPSEEK_API_KEY="fake",
                   CSSWITCH_UPSTREAM_URL=cls.up_url)
        cls.proc = subprocess.Popen(
            [sys.executable, PROXY, "--provider", "deepseek",
             "--port", str(cls.port), "--auth-token", SEC],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(1.0)

    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate()
        cls.proc.wait(timeout=5)
        cls.up_sock.close()

    def test_midstream_error_yields_clean_sse_not_spliced_json(self):
        body = b'{"model":"claude-opus-4-8","max_tokens":10,"stream":true,' \
               b'"messages":[{"role":"user","content":"hi"}]}'
        raw = raw_post("127.0.0.1", self.port, f"/{SEC}/v1/messages", body)
        head, _, tail = raw.partition(b"\r\n\r\n")
        self.assertIn(b"200", head.split(b"\r\n")[0])          # 头已发 200
        self.assertIn(b"event: error", tail)                   # 干净的 SSE 错误帧
        self.assertTrue(raw.rstrip().endswith(b"0"))           # chunked 终止块
        self.assertNotIn(b"HTTP/1.1 502", tail)                # 未把 502 拼进流


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd /Users/superjj/ccproj/CSswitch && python3 -m unittest test.test_proxy_stream -v`
Expected: FAIL（当前断流会走 `_send_json(502)`，body 里出现 `HTTP/1.1 502`、无 `event: error`）。

- [ ] **Step 3: 新增 `_sse_error_and_terminate`（放在 `_sse` 之后）**

```python
    def _sse_error_and_terminate(self, msg):
        frame = ("event: error\ndata: " + json.dumps(
            {"type": "error", "error": {"type": "api_error", "message": msg}},
            ensure_ascii=False) + "\n\n").encode()
        self.wfile.write(hex(len(frame))[2:].encode() + b"\r\n" + frame + b"\r\n")
        self.wfile.write(b"0\r\n\r\n")
```

- [ ] **Step 4: 重写 `_handle_anthropic` 的 try 块，加 `headers_sent` 守卫**

把现有 `try:` 到末尾 `except Exception` 段整体替换为：

```python
        headers_sent = False
        try:
            if stream:
                r, first, ct = open_stream(PROV["url"], data, headers)
                with r:
                    self.send_response(200)
                    self.send_header("Content-Type", ct)
                    self.send_header("Cache-Control", "no-cache")
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    headers_sent = True
                    self.wfile.write(hex(len(first))[2:].encode() + b"\r\n" + first + b"\r\n")
                    while True:
                        try:
                            chunk = r.read(4096)
                        except Exception as e:
                            log(f"  !! 流中断（头已发），SSE error 收尾: {e}")
                            self._sse_error_and_terminate(str(e))
                            return
                        if not chunk:
                            break
                        self.wfile.write(hex(len(chunk))[2:].encode() + b"\r\n" + chunk + b"\r\n")
                    self.wfile.write(b"0\r\n\r\n")
                log(f"  <- {PROV_NAME} 流式透传 OK")
            else:
                body_bytes, ct = http_post(PROV["url"], data, headers)
                self.send_response(200)
                self.send_header("Content-Type", ct)
                self.send_header("Content-Length", str(len(body_bytes)))
                self.end_headers()
                headers_sent = True
                self.wfile.write(body_bytes)
                log(f"  <- {PROV_NAME} 非流式透传 OK")
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", "replace")[:400]
            log(f"  !! 上游 HTTP {e.code}: {detail}")
            if not headers_sent:
                self._send_json(502, {"type": "error", "error": {
                    "type": "api_error", "message": f"upstream {e.code}: {detail}"}})
        except Exception as e:
            log(f"  !! 代理异常: {e}")
            if headers_sent:
                try:
                    self._sse_error_and_terminate(str(e))
                except Exception:
                    pass
            else:
                self._send_json(502, {"type": "error", "error": {
                    "type": "api_error", "message": str(e)}})
```

- [ ] **Step 5: 运行，确认通过**

Run: `cd /Users/superjj/ccproj/CSswitch && python3 -m unittest test.test_proxy_stream -v`
Expected: PASS。

- [ ] **Step 6: 跑一遍全部 Python 测试，确认无回归**

Run: `cd /Users/superjj/ccproj/CSswitch && python3 -m unittest discover -s test -p 'test_*.py' -v 2>&1 | tail -4`
Expected: `OK`，Ran 数 ≥ 16。

- [ ] **Step 7: Commit**

```bash
cd /Users/superjj/ccproj/CSswitch
git add proxy/csswitch_proxy.py test/test_proxy_stream.py
git commit -m "fix(proxy): clean SSE error on mid-stream drop, no spliced 502 (7.3)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 5: [7.1] 伪造器抗符号链接（目录 + 叶子文件 + 原子写）

**Files:**
- Modify: `scripts/make-virtual-oauth.mjs`
- Create: `test/test_make_virtual_oauth.mjs`

**Interfaces:**
- Produces: 伪造器对下列情形一律非零退出且零改动目标：(a) authDir 是符号链接、其 realpath 不在 `.sandbox/` 下；(b) 任一写入目标（encryption.key/active-org.json/.enc）是符号链接。正常沙箱目录写出的都是普通文件、mode 0600。

- [ ] **Step 1: 写失败测试 `test/test_make_virtual_oauth.mjs`**

```javascript
import { test } from "node:test";
import assert from "node:assert";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const SCRIPT = path.join(import.meta.dirname, "..", "scripts", "make-virtual-oauth.mjs");

function mktmp() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "csswitch-oauth-"));
}
function run(authDir, extra = []) {
  return execFileSync("node", [SCRIPT, "--auth-dir", authDir, ...extra],
    { stdio: "pipe" });
}

test("auth-dir symlink whose realpath is outside .sandbox is rejected", () => {
  const t = mktmp();
  const outside = path.join(t, "outside");
  fs.mkdirSync(outside, { recursive: true });
  const sbParent = path.join(t, ".sandbox");
  fs.mkdirSync(sbParent, { recursive: true });
  const link = path.join(sbParent, "auth");
  fs.symlinkSync(outside, link); // .sandbox/auth -> outside
  assert.throws(() => run(link));
  assert.deepEqual(fs.readdirSync(outside), []); // target untouched
});

test("leaf encryption.key symlink is refused, target untouched", () => {
  const t = mktmp();
  const auth = path.join(t, ".sandbox", "auth");
  fs.mkdirSync(auth, { recursive: true });
  const secret = path.join(t, "secret-target");
  fs.writeFileSync(secret, "ORIGINAL");
  fs.symlinkSync(secret, path.join(auth, "encryption.key"));
  assert.throws(() => run(auth));
  assert.equal(fs.readFileSync(secret, "utf-8"), "ORIGINAL");
});

test("normal sandbox dir writes regular 0600 files", () => {
  const t = mktmp();
  const auth = path.join(t, ".sandbox", "auth");
  fs.mkdirSync(auth, { recursive: true });
  run(auth);
  for (const f of ["encryption.key", "active-org.json"]) {
    const st = fs.lstatSync(path.join(auth, f));
    assert.ok(!st.isSymbolicLink());
    assert.equal(st.mode & 0o777, 0o600);
  }
  const enc = fs.readdirSync(path.join(auth, ".oauth-tokens")).filter((x) => x.endsWith(".enc"));
  assert.equal(enc.length, 1);
});
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd /Users/superjj/ccproj/CSswitch && node --test test/test_make_virtual_oauth.mjs`
Expected: FAIL（叶子链接用例：当前 writeFileSync 跟随链接覆盖了 secret-target，断言 ORIGINAL 失败）。

- [ ] **Step 3: 在 `make-virtual-oauth.mjs` 顶部（import 之后）加两个助手**

```javascript
function realAncestor(p) {
  // 逐层向上找到最近的已存在祖先并 realpath，再把不存在的尾巴拼回，看穿符号链接
  let cur = path.resolve(p);
  const tail = [];
  while (!fs.existsSync(cur)) {
    tail.unshift(path.basename(cur));
    const parent = path.dirname(cur);
    if (parent === cur) break;
    cur = parent;
  }
  const base = fs.existsSync(cur) ? fs.realpathSync(cur) : cur;
  return tail.length ? path.join(base, ...tail) : base;
}
function assertNotSymlink(p) {
  try {
    if (fs.lstatSync(p).isSymbolicLink()) {
      console.error(`拒绝：${p} 是符号链接，绝不跟随写入。`);
      process.exit(3);
    }
  } catch (e) {
    if (e.code !== "ENOENT") throw e; // 不存在则允许（稍后新建）
  }
}
function safeWrite(filePath, data, mode) {
  assertNotSymlink(filePath);
  const tmp = path.join(path.dirname(filePath), `.tmp-${crypto.randomBytes(6).toString("hex")}`);
  const fd = fs.openSync(tmp, "wx", mode); // O_CREAT|O_EXCL
  try {
    fs.writeSync(fd, data);
  } finally {
    fs.closeSync(fd);
  }
  fs.renameSync(tmp, filePath);
  fs.chmodSync(filePath, mode);
}
```

- [ ] **Step 4: 用 realpath 收紧目录护栏**

把 `const resolvedAuth = path.resolve(authDir);` 改为：

```javascript
const resolvedAuth = realAncestor(authDir);
```

（后面的 `=== realDir`、`.sandbox/` 正则、`localhost.invalid` 校验保持不变，此时它们看到的是穿透链接后的真实路径。）

- [ ] **Step 5: 三处写入改走 `safeWrite`，删 .enc 前 lstat 拒链接**

- `fs.writeFileSync(keyFile, keyBlob, { mode: 0o600 });` → `safeWrite(keyFile, keyBlob, 0o600);`
- `.enc` 写入 `fs.writeFileSync(path.join(tokDir, `${userId}.enc`), encFileBody, { mode: 0o600 });` → `safeWrite(path.join(tokDir, `${userId}.enc`), encFileBody, 0o600);`
- active-org.json 的 `fs.writeFileSync(path.join(resolvedAuth, "active-org.json"), ... , { mode: 0o600 });` → 用 `safeWrite(path.join(resolvedAuth, "active-org.json"), JSON.stringify({ org_uuid: orgUuid }, null, 2) + "\n", 0o600);`
- 清理旧 .enc 的循环里，`fs.unlinkSync` 之前加 `assertNotSymlink(path.join(tokDir, f));`

- [ ] **Step 6: 运行，确认通过**

Run: `cd /Users/superjj/ccproj/CSswitch && node --test test/test_make_virtual_oauth.mjs`
Expected: PASS（3 个用例全绿）。

- [ ] **Step 7: 回归自校验仍在（解密 roundtrip）**

Run: `cd /Users/superjj/ccproj/CSswitch && D=$(mktemp -d)/.sandbox/auth && mkdir -p "$D" && node scripts/make-virtual-oauth.mjs --auth-dir "$D" | grep -c '"selfcheck"'`
Expected: `1`（脚本内建解密自校验通过、正常输出）。

- [ ] **Step 8: Commit**

```bash
cd /Users/superjj/ccproj/CSswitch
git add scripts/make-virtual-oauth.mjs test/test_make_virtual_oauth.mjs
git commit -m "fix(oauth): reject symlink write targets, atomic writes, realpath guard (7.1)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 6: [7.6] 停止脚本按退出码如实报告

**Files:**
- Modify: `scripts/stop-science-sandbox.sh`
- Create: `test/test_scripts.sh`（本 Task 建文件，Task 7 追加）

**Interfaces:**
- Produces: `stop-science-sandbox.sh` 支持 `SCIENCE_BIN` 与 `SANDBOX_HOME` 覆盖；stop 命令退出码非零时脚本非零退出且不打印「沙箱已停」。

- [ ] **Step 1: 写失败测试 `test/test_scripts.sh`**

```bash
#!/usr/bin/env bash
set -u
FAILS=0
ok() { echo "ok - $1"; }
no() { echo "NOT ok - $1"; FAILS=$((FAILS+1)); }
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# --- 7.6 停止脚本如实报告 ---
T="$(mktemp -d)"
mkdir -p "$T/home/.claude-science"           # DATA_DIR 存在，走到 stop 调用
FAKE_FAIL="$T/fake-fail"; printf '#!/bin/sh\nexit 1\n' > "$FAKE_FAIL"; chmod +x "$FAKE_FAIL"
FAKE_OK="$T/fake-ok";     printf '#!/bin/sh\nexit 0\n' > "$FAKE_OK";   chmod +x "$FAKE_OK"

out="$(SANDBOX_HOME="$T/home" SCIENCE_BIN="$FAKE_FAIL" "$ROOT/scripts/stop-science-sandbox.sh" 2>&1)"; rc=$?
if [ $rc -ne 0 ]; then ok "stop reports failure rc!=0"; else no "stop hid failure (rc=$rc)"; fi
if echo "$out" | grep -q "沙箱已停"; then no "stop falsely claimed success"; else ok "stop did not falsely claim success"; fi

out="$(SANDBOX_HOME="$T/home" SCIENCE_BIN="$FAKE_OK" "$ROOT/scripts/stop-science-sandbox.sh" 2>&1)"; rc=$?
if [ $rc -eq 0 ] && echo "$out" | grep -q "沙箱已停"; then ok "stop reports success on rc=0"; else no "stop mis-reported success path (rc=$rc)"; fi

echo "----"
if [ $FAILS -eq 0 ]; then echo "ALL PASS"; exit 0; else echo "$FAILS FAILED"; exit 1; fi
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd /Users/superjj/ccproj/CSswitch && bash test/test_scripts.sh`
Expected: FAIL（当前 `|| true` 吞错、恒打印「沙箱已停」，且不认 `SANDBOX_HOME`/`SCIENCE_BIN`）。

- [ ] **Step 3: 改 `scripts/stop-science-sandbox.sh`**

替换正文（保留 shebang 与注释）为：

```bash
set -euo pipefail
PROJ="${0:A:h:h}"
SANDBOX_HOME="${SANDBOX_HOME:-$PROJ/.sandbox/home}"
DATA_DIR="$SANDBOX_HOME/.claude-science"
BIN="${SCIENCE_BIN:-/Applications/Claude Science.app/Contents/Resources/bin/claude-science}"

if [[ ! -d "$DATA_DIR" ]]; then echo "沙箱不存在，无需停止。"; exit 0; fi

if HOME="$SANDBOX_HOME" "$BIN" stop --data-dir "$DATA_DIR" 2>&1 | tail -2; then
  echo "沙箱已停。真实实例 8765 未受影响。"
else
  rc=${pipestatus[1]:-$?}
  echo "停止失败（退出码 $rc）。真实实例 8765 未受影响。" >&2
  exit "$rc"
fi
```

说明：zsh 下 `${pipestatus[1]}` 取管道首命令（stop）的退出码，避免被 `tail` 的退出码掩盖。

- [ ] **Step 4: 运行，确认通过**

Run: `cd /Users/superjj/ccproj/CSswitch && bash test/test_scripts.sh`
Expected: 三条 7.6 断言 `ok`，`ALL PASS`。

注意：脚本是 zsh（`${0:A:h:h}`、`pipestatus`），测试用 `zsh` 执行脚本本体；`test_scripts.sh` 以 bash 跑但通过子进程调用脚本，脚本自带 `#!/bin/zsh`。

- [ ] **Step 5: Commit**

```bash
cd /Users/superjj/ccproj/CSswitch
git add scripts/stop-science-sandbox.sh test/test_scripts.sh
git commit -m "fix(scripts): stop-sandbox reports real exit code, no false success (7.6)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 7: [7.7] 启动脚本端口整数归一化 + data-dir realpath + dry-run

**Files:**
- Modify: `scripts/launch-virtual-sandbox.sh`
- Modify: `scripts/launch-science-sandbox.sh`
- Modify: `test/test_scripts.sh`（追加 7.7 断言）

**Interfaces:**
- Produces: 两个启动脚本支持 `--dry-run`（跑完全部护栏后、真正启动前退出）；端口 `08765` 之类按十进制整数归一后等于 8765 即拒绝；data-dir 的 realpath 等于真实目录即拒绝。

- [ ] **Step 1: 往 `test/test_scripts.sh` 的 `echo "----"` 之前追加 7.7 断言**

```bash
# 7.7 端口归一化 + dry-run
out="$(SANDBOX_HOME="$T/vh" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 08765 --dry-run 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "拒绝"; then ok "08765 rejected via int-normalize"; else no "08765 bypassed guard (rc=$rc)"; fi

out="$(SANDBOX_HOME="$T/vh" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9931 --dry-run 2>&1)"; rc=$?
if [ $rc -eq 0 ] && echo "$out" | grep -q "DRY-RUN OK"; then ok "valid port passes guards in dry-run"; else no "valid port dry-run failed (rc=$rc): $out"; fi
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd /Users/superjj/ccproj/CSswitch && bash test/test_scripts.sh`
Expected: FAIL（当前只拒精确字符串 `8765`，`08765` 放行；且无 `--dry-run`，脚本会尝试真正启动）。

- [ ] **Step 3: 改 `scripts/launch-virtual-sandbox.sh`**

在参数解析 `while` 循环里加一条分支：

```bash
    --dry-run) DRY_RUN=1; shift;;
```

在循环前给 `DRY_RUN` 默认值（与其它默认变量同处）：

```bash
DRY_RUN=0
```

把端口护栏 `if [[ "$PORT" == "8765" ]]; then ...` 改为整数归一化，并把 data-dir 护栏改 realpath：

```bash
if (( 10#${PORT} == 8765 )); then echo "拒绝：端口 8765 是真实实例保留端口"; exit 1; fi
_dd_real="$(realpath -m -- "$DATA_DIR")"; _real_real="$(realpath -m -- "$REAL_DIR")"
if [[ "$_dd_real" == "$_real_real" ]]; then echo "拒绝：data-dir 的真实路径指向真实目录"; exit 1; fi
```

紧接在上面的端口 / data-dir 护栏之后、且在首次资产克隆块（`cp -Rc "$REAL_DIR/..."`）之前加 dry-run 出口。这样 dry-run 只校验护栏，绝不去读真实 Science 目录（铁律：真实目录只读也要谨慎）：

```bash
if [[ "$DRY_RUN" == "1" ]]; then echo "DRY-RUN OK：护栏通过，未启动沙箱。"; exit 0; fi
```

同时把脚本顶部 `DATA_DIR` 派生改为可被 `SANDBOX_HOME` 覆盖（与 stop 脚本一致）：把 `SANDBOX_HOME="$PROJ/.sandbox/home"` 改为 `SANDBOX_HOME="${SANDBOX_HOME:-$PROJ/.sandbox/home}"`。

- [ ] **Step 4: 给 `scripts/launch-science-sandbox.sh` 打同样的端口整数归一化补丁**

找到该脚本里对端口 `8765` 的字符串比较（同病），替换为：

```bash
if (( 10#${PORT} == 8765 )); then echo "拒绝：端口 8765 是真实实例保留端口"; exit 1; fi
```

（此脚本不要求加 dry-run，测试只覆盖虚拟沙箱脚本；此步仅消除相同的字符串绕过隐患。）

- [ ] **Step 5: 运行，确认通过**

Run: `cd /Users/superjj/ccproj/CSswitch && bash test/test_scripts.sh`
Expected: 7.6 与 7.7 全部 `ok`，`ALL PASS`。

- [ ] **Step 6: Commit**

```bash
cd /Users/superjj/ccproj/CSswitch
git add scripts/launch-virtual-sandbox.sh scripts/launch-science-sandbox.sh test/test_scripts.sh
git commit -m "fix(scripts): integer port guard + realpath data-dir + dry-run (7.7)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 8: Phase 0 收口，全套测试一键跑

**Files:**
- Create: `test/run_all.sh`（聚合三套测试，供 Phase 1/CI 复用）

**Interfaces:**
- Produces: `test/run_all.sh` 顺序跑 Python unittest、node --test、bash 脚本测试，任一失败即非零退出。

- [ ] **Step 1: 写 `test/run_all.sh`**

```bash
#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
echo "== python unittest =="
python3 -m unittest discover -s test -p 'test_*.py' -v
echo "== node --test =="
node --test test/test_make_virtual_oauth.mjs
echo "== bash scripts =="
bash test/test_scripts.sh
echo "ALL GREEN"
```

- [ ] **Step 2: 跑全套，确认全绿**

Run: `cd /Users/superjj/ccproj/CSswitch && bash test/run_all.sh 2>&1 | tail -8`
Expected: 末尾 `ALL GREEN`；Python `OK`、node 全 pass、bash `ALL PASS`。

- [ ] **Step 3: Commit**

```bash
cd /Users/superjj/ccproj/CSswitch
git add test/run_all.sh
git commit -m "test: aggregate runner for phase 0 hardening suite

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Self-Review（写完后自查）

**1. Spec 覆盖（§7 逐项）：**
- 7.1 → Task 5（realpath 目录 + 叶子 lstat + 原子写 + 三用例）✓
- 7.2 → Task 3（path secret + 威胁模型内注释 + secret 不落日志 + 4 用例）✓
- 7.3 → Task 4（headers_sent 守卫 + SSE error 收尾 + raw socket 断言）✓
- 7.4 → Task 1（tool_choice 分档 + stop/top_p）✓
- 7.5 → Task 2（按模型查表）✓
- 7.6 → Task 6（退出码如实报告）✓
- 7.7 → Task 7（整数归一 + realpath + dry-run；两个脚本都改）✓
- 7.8 → 贯穿：删旧 test、命名 `test_*.py` 修好发现、Task 8 聚合 runner ✓
- §12.3 provisional 值（deepseek/qwen caps）已在 Task 2 注释标注待核对 ✓

**2. 占位符扫描：** 无 TBD/TODO；每个改动步都给了完整代码与预期输出。cap 数值为具体 provisional 常量并注明核对来源（§12.3），非占位符。

**3. 类型/命名一致性：** `map_tool_choice`、`clamp_max_tokens(v, model)`、`_auth_ok`、`_sse_error_and_terminate`、`safeWrite`/`assertNotSymlink`/`realAncestor`、`start_mock`/`hits`/`stop` 在定义与被引用处名称一致。`AUTH_SECRET`/`PROV_NAME`/`PROV` 均为模块级全局，测试与 handler 引用一致。

**4. 范围：** 全部 Python/Node/shell 加固，无 Tauri，Phase 0 自成可测交付。可测性开关都有生产默认值，对真实运行零影响。

**遗留到实现阶段实测（§12.3，不阻塞本 Phase）：** DeepSeek 官方模型 id 与真实上限核对、DashScope `required` 支持、path secret 与 Science base_url 拼接兼容性。这三项在 Phase 1 整链联调时验证。
