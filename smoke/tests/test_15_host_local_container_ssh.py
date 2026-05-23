from __future__ import annotations

import shlex

from awsmoke.gateway import local_gateway, local_gateway_command
from awsmoke.hosts import Host
from awsmoke.ssh import remote


def test_local_gateway_config_validates(host: Host) -> None:
    result = local_gateway(host, "config", "validate")
    result.assert_success()
    assert "ok" in result.stdout.lower()


def test_host_can_ssh_to_container_without_host_gateway_sshd(host: Host) -> None:
    gateway = local_gateway_command(host)
    alias = f"aw-{host.target}"
    script = f"""
set -euo pipefail
tmp=$(mktemp -d)
up_pid=
cleanup() {{
  if [ -n "${{up_pid}}" ]; then
    kill "${{up_pid}}" >/dev/null 2>&1 || true
    wait "${{up_pid}}" >/dev/null 2>&1 || true
  fi
  {gateway} stop {shlex.quote(host.target)} >/dev/null 2>&1 || true
  rm -rf "${{tmp}}"
}}
trap cleanup EXIT
{gateway} remove {shlex.quote(host.target)} >/dev/null 2>&1 || true
{gateway} up {shlex.quote(host.target)} --json >"${{tmp}}/up.json" 2>"${{tmp}}/up.err" &
up_pid=$!
for _ in $(seq 1 90); do
  if ! kill -0 "${{up_pid}}" >/dev/null 2>&1; then
    cat "${{tmp}}/up.err" >&2
    exit 1
  fi
  if bundle=$({gateway} client-bundle {shlex.quote(host.target)} 2>"${{tmp}}/bundle.err"); then
    if [ -s "${{bundle}}/ssh_config" ]; then
      key=$(find "${{bundle}}" -maxdepth 1 -type f -name '*_inner_ed25519' ! -name '*.pub' | head -n 1)
      if [ ! -s "${{key}}" ] || [ ! -s "${{key}}.pub" ]; then
        sleep 1
        continue
      fi
      {gateway} add-container-key {shlex.quote(host.target)} --public-key "${{key}}.pub" >/dev/null
      awk -v key="${{key}}" '
        $1 == "IdentityFile" {{ print "    IdentityFile " key; next }}
        {{ print }}
      ' "${{bundle}}/ssh_config" >"${{tmp}}/ssh_config"
      if ssh -F "${{tmp}}/ssh_config" -o BatchMode=yes {shlex.quote(alias)} id; then
        exit 0
      fi
    fi
  fi
  sleep 1
done
cat "${{tmp}}/up.err" >&2 || true
cat "${{tmp}}/bundle.err" >&2 || true
exit 1
"""
    result = remote(host.ssh, script, timeout=420)
    result.assert_success()
    assert "uid=" in result.stdout
