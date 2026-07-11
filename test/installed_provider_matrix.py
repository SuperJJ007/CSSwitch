#!/usr/bin/env python3
"""Safety-first controller for installed/local-mock provider acceptance.

The controller prepares an isolated per-case HOME, starts the strict provider
scenario API in-process, and emits *plans* and redacted observations for the
human/GUI driver.  It deliberately does not launch, quit, or broadly signal the
installed application.  The root driver owns GUI actions and executes the
returned LaunchServices plan after reviewing it.

No command in this module reads process argv.  Process inspection is limited to
PID/PPID/comm, executable identity, and explicitly selected loopback listeners.
Secrets stay in the 0600 config file or process memory and are never returned by
the JSON controller.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import http.client
import json
import os
import plistlib
import re
import secrets as secrets_module
import shlex
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple

try:
    from _loopback_ports import FORBIDDEN_PORTS, LoopbackPortReservation
    from provider_mock_scenarios import (
        ACTION_TYPES,
        SCHEMA as MOCK_MANIFEST_SCHEMA,
        SCHEMA_VERSION as MOCK_MANIFEST_VERSION,
        Scenario,
        ScenarioStep,
        load_manifest,
        scenario_from_steps,
        start_scenario,
    )
except ImportError:  # ``python -m unittest test.test_...``
    from test._loopback_ports import FORBIDDEN_PORTS, LoopbackPortReservation
    from test.provider_mock_scenarios import (
        ACTION_TYPES,
        SCHEMA as MOCK_MANIFEST_SCHEMA,
        SCHEMA_VERSION as MOCK_MANIFEST_VERSION,
        Scenario,
        ScenarioStep,
        load_manifest,
        scenario_from_steps,
        start_scenario,
    )


APP_BUNDLE = Path("/Applications/CSSwitch.app")
EXPECTED_BUNDLE_ID = "com.csswitch.menubar"
EXPECTED_EXECUTABLE = "desktop"
GATEWAY_EXECUTABLE = "csswitch-gateway"
FIXED_PATH_SECRET = "6f2b0bbd37f98f6f9f8d9e3c8f7a2b10"
FAKE_API_KEY = "csswitch-installed-fake-key-never-use"
CONTROLLER_SCHEMA = "csswitch.installed-provider-controller.v1"
SAFE_LABEL = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.:-]{0,95}$")
MAX_CONTROL_LINE = 65_536


class ControllerError(RuntimeError):
    """A fail-closed installed acceptance error."""


@dataclass(frozen=True)
class CaseDefinition:
    case_id: str
    template_id: str
    profile_name: str
    category: str
    api_format: str
    adapter: str
    shim: str
    model: str
    base_kind: str
    base_prefix: str
    message_path: str
    models_path: Optional[str]
    mock_scenario: str
    formal_step_id: str
    formal_variant: str
    expected_upstream_model: Optional[str] = None
    blockers: Tuple[str, ...] = ()


CASE_DEFINITIONS: Dict[str, CaseDefinition] = {
    "deepseek-off": CaseDefinition(
        "deepseek-off", "deepseek", "Installed DeepSeek off", "cn_official",
        "anthropic", "deepseek", "off", "", "native",
        "", "/deepseek/v1/messages", None,
        "installed_deepseek_matrix", "deepseek-formal-off", "basic",
    ),
    "deepseek-detect": CaseDefinition(
        "deepseek-detect", "deepseek", "Installed DeepSeek detect", "cn_official",
        "anthropic", "deepseek", "detect", "", "native",
        "", "/deepseek/v1/messages", None,
        "installed_deepseek_matrix", "deepseek-formal-detect", "tools",
    ),
    "deepseek-rewrite": CaseDefinition(
        "deepseek-rewrite", "deepseek", "Installed DeepSeek rewrite", "cn_official",
        "anthropic", "deepseek", "rewrite", "", "native",
        "", "/deepseek/v1/messages", None,
        "installed_deepseek_matrix", "deepseek-formal-rewrite-stream", "stream-tools",
    ),
    "qwen-chat": CaseDefinition(
        "qwen-chat", "qwen", "Installed Qwen Chat", "cn_official",
        "openai_chat", "qwen", "off", "qwen-plus-latest", "native",
        "", "/qwen/v1/chat/completions", None,
        "installed_qwen_matrix", "qwen-formal-chat", "basic", "qwen-plus-latest",
    ),
    "qwen-tools": CaseDefinition(
        "qwen-tools", "qwen", "Installed Qwen tools", "cn_official",
        "openai_chat", "qwen", "off", "qwen-plus-latest", "native",
        "", "/qwen/v1/chat/completions", None,
        "installed_qwen_matrix", "qwen-formal-tools-results", "tools", "qwen-plus-latest",
    ),
    "qwen-stream": CaseDefinition(
        "qwen-stream", "qwen", "Installed Qwen stream", "cn_official",
        "openai_chat", "qwen", "off", "qwen-plus-latest", "native",
        "", "/qwen/v1/chat/completions", None,
        "installed_qwen_matrix", "qwen-formal-stream", "stream", "qwen-plus-latest",
    ),
    "custom-chat": CaseDefinition(
        "custom-chat", "custom-openai", "Installed custom OpenAI Chat", "custom",
        "openai_chat", "openai-custom", "off", "glm-4.5", "loopback",
        "/openai/v1", "/openai/v1/chat/completions", "/openai/v1/models",
        "installed_openai_chat_matrix", "openai-chat-formal", "tools", "glm-4.5",
    ),
    "responses": CaseDefinition(
        "responses", "custom-openai-responses", "Installed OpenAI Responses", "custom",
        "openai_responses", "openai-responses", "off", "gpt-5.2", "loopback",
        "/responses/v1", "/responses/v1/responses", "/responses/v1/models",
        "installed_openai_responses_matrix", "responses-formal-tools-results", "tools-results", "gpt-5.2",
    ),
    "relay-force": CaseDefinition(
        "relay-force", "custom", "Installed relay force", "custom",
        "anthropic", "relay", "off", "MiniMax-M2", "loopback",
        "/relay", "/relay/v1/messages", "/relay/v1/models",
        "installed_relay_matrix", "relay-formal-force-schema", "force-tools", "MiniMax-M2",
    ),
    "kimi": CaseDefinition(
        "kimi", "kimi", "Installed Kimi", "cn_official",
        "anthropic", "relay", "off", "kimi-k2.7-code", "loopback",
        "/relay", "/relay/v1/messages", "/relay/v1/models",
        "installed_relay_matrix", "relay-formal-kimi-thinking-filter", "stream-tools", "kimi-k2.7-code",
    ),
    "siliconflow": CaseDefinition(
        "siliconflow", "siliconflow", "Installed SiliconFlow", "cn_official",
        "anthropic", "relay", "off", "deepseek-ai/DeepSeek-V4-Pro", "proxy",
        "http://api.siliconflow.cn", "/v1/messages", "/v1/models",
        "installed_relay_matrix", "siliconflow-exact-host", "tools",
        "deepseek-ai/DeepSeek-V4-Pro",
    ),
}


def _require_safe_label(value: str, field: str = "label") -> str:
    if not isinstance(value, str) or not SAFE_LABEL.fullmatch(value):
        raise ControllerError(f"unsafe {field}")
    return value


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def _reject_symlink(path: Path) -> None:
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError:
        return
    if stat.S_ISLNK(mode):
        raise ControllerError(f"refusing symlink: {path}")


def _reject_existing_symlink_components(path: Path) -> None:
    """Reject every existing component of an absolute identity path."""

    path = Path(path)
    if not path.is_absolute():
        raise ControllerError("identity path must be absolute")
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current /= part
        try:
            info = current.lstat()
        except FileNotFoundError:
            raise ControllerError(f"identity path component is missing: {current}") from None
        if stat.S_ISLNK(info.st_mode):
            raise ControllerError(f"identity path traverses a symlink: {current}")


def _ensure_private_dir(path: Path) -> None:
    _reject_symlink(path)
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    _reject_symlink(path)
    if not path.is_dir():
        raise ControllerError(f"not a directory: {path}")
    path.chmod(0o700)


def _safe_write(path: Path, data: bytes, mode: int = 0o600) -> None:
    _reject_symlink(path)
    _ensure_private_dir(path.parent)
    tmp = path.parent / f".{path.name}.tmp-{os.getpid()}-{time.monotonic_ns()}"
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    flags |= getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(tmp, flags, mode)
    try:
        view = memoryview(data)
        while view:
            written = os.write(fd, view)
            view = view[written:]
        os.fsync(fd)
    finally:
        os.close(fd)
    os.replace(tmp, path)
    path.chmod(mode)


def _safe_json_write(path: Path, value: Mapping[str, Any]) -> None:
    encoded = (json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode()
    _safe_write(path, encoded, 0o600)


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _json_pointer_escape(part: str) -> str:
    return part.replace("~", "~0").replace("/", "~1")


def _changed_json_paths(before: Any, after: Any, prefix: str = "") -> List[str]:
    if type(before) is not type(after):
        return [prefix or "/"]
    if isinstance(before, dict):
        paths: List[str] = []
        for key in sorted(set(before) | set(after)):
            child = f"{prefix}/{_json_pointer_escape(str(key))}"
            if key not in before or key not in after:
                paths.append(child)
            else:
                paths.extend(_changed_json_paths(before[key], after[key], child))
        return paths
    if isinstance(before, list):
        paths = []
        for index in range(max(len(before), len(after))):
            child = f"{prefix}/{index}"
            if index >= len(before) or index >= len(after):
                paths.append(child)
            else:
                paths.extend(_changed_json_paths(before[index], after[index], child))
        return paths
    return [] if before == after else [prefix or "/"]


def _clone_step(
    step: ScenarioStep,
    *,
    step_id: str,
    phase: str,
    path: Optional[str] = None,
    expected_model: Optional[str] = None,
) -> Dict[str, Any]:
    checks = copy.deepcopy(step.checks)
    if expected_model:
        body = checks.setdefault("body", {})
        body.setdefault("equals", {})["/model"] = expected_model
    action = copy.deepcopy(step.action)
    if expected_model and action.get("type") in {
        "anthropic_json",
        "anthropic_sse",
        "dsml",
        "openai_chat_text_tool",
        "openai_responses_text_tool",
    }:
        action["model"] = expected_model
    return {
        "id": _require_safe_label(step_id, "step id"),
        "phase": _require_safe_label(phase, "phase"),
        "method": step.method,
        "path": path if path is not None else step.path,
        "action": action,
        "checks": checks,
    }


def build_case_scenario(case: CaseDefinition) -> Scenario:
    """Select one case from the frozen installed-family scenario fixtures."""

    catalog = load_manifest()
    source = catalog[case.mock_scenario]
    selected = [step for step in source.steps if step.phase != "formal"]
    if case.case_id == "siliconflow":
        formal = catalog["relay_siliconflow_proxy_positive"].steps[0]
    else:
        formal = next(
            (step for step in source.steps if step.step_id == case.formal_step_id),
            None,
        )
        if formal is None:
            raise ControllerError("installed mock formal step is missing")
    insert_at = next(
        (index for index, step in enumerate(selected) if step.phase == "reuse"),
        len(selected),
    )
    selected.insert(insert_at, formal)

    expected_model = case.expected_upstream_model or case.model or None
    steps: List[Dict[str, Any]] = []
    phase_ordinals: Dict[str, int] = {}
    for template in selected:
        phase_ordinals[template.phase] = phase_ordinals.get(template.phase, 0) + 1
        suffix = (
            template.phase
            if phase_ordinals[template.phase] == 1
            else f"{template.phase}-{phase_ordinals[template.phase]}"
        )
        path = template.path
        if case.base_kind == "proxy":
            path = (
                "http://api.siliconflow.cn/v1/models"
                if template.phase == "discovery"
                else "http://api.siliconflow.cn/v1/messages"
            )
        step_expected_model = expected_model
        if case.adapter == "qwen":
            step_expected_model = "qwen3.7-max"
        item = _clone_step(
            template,
            step_id=f"{case.case_id}-{suffix}",
            phase=template.phase,
            path=path,
            expected_model=(None if template.phase == "discovery" else step_expected_model),
        )
        if case.case_id == "kimi" and template.phase in {"scratch", "formal"}:
            equals = item["checks"].setdefault("body", {}).setdefault("equals", {})
            equals["/thinking/type"] = "enabled"
            equals["/thinking/budget_tokens"] = 1 if "scratch" in template.step_id else 1024
        if case.case_id == "kimi" and template.phase == "formal":
            body = item["checks"].setdefault("body", {})
            body.setdefault("required", []).extend(
                pointer
                for pointer in ("/tools/0/name", "/tools/1/name")
                if pointer not in body.setdefault("required", [])
            )
            body.setdefault("absent", []).extend(
                pointer
                for pointer in ("/tools/2", "/tool_choice")
                if pointer not in body.setdefault("absent", [])
            )
            body.setdefault("equals", {})["/stream"] = True
            item["action"] = {"type": "kimi_sse"}
        steps.append(item)
    return scenario_from_steps(
        f"installed-{case.case_id}",
        steps,
        description=f"Installed provider matrix for {case.case_id}",
        phases=("discovery", "scratch", "formal", "reuse", "restart"),
    )


class InProcessScenarioControl:
    """Unit-test-only facade matching the external control contract."""

    def __init__(self, scenario: Scenario):
        self._scenario = scenario
        self._mock = None

    def start(self) -> Dict[str, Any]:
        if self._mock is not None:
            raise ControllerError("mock already started")
        self._mock = start_scenario(
            self._scenario,
            secrets={"provider_key": FAKE_API_KEY},
        )
        return self._mock.ready()

    @property
    def expected_executable(self) -> Path:
        return Path(sys.executable).resolve(strict=True)

    @property
    def base_url(self) -> str:
        if self._mock is None:
            raise ControllerError("mock is not started")
        return self._mock.base_url

    @property
    def port(self) -> int:
        if self._mock is None:
            raise ControllerError("mock is not started")
        return self._mock.port

    def enter_phase(self, phase: str) -> None:
        if self._mock is None:
            raise ControllerError("mock is not started")
        self._mock.enter_phase(phase)

    def status(self) -> Dict[str, Any]:
        if self._mock is None:
            raise ControllerError("mock is not started")
        return self._mock.result()

    def wait(self, timeout_seconds: float) -> bool:
        if self._mock is None:
            raise ControllerError("mock is not started")
        return self._mock.wait_complete(timeout_seconds)

    def stop(self) -> Dict[str, Any]:
        if self._mock is None:
            return {
                "schema": "csswitch.provider-mock-result.v1",
                "stopped": True,
                "complete": False,
                "ok": False,
                "requests": [],
                "failures": [],
            }
        return self._mock.stop()


def _scenario_manifest_value(scenario: Scenario) -> Dict[str, Any]:
    return {
        "schema": MOCK_MANIFEST_SCHEMA,
        "version": MOCK_MANIFEST_VERSION,
        "actions": sorted(ACTION_TYPES),
        "scenarios": {
            scenario.name: {
                "description": scenario.description,
                "phases": list(scenario.phases),
                "steps": [
                    {
                        "id": step.step_id,
                        "phase": step.phase,
                        "method": step.method,
                        "path": step.path,
                        "action": copy.deepcopy(step.action),
                        "checks": copy.deepcopy(step.checks),
                    }
                    for step in scenario.steps
                ],
            }
        },
    }


class SubprocessScenarioControl:
    """Owned CLI mock using anonymous secret FDs and authenticated Unix control."""

    def __init__(self, scenario: Scenario, evidence_parent: Path):
        self._scenario = scenario
        self._parent = Path(evidence_parent)
        self.evidence_dir = self._parent / "provider-mock"
        self.manifest_path = self._parent / "provider-mock-manifest.v1.json"
        self._process: Optional[subprocess.Popen] = None
        self._client: Optional[UnixScenarioControlClient] = None
        self._token: Optional[str] = None
        self._ready: Optional[Dict[str, Any]] = None

    @property
    def expected_executable(self) -> Path:
        return Path(sys.executable).resolve(strict=True)

    @property
    def base_url(self) -> str:
        if self._client is None:
            raise ControllerError("mock is not started")
        return self._client.base_url

    @property
    def port(self) -> int:
        if self._client is None:
            raise ControllerError("mock is not started")
        return self._client.port

    @property
    def process_pid(self) -> Optional[int]:
        return self._process.pid if self._process is not None else None

    @property
    def process_alive(self) -> bool:
        return self._process is not None and self._process.poll() is None

    @staticmethod
    def _write_pipe(fd: int, payload: bytes) -> None:
        try:
            view = memoryview(payload)
            while view:
                written = os.write(fd, view)
                if written <= 0:
                    raise ControllerError("short anonymous mock input write")
                view = view[written:]
        finally:
            os.close(fd)

    def _read_ready(self, timeout_seconds: float = 8.0) -> Dict[str, Any]:
        ready_path = self.evidence_dir / "ready.json"
        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            if self._process is not None and self._process.poll() is not None:
                raise ControllerError(
                    f"provider mock exited before ready (exit {self._process.returncode})"
                )
            try:
                _reject_symlink(self.evidence_dir)
                directory_info = self.evidence_dir.stat()
                _reject_symlink(ready_path)
                ready_info = ready_path.stat()
                if (
                    directory_info.st_uid != os.getuid()
                    or stat.S_IMODE(directory_info.st_mode) != 0o700
                    or not stat.S_ISDIR(directory_info.st_mode)
                    or ready_info.st_uid != os.getuid()
                    or stat.S_IMODE(ready_info.st_mode) != 0o600
                    or not stat.S_ISREG(ready_info.st_mode)
                ):
                    raise ControllerError("provider mock ready evidence is not private")
                value = json.loads(ready_path.read_text(encoding="utf-8"))
                if not isinstance(value, dict):
                    raise ControllerError("provider mock ready evidence is invalid")
                return value
            except FileNotFoundError:
                time.sleep(0.02)
        raise ControllerError("provider mock did not publish ready evidence")

    def start(self) -> Dict[str, Any]:
        if self._process is not None:
            raise ControllerError("mock already started")
        _reject_symlink(self._parent)
        parent_info = self._parent.stat()
        if (
            not stat.S_ISDIR(parent_info.st_mode)
            or parent_info.st_uid != os.getuid()
            or stat.S_IMODE(parent_info.st_mode) != 0o700
        ):
            raise ControllerError("mock evidence parent must be owned 0700")
        if self.evidence_dir.exists() or self.evidence_dir.is_symlink():
            raise ControllerError("mock evidence directory must not pre-exist")
        _safe_json_write(self.manifest_path, _scenario_manifest_value(self._scenario))

        token_read, token_write = os.pipe()
        secrets_read, secrets_write = os.pipe()
        token = secrets_module.token_urlsafe(32)
        command = [
            str(self.expected_executable),
            str(Path(__file__).with_name("provider_mock_scenarios.py")),
            "--manifest",
            str(self.manifest_path),
            "--scenario",
            self._scenario.name,
            "--evidence-dir",
            str(self.evidence_dir),
            "--control-token-fd",
            str(token_read),
            "--secrets-fd",
            str(secrets_read),
        ]
        try:
            try:
                self._process = subprocess.Popen(
                    command,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    close_fds=True,
                    pass_fds=(token_read, secrets_read),
                )
            finally:
                os.close(token_read)
                os.close(secrets_read)
            self._write_pipe(token_write, token.encode("utf-8"))
            token_write = -1
            self._write_pipe(
                secrets_write,
                json.dumps({"provider_key": FAKE_API_KEY}, separators=(",", ":")).encode(),
            )
            secrets_write = -1
        except Exception:
            for fd in (token_write, secrets_write):
                if fd < 0:
                    continue
                try:
                    os.close(fd)
                except OSError:
                    pass
            if self._process is not None:
                self._terminate_owned_process()
            raise
        client: Optional[UnixScenarioControlClient] = None
        try:
            ready = self._read_ready()
            client = UnixScenarioControlClient(self.evidence_dir, ready, token)
            if (
                ready.get("schema") != "csswitch.provider-mock-ready.v1"
                or ready.get("scenario") != self._scenario.name
                or ready.get("owned_pid") != self._process.pid
                or ready.get("host") != "127.0.0.1"
                or ready.get("phase") is not None
            ):
                raise ControllerError("provider mock ready identity is invalid")
            self._token = token
            self._ready = copy.deepcopy(ready)
            self._client = client
            return self._client.start()
        except Exception:
            if client is not None:
                try:
                    client.stop()
                except Exception:
                    pass
            self._terminate_owned_process()
            self._token = None
            self._client = None
            self._ready = None
            raise

    def _terminate_owned_process(self) -> None:
        """Stop only the exact subprocess created by this controller instance."""

        process = self._process
        if process is None or process.poll() is not None:
            return
        process.terminate()
        try:
            process.wait(timeout=3.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3.0)

    def enter_phase(self, phase: str) -> None:
        if self._client is None:
            raise ControllerError("mock is not started")
        self._client.enter_phase(phase)

    def status(self) -> Dict[str, Any]:
        if self._client is None:
            raise ControllerError("mock is not started")
        return self._client.status()

    def wait(self, timeout_seconds: float) -> bool:
        if self._client is None:
            raise ControllerError("mock is not started")
        return self._client.wait(timeout_seconds)

    def stop(self) -> Dict[str, Any]:
        if self._client is None or self._process is None:
            raise ControllerError("mock is not started")
        result = self._client.stop()
        try:
            exit_code = self._process.wait(timeout=8.0)
        except subprocess.TimeoutExpired:
            self._terminate_owned_process()
            raise ControllerError("owned provider mock did not exit after control stop") from None
        self._token = None
        self._client.forget_token()
        safe = copy.deepcopy(result)
        safe["owned_process_exit_code"] = exit_code
        return safe


class UnixScenarioControlClient:
    """Client for the reviewed authenticated ``control.sock`` protocol.

    The caller owns mock process creation and supplies the token through an
    anonymous FD.  This client keeps that token in memory only; neither ready
    evidence nor controller responses contain it.
    """

    def __init__(
        self,
        evidence_dir: Path,
        ready: Mapping[str, Any],
        token: str,
        *,
        result_timeout: float = 8.0,
    ):
        self.evidence_dir = Path(evidence_dir)
        _reject_symlink(self.evidence_dir)
        info = self.evidence_dir.stat()
        if (
            not stat.S_ISDIR(info.st_mode)
            or info.st_uid != os.getuid()
            or stat.S_IMODE(info.st_mode) != 0o700
        ):
            raise ControllerError("mock evidence directory must be owned 0700")
        socket_name = ready.get("control_socket")
        if not isinstance(socket_name, str) or Path(socket_name).name != socket_name:
            raise ControllerError("mock ready evidence has unsafe control socket")
        if (
            not isinstance(token, str)
            or not (16 <= len(token) <= 1024)
            or any(ord(ch) < 33 for ch in token)
        ):
            raise ControllerError("invalid in-memory mock control token")
        self._ready = dict(ready)
        self._token = token
        self.socket_path = self.evidence_dir / socket_name
        if len(os.fsencode(self.socket_path)) >= 104:
            raise ControllerError("mock control socket path is too long for macOS")
        self.result_timeout = result_timeout

    @property
    def base_url(self) -> str:
        value = self._ready.get("base_url")
        if not isinstance(value, str) or not value.startswith("http://127.0.0.1:"):
            raise ControllerError("mock ready base URL is not loopback")
        return value

    @property
    def port(self) -> int:
        value = self._ready.get("port")
        if not isinstance(value, int) or value in FORBIDDEN_PORTS or not (1 <= value <= 65535):
            raise ControllerError("mock ready port is invalid")
        return value

    def start(self) -> Dict[str, Any]:
        self._validate_socket()
        return copy.deepcopy(self._ready)

    def _validate_socket(self) -> None:
        _reject_symlink(self.socket_path)
        info = os.lstat(self.socket_path)
        if (
            not stat.S_ISSOCK(info.st_mode)
            or info.st_uid != os.getuid()
            or stat.S_IMODE(info.st_mode) != 0o600
        ):
            raise ControllerError("mock control socket must be owned 0600")

    def _request(self, command: str, **fields: Any) -> Dict[str, Any]:
        self._validate_socket()
        request = {"token": self._token, "command": command, **fields}
        encoded = json.dumps(request, separators=(",", ":")).encode() + b"\n"
        if len(encoded) > MAX_CONTROL_LINE:
            raise ControllerError("mock control request is too large")
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(3.0)
        try:
            client.connect(str(self.socket_path))
            client.sendall(encoded)
            chunks = bytearray()
            while not chunks.endswith(b"\n"):
                chunk = client.recv(8192)
                if not chunk:
                    break
                chunks.extend(chunk)
                if len(chunks) > MAX_CONTROL_LINE:
                    raise ControllerError("mock control response is too large")
        finally:
            client.close()
        try:
            response = json.loads(bytes(chunks))
        except (UnicodeDecodeError, json.JSONDecodeError):
            raise ControllerError("mock control response is invalid") from None
        if not isinstance(response, dict) or response.get("ok") is not True:
            raise ControllerError("mock control command was rejected")
        return response

    def enter_phase(self, phase: str) -> None:
        self._request("enter_phase", phase=_require_safe_label(phase, "phase"))

    def status(self) -> Dict[str, Any]:
        response = self._request("status")
        status_value = response.get("status")
        if not isinstance(status_value, dict):
            raise ControllerError("mock control status is missing")
        return status_value

    def wait(self, timeout_seconds: float) -> bool:
        timeout_ms = int(max(0.0, min(timeout_seconds, 600.0)) * 1000)
        return bool(self._request("wait", timeout_ms=timeout_ms).get("completed"))

    def stop(self) -> Dict[str, Any]:
        self._request("stop")
        result_path = self.evidence_dir / "result.json"
        deadline = time.monotonic() + self.result_timeout
        while time.monotonic() < deadline:
            try:
                _reject_symlink(result_path)
                info = result_path.stat()
                if (
                    stat.S_ISREG(info.st_mode)
                    and info.st_uid == os.getuid()
                    and stat.S_IMODE(info.st_mode) == 0o600
                ):
                    value = json.loads(result_path.read_text(encoding="utf-8"))
                    if isinstance(value, dict):
                        return value
            except FileNotFoundError:
                pass
            time.sleep(0.02)
        raise ControllerError("mock result evidence did not appear after stop")

    def forget_token(self) -> None:
        self._token = ""


@dataclass(frozen=True)
class ProcessRecord:
    pid: int
    ppid: int
    comm: str


class ProcessInspector:
    """PID/PPID/comm/executable/listener inspection without process argv."""

    def process_table(self) -> List[ProcessRecord]:
        result = subprocess.run(
            ["/bin/ps", "-Ao", "pid=,ppid=,comm="],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        records = []
        for line in result.stdout.splitlines():
            parts = line.strip().split(None, 2)
            if len(parts) != 3 or not parts[0].isdigit() or not parts[1].isdigit():
                continue
            records.append(ProcessRecord(int(parts[0]), int(parts[1]), parts[2]))
        return records

    def executable_for_pid(self, pid: int) -> Optional[Path]:
        if pid <= 1:
            return None
        result = subprocess.run(
            ["/usr/sbin/lsof", "-nP", "-a", "-p", str(pid), "-d", "txt", "-Fn"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        for line in result.stdout.splitlines():
            if line.startswith("n") and len(line) > 1:
                return Path(line[1:])
        return None

    def listener_owned(self, pid: int, port: int) -> bool:
        if pid <= 1 or port in FORBIDDEN_PORTS:
            return False
        result = subprocess.run(
            [
                "/usr/sbin/lsof", "-nP", "-a", "-p", str(pid),
                f"-iTCP:{port}", "-sTCP:LISTEN",
            ],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return result.returncode == 0

    def pid_alive(self, pid: int) -> bool:
        if pid <= 1:
            return False
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False
        except PermissionError:
            return True

    def children(self, parent_pid: int) -> List[ProcessRecord]:
        return [record for record in self.process_table() if record.ppid == parent_pid]


def _bundle_for_executable(executable: Path) -> Optional[Path]:
    try:
        if executable.parent.name != "MacOS":
            return None
        contents = executable.parent.parent
        bundle = contents.parent
        if contents.name != "Contents" or bundle.suffix != ".app":
            return None
        return bundle
    except (AttributeError, IndexError):
        return None


def _read_bundle_info(bundle: Path) -> Dict[str, Any]:
    info_path = bundle / "Contents/Info.plist"
    _reject_existing_symlink_components(bundle)
    _reject_existing_symlink_components(info_path)
    with info_path.open("rb") as handle:
        info = plistlib.load(handle)
    if not isinstance(info, dict):
        raise ControllerError("invalid app Info.plist")
    return info


class InstalledProviderSession:
    def __init__(
        self,
        case_id: str,
        *,
        root: Optional[Path] = None,
        app_bundle: Path = APP_BUNDLE,
        allow_test_bundle: bool = False,
        inspector: Optional[ProcessInspector] = None,
        scenario_control: Optional[Any] = None,
    ):
        if case_id not in CASE_DEFINITIONS:
            raise ControllerError("unknown installed provider case")
        self.case = CASE_DEFINITIONS[case_id]
        self.app_bundle = Path(app_bundle)
        if self.app_bundle != APP_BUNDLE and not allow_test_bundle:
            raise ControllerError("non-installed app bundle is test-only")
        self.inspector = inspector or ProcessInspector()
        try:
            Path(root).lstat() if root is not None else None
            root_preexisted = root is not None
        except FileNotFoundError:
            root_preexisted = False
        self.root = self._create_root(root)
        self._root_created_by_session = not root_preexisted
        self._workspace_destroyed = False
        self.home = self.root / "home"
        self.csswitch_dir = self.home / ".csswitch"
        self.evidence = self.root / "evidence"
        self.tmp = self.root / "tmp"
        self.bin_dir = self.root / "bin"
        for path in (self.home, self.csswitch_dir, self.evidence, self.tmp, self.bin_dir):
            _ensure_private_dir(path)
        self.app_bin, self.gateway_bin = self._validate_bundle()
        self.fake_science = self.bin_dir / "claude-science"
        self._install_wrappers()
        self._scenario = build_case_scenario(self.case)
        self._proxy_reservation = LoopbackPortReservation()
        try:
            self._sandbox_reservation = LoopbackPortReservation()
        except Exception:
            self._proxy_reservation.release()
            raise
        self.proxy_port = self._proxy_reservation.port
        self.sandbox_port = self._sandbox_reservation.port
        if len({self.proxy_port, self.sandbox_port}) != 2:
            self._proxy_reservation.release()
            self._sandbox_reservation.release()
            raise ControllerError("dynamic port collision")
        if FORBIDDEN_PORTS.intersection({self.proxy_port, self.sandbox_port}):
            self._proxy_reservation.release()
            self._sandbox_reservation.release()
            raise ControllerError("forbidden port selected")
        if isinstance(scenario_control, InProcessScenarioControl) and not allow_test_bundle:
            self._proxy_reservation.release()
            self._sandbox_reservation.release()
            raise ControllerError("in-process provider mock is unit-test-only")
        self._mock = scenario_control or SubprocessScenarioControl(self._scenario, self.evidence)
        self._mock_started = False
        self._mock_stopped = False
        self._phase_snapshot: Optional[Tuple[str, int, bool]] = None
        self._config_checkpoint: Optional[Any] = None
        self._config_checkpoint_label: Optional[str] = None
        self._app_pid: Optional[int] = None
        self._runtime_records: Dict[str, Dict[str, Any]] = {}
        self._mock_result: Optional[Dict[str, Any]] = None
        self._last_log_scan: Optional[Dict[str, Any]] = None
        self._closed = False

    @staticmethod
    def _create_root(root: Optional[Path]) -> Path:
        if root is None:
            short_tmp = Path("/private/tmp") if Path("/private/tmp").is_dir() else Path(tempfile.gettempdir())
            candidate = Path(tempfile.mkdtemp(prefix="csim.", dir=short_tmp))
        else:
            candidate = Path(root)
            _reject_symlink(candidate)
            candidate.mkdir(parents=True, exist_ok=True, mode=0o700)
        _reject_symlink(candidate)
        candidate.chmod(0o700)
        canonical = candidate.resolve(strict=True)
        real_home = Path(os.path.expanduser("~")).resolve(strict=True)
        if canonical == real_home or _is_relative_to(canonical, real_home):
            raise ControllerError("test root resolves inside real HOME")
        return canonical

    def _validate_bundle(self) -> Tuple[Path, Path]:
        info = _read_bundle_info(self.app_bundle)
        if info.get("CFBundleIdentifier") != EXPECTED_BUNDLE_ID:
            raise ControllerError("unexpected installed bundle identifier")
        executable_name = info.get("CFBundleExecutable")
        if executable_name != EXPECTED_EXECUTABLE:
            raise ControllerError("unexpected installed executable name")
        app_bin = self.app_bundle / "Contents/MacOS" / executable_name
        gateway = self.app_bundle / "Contents/MacOS" / GATEWAY_EXECUTABLE
        for path in (app_bin, gateway):
            _reject_existing_symlink_components(path)
            if not path.is_file() or not os.access(path, os.X_OK):
                raise ControllerError(f"missing installed executable: {path.name}")
        return app_bin, gateway

    def _install_wrappers(self) -> None:
        open_log = self.evidence / "fake-open.log"
        tripwire = self.evidence / "python3-tripwire.log"
        open_script = """#!/bin/sh
