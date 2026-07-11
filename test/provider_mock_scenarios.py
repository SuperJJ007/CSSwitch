"""Strict, redacted, test-only provider scenario server.

This module is intentionally independent from ``mock_upstream.start_mock`` so
the small legacy helper can keep its API.  Installed/local-mock drivers can use
the public API without touching a real provider or the production port::

    mock = start_manifest_scenario(
        "installed_deepseek_matrix",
        secrets={"provider_key": provider_key},
    )
    print(mock.ready())
    # Point the isolated runtime at mock.base_url + "/deepseek/v1/messages".
    mock.enter_phase("discovery")  # Native discovery has no upstream request.
    mock.enter_phase("scratch")
    ...
    mock.stop()
    result = mock.result()

Each request must match the next step's phase, method, path, and checks.  An
unexpected request or a failed check makes the scenario terminally failed.  A
driver must call ``enter_phase`` before the first request in a new phase.

Only redacted summaries are retained: header values, raw bodies, and unexpected
request paths are never stored.  The server records its own PID and dynamic
port, but executable identity is deliberately left for the external driver to
verify.
"""

from __future__ import annotations

import argparse
import copy
import hmac
import json
import os
import re
import secrets as secrets_module
import signal
import socket
import socketserver
import stat
import sys
import threading
import time
from dataclasses import dataclass
from enum import Enum
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Dict, Iterable, List, Mapping, Optional, Tuple

try:  # ``PYTHONPATH=test`` runners use the first form.
    from _loopback_ports import FORBIDDEN_PORTS, bind_http_server, bind_loopback_listener
except ImportError:  # ``python -m unittest test.test_...`` uses the package form.
    from test._loopback_ports import (
        FORBIDDEN_PORTS,
        bind_http_server,
        bind_loopback_listener,
    )


SCHEMA = "csswitch.provider-mock-scenarios"
SCHEMA_VERSION = 1
RESULT_SCHEMA = "csswitch.provider-mock-result.v1"
READY_SCHEMA = "csswitch.provider-mock-ready.v1"
DEFAULT_MANIFEST = Path(__file__).with_name("provider_mock_scenarios.v1.json")
MAX_REQUEST_BODY = 1_048_576
MAX_ACTION_DEPTH = 4
MAX_CONTROL_MESSAGE = 65_536
MAX_SECRET_BYTES = 65_536

ACTION_TYPES = frozenset(
    {
        "anthropic_json",
        "anthropic_sse",
        "dsml",
        "openai_chat_text_tool",
        "openai_responses_text_tool",
        "kimi_sse",
        "models_json",
        "status",
        "malformed_json",
        "drop_before_headers",
        "drop_then_success",
        "delay",
        "stall",
        "truncated",
    }
)

_SAFE_LABEL_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.:#/-]{0,127}$")
_HEADER_NAME_RE = re.compile(r"^[!#$%&'*+.^_`|~0-9A-Za-z-]+$")
_MISSING = object()


class ScenarioError(ValueError):
    """Raised when a manifest or scenario violates the v1 contract."""


class ActionOutcome(Enum):
    CONSUMED = "CONSUMED"
    RETRY = "RETRY"
    EXPECTED_DROP = "EXPECTED_DROP"
    FAILED = "FAILED"


@dataclass(frozen=True)
class ScenarioStep:
    step_id: str
    phase: str
    method: str
    path: str
    action: Dict[str, Any]
    checks: Dict[str, Any]


@dataclass(frozen=True)
class Scenario:
    name: str
    description: str
    phases: Tuple[str, ...]
    steps: Tuple[ScenarioStep, ...]


class _ScenarioHTTPServer(ThreadingHTTPServer):
    # Non-daemon request workers plus block_on_close make stop/result a real
    # lifecycle barrier.  Stall/delay actions wait on _stop_event, so owned stop
    # releases them before server_close joins the workers.
    daemon_threads = False
    block_on_close = True
    allow_reuse_address = False

    def handle_error(self, _request, _client_address):
        # socketserver's default traceback can include request internals.  The
        # scenario state/result is the only evidence channel.
        return


def _safe_label(value: Any, field: str) -> str:
    if not isinstance(value, str) or not _SAFE_LABEL_RE.fullmatch(value):
        raise ScenarioError(f"{field} must be a short non-secret label")
    return value


