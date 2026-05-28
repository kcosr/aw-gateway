from __future__ import annotations

import json
import shlex
import time
import uuid
from collections.abc import Callable
from typing import Any

import pytest

from awsmoke.gateway import gateway_command_for_config
from awsmoke.hosts import Host
from awsmoke.ssh import remote, remote_check


def test_gateway_owned_idle_cleanup_stops_after_short_grace(host: Host) -> None:
    config = _write_temp_config(
        host,
        owner="gateway",
        action="exit_container",
        preserve_processes=[],
        idle_grace="1s",
        poll_interval="1s",
        shutdown_timeout="3s",
    )
    try:
        _remove_fixed_target(host, config)
        run = _gateway(host, config, "run", host.target, "--", "/bin/true", timeout=120)
        run.assert_success()

        status = _gateway_json(host, config, "status", host.target, "--json", timeout=60)
        assert status["status"] == "not-running"
    finally:
        _remove_fixed_target(host, config)
        _rm(host, config)


def test_agent_owned_idle_cleanup_stops_after_short_grace(host: Host) -> None:
    _skip_without_agent_control(host)
    config = _write_temp_config(
        host,
        owner="agent",
        action="exit_container",
        preserve_processes=[],
        idle_grace="2s",
        poll_interval="1s",
        shutdown_timeout="3s",
    )
    try:
        _remove_fixed_target(host, config)
        run = _gateway(host, config, "run", host.target, "--", "/bin/true", timeout=120)
        run.assert_success()

        status = _wait_for_status(host, config, lambda value: value["status"] == "not-running")
        assert status["status"] == "not-running"
    finally:
        _remove_fixed_target(host, config)
        _rm(host, config)


def test_agent_idle_cleanup_preserves_tmux_named_process(host: Host) -> None:
    _skip_without_agent_control(host)
    config = _write_temp_config(
        host,
        owner="agent",
        action="exit_container",
        preserve_processes=["tmux"],
        idle_grace="2s",
        poll_interval="1s",
        shutdown_timeout="3s",
    )
    try:
        _remove_fixed_target(host, config)
        starter = _gateway(
            host,
            config,
            "run",
            host.target,
            "--",
            "/bin/bash",
            "-lc",
            "cp /bin/sleep /tmp/tmux && /tmp/tmux 60 >/tmp/tmux.out 2>&1 & echo $!",
            timeout=120,
        )
        starter.assert_success()

        status = _wait_for_status(host, config, _agent_preserved_tmux)
        cleanup = status["agent"]["idle_cleanup"]
        assert cleanup["state"] == "preserved"
        assert cleanup["preserve"] is True
        assert any(process["comm"] == "tmux" for process in cleanup["matched_processes"])
        assert status["status"] == "ready"

        killer = _gateway(
            host,
            config,
            "run",
            host.target,
            "--",
            "/bin/bash",
            "-lc",
            "pkill -x tmux || true",
            timeout=120,
        )
        killer.assert_success()

        stopped = _wait_for_status(host, config, lambda value: value["status"] == "not-running")
        assert stopped["status"] == "not-running"
    finally:
        _remove_fixed_target(host, config)
        _rm(host, config)


