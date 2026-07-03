from __future__ import annotations

from dataclasses import dataclass
import shlex

import pytest

from awsmoke.gateway import gateway_command_for_config
from awsmoke.hosts import Host


@dataclass(frozen=True)
class TransferPolicy:
    name: str
    sftp: str
    legacy_scp: str
    expect_sftp: bool
    expect_modern_scp: bool
    expect_legacy_upload: bool
    expect_legacy_download: bool


POLICIES = [
    TransferPolicy("sftp-allow-legacy-allow", "allow", "allow", True, True, True, True),
    TransferPolicy("sftp-allow-legacy-deny", "allow", "deny", True, True, False, False),
    TransferPolicy("sftp-deny-legacy-allow", "deny", "allow", False, False, True, True),
    TransferPolicy("sftp-allow-legacy-inbound", "allow", "inbound", True, True, True, False),
    TransferPolicy("sftp-allow-legacy-outbound", "allow", "outbound", True, True, False, True),
    TransferPolicy("sftp-deny-legacy-deny", "deny", "deny", False, False, False, False),
]


@pytest.mark.parametrize("policy", POLICIES, ids=[policy.name for policy in POLICIES])
def test_container_ssh_transfer_policy(host: Host, policy: TransferPolicy) -> None:
    gateway = f'{shlex.quote(host.gateway_path)} --config "${{config}}"'
    target = shlex.quote(host.target)
    alias = shlex.quote(f"aw-{host.target}")
    local_config = shlex.quote(host.local_config_path)
    sftp_rewrite = shlex.quote(f's/sftp = "[^"]*"/sftp = "{policy.sftp}"/')
    legacy_rewrite = shlex.quote(f's/legacy_scp = "[^"]*"/legacy_scp = "{policy.legacy_scp}"/')
    sftp_line = shlex.quote(f'sftp = "{policy.sftp}"')
    legacy_line = shlex.quote(f'legacy_scp = "{policy.legacy_scp}"')
    content = shlex.quote(f"aw-gateway-transfer-{host.name}-{policy.name}")

    script = f"""
set -euo pipefail
tmp=$(mktemp -d)
up_pid=
cleanup() {{
  if [ -n "${{up_pid}}" ]; then
    kill "${{up_pid}}" >/dev/null 2>&1 || true
    wait "${{up_pid}}" >/dev/null 2>&1 || true
  fi
  {gateway} stop {target} >/dev/null 2>&1 || true
  {gateway} remove {target} >/dev/null 2>&1 || true
  rm -rf "${{tmp}}"
}}
trap cleanup EXIT

# --- rewrite transfer policy in a temporary config ---
config="${{tmp}}/gateway-transfer.toml"
cp {local_config} "${{config}}"
sed -i.bak -e {sftp_rewrite} -e {legacy_rewrite} "${{config}}"
grep -q {sftp_line} "${{config}}"
grep -q {legacy_line} "${{config}}"

# --- make the command filter path explicit for already-deployed configs ---
filter_path=$(awk -F'"' '/target = ".*aw-ssh-command-filter"/ {{ print $2; exit }}' "${{config}}")
if [ -n "${{filter_path}}" ] && ! grep -q 'AW_SSH_COMMAND_FILTER' "${{config}}"; then
  awk -v filter_path="${{filter_path}}" '
    function emit_filter() {{
      if (in_sshd && !inserted) {{
        print "[target_defaults.container_agent.services.env.AW_SSH_COMMAND_FILTER]"
        print "value = \\"" filter_path "\\""
        print ""
        inserted = 1
      }}
    }}
    {{
      if ($0 ~ /^\\[\\[target_defaults\\.container_agent\\.services\\]\\]$/) {{
        emit_filter()
        in_service = 1
        in_sshd = 0
        print
        next
      }}
      if ($0 ~ /^\\[/ && $0 !~ /^\\[target_defaults\\.container_agent\\.services\\./) {{
        emit_filter()
        in_service = 0
        in_sshd = 0
        print
        next
      }}
      print
      if (in_service && $0 ~ /^name = "container-sshd"$/) {{
        in_sshd = 1
      }}
    }}
    END {{
      emit_filter()
    }}
  ' "${{config}}" >"${{config}}.env"
  mv "${{config}}.env" "${{config}}"
fi
grep -q 'AW_SSH_COMMAND_FILTER' "${{config}}"

# --- start the target and wait for a usable host-local client bundle ---
ssh_config="${{tmp}}/ssh_config"
alias_name={alias}
source_file="${{tmp}}/source.txt"
printf '%s\\n' {content} > "${{source_file}}"

{gateway} remove {target} >/dev/null 2>&1 || true
{gateway} up {target} --json >"${{tmp}}/up.json" 2>"${{tmp}}/up.err" &
up_pid=$!

ready=
for _ in $(seq 1 120); do
  if ! kill -0 "${{up_pid}}" >/dev/null 2>&1; then
    cat "${{tmp}}/up.err" >&2 || true
    exit 1
  fi
  if bundle=$({gateway} client-bundle {target} 2>"${{tmp}}/bundle.err"); then
    if [ -s "${{bundle}}/ssh_config" ]; then
      key=$(find "${{bundle}}" -maxdepth 1 -type f -name '*_inner_ed25519' ! -name '*.pub' | head -n 1)
      if [ -s "${{key}}" ] && [ -s "${{key}}.pub" ]; then
        {gateway} add-container-key {target} --public-key "${{key}}.pub" >/dev/null
        awk -v key="${{key}}" '
          $1 == "IdentityFile" {{ print "    IdentityFile " key; next }}
          {{ print }}
        ' "${{bundle}}/ssh_config" >"${{ssh_config}}"
        if ssh -F "${{ssh_config}}" -o BatchMode=yes "${{alias_name}}" true; then
          ready=1
          break
        fi
      fi
    fi
  fi
  sleep 1
done

if [ "${{ready}}" != "1" ]; then
  cat "${{tmp}}/up.err" >&2 || true
  cat "${{tmp}}/bundle.err" >&2 || true
  exit 1
fi

# --- shared assertion helpers ---
container_ssh() {{
  ssh -F "${{ssh_config}}" -o BatchMode=yes "${{alias_name}}" "$@"
}}

dump_case() {{
  local label="$1"
  cat "${{tmp}}/${{label}}.out" >&2 2>/dev/null || true
  cat "${{tmp}}/${{label}}.err" >&2 2>/dev/null || true
}}

expect_success() {{
  local label="$1"
  shift
  if "$@" >"${{tmp}}/${{label}}.out" 2>"${{tmp}}/${{label}}.err"; then
    return 0
  fi
  echo "expected success for ${{label}}" >&2
  dump_case "${{label}}"
  exit 1
}}

expect_failure() {{
  local label="$1"
  shift
  if "$@" >"${{tmp}}/${{label}}.out" 2>"${{tmp}}/${{label}}.err"; then
    echo "expected failure for ${{label}}" >&2
    dump_case "${{label}}"
    exit 1
  fi
}}

expect_result() {{
  local expected="$1"
  local label="$2"
  shift 2
  if [ "${{expected}}" = "yes" ]; then
    expect_success "${{label}}" "$@"
  else
    expect_failure "${{label}}" "$@"
  fi
}}

verify_uploaded() {{
  local label="$1"
  local remote_file="$2"
  expect_success "${{label}}-cat" container_ssh "cat '${{remote_file}}'"
  if ! cmp -s "${{source_file}}" "${{tmp}}/${{label}}-cat.out"; then
    echo "uploaded content mismatch for ${{label}}" >&2
    exit 1
  fi
}}

prepare_remote_file() {{
  local remote_file="$1"
  expect_success "prepare-${{remote_file}}" container_ssh "printf '%s\\n' {content} > '${{remote_file}}'"
}}

verify_downloaded() {{
  local label="$1"
  local downloaded="$2"
  if ! cmp -s "${{source_file}}" "${{downloaded}}"; then
    echo "downloaded content mismatch for ${{label}}" >&2
    exit 1
  fi
}}

remote_prefix="aw-smoke-{policy.name}"

# --- SFTP and default OpenSSH scp both use the SFTP subsystem ---
sftp_remote="${{remote_prefix}}-sftp.txt"
sftp_download="${{tmp}}/sftp-download.txt"
printf 'put %s %s\\nget %s %s\\n' "${{source_file}}" "${{sftp_remote}}" "${{sftp_remote}}" "${{sftp_download}}" >"${{tmp}}/sftp.batch"
expect_result {"yes" if policy.expect_sftp else "no"} sftp-roundtrip sftp -F "${{ssh_config}}" -b "${{tmp}}/sftp.batch" "${{alias_name}}"
if [ {"yes" if policy.expect_sftp else "no"} = "yes" ]; then
  verify_downloaded sftp-roundtrip "${{sftp_download}}"
fi

modern_upload_remote="${{remote_prefix}}-modern-upload.txt"
expect_result {"yes" if policy.expect_modern_scp else "no"} modern-scp-upload scp -F "${{ssh_config}}" -o BatchMode=yes "${{source_file}}" "${{alias_name}}:${{modern_upload_remote}}"
if [ {"yes" if policy.expect_modern_scp else "no"} = "yes" ]; then
  verify_uploaded modern-scp-upload "${{modern_upload_remote}}"
fi

modern_download_remote="${{remote_prefix}}-modern-download.txt"
modern_download="${{tmp}}/modern-download.txt"
prepare_remote_file "${{modern_download_remote}}"
expect_result {"yes" if policy.expect_modern_scp else "no"} modern-scp-download scp -F "${{ssh_config}}" -o BatchMode=yes "${{alias_name}}:${{modern_download_remote}}" "${{modern_download}}"
if [ {"yes" if policy.expect_modern_scp else "no"} = "yes" ]; then
  verify_downloaded modern-scp-download "${{modern_download}}"
fi

# --- legacy scp -O uses scp -t for upload and scp -f for download ---
legacy_upload_remote="${{remote_prefix}}-legacy-upload.txt"
expect_result {"yes" if policy.expect_legacy_upload else "no"} legacy-scp-upload scp -O -F "${{ssh_config}}" -o BatchMode=yes "${{source_file}}" "${{alias_name}}:${{legacy_upload_remote}}"
if [ {"yes" if policy.expect_legacy_upload else "no"} = "yes" ]; then
  verify_uploaded legacy-scp-upload "${{legacy_upload_remote}}"
fi

legacy_download_remote="${{remote_prefix}}-legacy-download.txt"
legacy_download="${{tmp}}/legacy-download.txt"
prepare_remote_file "${{legacy_download_remote}}"
expect_result {"yes" if policy.expect_legacy_download else "no"} legacy-scp-download scp -O -F "${{ssh_config}}" -o BatchMode=yes "${{alias_name}}:${{legacy_download_remote}}" "${{legacy_download}}"
if [ {"yes" if policy.expect_legacy_download else "no"} = "yes" ]; then
  verify_downloaded legacy-scp-download "${{legacy_download}}"
fi
"""
    result = host.run(script, timeout=600)
    result.assert_success()