set -eu
test -n "${CSSWITCH_FAKE_OPEN_LOG:-}" && printf 'open-called\\n' >> "$CSSWITCH_FAKE_OPEN_LOG"
exit 0
"""
        security_script = "#!/bin/sh\nexit 0\n"
        python_tripwire = (
            "#!/bin/sh\nset -eu\nprintf 'python3-invoked\\n' >> "
            + shlex.quote(str(tripwire))
            + "\nexit 97\n"
        )
        server_path = self.bin_dir / "fake-science-server"
        server_script = """#!/usr/bin/python3
import http.server
import json
import os
from pathlib import Path
import socketserver
import subprocess
import sys

port = int(sys.argv[1])
state = Path(sys.argv[2])
state.mkdir(parents=True, exist_ok=True)
os.chmod(state, 0o700)

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def do_GET(self):
        body = b'{"status":"ok","fake_science":true}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

class Server(socketserver.TCPServer):
    allow_reuse_address = False

with Server(("127.0.0.1", port), Handler) as server:
    pid = os.getpid()
    result = subprocess.run(
        ["/usr/sbin/lsof", "-nP", "-a", "-p", str(pid), "-d", "txt", "-Fn"],
        check=True, capture_output=True, text=True,
    )
    executable = next((line[1:] for line in result.stdout.splitlines() if line.startswith("n")), "")
    if not executable:
        raise RuntimeError("missing executable identity")
    values = {"pid": str(pid), "port": str(port), "executable": executable}
    for name, value in values.items():
        path = state / name
        path.write_text(value, encoding="utf-8")
        os.chmod(path, 0o600)
    ready = state / "ready"
    ready.write_text("ready", encoding="utf-8")
    os.chmod(ready, 0o600)
    server.serve_forever()