def _json_type(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return "number"
    return "unknown"


def _validate_pointer(pointer: Any, field: str) -> str:
    if not isinstance(pointer, str) or not pointer.startswith("/"):
        raise ScenarioError(f"{field} must be a JSON pointer")
    if len(pointer) > 256 or any(ord(ch) < 32 for ch in pointer):
        raise ScenarioError(f"{field} is not a safe JSON pointer")
    return pointer


def _pointer_get(value: Any, pointer: str) -> Any:
    current = value
    for raw_part in pointer.split("/")[1:]:
        part = raw_part.replace("~1", "/").replace("~0", "~")
        if isinstance(current, dict):
            if part not in current:
                return _MISSING
            current = current[part]
        elif isinstance(current, list) and part.isdigit():
            index = int(part)
            if index >= len(current):
                return _MISSING
            current = current[index]
        else:
            return _MISSING
    return current


def _response_string(
    value: Any, field: str, max_length: int, allow_multiline: bool = False
) -> str:
    if (
        not isinstance(value, str)
        or not value
        or len(value.encode("utf-8")) > max_length
    ):
        raise ScenarioError(f"{field} must be a non-empty string")
    if any(
        ord(ch) < 32 and not (allow_multiline and ch in "\n\t") for ch in value
    ):
        raise ScenarioError(f"{field} contains unsafe control bytes")
    return value


def _number_in_range(value: Any, field: str, minimum: float, maximum: float) -> float:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        raise ScenarioError(f"{field} must be numeric")
    if value < minimum or value > maximum:
        raise ScenarioError(f"{field} must be between {minimum} and {maximum}")
    return float(value)


def _normalise_action(raw: Any, depth: int = 0) -> Dict[str, Any]:
    if depth > MAX_ACTION_DEPTH:
        raise ScenarioError("action nesting is too deep")
    if isinstance(raw, str):
        action: Dict[str, Any] = {"type": raw}
    elif isinstance(raw, dict):
        action = copy.deepcopy(raw)
    else:
        raise ScenarioError("action must be a string or object")
    kind = action.get("type")
    if kind not in ACTION_TYPES:
        raise ScenarioError(f"unsupported action type: {kind!r}")
    allowed_by_kind = {
        "anthropic_json": {"type", "model", "text", "include_tool", "tool_name"},
        "anthropic_sse": {"type", "model", "text", "chunk_delay"},
        "dsml": {"type", "model", "text", "stream", "chunk_delay"},
        "openai_chat_text_tool": {
            "type",
            "model",
            "text",
            "include_tool",
            "tool_name",
        },
        "openai_responses_text_tool": {
            "type",
            "model",
            "text",
            "include_tool",
            "tool_name",
        },
        "kimi_sse": {"type", "chunk_delay"},
        "models_json": {"type", "models"},
        "status": {"type", "status", "json"},
        "malformed_json": {"type"},
        "drop_before_headers": {"type"},
        "drop_then_success": {"type", "drops", "then"},
        "delay": {"type", "seconds", "then"},
        "stall": {"type", "seconds", "then"},
        "truncated": {"type", "missing_bytes", "status", "content_type"},
    }
    unknown = set(action) - allowed_by_kind[kind]
    if unknown:
        raise ScenarioError(f"action {kind} has unknown keys: {sorted(unknown)}")
    if kind in {"drop_then_success", "delay", "stall"}:
        fallback = "anthropic_json" if kind == "drop_then_success" else "status"
        action["then"] = _normalise_action(action.get("then", fallback), depth + 1)
    if kind == "drop_then_success":
        drops = action.get("drops", 1)
        if not isinstance(drops, int) or isinstance(drops, bool) or not (1 <= drops <= 20):
            raise ScenarioError("drop_then_success.drops must be an integer from 1 to 20")
    if kind in {"delay", "stall"}:
        seconds = action.get("seconds", 0.1 if kind == "delay" else 30.0)
        _number_in_range(seconds, f"{kind}.seconds", 0, 600)
    if kind == "status":
        status = action.get("status", 200)
        if not isinstance(status, int) or isinstance(status, bool) or not (200 <= status <= 599):
            raise ScenarioError("status.status must be an HTTP status")
        if "json" in action:
            try:
                encoded = _json_bytes(action["json"])
            except (TypeError, ValueError):
                raise ScenarioError("status.json must be JSON serializable") from None
            if len(encoded) > MAX_REQUEST_BODY:
                raise ScenarioError("status.json is too large")
    for field, maximum in (("model", 256), ("tool_name", 128), ("text", MAX_REQUEST_BODY)):
        if field in action:
            _response_string(
                action[field],
                f"{kind}.{field}",
                maximum,
                allow_multiline=field == "text",
            )
    if "include_tool" in action and not isinstance(action["include_tool"], bool):
        raise ScenarioError(f"{kind}.include_tool must be boolean")
    if "stream" in action and not isinstance(action["stream"], bool):
        raise ScenarioError(f"{kind}.stream must be boolean")
    if "chunk_delay" in action:
        _number_in_range(action["chunk_delay"], f"{kind}.chunk_delay", 0, 5)
    if kind == "models_json":
        models = action.get("models", ["mock-model", "mock-tool-model"])
        if not isinstance(models, list) or not models or len(models) > 1000:
            raise ScenarioError("models_json.models must be a non-empty array")
        action["models"] = [
            _response_string(model, "models_json.models item", 256) for model in models
        ]
    if kind == "truncated":
        missing = action.get("missing_bytes", 32)
        if not isinstance(missing, int) or isinstance(missing, bool) or not (1 <= missing <= MAX_REQUEST_BODY):
            raise ScenarioError("truncated.missing_bytes must be a positive bounded integer")
        status = action.get("status", 200)
        if not isinstance(status, int) or isinstance(status, bool) or not (200 <= status <= 599):
            raise ScenarioError("truncated.status must be an HTTP status")
        content_type = action.get("content_type", "application/json")
        if (
            not isinstance(content_type, str)
            or not content_type
            or len(content_type) > 128
            or "\r" in content_type
            or "\n" in content_type
        ):
            raise ScenarioError("truncated.content_type must be a safe MIME type")
    return action


def _normalise_checks(raw: Any) -> Dict[str, Any]:
    if raw is None:
        return {}
    if not isinstance(raw, dict):
        raise ScenarioError("checks must be an object")
    checks = copy.deepcopy(raw)
    unknown = set(checks) - {"headers", "body"}
    if unknown:
        raise ScenarioError(f"unknown checks keys: {sorted(unknown)}")

    headers = checks.get("headers", {})
    if not isinstance(headers, dict):
        raise ScenarioError("checks.headers must be an object")
    normalised_headers: Dict[str, Any] = {}
    for raw_name, policy in headers.items():
        if not isinstance(raw_name, str) or not _HEADER_NAME_RE.fullmatch(raw_name):
            raise ScenarioError("checks.headers contains an invalid header name")
        name = raw_name.lower()
        if isinstance(policy, bool):
            policy = {"present": policy}
        if not isinstance(policy, dict) or not policy:
            raise ScenarioError(f"header check {name} must be a boolean or non-empty object")
        allowed = {
            "present",
            "equals",
            "starts_with",
            "contains",
            "equals_secret",
            "bearer_secret",
        }
        if set(policy) - allowed:
            raise ScenarioError(f"header check {name} has unknown operators")
        if "present" in policy and not isinstance(policy["present"], bool):
            raise ScenarioError(f"header check {name}.present must be boolean")
        comparators = {
            "equals",
            "starts_with",
            "contains",
            "equals_secret",
            "bearer_secret",
        } & set(policy)
        if len(comparators) > 1:
            raise ScenarioError(f"header check {name} must use at most one comparator")
        for operator in ("equals", "starts_with", "contains"):
            if operator in policy and not isinstance(policy[operator], str):
                raise ScenarioError(f"header check {name}.{operator} must be a string")
        for operator in ("equals_secret", "bearer_secret"):
            if operator in policy:
                _safe_label(policy[operator], f"header check {name}.{operator}")
        sensitive = (
            name in {"authorization", "proxy-authorization", "x-api-key"}
            or "token" in name
            or "secret" in name
            or name.endswith("-key")
        )
        if sensitive and ({"equals", "contains"} & set(policy)):
            raise ScenarioError(
                f"sensitive header {name} cannot use literal equals/contains"
            )
        if sensitive and "starts_with" in policy and policy["starts_with"] != "Bearer ":
            raise ScenarioError(
                f"sensitive header {name} starts_with may only check the Bearer scheme"
            )
        normalised_headers[name] = policy
    checks["headers"] = normalised_headers

    body = checks.get("body", {})
    if not isinstance(body, dict):
        raise ScenarioError("checks.body must be an object")
    allowed_body = {"json", "required", "absent", "types", "equals", "shape"}
    if set(body) - allowed_body:
        raise ScenarioError("checks.body has unknown operators")
    if "json" in body and not isinstance(body["json"], bool):
        raise ScenarioError("checks.body.json must be boolean")
    for key in ("required", "absent", "shape"):
        values = body.get(key, [])
        if not isinstance(values, list) or len(values) > 256:
            raise ScenarioError(f"checks.body.{key} must be an array")
        body[key] = [_validate_pointer(value, f"checks.body.{key}") for value in values]
    for key in ("types", "equals"):
        values = body.get(key, {})
        if not isinstance(values, dict) or len(values) > 256:
            raise ScenarioError(f"checks.body.{key} must be an object")
        normalised: Dict[str, Any] = {}
        for pointer, expected in values.items():
            pointer = _validate_pointer(pointer, f"checks.body.{key}")
            if key == "types" and expected not in {
                "null",
                "boolean",
                "string",
                "array",
                "object",
                "number",
            }:
                raise ScenarioError(f"unsupported JSON type for {pointer}")
            if key == "equals":
                try:
                    encoded_expected = _json_bytes(expected)
                except (TypeError, ValueError):
                    raise ScenarioError(
                        f"checks.body.equals value for {pointer} must be JSON"
                    ) from None
                if len(encoded_expected) > MAX_REQUEST_BODY:
                    raise ScenarioError(
                        f"checks.body.equals value for {pointer} is too large"
                    )
            normalised[pointer] = expected
        body[key] = normalised
    checks["body"] = body
    return checks


def _scenario_from_mapping(name: str, raw: Mapping[str, Any]) -> Scenario:
    _safe_label(name, "scenario name")
    if not isinstance(raw, dict):
        raise ScenarioError(f"scenario {name} must be an object")
    unknown_scenario = set(raw) - {"description", "phases", "steps"}
    if unknown_scenario:
        raise ScenarioError(f"scenario {name} has unknown keys")
    description = raw.get("description", "")
    if (
        not isinstance(description, str)
        or len(description) > 2048
        or any(ord(ch) < 32 and ch not in "\n\t" for ch in description)
    ):
        raise ScenarioError(f"scenario {name}.description must be a string")
    raw_steps = raw.get("steps")
    if not isinstance(raw_steps, list) or not raw_steps:
        raise ScenarioError(f"scenario {name} must contain at least one step")
    steps: List[ScenarioStep] = []
    seen = set()
    for index, raw_step in enumerate(raw_steps):
        if not isinstance(raw_step, dict):
            raise ScenarioError(f"scenario {name} step {index} must be an object")
        unknown = set(raw_step) - {"id", "phase", "method", "path", "action", "checks"}
        if unknown:
            raise ScenarioError(f"scenario {name} step {index} has unknown keys")
        step_id = _safe_label(raw_step.get("id"), f"scenario {name} step id")
        if step_id in seen:
            raise ScenarioError(f"scenario {name} has duplicate step id {step_id}")
        seen.add(step_id)
        phase = _safe_label(raw_step.get("phase"), f"scenario {name} step phase")
        method = raw_step.get("method")
        if (
            not isinstance(method, str)
            or len(method) > 16
            or not re.fullmatch(r"[A-Za-z]+", method)
        ):
            raise ScenarioError(f"scenario {name} step {step_id} has invalid method")
        method = method.upper()
        path = raw_step.get("path")
        if not isinstance(path, str) or not path or len(path) > 2048:
            raise ScenarioError(f"scenario {name} step {step_id} has invalid path")
        if any(ord(ch) < 32 for ch in path):
            raise ScenarioError(f"scenario {name} step {step_id} path contains control bytes")
        steps.append(
            ScenarioStep(
                step_id=step_id,
                phase=phase,
                method=method,
                path=path,
                action=_normalise_action(raw_step.get("action")),
                checks=_normalise_checks(raw_step.get("checks")),
            )
        )
    raw_phases = raw.get("phases")
    if raw_phases is None:
        phases = list(dict.fromkeys(step.phase for step in steps))
    else:
        if not isinstance(raw_phases, list) or not raw_phases:
            raise ScenarioError(f"scenario {name}.phases must be a non-empty array")
        phases = [
            _safe_label(phase, f"scenario {name} phase") for phase in raw_phases
        ]
        if len(set(phases)) != len(phases):
            raise ScenarioError(f"scenario {name}.phases must be unique")
    phase_indexes = {phase: index for index, phase in enumerate(phases)}
    if any(step.phase not in phase_indexes for step in steps):
        raise ScenarioError(f"scenario {name} step references an undeclared phase")
    indexes = [phase_indexes[step.phase] for step in steps]
    if indexes != sorted(indexes):
        raise ScenarioError(f"scenario {name} steps must follow declared phase order")
    return Scenario(
        name=name,
        description=description,
        phases=tuple(phases),
        steps=tuple(steps),
    )


def load_manifest(path: os.PathLike[str] | str = DEFAULT_MANIFEST) -> Dict[str, Scenario]:
    """Load and validate a v1 manifest without retaining request secrets."""

    manifest_path = Path(path)
    with manifest_path.open("r", encoding="utf-8") as handle:
        raw = json.load(handle)
    if not isinstance(raw, dict):
        raise ScenarioError("manifest root must be an object")
    unknown = set(raw) - {"schema", "version", "actions", "scenarios"}
    if unknown:
        raise ScenarioError(f"manifest has unknown keys: {sorted(unknown)}")
    version = raw.get("version")
    if (
        raw.get("schema") != SCHEMA
        or not isinstance(version, int)
        or isinstance(version, bool)
        or version != SCHEMA_VERSION
    ):
        raise ScenarioError("unsupported provider mock manifest schema/version")
    actions = raw.get("actions")
    if (
        not isinstance(actions, list)
        or len(actions) != len(ACTION_TYPES)
        or not all(isinstance(action, str) for action in actions)
        or len(set(actions)) != len(actions)
        or set(actions) != ACTION_TYPES
    ):
        raise ScenarioError("manifest actions must exactly match the v1 action catalog")
    raw_scenarios = raw.get("scenarios")
    if not isinstance(raw_scenarios, dict) or not raw_scenarios:
        raise ScenarioError("manifest scenarios must be a non-empty object")
    return {
        name: _scenario_from_mapping(name, scenario)
        for name, scenario in raw_scenarios.items()
    }


def scenario_from_steps(
    name: str,
    steps: Iterable[Mapping[str, Any]],
    description: str = "",
    phases: Optional[Iterable[str]] = None,
) -> Scenario:
    """Build a validated in-memory scenario for a targeted installed driver."""

    value: Dict[str, Any] = {"description": description, "steps": list(steps)}
    if phases is not None:
        value["phases"] = list(phases)
    return _scenario_from_mapping(name, value)


def _evaluate_checks(
    checks: Mapping[str, Any],
    headers: Mapping[str, str],
    raw_body: bytes,
    secrets: Mapping[str, str],
) -> Tuple[bool, Dict[str, Any]]:
    header_results: Dict[str, bool] = {}
    for name, policy in checks.get("headers", {}).items():
        value = headers.get(name)
        matched = True
        if "present" in policy:
            matched = matched and ((value is not None) == policy["present"])
        if "equals" in policy:
            matched = matched and value == policy["equals"]
        if "starts_with" in policy:
            matched = matched and value is not None and value.startswith(policy["starts_with"])
        if "contains" in policy:
            matched = matched and value is not None and policy["contains"] in value
        if "equals_secret" in policy:
            expected = secrets.get(policy["equals_secret"])
            matched = (
                matched
                and value is not None
                and expected is not None
                and hmac.compare_digest(value.encode("utf-8"), expected.encode("utf-8"))
            )
        if "bearer_secret" in policy:
            expected = secrets.get(policy["bearer_secret"])
            matched = (
                matched
                and value is not None
                and expected is not None
                and hmac.compare_digest(
                    value.encode("utf-8"), ("Bearer " + expected).encode("utf-8")
                )
            )
        header_results[name] = bool(matched)

    parsed: Any = _MISSING
    try:
        parsed = json.loads(raw_body) if raw_body else _MISSING
        json_valid = parsed is not _MISSING
    except (UnicodeDecodeError, json.JSONDecodeError):
        json_valid = False

    body_policy = checks.get("body", {})
    body_results: Dict[str, bool] = {}
    if "json" in body_policy:
        body_results["json"] = json_valid == body_policy["json"]
    for pointer in body_policy.get("required", []):
        body_results[f"required:{pointer}"] = (
            json_valid and _pointer_get(parsed, pointer) is not _MISSING
        )
    for pointer in body_policy.get("absent", []):
        body_results[f"absent:{pointer}"] = (
            not json_valid or _pointer_get(parsed, pointer) is _MISSING
        )
    for pointer, expected in body_policy.get("types", {}).items():
        value = _pointer_get(parsed, pointer) if json_valid else _MISSING
        body_results[f"type:{pointer}"] = (
            value is not _MISSING and _json_type(value) == expected
        )
    for pointer, expected in body_policy.get("equals", {}).items():
        value = _pointer_get(parsed, pointer) if json_valid else _MISSING
        body_results[f"equals:{pointer}"] = value is not _MISSING and value == expected

    shape_pointers = list(body_policy.get("shape", []))
    for source in ("required", "absent"):
        shape_pointers.extend(body_policy.get(source, []))
    shape_pointers.extend(body_policy.get("types", {}).keys())
    shape_pointers.extend(body_policy.get("equals", {}).keys())
    shape: Dict[str, Dict[str, Any]] = {}
    for pointer in dict.fromkeys(shape_pointers):
        value = _pointer_get(parsed, pointer) if json_valid else _MISSING
        shape[pointer] = {
            "present": value is not _MISSING,
            "type": None if value is _MISSING else _json_type(value),
        }

    matched = all(header_results.values()) and all(body_results.values())
    return matched, {
        "content_length": len(raw_body),
        "json": json_valid,
        "header_matches": header_results,
        "body_matches": body_results,
        "body_shape": shape,
    }


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def _write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        written = os.write(fd, view)
        if written <= 0:
            raise OSError("short evidence write")
        view = view[written:]


def _read_anonymous_fd(fd: int, label: str) -> bytes:
    """Read a bounded pipe/socket FD and close it; regular files are rejected."""

    try:
        info = os.fstat(fd)
        if not (stat.S_ISFIFO(info.st_mode) or stat.S_ISSOCK(info.st_mode)):
            raise ScenarioError(f"{label} must be supplied through an anonymous pipe/socket FD")
        chunks: List[bytes] = []
        total = 0
        while True:
            chunk = os.read(fd, min(8192, MAX_SECRET_BYTES + 1 - total))
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
            if total > MAX_SECRET_BYTES:
                raise ScenarioError(f"{label} payload is too large")
        return b"".join(chunks)
    finally:
        try:
            os.close(fd)
        except OSError:
            pass


def read_control_token_fd(fd: int) -> str:
    raw = _read_anonymous_fd(fd, "control token")
    try:
        token = raw.decode("utf-8").rstrip("\r\n")
    except UnicodeDecodeError:
        raise ScenarioError("control token must be UTF-8") from None
    if not (16 <= len(token) <= 1024) or any(ord(ch) < 33 for ch in token):
        raise ScenarioError("control token has invalid length or characters")
    return token


def read_secrets_fd(fd: int) -> Dict[str, str]:
    raw = _read_anonymous_fd(fd, "provider secrets")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise ScenarioError("provider secrets FD must contain a JSON object") from None
    if not isinstance(value, dict) or not value:
        raise ScenarioError("provider secrets FD must contain a non-empty JSON object")
    out: Dict[str, str] = {}
    for name, secret in value.items():
        name = _safe_label(name, "provider secret name")
        if (
            not isinstance(secret, str)
            or not secret
            or len(secret.encode("utf-8")) > 4096
            or "\x00" in secret
        ):
            raise ScenarioError(f"provider secret {name} has an invalid value")
        out[name] = secret
    return out


def _assert_no_symlink_ancestors(path: Path) -> None:
    if not path.is_absolute():
        raise ScenarioError("evidence directory must be an absolute path")
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current = current / part
        try:
            info = os.lstat(current)
        except FileNotFoundError:
            if current != path:
                raise ScenarioError("evidence directory parent does not exist") from None
            return
        if stat.S_ISLNK(info.st_mode):
            raise ScenarioError("evidence path cannot traverse a symlink")
        if current != path and not stat.S_ISDIR(info.st_mode):
            raise ScenarioError("evidence path parent is not a directory")
    raise ScenarioError("evidence directory must not already exist")


class EvidenceStore:
    """Owned 0700 evidence directory with durable, 0600, non-following files."""

    def __init__(self, path: Path, dir_fd: int, hits_fd: int):
        self.path = path
        self._dir_fd = dir_fd
        self._hits_fd: Optional[int] = hits_fd
        self._lock = threading.Lock()
        self._ready_written = False
        self._result_written = False
        self._closed = False

    @classmethod
    def create(cls, path: os.PathLike[str] | str) -> "EvidenceStore":
        evidence_path = Path(path)
        _assert_no_symlink_ancestors(evidence_path)
        parent_info = os.lstat(evidence_path.parent)
        if (
            not stat.S_ISDIR(parent_info.st_mode)
            or parent_info.st_uid != os.getuid()
            or stat.S_IMODE(parent_info.st_mode) != 0o700
        ):
            raise ScenarioError("evidence directory parent must be owned 0700")
        os.mkdir(evidence_path, 0o700)
        info = os.lstat(evidence_path)
        if (
            not stat.S_ISDIR(info.st_mode)
            or stat.S_ISLNK(info.st_mode)
            or info.st_uid != os.getuid()
        ):
            raise ScenarioError("failed to create an owned evidence directory")
        os.chmod(evidence_path, 0o700, follow_symlinks=False)
        if not hasattr(os, "O_DIRECTORY") or not hasattr(os, "O_NOFOLLOW"):
            raise ScenarioError("platform lacks required no-follow directory opens")
        flags = (
            os.O_RDONLY
            | os.O_DIRECTORY
            | os.O_NOFOLLOW
            | getattr(os, "O_CLOEXEC", 0)
        )
        dir_fd = os.open(evidence_path, flags)
        try:
            hits_fd = cls._open_new_regular(dir_fd, "hits.jsonl")
        except Exception:
            os.close(dir_fd)
            raise
        return cls(evidence_path, dir_fd, hits_fd)

    @property
    def result_written(self) -> bool:
        return self._result_written

    @staticmethod
    def _open_new_regular(dir_fd: int, name: str) -> int:
        if not hasattr(os, "O_NOFOLLOW"):
            raise ScenarioError("platform lacks required no-follow file opens")
        flags = (
            os.O_WRONLY
            | os.O_CREAT
            | os.O_EXCL
            | os.O_NOFOLLOW
            | getattr(os, "O_CLOEXEC", 0)
        )
        fd = os.open(name, flags, 0o600, dir_fd=dir_fd)
        try:
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                raise ScenarioError("evidence target is not a private regular file")
            os.fchmod(fd, 0o600)
            if stat.S_IMODE(os.fstat(fd).st_mode) != 0o600:
                raise ScenarioError("evidence file permissions are not 0600")
            return fd
        except Exception:
            os.close(fd)
            raise

    def _target_absent(self, name: str) -> None:
        try:
            os.stat(name, dir_fd=self._dir_fd, follow_symlinks=False)
        except FileNotFoundError:
            return
        raise ScenarioError(f"refusing pre-existing evidence target {name}")

    def _atomic_write_new(self, name: str, value: Mapping[str, Any]) -> None:
        with self._lock:
            if self._closed:
                raise ScenarioError("evidence store is closed")
            self._target_absent(name)
            temp_name = f".{name}.tmp-{secrets_module.token_hex(8)}"
            fd = self._open_new_regular(self._dir_fd, temp_name)
            try:
                _write_all(fd, _json_bytes(value) + b"\n")
                os.fsync(fd)
            finally:
                os.close(fd)
            try:
                self._target_absent(name)
                os.replace(
                    temp_name,
                    name,
                    src_dir_fd=self._dir_fd,
                    dst_dir_fd=self._dir_fd,
                )
                os.fsync(self._dir_fd)
            except Exception:
                try:
                    os.unlink(temp_name, dir_fd=self._dir_fd)
                except OSError:
                    pass
                raise

    def write_ready(self, value: Mapping[str, Any]) -> None:
        if self._ready_written:
            raise ScenarioError("ready evidence already written")
        self._atomic_write_new("ready.json", value)
        self._ready_written = True

    def append_hit(self, value: Mapping[str, Any]) -> None:
        encoded = _json_bytes(value) + b"\n"
        if len(encoded) > MAX_REQUEST_BODY:
            raise ScenarioError("redacted hit evidence is unexpectedly large")
        with self._lock:
            if self._closed:
                raise ScenarioError("evidence store is closed")
            if self._hits_fd is None:
                raise ScenarioError("evidence hit stream is closed")
            _write_all(self._hits_fd, encoded)
            os.fsync(self._hits_fd)

    def write_result(self, value: Mapping[str, Any]) -> None:
        if self._result_written:
            raise ScenarioError("result evidence already written")
        self._atomic_write_new("result.json", value)
        self._result_written = True

    def close_hits(self) -> None:
        with self._lock:
            if self._hits_fd is None:
                return
            os.fsync(self._hits_fd)
            os.close(self._hits_fd)
            self._hits_fd = None

    def close(self) -> None:
        with self._lock:
            if self._closed:
                return
            if self._hits_fd is not None:
                os.fsync(self._hits_fd)
                os.close(self._hits_fd)
                self._hits_fd = None
            os.close(self._dir_fd)
            self._closed = True


def _anthropic_body(action: Mapping[str, Any], dsml: bool = False) -> Dict[str, Any]:
    model = action.get("model", "mock-model")
    text = action.get("text", "mock anthropic ok")
    if dsml:
        text = action.get(
            "text",
            "<｜｜DSML｜｜tool_calls> "
            '<｜｜DSML｜｜invoke name="web_search">'
            '<｜｜DSML｜｜parameter name="query" string="true">mock-query'
            "</｜｜DSML｜｜parameter></｜｜DSML｜｜invoke> "
            "</｜｜DSML｜｜tool_calls>",
        )
    content: List[Dict[str, Any]] = [{"type": "text", "text": text}]
    if action.get("include_tool"):
        content.append(
            {
                "type": "tool_use",
                "id": "toolu_mock_1",
                "name": action.get("tool_name", "lookup"),
                "input": {"query": "mock-query"},
            }
        )
    return {
        "id": "msg_mock",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": "tool_use" if action.get("include_tool") else "end_turn",
        "stop_sequence": None,
        "usage": {"input_tokens": 2, "output_tokens": 3},
    }


def _anthropic_sse(action: Mapping[str, Any], dsml: bool = False) -> bytes:
    body = _anthropic_body(action, dsml=dsml)
    text = body["content"][0]["text"]
    events = [
        (
            "message_start",
            {
                "type": "message_start",
                "message": {
                    "id": body["id"],
                    "type": "message",
                    "role": "assistant",
                    "model": body["model"],
                    "content": [],
                    "stop_reason": None,
                    "usage": {"input_tokens": 2, "output_tokens": 0},
                },
            },
        ),
        (
            "content_block_start",
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            },
        ),
        (
            "content_block_delta",
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text},
            },
        ),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        (
            "message_delta",
            {
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                "usage": {"output_tokens": 3},
            },
        ),
        ("message_stop", {"type": "message_stop"}),
    ]
    return b"".join(
        b"event: " + event.encode("ascii") + b"\ndata: " + _json_bytes(data) + b"\n\n"
        for event, data in events
    )


