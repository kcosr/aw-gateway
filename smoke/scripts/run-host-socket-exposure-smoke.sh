#!/usr/bin/env bash

set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
GATEWAY_BIN=${AW_HOST_SOCKET_SMOKE_GATEWAY_BIN:-$ROOT/target/debug/aw-gateway}
IMAGE=${AW_HOST_SOCKET_SMOKE_IMAGE:-python:3.12-slim}
DOCKER_HOST_URI=${AW_HOST_SOCKET_SMOKE_DOCKER_HOST:-${DOCKER_HOST:-unix:///var/run/docker.sock}}
TMP_DIR=
CONTAINER_NAME=
SERVER_PID=
SUCCESS=0

fail() {
    printf 'host socket exposure smoke failed: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    local status=$?
    trap - EXIT
    set +e
    if (( status == 0 && SUCCESS != 1 )); then
        printf '%s\n' 'host socket exposure smoke exited without its success marker' >&2
        status=1
    fi
    if [[ -n $CONTAINER_NAME ]]; then
        docker --host "$DOCKER_HOST_URI" rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
    fi
    if [[ -n $SERVER_PID ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if (( status != 0 )) && [[ ${AW_HOST_SOCKET_SMOKE_KEEP_FAILED:-0} == 1 ]]; then
        printf 'preserving failed smoke directory: %s\n' "$TMP_DIR" >&2
    else
        [[ -z $TMP_DIR ]] || rm -rf -- "$TMP_DIR"
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

[[ $(uname -s) == Linux ]] || fail 'this smoke requires a native Linux Docker host'
case "$DOCKER_HOST_URI" in
    unix:///*) ;;
    *) fail "Docker endpoint must be a local Unix socket: $DOCKER_HOST_URI" ;;
esac
DOCKER_SOCKET=${DOCKER_HOST_URI#unix://}
[[ -S $DOCKER_SOCKET ]] || fail "Docker endpoint is not a Unix socket: $DOCKER_SOCKET"
unset DOCKER_CONTEXT
export DOCKER_HOST=$DOCKER_HOST_URI

for command in cargo docker python3; do
    command -v "$command" >/dev/null 2>&1 || fail "required command is unavailable: $command"
done
docker --host "$DOCKER_HOST_URI" info >/dev/null 2>&1 \
    || fail "Docker daemon is unavailable at $DOCKER_HOST_URI"
docker --host "$DOCKER_HOST_URI" image inspect "$IMAGE" >/dev/null 2>&1 \
    || fail "required local image is unavailable (refusing an implicit pull): $IMAGE"

if [[ $GATEWAY_BIN == "$ROOT/target/debug/aw-gateway" ]]; then
    cargo build --quiet --locked --manifest-path "$ROOT/Cargo.toml" --bin aw-gateway \
        || fail 'locked AW Gateway build failed'
fi
[[ $GATEWAY_BIN == /* && -f $GATEWAY_BIN && -x $GATEWAY_BIN && ! -L $GATEWAY_BIN ]] \
    || fail "gateway binary must be an absolute executable regular file: $GATEWAY_BIN"

umask 077
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/aw-host-socket.XXXXXX")
mkdir -m 0700 "$TMP_DIR/home" "$TMP_DIR/state" "$TMP_DIR/workspace"
CONTAINER_NAME="aw-host-socket-smoke-$$"
HOST_SOCKET="$TMP_DIR/echo.sock"
READY_FILE="$TMP_DIR/server.ready"
CONFIG="$TMP_DIR/gateway.toml"

cat >"$TMP_DIR/server.py" <<'PY'
import os
import socket
import sys

path, ready, expected = sys.argv[1:]
expected = expected.encode("ascii")
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(path)
os.chmod(path, 0o600)
server.listen(1)
with open(ready, "x", encoding="ascii") as marker:
    marker.write("ready\n")
connection, _ = server.accept()
with connection:
    payload = connection.recv(4096)
    if payload != expected:
        raise SystemExit(f"unexpected payload: {payload!r}")
    connection.sendall(b"ack:" + payload)
server.close()
PY

start_server() {
    local expected=$1
    rm -f -- "$READY_FILE"
    python3 "$TMP_DIR/server.py" "$HOST_SOCKET" "$READY_FILE" "$expected" \
        >"$TMP_DIR/server.stdout" 2>"$TMP_DIR/server.stderr" &
    SERVER_PID=$!
    for _ in {1..100}; do
        [[ -f $READY_FILE && -S $HOST_SOCKET ]] && return
        kill -0 "$SERVER_PID" 2>/dev/null \
            || fail "host UDS server exited: $(cat "$TMP_DIR/server.stderr")"
        sleep 0.02
    done
    fail 'host UDS server did not become ready'
}

start_server first

cat >"$CONFIG" <<EOF
schema_version = "1"
default_target = "socket-smoke"

[runtime]
type = "docker"
docker_host = "$DOCKER_HOST_URI"

[logging]
level = "info"
console = false
directory = "$TMP_DIR/logs"

[target_defaults.workspace]
path = "$TMP_DIR/workspace"
state_dir = ".aw-gateway"

[target_defaults.container_agent]
enabled = false

[target_defaults.host_socket_exposures.echo]
host_path = "$HOST_SOCKET"
container_path = "/run/acl-proxy/echo.sock"
selinux_relabel = "none"

[targets.socket-smoke]
image = "$IMAGE"
mode = "fixed"
name = "$CONTAINER_NAME"
container_user = "root"
container_home = "/root"
stop_when_idle = false
remove_on_stop = false

[targets.socket-smoke.access]
method = "runtime_exec"

[targets.socket-smoke.identity]
bootstrap_user = "root"
session_user = "root"
session_uid = "0"
session_gid = "0"
session_home = "/root"
session_shell = "/bin/sh"
EOF

gateway() {
    AW_GATEWAY_TEST_HOME="$TMP_DIR/home" \
    AW_GATEWAY_STATE_HOME="$TMP_DIR/state" \
        "$GATEWAY_BIN" --config "$CONFIG" "$@"
}

gateway config validate
gateway up socket-smoke --json >"$TMP_DIR/up.json"
python3 - "$TMP_DIR/up.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    status = json.load(stream)
expected = [{
    "name": "echo",
    "realization": "pinned_inode",
    "ready": True,
}]
if status.get("host_socket_exposures") != expected:
    raise SystemExit(f"unexpected exposure status: {status.get('host_socket_exposures')!r}")
PY

MANIFEST=$(docker --host "$DOCKER_HOST_URI" inspect "$CONTAINER_NAME" \
    --format '{{ index .Config.Labels "io.aw-gateway.host-socket-exposures.v1" }}')
[[ $MANIFEST == sha256:* && ${#MANIFEST} == 71 ]] \
    || fail "container has invalid host socket exposure manifest: $MANIFEST"

python3 - "$DOCKER_HOST_URI" "$CONTAINER_NAME" "$HOST_SOCKET" <<'PY'
import json
import subprocess
import sys

docker_host, container, source = sys.argv[1:]
inspect = json.loads(subprocess.check_output([
    "docker", "--host", docker_host, "inspect", container,
]))[0]
matches = [mount for mount in inspect["Mounts"]
           if mount["Destination"] == "/run/acl-proxy/echo.sock"]
if len(matches) != 1:
    raise SystemExit(f"unexpected socket mount inventory: {matches!r}")
mount = matches[0]
if mount["Type"] != "bind" or mount["Source"] != source:
    raise SystemExit(f"unexpected socket realization: {mount!r}")
PY

exchange() {
    local payload=$1
    gateway run socket-smoke -- python3 -c '
import socket
import sys
client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect("/run/acl-proxy/echo.sock")
client.sendall(sys.argv[1].encode("ascii"))
print(client.recv(4096).decode("ascii"))
' "$payload"
}

RESPONSE=$(exchange first)
[[ $RESPONSE == ack:first ]] || fail "unexpected first UDS response: $RESPONSE"

wait "$SERVER_PID" || fail "host UDS server failed: $(cat "$TMP_DIR/server.stderr")"
SERVER_PID=
rm -f -- "$HOST_SOCKET"
start_server second

gateway status socket-smoke --json >"$TMP_DIR/rebound-status.json"
python3 - "$TMP_DIR/rebound-status.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    status = json.load(stream)
expected = [{
    "name": "echo",
    "realization": "pinned_inode",
    "ready": False,
    "failure_category": "recreate_required",
}]
if status.get("host_socket_exposures") != expected:
    raise SystemExit(f"unexpected rebound exposure status: {status.get('host_socket_exposures')!r}")
PY
if gateway up socket-smoke --json \
    >"$TMP_DIR/rebound-up.stdout" 2>"$TMP_DIR/rebound-up.stderr"; then
    fail 'AW Gateway reused a container pinned to the replaced socket inode'
fi
grep -q 'remove and recreate' "$TMP_DIR/rebound-up.stderr" \
    || fail "rebind refusal was not actionable: $(cat "$TMP_DIR/rebound-up.stderr")"

gateway remove socket-smoke
if docker --host "$DOCKER_HOST_URI" inspect "$CONTAINER_NAME" >/dev/null 2>&1; then
    fail 'gateway remove reported success but left the old container present'
fi

gateway up socket-smoke --json >"$TMP_DIR/recreated-up.json"
python3 - "$TMP_DIR/recreated-up.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    status = json.load(stream)
exposures = status.get("host_socket_exposures")
if exposures != [{"name": "echo", "realization": "pinned_inode", "ready": True}]:
    raise SystemExit(f"unexpected recreated exposure status: {exposures!r}")
PY
RECREATED_MANIFEST=$(docker --host "$DOCKER_HOST_URI" inspect "$CONTAINER_NAME" \
    --format '{{ index .Config.Labels "io.aw-gateway.host-socket-exposures.v1" }}')
[[ $RECREATED_MANIFEST == sha256:* && ${#RECREATED_MANIFEST} == 71 ]] \
    || fail "recreated container has invalid exposure manifest: $RECREATED_MANIFEST"
[[ $RECREATED_MANIFEST != "$MANIFEST" ]] \
    || fail 'socket inode replacement did not change the pinned-inode manifest'

RESPONSE=$(exchange second)
[[ $RESPONSE == ack:second ]] || fail "unexpected second UDS response: $RESPONSE"
wait "$SERVER_PID" || fail "replacement UDS server failed: $(cat "$TMP_DIR/server.stderr")"
SERVER_PID=

gateway remove socket-smoke
if docker --host "$DOCKER_HOST_URI" inspect "$CONTAINER_NAME" >/dev/null 2>&1; then
    fail 'final gateway remove reported success but left the recreated container present'
fi

SUCCESS=1
printf '%s\n' \
    'host-socket-create=passed' \
    'pinned-inode-rebind-fail-closed=passed' \
    'gateway-remove=passed' \
    'workload-recreate-recovery=passed' \
    'second-uds-exchange=passed'