"""
        science_script = """#!/bin/sh
set -eu
cmd="${1:-}"
test "$#" -eq 0 || shift
data_dir=''
port=''
while test "$#" -gt 0; do
  case "$1" in
    --data-dir) data_dir="$2"; shift 2 ;;
    --port) port="$2"; shift 2 ;;
    *) shift ;;
  esac
done
test -n "$data_dir"
state="$data_dir/csswitch-installed-fake-science"
case "$cmd" in
  serve)
    case "$port" in ''|*[!0-9]*) exit 2 ;; esac
    test "$port" != 8765
    mkdir -p "$state"
    chmod 700 "$state"
    rm -f "$state/pid" "$state/port" "$state/executable" "$state/ready"
    /usr/bin/python3 @SERVER@ "$port" "$state" >/dev/null 2>&1 &
    child=$!
    n=0
    while test ! -f "$state/ready" && test "$n" -lt 100; do
      /bin/sleep 0.05
      n=$((n + 1))
    done
    if test ! -f "$state/ready"; then
      /bin/kill -TERM "$child" 2>/dev/null || true
      wait "$child" 2>/dev/null || true
      exit 1
    fi
    ;;
  status)
    pid="$(cat "$state/pid" 2>/dev/null || true)"
    recorded_port="$(cat "$state/port" 2>/dev/null || true)"
    case "$pid:$recorded_port" in *[!0-9:]*) echo '{"running":false}'; exit 1 ;; esac
    if /bin/kill -0 "$pid" 2>/dev/null && /usr/sbin/lsof -nP -a -p "$pid" -iTCP:"$recorded_port" -sTCP:LISTEN >/dev/null 2>&1; then
      echo '{"running":true}'
    else
      echo '{"running":false}'
      exit 1
    fi
    ;;
  url)
    recorded_port="$(cat "$state/port")"
    printf 'http://127.0.0.1:%s\\n' "$recorded_port"
    ;;
  stop)
    if test ! -e "$state/pid" && test ! -e "$state/port" && test ! -e "$state/executable"; then
      echo already-stopped
      exit 0
    fi
    pid="$(cat "$state/pid" 2>/dev/null || true)"
    recorded_port="$(cat "$state/port" 2>/dev/null || true)"
    recorded_exe="$(cat "$state/executable" 2>/dev/null || true)"
    expected_port="${CSSWITCH_EXPECTED_SANDBOX_PORT:-}"
    case "$pid:$recorded_port:$expected_port" in *[!0-9:]*) echo REFUSE >&2; exit 1 ;; esac
    test "$pid" -gt 1 && test "$recorded_port" = "$expected_port" && test "$recorded_port" != 8765
    actual_exe="$(/usr/sbin/lsof -nP -a -p "$pid" -d txt -Fn 2>/dev/null | /usr/bin/sed -n 's/^n//p' | /usr/bin/head -n 1)"
    test -n "$recorded_exe" && test "$actual_exe" = "$recorded_exe" || { echo REFUSE >&2; exit 1; }
    /usr/sbin/lsof -nP -a -p "$pid" -iTCP:"$recorded_port" -sTCP:LISTEN >/dev/null 2>&1 || { echo REFUSE >&2; exit 1; }
    /bin/kill -TERM "$pid"
    rm -f "$state/pid" "$state/port" "$state/executable" "$state/ready"
    echo stopped
    ;;
  *) exit 2 ;;
