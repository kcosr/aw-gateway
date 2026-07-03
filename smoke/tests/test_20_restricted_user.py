from __future__ import annotations

import pytest

from awsmoke.hosts import Host
from awsmoke.ssh import remote


def restricted_host(host: Host) -> str:
    if host.transport != "ssh":
        pytest.skip("restricted ForceCommand coverage requires an SSH smoke transport")
    if host.runtime == "colima":
        pytest.skip("restricted ForceCommand coverage is Linux-only for now")
    return f"{host.restricted_user}@{host.ssh}"


def test_restricted_user_gateway_help(host: Host) -> None:
    result = remote(restricted_host(host), "help")
    result.assert_success()
    assert "aw-gateway" in result.stdout.lower() or "commands" in result.stdout.lower()


def test_restricted_user_lists_targets(host: Host) -> None:
    result = remote(restricted_host(host), "targets")
    result.assert_success()
    assert host.target in result.stdout


def test_restricted_user_runs_container_command(host: Host) -> None:
    result = remote(restricted_host(host), f"run {host.target} -- id", timeout=300)
    result.assert_success()
    assert "uid=" in result.stdout