def test_agent_reap_processes_after_tmux_preserve_clears(host: Host) -> None:
    _skip_without_agent_control(host)
    config = _write_temp_config(
        host,
        owner="agent",
        action="reap_processes",
        preserve_processes=["tmux"],
        idle_grace="2s",
        poll_interval="1s",
        shutdown_timeout="3s",
        reap_signal="TERM",
        reap_kill_after="1s",
        container_env={"AW_CONTAINER_AGENT_ALLOW_PROCESS_REAP": "1"},
    )
    try:
        _remove_fixed_target(host, config)
        starter = _gateway(
            host,
            config,
            "run",
            host.target,
            "--",
            "/bin/bash",
            "-lc",
            (
                "cp /bin/sleep /tmp/tmux && "
                "/tmp/tmux 60 >/tmp/reap-tmux.out 2>&1 & echo $! >/tmp/reap-tmux.pid; "
                "/bin/sleep 60 >/tmp/reap-sleep.out 2>&1 & echo $! >/tmp/reap-sleep.pid"
            ),
            timeout=120,
        )
        starter.assert_success()

        preserved = _wait_for_status(host, config, _agent_preserved_tmux)
        assert preserved["agent"]["idle_cleanup"]["last_reap_result"] is None

        killer = _gateway(
            host,
            config,
            "run",
            host.target,
            "--",
            "/bin/bash",
            "-lc",
            "pkill -KILL -x tmux",
            timeout=120,
        )
        killer.assert_success()

        status = _wait_for_status(host, config, _agent_reaped_sleep)
        result = status["agent"]["idle_cleanup"]["last_reap_result"]
        assert result["dry_run"] is False
        assert any(process["comm"] == "sleep" for process in result["would_terminate"])

        verifier = _gateway(
            host,
            config,
            "run",
            host.target,
            "--",
            "/bin/bash",
            "-lc",
            (
                "tmux_pid=$(cat /tmp/reap-tmux.pid); "
                "sleep_pid=$(cat /tmp/reap-sleep.pid); "
                "! kill -0 \"$tmux_pid\" 2>/dev/null; "
                "! kill -0 \"$sleep_pid\" 2>/dev/null"
            ),
            timeout=120,
        )
        verifier.assert_success()
    finally:
        _remove_fixed_target(host, config)
        _rm(host, config)


def test_ephemeral_workspace_cleanup_removes_session_workspace(host: Host) -> None:
    session_id = f"cleanup-{host.name.replace('_', '-')}"
    workspace = _ephemeral_workspace(host, session_id)
    config = _write_temp_config(
        host,
        owner="gateway",
        action="exit_container",
        preserve_processes=[],
        idle_grace="0s",
        poll_interval="1s",
        shutdown_timeout="3s",
        ephemeral_workspace=True,
    )
    try:
        _cleanup_ephemeral(host, config, session_id, workspace)
        run = _gateway(
            host,
            config,
            "run",
            "--session-id",
            session_id,
            host.target,
            "--",
            "/bin/bash",
            "-lc",
            "printf workspace-cleanup > workspace-cleanup.txt",
            timeout=180,
        )
        run.assert_success()

        missing = remote_check(host.ssh, f"test ! -e {shlex.quote(workspace)}", timeout=30)
        assert missing.returncode == 0
    finally:
        _cleanup_ephemeral(host, config, session_id, workspace)
        _rm(host, config)


def test_interrupted_ephemeral_launch_cleans_session_workspace(host: Host) -> None:
    session_id = f"cancel-{host.name.replace('_', '-')}"
    workspace = _ephemeral_workspace(host, session_id)
    marker_name = f"launch-started-{uuid.uuid4().hex}"
    marker = f"{workspace}/{marker_name}"
    background: tuple[str, str, str] | None = None
    config = _write_temp_config(
        host,
        owner="gateway",
        action="exit_container",
        preserve_processes=[],
        idle_grace="0s",
        poll_interval="1s",
        shutdown_timeout="3s",
        ephemeral_workspace=True,
        long_launch_marker=marker_name,
    )
    try:
        _cleanup_ephemeral(host, config, session_id, workspace)
        background = _start_ephemeral_launch_background(host, config, session_id)
        _wait_for_remote_path(host, marker, *background)

        pidfile, _, _ = background
        remote_check(
            host.ssh,
            f"kill -HUP $(cat {shlex.quote(pidfile)})",
            timeout=30,
        )
        _wait_for_remote_absent(host, workspace, timeout=90)
        background = None
    finally:
        if background is not None:
            _stop_background(host, background)
        _cleanup_ephemeral(host, config, session_id, workspace)
        _rm(host, config)