def _kimi_sse() -> bytes:
    events = [
        {
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "server_tool_use", "name": "web_search"},
        },
        {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{}"},
        },
        {"type": "content_block_stop", "index": 0},
        {
            "type": "content_block_start",
            "index": 1,
            "content_block": {"type": "web_search_tool_result", "content": []},
        },
        {"type": "content_block_stop", "index": 1},
        {
            "type": "content_block_start",
            "index": 2,
            "content_block": {"type": "thinking", "thinking": "", "signature": ""},
        },
        {"type": "content_block_stop", "index": 2},
        {
            "type": "content_block_start",
            "index": 3,
            "content_block": {"type": "text", "text": ""},
        },
        {
            "type": "content_block_delta",
            "index": 3,
            "delta": {"type": "text_delta", "text": "mock kimi ok"},
        },
        {"type": "content_block_stop", "index": 3},
        {
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 3},
        },
        {"type": "message_stop"},
    ]
    return b"".join(b"data: " + _json_bytes(event) + b"\n\n" for event in events)


class RunningScenarioMock:
    """An owned in-process scenario server with deterministic stop/result APIs."""

    def __init__(
        self,
        scenario: Scenario,
        secrets: Optional[Mapping[str, str]] = None,
        evidence: Optional[EvidenceStore] = None,
    ):
        self.scenario = scenario
        self._secrets = dict(secrets or {})
        for name, value in self._secrets.items():
            _safe_label(name, "provider secret name")
            if not isinstance(value, str) or not value or "\x00" in value:
                raise ScenarioError(f"provider secret {name} has an invalid value")
        required_secrets = {
            policy[operator]
            for step in scenario.steps
            for policy in step.checks.get("headers", {}).values()
            for operator in ("equals_secret", "bearer_secret")
            if operator in policy
        }
        missing_secrets = required_secrets - set(self._secrets)
        if missing_secrets:
            raise ScenarioError(
                f"scenario requires missing in-memory secrets: {sorted(missing_secrets)}"
            )
        self._evidence = evidence
        self._lock = threading.RLock()
        self._stop_condition = threading.Condition(self._lock)
        self._stop_event = threading.Event()
        self._complete_event = threading.Event()
        self._server: Optional[_ScenarioHTTPServer] = None
        self._thread: Optional[threading.Thread] = None
        self._next_index = 0
        self._active_phase: Optional[str] = None
        self._entered_phase_index = -1
        self._inflight = False
        self._attempts: Dict[str, int] = {}
        self._requests: List[Dict[str, Any]] = []
        self._failures: List[Dict[str, Any]] = []
        self._terminal_failure = False
        self._stop_started = False
        self._stopped = False
        self._control_socket_name: Optional[str] = None

    @property
    def host(self) -> str:
        return "127.0.0.1"

    @property
    def port(self) -> int:
        if self._server is None:
            raise RuntimeError("scenario server is not started")
        return int(self._server.server_address[1])

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    @property
    def thread_alive(self) -> bool:
        return self._thread is not None and self._thread.is_alive()

    def start(self, port: int = 0) -> "RunningScenarioMock":
        """Bind an OS-selected non-8765 loopback port and start one server thread."""

        if port in FORBIDDEN_PORTS:
            raise ScenarioError(f"refusing forbidden test port {port}")
        if port != 0:
            raise ScenarioError("scenario mock requires an OS-selected dynamic port")
        with self._lock:
            if self._server is not None:
                raise RuntimeError("scenario server already started")
            handler = self._handler_type()
            server = bind_http_server(_ScenarioHTTPServer, handler)
            if server.server_address[1] in FORBIDDEN_PORTS:
                server.server_close()
                raise RuntimeError("dynamic bind selected a forbidden port")
            self._server = server
            self._thread = threading.Thread(
                target=server.serve_forever,
                name=f"provider-mock-{self.scenario.name}",
                daemon=False,
            )
            try:
                self._thread.start()
            except Exception:
                server.server_close()
                self._server = None
                self._thread = None
                raise
        return self

    def _handler_type(self):
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            server_version = "CSSwitchProviderMock/1"
            sys_version = ""

            def log_message(self, _format, *_args):
                return

            def setup(self):
                super().setup()
                # A peer that advertises a body and never sends it must not make
                # owned shutdown unbounded.
                self.connection.settimeout(2.0)

            def do_GET(self):
                owner._handle(self)

            def do_POST(self):
                owner._handle(self)

            def do_PUT(self):
                owner._handle(self)

            def do_PATCH(self):
                owner._handle(self)

            def do_DELETE(self):
                owner._handle(self)

            def do_OPTIONS(self):
                owner._handle(self)

            def do_HEAD(self):
                owner._handle(self)

            def do_CONNECT(self):
                owner._handle(self)

            def do_TRACE(self):
                owner._handle(self)

        return Handler

    def ready(self) -> Dict[str, Any]:
        with self._lock:
            value = {
                "schema": READY_SCHEMA,
                "scenario": self.scenario.name,
                "host": self.host,
                "port": self.port,
                "base_url": self.base_url,
                "owned_pid": os.getpid(),
                "executable_verified_by_driver": False,
                "port_closed_verified_by_driver": False,
                "phase": self._active_phase,
            }
            if self._control_socket_name is not None:
                value["control_socket"] = self._control_socket_name
            return value

    def ready_json(self) -> str:
        return json.dumps(self.ready(), ensure_ascii=False, sort_keys=True)

    def write_ready_evidence(self, control_socket: Optional[str] = None) -> Dict[str, Any]:
        if control_socket is not None:
            if Path(control_socket).name != control_socket:
                raise ScenarioError("control socket evidence must use a relative basename")
            self._control_socket_name = control_socket
        value = self.ready()
        if self._evidence is not None:
            self._evidence.write_ready(value)
        return value

    def enter_phase(self, phase: str) -> None:
        """Arm phases sequentially, including declared phases with no requests."""

        _safe_label(phase, "phase")
        with self._lock:
            if self._terminal_failure:
                raise RuntimeError("scenario already failed")
            if self._next_index >= len(self.scenario.steps):
                raise RuntimeError("scenario already complete")
            try:
                phase_index = self.scenario.phases.index(phase)
            except ValueError:
                raise ScenarioError("phase is not declared by the scenario") from None
            if phase == self._active_phase:
                return
            if phase_index != self._entered_phase_index + 1:
                raise ScenarioError("phases must be entered sequentially")
            next_step_phase = self.scenario.steps[self._next_index].phase
            next_step_index = self.scenario.phases.index(next_step_phase)
            if phase_index > next_step_index:
                raise ScenarioError("cannot skip a queued step from an earlier phase")
            if (
                self._active_phase is not None
                and self._active_phase == next_step_phase
            ):
                raise ScenarioError("current phase still has a queued step")
            self._active_phase = phase
            self._entered_phase_index = phase_index

    def _read_body(self, handler: BaseHTTPRequestHandler) -> Tuple[Optional[bytes], Optional[str]]:
        raw_length = handler.headers.get("Content-Length")
        if raw_length is None:
            return b"", None
        try:
            length = int(raw_length)
        except ValueError:
            return None, "invalid_content_length"
        if length < 0 or length > MAX_REQUEST_BODY:
            return None, "body_too_large"
        try:
            return handler.rfile.read(length), None
        except (ConnectionError, OSError):
            return None, "body_read_failed"

    def _terminal_reject(
        self,
        handler: BaseHTTPRequestHandler,
        kind: str,
        expected: Optional[ScenarioStep],
        method_match: bool = False,
        path_match: bool = False,
        phase_match: bool = False,
        status: int = 409,
        record_request: bool = True,
    ) -> None:
        request_record = {
            "kind": "rejected_request",
            "step": expected.step_id if expected else None,
            "phase": expected.phase if expected else None,
            "method_match": bool(method_match),
            "path_match": bool(path_match),
            "phase_match": bool(phase_match),
            "checks_match": None,
            "action": None,
            "outcome": ActionOutcome.FAILED.value,
        }
        with self._lock:
            self._terminal_failure = True
            self._inflight = False
            self._failures.append(
                {
                    "kind": kind,
                    "expected_step": expected.step_id if expected else None,
                    "expected_phase": expected.phase if expected else None,
                    "method_match": bool(method_match),
                    "path_match": bool(path_match),
                    "phase_match": bool(phase_match),
                }
            )
            if record_request:
                self._requests.append(request_record)
            self._complete_event.set()
        if record_request:
            try:
                self._append_hit(request_record)
            except ScenarioError:
                with self._lock:
                    self._failures.append(
                        {
                            "kind": "evidence_write_failed",
                            "expected_step": expected.step_id if expected else None,
                        }
                    )
        try:
            self._send_json(
                handler,
                status,
                {"error": "provider mock rejected request", "kind": kind},
                close=True,
            )
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    def _append_hit(self, value: Mapping[str, Any]) -> None:
        if self._evidence is not None:
            self._evidence.append_hit(value)

    def _handle(self, handler: BaseHTTPRequestHandler) -> None:
        method = handler.command.upper()
        with self._lock:
            if self._terminal_failure:
                record = {
                    "kind": "rejected_after_failure",
                    "step": None,
                    "phase": None,
                    "method_match": False,
                    "path_match": False,
                    "phase_match": False,
                    "checks_match": None,
                    "action": None,
                    "outcome": ActionOutcome.FAILED.value,
                }
                self._requests.append(record)
                try:
                    self._append_hit(record)
                except ScenarioError:
                    pass
                try:
                    self._send_json(
                        handler,
                        409,
                        {"error": "provider mock scenario already failed"},
                        close=True,
                    )
                except (BrokenPipeError, ConnectionResetError, OSError):
                    pass
                return
            if self._stop_started:
                record = {
                    "kind": "rejected_during_stop",
                    "step": None,
                    "phase": None,
                    "method_match": False,
                    "path_match": False,
                    "phase_match": False,
                    "checks_match": None,
                    "action": None,
                    "outcome": ActionOutcome.FAILED.value,
                }
                self._requests.append(record)
                try:
                    self._append_hit(record)
                except ScenarioError:
                    pass
                try:
                    self._send_json(
                        handler, 503, {"error": "provider mock stopping"}, close=True
                    )
                except (BrokenPipeError, ConnectionResetError, OSError):
                    pass
                return
            if self._next_index >= len(self.scenario.steps):
                self._terminal_reject(handler, "request_after_complete", None)
                return
            step = self.scenario.steps[self._next_index]
            method_match = method == step.method
            path_match = handler.path == step.path
            phase_match = self._active_phase == step.phase
            if self._inflight:
                self._terminal_reject(
                    handler,
                    "concurrent_request",
                    step,
                    method_match,
                    path_match,
                    phase_match,
                )
                return
            if not (method_match and path_match and phase_match):
                kind = "phase_not_entered" if not phase_match else "unexpected_request"
                self._terminal_reject(
                    handler,
                    kind,
                    step,
                    method_match,
                    path_match,
                    phase_match,
                )
                return
            self._inflight = True

        raw_body, body_error = self._read_body(handler)
        if body_error is not None or raw_body is None:
            self._terminal_reject(
                handler,
                body_error or "body_read_failed",
                step,
                True,
                True,
                True,
                status=413 if body_error == "body_too_large" else 400,
            )
            return
        headers = {name.lower(): value for name, value in handler.headers.items()}
        checks_ok, summary = _evaluate_checks(
            step.checks, headers, raw_body, self._secrets
        )
        if not checks_ok:
            record = {
                "kind": "matched_request",
                "step": step.step_id,
                "phase": step.phase,
                "method_match": True,
                "path_match": True,
                "phase_match": True,
                "checks_match": False,
                "attempt": self._attempts.get(step.step_id, 0) + 1,
                "request": summary,
                "action": None,
                "outcome": ActionOutcome.FAILED.value,
                "consumed": False,
            }
            with self._lock:
                self._requests.append(record)
            try:
                self._append_hit(record)
            except ScenarioError:
                with self._lock:
                    self._failures.append(
                        {"kind": "evidence_write_failed", "expected_step": step.step_id}
                    )
            self._terminal_reject(
                handler,
                "check_failed",
                step,
                True,
                True,
                True,
                status=422,
                record_request=False,
            )
            return

        with self._lock:
            attempt = self._attempts.get(step.step_id, 0) + 1
            self._attempts[step.step_id] = attempt
        action_kind = step.action["type"]
        outcome = ActionOutcome.FAILED
        failure_kind: Optional[str] = None
        try:
            outcome = self._dispatch_action(handler, step.action, attempt)
        except (BrokenPipeError, ConnectionResetError, OSError):
            handler.close_connection = True
            failure_kind = "response_write_failed"
        except Exception:
            failure_kind = "action_error"
            try:
                self._send_json(
                    handler, 500, {"error": "provider mock action failed"}, close=True
                )
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass
        finally:
            if outcome == ActionOutcome.FAILED and failure_kind is None:
                failure_kind = "action_failed"
            if failure_kind is not None:
                outcome = ActionOutcome.FAILED
                with self._lock:
                    self._terminal_failure = True
                    self._failures.append(
                        {
                            "kind": failure_kind,
                            "expected_step": step.step_id,
                            "expected_phase": step.phase,
                            "method_match": True,
                            "path_match": True,
                            "phase_match": True,
                        }
                    )
                    self._complete_event.set()
            consumed = outcome in {
                ActionOutcome.CONSUMED,
                ActionOutcome.EXPECTED_DROP,
            }
            record = {
                "kind": "matched_request",
                "step": step.step_id,
                "phase": step.phase,
                "method_match": True,
                "path_match": True,
                "phase_match": True,
                "checks_match": True,
                "attempt": attempt,
                "request": summary,
                "action": action_kind,
                "outcome": outcome.value,
                "consumed": consumed,
            }
            with self._lock:
                self._requests.append(record)
                if consumed and not self._terminal_failure:
                    self._next_index += 1
                    if self._next_index >= len(self.scenario.steps):
                        self._active_phase = None
                        self._complete_event.set()
                    else:
                        next_phase = self.scenario.steps[self._next_index].phase
                        if next_phase != step.phase:
                            self._active_phase = None
                self._inflight = False
            try:
                self._append_hit(record)
            except ScenarioError:
                with self._lock:
                    self._terminal_failure = True
                    self._failures.append(
                        {"kind": "evidence_write_failed", "expected_step": step.step_id}
                    )
                    self._complete_event.set()

    def _dispatch_action(
        self, handler: BaseHTTPRequestHandler, action: Mapping[str, Any], attempt: int
    ) -> ActionOutcome:
        kind = action["type"]
        if kind == "anthropic_json":
            self._send_json(handler, 200, _anthropic_body(action))
        elif kind == "anthropic_sse":
            if not self._send_sse(
                handler, _anthropic_sse(action), float(action.get("chunk_delay", 0))
            ):
                return ActionOutcome.RETRY
        elif kind == "dsml":
            if action.get("stream"):
                if not self._send_sse(
                    handler,
                    _anthropic_sse(action, dsml=True),
                    float(action.get("chunk_delay", 0)),
                ):
                    return ActionOutcome.RETRY
            else:
                self._send_json(handler, 200, _anthropic_body(action, dsml=True))
        elif kind == "openai_chat_text_tool":
            content = action.get("text", "mock openai chat ok")
            message: Dict[str, Any] = {"role": "assistant", "content": content}
            if action.get("include_tool", True):
                message["tool_calls"] = [
                    {
                        "id": "call_mock_1",
                        "type": "function",
                        "function": {
                            "name": action.get("tool_name", "lookup"),
                            "arguments": "{\"query\":\"mock-query\"}",
                        },
                    }
                ]
            self._send_json(
                handler,
                200,
                {
                    "id": "chatcmpl_mock",
                    "object": "chat.completion",
                    "model": action.get("model", "mock-model"),
                    "choices": [
                        {
                            "index": 0,
                            "message": message,
                            "finish_reason": "tool_calls"
                            if action.get("include_tool", True)
                            else "stop",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 2,
                        "completion_tokens": 3,
                        "total_tokens": 5,
                    },
                },
            )
        elif kind == "openai_responses_text_tool":
            output: List[Dict[str, Any]] = [
                {
                    "id": "msg_mock",
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": action.get("text", "mock responses ok"),
                        }
                    ],
                }
            ]
            if action.get("include_tool", True):
                output.append(
                    {
                        "id": "fc_mock_1",
                        "type": "function_call",
                        "call_id": "call_mock_1",
                        "name": action.get("tool_name", "lookup"),
                        "arguments": "{\"query\":\"mock-query\"}",
                    }
                )
            self._send_json(
                handler,
                200,
                {
                    "id": "resp_mock",
                    "object": "response",
                    "status": "completed",
                    "model": action.get("model", "mock-model"),
                    "output": output,
                    "usage": {
                        "input_tokens": 2,
                        "output_tokens": 3,
                        "total_tokens": 5,
                    },
                },
            )
        elif kind == "kimi_sse":
            if not self._send_sse(
                handler, _kimi_sse(), float(action.get("chunk_delay", 0))
            ):
                return ActionOutcome.RETRY
        elif kind == "models_json":
            model_ids = action.get("models", ["mock-model", "mock-tool-model"])
            body = {
                "object": "list",
                "data": [
                    {
                        "object": "model",
                        "id": model_id,
                        "display_name": model_id,
                        "supported_parameters": ["tools"],
                    }
                    for model_id in model_ids
                ]
            }
            self._send_json(handler, 200, body)
        elif kind == "status":
            status = action.get("status", 200)
            body = action.get(
                "json",
                {
                    "type": "error" if status >= 400 else "mock_status",
                    "status": status,
                },
            )
            self._send_json(handler, status, body)
        elif kind == "malformed_json":
            self._send_bytes(handler, 200, "application/json", b'{"malformed":')
        elif kind == "drop_before_headers":
            self._drop_connection(handler)
            return ActionOutcome.EXPECTED_DROP
        elif kind == "drop_then_success":
            if attempt <= int(action.get("drops", 1)):
                self._drop_connection(handler)
                return ActionOutcome.RETRY
            return self._dispatch_action(handler, action["then"], attempt)
        elif kind in {"delay", "stall"}:
            seconds = float(action.get("seconds", 0.1 if kind == "delay" else 30.0))
            if self._stop_event.wait(seconds):
                self._drop_connection(handler)
                return ActionOutcome.RETRY
            else:
                return self._dispatch_action(handler, action["then"], attempt)
        elif kind == "truncated":
            prefix = b'{"partial":'
            declared = len(prefix) + int(action.get("missing_bytes", 32))
            handler.send_response(action.get("status", 200))
            handler.send_header("Content-Type", action.get("content_type", "application/json"))
            handler.send_header("Content-Length", str(declared))
            handler.send_header("Connection", "close")
            handler.end_headers()
            if handler.command != "HEAD":
                handler.wfile.write(prefix)
                handler.wfile.flush()
            self._drop_connection(handler)
            return ActionOutcome.EXPECTED_DROP
        else:  # Defensive; manifest validation should make this unreachable.
            raise ScenarioError("unsupported action")
        return ActionOutcome.CONSUMED

    def _send_sse(
        self, handler: BaseHTTPRequestHandler, body: bytes, chunk_delay: float
    ) -> bool:
        handler.send_response(200)
        handler.send_header("Content-Type", "text/event-stream")
        handler.send_header("Content-Length", str(len(body)))
        handler.end_headers()
        if handler.command == "HEAD":
            return True
        chunks = [part + b"\n\n" for part in body.split(b"\n\n") if part]
        for index, chunk in enumerate(chunks):
            handler.wfile.write(chunk)
            handler.wfile.flush()
            if chunk_delay and index + 1 < len(chunks):
                if self._stop_event.wait(chunk_delay):
                    self._drop_connection(handler)
                    return False
        return True

    @staticmethod
    def _send_json(
        handler: BaseHTTPRequestHandler, status: int, value: Any, close: bool = False
    ) -> None:
        RunningScenarioMock._send_bytes(
            handler, status, "application/json", _json_bytes(value), close=close
        )

    @staticmethod
    def _send_bytes(
        handler: BaseHTTPRequestHandler,
        status: int,
        content_type: str,
        body: bytes,
        close: bool = False,
    ) -> None:
        handler.send_response(status)
        handler.send_header("Content-Type", content_type)
        handler.send_header("Content-Length", str(len(body)))
        if close:
            handler.send_header("Connection", "close")
            handler.close_connection = True
        handler.end_headers()
        if handler.command != "HEAD":
            handler.wfile.write(body)
            handler.wfile.flush()

    @staticmethod
    def _drop_connection(handler: BaseHTTPRequestHandler) -> None:
        handler.close_connection = True
        try:
            handler.connection.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            handler.connection.close()
        except OSError:
            pass

    def wait_complete(self, timeout: Optional[float] = None) -> bool:
        return self._complete_event.wait(timeout)

    def join(self, timeout: float = 5.0) -> bool:
        thread = self._thread
        if thread is None:
            return True
        thread.join(timeout)
        return not thread.is_alive()

    def stop(
        self, timeout: float = 5.0, write_evidence: bool = True
    ) -> Dict[str, Any]:
        """Idempotently stop, close, join, and return a redacted snapshot."""

        deadline = time.monotonic() + timeout
        with self._stop_condition:
            if self._stopped:
                result = self.result()
                if (
                    write_evidence
                    and self._evidence is not None
                    and not self._evidence.result_written
                ):
                    self._evidence.write_result(result)
                return result
            if self._stop_started:
                while not self._stopped:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise RuntimeError("provider mock stop did not finish")
                    self._stop_condition.wait(remaining)
                return self.result()
            self._stop_started = True
            self._stop_event.set()
            server = self._server
        if server is not None:
            server.shutdown()
            server.server_close()
        remaining = max(0.0, deadline - time.monotonic())
        if not self.join(remaining):
            with self._lock:
                self._terminal_failure = True
                self._failures.append({"kind": "server_thread_join_failed"})
            raise RuntimeError("provider mock server thread did not stop")
        with self._stop_condition:
            self._stopped = True
            self._stop_condition.notify_all()
        result = self.result()
        if (
            write_evidence
            and self._evidence is not None
            and not self._evidence.result_written
        ):
            self._evidence.write_result(result)
        return result

    def result(self) -> Dict[str, Any]:
        with self._lock:
            queue_complete = self._next_index == len(self.scenario.steps)
            protocol_complete = queue_complete and not self._terminal_failure
            server_thread_alive = self.thread_alive
            final_ok = protocol_complete and self._stopped and not server_thread_alive
            value = {
                "schema": RESULT_SCHEMA,
                "scenario": self.scenario.name,
                "owned": {
                    "pid": os.getpid(),
                    "host": self.host,
                    "port": self.port if self._server is not None else None,
                    "executable_verified_by_driver": False,
                    "port_closed_verified_by_driver": False,
                },
                "stop_started": self._stop_started,
                "stopped": self._stopped,
                "server_thread_alive": server_thread_alive,
                "queue_complete": queue_complete,
                "protocol_complete": protocol_complete,
                "complete": queue_complete,
                "final_ok": final_ok,
                "ok": final_ok,
                "active_phase": self._active_phase,
                "next_step": (
                    self.scenario.steps[self._next_index].step_id
                    if self._next_index < len(self.scenario.steps)
                    else None
                ),
                "requests": copy.deepcopy(self._requests),
                "failures": copy.deepcopy(self._failures),
            }
        return value

    def assert_success(self) -> Dict[str, Any]:
        result = self.result()
        if not result["final_ok"]:
            raise AssertionError(
                "provider mock scenario incomplete or failed: "
                + json.dumps(result, ensure_ascii=False, sort_keys=True)
            )
        return result

    def __enter__(self) -> "RunningScenarioMock":
        if self._server is None:
            self.start()
        return self

    def __exit__(self, _exc_type, _exc, _traceback) -> None:
        self.stop()


