#!/usr/bin/env python3
"""Isolated v3 -> v4 coverage-install Acceptance for provider model catalogs.

The caller supplies an old schema-v3 Acceptance bundle and the current
Acceptance bundle. Both use the same run-unique Acceptance identity. The test installs
the old bundle under a private temporary root, launches only that owned binary
against a fake v3 Qwen profile, replaces the same installation path with the
new bundle, and reuses the same temporary HOME.

No /Applications bundle, real credential store, or real Science data directory
is read or modified. Every process stopped by this script was created by this
script and is addressed through its exact Popen handle or the existing
strong-identity fake-Science controller.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import plistlib
import signal
import socket
import stat
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Callable, Dict, Optional

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from test.installed_provider_matrix import (
    FAKE_API_KEY,
    FIXED_PATH_SECRET,
    InstalledProviderSession,
    ProcessInspector,
    _read_bundle_info,
    _safe_json_write,
)


ACCEPTANCE_BUNDLE_ID = "com.csswitch.acceptance.modelcatalog"
APP_NAME = "CSSwitch Model Catalog Acceptance.app"
PROFILE_ID = "coverage-qwen-v3"


class AcceptanceFailure(RuntimeError):
    pass


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def require_isolated_root(path: Path) -> Path:
    if path.exists() and path.is_symlink():
        raise AcceptanceFailure("acceptance root must not be a symlink")
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    path.chmod(0o700)
    resolved = path.resolve(strict=True)
    real_home = Path(os.path.expanduser("~")).resolve(strict=True)
    if resolved == real_home or real_home in resolved.parents:
        raise AcceptanceFailure("acceptance root resolves inside the real HOME")
    safe_roots = [Path("/private/tmp").resolve(strict=True), Path("/tmp").resolve(strict=True)]
    if not any(resolved == safe or safe in resolved.parents for safe in safe_roots):
        raise AcceptanceFailure("coverage-install root must be under /private/tmp or /tmp")
    return resolved


def require_acceptance_bundle(
    path: Path, expected_bundle_id: str = ACCEPTANCE_BUNDLE_ID
) -> Dict[str, Any]:
    resolved = path.resolve(strict=True)
    if Path("/Applications") in resolved.parents:
        raise AcceptanceFailure("refusing an /Applications bundle")
    info = _read_bundle_info(resolved)
    if info.get("CFBundleIdentifier") != expected_bundle_id:
        raise AcceptanceFailure("bundle identifier is not the isolated model-catalog Acceptance ID")
    if info.get("CFBundleExecutable") != "desktop":
        raise AcceptanceFailure("unexpected Acceptance executable")
    for relative in ("Contents/MacOS/desktop", "Contents/MacOS/csswitch-gateway"):
        executable = resolved / relative
        if not executable.is_file() or not os.access(executable, os.X_OK):
            raise AcceptanceFailure(f"missing executable: {relative}")
    return info


def ditto_bundle(source: Path, destination: Path) -> None:
    if destination.exists() or destination.is_symlink():
        raise AcceptanceFailure(f"destination already exists: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    subprocess.run(
        ["/usr/bin/ditto", "--rsrc", str(source), str(destination)],
        check=True,
        timeout=120,
    )


def terminate_owned(pid: Optional[int], binary: Path, label: str) -> None:
    if pid is None:
        return
    inspector = ProcessInspector()
    if not inspector.pid_alive(pid):
        return
    expected = binary.resolve(strict=True)
    if inspector.executable_for_pid(pid) != expected:
        raise AcceptanceFailure(f"refusing to stop {label}: executable identity changed")
    os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline and inspector.pid_alive(pid):
        time.sleep(0.1)
    if inspector.pid_alive(pid):
        if inspector.executable_for_pid(pid) != expected:
            raise AcceptanceFailure(f"refusing SIGKILL for {label}: identity changed")
        os.kill(pid, signal.SIGKILL)
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline and inspector.pid_alive(pid):
            time.sleep(0.1)
    if inspector.pid_alive(pid):
        raise AcceptanceFailure(f"owned {label} process did not stop")


def crash_owned_app(pid: int, binary: Path) -> None:
    """Simulate an interrupted upgrade for this exact test-owned app only."""
    inspector = ProcessInspector()
    expected = binary.resolve(strict=True)
    if pid <= 1 or not inspector.pid_alive(pid):
        raise AcceptanceFailure("old Acceptance crash target is not alive")
    if inspector.executable_for_pid(pid) != expected:
        raise AcceptanceFailure("refusing crash simulation: old app identity changed")
    os.kill(pid, signal.SIGKILL)
    wait_until(
        lambda: not inspector.pid_alive(pid),
        timeout=4,
        description="old Acceptance crash simulation",
    )


def wait_until(
    predicate: Callable[[], Optional[Any]],
    *,
    timeout: float,
    description: str,
) -> Any:
    deadline = time.monotonic() + timeout
    last_error: Optional[Exception] = None
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except Exception as error:  # bounded polling preserves the final cause
            last_error = error
        time.sleep(0.1)
    suffix = f": {last_error}" if last_error else ""
    raise AcceptanceFailure(f"timed out waiting for {description}{suffix}")


def launch_owned(
    bundle: Path,
    binary: Path,
    env: Dict[str, str],
    stdout_path: Path,
    stderr_path: Path,
) -> int:
    inspector = ProcessInspector()
    expected = binary.resolve(strict=True)
    before = [
        record.pid
        for record in inspector.process_table()
        if inspector.executable_for_pid(record.pid) == expected
    ]
    if before:
        raise AcceptanceFailure("exact temporary Acceptance bundle is already running")
    argv = [
        "/usr/bin/open",
        "-n",
        "-F",
        "-g",
        "--stdout",
        str(stdout_path),
        "--stderr",
        str(stderr_path),
    ]
    for name in sorted(env):
        argv.extend(["--env", f"{name}={env[name]}"])
    argv.append(str(bundle))
    completed = subprocess.run(argv, check=False, capture_output=True, timeout=20)
    if completed.returncode != 0:
        raise AcceptanceFailure(
            "LaunchServices rejected temporary Acceptance: "
            + completed.stderr.decode("utf-8", "replace")[:1000]
        )

    def exact_pid() -> Optional[int]:
        matches = [
            record.pid
            for record in inspector.process_table()
            if inspector.executable_for_pid(record.pid) == expected
        ]
        if len(matches) > 1:
            raise AcceptanceFailure("multiple exact temporary Acceptance processes observed")
        return matches[0] if matches else None

    pid = wait_until(exact_pid, timeout=8, description="exact Acceptance process")
    if inspector.executable_for_pid(pid) != expected:
        raise AcceptanceFailure("launched PID executable identity mismatch")
    return pid


def cleanup_owned_runtime(session: InstalledProviderSession) -> None:
    """Best-effort cleanup restricted to exact test identities and ports."""
    try:
        science = session.inspect_fake_science()
        if science.get("identity_verified"):
            session.stop_fake_science()
    except Exception:
        pass
    try:
        health = session.inspect_health()
        if not health.get("ok"):
            return
        matches = []
        expected = session.gateway_bin.resolve(strict=True)
        for record in session.inspector.process_table():
            executable = session.inspector.executable_for_pid(record.pid)
            if executable == expected and session.inspector.listener_owned(
                record.pid, session.proxy_port
            ):
                matches.append(record.pid)
        if len(matches) == 1 and matches[0] > 1:
            os.kill(matches[0], signal.SIGTERM)
            wait_until(
                lambda: not session.inspector.listener_owned(matches[0], session.proxy_port),
                timeout=4,
                description="owned Gateway cleanup",
            )
    except Exception:
        pass


def cleanup_run_scoped_single_instance_socket(expected_bundle_id: str) -> bool:
    """Remove only this run-unique Tauri socket after its exact app is stopped."""
    socket_path = Path("/tmp") / f"{expected_bundle_id.replace('.', '_')}_si.sock"
    try:
        metadata = socket_path.lstat()
    except FileNotFoundError:
        return True
    if metadata.st_uid != os.getuid() or not stat.S_ISSOCK(metadata.st_mode):
        raise AcceptanceFailure("refusing to remove a foreign single-instance socket")
    probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        probe.settimeout(0.2)
        if probe.connect_ex(str(socket_path)) == 0:
            raise AcceptanceFailure("single-instance socket still has a live listener")
    finally:
        probe.close()
    socket_path.unlink()
    return not socket_path.exists()


def stop_exact_owned_gateway(session: InstalledProviderSession) -> None:
    """Stop only the old test Gateway so the replacement can reuse its port."""
    health = session.inspect_health()
    if not health.get("ok"):
        raise AcceptanceFailure("old test Gateway identity is not healthy before replacement")
    expected = session.gateway_bin.resolve(strict=True)
    matches = []
    for record in session.inspector.process_table():
        if (
            session.inspector.executable_for_pid(record.pid) == expected
            and session.inspector.listener_owned(record.pid, session.proxy_port)
        ):
            matches.append(record.pid)
    if len(matches) != 1 or matches[0] <= 1:
        raise AcceptanceFailure("old test Gateway listener identity is not unique")
    pid = matches[0]
    os.kill(pid, signal.SIGTERM)
    wait_until(
        lambda: not session.inspector.listener_owned(pid, session.proxy_port),
        timeout=5,
        description="old owned Gateway stop",
    )


def v3_fixture(session: InstalledProviderSession) -> Dict[str, Any]:
    return {
        "schema_version": 3,
        "profiles": [
            {
                "id": PROFILE_ID,
                "name": "Coverage Qwen v3",
                "template_id": "qwen",
                "category": "cn_official",
                "api_format": "openai_chat",
                "base_url": "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "api_key": FAKE_API_KEY,
                "model": "qwen-plus-latest",
                "credential_source": "api_key",
                "credential_ref": None,
                "model_policy": "optional_fixed",
                "website_url": None,
                "icon": "qwen",
                "icon_color": "#615CED",
                "sort_index": 1,
                "created_at": 1,
                "notes": "coverage-install-v3-fixture",
                "future_profile_field": {"preserve": True},
            }
        ],
        "active_id": PROFILE_ID,
        "proxy_port": session.proxy_port,
        "sandbox_port": session.sandbox_port,
        "reuse_system_ssh": False,
        "experimental_codex_enabled": False,
        "secret": FIXED_PATH_SECRET,
        "mode": "proxy",
        "pending_notice": None,
        "future_top_level": {"preserve": True},
    }


def read_json(path: Path) -> Dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise AcceptanceFailure(f"expected JSON object: {path}")
    return value


def migrated_config(session: InstalledProviderSession) -> Optional[Dict[str, Any]]:
    value = read_json(session.config_path)
    return value if value.get("schema_version") == 4 else None


def validate_v4(session: InstalledProviderSession, original_v3: bytes) -> Dict[str, Any]:
    cfg = read_json(session.config_path)
    if cfg.get("schema_version") != 4 or cfg.get("active_id") != PROFILE_ID:
        raise AcceptanceFailure("schema or active profile did not migrate")
    profiles = [item for item in cfg.get("profiles", []) if item.get("id") == PROFILE_ID]
    if len(profiles) != 1:
        raise AcceptanceFailure("migrated Qwen profile is missing or duplicated")
    profile = profiles[0]
    routes = profile.get("model_catalog")
    if not isinstance(routes, list) or len(routes) < 3:
        raise AcceptanceFailure("migrated Qwen catalog has fewer than three routes")
    selector_ids = [item.get("selector_id") for item in routes]
    upstreams = [item.get("upstream_model") for item in routes]
    if len(set(selector_ids)) != len(selector_ids) or not all(
        isinstance(item, str) and item.startswith("claude-csswitch-") for item in selector_ids
    ):
        raise AcceptanceFailure("migrated selector IDs are missing, duplicated, or unsafe")
    default = profile.get("default_model_route_id")
    if default not in selector_ids or routes[0].get("selector_id") != default:
        raise AcceptanceFailure("default selector is not first and valid")
    if routes[0].get("upstream_model") != "qwen-plus-latest":
        raise AcceptanceFailure("v3 selected Qwen model was not preserved as default")
    roles = profile.get("role_bindings")
    if not isinstance(roles, dict) or any(roles.get(role) not in selector_ids for role in (
        "sonnet", "opus", "haiku", "fable"
    )):
        raise AcceptanceFailure("role bindings do not target migrated selectors")
    if profile.get("future_profile_field") != {"preserve": True}:
        raise AcceptanceFailure("unknown profile field was not preserved")
    if cfg.get("future_top_level") != {"preserve": True}:
        raise AcceptanceFailure("unknown top-level field was not preserved")
    backup = session.csswitch_dir / "config.json.v3.bak"
    if not backup.is_file() or backup.read_bytes() != original_v3:
        raise AcceptanceFailure("immutable v3 migration backup is absent or changed")
    return {
        "model_count": len(routes),
        "selector_ids": selector_ids,
        "upstreams": upstreams,
        "default_selector": default,
        "role_bindings": {role: roles[role] for role in ("sonnet", "opus", "haiku", "fable")},
        "backup_sha256": sha256(backup),
    }


def gateway_catalog(session: InstalledProviderSession) -> Dict[str, Any]:
    status, _, raw = session._http_request(
        "GET", f"/{FIXED_PATH_SECRET}/v1/models", timeout=4.0
    )
    if status != 200:
        raise AcceptanceFailure(f"gateway /v1/models returned {status}")
    value = json.loads(raw)
    models = value.get("data") if isinstance(value, dict) else None
    if not isinstance(models, list) or len(models) < 3:
        raise AcceptanceFailure("gateway did not publish the migrated whitelist")
    ids = [item.get("id") for item in models]
    display_names = [item.get("display_name") for item in models]
    if any(str(item).startswith("claude-csswitch-codex-") for item in ids):
        raise AcceptanceFailure("Qwen gateway leaked stale Codex aliases")
    if any(
        not isinstance(name, str) or not name.strip() or name.strip().lower() == "default"
        for name in display_names
    ):
        raise AcceptanceFailure("gateway exposed an empty/default placeholder instead of model names")
    return {"count": len(ids), "ids": ids, "display_names": display_names}


def strict_route_checks(
    session: InstalledProviderSession,
    *,
    exact_selector: str,
    expected_upstream: str,
) -> Dict[str, Any]:
    if expected_upstream != "qwen3.7-max":
        raise AcceptanceFailure("exact-selector fixture must target qwen3.7-max")

    def post_exact(max_tokens: int, content: str) -> int:
        exact_body = json.dumps(
            {
                "model": exact_selector,
                "max_tokens": max_tokens,
                "messages": [{"role": "user", "content": content}],
            },
            separators=(",", ":"),
        ).encode()
        status, _, _ = session._http_request(
            "POST",
            f"/{FIXED_PATH_SECRET}/v1/messages",
            body=exact_body,
            headers={"Content-Type": "application/json"},
            timeout=4.0,
        )
        return status

    session.enter_phase("discovery")
    discovery_phase = session.finish_phase("discovery")

    session.enter_phase("scratch")
    exact_status = post_exact(1, "exact selector scratch coverage")
    exact_phase = session.finish_phase("scratch")
    if exact_status != 200 or not exact_phase.get("ok"):
        raise AcceptanceFailure("exact selector did not route to its saved Qwen upstream")

    session.enter_phase("formal")
    formal = session.send_formal()
    formal_phase = session.finish_phase("formal")
    if formal.get("status") != 200 or not formal_phase.get("ok"):
        raise AcceptanceFailure("official Claude role alias did not map to exact Qwen upstream")

    session.enter_phase("reuse")
    reuse_statuses = [
        post_exact(1, "exact selector reuse scratch coverage"),
        post_exact(1_000_000, "exact selector reuse formal coverage"),
    ]
    reuse_phase = session.finish_phase("reuse")
    if reuse_statuses != [200, 200] or not reuse_phase.get("ok"):
        raise AcceptanceFailure("exact selector reuse phase did not preserve upstream mapping")

    session.enter_phase("restart")
    restart_status = post_exact(1_000_000, "exact selector restart coverage")
    restart_phase = session.finish_phase("restart")
    if restart_status != 200 or not restart_phase.get("ok"):
        raise AcceptanceFailure("exact selector restart phase did not preserve upstream mapping")

    before = len(session._mock.status().get("requests", []))
    body = json.dumps(
        {
            "model": "claude-csswitch-codex-stale-should-fail",
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "must not reach upstream"}],
        },
        separators=(",", ":"),
    ).encode()
    status, _, raw = session._http_request(
        "POST",
        f"/{FIXED_PATH_SECRET}/v1/messages",
        body=body,
        headers={"Content-Type": "application/json"},
        timeout=4.0,
    )
    after = len(session._mock.status().get("requests", []))
    value = json.loads(raw)
    error_type = value.get("error", {}).get("type") if isinstance(value, dict) else None
    if status != 400 or error_type != "route_unknown" or after != before:
        raise AcceptanceFailure("unknown/Codex alias was not rejected before Qwen upstream")
    return {
        "role_alias_status": formal["status"],
        "formal_phase_ok": formal_phase["ok"],
        "exact_selector": exact_selector,
        "expected_upstream": expected_upstream,
        "exact_selector_status": exact_status,
        "exact_selector_phase_ok": exact_phase["ok"],
        "discovery_phase_ok": discovery_phase["ok"],
        "reuse_phase_ok": reuse_phase["ok"],
        "restart_phase_ok": restart_phase["ok"],
        "unknown_status": status,
        "unknown_type": error_type,
        "unknown_upstream_requests": after - before,
    }


def run(
    old_bundle: Path,
    new_bundle: Path,
    root: Path,
    expected_bundle_id: str = ACCEPTANCE_BUNDLE_ID,
    v3_commit: Optional[str] = None,
) -> Dict[str, Any]:
    root = require_isolated_root(root)
    control_socket = root / "session/evidence/provider-mock/control.sock"
    if len(os.fsencode(control_socket)) >= 100:
        raise AcceptanceFailure(
            "acceptance root is too long for the macOS Unix control socket; use a shorter /private/tmp path"
        )
    old_info = require_acceptance_bundle(old_bundle, expected_bundle_id)
    new_info = require_acceptance_bundle(new_bundle, expected_bundle_id)
    installed = root / "installed" / APP_NAME
    previous = root / "previous-install" / APP_NAME
    ditto_bundle(old_bundle, installed)
    require_acceptance_bundle(installed, expected_bundle_id)

    session_root = root / "session"
    old_process: Optional[int] = None
    new_process: Optional[int] = None
    with InstalledProviderSession(
        "qwen-chat",
        root=session_root,
        app_bundle=installed,
        allow_test_bundle=True,
        expected_bundle_id=expected_bundle_id,
        config_dir_name=".csswitch-acceptance",
    ) as session:
        try:
            session.start_mock()
            fixture = v3_fixture(session)
            _safe_json_write(session.config_path, fixture)
            session.config_path.chmod(0o600)
            original_v3 = session.config_path.read_bytes()

            # Start the old runtime in the same temporary HOME so replacement
            # proves a Science refresh, not merely a first post-migration boot.
            session.release_app_ports()
            old_env = session._launch_environment(session._mock.base_url, auto_boot=True)
            old_process = launch_owned(
                installed,
                session.app_bin.resolve(strict=True),
                old_env,
                session.evidence / "old-app.stdout.log",
                session.evidence / "old-app.stderr.log",
            )
            old_health = wait_until(
                lambda: (lambda value: value if value.get("ok") else None)(session.inspect_health()),
                timeout=20,
                description="old formal Gateway health",
            )
            old_science = wait_until(
                lambda: (
                    lambda value: value if value.get("identity_verified") else None
                )(session.inspect_fake_science()),
                timeout=20,
                description="old isolated fake Science",
            )
            if read_json(session.config_path).get("schema_version") != 3:
                raise AcceptanceFailure("old Acceptance did not retain the v3 fixture")
            old_binary_sha = sha256(session.app_bin)
            crash_owned_app(old_process, session.app_bin)
            old_process = None
            stop_exact_owned_gateway(session)
            wait_until(
                lambda: (
                    lambda value: value
                    if value.get("identity_verified") and value.get("pid") == old_science["pid"]
                    else None
                )(session.inspect_fake_science()),
                timeout=5,
                description="old Science preserved across app replacement",
            )

            previous.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            os.replace(installed, previous)
            try:
                ditto_bundle(new_bundle, installed)
            except Exception:
                if not installed.exists() and previous.exists():
                    os.replace(previous, installed)
                raise
            require_acceptance_bundle(installed, expected_bundle_id)
            new_binary_sha = sha256(installed / "Contents/MacOS/desktop")
            if old_binary_sha == new_binary_sha:
                raise AcceptanceFailure("coverage replacement did not change the desktop binary")
            if (installed / "Contents/Resources/coverage-old-only-marker").exists():
                raise AcceptanceFailure("new bundle contains a stale old-only resource")

            new_env = session._launch_environment(session._mock.base_url, auto_boot=True)
            new_process = launch_owned(
                installed,
                (installed / "Contents/MacOS/desktop").resolve(strict=True),
                new_env,
                session.evidence / "new-app.stdout.log",
                session.evidence / "new-app.stderr.log",
            )
            wait_until(
                lambda: migrated_config(session), timeout=15, description="v4 migration"
            )
            migration = validate_v4(session, original_v3)
            health = wait_until(
                lambda: (lambda value: value if value.get("ok") else None)(session.inspect_health()),
                timeout=20,
                description="formal Gateway health",
            )
            fake_science = wait_until(
                lambda: (
                    lambda value: value
                    if value.get("identity_verified") and value.get("pid") != old_science["pid"]
                    else None
                )(session.inspect_fake_science()),
                timeout=20,
                description="refreshed isolated fake Science",
            )
            catalog = gateway_catalog(session)
            if catalog["ids"][0] != migration["default_selector"]:
                raise AcceptanceFailure("gateway default selector differs from migrated config")
            try:
                exact_index = migration["upstreams"].index("qwen3.7-max")
            except ValueError as error:
                raise AcceptanceFailure("migrated Qwen catalog lacks exact-selector fixture route") from error
            routes = strict_route_checks(
                session,
                exact_selector=migration["selector_ids"][exact_index],
                expected_upstream=migration["upstreams"][exact_index],
            )
            open_log = session.evidence / "fake-open.log"
            wait_until(
                lambda: (
                    open_log.is_file()
                    and len(
                        [line for line in open_log.read_text().splitlines() if "open-called" in line]
                    )
                    >= 2
                ),
                timeout=8,
                description="old and new browser opener evidence",
            )
            log_scan = session.scan_logs()
            if not log_scan.get("ok"):
                raise AcceptanceFailure("Acceptance logs contain sensitive fixtures or Python tripwire")

            terminate_owned(
                new_process, installed / "Contents/MacOS/desktop", "new Acceptance"
            )
            new_process = None
            stop_exact_owned_gateway(session)
            stopped_science = session.stop_fake_science()
            if not stopped_science.get("ok"):
                raise AcceptanceFailure("exact fake Science stop command failed")
            wait_until(
                lambda: not session.inspector.listener_owned(fake_science["pid"], session.sandbox_port),
                timeout=8,
                description="fake Science cleanup",
            )
            mock_result = session.stop_mock()
            if not (
                mock_result.get("stopped")
                and mock_result.get("complete")
                and mock_result.get("ok")
            ):
                raise AcceptanceFailure("provider mock phases were incomplete or failed")
            cleanup = wait_until(
                lambda: (
                    lambda value: value if value.get("ok") else None
                )(session.verify_cleanup()),
                timeout=12,
                description="owned Acceptance process and port cleanup",
            )
            cleanup["single_instance_socket_closed"] = (
                cleanup_run_scoped_single_instance_socket(expected_bundle_id)
            )

            summary = {
                "schema": "csswitch.model-catalog-coverage-acceptance.v1",
                "status": "passed",
                "v3_commit": v3_commit,
                "old_bundle_id": old_info["CFBundleIdentifier"],
                "new_bundle_id": new_info["CFBundleIdentifier"],
                "installation_path_reused": True,
                "same_home_reused": True,
                "old_binary_sha256": old_binary_sha,
                "new_binary_sha256": new_binary_sha,
                "migration": migration,
                "gateway_catalog": catalog,
                "strict_routes": routes,
                "gateway_health": {
                    "provider": health["provider"],
                    "gateway": health["gateway"],
                    "shim": health["shim"],
                },
                "old_gateway_health": {
                    "provider": old_health["provider"],
                    "gateway": old_health["gateway"],
                    "shim": old_health["shim"],
                },
                "fake_science": {
                    "old_pid": old_science["pid"],
                    "new_pid": fake_science["pid"],
                    "restarted_for_migrated_catalog": old_science["pid"] != fake_science["pid"],
                    "port": fake_science["port"],
                    "identity_verified": fake_science["identity_verified"],
                },
                "browser_opener_called": True,
                "log_scan": log_scan,
                "mock_stopped": mock_result.get("stopped"),
                "cleanup": cleanup,
                "safety": {
                    "formal_app_path_used": False,
                    "real_credentials_read": False,
                    "real_science_data_used": False,
                    "test_root": str(root),
                },
            }
            _safe_json_write(session.evidence / "coverage-install-summary.json", summary)
            return summary
        finally:
            terminate_owned(
                new_process, installed / "Contents/MacOS/desktop", "new Acceptance"
            )
            terminate_owned(old_process, installed / "Contents/MacOS/desktop", "old Acceptance")
            cleanup_owned_runtime(session)
            cleanup_run_scoped_single_instance_socket(expected_bundle_id)
            if session._mock_started and not session._mock_stopped:
                session.stop_mock()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--old-bundle", type=Path, required=True)
    parser.add_argument("--new-bundle", type=Path, required=True)
    parser.add_argument("--expected-bundle-id", default=ACCEPTANCE_BUNDLE_ID)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--v3-commit")
    args = parser.parse_args()
    try:
        summary = run(
            args.old_bundle,
            args.new_bundle,
            args.root,
            args.expected_bundle_id,
            args.v3_commit,
        )
    except Exception as error:
        failure = {
            "schema": "csswitch.model-catalog-coverage-acceptance.v1",
            "status": "failed",
            "stage": "coverage_install",
            "message": str(error)[:1000],
        }
        print(json.dumps(failure, ensure_ascii=False, sort_keys=True, indent=2))
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print(json.dumps(summary, ensure_ascii=False, sort_keys=True, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
