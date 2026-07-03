#!/usr/bin/env bash
set -Eeuo pipefail

export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
SMOKE_ROOT="${AWGATEWAY_SMOKE_ROOT:-$HOME/aw-gateway-apple-smoke}"
RUN_ROOT="${AWGATEWAY_SMOKE_RUN_ROOT:-$SMOKE_ROOT/runs/$STAMP}"
LOG_DIR="${RUN_ROOT}/logs"
SANITIZED_DIR="${RUN_ROOT}/sanitized"
INSTALL_ROOT="${AWGATEWAY_INSTALL_ROOT:-$RUN_ROOT/install}"
INVENTORY="${RUN_ROOT}/inventory.toml"
VENV="${RUN_ROOT}/venv"
HOST_NAME="${AWGATEWAY_APPLE_SMOKE_HOST:-macos-apple-container}"
TARGET="${AWGATEWAY_APPLE_SMOKE_TARGET:-ubuntu}"
IMAGE="${AWGATEWAY_APPLE_SMOKE_IMAGE:-aw-gateway/ubuntu-base:latest}"
CONTAINER_NAME="aw-gateway-ubuntu-base"
CONFIG="${INSTALL_ROOT}/etc/gateway.toml"
GATEWAY="${INSTALL_ROOT}/bin/aw-gateway"
ARCHIVE=""
SMOKE_STATUS="failed"