def test_explicit_ephemeral_remove_cleans_session_workspace(host: Host) -> None:
    session_id = f"remove-{host.name.replace('_', '-')}"
    workspace = _ephemeral_workspace(host, session_id)
    background: tuple[str, str, str] | None = None
    config = _write_temp_config(
        host,
        owner="gateway",
        action="exit_container",
        preserve_processes=[],
        idle_grace="5m",
        poll_interval="1s",
        shutdown_timeout="3s",
        ephemeral_workspace=True,
    )
    try:
        _cleanup_ephemeral(host, config, session_id, workspace)
        background = _start_ephemeral_up_background(host, config, session_id, workspace)
        _stop_background(host, background)
        background = None

        marker = remote_check(
            host.ssh,
            f"test -d {shlex.quote(workspace)} && printf explicit-remove > {shlex.quote(workspace)}/explicit-remove.txt",
            timeout=30,
        )
        assert marker.returncode == 0

        remove = _gateway(
            host,
            config,
            "remove",
            host.target,
            "--session-id",
            session_id,
            timeout=180,
        )
        remove.assert_success()

        missing = remote_check(host.ssh, f"test ! -e {shlex.quote(workspace)}", timeout=30)
        assert missing.returncode == 0
    finally:
        if background is not None:
            _stop_background(host, background)
        _cleanup_ephemeral(host, config, session_id, workspace)
        _rm(host, config)


def test_ssh_dispatch_ephemeral_remove_cleans_session_workspace(host: Host) -> None:
    session_id = f"ssh-remove-{host.name.replace('_', '-')}"
    workspace = _ephemeral_workspace(host, session_id)
    background: tuple[str, str, str] | None = None
    config = _write_temp_config(
        host,
        owner="gateway",
        action="exit_container",
        preserve_processes=[],
        idle_grace="5m",
        poll_interval="1s",
        shutdown_timeout="3s",
        ephemeral_workspace=True,
    )
    try:
        _cleanup_ephemeral(host, config, session_id, workspace)
        background = _start_ephemeral_up_background(host, config, session_id, workspace)
        _stop_background(host, background)
        background = None

        marker = remote_check(
            host.ssh,
            f"test -d {shlex.quote(workspace)} && printf ssh-remove > {shlex.quote(workspace)}/ssh-remove.txt",
            timeout=30,
        )
        assert marker.returncode == 0

        original_command = f"remove {host.target} --session-id={session_id}"
        dispatched = remote(
            host.ssh,
            f"SSH_ORIGINAL_COMMAND={shlex.quote(original_command)} {gateway_command_for_config(host, config)}",
            timeout=180,
        )
        dispatched.assert_success()

        missing = remote_check(host.ssh, f"test ! -e {shlex.quote(workspace)}", timeout=30)
        assert missing.returncode == 0
    finally:
        if background is not None:
            _stop_background(host, background)
        _cleanup_ephemeral(host, config, session_id, workspace)
        _rm(host, config)


def _skip_without_agent_control(host: Host) -> None:
    if host.runtime == "colima":
        pytest.skip("Colima smoke config uses gateway-owned cleanup and disables agent control")


def _ephemeral_workspace(host: Host, session_id: str) -> str:
    home = host.home_dir
    if home == "~":
        home = remote_check(host.ssh, 'printf "%s" "$HOME"', timeout=30).stdout
    return f"{home}/.cache/aw-gateway/workspaces/{host.target}-{session_id}"