esac
""".replace("@SERVER@", shlex.quote(str(server_path)))
        for path, body in (
            (self.bin_dir / "open", open_script),
            (self.bin_dir / "security", security_script),
            (self.bin_dir / "python3", python_tripwire),
            (server_path, server_script),
            (self.fake_science, science_script),
        ):
            _safe_write(path, body.encode(), 0o700)
            _reject_symlink(path)
            path.chmod(0o700)
        self._open_log = open_log
        self._python_tripwire = tripwire

    def _profile_base_url(self, mock_base: str) -> str:
        if self.case.base_kind == "native":
            return {
                "deepseek": "https://api.deepseek.com/anthropic",
                "qwen": "https://dashscope.aliyuncs.com/compatible-mode/v1",
            }[self.case.adapter]
        if self.case.base_kind == "proxy":
            return self.case.base_prefix
        return mock_base + self.case.base_prefix

    def _native_override(self, mock_base: str) -> str:
        if self.case.base_kind == "native":
            return mock_base + self.case.message_path
        # Status consumes this loopback-only diagnostic override for every
        # adapter.  The formal/scratch child boundary removes it for
        # relay/custom, so transport still follows the profile base (and, for
        # SiliconFlow, the owned HTTP proxy).
        return mock_base

    def _config_value(self, mock_base: str) -> Dict[str, Any]:
        profile_id = f"installed-{self.case.case_id}"
        return {
            "schema_version": 2,
            "profiles": [
                {
                    "id": profile_id,
                    "name": self.case.profile_name,
                    "template_id": self.case.template_id,
                    "category": self.case.category,
                    "api_format": self.case.api_format,
                    "base_url": self._profile_base_url(mock_base),
                    "api_key": FAKE_API_KEY,
                    "model": self.case.model,
                    "website_url": None,
                    "icon": None,
                    "icon_color": None,
                    "sort_index": 0,
                    "created_at": 1,
                    "notes": "installed-local-mock",
                }
            ],
            "active_id": "",
            "proxy_port": self.proxy_port,
            "sandbox_port": self.sandbox_port,
            "secret": FIXED_PATH_SECRET,
            "mode": "proxy",
            "pending_notice": None,
        }

    @property
    def config_path(self) -> Path:
        return self.csswitch_dir / "config.json"

    def _owned_path_has_symlink(self, path: Path) -> bool:
        try:
            relative = Path(path).relative_to(self.root)
        except ValueError:
            return True
        current = self.root
        for part in relative.parts:
            current = current / part
            try:
                if stat.S_ISLNK(current.lstat().st_mode):
                    return True
            except FileNotFoundError:
                return False
        return False

    def _write_config(self, mock_base: str) -> None:
        value = self._config_value(mock_base)
        _safe_json_write(self.config_path, value)
        self.config_path.chmod(0o600)
        self._config_checkpoint = copy.deepcopy(value)
        self._config_checkpoint_label = "prepared"
        self._record_config_fingerprint("prepared")

    def _record_config_fingerprint(self, label: str) -> str:
        raw = self.config_path.read_bytes()
        digest = _sha256(raw)
        _safe_json_write(
            self.evidence / f"config-{_require_safe_label(label)}.json",
            {"schema": CONTROLLER_SCHEMA, "label": label, "sha256": digest},
        )
        return digest

    def preflight_blockers(self) -> List[str]:
        blockers = list(self.case.blockers)
        if self.same_bundle_processes():
            blockers.append("same_bundle_process_running")
        return sorted(set(blockers))

    def same_bundle_processes(self) -> List[Dict[str, Any]]:
        matches = []
        for record in self.inspector.process_table():
            if Path(record.comm).name != EXPECTED_EXECUTABLE:
                continue
            executable = self.inspector.executable_for_pid(record.pid)
            if executable is None:
                # A same-named process that cannot be identified is not safe to ignore.
                matches.append({"pid": record.pid, "executable": None, "identity": "unknown"})
                continue
            bundle = _bundle_for_executable(executable)
            if bundle is None:
                continue
            try:
                bundle_id = _read_bundle_info(bundle).get("CFBundleIdentifier")
            except (ControllerError, OSError, plistlib.InvalidFileException):
                matches.append(
                    {
                        "pid": record.pid,
                        "executable": str(executable),
                        "identity": "unknown_bundle",
                    }
                )
                continue
            if bundle_id == EXPECTED_BUNDLE_ID:
                matches.append(
                    {"pid": record.pid, "executable": str(executable), "identity": "same_bundle"}
                )
        return matches

    def start_mock(self) -> Dict[str, Any]:
        if self._mock_started:
            raise ControllerError("mock already started")
        ready = self._mock.start()
        if self._mock.port in {self.proxy_port, self.sandbox_port} | set(FORBIDDEN_PORTS):
            self._mock.stop()
            raise ControllerError("mock selected a reserved port")
        mock_pid = ready.get("owned_pid")
        if not isinstance(mock_pid, int) or mock_pid <= 1:
            self._mock.stop()
            raise ControllerError("mock owned PID is invalid")
        executable = self.inspector.executable_for_pid(mock_pid)
        expected_executables = {self._mock.expected_executable}
        if isinstance(self._mock, SubprocessScenarioControl):
            controller_executable = self.inspector.executable_for_pid(os.getpid())
            if controller_executable is not None:
                expected_executables.add(controller_executable.resolve(strict=False))
        try:
            executable_matches = (
                executable is not None
                and executable.resolve(strict=True) in expected_executables
            )
        except OSError:
            executable_matches = False
        if not executable_matches or not self.inspector.listener_owned(mock_pid, self._mock.port):
            self._mock.stop()
            raise ControllerError("mock PID/executable/listener identity not proven")
        self._mock_started = True
        self._mock_stopped = False
        self._write_config(self._mock.base_url)
        safe_ready = {
            "schema": ready["schema"],
            "scenario": ready["scenario"],
            "host": ready["host"],
            "port": ready["port"],
            "base_url": ready["base_url"],
            "owned_pid": ready["owned_pid"],
            "owned_executable": str(executable),
            "listener_verified": True,
            "phase": ready["phase"],
        }
        _safe_json_write(self.evidence / "mock-ready.json", safe_ready)
        return safe_ready

    def prepare_dry_run(self) -> Dict[str, Any]:
        reservation = LoopbackPortReservation()
        try:
            if reservation.port in {self.proxy_port, self.sandbox_port}:
                raise ControllerError("dry-run mock port collision")
            mock_base = f"http://127.0.0.1:{reservation.port}"
            self._write_config(mock_base)
            plan = self.safe_plan(mock_base=mock_base)
            plan["dry_run"] = True
            plan["do_not_execute"] = True
            plan["preflight_would_allow_launch"] = plan["launch_allowed"]
            plan["launch_allowed"] = False
            return plan
        finally:
            reservation.release()

    def _launch_environment(self, mock_base: str, *, auto_boot: bool = False) -> Dict[str, str]:
        env = {
            "HOME": str(self.home),
            "TMPDIR": str(self.tmp),
            "PATH": f"{self.bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
            "SCIENCE_BIN": str(self.fake_science),
            "CSSWITCH_EXPECTED_SANDBOX_PORT": str(self.sandbox_port),
            "CSSWITCH_TOOLUSE_SHIM": self.case.shim,
            "CSSWITCH_UPSTREAM_URL": self._native_override(mock_base),
            "CSSWITCH_FAKE_OPEN_LOG": str(self._open_log),
            "CSSWITCH_DOCTOR_CHECK_REAL_HOME": "0",
            "CSSWITCH_AUTO_BOOT_ON_LAUNCH": "1" if auto_boot else "0",
            "CSSWITCH_SCIENCE_WEBVIEW_SPIKE": "0",
            "CSSWITCH_REPO": "",
            "CSSWITCH_PROVIDER": "",
            "CSSWITCH_AUTH_TOKEN": "",
            "CSSWITCH_LAUNCH_ID": "",
            "CSSWITCH_OPENAI_BASE_URL": "",
            "CSSWITCH_OPENAI_MODEL": "",
            "CSSWITCH_RELAY_BASE_URL": "",
            "CSSWITCH_RELAY_MODEL": "",
            "CSSWITCH_RELAY_THINKING": "",
            "DEEPSEEK_API_KEY": "",
            "DASHSCOPE_API_KEY": "",
            "CSSWITCH_RELAY_KEY": "",
            "CSSWITCH_OPENAI_KEY": "",
            "ANTHROPIC_API_KEY": "",
            "ANTHROPIC_AUTH_TOKEN": "",
            "ANTHROPIC_BASE_URL": "",
            "HTTP_PROXY": "",
            "HTTPS_PROXY": "",
            "ALL_PROXY": "",
            "http_proxy": "",
            "https_proxy": "",
            "all_proxy": "",
            "NO_PROXY": "127.0.0.1,localhost,::1",
            "no_proxy": "127.0.0.1,localhost,::1",
        }
        if self.case.base_kind == "proxy":
            env.update(
                {
                    "HTTP_PROXY": mock_base,
                    "http_proxy": mock_base,
                    "HTTPS_PROXY": mock_base,
                    "https_proxy": mock_base,
                    "NO_PROXY": "",
                    "no_proxy": "",
                }
            )
        return env

    def launch_argv(self, *, auto_boot: bool = False, mock_base: Optional[str] = None) -> List[str]:
        if self.case.blockers:
            raise ControllerError("case has unresolved fail-closed blockers")
        if mock_base is None:
            if not self._mock_started:
                raise ControllerError("mock must be started before launch plan")
            mock_base = self._mock.base_url
        env = self._launch_environment(mock_base, auto_boot=auto_boot)
        argv = ["/usr/bin/open", "-n", "-F", "-g"]
        for name in sorted(env):
            argv.extend(["--env", f"{name}={env[name]}"])
        argv.append(str(self.app_bundle))
        return argv

    def safe_plan(self, *, mock_base: Optional[str] = None, auto_boot: bool = False) -> Dict[str, Any]:
        if mock_base is None:
            if not self._mock_started:
                raise ControllerError("mock must be started before launch plan")
            mock_base = self._mock.base_url
        blockers = self.preflight_blockers()
        argv = None
        if not blockers:
            argv = self.launch_argv(auto_boot=auto_boot, mock_base=mock_base)
            encoded = json.dumps(argv, sort_keys=True)
            if FIXED_PATH_SECRET in encoded or FAKE_API_KEY in encoded:
                raise AssertionError("secret entered launch plan")
        return {
            "schema": CONTROLLER_SCHEMA,
            "case": self.case.case_id,
            "app_bundle": str(self.app_bundle),
            "app_executable": str(self.app_bin),
            "gateway_executable": str(self.gateway_bin),
            "proxy_port": self.proxy_port,
            "sandbox_port": self.sandbox_port,
            "mock_base_url": mock_base,
            "adapter": self.case.adapter,
            "shim": self.case.shim,
            "formal_variant": self.case.formal_variant,
            "mock_phases": self.mock_phases(),
            "blockers": blockers,
            "launch_allowed": not blockers,
            "launch_argv": argv,
            "controller_launches_app": False,
        }

    def release_app_ports(self) -> None:
        """Release numeric-port reservations immediately before root runs open."""

        blockers = self.preflight_blockers()
        if blockers:
            raise ControllerError("refusing port release while preflight is blocked")
        self._proxy_reservation.release()
        self._sandbox_reservation.release()

    def mock_phases(self) -> List[Dict[str, Any]]:
        counts: Dict[str, int] = {}
        for step in self._scenario.steps:
            counts[step.phase] = counts.get(step.phase, 0) + 1
        phases = []
        if self.case.base_kind == "native":
            phases.append({"phase": "discovery", "mock_request_expected": False, "steps": 0})
        for phase in ("discovery", "scratch", "formal", "reuse", "restart"):
            if counts.get(phase):
                phases.append(
                    {"phase": phase, "mock_request_expected": True, "steps": counts[phase]}
                )
        return phases

    def enter_phase(self, phase: str) -> Dict[str, Any]:
        _require_safe_label(phase, "phase")
        if not self._mock_started:
            raise ControllerError("mock not started")
        status = self._mock.status()
        request_count = len(status.get("requests", []))
        no_request = self.case.base_kind == "native" and phase == "discovery"
        self._mock.enter_phase(phase)
        self._phase_snapshot = (phase, request_count, no_request)
        return {
            "phase": phase,
            "mock_request_expected": not no_request,
            "request_count_before": request_count,
        }

    def finish_phase(self, phase: str) -> Dict[str, Any]:
        if self._phase_snapshot is None or self._phase_snapshot[0] != phase:
            raise ControllerError("phase was not entered")
        _, before, no_request = self._phase_snapshot
        result = self._mock.status()
        requests = result.get("requests", [])
        after = len(requests)
        expected_ids = [step.step_id for step in self._scenario.steps if step.phase == phase]
        consumed_ids = [item.get("step") for item in requests]
        if no_request:
            ok = after == before
        else:
            ok = all(step_id in consumed_ids for step_id in expected_ids)
        self._phase_snapshot = None
        return {
            "phase": phase,
            "ok": ok,
            "requests_added": after - before,
            "expected_steps": len(expected_ids),
            "terminal_failure": bool(result.get("failures")),
        }

    def observe_app(self, timeout_seconds: float = 8.0) -> Dict[str, Any]:
        """Observe the exact installed process after root executes launch_argv."""

        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            matches = []
            for record in self.inspector.process_table():
                executable = self.inspector.executable_for_pid(record.pid)
                if executable == self.app_bin:
                    matches.append(record.pid)
            if len(matches) == 1:
                self._app_pid = matches[0]
                value = {"pid": matches[0], "executable": str(self.app_bin), "identity_verified": True}
                _safe_json_write(self.evidence / "app-identity.json", value)
                return value
            if len(matches) > 1:
                raise ControllerError("multiple exact installed app processes observed")
            time.sleep(0.05)
        raise ControllerError("exact installed app process not observed")

    def inspect_children(self) -> Dict[str, Any]:
        if self._app_pid is None:
            raise ControllerError("app PID not observed")
        children = []
        python_count = 0
        for record in self.inspector.children(self._app_pid):
            executable = self.inspector.executable_for_pid(record.pid)
            name = executable.name if executable else Path(record.comm).name
            is_python = "python" in name.lower()
            python_count += int(is_python)
            children.append(
                {
                    "pid": record.pid,
                    "comm": Path(record.comm).name,
                    "executable": str(executable) if executable else None,
                    "python_like": is_python,
                }
            )
        return {"app_pid": self._app_pid, "children": children, "python_direct_children": python_count}

    def inspect_sidecar(self) -> Dict[str, Any]:
        if self._app_pid is None:
            raise ControllerError("app PID not observed")
        matches = []
        for record in self.inspector.children(self._app_pid):
            executable = self.inspector.executable_for_pid(record.pid)
            if executable == self.gateway_bin and self.inspector.listener_owned(record.pid, self.proxy_port):
                matches.append(record.pid)
        if len(matches) != 1:
            raise ControllerError("exact installed sidecar/listener identity is not unique")
        return {
            "pid": matches[0],
            "executable": str(self.gateway_bin),
            "port": self.proxy_port,
            "identity_verified": True,
        }

    @staticmethod
    def _safe_json_shape(raw: bytes) -> Dict[str, Any]:
        try:
            value = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            return {"json": False, "top_level": None, "keys": []}
        if isinstance(value, dict):
            return {"json": True, "top_level": "object", "keys": sorted(value)[:32]}
        if isinstance(value, list):
            return {"json": True, "top_level": "array", "keys": []}
        return {"json": True, "top_level": type(value).__name__, "keys": []}

    def _http_request(
        self,
        method: str,
        path: str,
        *,
        body: Optional[bytes] = None,
        headers: Optional[Mapping[str, str]] = None,
        timeout: float = 4.0,
    ) -> Tuple[int, Dict[str, str], bytes]:
        conn = http.client.HTTPConnection("127.0.0.1", self.proxy_port, timeout=timeout)
        conn.request(method, path, body=body, headers=dict(headers or {}))
        response = conn.getresponse()
        raw = response.read(1_048_577)
        status = response.status
        response_headers = {name.lower(): value for name, value in response.getheaders()}
        conn.close()
        if len(raw) > 1_048_576:
            raise ControllerError("response exceeds evidence limit")
        return status, response_headers, raw

    def inspect_health(self) -> Dict[str, Any]:
        missing, _, _ = self._http_request("GET", "/health")
        wrong, _, _ = self._http_request("GET", "/wrong-installed-secret/health")
        correct, _, raw = self._http_request("GET", f"/{FIXED_PATH_SECRET}/health")
        if FIXED_PATH_SECRET.encode() in raw or FAKE_API_KEY.encode() in raw:
            raise ControllerError("health response leaked sensitive test material")
        try:
            body = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ControllerError("health response is not JSON") from error
        identity = {
            "gateway": body.get("gateway"),
            "provider": body.get("provider"),
            "shim": body.get("shim"),
            "launch_id": body.get("launch_id"),
        }
        ok = (
            missing == 403
            and wrong == 403
            and correct == 200
            and identity["gateway"] == "rust"
            and identity["provider"] == self.case.adapter
            and identity["shim"] == self.case.shim
            and isinstance(identity["launch_id"], str)
            and bool(identity["launch_id"])
        )
        return {
            "ok": ok,
            "missing_secret_status": missing,
            "wrong_secret_status": wrong,
            "correct_secret_status": correct,
            **identity,
            "sensitive_material_absent": True,
        }

    def _formal_payload(self) -> bytes:
        payload: Dict[str, Any] = {
            "model": "claude-opus-4-8",
            "max_tokens": 1_000_000,
            "thinking": {"type": "auto"},
            "messages": [{"role": "user", "content": "installed local mock ping"}],
        }
        if "tools" in self.case.formal_variant:
            payload["tools"] = [
                {
                    "name": "lookup",
                    "description": "local mock tool",
                    "input_schema": {
                        "type": "object",
                        "properties": {"query": {"type": "string"}},
                        "required": ["query"],
                    },
                }
            ]
            if self.case.case_id in {"relay-force", "kimi"}:
                payload["tools"].append(
                    {
                        "name": "empty",
                        "description": "local mock empty-schema tool",
                        "input_schema": {},
                    }
                )
            payload["tool_choice"] = {"type": "tool", "name": "lookup"}
        if self.case.case_id in {"qwen-tools", "responses"}:
            payload["messages"] = [
                {"role": "user", "content": "installed local mock ping"},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "lookup",
                            "input": {"query": "local mock"},
                        }
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_1",
                            "content": "local mock result",
                        }
                    ],
                },
            ]
        if self.case.formal_variant.startswith("stream"):
            payload["stream"] = True
        return json.dumps(payload, separators=(",", ":")).encode()

    def send_formal(self) -> Dict[str, Any]:
        payload = self._formal_payload()
        status, headers, raw = self._http_request(
            "POST",
            f"/{FIXED_PATH_SECRET}/v1/messages",
            body=payload,
            headers={"Content-Type": "application/json"},
            timeout=10.0,
        )
        if FIXED_PATH_SECRET.encode() in raw or FAKE_API_KEY.encode() in raw:
            raise ControllerError("formal response leaked sensitive test material")
        content_type = headers.get("content-type", "").split(";", 1)[0]
        result = {
            "status": status,
            "content_type": content_type,
            "response_bytes": len(raw),
            "response_shape": self._safe_json_shape(raw),
            "variant": self.case.formal_variant,
            "sensitive_material_absent": True,
        }
        _safe_json_write(self.evidence / f"formal-{self.case.case_id}.json", result)
        return result

    def checkpoint_config(self, label: str) -> Dict[str, Any]:
        label = _require_safe_label(label)
        raw = self.config_path.read_bytes()
        value = json.loads(raw)
        self._config_checkpoint = copy.deepcopy(value)
        self._config_checkpoint_label = label
        return {"label": label, "sha256": self._record_config_fingerprint(label)}

    def inspect_config(self, allowed_paths: Iterable[str] = ()) -> Dict[str, Any]:
        if self._config_checkpoint is None:
            raise ControllerError("config checkpoint is missing")
        raw = self.config_path.read_bytes()
        current = json.loads(raw)
        changed = sorted(_changed_json_paths(self._config_checkpoint, current))
        allowed = sorted(set(allowed_paths))
        unexpected = [path for path in changed if path not in allowed]
        return {
            "checkpoint": self._config_checkpoint_label,
            "sha256": _sha256(raw),
            "changed_paths": changed,
            "allowed_paths": allowed,
            "unexpected_paths": unexpected,
            "ok": not unexpected,
        }

    def scan_logs(self) -> Dict[str, Any]:
        paths: List[Path] = []
        logs = self.csswitch_dir / "logs"
        if logs.is_dir():
            paths.extend(path for path in logs.rglob("*") if path.is_file() and not path.is_symlink())
        paths.extend(
            path
            for path in self.evidence.rglob("*")
            if path.is_file() and not path.is_symlink()
        )
        needles = (FIXED_PATH_SECRET.encode(), FAKE_API_KEY.encode())
        matches = 0
        for path in paths:
            if path == self.config_path:
                continue
            data = path.read_bytes()
            matches += sum(data.count(needle) for needle in needles)
        result = {
            "sensitive_log_match_count": matches,
            "python_tripwire_invoked": self._python_tripwire.exists(),
            "files_scanned": len(paths),
            "ok": matches == 0 and not self._python_tripwire.exists(),
        }
        self._last_log_scan = copy.deepcopy(result)
        return result

    def record_runtime(self, label: str) -> Dict[str, Any]:
        label = _require_safe_label(label)
        health = self.inspect_health()
        if not health["ok"]:
            raise ControllerError("runtime health identity mismatch")
        sidecar = self.inspect_sidecar()
        record = {
            "label": label,
            "pid": sidecar["pid"],
            "executable": sidecar["executable"],
            "port": sidecar["port"],
            "launch_id": health["launch_id"],
            "provider": health["provider"],
            "shim": health["shim"],
            "gateway": health["gateway"],
        }
        self._runtime_records[label] = record
        _safe_json_write(self.evidence / f"runtime-{label}.json", record)
        return record

    def compare_runtime(self, before: str, after: str, relation: str) -> Dict[str, Any]:
        if relation not in {"reuse", "restart"}:
            raise ControllerError("relation must be reuse or restart")
        first = self._runtime_records.get(before)
        second = self._runtime_records.get(after)
        if first is None or second is None:
            raise ControllerError("runtime record is missing")
        same_pid = first["pid"] == second["pid"]
        same_launch = first["launch_id"] == second["launch_id"]
        ok = (same_pid and same_launch) if relation == "reuse" else (not same_pid and not same_launch)
        return {
            "relation": relation,
            "before": before,
            "after": after,
            "same_pid": same_pid,
            "same_launch_id": same_launch,
            "ok": ok,
        }

    def inspect_fake_science(self) -> Dict[str, Any]:
        state = self.csswitch_dir / "sandbox/home/.claude-science/csswitch-installed-fake-science"
        if self._owned_path_has_symlink(state):
            raise ControllerError("fake Science state traverses a symlink")
        try:
            pid = int((state / "pid").read_text().strip())
            port = int((state / "port").read_text().strip())
            recorded_exe = Path((state / "executable").read_text().strip())
        except (FileNotFoundError, ValueError):
            raise ControllerError("fake Science owned state is incomplete")
        actual_exe = self.inspector.executable_for_pid(pid)
        ok = (
            pid > 1
            and port == self.sandbox_port
            and port not in FORBIDDEN_PORTS
            and actual_exe == recorded_exe
            and self.inspector.listener_owned(pid, port)
        )
        value = {
            "pid": pid,
            "port": port,
            "executable": str(recorded_exe),
            "identity_verified": ok,
        }
        if ok:
            _safe_json_write(self.evidence / "fake-science-identity.json", value)
        return value

    def stop_fake_science(self) -> Dict[str, Any]:
        env = {
            "HOME": str(self.home),
            "PATH": f"{self.bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
            "CSSWITCH_EXPECTED_SANDBOX_PORT": str(self.sandbox_port),
        }
        data_dir = self.csswitch_dir / "sandbox/home/.claude-science"
        if self._owned_path_has_symlink(data_dir):
            raise ControllerError("fake Science data directory traverses a symlink")
        result = subprocess.run(
            [str(self.fake_science), "stop", "--data-dir", str(data_dir)],
            check=False,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return {"ok": result.returncode == 0, "exit_code": result.returncode}

    def stop_mock(self) -> Dict[str, Any]:
        result = self._mock.stop()
        self._mock_stopped = True
        safe = copy.deepcopy(result)
        self._mock_result = copy.deepcopy(safe)
        _safe_json_write(self.evidence / "mock-result.json", safe)
        return safe

    def export_sanitized_summary(self, destination: Path) -> Dict[str, Any]:
        """Export counts/identity outcomes only; never copy raw config or hits."""

        destination = Path(destination)
        if not destination.is_absolute() or destination.name in {"", ".", ".."}:
            raise ControllerError("sanitized summary destination must be an absolute file")
        _reject_symlink(destination)
        parent = destination.parent.resolve(strict=True)
        parent_info = parent.stat()
        if (
            not stat.S_ISDIR(parent_info.st_mode)
            or parent_info.st_uid != os.getuid()
            or stat.S_IMODE(parent_info.st_mode) != 0o700
        ):
            raise ControllerError("sanitized summary parent must be owned 0700")
        repo_root = Path(__file__).resolve(strict=True).parents[1]
        real_home = Path(os.path.expanduser("~")).resolve(strict=True)
        canonical_destination = parent / destination.name
        if (
            canonical_destination == repo_root
            or _is_relative_to(canonical_destination, repo_root)
            or canonical_destination == self.root
            or _is_relative_to(canonical_destination, self.root)
            or canonical_destination == real_home
            or _is_relative_to(canonical_destination, real_home)
        ):
            raise ControllerError("sanitized summary must be outside repo, real HOME, and test root")
        if self._owned_path_has_symlink(self.config_path):
            raise ControllerError("config path traverses a symlink")
        config_sha256 = _sha256(self.config_path.read_bytes())
        mock_result = self._mock_result or {}
        cleanup = self.verify_cleanup()
        summary = {
            "schema": "csswitch.installed-provider-summary.v1",
            "case": self.case.case_id,
            "config_sha256": config_sha256,
            "runtime": [
                {
                    "label": label,
                    "gateway": record.get("gateway"),
                    "provider": record.get("provider"),
                    "shim": record.get("shim"),
                }
                for label, record in sorted(self._runtime_records.items())
            ],
            "mock": {
                "scenario": mock_result.get("scenario"),
                "protocol_complete": bool(mock_result.get("protocol_complete")),
                "final_ok": bool(mock_result.get("final_ok")),
                "request_count": len(mock_result.get("requests", [])),
                "failure_count": len(mock_result.get("failures", [])),
            },
            "log_scan": copy.deepcopy(self._last_log_scan),
            "cleanup": {
                "ok": cleanup["ok"],
                "ports_closed": copy.deepcopy(cleanup["ports_closed"]),
                "alive_owned_pid_count": sum(
                    int(item["alive"]) for item in cleanup["owned_pids"]
                ),
            },
        }
        encoded = json.dumps(summary, ensure_ascii=False, sort_keys=True)
        if FIXED_PATH_SECRET in encoded or FAKE_API_KEY in encoded:
            raise AssertionError("sanitized summary invariant failed")
        _safe_json_write(canonical_destination, summary)
        return {"exported": True, "destination": str(canonical_destination)}

    @staticmethod
    def _port_closed(port: int) -> bool:
        if port in FORBIDDEN_PORTS:
            raise ControllerError("refusing cleanup probe of forbidden port")
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(0.25)
        try:
            return sock.connect_ex(("127.0.0.1", port)) != 0
        finally:
            sock.close()

    def verify_cleanup(self) -> Dict[str, Any]:
        self._proxy_reservation.release()
        self._sandbox_reservation.release()
        ports = {"proxy": self.proxy_port, "sandbox": self.sandbox_port}
        if self._mock_started:
            ports["mock"] = self._mock.port
        port_results = {name: self._port_closed(port) for name, port in ports.items()}
        pid_results = []
        seen = set()
        for record in [
            *self._runtime_records.values(),
            ({"pid": self._app_pid, "executable": str(self.app_bin)} if self._app_pid else {}),
        ]:
            pid = record.get("pid")
            if not isinstance(pid, int) or pid in seen:
                continue
            seen.add(pid)
            alive = self.inspector.pid_alive(pid)
            actual = self.inspector.executable_for_pid(pid) if alive else None
            expected = record.get("executable")
            pid_results.append(
                {
                    "pid": pid,
                    "alive": alive,
                    "identity_match": alive and str(actual) == expected,
                }
            )
        if isinstance(self._mock, SubprocessScenarioControl) and self._mock.process_pid:
            mock_pid = self._mock.process_pid
            alive = self._mock.process_alive
            actual = self.inspector.executable_for_pid(mock_pid) if alive else None
            pid_results.append(
                {
                    "pid": mock_pid,
                    "alive": alive,
                    "identity_match": (
                        alive
                        and actual is not None
                        and actual.resolve(strict=False) == self._mock.expected_executable
                    ),
                }
            )

        fake_state_valid = True
        fake_state = self.csswitch_dir / "sandbox/home/.claude-science/csswitch-installed-fake-science"
        if self._owned_path_has_symlink(fake_state):
            fake_state_valid = False
        elif fake_state.exists():
            if not fake_state.is_dir():
                fake_state_valid = False
            else:
                try:
                    entries = list(fake_state.iterdir())
                    if entries:
                        fake_pid = int(
                            (fake_state / "pid").read_text(encoding="utf-8").strip()
                        )
                        fake_executable = (
                            fake_state / "executable"
                        ).read_text(encoding="utf-8").strip()
                        if fake_pid <= 1 or not fake_executable:
                            raise ValueError
                        alive = self.inspector.pid_alive(fake_pid)
                        actual = self.inspector.executable_for_pid(fake_pid) if alive else None
                        pid_results.append(
                            {
                                "pid": fake_pid,
                                "alive": alive,
                                "identity_match": alive and str(actual) == fake_executable,
                            }
                        )
                except (FileNotFoundError, OSError, ValueError):
                    fake_state_valid = False

        ok = (
            all(port_results.values())
            and fake_state_valid
            and not any(item["alive"] for item in pid_results)
        )
        return {
            "ports_closed": port_results,
            "owned_pids": pid_results,
            "fake_science_state_valid": fake_state_valid,
            "ok": ok,
        }

    def destroy_workspace(self) -> Dict[str, bool]:
        """Remove only this session's private root after owned cleanup is proven."""

        if self._workspace_destroyed:
            return {"root_removed": True}
        if not self._root_created_by_session:
            raise ControllerError("refusing to remove a pre-existing workspace root")
        _reject_symlink(self.root)
        info = self.root.stat()
        if (
            not stat.S_ISDIR(info.st_mode)
            or info.st_uid != os.getuid()
            or stat.S_IMODE(info.st_mode) != 0o700
            or self.root.resolve(strict=True) != self.root
        ):
            raise ControllerError("workspace root identity is not private and canonical")
        real_home = Path(os.path.expanduser("~")).resolve(strict=True)
        if self.root == real_home or _is_relative_to(self.root, real_home):
            raise ControllerError("refusing workspace removal inside real HOME")
        for current, directories, filenames in os.walk(self.root, followlinks=False):
            for name in [*directories, *filenames]:
                candidate = Path(current) / name
                if stat.S_ISLNK(candidate.lstat().st_mode):
                    raise ControllerError("refusing workspace removal containing a symlink")
        cleanup = self.verify_cleanup()
        if not cleanup["ok"]:
            raise ControllerError("refusing workspace removal before owned cleanup passes")
        shutil.rmtree(self.root)
        if self.root.exists() or self.root.is_symlink():
            raise ControllerError("workspace removal did not complete")
        self._workspace_destroyed = True
        return {"root_removed": True}

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self._mock_started and not self._mock_stopped:
            self._mock.stop()
            self._mock_stopped = True
        self._proxy_reservation.release()
        self._sandbox_reservation.release()

    def __enter__(self) -> "InstalledProviderSession":
        return self

    def __exit__(self, _exc_type, _exc, _traceback) -> None:
        self.close()


