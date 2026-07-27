#!/usr/bin/env bash

set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
FIREWALL="$ROOT/assets/aw-transparent-uds-firewall"
BASE_IMAGE=${AW_UDS_STACK_SMOKE_IMAGE:-ubuntu:24.04}

ACL_PROXY_BIN=
AGENT_BIN=
ACL_REPO=
ACCESS_RUNTIME_REPO=
AW_REPO=
EXPECTED_ACL_SHA=
EXPECTED_ACCESS_RUNTIME_SHA=
EXPECTED_AW_SHA=
TMP_DIR=
NETWORK=
ORIGIN_CONTAINER=
PARENT_CONTAINER=
WORKLOAD_CONTAINER=
WORKLOAD_IMAGE=
ACL_PID=
ACL_TIME_PID=
ACTIVE_CLIENT_PID=
IDENTITY_WRITER_PID=
FORWARDER_PID=
MEASUREMENT_CONTROL_DIR=
MEASUREMENT_LOAD=
PROXY_PROJECTION_PROBE=
GATEWAY_PROJECTION_PROBE=
MEASUREMENT_SECRET_SCAN_FILE=
PROXY_PROJECTED_BYTES=
PROXY_PROJECTED_DESCRIPTORS=
PROXY_MINIMUM_ACTIVE_BYTES_PER_FLOW=
GATEWAY_PROJECTED_BYTES=
GATEWAY_PROJECTED_DESCRIPTORS=
MEASUREMENT_CLIENTS_STARTED=0
SUCCESS=0

fail() {
    printf 'TLS Access Flow stack smoke failed: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat >&2 <<'EOF'
Usage: run-tls-access-flow-stack-smoke.sh \
  --acl-proxy-bin <absolute-path> \
  --agent-bin <absolute-path> \
  --acl-repo <absolute-path> \
  --access-runtime-repo <absolute-path> \
  --aw-repo <absolute-path> \
  --expected-acl-sha <full-sha> \
  --expected-access-runtime-sha <full-sha> \
  --expected-aw-sha <full-sha> \
  [--measurement-control-dir <absolute-empty-directory> \
   --measurement-load <0|32|128> \
   --proxy-projection-probe <absolute-regular-file> \
   --gateway-projection-probe <absolute-regular-file> \
   --measurement-secret-scan-file <absolute-new-file>]
EOF
    exit 2
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command is unavailable: $1"
}