def _write_temp_config(
    host: Host,
    *,
    owner: str,
    action: str,
    preserve_processes: list[str],
    idle_grace: str,
    poll_interval: str,
    shutdown_timeout: str,
    reap_signal: str | None = None,
    reap_kill_after: str | None = None,
    container_env: dict[str, str] | None = None,
    ephemeral_workspace: bool = False,
    long_launch_marker: str | None = None,
) -> str:
    config = f"/tmp/aw-gateway-smoke-cleanup-{host.name}-{uuid.uuid4().hex}.toml"
    env = {
        "SOURCE_CONFIG": host.local_config_path,
        "CONFIG": config,
        "TARGET": host.target,
        "OWNER": owner,
        "ACTION": action,
        "PRESERVE_PROCESSES": json.dumps(preserve_processes),
        "IDLE_GRACE": idle_grace,
        "POLL_INTERVAL": poll_interval,
        "SHUTDOWN_TIMEOUT": shutdown_timeout,
        "REAP_SIGNAL": reap_signal or "",
        "REAP_KILL_AFTER": reap_kill_after or "",
        "CONTAINER_ENV": json.dumps(container_env or {}),
        "EPHEMERAL_WORKSPACE": "1" if ephemeral_workspace else "0",
        "LONG_LAUNCH_MARKER": long_launch_marker or "",
    }
    assignments = " ".join(f"{key}={shlex.quote(value)}" for key, value in env.items())
    script = f"""
set -euo pipefail
{assignments} python3 - <<'PY'
import json
import os
from pathlib import Path


def ensure_section(lines, section):
    header = f"[{{section}}]"
    for index, line in enumerate(lines):
        if line.strip() == header:
            return index
    lines.extend(["", header])
    return len(lines) - 1


def section_end(lines, start):
    for index in range(start + 1, len(lines)):
        stripped = lines[index].strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            return index
    return len(lines)


def set_key(lines, section, key, value):
    start = ensure_section(lines, section)
    end = section_end(lines, start)
    for index in range(start + 1, end):
        stripped = lines[index].strip()
        if stripped.startswith(f"{{key}} ") or stripped.startswith(f"{{key}}="):
            prefix = lines[index][: len(lines[index]) - len(lines[index].lstrip())]
            lines[index] = f"{{prefix}}{{key}} = {{value}}"
            return
    lines.insert(end, f"{{key}} = {{value}}")


def remove_key(lines, section, key):
    start = ensure_section(lines, section)
    end = section_end(lines, start)
    for index in range(end - 1, start, -1):
        stripped = lines[index].strip()
        if stripped.startswith(f"{{key}} ") or stripped.startswith(f"{{key}}="):
            lines.pop(index)


def ensure_array_item(lines, section, key, item):
    start = ensure_section(lines, section)
    end = section_end(lines, start)
    for index in range(start + 1, end):
        if lines[index].strip().startswith(f"{{key}} "):
            array_start = index
            break
    else:
        lines.insert(end, f"{{key}} = [")
        lines.insert(end + 1, f"  {{json.dumps(item)}},")
        lines.insert(end + 2, "]")
        return

    array_end = array_start
    while array_end < end and "]" not in lines[array_end]:
        array_end += 1
    existing = "\\n".join(lines[array_start : array_end + 1])
    if json.dumps(item) in existing:
        return
    lines.insert(array_end, f"  {{json.dumps(item)}},")


source = Path(os.environ["SOURCE_CONFIG"])
output = Path(os.environ["CONFIG"])
target = os.environ["TARGET"]
lines = source.read_text().splitlines()

ensure_array_item(lines, "ssh_dispatch", "enabled_actions", "remove")

idle = f"targets.{{target}}.idle_cleanup"
set_key(lines, idle, "owner", json.dumps(os.environ["OWNER"]))
set_key(lines, idle, "action", json.dumps(os.environ["ACTION"]))
set_key(lines, idle, "idle_grace", json.dumps(os.environ["IDLE_GRACE"]))
set_key(lines, idle, "poll_interval", json.dumps(os.environ["POLL_INTERVAL"]))
set_key(lines, idle, "shutdown_timeout", json.dumps(os.environ["SHUTDOWN_TIMEOUT"]))
set_key(lines, idle, "preserve_processes", json.dumps(json.loads(os.environ["PRESERVE_PROCESSES"])))

if os.environ["REAP_SIGNAL"]:
    set_key(lines, idle, "reap_signal", json.dumps(os.environ["REAP_SIGNAL"]))
if os.environ["REAP_KILL_AFTER"]:
    set_key(lines, idle, "reap_kill_after", json.dumps(os.environ["REAP_KILL_AFTER"]))

for key, value in json.loads(os.environ["CONTAINER_ENV"]).items():
    set_key(lines, f"targets.{{target}}.container_env", key, json.dumps(value))

if os.environ["EPHEMERAL_WORKSPACE"] == "1":
    target_section = f"targets.{{target}}"
    set_key(lines, target_section, "mode", json.dumps("ephemeral"))
    remove_key(lines, target_section, "name")
    set_key(lines, target_section, "ephemeral_name", json.dumps("{{image_slug}}-{{session_id}}"))
    set_key(lines, target_section, "stop_when_idle", "true")
    set_key(lines, target_section, "remove_on_stop", "true")
    workspace = f"targets.{{target}}.workspace"
    set_key(
        lines,
        workspace,
        "path",
        json.dumps("{{home}}/.cache/aw-gateway/workspaces/{{target}}-{{session_id}}"),
    )
    set_key(lines, workspace, "cleanup", json.dumps("always"))

if os.environ["LONG_LAUNCH_MARKER"]:
    marker = os.environ["LONG_LAUNCH_MARKER"]
    lines.extend(
        [
            "",
            "[launches.smoke-sleep]",
            f"target = {{json.dumps(target)}}",
            "description = " + json.dumps("Interrupted cleanup smoke launch."),
            "command = "
            + json.dumps(
                    [
                        "/bin/bash",
                        "-lc",
                        f"echo started > {{marker}}; sleep 60",
                    ]
                ),
        ]
    )

output.write_text("\\n".join(lines) + "\\n")
PY
"""
    remote_check(host.ssh, script, timeout=60)
    return config