def _scrub_error(message: str) -> str:
    return message.replace(FIXED_PATH_SECRET, "<redacted>").replace(FAKE_API_KEY, "<redacted>")


def _dispatch(session: InstalledProviderSession, command: Mapping[str, Any]) -> Any:
    op = command.get("op")
    if op == "start_mock":
        return session.start_mock()
    if op == "plan":
        return session.safe_plan(auto_boot=bool(command.get("auto_boot", False)))
    if op == "release_ports":
        session.release_app_ports()
        return {"released": True}
    if op == "observe_app":
        return session.observe_app(float(command.get("timeout_seconds", 8.0)))
    if op == "enter_phase":
        return session.enter_phase(str(command.get("phase", "")))
    if op == "finish_phase":
        return session.finish_phase(str(command.get("phase", "")))
    if op == "health":
        return session.inspect_health()
    if op == "formal":
        return session.send_formal()
    if op == "children":
        return session.inspect_children()
    if op == "sidecar":
        return session.inspect_sidecar()
    if op == "record_runtime":
        return session.record_runtime(str(command.get("label", "")))
    if op == "compare_runtime":
        return session.compare_runtime(
            str(command.get("before", "")),
            str(command.get("after", "")),
            str(command.get("relation", "")),
        )
    if op == "config_checkpoint":
        return session.checkpoint_config(str(command.get("label", "")))
    if op == "config_check":
        allowed = command.get("allowed_paths", [])
        if not isinstance(allowed, list) or not all(isinstance(item, str) for item in allowed):
            raise ControllerError("allowed_paths must be a string array")
        return session.inspect_config(allowed)
    if op == "log_scan":
        return session.scan_logs()
    if op == "fake_science":
        return session.inspect_fake_science()
    if op == "stop_fake_science":
        return session.stop_fake_science()
    if op == "mock_status":
        return session._mock.status()
    if op == "mock_wait":
        return {"complete": session._mock.wait(float(command.get("timeout_seconds", 0.0)))}
    if op == "stop_mock":
        return session.stop_mock()
    if op == "cleanup_check":
        return session.verify_cleanup()
    if op == "export_summary":
        destination = command.get("destination")
        if not isinstance(destination, str):
            raise ControllerError("summary destination must be a string")
        return session.export_sanitized_summary(Path(destination))
    if op == "destroy_workspace":
        return session.destroy_workspace()
    if op == "close":
        session.close()
        return {"closed": True}
    raise ControllerError("unknown controller operation")


