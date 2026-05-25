from __future__ import annotations

import json
import shlex

from awsmoke.gateway import gateway
from awsmoke.hosts import Host
from awsmoke.ssh import remote


def test_gateway_config_validates(host: Host) -> None:
    result = gateway(host, "config", "validate")
    result.assert_success()
    assert "ok" in result.stdout.lower()


def test_gateway_config_paths_reports_explicit_config(host: Host) -> None:
    result = gateway(host, "config", "paths", "--json")
    result.assert_success()
    data = json.loads(result.stdout)
    candidates = data["candidates"]
    assert data["selected_source"] == "explicit_flag"
    assert data["selected_path"] == host.config_path
    assert [candidate["source"] for candidate in candidates] == [
        "explicit_flag",
        "user",
        "system",
    ]
    assert candidates[0]["path"] == host.config_path
    assert candidates[0]["exists"] is True
    assert candidates[1]["path"] == data["user_config_file"]
    assert candidates[2]["path"] == data["system_config_file"]
    assert data["user_config_file"].endswith("/aw-gateway/gateway.toml")
    assert data["system_config_file"] == "/etc/aw-gateway/gateway.toml"


def test_gateway_config_paths_selects_temporary_user_config(host: Host) -> None:
    command = f"""
set -eu
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/aw-gateway"
cp {shlex.quote(host.config_path)} "$tmp/aw-gateway/gateway.toml"
AW_GATEWAY_CONFIG_HOME="$tmp" {shlex.quote(host.gateway_path)} config paths --json
"""
    result = remote(host.ssh, command)
    result.assert_success()
    data = json.loads(result.stdout)
    candidates = data["candidates"]
    assert data["selected_source"] == "user"
    assert data["selected_path"] == data["user_config_file"]
    assert [candidate["source"] for candidate in candidates] == ["user", "system"]
    assert candidates[0]["path"] == data["user_config_file"]
    assert candidates[0]["exists"] is True
    assert data["user_config_file"].endswith("/aw-gateway/gateway.toml")
    assert data["system_config_file"] == "/etc/aw-gateway/gateway.toml"


def test_gateway_config_extends_deployed_config(host: Host) -> None:
    command = f"""
set -eu
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cp {shlex.quote(host.config_path)} "$tmp/base.toml"
cat > "$tmp/child.toml" <<'EOF'
extends = "base.toml"

[launches.smoke-extends]
target = "{host.target}"
command = ["true"]
EOF
{shlex.quote(host.gateway_path)} --config "$tmp/child.toml" config validate >/dev/null
{shlex.quote(host.gateway_path)} --config "$tmp/child.toml" launch show smoke-extends --json
"""
    result = remote(host.ssh, command)
    result.assert_success()
    data = json.loads(result.stdout)
    assert data["name"] == "smoke-extends"
    assert data["target"] == host.target


def test_gateway_config_accepts_template_scoped_identity_and_service_user(host: Host) -> None:
    command = f"""
set -eu
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cp {shlex.quote(host.config_path)} "$tmp/base.toml"
cat > "$tmp/child.toml" <<'EOF'
extends = "base.toml"

[targets.{host.target}]
container_home = "/tmp/aw-smoke-{{user}}"

[[targets.{host.target}.container_agent.services]]
name = "smoke-template-user"
required = false
user = "{{container_user}}"
command = ["sleep", "infinity"]
EOF
{shlex.quote(host.gateway_path)} --config "$tmp/child.toml" config validate
"""
    result = remote(host.ssh, command)
    result.assert_success()
    assert "ok" in result.stdout.lower()


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
