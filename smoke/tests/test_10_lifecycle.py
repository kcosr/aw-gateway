from __future__ import annotations

import json

from awsmoke.gateway import gateway
from awsmoke.hosts import Host


def test_gateway_config_validates(host: Host) -> None:
    result = gateway(host, "config", "validate")
    result.assert_success()
    assert "ok" in result.stdout.lower()


def test_gateway_config_paths_reports_explicit_config(host: Host) -> None:
    result = gateway(host, "config", "paths", "--json")
    result.assert_success()
    data = json.loads(result.stdout)
    assert data["selected_source"] == "explicit_flag"
    assert data["selected_path"] == host.config_path
    assert data["candidates"][0]["source"] == "explicit_flag"
    assert data["candidates"][0]["path"] == host.config_path
    assert data["candidates"][0]["exists"] is True
    assert data["user_config_file"].endswith("/aw-gateway/gateway.toml")
    assert data["system_config_file"] == "/etc/aw-gateway/gateway.toml"


def test_gateway_lists_targets(host: Host) -> None:
    result = gateway(host, "targets", "--json")
    result.assert_success()
    targets = json.loads(result.stdout)
    names = {target["target"] for target in targets}
    assert host.target in names


def test_gateway_run_status_stop(host: Host) -> None:
    gateway(host, "remove", host.target, timeout=180)

    run = gateway(host, "run", host.target, "--", "id", timeout=300)
    run.assert_success()
    assert "uid=" in run.stdout

    status = gateway(host, "status", host.target, "--json", timeout=120)
    status.assert_success()
    data = json.loads(status.stdout)
    assert data["target"] == host.target

    stop = gateway(host, "stop", host.target, timeout=180)
    stop.assert_success()