def _agent_preserved_tmux(status: dict[str, Any]) -> bool:
    cleanup = (((status.get("agent") or {}).get("idle_cleanup")) or {})
    return cleanup.get("preserve") is True and any(
        process.get("comm") == "tmux" for process in cleanup.get("matched_processes", [])
    )


def _agent_reaped_sleep(status: dict[str, Any]) -> bool:
    cleanup = (((status.get("agent") or {}).get("idle_cleanup")) or {})
    result = cleanup.get("last_reap_result") or {}
    would_terminate = result.get("would_terminate") or []
    return (
        result.get("dry_run") is False
        and any(process.get("comm") == "sleep" for process in would_terminate)
    )


def _wait_for_status(
    host: Host,
    config: str,
    predicate: Callable[[dict[str, Any]], bool],
    *,
    timeout: float = 30.0,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last: dict[str, Any] | None = None
    while time.monotonic() < deadline:
        last = _gateway_json(host, config, "status", host.target, "--json", timeout=60)
        if predicate(last):
            return last
        time.sleep(1.0)
    raise AssertionError(f"condition was not met before timeout; last status={last!r}")


def _start_ephemeral_up_background(
    host: Host,
    config: str,
    session_id: str,
    workspace: str,
) -> tuple[str, str, str]:
    token = uuid.uuid4().hex
    pidfile = f"/tmp/aw-gateway-smoke-up-{host.name}-{token}.pid"
    stdout = f"/tmp/aw-gateway-smoke-up-{host.name}-{token}.out"
    stderr = f"/tmp/aw-gateway-smoke-up-{host.name}-{token}.err"
    command = (
        f"rm -f {shlex.quote(pidfile)} {shlex.quote(stdout)} {shlex.quote(stderr)}; "
        f"nohup {gateway_command_for_config(host, config, 'up', host.target, '--session-id', session_id, '--json')} "
        f"> {shlex.quote(stdout)} 2> {shlex.quote(stderr)} < /dev/null & "
        f"echo $! > {shlex.quote(pidfile)}"
    )
    remote_check(host.ssh, command, timeout=30)
    _wait_for_remote_path(host, workspace, pidfile, stdout, stderr)
    return pidfile, stdout, stderr


def _start_ephemeral_launch_background(
    host: Host,
    config: str,
    session_id: str,
) -> tuple[str, str, str]:
    token = uuid.uuid4().hex
    pidfile = f"/tmp/aw-gateway-smoke-launch-{host.name}-{token}.pid"
    stdout = f"/tmp/aw-gateway-smoke-launch-{host.name}-{token}.out"
    stderr = f"/tmp/aw-gateway-smoke-launch-{host.name}-{token}.err"
    command = (
        f"rm -f {shlex.quote(pidfile)} {shlex.quote(stdout)} {shlex.quote(stderr)}; "
        f"nohup {gateway_command_for_config(host, config, 'launch', 'smoke-sleep', '--session-id', session_id)} "
        f"> {shlex.quote(stdout)} 2> {shlex.quote(stderr)} < /dev/null & "
        f"echo $! > {shlex.quote(pidfile)}"
    )
    remote_check(host.ssh, command, timeout=30)
    return pidfile, stdout, stderr


def _wait_for_remote_path(
    host: Host,
    path: str,
    pidfile: str,
    stdout: str,
    stderr: str,
    *,
    timeout: float = 60.0,
) -> None:
    deadline = time.monotonic() + timeout
    quoted_path = shlex.quote(path)
    quoted_pidfile = shlex.quote(pidfile)
    while time.monotonic() < deadline:
        check = remote(
            host.ssh,
            f"test -e {quoted_path} && exit 0; "
            f"if ! kill -0 $(cat {quoted_pidfile}) 2>/dev/null; then exit 2; fi; "
            "exit 1",
            timeout=30,
        )
        if check.returncode == 0:
            return
        if check.returncode == 2:
            logs = remote(
                host.ssh,
                f"printf 'stdout:\\n'; cat {shlex.quote(stdout)} 2>/dev/null; "
                f"printf '\\nstderr:\\n'; cat {shlex.quote(stderr)} 2>/dev/null",
                timeout=30,
            )
            raise AssertionError(f"background up exited before workspace existed:\n{logs.stdout}")
        time.sleep(1.0)
    logs = remote(
        host.ssh,
        f"printf 'stdout:\\n'; cat {shlex.quote(stdout)} 2>/dev/null; "
        f"printf '\\nstderr:\\n'; cat {shlex.quote(stderr)} 2>/dev/null",
        timeout=30,
    )
    raise AssertionError(f"path did not appear before timeout: {path}\n{logs.stdout}")


def _wait_for_remote_absent(host: Host, path: str, *, timeout: float = 60.0) -> None:
    deadline = time.monotonic() + timeout
    quoted_path = shlex.quote(path)
    while time.monotonic() < deadline:
        check = remote(host.ssh, f"test ! -e {quoted_path}", timeout=30)
        if check.returncode == 0:
            return
        time.sleep(1.0)
    raise AssertionError(f"path still exists after timeout: {path}")


def _stop_background(host: Host, background: tuple[str, str, str]) -> None:
    pidfile, stdout, stderr = background
    remote(
        host.ssh,
        f"if [ -s {shlex.quote(pidfile)} ]; then kill $(cat {shlex.quote(pidfile)}) 2>/dev/null || true; fi; "
        f"rm -f {shlex.quote(pidfile)} {shlex.quote(stdout)} {shlex.quote(stderr)}",
        timeout=30,
    )


def _gateway_json(host: Host, config: str, *args: str, timeout: int = 60) -> dict[str, Any]:
    result = _gateway(host, config, *args, timeout=timeout)
    result.assert_success()
    return json.loads(result.stdout)


def _gateway(host: Host, config: str, *args: str, timeout: int = 60):
    return remote(host.ssh, gateway_command_for_config(host, config, *args), timeout=timeout)


def _remove_fixed_target(host: Host, config: str) -> None:
    _gateway(host, config, "remove", host.target, timeout=180)


def _cleanup_ephemeral(host: Host, config: str, session_id: str, workspace: str) -> None:
    _gateway(host, config, "stop", host.target, "--session-id", session_id, timeout=120)
    quoted_workspace = shlex.quote(workspace)
    remote(
        host.ssh,
        f"rm -rf {quoted_workspace} 2>/dev/null || sudo -n rm -rf {quoted_workspace}",
        timeout=30,
    )


def _rm(host: Host, path: str) -> None:
    remote(host.ssh, f"rm -f {shlex.quote(path)}", timeout=30)