def start_scenario(
    scenario: Scenario,
    port: int = 0,
    secrets: Optional[Mapping[str, str]] = None,
    evidence: Optional[EvidenceStore] = None,
) -> RunningScenarioMock:
    return RunningScenarioMock(scenario, secrets=secrets, evidence=evidence).start(port=port)


def start_manifest_scenario(
    name: str,
    manifest_path: os.PathLike[str] | str = DEFAULT_MANIFEST,
    port: int = 0,
    secrets: Optional[Mapping[str, str]] = None,
    evidence: Optional[EvidenceStore] = None,
) -> RunningScenarioMock:
    scenarios = load_manifest(manifest_path)
    if name not in scenarios:
        raise ScenarioError(f"unknown scenario: {name}")
    return start_scenario(
        scenarios[name], port=port, secrets=secrets, evidence=evidence
    )


class OwnedTCPEcho:
    """Owned raw loopback echo used only for gateway CONNECT round trips."""

    def __init__(self):
        self._listener: Optional[socket.socket] = None
        self._port: Optional[int] = None
        self._thread: Optional[threading.Thread] = None
        self._workers: List[threading.Thread] = []
        self._connections: List[socket.socket] = []
        self._lock = threading.Lock()
        self._stop_event = threading.Event()
        self._round_trips = 0
        self._stopped = False

    @property
    def host(self) -> str:
        return "127.0.0.1"

    @property
    def port(self) -> int:
        if self._port is None:
            raise RuntimeError("owned echo is not started")
        return self._port

    def start(self) -> "OwnedTCPEcho":
        if self._listener is not None:
            raise RuntimeError("owned echo already started")
        listener = bind_loopback_listener()
        if listener.getsockname()[1] in FORBIDDEN_PORTS:
            listener.close()
            raise RuntimeError("owned echo selected a forbidden port")
        listener.settimeout(0.2)
        self._listener = listener
        self._port = int(listener.getsockname()[1])
        self._thread = threading.Thread(
            target=self._accept_loop, name="provider-mock-owned-echo", daemon=False
        )
        self._thread.start()
        return self

    def _accept_loop(self) -> None:
        assert self._listener is not None
        while not self._stop_event.is_set():
            try:
                connection, _address = self._listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            connection.settimeout(0.2)
            worker = threading.Thread(
                target=self._echo_connection,
                args=(connection,),
                name="provider-mock-owned-echo-connection",
                daemon=False,
            )
            with self._lock:
                self._connections.append(connection)
                self._workers.append(worker)
            worker.start()

    def _echo_connection(self, connection: socket.socket) -> None:
        round_trip = False
        try:
            while not self._stop_event.is_set():
                try:
                    payload = connection.recv(65_536)
                except socket.timeout:
                    continue
                if not payload:
                    break
                connection.sendall(payload)
                round_trip = True
        except OSError:
            pass
        finally:
            try:
                connection.close()
            except OSError:
                pass
            with self._lock:
                if round_trip:
                    self._round_trips += 1
                if connection in self._connections:
                    self._connections.remove(connection)

    def ready(self) -> Dict[str, Any]:
        return {
            "schema": "csswitch.provider-mock-owned-echo-ready.v1",
            "host": self.host,
            "port": self.port,
            "owned_pid": os.getpid(),
            "executable_verified_by_driver": False,
            "port_closed_verified_by_driver": False,
        }

    def stop(self, timeout: float = 5.0) -> Dict[str, Any]:
        if self._stopped:
            return self.result()
        self._stop_event.set()
        listener = self._listener
        if listener is not None:
            listener.close()
        with self._lock:
            connections = list(self._connections)
        for connection in connections:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                connection.close()
            except OSError:
                pass
        deadline = time.monotonic() + timeout
        if self._thread is not None:
            self._thread.join(max(0, deadline - time.monotonic()))
            if self._thread.is_alive():
                raise RuntimeError("owned echo accept thread did not stop")
        with self._lock:
            workers = list(self._workers)
        for worker in workers:
            worker.join(max(0, deadline - time.monotonic()))
            if worker.is_alive():
                raise RuntimeError("owned echo worker did not stop")
        self._stopped = True
        return self.result()

    def result(self) -> Dict[str, Any]:
        thread_alive = self._thread is not None and self._thread.is_alive()
        with self._lock:
            worker_alive = any(worker.is_alive() for worker in self._workers)
            round_trips = self._round_trips
        return {
            "schema": "csswitch.provider-mock-owned-echo-result.v1",
            "owned": {
                "pid": os.getpid(),
                "host": self.host,
                "port": self._port,
                "executable_verified_by_driver": False,
                "port_closed_verified_by_driver": False,
            },
            "round_trips": round_trips,
            "stopped": self._stopped,
            "thread_alive": thread_alive,
            "worker_alive": worker_alive,
            "final_ok": self._stopped
            and not thread_alive
            and not worker_alive
            and round_trips > 0,
        }

    def __enter__(self) -> "OwnedTCPEcho":
        return self.start()

    def __exit__(self, _exc_type, _exc, _traceback) -> None:
        self.stop()