def run_json_lines(session: InstalledProviderSession) -> int:
    hello = {
        "ok": True,
        "event": "controller_ready",
        "schema": CONTROLLER_SCHEMA,
        "case": session.case.case_id,
        "test_root": str(session.root),
        "controller_launches_app": False,
    }
    print(json.dumps(hello, sort_keys=True), flush=True)
    for raw_line in sys.stdin:
        if len(raw_line.encode("utf-8", "replace")) > MAX_CONTROL_LINE:
            response = {"ok": False, "error": "control line too large"}
            print(json.dumps(response, sort_keys=True), flush=True)
            continue
        try:
            command = json.loads(raw_line)
            if not isinstance(command, dict):
                raise ControllerError("controller command must be an object")
            result = _dispatch(session, command)
            response = {"ok": True, "op": command.get("op"), "result": result}
        except Exception as error:  # JSON control must stay alive for safe cleanup.
            response = {
                "ok": False,
                "error_type": type(error).__name__,
                "error": _scrub_error(str(error)),
            }
        encoded = json.dumps(response, ensure_ascii=False, sort_keys=True)
        if FIXED_PATH_SECRET in encoded or FAKE_API_KEY in encoded:
            encoded = json.dumps(
                {"ok": False, "error": "controller redaction invariant failed"}, sort_keys=True
            )
        print(encoded, flush=True)
        if command.get("op") == "close" and response.get("ok"):
            return 0
    session.close()
    return 0


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--case", required=True, choices=sorted(CASE_DEFINITIONS))
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--jsonl", action="store_true")
    args = parser.parse_args(argv)
    if not args.dry_run and not args.jsonl:
        parser.error("choose --dry-run or --jsonl")
    session = InstalledProviderSession(args.case)
    destroy_after_dry_run = False
    try:
        if args.dry_run:
            result = session.prepare_dry_run()
            encoded = json.dumps(result, ensure_ascii=False, sort_keys=True)
            if FIXED_PATH_SECRET in encoded or FAKE_API_KEY in encoded:
                raise AssertionError("dry-run output leaked sensitive material")
            print(encoded)
            destroy_after_dry_run = True
            return 0
        return run_json_lines(session)
    finally:
        session.close()
        if destroy_after_dry_run:
            session.destroy_workspace()


if __name__ == "__main__":
    raise SystemExit(main())
