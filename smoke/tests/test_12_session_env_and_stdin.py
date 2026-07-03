from __future__ import annotations

import shlex
import uuid

from awsmoke.gateway import gateway_command_for_config
from awsmoke.hosts import Host


SESSION_ENV_KEY = "AW_GATEWAY_SMOKE_SESSION_ENV"


def test_session_env_inherit_is_per_gateway_process(host: Host) -> None:
    config = _write_session_config(host)
    try:
        _remove_fixed_target(host, config)

        inherited = _remote_gateway(
            host,
            config,
            "run",
            host.target,
            "--",
            "/bin/sh",
            "-lc",
            f'test -n "${{{SESSION_ENV_KEY}:-}}"',
            env={SESSION_ENV_KEY: "session-value"},
            timeout=300,
        )
        inherited.assert_success()

        absent = _remote_gateway(
            host,
            config,
            "run",
            host.target,
            "--",
            "/bin/sh",
            "-lc",
            f'test "${{{SESSION_ENV_KEY}+set}}" != set',
            timeout=300,
        )
        absent.assert_success()
    finally:
        _remove_fixed_target(host, config)
        _rm(host, config)


def test_streamed_run_and_launch_keep_piped_stdin_attached(host: Host) -> None:
    config = _write_session_config(host)
    try:
        _remove_fixed_target(host, config)

        run_result = _pipe_to_gateway(
            host,
            config,
            "run",
            host.target,
            "--",
            "/bin/sh",
            "-lc",
            'IFS= read -r line && test "$line" = "run-stdin"',
            stdin_text="run-stdin\n",
            timeout=300,
        )
        run_result.assert_success()

        launch_result = _pipe_to_gateway(
            host,
            config,
            "launch",
            "smoke-stdin",
            stdin_text="launch-stdin\n",
            timeout=300,
        )
        launch_result.assert_success()
    finally:
        _remove_fixed_target(host, config)
        _rm(host, config)


def _write_session_config(host: Host) -> str:
    config = f"/tmp/aw-gateway-smoke-session-{host.name}-{uuid.uuid4().hex}.toml"
    env = {
        "SOURCE_CONFIG": host.local_config_path,
        "CONFIG": config,
        "TARGET": host.target,
        "SESSION_ENV_KEY": SESSION_ENV_KEY,
    }
    assignments = " ".join(f"{key}={shlex.quote(value)}" for key, value in env.items())
    script = f"""
set -euo pipefail
{assignments} python3 - <<'PY'
import json
import os
from pathlib import Path


output = Path(os.environ["CONFIG"])
target = os.environ["TARGET"]
session_env_key = os.environ["SESSION_ENV_KEY"]

text = f"extends = {{json.dumps(os.environ['SOURCE_CONFIG'])}}\\n"
text += f"\\n[targets.{{target}}]\\n"
text += f"session_env_inherit = [{{json.dumps(session_env_key)}}]\\n"
text += "\\n[launches.smoke-stdin]\\n"
text += f"target = {{json.dumps(target)}}\\n"
text += "description = \\"Streaming stdin attachment smoke launch.\\"\\n"
text += "command = "
text += json.dumps(
    [
        "/bin/sh",
        "-lc",
        'IFS= read -r line && test "$line" = "launch-stdin"',
    ]
)
text += "\\n"
output.write_text(text + "\\n")
PY
"""
    host.check(script, timeout=60)
    return config


def _pipe_to_gateway(
    host: Host,
    config: str,
    *args: str,
    stdin_text: str,
    timeout: int,
):
    gateway = gateway_command_for_config(host, config, *args)
    command = f"printf %s {shlex.quote(stdin_text)} | {gateway}"
    return host.run(command, timeout=timeout)


def _remote_gateway(
    host: Host,
    config: str,
    *args: str,
    env: dict[str, str] | None = None,
    timeout: int,
):
    command = f"{_env_assignments(env or {})} {gateway_command_for_config(host, config, *args)}"
    return host.run(command.strip(), timeout=timeout)


def _env_assignments(env: dict[str, str]) -> str:
    return " ".join(f"{key}={shlex.quote(value)}" for key, value in env.items())


def _remove_fixed_target(host: Host, config: str) -> None:
    _remote_gateway(host, config, "remove", host.target, timeout=180)


def _rm(host: Host, path: str) -> None:
    host.run(f"rm -f {shlex.quote(path)}", timeout=30)