def start_owned_tcp_echo() -> OwnedTCPEcho:
    return OwnedTCPEcho().start()


class _ThreadingUnixServer(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = False
    block_on_close = True

    def handle_error(self, _request, _client_address):
        return


class ScenarioControlPlane:
    """Authenticated local control socket; the token remains memory-only."""

    def __init__(
        self,
        mock: RunningScenarioMock,
        socket_path: Path,
        token: str,
        stop_requested: threading.Event,
    ):
        self.mock = mock
        self.socket_path = socket_path
        self._token = token
        self._stop_requested = stop_requested
        self._thread: Optional[threading.Thread] = None
        self._server: Optional[_ThreadingUnixServer] = None

    def start(self) -> "ScenarioControlPlane":
        if (
            self.mock._evidence is None
            or self.socket_path.parent != self.mock._evidence.path
        ):
            raise ScenarioError("control socket must live in the owned evidence directory")
        try:
            os.lstat(self.socket_path)
        except FileNotFoundError:
            pass
        else:
            raise ScenarioError("control socket target already exists")
        owner = self

        class Handler(socketserver.StreamRequestHandler):
            def setup(self):
                super().setup()
                self.connection.settimeout(2.0)

            def handle(self):
                raw = self.rfile.readline(MAX_CONTROL_MESSAGE + 1)
                if not raw or len(raw) > MAX_CONTROL_MESSAGE or not raw.endswith(b"\n"):
                    owner._reply(self.wfile, {"ok": False, "error": "invalid_control_request"})
                    return
                try:
                    request = json.loads(raw)
                except (UnicodeDecodeError, json.JSONDecodeError):
                    owner._reply(self.wfile, {"ok": False, "error": "invalid_control_request"})
                    return
                owner._handle_control(self.wfile, request)

        server = _ThreadingUnixServer(str(self.socket_path), Handler)
        self._server = server
        try:
            os.chmod(self.socket_path, 0o600, follow_symlinks=False)
            info = os.lstat(self.socket_path)
            if (
                not stat.S_ISSOCK(info.st_mode)
                or stat.S_ISLNK(info.st_mode)
                or info.st_uid != os.getuid()
                or stat.S_IMODE(info.st_mode) != 0o600
            ):
                raise ScenarioError("control socket is not an owned 0600 Unix socket")
            self._thread = threading.Thread(
                target=server.serve_forever,
                name=f"provider-mock-control-{self.mock.scenario.name}",
                daemon=False,
            )
            self._thread.start()
            return self
        except Exception:
            self.force_cleanup()
            raise

    @staticmethod
    def _reply(writer, value: Mapping[str, Any]) -> None:
        writer.write(_json_bytes(value) + b"\n")
        writer.flush()

    def _authenticated(self, supplied: Any) -> bool:
        return isinstance(supplied, str) and hmac.compare_digest(
            supplied.encode("utf-8"), self._token.encode("utf-8")
        )

    def _handle_control(self, writer, request: Any) -> None:
        if not isinstance(request, dict):
            self._reply(writer, {"ok": False, "error": "invalid_control_request"})
            return
        if not self._authenticated(request.get("token")):
            self._reply(writer, {"ok": False, "error": "unauthorized"})
            return
        command = request.get("command")
        allowed_keys = {
            "enter_phase": {"token", "command", "phase"},
            "status": {"token", "command"},
            "wait": {"token", "command", "timeout_ms"},
            "stop": {"token", "command"},
        }
        if command not in allowed_keys or set(request) != allowed_keys[command]:
            self._reply(writer, {"ok": False, "error": "invalid_control_request"})
            return
        if command == "enter_phase":
            try:
                self.mock.enter_phase(request["phase"])
            except (ScenarioError, RuntimeError):
                self._reply(writer, {"ok": False, "error": "phase_rejected"})
                return
            self._reply(writer, {"ok": True, "status": self.mock.result()})
            return
        if command == "status":
            self._reply(writer, {"ok": True, "status": self.mock.result()})
            return
        if command == "wait":
            timeout_ms = request["timeout_ms"]
            if (
                not isinstance(timeout_ms, int)
                or isinstance(timeout_ms, bool)
                or not (0 <= timeout_ms <= 600_000)
            ):
                self._reply(writer, {"ok": False, "error": "invalid_timeout"})
                return
            deadline = time.monotonic() + timeout_ms / 1000
            completed = self.mock.wait_complete(0)
            while not completed and not self._stop_requested.is_set():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                completed = self.mock.wait_complete(min(remaining, 0.1))
            self._reply(
                writer,
                {
                    "ok": True,
                    "completed": completed,
                    "status": self.mock.result(),
                },
            )
            return
        self._stop_requested.set()
        self._reply(writer, {"ok": True, "accepted": True})

    def stop(self, timeout: float = 5.0) -> None:
        self.force_cleanup(timeout)

    def force_cleanup(self, timeout: float = 5.0) -> None:
        """Best-effort owned cleanup used even when the public stop path fails."""

        server = self._server
        if server is None:
            try:
                os.unlink(self.socket_path)
            except FileNotFoundError:
                pass
            return
        self._stop_requested.set()
        cleanup_error: Optional[Exception] = None
        if self._thread is not None and self._thread.is_alive():
            try:
                server.shutdown()
            except Exception as error:
                cleanup_error = error
        try:
            server.server_close()
        except Exception as error:
            if cleanup_error is None:
                cleanup_error = error
        if self._thread is not None and self._thread.ident is not None:
            self._thread.join(timeout)
            if self._thread.is_alive() and cleanup_error is None:
                cleanup_error = RuntimeError("provider mock control thread did not stop")
        try:
            os.unlink(self.socket_path)
        except FileNotFoundError:
            pass
        except Exception as error:
            if cleanup_error is None:
                cleanup_error = error
        self._server = None
        if cleanup_error is not None:
            raise cleanup_error


def _failed_cli_result(scenario: str) -> Dict[str, Any]:
    return {
        "schema": RESULT_SCHEMA,
        "scenario": scenario,
        "owned": {
            "pid": os.getpid(),
            "host": "127.0.0.1",
            "port": None,
            "executable_verified_by_driver": False,
            "port_closed_verified_by_driver": False,
        },
        "stop_started": False,
        "stopped": True,
        "server_thread_alive": False,
        "queue_complete": False,
        "protocol_complete": False,
        "complete": False,
        "final_ok": False,
        "ok": False,
        "active_phase": None,
        "next_step": None,
        "requests": [],
        "failures": [{"kind": "cli_start_or_cleanup_failed"}],
    }


def _mark_cli_result_failed(value: Mapping[str, Any]) -> Dict[str, Any]:
    result = copy.deepcopy(dict(value))
    result["final_ok"] = False
    result["ok"] = False
    failures = result.setdefault("failures", [])
    if not any(failure.get("kind") == "cli_start_or_cleanup_failed" for failure in failures):
        failures.append({"kind": "cli_start_or_cleanup_failed"})
    return result


def _main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--scenario")
    parser.add_argument("--list", action="store_true", dest="list_scenarios")
    parser.add_argument("--evidence-dir", type=Path)
    parser.add_argument("--control-token-fd", type=int)
    parser.add_argument("--secrets-fd", type=int)
    args = parser.parse_args(argv)

    scenarios = load_manifest(args.manifest)
    if args.list_scenarios:
        print(json.dumps({"scenarios": sorted(scenarios)}, sort_keys=True), flush=True)
        return 0
    if not args.scenario:
        parser.error("--scenario is required unless --list is used")
    if args.scenario not in scenarios:
        parser.error(f"unknown scenario: {args.scenario}")
    if args.evidence_dir is None:
        parser.error("--evidence-dir is required")
    if args.control_token_fd is None:
        parser.error("--control-token-fd is required")

    token = read_control_token_fd(args.control_token_fd)
    provider_secrets = read_secrets_fd(args.secrets_fd) if args.secrets_fd is not None else {}
    done = threading.Event()
    evidence: Optional[EvidenceStore] = None
    mock: Optional[RunningScenarioMock] = None
    control: Optional[ScenarioControlPlane] = None
    installed_signal_handlers: Dict[int, Any] = {}
    result: Optional[Dict[str, Any]] = None
    primary_error: Optional[Exception] = None
    cleanup_errors: List[Exception] = []

    def request_stop(_signum, _frame):
        done.set()

    try:
        evidence = EvidenceStore.create(args.evidence_dir)
        mock = RunningScenarioMock(
            scenarios[args.scenario], secrets=provider_secrets, evidence=evidence
        )
        mock.start()
        control = ScenarioControlPlane(
            mock, evidence.path / "control.sock", token, done
        )
        control.start()
        for signum in (signal.SIGINT, signal.SIGTERM):
            installed_signal_handlers[signum] = signal.getsignal(signum)
            signal.signal(signum, request_stop)
        ready = mock.write_ready_evidence("control.sock")
        print(json.dumps(ready, ensure_ascii=False, sort_keys=True), flush=True)
        done.wait()
    except Exception as error:
        primary_error = error
    finally:
        for signum, previous in reversed(list(installed_signal_handlers.items())):
            try:
                signal.signal(signum, previous)
            except Exception as error:
                cleanup_errors.append(error)
        if control is not None:
            try:
                control.stop()
            except Exception as error:
                cleanup_errors.append(error)
                try:
                    control.force_cleanup()
                except Exception as force_error:
                    cleanup_errors.append(force_error)
            control_thread_alive = (
                control._thread is not None and control._thread.is_alive()
            )
            if control_thread_alive or os.path.lexists(control.socket_path):
                cleanup_errors.append(
                    RuntimeError("provider mock control cleanup is incomplete")
                )
        if mock is not None:
            try:
                result = mock.stop(write_evidence=False)
            except Exception as error:
                cleanup_errors.append(error)
                result = mock.result()
        if evidence is not None:
            try:
                evidence.close_hits()
            except Exception as error:
                cleanup_errors.append(error)
        if result is None and evidence is not None:
            result = _failed_cli_result(args.scenario)
        if result is not None and (primary_error is not None or cleanup_errors):
            result = _mark_cli_result_failed(result)
        output_error: Optional[Exception] = None
        if result is not None:
            try:
                print(json.dumps(result, ensure_ascii=False, sort_keys=True), flush=True)
            except Exception as error:
                output_error = error
                result = _mark_cli_result_failed(result)
        if evidence is not None and result is not None:
            try:
                evidence.write_result(result)
            except Exception as error:
                cleanup_errors.append(error)
        if evidence is not None:
            try:
                evidence.close()
            except Exception as error:
                cleanup_errors.append(error)
        if primary_error is not None:
            raise primary_error
        if cleanup_errors:
            raise cleanup_errors[0]
        if output_error is not None:
            raise output_error
    assert result is not None
    return 0 if result["final_ok"] else 1


if __name__ == "__main__":
    sys.exit(_main())
