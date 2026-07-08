from __future__ import annotations

import shlex
import uuid

from awsmoke.gateway import gateway_command_for_config
from awsmoke.hosts import Host


def test_direct_published_ssh_config_validates(host: Host) -> None:
    config = _write_direct_config(host)
    try:
        result = _remote_gateway(host, config, "config", "validate", timeout=60)
        result.assert_success()
        assert "ok" in result.stdout.lower()
    finally:
        _rm(host, config)


def test_direct_published_ssh_up_status_and_host_ssh(host: Host) -> None:
    config = _write_direct_config(host)
    gateway = gateway_command_for_config(host, config)
    target = shlex.quote(host.target)
    alias = shlex.quote(f"aw-{host.target}")
    try:
        script = f"""
set -euo pipefail
tmp="$(mktemp -d)"
cleanup() {{
  {gateway} remove {target} >/dev/null 2>&1 || true
  rm -rf "${{tmp}}"
}}
trap cleanup EXIT

{gateway} remove {target} >/dev/null 2>&1 || true

if ! {gateway} up {target} --json >"${{tmp}}/up.json" 2>"${{tmp}}/up.err"; then
  cat "${{tmp}}/up.err" >&2
  exit 1
fi
python3 - "${{tmp}}/up.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1]))
assert data["target"]
assert data["access"] == "ssh"
assert "ssh_socket" not in data
assert data["local_ssh"]["host"] == "127.0.0.1"
assert data["ssh_tcp"]["host"] == "127.0.0.1"
assert data["local_ssh"]["port"] == data["ssh_tcp"]["port"]
PY

bundle="$({gateway} client-bundle {target})"
key="$(find "${{bundle}}" -maxdepth 1 -type f -name '*_inner_ed25519' ! -name '*.pub' | head -n 1)"
test -s "${{key}}"
test -s "${{key}}.pub"
{gateway} add-container-key {target} --public-key "${{key}}.pub" >/dev/null
awk -v key="${{key}}" '
  $1 == "IdentityFile" {{ print "    IdentityFile " key; next }}
  {{ print }}
' "${{bundle}}/ssh_config" >"${{tmp}}/ssh_config"
grep -q 'HostName 127.0.0.1' "${{tmp}}/ssh_config"
grep -q '^    Port ' "${{tmp}}/ssh_config"
if grep -q 'ProxyCommand' "${{tmp}}/ssh_config"; then
  echo "direct SSH config unexpectedly contains ProxyCommand" >&2
  exit 1
fi

ssh -F "${{tmp}}/ssh_config" -o BatchMode=yes {alias} id
echo "direct-ssh-ok"

{gateway} status {target} --json >"${{tmp}}/running.json"
python3 - "${{tmp}}/running.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1]))
assert data["status"] in {{"container-running", "ready"}}
assert data["local_ssh"]["host"] == "127.0.0.1"
assert data["ssh_tcp"]["host"] == "127.0.0.1"
assert data["local_ssh"]["port"] == data["ssh_tcp"]["port"]
PY

{gateway} stop {target} >/dev/null
{gateway} status {target} --json >"${{tmp}}/stopped.json"
python3 - "${{tmp}}/stopped.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1]))
assert data["status"] == "not-running"
assert data["local_ssh"]["host"] == "127.0.0.1"
assert "ssh_tcp" not in data
PY
"""
        result = host.run(script, timeout=420)
        result.assert_success()
        assert "direct-ssh-ok" in result.stdout
        assert "uid=" in result.stdout
    finally:
        _remote_gateway(host, config, "remove", host.target, timeout=180)
        _rm(host, config)


def _write_direct_config(host: Host) -> str:
    config = f"/tmp/aw-gateway-smoke-direct-{host.name}-{uuid.uuid4().hex}.toml"
    env = {
        "SOURCE_CONFIG": host.local_config_path,
        "CONFIG": config,
        "TARGET": host.target,
    }
    assignments = " ".join(f"{key}={shlex.quote(value)}" for key, value in env.items())
    script = f"""
set -euo pipefail
{assignments} python3 - <<'PY'
import json
import os
from pathlib import Path

target = os.environ["TARGET"]
text = f"extends = {{json.dumps(os.environ['SOURCE_CONFIG'])}}\\n"
text += f"\\n[targets.{{target}}]\\n"
text += "stop_when_idle = false\\n"
text += f"\\n[targets.{{target}}.local_ssh]\\n"
text += 'mode = "direct"\\n'
text += 'backend = "published_port"\\n'
text += 'readiness = "ssh_only"\\n'
text += 'host = "127.0.0.1"\\n'
text += f"\\n[targets.{{target}}.idle_cleanup]\\n"
text += 'owner = "none"\\n'
text += 'action = "none"\\n'
text += f"\\n[[targets.{{target}}.container_agent.services]]\\n"
text += 'name = "container-sshd"\\n'
text += "required = true\\n"
text += 'user = "root"\\n'
text += 'command = ["/opt/aw-gateway/bin/start-container-sshd"]\\n'
text += 'restart = "always"\\n'
text += "depends_on = []\\n"
text += f"\\n[targets.{{target}}.container_agent.services.health_check]\\n"
text += 'type = "tcp"\\n'
text += 'host = "127.0.0.1"\\n'
text += "port = 22\\n"
text += 'interval = "2s"\\n'
text += 'timeout = "1s"\\n'
text += f"\\n[targets.{{target}}.container_agent.services.env.AW_SSHD_LISTEN_ADDRESS]\\n"
text += 'value = "0.0.0.0"\\n'
Path(os.environ["CONFIG"]).write_text(text)
PY
"""
    host.check(script, timeout=60)
    return config


def _remote_gateway(
    host: Host,
    config: str,
    *args: str,
    timeout: int,
):
    return host.run(gateway_command_for_config(host, config, *args), timeout=timeout)


def _rm(host: Host, path: str) -> None:
    host.run(f"rm -f {shlex.quote(path)}", timeout=30)
