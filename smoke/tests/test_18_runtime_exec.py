from __future__ import annotations

import json

from awsmoke.gateway import runtime_exec_gateway
from awsmoke.hosts import Host


def expected_runtime_exec_uid(host: Host) -> str:
    if host.runtime == "apple_container":
        result = host.run("id -u", timeout=30)
        result.assert_success()
        return result.stdout.strip()
    return "0"


def test_runtime_exec_config_validates(host: Host) -> None:
    result = runtime_exec_gateway(host, "config", "validate")
    result.assert_success()
    assert "ok" in result.stdout.lower()


def test_runtime_exec_run_shell_status_and_ssh_operation_rejection(host: Host) -> None:
    try:
        runtime_exec_gateway(host, "remove", host.target, timeout=180)

        run = runtime_exec_gateway(
            host,
            "run",
            host.target,
            "--",
            "/bin/sh",
            "-lc",
            "printf 'runtime-exec-run:%s' \"$(id -u)\"",
            timeout=300,
        )
        run.assert_success()
        assert f"runtime-exec-run:{expected_runtime_exec_uid(host)}" in run.stdout

        shell = runtime_exec_gateway(
            host,
            "shell",
            host.target,
            "--",
            "-lc",
            "printf runtime-exec-shell",
            timeout=300,
        )
        shell.assert_success()
        assert "runtime-exec-shell" in shell.stdout

        status = runtime_exec_gateway(host, "status", host.target, "--json", timeout=120)
        status.assert_success()
        data = json.loads(status.stdout)
        assert data["target"] == host.target
        assert data["access"] == "runtime_exec"
        if host.runtime == "apple_container":
            assert data["status"] in {"container-running", "not-running"}
        else:
            assert data["status"] == "container-running"
            assert data["container"]

        all_status = runtime_exec_gateway(host, "status", "--all", "--json", timeout=120)
        all_status.assert_success()
        entries = json.loads(all_status.stdout)
        matching = [entry for entry in entries if entry["target"] == host.target]
        assert matching
        assert matching[0]["access"] == "runtime_exec"

        client_config = runtime_exec_gateway(host, "client-config", host.target)
        assert client_config.returncode != 0
        assert "runtime_exec" in client_config.stderr
        assert "requires an SSH target" in client_config.stderr
    finally:
        runtime_exec_gateway(host, "remove", host.target, timeout=180)