require_absolute_file() {
    local label=$1 path=$2
    [[ $path == /* ]] || fail "$label must be an absolute path"
    [[ -f $path && ! -L $path && -x $path ]] || fail "$label is not an executable regular file: $path"
}

sanitize_diagnostics() {
    if [[ -n ${BEARER_HISTORY:-} && -f $BEARER_HISTORY ]]; then
        python3 -c '
import pathlib
import sys

secrets = [
    line for line in pathlib.Path(sys.argv[1]).read_bytes().splitlines() if line
]
data = sys.stdin.buffer.read()
for secret in secrets:
    data = data.replace(secret, b"[REDACTED]")
sys.stdout.buffer.write(data)
' "$BEARER_HISTORY"
    else
        cat
    fi
}

cleanup() {
    local status=$?
    trap - EXIT
    set +e
    if (( status == 0 && SUCCESS != 1 )); then
        printf '%s\n' 'transparent UDS stack smoke exited without its success marker' >&2
        status=1
    fi
    if (( status != 0 )); then
        if [[ -n $TMP_DIR ]]; then
            for log in "$TMP_DIR"/acl-proxy.*.stderr; do
                [[ -s $log ]] || continue
                printf '%s\n' "--- $log ---" >&2
                tail -n 80 "$log" | sanitize_diagnostics >&2
            done
        fi
        for container in "$WORKLOAD_CONTAINER" "$PARENT_CONTAINER" "$ORIGIN_CONTAINER"; do
            [[ -n $container ]] || continue
            docker logs --tail 80 "$container" 2>&1 \
                | sanitize_diagnostics >&2 || true
        done
    fi
    if (( status != 0 )) && [[ ${AW_UDS_STACK_SMOKE_KEEP_FAILED:-0} == 1 ]]; then
        printf 'preserving failed smoke resources: temp=%s network=%s workload=%s parent=%s origin=%s\n' \
            "$TMP_DIR" "$NETWORK" "$WORKLOAD_CONTAINER" "$PARENT_CONTAINER" "$ORIGIN_CONTAINER" >&2
        exit "$status"
    fi
    if (( MEASUREMENT_CLIENTS_STARTED == 1 )) && [[ -n $WORKLOAD_CONTAINER ]]; then
        docker exec "$WORKLOAD_CONTAINER" touch /tmp/measurement.stop >/dev/null 2>&1 || true
    fi
    if [[ -n $ACL_PID ]] && kill -0 "$ACL_PID" 2>/dev/null; then
        kill -TERM "$ACL_PID" 2>/dev/null || true
        for _ in {1..100}; do
            kill -0 "$ACL_PID" 2>/dev/null || break
            sleep 0.05
        done
        kill -KILL "$ACL_PID" 2>/dev/null || true
        if [[ -n $ACL_TIME_PID ]]; then
            wait "$ACL_TIME_PID" 2>/dev/null || true
        else
            wait "$ACL_PID" 2>/dev/null || true
        fi
    fi
    if [[ -n $ACTIVE_CLIENT_PID ]] && kill -0 "$ACTIVE_CLIENT_PID" 2>/dev/null; then
        kill -TERM "$ACTIVE_CLIENT_PID" 2>/dev/null || true
        wait "$ACTIVE_CLIENT_PID" 2>/dev/null || true
    fi
    if [[ -n $IDENTITY_WRITER_PID ]] && kill -0 "$IDENTITY_WRITER_PID" 2>/dev/null; then
        kill -TERM "$IDENTITY_WRITER_PID" 2>/dev/null || true
        wait "$IDENTITY_WRITER_PID" 2>/dev/null || true
    fi
    if [[ -n $FORWARDER_PID ]] && kill -0 "$FORWARDER_PID" 2>/dev/null; then
        kill -TERM "$FORWARDER_PID" 2>/dev/null || true
        wait "$FORWARDER_PID" 2>/dev/null || true
    fi
    for container in "$WORKLOAD_CONTAINER" "$PARENT_CONTAINER" "$ORIGIN_CONTAINER"; do
        [[ -n $container ]] || continue
        docker rm -f "$container" >/dev/null 2>&1 || true
    done
    [[ -z $NETWORK ]] || docker network rm "$NETWORK" >/dev/null 2>&1 || true
    [[ -z $WORKLOAD_IMAGE ]] || docker image rm -f "$WORKLOAD_IMAGE" >/dev/null 2>&1 || true
    [[ -z $TMP_DIR ]] || rm -rf -- "$TMP_DIR"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

while (($#)); do
    case "$1" in
        --acl-proxy-bin)
            (($# >= 2)) || usage
            [[ -z $ACL_PROXY_BIN ]] || usage
            ACL_PROXY_BIN=$2
            shift 2
            ;;
        --agent-bin)
            (($# >= 2)) || usage
            [[ -z $AGENT_BIN ]] || usage
            AGENT_BIN=$2
            shift 2
            ;;
        --acl-repo)
            (($# >= 2)) || usage
            [[ -z $ACL_REPO ]] || usage
            ACL_REPO=$2
            shift 2
            ;;
        --access-runtime-repo)
            (($# >= 2)) || usage
            [[ -z $ACCESS_RUNTIME_REPO ]] || usage
            ACCESS_RUNTIME_REPO=$2
            shift 2
            ;;
        --aw-repo)
            (($# >= 2)) || usage
            [[ -z $AW_REPO ]] || usage
            AW_REPO=$2
            shift 2
            ;;
        --expected-acl-sha)
            (($# >= 2)) || usage
            [[ -z $EXPECTED_ACL_SHA ]] || usage
            EXPECTED_ACL_SHA=$2
            shift 2
            ;;
        --expected-access-runtime-sha)
            (($# >= 2)) || usage
            [[ -z $EXPECTED_ACCESS_RUNTIME_SHA ]] || usage
            EXPECTED_ACCESS_RUNTIME_SHA=$2
            shift 2
            ;;
        --expected-aw-sha)
            (($# >= 2)) || usage
            [[ -z $EXPECTED_AW_SHA ]] || usage
            EXPECTED_AW_SHA=$2
            shift 2
            ;;
        --measurement-control-dir)
            (($# >= 2)) || usage
            [[ -z $MEASUREMENT_CONTROL_DIR ]] || usage
            MEASUREMENT_CONTROL_DIR=$2
            shift 2
            ;;
        --measurement-load)
            (($# >= 2)) || usage
            [[ -z $MEASUREMENT_LOAD ]] || usage
            MEASUREMENT_LOAD=$2
            shift 2
            ;;
        --proxy-projection-probe)
            (($# >= 2)) || usage
            [[ -z $PROXY_PROJECTION_PROBE ]] || usage
            PROXY_PROJECTION_PROBE=$2
            shift 2
            ;;
        --gateway-projection-probe)
            (($# >= 2)) || usage
            [[ -z $GATEWAY_PROJECTION_PROBE ]] || usage
            GATEWAY_PROJECTION_PROBE=$2
            shift 2
            ;;
        --measurement-secret-scan-file)
            (($# >= 2)) || usage
            [[ -z $MEASUREMENT_SECRET_SCAN_FILE ]] || usage
            MEASUREMENT_SECRET_SCAN_FILE=$2
            shift 2
            ;;
        *) usage ;;
    esac
done

[[ -n $ACL_PROXY_BIN && -n $AGENT_BIN && -n $ACL_REPO \
    && -n $ACCESS_RUNTIME_REPO && -n $AW_REPO && -n $EXPECTED_ACL_SHA \
    && -n $EXPECTED_ACCESS_RUNTIME_SHA && -n $EXPECTED_AW_SHA ]] || usage
if [[ -n $MEASUREMENT_CONTROL_DIR || -n $MEASUREMENT_LOAD \
    || -n $PROXY_PROJECTION_PROBE || -n $GATEWAY_PROJECTION_PROBE \
    || -n $MEASUREMENT_SECRET_SCAN_FILE ]]; then
    [[ -n $MEASUREMENT_CONTROL_DIR && $MEASUREMENT_LOAD =~ ^(0|32|128)$ \
        && $PROXY_PROJECTION_PROBE == /* && -f $PROXY_PROJECTION_PROBE \
        && ! -L $PROXY_PROJECTION_PROBE \
        && $GATEWAY_PROJECTION_PROBE == /* && -f $GATEWAY_PROJECTION_PROBE \
        && ! -L $GATEWAY_PROJECTION_PROBE \
        && $MEASUREMENT_SECRET_SCAN_FILE == /* \
        && ! -e $MEASUREMENT_SECRET_SCAN_FILE && ! -L $MEASUREMENT_SECRET_SCAN_FILE \
        && -d $(dirname -- "$MEASUREMENT_SECRET_SCAN_FILE") \
        && ! -L $(dirname -- "$MEASUREMENT_SECRET_SCAN_FILE") ]] || usage
    [[ $MEASUREMENT_CONTROL_DIR == /* && -d $MEASUREMENT_CONTROL_DIR \
        && ! -L $MEASUREMENT_CONTROL_DIR ]] \
        || fail "measurement control directory must be an absolute non-symlink directory"
    MEASUREMENT_CONTROL_DIR=$(cd -- "$MEASUREMENT_CONTROL_DIR" && pwd -P)
    [[ -z $(find "$MEASUREMENT_CONTROL_DIR" -mindepth 1 -print -quit) ]] \
        || fail "measurement control directory must be empty"
    command -v python3 >/dev/null 2>&1 \
        || fail "required command is unavailable: python3"
    PROXY_PROJECTION_PROBE=$(cd -- "$(dirname -- "$PROXY_PROJECTION_PROBE")" \
        && pwd -P)/$(basename -- "$PROXY_PROJECTION_PROBE")
    GATEWAY_PROJECTION_PROBE=$(cd -- "$(dirname -- "$GATEWAY_PROJECTION_PROBE")" \
        && pwd -P)/$(basename -- "$GATEWAY_PROJECTION_PROBE")
    MEASUREMENT_SECRET_SCAN_FILE=$(cd -- "$(dirname -- "$MEASUREMENT_SECRET_SCAN_FILE")" \
        && pwd -P)/$(basename -- "$MEASUREMENT_SECRET_SCAN_FILE")
    read -r PROXY_PROJECTED_BYTES PROXY_PROJECTED_DESCRIPTORS \
        PROXY_MINIMUM_ACTIVE_BYTES_PER_FLOW < <(
        python3 - "$PROXY_PROJECTION_PROBE" <<'PY'
import csv
from pathlib import Path
import sys

with Path(sys.argv[1]).open(encoding="ascii", newline="") as source:
    rows = list(csv.DictReader(source, delimiter="\t"))
expected = {
    "proxy_projected_bytes",
    "proxy_projected_descriptors",
    "proxy_minimum_active_bytes_per_flow",
}
if (
    not rows
    or set(rows[0]) != {"name", "value"}
    or {row["name"] for row in rows} != expected
    or len(rows) != len(expected)
):
    raise SystemExit("Proxy projection probe has an invalid contract")
values = {}
for row in rows:
    try:
        value = int(row["value"])
    except ValueError:
        raise SystemExit("Proxy projection probe contains a non-integer") from None
    if value <= 0:
        raise SystemExit("Proxy projection probe contains a nonpositive value")
    values[row["name"]] = value
print(
    values["proxy_projected_bytes"],
    values["proxy_projected_descriptors"],
    values["proxy_minimum_active_bytes_per_flow"],
)
PY
    ) || fail "could not consume the Proxy-owned RT06 projection probe"
    read -r GATEWAY_PROJECTED_BYTES GATEWAY_PROJECTED_DESCRIPTORS < <(
        python3 - "$GATEWAY_PROJECTION_PROBE" <<'PY'
import csv
from pathlib import Path
import sys

with Path(sys.argv[1]).open(encoding="ascii", newline="") as source:
    rows = list(csv.DictReader(source, delimiter="\t"))
expected = {"gateway_projected_bytes", "gateway_projected_descriptors"}
if (
    not rows
    or set(rows[0]) != {"name", "value"}
    or {row["name"] for row in rows} != expected
    or len(rows) != len(expected)
):
    raise SystemExit("Gateway projection probe has an invalid contract")
values = {}
for row in rows:
    try:
        value = int(row["value"])
    except ValueError:
        raise SystemExit("Gateway projection probe contains a non-integer") from None
    if value <= 0:
        raise SystemExit("Gateway projection probe contains a nonpositive value")
    values[row["name"]] = value
print(
    values["gateway_projected_bytes"],
    values["gateway_projected_descriptors"],
)
PY
    ) || fail "could not consume the Gateway-owned RT06 projection probe"
fi

for command in awk curl docker git jq openssl python3 sha256sum stat timeout; do
    require_command "$command"
done
if [[ -n $MEASUREMENT_CONTROL_DIR ]]; then
    require_command /usr/bin/time
    require_command pgrep
fi

canonical_repo() {
    local label=$1 path=$2
    [[ $path == /* && -d $path && ! -L $path ]] \
        || fail "$label must be an absolute Git worktree"
    local canonical top_level
    canonical=$(cd -- "$path" && pwd -P)
    top_level=$(git -C "$canonical" rev-parse --show-toplevel 2>/dev/null) \
        || fail "$label must be an absolute Git worktree"
    [[ $top_level == "$canonical" ]] || fail "$label must name the Git worktree root"
    printf '%s\n' "$canonical"
}

canonical_private_temp_base() {
    local candidate=$1
    [[ $candidate == /* ]] || fail "smoke temporary base must be absolute"
    python3 - "$candidate" <<'PY'
import os
import pathlib
import stat
import sys

candidate = pathlib.Path(sys.argv[1])
try:
    base = candidate.resolve(strict=True)
except OSError as error:
    raise SystemExit(f"smoke temporary base is unavailable: {error}")
if not base.is_dir():
    raise SystemExit("smoke temporary base is not a directory")

def require_trusted_directory(current: pathlib.Path) -> None:
    metadata = current.lstat()
    if not stat.S_ISDIR(metadata.st_mode):
        raise SystemExit(f"smoke temporary base has a non-directory ancestor: {current}")
    if metadata.st_uid not in (0, os.geteuid()):
        raise SystemExit(f"smoke temporary base has an untrusted owner: {current}")
    if stat.S_IMODE(metadata.st_mode) & 0o022:
        raise SystemExit(f"smoke temporary base is group/other writable: {current}")

current = pathlib.Path("/")
require_trusted_directory(current)
for component in base.parts[1:]:
    current /= component
    require_trusted_directory(current)
print(base)
PY
}

require_expected_sha() {
    local label=$1 repo=$2 expected=$3 actual
    [[ $expected =~ ^[0-9a-f]{40}$ ]] || fail "$label expected SHA must be a full lowercase SHA-1"
    actual=$(git -C "$repo" rev-parse HEAD)
    [[ $actual == "$expected" ]] \
        || fail "$label HEAD is $actual, expected $expected"
}

ACL_REPO=$(canonical_repo acl-repo "$ACL_REPO")
ACCESS_RUNTIME_REPO=$(canonical_repo access-runtime-repo "$ACCESS_RUNTIME_REPO")
AW_REPO=$(canonical_repo aw-repo "$AW_REPO")
[[ $AW_REPO == "$ROOT" ]] || fail "aw-repo must be the repository containing this harness"
[[ $(readlink -f -- "$ACL_REPO/../access-runtime") == "$ACCESS_RUNTIME_REPO" ]] \
    || fail "ACL Proxy path dependencies do not resolve to access-runtime-repo"
require_expected_sha acl-proxy "$ACL_REPO" "$EXPECTED_ACL_SHA"
require_expected_sha access-runtime "$ACCESS_RUNTIME_REPO" "$EXPECTED_ACCESS_RUNTIME_SHA"
require_expected_sha aw-gateway "$AW_REPO" "$EXPECTED_AW_SHA"
PINNED_ACCESS_RUNTIME_SHA=$(
    python3 "$AW_REPO/scripts/validate-access-runtime-pin.py" \
        "$AW_REPO/Cargo.toml" \
        "$AW_REPO/Cargo.lock" \
        https://github.com/kcosr/access-runtime.git
)
[[ $PINNED_ACCESS_RUNTIME_SHA == "$EXPECTED_ACCESS_RUNTIME_SHA" ]] \
    || fail "AW Gateway Access Runtime pin does not match access-runtime-repo"
[[ -z $(git -C "$ACL_REPO" status --porcelain --untracked-files=all) ]] \
    || fail "acl-repo must be clean for provenance-bound release builds"
[[ -z $(git -C "$ACCESS_RUNTIME_REPO" status --porcelain --untracked-files=all) ]] \
    || fail "access-runtime-repo must be clean for provenance-bound release builds"
[[ -z $(git -C "$AW_REPO" status --porcelain --untracked-files=all) ]] \
    || fail "aw-repo must be clean for provenance-bound smoke evidence"

require_absolute_file acl-proxy "$ACL_PROXY_BIN"
require_absolute_file agent "$AGENT_BIN"
[[ -x $FIREWALL && ! -L $FIREWALL ]] || fail "firewall asset is missing or not executable"

docker info >/dev/null 2>&1 || fail "Docker daemon is unavailable"
docker image inspect "$BASE_IMAGE" >/dev/null 2>&1 \
    || fail "required local image is unavailable (refusing an implicit pull): $BASE_IMAGE"
docker image inspect python:3.12-slim >/dev/null 2>&1 \
    || fail "required local image is unavailable (refusing an implicit pull): python:3.12-slim"

umask 077
TEMP_BASE_INPUT=${AW_UDS_STACK_SMOKE_TEMP_BASE:-${HOME:-}}
[[ -n $TEMP_BASE_INPUT ]] || fail "HOME or AW_UDS_STACK_SMOKE_TEMP_BASE is required"
SMOKE_TEMP_BASE=$(canonical_private_temp_base "$TEMP_BASE_INPUT")
TMP_DIR=$(mktemp -d "$SMOKE_TEMP_BASE/.aw-transparent-uds-stack.XXXXXX")
mkdir -m 0700 "$TMP_DIR/home" "$TMP_DIR/socket-runtime" "$TMP_DIR/config" \
    "$TMP_DIR/logs" "$TMP_DIR/parent-logs"
touch "$TMP_DIR/parent-logs/events.jsonl"
chmod 0666 "$TMP_DIR/parent-logs/events.jsonl"
IDENTITY_TOKEN=$(openssl rand -hex 24)
[[ $IDENTITY_TOKEN =~ ^[0-9a-f]{48}$ ]] || fail "could not create the workload bearer"
IDENTITY_TOKEN_FILE="$TMP_DIR/config/identity-token"
IDENTITY_FIFO="$TMP_DIR/identity-token.fifo"
BEARER_HISTORY="$TMP_DIR/.bearer-history"
printf '%s' "$IDENTITY_TOKEN" >"$IDENTITY_TOKEN_FILE"
chmod 0600 "$IDENTITY_TOKEN_FILE"
printf '%s\n' "$IDENTITY_TOKEN" >"$BEARER_HISTORY"
chmod 0600 "$BEARER_HISTORY"
mkfifo -m 0600 "$IDENTITY_FIFO"
ACTIVE_BEARER=$IDENTITY_TOKEN

openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 \
    -out "$TMP_DIR/config/mitm-ca-key.pem" >/dev/null 2>&1
openssl req -x509 -new -key "$TMP_DIR/config/mitm-ca-key.pem" -days 1 \
    -subj '/CN=transparent UDS smoke MITM CA' \
    -addext 'basicConstraints=critical,CA:TRUE' \
    -addext 'keyUsage=critical,keyCertSign,cRLSign' \
    -addext 'subjectKeyIdentifier=hash' \
    -addext 'authorityKeyIdentifier=keyid:always' \
    -out "$TMP_DIR/config/mitm-ca-cert.pem" >/dev/null 2>&1
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -subj '/CN=transparent UDS smoke origin CA' \
    -addext 'basicConstraints=critical,CA:TRUE' \
    -addext 'keyUsage=critical,keyCertSign,cRLSign' \
    -keyout "$TMP_DIR/config/origin-ca-key.pem" \
    -out "$TMP_DIR/config/origin-ca-cert.pem" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes \
    -subj '/CN=origin.test' \
    -addext 'subjectAltName=DNS:origin.test' \
    -keyout "$TMP_DIR/config/origin-key.pem" \
    -out "$TMP_DIR/config/origin.csr" >/dev/null 2>&1
openssl x509 -req -days 1 -sha256 \
    -in "$TMP_DIR/config/origin.csr" \
    -CA "$TMP_DIR/config/origin-ca-cert.pem" \
    -CAkey "$TMP_DIR/config/origin-ca-key.pem" -CAcreateserial \
    -out "$TMP_DIR/config/origin-cert.pem" \
    -extfile <(printf '%s\n' 'subjectAltName=DNS:origin.test') >/dev/null 2>&1
chmod 0600 "$TMP_DIR/config/"*-key.pem
chmod 0644 "$TMP_DIR/config/"*-cert.pem

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
        -subj '/CN=Access Flow smoke root' \
        -addext 'basicConstraints=critical,CA:TRUE' \
        -addext 'keyUsage=critical,keyCertSign,cRLSign' \
        -keyout "$TMP_DIR/config/access-flow-root-key.pem" \
        -out "$TMP_DIR/config/access-flow-root.pem" >/dev/null 2>&1
    openssl req -newkey rsa:2048 -nodes \
        -subj '/CN=proxy.access-flow.test' \
        -addext 'subjectAltName=DNS:proxy.access-flow.test' \
        -keyout "$TMP_DIR/config/access-flow-server-key.pem" \
        -out "$TMP_DIR/config/access-flow-server.csr" >/dev/null 2>&1
    cat >"$TMP_DIR/config/access-flow-server.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:proxy.access-flow.test
EOF
    openssl x509 -req -days 1 -sha256 \
        -in "$TMP_DIR/config/access-flow-server.csr" \
        -CA "$TMP_DIR/config/access-flow-root.pem" \
        -CAkey "$TMP_DIR/config/access-flow-root-key.pem" -CAcreateserial \
        -extfile "$TMP_DIR/config/access-flow-server.ext" \
        -out "$TMP_DIR/config/access-flow-server-leaf.pem" >/dev/null 2>&1
    cat "$TMP_DIR/config/access-flow-server-leaf.pem" \
        "$TMP_DIR/config/access-flow-root.pem" \
        >"$TMP_DIR/config/access-flow-server-chain.pem"
    chmod 0600 "$TMP_DIR/config/access-flow-root-key.pem" \
        "$TMP_DIR/config/access-flow-server-key.pem"
chmod 0644 "$TMP_DIR/config/access-flow-root.pem" \
        "$TMP_DIR/config/access-flow-server-leaf.pem" \
        "$TMP_DIR/config/access-flow-server-chain.pem"

SUFFIX=$(openssl rand -hex 6)
[[ $SUFFIX =~ ^[0-9a-f]{12}$ ]] || fail "could not create a resource suffix"
NETWORK="aw-uds-$SUFFIX"
ORIGIN_CONTAINER="aw-uds-origin-$SUFFIX"
PARENT_CONTAINER="aw-uds-parent-$SUFFIX"
WORKLOAD_CONTAINER="aw-uds-workload-$SUFFIX"
WORKLOAD_IMAGE="aw-uds-workload-image-$SUFFIX"

BASE_IMAGE_ID=$(docker image inspect "$BASE_IMAGE" --format '{{.Id}}')
mkdir -m 0700 "$TMP_DIR/workload-image-context"
cat >"$TMP_DIR/workload-image-context/Dockerfile" <<EOF
FROM $BASE_IMAGE
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
        ca-certificates curl iproute2 iptables netcat-openbsd openssl procps time util-linux \
    && rm -rf /var/lib/apt/lists/*
EOF
docker build --pull=false --quiet --tag "$WORKLOAD_IMAGE" \
    "$TMP_DIR/workload-image-context" >/dev/null \
    || fail "could not prebuild the protected workload image"
WORKLOAD_IMAGE_ID=$(docker image inspect "$WORKLOAD_IMAGE" --format '{{.Id}}')

docker network create "$NETWORK" >/dev/null
NETWORK_GATEWAY=$(docker network inspect "$NETWORK" \
    --format '{{(index .IPAM.Config 0).Gateway}}')
[[ $NETWORK_GATEWAY =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || fail "Docker network did not report an IPv4 gateway"

cat >"$TMP_DIR/origin.py" <<'PY'
import http.server
import base64
import hashlib
import ssl
import sys
import threading
import time


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        identity = self.headers.get("x-aw-identity-token", "absent")
        if self.path == "/websocket" and self.headers.get("Upgrade", "").lower() == "websocket":
            key = self.headers.get("Sec-WebSocket-Key", "")
            accept = base64.b64encode(hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
            ).digest()).decode("ascii")
            self.send_response(101, "Switching Protocols")
            self.send_header("Connection", "Upgrade")
            self.send_header("Upgrade", "websocket")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.send_header("X-Origin-Identity", identity)
            self.end_headers()
            header = self.rfile.read(2)
            if len(header) != 2 or header[0] != 0x81 or not header[1] & 0x80:
                return
            length = header[1] & 0x7f
            mask = self.rfile.read(4)
            payload = bytes(value ^ mask[index % 4] for index, value in enumerate(self.rfile.read(length)))
            if payload == b"ping":
                self.wfile.write(b"\x81\x04pong")
                self.wfile.flush()
            self.close_connection = True
            return
        if self.path == "/stream":
            chunk = b"0123456789abcdef" * 1024
            chunk_count = 64
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(chunk) * chunk_count))
            self.end_headers()
            for _ in range(chunk_count):
                self.wfile.write(chunk)
                self.wfile.flush()
                time.sleep(0.02)
            return
        if self.path.startswith("/measurement-hold/"):
            chunk = b"x" * 1024
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(1024 * 1024 * 1024))
            self.end_headers()
            try:
                while True:
                    self.wfile.write(chunk)
                    self.wfile.flush()
                    time.sleep(0.1)
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        private = "present" if self.headers.get("x-private-smoke") else "absent"
        body = f"origin:{self.path}:identity={identity}:private={private}".encode("ascii")
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format, *_args):
        pass


http_server = http.server.ThreadingHTTPServer(("0.0.0.0", 80), Handler)
escape_server = http.server.ThreadingHTTPServer(("0.0.0.0", 8080), Handler)
https_server = http.server.ThreadingHTTPServer(("0.0.0.0", 443), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(sys.argv[1], sys.argv[2])
https_server.socket = context.wrap_socket(https_server.socket, server_side=True)
threading.Thread(target=http_server.serve_forever, daemon=True).start()
threading.Thread(target=escape_server.serve_forever, daemon=True).start()
https_server.serve_forever()
PY

cat >"$TMP_DIR/parent.py" <<'PY'
import http.client
import http.server
import json
import select
import socket
import threading
import urllib.parse


events_path = "/logs/events.jsonl"
events_lock = threading.Lock()
hop_headers = {
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
    "proxy-connection", "te", "trailer", "transfer-encoding", "upgrade",
}


def record(**event):
    with events_lock, open(events_path, "a", encoding="utf-8") as output:
        output.write(json.dumps(event, sort_keys=True) + "\n")
        output.flush()


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_CONNECT(self):
        host, separator, port = self.path.rpartition(":")
        if not separator or not host or not port.isdigit():
            self.send_error(400)
            return
        record(event="connect", target=self.path)
        upstream = socket.create_connection((host, int(port)), timeout=10)
        self.send_response(200, "Connection Established")
        self.end_headers()
        self.connection.setblocking(False)
        upstream.setblocking(False)
        try:
            streams = [self.connection, upstream]
            while streams:
                readable, _, _ = select.select(streams, [], [], 30)
                if not readable:
                    break
                for source in readable:
                    data = source.recv(65536)
                    if not data:
                        streams = []
                        break
                    target = upstream if source is self.connection else self.connection
                    target.sendall(data)
        finally:
            upstream.close()
            self.close_connection = True

    def do_GET(self):
        target = urllib.parse.urlsplit(self.path)
        if target.scheme != "http" or not target.hostname:
            self.send_error(400)
            return
        port = target.port or 80
        path = urllib.parse.urlunsplit(("", "", target.path or "/", target.query, ""))
        headers = {
            name: value for name, value in self.headers.items()
            if name.lower() not in hop_headers
        }
        headers["Host"] = target.netloc
        headers["Connection"] = "close"
        record(
            event="request",
            target=self.path,
            identity=self.headers.get("x-aw-identity-token", "absent"),
        )
        upstream = http.client.HTTPConnection(target.hostname, port, timeout=10)
        try:
            upstream.request("GET", path, headers=headers)
            response = upstream.getresponse()
            self.send_response(response.status)
            for name, value in response.getheaders():
                if name.lower() not in hop_headers:
                    self.send_header(name, value)
            self.end_headers()
            while True:
                chunk = response.read(16384)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
        finally:
            upstream.close()

    def log_message(self, _format, *_args):
        pass


http.server.ThreadingHTTPServer(("0.0.0.0", 8888), Handler).serve_forever()
PY

docker run -d --name "$ORIGIN_CONTAINER" --network "$NETWORK" \
    --network-alias origin.test \
    --mount "type=bind,src=$TMP_DIR/origin.py,dst=/origin.py,readonly" \
    --mount "type=bind,src=$TMP_DIR/config/origin-cert.pem,dst=/tls/origin-cert.pem,readonly" \
    --mount "type=bind,src=$TMP_DIR/config/origin-key.pem,dst=/tls/origin-key.pem,readonly" \
    python:3.12-slim python3 /origin.py /tls/origin-cert.pem /tls/origin-key.pem >/dev/null
docker run -d --name "$PARENT_CONTAINER" --network "$NETWORK" \
    --mount "type=bind,src=$TMP_DIR/parent.py,dst=/parent.py,readonly" \
    --mount "type=bind,src=$TMP_DIR/parent-logs,dst=/logs" \
    python:3.12-slim python3 /parent.py >/dev/null

ORIGIN_IP=$(docker inspect "$ORIGIN_CONTAINER" \
    --format "{{with index .NetworkSettings.Networks \"$NETWORK\"}}{{.IPAddress}}{{end}}")
PARENT_IP=$(docker inspect "$PARENT_CONTAINER" \
    --format "{{with index .NetworkSettings.Networks \"$NETWORK\"}}{{.IPAddress}}{{end}}")
for value in "$ORIGIN_IP" "$PARENT_IP"; do
    [[ $value =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] \
        || fail "Docker service did not report an IPv4 address"
done

wait_container_port() {
    local container=$1 host=$2 port=$3
    for _ in {1..100}; do
        if docker exec "$container" python3 -c \
            "import socket; s=socket.create_connection(('$host',$port),.2); s.close()" \
            >/dev/null 2>&1; then
            return
        fi
        sleep 0.05
    done
    fail "$container did not open $host:$port"
}
wait_container_port "$ORIGIN_CONTAINER" 127.0.0.1 80
wait_container_port "$ORIGIN_CONTAINER" 127.0.0.1 443
wait_container_port "$ORIGIN_CONTAINER" 127.0.0.1 8080
wait_container_port "$PARENT_CONTAINER" 127.0.0.1 8888

CONTROL_PORT=$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)
TLS_HTTP_PORT=$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("0.0.0.0", 0))
print(s.getsockname()[1])
s.close()
PY
)
TLS_HTTPS_PORT=$(python3 - "$TLS_HTTP_PORT" <<'PY'
import socket
import sys
used = int(sys.argv[1])
while True:
    s = socket.socket()
    s.bind(("0.0.0.0", 0))
    port = s.getsockname()[1]
    s.close()
    if port != used:
        print(port)
        break
PY
)
AGENT_TLS_HTTP_PORT=$(python3 - "$TLS_HTTP_PORT" "$TLS_HTTPS_PORT" <<'PY'
import socket
import sys
used = {int(value) for value in sys.argv[1:]}
while True:
    with socket.socket() as sock:
        sock.bind(("0.0.0.0", 0))
        port = sock.getsockname()[1]
    if port not in used:
        print(port)
        break
PY
)
AGENT_TLS_HTTPS_PORT=$(python3 - "$TLS_HTTP_PORT" "$TLS_HTTPS_PORT" "$AGENT_TLS_HTTP_PORT" <<'PY'
import socket
import sys
used = {int(value) for value in sys.argv[1:]}
while True:
    with socket.socket() as sock:
        sock.bind(("0.0.0.0", 0))
        port = sock.getsockname()[1]
    if port not in used:
        print(port)
        break
PY
)
ACL_CONFIG="$TMP_DIR/config/acl-proxy.toml"
PROVIDER_LOG="$TMP_DIR/logs/provider.jsonl"
cat >"$TMP_DIR/config/provider.py" <<'PY'
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    with open(sys.argv[1], "a", encoding="utf-8") as output:
        output.write(json.dumps(request, sort_keys=True) + "\n")
        output.flush()
    path = request["url"].split("?", 1)[0]
    if path.endswith("/delegate-deny"):
        decision = "deny"
    elif path.endswith("/delegate-pass"):
        decision = "pass"
    else:
        decision = "allow"
    print(json.dumps({
        "id": request["id"],
        "type": "response",
        "decision": decision,
    }), flush=True)
PY
chmod 0700 "$TMP_DIR/config/provider.py"

TLS_MAX_HANDSHAKES=32
TLS_MAX_CONNECTIONS_PER_SOURCE=32
TLS_HANDSHAKE_BURST=100
TLS_SOURCE_TABLE_CAPACITY=128
LISTENER_MAX_CONNECTIONS=64
RELAY_MAX_CONNECTIONS=64
IDENTITY_MAX_PENDING=32
if [[ -n $MEASUREMENT_CONTROL_DIR ]]; then
    TLS_MAX_HANDSHAKES=64
    TLS_MAX_CONNECTIONS_PER_SOURCE=64
    TLS_HANDSHAKE_BURST=200
    LISTENER_MAX_CONNECTIONS=64
    RELAY_MAX_CONNECTIONS=128
    IDENTITY_MAX_PENDING=128
fi

HTTP_TRANSPORT_CONFIG=$(cat <<EOF
kind = "tls_tcp"
bind = "0.0.0.0:$TLS_HTTP_PORT"
handshake_timeout = "2s"

[listeners.transparent_http.endpoint.transport.server_identity]
certificate_chain = "$TMP_DIR/config/access-flow-server-chain.pem"
private_key = "$TMP_DIR/config/access-flow-server-key.pem"

[listeners.transparent_http.endpoint.transport.abuse_control]
max_handshakes = $TLS_MAX_HANDSHAKES
max_connections_per_source = $TLS_MAX_CONNECTIONS_PER_SOURCE
handshakes_per_second = 100
handshake_burst = $TLS_HANDSHAKE_BURST
source_table_capacity = $TLS_SOURCE_TABLE_CAPACITY
EOF
)
HTTPS_TRANSPORT_CONFIG=$(cat <<EOF
kind = "tls_tcp"
bind = "0.0.0.0:$TLS_HTTPS_PORT"
handshake_timeout = "2s"

[listeners.transparent_https.endpoint.transport.server_identity]
certificate_chain = "$TMP_DIR/config/access-flow-server-chain.pem"
private_key = "$TMP_DIR/config/access-flow-server-key.pem"

[listeners.transparent_https.endpoint.transport.abuse_control]
max_handshakes = $TLS_MAX_HANDSHAKES
max_connections_per_source = $TLS_MAX_CONNECTIONS_PER_SOURCE
handshakes_per_second = 100
handshake_burst = $TLS_HANDSHAKE_BURST
source_table_capacity = $TLS_SOURCE_TABLE_CAPACITY
EOF
)
cat >"$ACL_CONFIG" <<EOF
schema_version = 4

[service]
request_timeout = "30s"
shutdown_drain_timeout = "2s"

[control]
bind = "127.0.0.1:$CONTROL_PORT"
base_path = "/_acl-proxy"
request_body_timeout = "2s"

[listeners.transparent_http]
http_versions = ["http1"]
max_connections = $LISTENER_MAX_CONNECTIONS

[listeners.transparent_http.identity]
mode = "required"

[listeners.transparent_http.endpoint]
kind = "access_flow"
admission_timeout = "2s"
allowed_destination_ports = [80]

[listeners.transparent_http.endpoint.transport]
$HTTP_TRANSPORT_CONFIG

[listeners.transparent_https]
http_versions = ["http1"]
max_connections = $LISTENER_MAX_CONNECTIONS

[listeners.transparent_https.identity]
mode = "required"

[listeners.transparent_https.endpoint]
kind = "access_flow"
admission_timeout = "2s"
allowed_destination_ports = [443]

[listeners.transparent_https.endpoint.transport]
$HTTPS_TRANSPORT_CONFIG

[mitm]
mode = "files"
ca_certificate = "$TMP_DIR/config/mitm-ca-cert.pem"
ca_private_key = "$TMP_DIR/config/mitm-ca-key.pem"
directory = "$TMP_DIR/config/generated-certs"

[identity]
max_pending_authentications = $IDENTITY_MAX_PENDING
max_pending_authentications_per_connection = 4

[identity.resolver]
kind = "static"
authority = "uds-stack-smoke"

[[identity.resolver.principals]]
id = "protected-workload"
kind = "service_account"

[[identity.resolver.principals]]
id = "protected-workload-auditor"
kind = "service_account"

[[identity.resolver.groups]]
id = "network-clients"
members = ["protected-workload", "protected-workload-auditor"]

[[identity.resolver.tokens]]
id = "protected-workload-primary"
principal = "protected-workload"
source = "file"
path = "$IDENTITY_TOKEN_FILE"

[authorization.providers.smoke_delegate]
kind = "process"
command = "/usr/bin/python3"
args = ["$TMP_DIR/config/provider.py", "$PROVIDER_LOG"]
timeout = "2s"
inherit_environment = false
include_identity = true
include_headers = ["x-private-smoke"]
max_stdout_line_bytes = 65536
max_pending_requests = 8
restart_backoff = "100ms"
retire_timeout = "30s"

[credentials.inbound.smoke_private]
header = "x-private-smoke"
process_providers = ["smoke_delegate"]

[egress]
route = "parent_proxy"

[egress.parent_proxy]
kind = "generic"
url = "http://$PARENT_IP:8888"

[egress.origin_tls]
trust = "custom"
ca_certificate = "$TMP_DIR/config/origin-ca-cert.pem"
http_versions = ["http1"]

[policy]
default = "deny"

[[policy.rules]]
id = "deny-fixture"
decision = "deny"
urls = ["http://origin.test/denied"]

[[policy.rules]]
id = "delegate-fixture"
decision = "delegate"
authorization_provider = "smoke_delegate"
urls = [
  "http://origin.test/delegate",
  "http://origin.test/delegate-deny",
  "http://origin.test/delegate-pass",
]
identity_states = ["authenticated"]
identity_subjects = [
  { kind = "group", authority = "uds-stack-smoke", id = "network-clients" },
]

[[policy.rules]]
id = "allow-fixture"
decision = "allow"
allow_upgrades = true
urls = ["http://origin.test/**", "https://origin.test/**"]
identity_states = ["authenticated"]
identity_subjects = [
  { kind = "group", authority = "uds-stack-smoke", id = "network-clients" },
]

[redaction.profiles.capture]
rules = [{ literals = ["never-present-in-smoke"] }]

[observation.capture]
events = ["request.allowed"]
redaction_profile = "capture"
directory = "$TMP_DIR/logs/captures"
filename = "{requestId}-{suffix}.json"
max_body_bytes = 1024
max_inflight_body_bytes = 134217728
max_pending_records = 32
max_files = 128
max_total_bytes = 4194304

[observation.logging]
level = "info"
directory = "$TMP_DIR/logs"
max_bytes = 1048576
max_files = 2
console = false

[observation.logging.policy_decisions]
allows = true
denies = true
allow_level = "info"
deny_level = "warn"

[safety.loop_detection]
inject = true
header = "x-acl-proxy-request-id"
EOF

env -i HOME="$TMP_DIR/home" PATH=/usr/bin:/bin \
    "$ACL_PROXY_BIN" config validate --config "$ACL_CONFIG" \
    >"$TMP_DIR/acl-proxy.validate" 2>&1 \
    || fail "ephemeral ACL Proxy configuration did not validate"
start_acl_proxy() {
    local generation=$1
    if [[ -n $MEASUREMENT_CONTROL_DIR ]]; then
        LC_ALL=C /usr/bin/time -v -o "$TMP_DIR/acl-proxy.time-v" \
            env -i HOME="$TMP_DIR/home" PATH=/usr/bin:/bin \
            "$ACL_PROXY_BIN" run --config "$ACL_CONFIG" \
            >"$TMP_DIR/acl-proxy.$generation.stdout" \
            2>"$TMP_DIR/acl-proxy.$generation.stderr" &
        ACL_TIME_PID=$!
        for _ in {1..100}; do
            ACL_PID=$(pgrep -P "$ACL_TIME_PID" -f -- "$ACL_PROXY_BIN" 2>/dev/null | head -n 1 || true)
            [[ -z $ACL_PID ]] || break
            kill -0 "$ACL_TIME_PID" 2>/dev/null || fail "ACL Proxy timing wrapper exited before spawn"
            sleep 0.01
        done
        [[ -n $ACL_PID ]] || fail "could not resolve the measured ACL Proxy process"
    else
        env -i HOME="$TMP_DIR/home" PATH=/usr/bin:/bin \
            "$ACL_PROXY_BIN" run --config "$ACL_CONFIG" \
            >"$TMP_DIR/acl-proxy.$generation.stdout" \
            2>"$TMP_DIR/acl-proxy.$generation.stderr" &
        ACL_PID=$!
    fi
    : >"$TMP_DIR/acl-proxy.stderr"
    for _ in {1..200}; do
        kill -0 "$ACL_PID" 2>/dev/null || fail "ACL Proxy exited before readiness"
        if curl --fail --silent --noproxy '*' \
            "http://127.0.0.1:$CONTROL_PORT/_acl-proxy/ready" \
            | grep -qx '{"status":"ready"}'; then
            if timeout 0.2 bash -c "exec 3<>/dev/tcp/127.0.0.1/$TLS_HTTP_PORT" \
                >/dev/null 2>&1 \
                && timeout 0.2 bash -c "exec 3<>/dev/tcp/127.0.0.1/$TLS_HTTPS_PORT" \
                    >/dev/null 2>&1; then
                return
            fi
        fi
        sleep 0.05
    done
    fail "ACL Proxy did not publish both Access Flow listeners"
}

stop_acl_proxy() {
    kill -TERM "$ACL_PID"
    for _ in {1..100}; do
        kill -0 "$ACL_PID" 2>/dev/null || break
        sleep 0.05
    done
    kill -0 "$ACL_PID" 2>/dev/null \
        && fail "ACL Proxy did not stop within the shutdown bound"
    if [[ -n $ACL_TIME_PID ]]; then
        wait "$ACL_TIME_PID" || fail "ACL Proxy exited unsuccessfully"
        ACL_TIME_PID=
    else
        wait "$ACL_PID" || fail "ACL Proxy exited unsuccessfully"
    fi
    ACL_PID=
}

start_acl_proxy first

cat >"$TMP_DIR/tls-accept-counter.py" <<'PY'
import json
import select
import socket
import socketserver
import sys
import threading

log_path = sys.argv[1]
log_lock = threading.Lock()


class Forwarder(socketserver.BaseRequestHandler):
    def handle(self):
        label, target_port = self.server.route
        with log_lock, open(log_path, "a", encoding="utf-8") as output:
            output.write(json.dumps({"event": "accept", "route": label}) + "\n")
            output.flush()
        upstream = socket.create_connection(("127.0.0.1", target_port), timeout=2)
        sockets = [self.request, upstream]
        try:
            while sockets:
                readable, _, _ = select.select(sockets, [], [], 10)
                if not readable:
                    continue
                for source in readable:
                    data = source.recv(65536)
                    target = upstream if source is self.request else self.request
                    if data:
                        target.sendall(data)
                    else:
                        sockets.remove(source)
                        try:
                            target.shutdown(socket.SHUT_WR)
                        except OSError:
                            pass
        finally:
            upstream.close()


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


servers = []
for label, listen_port, target_port in (
    ("http", int(sys.argv[2]), int(sys.argv[3])),
    ("https", int(sys.argv[4]), int(sys.argv[5])),
):
    server = Server(("0.0.0.0", listen_port), Forwarder)
    server.route = (label, target_port)
    servers.append(server)
    threading.Thread(target=server.serve_forever, daemon=True).start()
threading.Event().wait()
PY
: >"$TMP_DIR/logs/tls-accepts.jsonl"
python3 "$TMP_DIR/tls-accept-counter.py" "$TMP_DIR/logs/tls-accepts.jsonl" \
    "$AGENT_TLS_HTTP_PORT" "$TLS_HTTP_PORT" \
    "$AGENT_TLS_HTTPS_PORT" "$TLS_HTTPS_PORT" \
    >"$TMP_DIR/tls-accept-counter.stdout" \
    2>"$TMP_DIR/tls-accept-counter.stderr" &
FORWARDER_PID=$!
for port in "$AGENT_TLS_HTTP_PORT" "$AGENT_TLS_HTTPS_PORT"; do
    for _ in {1..100}; do
        kill -0 "$FORWARDER_PID" 2>/dev/null || fail "TLS accept counter exited"
        timeout 0.2 bash -c "exec 3<>/dev/tcp/127.0.0.1/$port" \
            >/dev/null 2>&1 && break
        sleep 0.02
    done
done
: >"$TMP_DIR/logs/tls-accepts.jsonl"

AGENT_HTTP_TRANSPORT=$(cat <<EOF
kind = "tls_tcp"
address = "$NETWORK_GATEWAY:$AGENT_TLS_HTTP_PORT"
server_name = "proxy.access-flow.test"

[container_agent.access_flow_relay.routes.transport.trust]
kind = "pem_bundle"
path = "/run/aw-gateway/trust/access-flow-root.pem"
EOF
)
AGENT_HTTPS_TRANSPORT=$(cat <<EOF
kind = "tls_tcp"
address = "$NETWORK_GATEWAY:$AGENT_TLS_HTTPS_PORT"
server_name = "proxy.access-flow.test"

[container_agent.access_flow_relay.routes.transport.trust]
kind = "pem_bundle"
path = "/run/aw-gateway/trust/access-flow-root.pem"
EOF
)
FIREWALL_COMMAND=$(cat <<'EOF'
  "/opt/aw-gateway/bin/aw-transparent-tls-smoke-firewall",
EOF
)
cat >"$TMP_DIR/tls-firewall.sh" <<EOF
#!/usr/bin/env bash
set -Eeuo pipefail
iptables -t nat -N AWTLSN
iptables -t nat -A AWTLSN -p tcp --dport 80 -j REDIRECT --to-ports 3128
iptables -t nat -A AWTLSN -p tcp --dport 443 -j REDIRECT --to-ports 3129
iptables -t nat -A AWTLSN -d 127.0.0.0/8 -j RETURN
iptables -t nat -A AWTLSN -j RETURN
iptables -t nat -I OUTPUT 1 -j AWTLSN
iptables -N AWTLSF
iptables -A AWTLSF -m conntrack --ctstate DNAT -j ACCEPT
iptables -A AWTLSF -d 127.0.0.0/8 -j ACCEPT
iptables -A AWTLSF -m owner --uid-owner 0 -d $NETWORK_GATEWAY/32 -p tcp \
    -m multiport --dports $AGENT_TLS_HTTP_PORT,$AGENT_TLS_HTTPS_PORT -j ACCEPT
iptables -A AWTLSF -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
iptables -A AWTLSF -d $NETWORK_GATEWAY/32 -p udp --dport 53 -j ACCEPT
iptables -A AWTLSF -d $NETWORK_GATEWAY/32 -p tcp --dport 53 -j ACCEPT
iptables -A AWTLSF -j DROP
iptables -I OUTPUT 1 -j AWTLSF
ip6tables -N AWTLS6
ip6tables -A AWTLS6 -d ::1/128 -j ACCEPT
ip6tables -A AWTLS6 -j DROP
ip6tables -I OUTPUT 1 -j AWTLS6
while sleep 1; do
    iptables -t nat -C OUTPUT -j AWTLSN
    iptables -C OUTPUT -j AWTLSF
    ip6tables -C OUTPUT -j AWTLS6
done
EOF
chmod 0700 "$TMP_DIR/tls-firewall.sh"

AGENT_CONTROL_SOCKET=false
if [[ -n $MEASUREMENT_CONTROL_DIR ]]; then
    AGENT_CONTROL_SOCKET='"/run/aw-gateway/agent.sock"'
fi
cat >"$TMP_DIR/container-agent.toml" <<EOF
schema_version = "1"

[container_agent]
enabled = true
control_socket = $AGENT_CONTROL_SOCKET

[container_agent.access_flow_relay]
setup_timeout = "2s"
drain_timeout = "10s"
max_connections = $RELAY_MAX_CONNECTIONS
copy_buffer_bytes_per_direction = 16384
start_after_services = ["transparent-firewall"]

[container_agent.access_flow_relay.presentation]
kind = "bearer_environment"
variable = "AW_IDENTITY_TOKEN"

[[container_agent.access_flow_relay.routes]]
name = "http"
listen = "127.0.0.1:3128"
allowed_destination_ports = [80]

[container_agent.access_flow_relay.routes.transport]
$AGENT_HTTP_TRANSPORT

[[container_agent.access_flow_relay.routes]]
name = "https"
listen = "127.0.0.1:3129"
allowed_destination_ports = [443]

[container_agent.access_flow_relay.routes.transport]
$AGENT_HTTPS_TRANSPORT

[[container_agent.services]]
name = "transparent-firewall"
required = true
user = "root"
command = [
$FIREWALL_COMMAND
]
restart = "always"
depends_on = []

[container_agent.services.health_check]
type = "process"
EOF
chmod 0600 "$TMP_DIR/container-agent.toml"

WORKLOAD_ARTIFACT=$AGENT_BIN
WORKLOAD_CONFIG="$TMP_DIR/container-agent.toml"
WORKLOAD_ARTIFACT_DEST=/opt/aw-gateway/bin/aw-container-agent
WORKLOAD_CONFIG_DEST=/etc/aw-gateway/container-agent.toml
cat >"$TMP_DIR/workload.sh" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
cleanup() {
    local status=$?
    set +e
    if (( status != 0 )); then
        [[ ! -s /tmp/relay.stdout ]] || cat /tmp/relay.stdout >&2
        [[ ! -s /tmp/relay.stderr ]] || cat /tmp/relay.stderr >&2
    fi
    [[ -z ${RELAY_PID:-} ]] || kill "$RELAY_PID" 2>/dev/null || true
    if [[ -n ${RELAY_TIME_PID:-} ]]; then
        wait "$RELAY_TIME_PID" 2>/dev/null || true
    else
        wait 2>/dev/null || true
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM
install -D -o 0 -g 0 -m 0644 \
    /mnt/aw-gateway/access-flow-root.pem \
    /run/aw-gateway/trust/access-flow-root.pem
IFS= read -r AW_IDENTITY_TOKEN </run/aw-gateway/identity-token.fifo
export AW_IDENTITY_TOKEN
if [[ ${AW_RESOURCE_MEASUREMENT:-0} == 1 ]]; then
    LC_ALL=C /usr/bin/time -v -o /tmp/agent.time-v \
        /opt/aw-gateway/bin/aw-container-agent \
        --config /etc/aw-gateway/container-agent.toml run \
        >/tmp/relay.stdout 2>/tmp/relay.stderr &
    RELAY_TIME_PID=$!
    for _ in $(seq 1 100); do
        RELAY_PID=$(pgrep -P "$RELAY_TIME_PID" -f \
            '/opt/aw-gateway/bin/aw-container-agent' 2>/dev/null | head -n 1 || true)
        [[ -z $RELAY_PID ]] || break
        kill -0 "$RELAY_TIME_PID"
        sleep 0.01
    done
    [[ -n $RELAY_PID ]]
else
    /opt/aw-gateway/bin/aw-container-agent \
        --config /etc/aw-gateway/container-agent.toml run \
        >/tmp/relay.stdout 2>/tmp/relay.stderr &
    RELAY_PID=$!
fi
unset AW_IDENTITY_TOKEN
printf '%s\n' "$RELAY_PID" >/tmp/relay.pid
for _ in {1..100}; do
    if timeout 0.2 bash -c 'exec 3<>/dev/tcp/127.0.0.1/3128' \
        >/dev/null 2>&1 \
        && timeout 0.2 bash -c 'exec 3<>/dev/tcp/127.0.0.1/3129' \
            >/dev/null 2>&1; then
        touch /tmp/stack.ready
        break
    fi
    kill -0 "$RELAY_PID"
    sleep 0.05
done
[[ -f /tmp/stack.ready ]]
if [[ -n ${RELAY_TIME_PID:-} ]]; then
    wait "$RELAY_TIME_PID"
else
    wait "$RELAY_PID"
fi
SH
chmod 0700 "$TMP_DIR/workload.sh"

start_workload() {
    local -a consumer_mounts=(
        --mount "type=bind,src=$WORKLOAD_ARTIFACT,dst=$WORKLOAD_ARTIFACT_DEST,readonly"
        --mount "type=bind,src=$WORKLOAD_CONFIG,dst=$WORKLOAD_CONFIG_DEST,readonly"
    )
    local -a transport_mounts=(
        --mount "type=bind,src=$TMP_DIR/config/access-flow-root.pem,dst=/mnt/aw-gateway/access-flow-root.pem,readonly"
        --mount "type=bind,src=$TMP_DIR/tls-firewall.sh,dst=/opt/aw-gateway/bin/aw-transparent-tls-smoke-firewall,readonly"
    )
    local -a measurement_environment=()
    if [[ -n $MEASUREMENT_CONTROL_DIR ]]; then
        measurement_environment=(-e AW_RESOURCE_MEASUREMENT=1)
    fi
    docker run -d --name "$WORKLOAD_CONTAINER" --privileged --network "$NETWORK" \
        --ulimit nofile=4096:4096 \
        --mount "type=bind,src=$TMP_DIR/config/mitm-ca-cert.pem,dst=/etc/acl-proxy/mitm-ca-cert.pem,readonly" \
        --mount "type=bind,src=$IDENTITY_FIFO,dst=/run/aw-gateway/identity-token.fifo" \
        --mount "type=bind,src=$TMP_DIR/workload.sh,dst=/usr/local/bin/workload-smoke,readonly" \
        "${measurement_environment[@]}" \
        "${transport_mounts[@]}" \
        "${consumer_mounts[@]}" \
        "$WORKLOAD_IMAGE" bash /usr/local/bin/workload-smoke >/dev/null
    (printf '%s\n' "$ACTIVE_BEARER" >"$IDENTITY_FIFO") &
    IDENTITY_WRITER_PID=$!
    for _ in {1..100}; do
        kill -0 "$IDENTITY_WRITER_PID" 2>/dev/null || break
        sleep 0.05
    done
    if kill -0 "$IDENTITY_WRITER_PID" 2>/dev/null; then
        kill -TERM "$IDENTITY_WRITER_PID" 2>/dev/null || true
        wait "$IDENTITY_WRITER_PID" 2>/dev/null || true
        IDENTITY_WRITER_PID=
        fail "workload did not consume its one-time bearer"
    fi
    wait "$IDENTITY_WRITER_PID" || fail "one-time bearer delivery failed"
    IDENTITY_WRITER_PID=

    for _ in {1..400}; do
        if ! docker inspect "$WORKLOAD_CONTAINER" --format '{{.State.Running}}' 2>/dev/null \
            | grep -qx true; then
            workload_state=$(docker inspect "$WORKLOAD_CONTAINER" \
                --format 'exit={{.State.ExitCode}} error={{.State.Error}}')
            fail "workload container exited before readiness: $workload_state"
        fi
        docker exec "$WORKLOAD_CONTAINER" test -f /tmp/stack.ready 2>/dev/null && return
        sleep 0.1
    done
    fail "workload relay did not become ready"
}

assert_workload_relay_alive() {
    docker inspect "$WORKLOAD_CONTAINER" --format '{{.State.Running}}' \
        | grep -qx true \
        || fail "workload container is not running"
    docker exec "$WORKLOAD_CONTAINER" sh -eu -c '
        relay_pid=$(cat /tmp/relay.pid)
        kill -0 "$relay_pid"
    ' || fail "workload relay consumer is not running"
}

assert_workload_launcher_bearer_removed() {
    docker exec "$WORKLOAD_CONTAINER" sh -eu -c '
        test "${AW_IDENTITY_TOKEN+x}" != x
        relay_pid=$(cat /tmp/relay.pid)
        kill -0 "$relay_pid"
    ' || fail "could not prove launcher bearer removal with a live relay"
}

capture_workload_observations() {
    local label=$1
    [[ $label =~ ^[a-z0-9-]+$ ]] || fail "invalid observation label"
    local destination="$TMP_DIR/observable/$label"
    mkdir -m 0700 -p "$destination"
    docker inspect "$WORKLOAD_CONTAINER" >"$destination/docker-inspect.json" \
        || fail "could not capture workload metadata"
    docker logs "$WORKLOAD_CONTAINER" >"$destination/docker.stdout" \
        2>"$destination/docker.stderr" \
        || fail "could not capture workload container logs"
    local -a required_outputs=(relay.stdout relay.stderr)
    local source
    for source in "${required_outputs[@]}"; do
        docker cp "$WORKLOAD_CONTAINER:/tmp/$source" "$destination/$source" \
            >/dev/null 2>&1 \
            || fail "could not capture required workload output: $source"
    done
}

scan_observable_secrets() {
    python3 - "$TMP_DIR" "$IDENTITY_TOKEN_FILE" "$BEARER_HISTORY" \
        3<"$BEARER_HISTORY" <<'PY'
import os
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
excluded = {pathlib.Path(value) for value in sys.argv[2:]}
secrets = [line for line in os.fdopen(3, "rb").read().splitlines() if line]
if not secrets:
    raise SystemExit("observable secret scan has no bearer inputs")
for path in root.rglob("*"):
    if path in excluded:
        continue
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        continue
    if not stat.S_ISREG(metadata.st_mode):
        continue
    data = path.read_bytes()
    if any(secret in data for secret in secrets):
        raise SystemExit("generated bearer appeared in observable smoke evidence")
PY
}

request_http() {
    docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
        --fail --silent --show-error --connect-timeout 2 --max-time 10 \
        --noproxy '*' --resolve "origin.test:80:$ORIGIN_IP" "$1"
}

request_https() {
    docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
        --fail --silent --show-error --connect-timeout 2 --max-time 10 \
        --cacert /etc/acl-proxy/mitm-ca-cert.pem --noproxy '*' \
        --resolve "origin.test:443:$ORIGIN_IP" "$1"
}

wait_for_measurement_file() {
    local path=$1 label=$2
    for _ in {1..600}; do
        [[ -f $path ]] && return
        assert_workload_relay_alive
        kill -0 "$ACL_PID" 2>/dev/null || fail "ACL Proxy exited while waiting for $label"
        sleep 0.05
    done
    fail "timed out waiting for $label"
}

measurement_active_flows() {
    docker exec "$WORKLOAD_CONTAINER" sh -eu -c \
        "printf '%s' '{\"id\":\"measurement\",\"method\":\"status\"}' \
            | nc -N -w 1 -U /run/aw-gateway/agent.sock" \
        | jq -er '.result.access_flow_relay.active_flows'
}

wait_for_measurement_active_flows() {
    local expected=$1 require_clients=${2:-0} observed
    for _ in {1..600}; do
        if ((require_clients == 1)); then
            assert_measurement_clients_alive
        fi
        observed=$(measurement_active_flows 2>/dev/null || true)
        [[ $observed == "$expected" ]] && return
        assert_workload_relay_alive
        kill -0 "$ACL_PID" 2>/dev/null || fail "ACL Proxy exited during measured flow admission"
        sleep 0.05
    done
    fail "measured relay active-flow count did not reach $expected (last observed ${observed:-unavailable})"
}

assert_measurement_clients_alive() {
    docker exec "$WORKLOAD_CONTAINER" test ! -f /tmp/measurement-clients.failed \
        || fail "a measured HTTP/HTTPS client exited before the active epoch ended"
    local client_pid
    client_pid=$(docker exec "$WORKLOAD_CONTAINER" cat /tmp/measurement-clients.pid \
        2>/dev/null || true)
    [[ $client_pid =~ ^[1-9][0-9]*$ ]] \
        || fail "measurement client supervisor did not publish its PID"
    docker exec "$WORKLOAD_CONTAINER" kill -0 "$client_pid" 2>/dev/null \
        || fail "measurement client supervisor exited before the active epoch ended"
}

wait_for_measurement_stop() {
    local expected=$1 observed
    for _ in {1..2400}; do
        assert_measurement_clients_alive
        observed=$(measurement_active_flows 2>/dev/null || true)
        [[ $observed == "$expected" ]] \
            || fail "measured relay active-flow count changed during active epoch (expected $expected, observed ${observed:-unavailable})"
        if [[ -f $MEASUREMENT_CONTROL_DIR/stop ]]; then
            assert_measurement_clients_alive
            observed=$(measurement_active_flows 2>/dev/null || true)
            [[ $observed == "$expected" ]] \
                || fail "measured relay active-flow count changed before active stop"
            return
        fi
        sleep 0.02
    done
    fail "timed out waiting for measurement stop"
}

write_measurement_phase() {
    local epoch=$1 phase=$2 active=$3 http=$4 https=$5
    printf '%s\t%s\n' "$epoch" "$phase" >"$MEASUREMENT_CONTROL_DIR/phase.tmp"
    mv "$MEASUREMENT_CONTROL_DIR/phase.tmp" "$MEASUREMENT_CONTROL_DIR/phase"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$epoch" "$phase" "$active" "$http" "$https" \
        "$((active * 3))" "$((active * 2))" "$((active * 98304))" \
        "$((active * 327680))" >>"$MEASUREMENT_CONTROL_DIR/logical.tsv"
}

write_measurement_control_phase() {
    local epoch=$1 phase=$2
    printf '%s\t%s\n' "$epoch" "$phase" >"$MEASUREMENT_CONTROL_DIR/phase.tmp"
    mv "$MEASUREMENT_CONTROL_DIR/phase.tmp" "$MEASUREMENT_CONTROL_DIR/phase"
}

write_measurement_projection() {
    cat >"$MEASUREMENT_CONTROL_DIR/projection.tsv" <<EOF
name	value
proxy_feature_ceiling_bytes	268435456
gateway_feature_ceiling_bytes	268435456
proxy_projected_bytes	$PROXY_PROJECTED_BYTES
gateway_projected_bytes	$GATEWAY_PROJECTED_BYTES
proxy_projected_descriptors	$PROXY_PROJECTED_DESCRIPTORS
gateway_projected_descriptors	$GATEWAY_PROJECTED_DESCRIPTORS
proxy_minimum_active_bytes_per_flow	$PROXY_MINIMUM_ACTIVE_BYTES_PER_FLOW
gateway_minimum_active_bytes_per_flow	147456
composed_minimum_active_bytes_per_flow	327680
known_buffer_bytes_per_flow	98304
logical_permits_per_flow	3
logical_tasks_per_flow	2
configured_max_flows	$RELAY_MAX_CONNECTIONS
logical_permit_ceiling	$((RELAY_MAX_CONNECTIONS * 3))
logical_task_ceiling	$((RELAY_MAX_CONNECTIONS * 2))
known_buffer_ceiling_bytes	$((RELAY_MAX_CONNECTIONS * 98304))
EOF
}

assert_measurement_diagnostics_secret_free() {
    python3 - "$MEASUREMENT_SECRET_SCAN_FILE" \
        "$MEASUREMENT_CONTROL_DIR/gateway.stderr" \
        "$MEASUREMENT_CONTROL_DIR/proxy.stderr" \
        "$MEASUREMENT_CONTROL_DIR/gateway.time-v" \
        "$MEASUREMENT_CONTROL_DIR/proxy.time-v" <<'PY'
from pathlib import Path
import sys

lines = Path(sys.argv[1]).read_bytes().splitlines()
if lines[:1] != [b"contract_version=2"]:
    raise SystemExit("ephemeral secret scan input has an invalid contract")
secrets = []
for line in lines[1:]:
    if line.startswith((b"bearer=", b"key_b64=")):
        secrets.append(line.partition(b"=")[2])
    elif line.startswith(b"key_der_hex="):
        value = line.removeprefix(b"key_der_hex=")
        secrets.extend((bytes.fromhex(value.decode("ascii")), value.lower(), value.upper()))
    else:
        raise SystemExit("ephemeral secret scan input has an invalid field")
for name in sys.argv[2:]:
    diagnostic = Path(name).read_bytes()
    if any(secret in diagnostic for secret in secrets):
        raise SystemExit(f"retained diagnostic contains secret material: {Path(name).name}")
PY
}

publish_measurement_secret_scan_input() {
    python3 - "$MEASUREMENT_SECRET_SCAN_FILE" "$BEARER_HISTORY" \
        "$TMP_DIR/config/access-flow-root-key.pem" \
        "$TMP_DIR/config/access-flow-server-key.pem" \
        "$TMP_DIR/config/mitm-ca-key.pem" <<'PY'
import os
import base64
from pathlib import Path
import sys

output = Path(sys.argv[1])
bearers = [value for value in Path(sys.argv[2]).read_bytes().splitlines() if value]
if len(bearers) != 1:
    raise SystemExit("measurement must have exactly one actual bearer")
base64_chunks = []
der_hex_chunks = []

def regions(value, width):
    if len(value) < width:
        raise SystemExit("private-key fixture is shorter than a scan region")
    return (
        value[:width],
        value[(len(value) - width) // 2:(len(value) - width) // 2 + width],
        value[-width:],
    )

for name in sys.argv[3:]:
    body = b"".join(
        line
        for line in Path(name).read_bytes().splitlines()
        if line and not line.startswith(b"---")
    )
    try:
        der = base64.b64decode(body, validate=True)
    except ValueError:
        raise SystemExit("private-key fixture body is not valid base64") from None
    base64_chunks.extend(regions(body, 32))
    der_hex_chunks.extend(chunk.hex().encode("ascii") for chunk in regions(der, 24))
descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(descriptor, "wb") as sink:
    sink.write(b"contract_version=2\n")
    for bearer in bearers:
        sink.write(b"bearer=" + bearer + b"\n")
    for chunk in base64_chunks:
        sink.write(b"key_b64=" + chunk + b"\n")
    for chunk in der_hex_chunks:
        sink.write(b"key_der_hex=" + chunk + b"\n")
    sink.flush()
    os.fsync(sink.fileno())
PY
}

run_resource_measurement() {
    local load=$MEASUREMENT_LOAD
    local http_count=$((load / 2))
    local https_count=$((load - http_count))
    local agent_namespace_pid agent_container_init_pid
    publish_measurement_secret_scan_input \
        || fail "could not publish ephemeral measurement secret scan input"
    agent_namespace_pid=$(docker exec "$WORKLOAD_CONTAINER" cat /tmp/relay.pid)
    [[ $agent_namespace_pid =~ ^[1-9][0-9]*$ ]] \
        || fail "measured agent did not publish a namespace PID"
    agent_container_init_pid=$(docker inspect --format '{{.State.Pid}}' "$WORKLOAD_CONTAINER")
    [[ $agent_container_init_pid =~ ^[1-9][0-9]*$ ]] \
        || fail "could not resolve the workload container host PID"
    (
        umask 0077
        printf 'field\tvalue\n'
        printf 'namespace_pid\t%s\n' "$agent_namespace_pid"
        printf 'container_init_pid\t%s\n' "$agent_container_init_pid"
        printf 'agent_bin\t%s\n' "$AGENT_BIN"
        printf 'proxy_pid\t%s\n' "$ACL_PID"
        printf 'proxy_bin\t%s\n' "$ACL_PROXY_BIN"
    ) >"$MEASUREMENT_CONTROL_DIR/process-request.tsv"
    (umask 0077; : >"$MEASUREMENT_CONTROL_DIR/process-request-ready")
    wait_for_measurement_file "$MEASUREMENT_CONTROL_DIR/pids-bound" \
        "exact measured process binding"
    rm -- "$MEASUREMENT_CONTROL_DIR/process-request.tsv" \
        "$MEASUREMENT_CONTROL_DIR/process-request-ready" \
        "$MEASUREMENT_CONTROL_DIR/pids-bound"
    printf 'epoch\tphase\tactive_flows\thttp_flows\thttps_flows\tlogical_permits\tlogical_tasks\tknown_buffer_bytes\tminimum_active_projection_bytes\n' \
        >"$MEASUREMENT_CONTROL_DIR/logical.tsv"
    write_measurement_projection
    write_measurement_phase 1 idle 0 0 0
    touch "$MEASUREMENT_CONTROL_DIR/pids-ready"
    wait_for_measurement_file "$MEASUREMENT_CONTROL_DIR/sampler-start" "measurement sampler start"
    wait_for_measurement_file "$MEASUREMENT_CONTROL_DIR/idle-ready" "measured idle baseline"
    write_measurement_control_phase 2 ramp

    cat >"$TMP_DIR/measurement-clients.sh" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
load=$1
origin_ip=$2
http_count=$((load / 2))
https_count=$((load - http_count))
pids=()
printf '%s\n' "$$" >/tmp/measurement-clients.pid
for ((index = 0; index < http_count; index++)); do
    curl --fail --silent --show-error --noproxy '*' \
        --resolve "origin.test:80:$origin_ip" \
        "http://origin.test/measurement-hold/http-$index" >/dev/null &
    pids+=("$!")
done
for ((index = 0; index < https_count; index++)); do
    curl --fail --silent --show-error --noproxy '*' \
        --cacert /etc/acl-proxy/mitm-ca-cert.pem \
        --resolve "origin.test:443:$origin_ip" \
        "https://origin.test/measurement-hold/https-$index" >/dev/null &
    pids+=("$!")
done
touch /tmp/measurement-clients.ready
while [[ ! -f /tmp/measurement.stop ]]; do
    for pid in "${pids[@]}"; do
        if ! kill -0 "$pid" 2>/dev/null; then
            touch /tmp/measurement-clients.failed
            exit 1
        fi
    done
    sleep 0.02
done
for pid in "${pids[@]}"; do
    kill -TERM "$pid" 2>/dev/null || true
done
for pid in "${pids[@]}"; do
    wait "$pid" 2>/dev/null || true
done
touch /tmp/measurement-clients.drained
SH
    chmod 0755 "$TMP_DIR/measurement-clients.sh"
    docker cp "$TMP_DIR/measurement-clients.sh" \
        "$WORKLOAD_CONTAINER:/tmp/measurement-clients.sh" >/dev/null
    docker exec "$WORKLOAD_CONTAINER" chmod 0755 /tmp/measurement-clients.sh
    docker exec -d --user 65534:65534 "$WORKLOAD_CONTAINER" \
        /tmp/measurement-clients.sh "$load" "$ORIGIN_IP"
    MEASUREMENT_CLIENTS_STARTED=1
    for _ in {1..600}; do
        docker exec "$WORKLOAD_CONTAINER" test -f /tmp/measurement-clients.ready \
            2>/dev/null && break
        sleep 0.05
    done
    docker exec "$WORKLOAD_CONTAINER" test -f /tmp/measurement-clients.ready \
        || fail "measurement clients did not start"
    wait_for_measurement_active_flows "$load" 1
    write_measurement_phase 2 active "$load" "$http_count" "$https_count"
    touch "$MEASUREMENT_CONTROL_DIR/active-ready"
    wait_for_measurement_stop "$load"

    docker exec "$WORKLOAD_CONTAINER" touch /tmp/measurement.stop
    for _ in {1..600}; do
        docker exec "$WORKLOAD_CONTAINER" test -f /tmp/measurement-clients.drained \
            2>/dev/null && break
        sleep 0.05
    done
    docker exec "$WORKLOAD_CONTAINER" test -f /tmp/measurement-clients.drained \
        || fail "measurement clients did not drain"
    MEASUREMENT_CLIENTS_STARTED=0
    wait_for_measurement_active_flows 0
    write_measurement_phase 3 drained 0 0 0
    touch "$MEASUREMENT_CONTROL_DIR/drained-ready"
    wait_for_measurement_file "$MEASUREMENT_CONTROL_DIR/finish" "measurement finish"

    docker stop --time 15 "$WORKLOAD_CONTAINER" >/dev/null \
        || fail "measured workload consumer did not stop cleanly"
    docker cp "$WORKLOAD_CONTAINER:/tmp/agent.time-v" \
        "$MEASUREMENT_CONTROL_DIR/gateway.time-v" >/dev/null \
        || fail "measured agent did not publish GNU time evidence"
    docker cp "$WORKLOAD_CONTAINER:/tmp/relay.stderr" \
        "$TMP_DIR/relay.measurement.stderr" >/dev/null \
        || fail "could not capture measured agent diagnostics after stop"
    sanitize_diagnostics <"$TMP_DIR/relay.measurement.stderr" \
        >"$MEASUREMENT_CONTROL_DIR/gateway.stderr" \
        || fail "could not retain sanitized measured agent diagnostics"
    stop_acl_proxy
    cp "$TMP_DIR/acl-proxy.time-v" "$MEASUREMENT_CONTROL_DIR/proxy.time-v"
    sanitize_diagnostics <"$TMP_DIR/acl-proxy.first.stderr" \
        >"$MEASUREMENT_CONTROL_DIR/proxy.stderr"
    assert_measurement_diagnostics_secret_free \
        || fail "measured product diagnostics contain secret material"
    printf 'load=%s\nhttp_flows=%s\nhttps_flows=%s\n' \
        "$load" "$http_count" "$https_count" >"$MEASUREMENT_CONTROL_DIR/trial.txt"
    touch "$MEASUREMENT_CONTROL_DIR/trial-complete"
    printf '%s\n' "tls-access-flow-resource-trial=passed load=$load"
    SUCCESS=1
}

if [[ -n $MEASUREMENT_CONTROL_DIR ]]; then
    ACTIVE_BEARER=$IDENTITY_TOKEN
    start_workload
    assert_workload_relay_alive
    assert_workload_launcher_bearer_removed
    run_resource_measurement
    exit 0
fi

VALID_BEARER=$ACTIVE_BEARER
INVALID_BEARER=$(openssl rand -hex 24)
[[ $INVALID_BEARER =~ ^[0-9a-f]{48}$ && $INVALID_BEARER != "$VALID_BEARER" ]] \
    || fail "could not create a distinct invalid workload bearer"
printf '%s\n' "$INVALID_BEARER" >>"$BEARER_HISTORY"
ACTIVE_BEARER=$INVALID_BEARER
start_workload
assert_workload_relay_alive
if request_http http://origin.test/invalid-bearer >/dev/null 2>&1; then
    fail "invalid bearer reached an authenticated HTTP policy"
fi
[[ ! -s $PROVIDER_LOG ]] || fail "invalid bearer reached the authorization provider"
[[ ! -s $TMP_DIR/parent-logs/events.jsonl ]] || fail "invalid bearer reached the parent or origin"
docker rm -f "$WORKLOAD_CONTAINER" >/dev/null
ACTIVE_BEARER=$VALID_BEARER
unset INVALID_BEARER VALID_BEARER
start_workload
assert_workload_relay_alive
assert_workload_launcher_bearer_removed

python3 - "$WORKLOAD_CONTAINER" \
    "$TMP_DIR/config/mitm-ca-cert.pem" "$WORKLOAD_ARTIFACT" \
    "$WORKLOAD_CONFIG" "$TMP_DIR/workload.sh" "$IDENTITY_FIFO" \
    "$TMP_DIR/config/access-flow-root.pem" "$TMP_DIR/tls-firewall.sh" <<'PY'
import json
import pathlib
import subprocess
import sys

(
    container, public_ca, consumer, config, script, fifo, trust, firewall,
) = sys.argv[1:]
inspect = json.loads(subprocess.check_output(["docker", "inspect", container]))[0]
sources = {mount["Source"] for mount in inspect["Mounts"]}
expected = {public_ca, consumer, config, script, fifo, trust, firewall}
if sources != expected:
    raise SystemExit(f"unexpected workload mount inventory: {sources!r}")
if any("acl-proxy.toml" in source or "key" in pathlib.Path(source).name.lower() for source in sources):
    raise SystemExit("workload received ACL Proxy configuration or private-key material")
if any(item.startswith("AW_IDENTITY_TOKEN=") for item in inspect["Config"]["Env"]):
    raise SystemExit("Docker launch metadata retained AW_IDENTITY_TOKEN")
PY
docker exec "$WORKLOAD_CONTAINER" sh -eu -c "
    test ! -e /opt/aw-gateway/bin/acl-proxy
    test ! -e /etc/acl-proxy/acl-proxy.toml
    test ! -e /etc/acl-proxy/identity-token
    test ! -e /run/acl-proxy/authorization-provider.sock
    test -r /etc/acl-proxy/mitm-ca-cert.pem
    test -p /run/aw-gateway/identity-token.fifo
    test ! -e /run/acl-proxy/transparent-http.sock
    test ! -e /run/acl-proxy/transparent-https.sock
    test -r /mnt/aw-gateway/access-flow-root.pem
    test -r /run/aw-gateway/trust/access-flow-root.pem
    test ! -e /etc/aw-gateway/access-flow-server-key.pem
    test -x /opt/aw-gateway/bin/aw-container-agent
    test ! -e /opt/acl-proxy/bin/acl-proxy-access-flow-relay
"

body=$(request_http http://origin.test/allow)
[[ $body == 'origin:/allow:identity=absent:private=absent' ]] \
    || fail "redirected HTTP response or protected identity stripping was incorrect"
python3 - "$TMP_DIR/logs/tls-accepts.jsonl" <<'PY'
import json
import pathlib
import sys

records = [
    json.loads(line)
    for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
]
allow_flows = [record for record in records if record.get("route") == "http"]
# One invalid-bearer flow and one authenticated /allow flow have completed.
if len(allow_flows) != 2:
    raise SystemExit(
        f"two deliberate workload TCP flows used {len(allow_flows)} outer TLS connections"
    )
PY
https_body=$(request_https https://origin.test/secure)
[[ $https_body == 'origin:/secure:identity=absent:private=absent' ]] \
    || fail "redirected HTTPS response or protected identity stripping was incorrect"
delegate_body=$(docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --fail --silent --show-error --connect-timeout 2 --max-time 10 \
    --noproxy '*' --resolve "origin.test:80:$ORIGIN_IP" \
    -H 'x-private-smoke: private-smoke-value' http://origin.test/delegate)
[[ $delegate_body == 'origin:/delegate:identity=absent:private=absent' ]] \
    || fail "delegated HTTP response or protected identity stripping was incorrect"
delegate_pass_body=$(request_http http://origin.test/delegate-pass)
[[ $delegate_pass_body == 'origin:/delegate-pass:identity=absent:private=absent' ]] \
    || fail "provider pass did not continue to the following allow rule"
delegate_deny_status=$(docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --silent --show-error --connect-timeout 2 --max-time 10 --noproxy '*' \
    --resolve "origin.test:80:$ORIGIN_IP" --output /tmp/delegate-deny.body \
    --write-out '%{http_code}' http://origin.test/delegate-deny)
[[ $delegate_deny_status == 403 ]] || fail "provider deny returned HTTP $delegate_deny_status"
! grep -q '/delegate-deny' "$TMP_DIR/parent-logs/events.jsonl" \
    || fail "provider-denied request reached the parent proxy"
for _ in {1..100}; do
    [[ -s $PROVIDER_LOG ]] && break
    sleep 0.02
done
[[ -s $PROVIDER_LOG ]] || fail "delegated request did not reach the process provider"
python3 - "$PROVIDER_LOG" <<'PY'
import json
import pathlib
import sys

records = [
    json.loads(line)
    for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
]
matches = [record for record in records if record.get("ruleId") == "delegate-fixture"]
if len(matches) != 3:
    raise SystemExit(f"expected three delegate requests, found {len(matches)}")
request = next(record for record in matches if record["url"] == "http://origin.test/delegate")
identity = request.get("identity")
if identity != {
    "state": "authenticated",
    "principal": {
        "authority": "uds-stack-smoke",
        "id": "protected-workload",
        "kind": "service_account",
    },
    "groups": [
        {"authority": "uds-stack-smoke", "id": "network-clients"},
    ],
}:
    raise SystemExit(f"delegate identity projection was incorrect: {identity!r}")
expected_client = "0.0.0.0"
if request.get("clientIp") != expected_client:
    raise SystemExit(
        f"delegate client sentinel was {request.get('clientIp')!r}, "
        f"expected {expected_client!r}"
    )
if any(record.get("clientIp") != expected_client for record in matches):
    raise SystemExit("not every delegate decision used the remote client sentinel")
if request.get("headers", {}).get("x-private-smoke") != ["private-smoke-value"]:
    raise SystemExit("process provider did not receive the authorized private inbound field")
PY
for _ in {1..200}; do
        if find "$TMP_DIR/logs/captures" -type f -name '*.json' -print -quit \
            2>/dev/null | grep -q .; then
            break
        fi
        sleep 0.02
    done
    python3 - "$TMP_DIR/logs/captures" <<'PY'
import json
import pathlib
import sys

records = [
    json.loads(path.read_text(encoding="utf-8"))
    for path in pathlib.Path(sys.argv[1]).glob("*.json")
]
matches = [
    record for record in records
    if record.get("url") == "http://origin.test/delegate"
]
if len(matches) != 1:
    raise SystemExit(f"expected one delegated capture, found {len(matches)}")
if matches[0].get("client") != {"address": "0.0.0.0", "port": 0}:
    raise SystemExit(f"remote capture sentinel was incorrect: {matches[0].get('client')!r}")
PY
grep -F 'client_ip=0.0.0.0' "$TMP_DIR/logs/acl-proxy.log" >/dev/null \
    || fail "policy log did not preserve the remote client sentinel"
python3 - "$TMP_DIR" "$PROVIDER_LOG" <<'PY'
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
allowed = pathlib.Path(sys.argv[2])
needle = b"private-smoke-value"
hits = []
for path in root.rglob("*"):
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        continue
    if stat.S_ISREG(metadata.st_mode) and needle in path.read_bytes():
        hits.append(path)
if hits != [allowed]:
    raise SystemExit(f"private inbound value escaped its provider-only boundary: {hits!r}")
if allowed.read_bytes().count(needle) != 1:
    raise SystemExit("private inbound value did not appear exactly once in provider input")
PY

keepalive_connects=$(docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --fail --silent --show-error --connect-timeout 2 --max-time 10 \
    --write-out '%{num_connects}\n' --noproxy '*' \
    --resolve "origin.test:80:$ORIGIN_IP" \
    --output /dev/null http://origin.test/keepalive-one \
    --output /dev/null http://origin.test/keepalive-two)
keepalive_new_connections=$(awk '{total += $1} END {print total + 0}' \
    <<<"$keepalive_connects")
[[ $keepalive_new_connections == 1 ]] \
    || fail "two HTTP/1 requests opened $keepalive_new_connections downstream connections"

python3 - "$TMP_DIR/parent-logs/events.jsonl" <<'PY'
import json
import pathlib
import sys
records = [json.loads(line) for line in pathlib.Path(sys.argv[1]).read_text().splitlines()]
matches = [record for record in records if record.get("target") == "http://origin.test/allow"]
if len(matches) != 1 or matches[0].get("identity") != "absent":
    raise SystemExit("parent proxy observed the protected identity carrier")
connects = [record for record in records if record.get("event") == "connect"]
if [record.get("target") for record in connects] != ["origin.test:443"]:
    raise SystemExit(f"parent proxy did not observe one HTTPS CONNECT: {connects!r}")
PY

set +e
python3 - <<'PY' | docker exec -i --user 65534:65534 "$WORKLOAD_CONTAINER" \
    timeout 10 openssl s_client -quiet -ign_eof \
    -connect "$ORIGIN_IP:443" -servername origin.test \
    -CAfile /etc/acl-proxy/mitm-ca-cert.pem \
    >"$TMP_DIR/websocket.out" 2>"$TMP_DIR/websocket.stderr"
import sys

mask = b"\x01\x02\x03\x04"
payload = b"ping"
masked = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
request = (
    b"GET /websocket HTTP/1.1\r\n"
    b"Host: origin.test\r\n"
    b"Connection: Upgrade\r\n"
    b"Upgrade: websocket\r\n"
    b"Sec-WebSocket-Version: 13\r\n"
    b"Sec-WebSocket-Key: MDEyMzQ1Njc4OWFiY2RlZg==\r\n"
    b"\r\n"
)
sys.stdout.buffer.write(request + b"\x81\x84" + mask + masked)
PY
websocket_status=$?
set -e
[[ $websocket_status == 0 || $websocket_status == 1 ]] \
    || fail "WebSocket client exited with status $websocket_status"
python3 - "$TMP_DIR/websocket.out" <<'PY'
import pathlib
import sys

response = pathlib.Path(sys.argv[1]).read_bytes()
for marker in (
    b"101 Switching Protocols",
    b"X-Origin-Identity: absent",
    b"\x81\x04pong",
):
    if marker.lower() not in response.lower():
        raise SystemExit(f"WebSocket response omitted {marker!r}")
PY

deny_status=$(docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --silent --show-error --connect-timeout 2 --max-time 10 --noproxy '*' \
    --resolve "origin.test:80:$ORIGIN_IP" --output /tmp/denied.body \
    --write-out '%{http_code}' http://origin.test/denied)
[[ $deny_status == 403 ]] || fail "denied request returned HTTP $deny_status"
! grep -q '/denied' "$TMP_DIR/parent-logs/events.jsonl" \
    || fail "denied request reached the parent proxy"

printf 'GET /half-close HTTP/1.1\r\nHost: origin.test\r\nConnection: close\r\n\r\n' \
    | docker exec -i --user 65534:65534 "$WORKLOAD_CONTAINER" \
        timeout 10 nc -N "$ORIGIN_IP" 80 >"$TMP_DIR/half-close.response"
grep -F 'origin:/half-close:identity=absent:private=absent' "$TMP_DIR/half-close.response" >/dev/null \
    || fail "half-closed workload request did not receive its complete response"

CANCEL_PIDS=()
for index in 1 2 3 4; do
    (
        printf 'GET /cancel-%s HTTP/1.1\r\nHost: origin.test\r\n' "$index" \
            | docker exec -i --user 65534:65534 "$WORKLOAD_CONTAINER" \
                timeout 1 nc -N "$ORIGIN_IP" 80 >/dev/null 2>&1 || true
    ) &
    CANCEL_PIDS+=("$!")
done
for pid in "${CANCEL_PIDS[@]}"; do
    wait "$pid"
done
body=$(request_http http://origin.test/after-cancellation)
[[ $body == 'origin:/after-cancellation:identity=absent:private=absent' ]] \
    || fail "low-concurrency cancellation impaired a subsequent flow"

expected_stream_sha=$(python3 - <<'PY'
import hashlib
print(hashlib.sha256(b"0123456789abcdef" * 65536).hexdigest())
PY
)
STREAM_SIZE=$((16 * 1024 * 64))
docker exec "$WORKLOAD_CONTAINER" rm -f /tmp/stream.body
docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --fail --silent --show-error --connect-timeout 2 --max-time 10 \
    --noproxy '*' --resolve "origin.test:80:$ORIGIN_IP" \
    --output /tmp/stream.body http://origin.test/stream \
    >"$TMP_DIR/stream-client.stdout" 2>"$TMP_DIR/stream-client.stderr" &
STREAM_CLIENT_PID=$!
STREAM_INCREMENTAL=0
for _ in {1..100}; do
    stream_size=$(docker exec "$WORKLOAD_CONTAINER" \
        stat -c '%s' /tmp/stream.body 2>/dev/null || printf '0\n')
    if (( stream_size > 0 && stream_size < STREAM_SIZE )) \
        && kill -0 "$STREAM_CLIENT_PID" 2>/dev/null; then
        STREAM_INCREMENTAL=1
        break
    fi
    kill -0 "$STREAM_CLIENT_PID" 2>/dev/null || break
    sleep 0.05
done
(( STREAM_INCREMENTAL == 1 )) \
    || fail "stream response was not observable incrementally before completion"
wait "$STREAM_CLIENT_PID" || fail "incremental stream request failed"
stream_sha=$(docker exec "$WORKLOAD_CONTAINER" sha256sum /tmp/stream.body | awk '{print $1}')
[[ $stream_sha == "$expected_stream_sha" ]] || fail "streaming response digest mismatch"

assert_workload_relay_alive
if docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --fail --silent --connect-timeout 1 --max-time 2 --noproxy '*' \
    "http://$ORIGIN_IP:8080/escape" >/dev/null 2>&1; then
    fail "direct non-protected egress bypassed the fail-closed firewall"
fi
assert_workload_relay_alive

python3 - "$TMP_DIR/logs/tls-accepts.jsonl" <<'PY'
import collections
import json
import pathlib
import sys

counts = collections.Counter(
    json.loads(line)["route"]
    for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
)
# HTTP: invalid, allow, 3 delegates, keepalive, local deny, half-close,
# 4 cancellations, post-cancellation, and stream. HTTPS: request and WebSocket.
expected = {"http": 14, "https": 2}
if dict(counts) != expected:
    raise SystemExit(f"deliberate workload flows did not map 1:1 to outer TLS accepts: {counts!r}")
PY

docker stop --time 15 "$WORKLOAD_CONTAINER" >"$TMP_DIR/final-workload.stop" \
    || fail "final workload consumer did not stop cleanly"
[[ $(docker inspect "$WORKLOAD_CONTAINER" --format '{{.State.Running}}') == false ]] \
    || fail "final workload container remained running after stop"
[[ $(docker inspect "$WORKLOAD_CONTAINER" --format '{{.State.ExitCode}}') != 137 ]] \
    || fail "final workload consumer required forced termination"
stop_acl_proxy
capture_workload_observations final-workload
scan_observable_secrets

ACL_SHA=$(git -C "$ACL_REPO" rev-parse HEAD)
printf '%s\n' \
        "relay-consumer=integrated-agent" \
        "access-flow-transport=tls_tcp" \
        "access-path=iptables-redirect-so-original-dst-awaf-tls-tcp" \
        "http-allow=passed" \
        "nested-https-mitm=passed" \
        "websocket-upgrade-frame=passed" \
        "parent-proxy=passed" \
        "parent-connect=passed" \
        "identity-authentication=passed" \
        "identity-group-policy=passed" \
        "delegate-identity-projection=passed" \
        "delegate-allow-pass-deny=passed" \
        "private-inbound-field=passed" \
        "invalid-bearer-before-provider-upstream=passed" \
        "provider-client-sentinel=0.0.0.0" \
        "capture-client-sentinel=0.0.0.0:0" \
        "policy-log-client-sentinel=0.0.0.0" \
        "protected-carrier-stripping=passed" \
        "observable-secret-scan=passed" \
        "http1-downstream-keepalive=passed" \
        "deny-before-parent=passed" \
        "streaming=passed" \
        "half-close=passed" \
        "low-concurrency-cancellation=passed" \
        "one-outer-connection-per-workload-flow=passed" \
        "no-unix-fallback=passed" \
        "workload-isolation=passed" \
        "acl-repository-sha=$ACL_SHA" \
        "access-runtime-repository-sha=$EXPECTED_ACCESS_RUNTIME_SHA" \
        "aw-repository-sha=$EXPECTED_AW_SHA" \
        "base-image-id=$BASE_IMAGE_ID" \
        "workload-image-id=$WORKLOAD_IMAGE_ID" \
        "acl-proxy-sha256=$(sha256sum "$ACL_PROXY_BIN" | awk '{print $1}')" \
        "aw-container-agent-sha256=$(sha256sum "$AGENT_BIN" | awk '{print $1}')" \
        "aw-firewall-sha256=$(sha256sum "$TMP_DIR/tls-firewall.sh" | awk '{print $1}')"
printf '%s\n' 'tls-access-flow-stack-smoke=passed'
SUCCESS=1