note() {
  printf '\n== %s\n' "$*"
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

select_python() {
  local candidates=()
  if [[ -n "${AWGATEWAY_PYTHON:-}" ]]; then
    candidates+=("$AWGATEWAY_PYTHON")
  fi
  candidates+=(python3.13 python3.12 python3.11 python3)

  local candidate
  for candidate in "${candidates[@]}"; do
    command -v "$candidate" >/dev/null 2>&1 || continue
    if "$candidate" - <<'PY' >/dev/null 2>&1
import sys
raise SystemExit(0 if sys.version_info >= (3, 9) else 1)
PY
    then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  fail "Python 3.9+ is required for the smoke harness"
}

log_command() {
  local path=$1
  shift
  {
    printf '$'
    printf ' %q' "$@"
    printf '\n'
  } >"$path"
}

run_step() {
  local name=$1
  shift
  note "$name"
  log_command "${LOG_DIR}/${name}.cmd" "$@"
  "$@" >"${LOG_DIR}/${name}.stdout" 2>"${LOG_DIR}/${name}.stderr"
}

capture_optional() {
  local name=$1
  shift
  local status
  log_command "${LOG_DIR}/${name}.cmd" "$@"
  if "$@" >"${LOG_DIR}/${name}.stdout" 2>"${LOG_DIR}/${name}.stderr"; then
    status=0
  else
    status=$?
  fi
  printf '%s\n' "$status" >"${LOG_DIR}/${name}.exit"
}

write_inventory() {
  cat >"$INVENTORY" <<EOF
[defaults]
repo_root = "$REPO_ROOT"
generated_dir = "$RUN_ROOT/generated"
target = "$TARGET"
gateway_config = "etc/gateway.toml"

[hosts.$HOST_NAME]
enabled = true
transport = "local"
runtime = "apple_container"
install_root = "$INSTALL_ROOT"
install_mode = "user"
requires_sudo = false
config_example = "examples/apple-container/gateway-local.toml"
image_context = "examples/apple-container"
image = "$IMAGE"
restricted_user = "awsmoke"
restricted_install_root = "$RUN_ROOT/restricted"
EOF
}

write_metadata() {
  {
    printf 'status=%s\n' "$SMOKE_STATUS"
    printf 'timestamp=%s\n' "$STAMP"
    printf 'repo_root=%s\n' "$REPO_ROOT"
    printf 'run_root=%s\n' "$RUN_ROOT"
    printf 'install_root=%s\n' "$INSTALL_ROOT"
    printf 'host=%s\n' "$HOST_NAME"
    printf 'target=%s\n' "$TARGET"
    printf 'image=%s\n' "$IMAGE"
    if [[ -d "${REPO_ROOT}/.git" ]]; then
      printf 'commit=%s\n' "$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || true)"
      printf 'branch=%s\n' "$(git -C "$REPO_ROOT" branch --show-current 2>/dev/null || true)"
    fi
    if [[ -n "$ARCHIVE" ]]; then
      printf 'archive=%s\n' "$ARCHIVE"
    fi
  } >"${RUN_ROOT}/metadata.txt"
}

capture_diagnostics() {
  local phase=$1
  capture_optional "${phase}_container_system_version" container system version --format json
  capture_optional "${phase}_container_system_status" container system status --format json
  capture_optional "${phase}_container_list_json" container list --all --format json
  capture_optional "${phase}_container_inspect" container inspect "$CONTAINER_NAME"
  capture_optional "${phase}_container_logs" container logs -n 400 "$CONTAINER_NAME"
  capture_optional "${phase}_container_boot_logs" container logs --boot -n 400 "$CONTAINER_NAME"
  capture_optional "${phase}_container_system_logs_tail" sh -lc 'container system logs 2>&1 | tail -300'
}

sanitize_results() {
  rm -rf "$SANITIZED_DIR"
  mkdir -p "$SANITIZED_DIR"

  cp "${RUN_ROOT}/metadata.txt" "${SANITIZED_DIR}/metadata.txt" 2>/dev/null || true
  cp "$INVENTORY" "${SANITIZED_DIR}/inventory.toml" 2>/dev/null || true
  if [[ -d "$LOG_DIR" ]]; then
    mkdir -p "${SANITIZED_DIR}/logs"
    cp -R "${LOG_DIR}/." "${SANITIZED_DIR}/logs/"
  fi
  if [[ -d "${RUN_ROOT}/generated" ]]; then
    mkdir -p "${SANITIZED_DIR}/generated"
    cp -R "${RUN_ROOT}/generated/." "${SANITIZED_DIR}/generated/"
  fi
  if [[ -f "$CONFIG" ]]; then
    cp "$CONFIG" "${SANITIZED_DIR}/gateway.toml"
  fi
  if [[ -d "${INSTALL_ROOT}/workspace/.aw-gateway/logs" ]]; then
    mkdir -p "${SANITIZED_DIR}/workspace-logs"
    cp -R "${INSTALL_ROOT}/workspace/.aw-gateway/logs/." "${SANITIZED_DIR}/workspace-logs/"
  fi

  local full_name host_name
  full_name="$(id -F 2>/dev/null || true)"
  host_name="$(hostname 2>/dev/null || true)"

  export SAN_REPO_ROOT="$REPO_ROOT"
  export SAN_RUN_ROOT="$RUN_ROOT"
  export SAN_INSTALL_ROOT="$INSTALL_ROOT"
  export SAN_HOME="$HOME"
  export SAN_USER="${USER:-}"
  export SAN_FULL_NAME="$full_name"
  export SAN_HOSTNAME="$host_name"

  find "$SANITIZED_DIR" -type f -print0 | xargs -0 perl -0pi -e '
    for my $pair (
      [$ENV{SAN_REPO_ROOT}, "<aw-gateway-repo>"],
      [$ENV{SAN_RUN_ROOT}, "<aw-gateway-smoke-run>"],
      [$ENV{SAN_INSTALL_ROOT}, "<aw-gateway-smoke-install>"],
      [$ENV{SAN_HOME}, "~"],
      [$ENV{SAN_FULL_NAME}, "mac-full-name"],
      [$ENV{SAN_USER}, "mac-user"],
      [$ENV{SAN_HOSTNAME}, "mac-host"],
    ) {
      my ($from, $to) = @$pair;
      next unless defined $from && length $from;
      s/\Q$from\E/$to/g;
    }
    s/(AW_(?:IDENTITY|CONTAINER_CONTROL)_TOKEN[[:space:]]*=[[:space:]]*")[^"]+/$1<redacted>/g;
    s/(token[[:space:]]*=[[:space:]]*")[^"]+/$1<redacted>/g;
  '
}

finalize() {
  local exit_code=$1
  set +e

  capture_diagnostics final

  if [[ -x "$GATEWAY" && -f "$CONFIG" ]]; then
    "$GATEWAY" --config "$CONFIG" remove "$TARGET" \
      >"${LOG_DIR}/cleanup_remove.stdout" 2>"${LOG_DIR}/cleanup_remove.stderr" || true
  fi
  if [[ "${AWGATEWAY_DELETE_CONTAINER:-1}" == "1" ]]; then
    capture_optional cleanup_delete_container container delete --force "$CONTAINER_NAME"
  fi

  ARCHIVE="${RUN_ROOT}/aw-gateway-apple-smoke-${STAMP}-sanitized.tar.gz"
  write_metadata
  sanitize_results
  tar -C "$SANITIZED_DIR" -czf "$ARCHIVE" . \
    >"${LOG_DIR}/archive.stdout" 2>"${LOG_DIR}/archive.stderr"

  printf '\nSanitized archive: %s\n' "$ARCHIVE"
  if [[ -n "${AWGATEWAY_APPLE_SMOKE_ARCHIVE_PATH_FILE:-}" ]]; then
    printf '%s\n' "$ARCHIVE" >"${AWGATEWAY_APPLE_SMOKE_ARCHIVE_PATH_FILE}"
  fi

  return "$exit_code"
}

trap 'code=$?; finalize "$code"; exit "$code"' EXIT

mkdir -p "$LOG_DIR" "$INSTALL_ROOT"

note "preflight"
[[ "$(uname -s)" == "Darwin" ]] || fail "this smoke script must run on macOS"
[[ "$(uname -m)" == "arm64" ]] || fail "Apple container requires Apple silicon arm64"

need cargo
need container
need git
need perl
need ssh
need scp
need tar
PYTHON_BIN="$(select_python)"

if command -v xcrun >/dev/null 2>&1; then
  xcrun --find cc >/dev/null 2>&1 || fail "Xcode command-line tools are required; run: xcode-select --install"
fi

write_inventory
write_metadata

run_step 00_container_system_version container system version --format json
run_step 01_container_system_status container system status --format json
run_step 02_python_version "$PYTHON_BIN" --version
run_step 03_create_venv "$PYTHON_BIN" -m venv "$VENV"
run_step 04_upgrade_pip "$VENV/bin/python" -m pip install --upgrade pip
run_step 05_install_smoke "$VENV/bin/python" -m pip install -e "${REPO_ROOT}/smoke"
run_step 06_awsmoke_hosts "$VENV/bin/awsmoke" --inventory "$INVENTORY" hosts
run_step 07_awsmoke_deploy "$VENV/bin/awsmoke" --inventory "$INVENTORY" deploy "$HOST_NAME"
run_step 08_pytest "$VENV/bin/python" -m pytest --inventory "$INVENTORY" --host "$HOST_NAME" -q "${REPO_ROOT}/smoke/tests"

SMOKE_STATUS="passed"
note "smoke_passed"
