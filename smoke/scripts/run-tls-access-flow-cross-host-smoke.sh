#!/usr/bin/env bash

set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
ACL_PROXY_BIN=
AGENT_BIN=
ACL_REPO=
ACCESS_RUNTIME_REPO=
AW_REPO=
EXPECTED_ACL_SHA=
EXPECTED_ACCESS_RUNTIME_SHA=
EXPECTED_AW_SHA=
REMOTE_HOST=
REMOTE_ADDRESS=
REMOTE_SOURCE_ADDRESS=
REMOTE_INTERFACE=
REMOTE_CONTAINER_BACKEND=
LOCAL_MACHINE_ID_SHA256=
REMOTE_MACHINE_ID_SHA256=
DENIED_SOURCE_ADDRESS=
WORKLOAD_BASE_IMAGE=
WORKLOAD_BASE_IMAGE_ID=
WORKLOAD_IMAGE_PACKAGE_MANIFEST_SHA256=
WORKLOAD_AGENT_IMAGE=
WORKLOAD_AGENT_IMAGE_ID=
REMOTE_IMAGE=
REMOTE_IMAGE_ID=

TMP_DIR=
REMOTE_DIR=
NETWORK=
WORKLOAD_IMAGE=
WORKLOAD_CONTAINER=
AGENT_CONTAINER=
IDENTITY_WRITER_PID=
CAPTURE_CONTROLLER_PID=
REMOTE_STARTED=0
REMOTE_CLEANUP_FAILED=0
SUCCESS=0

SSH_OPTIONS=(-o BatchMode=yes -o ConnectTimeout=10)

fail() {
    printf 'TLS Access Flow cross-host smoke failed: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat >&2 <<'EOF'
Usage: run-tls-access-flow-cross-host-smoke.sh \
  --acl-proxy-bin <absolute-path> \
  --agent-bin <absolute-path> \
  --acl-repo <absolute-path> \
  --access-runtime-repo <absolute-path> \
  --aw-repo <absolute-path> \
  --expected-acl-sha <full-sha> \
  --expected-access-runtime-sha <full-sha> \
  --expected-aw-sha <full-sha> \
  --remote-host <ssh-host> \
  --workload-image <image> \
  --agent-image <image> \
  --remote-image <image> \
  [--remote-address <IPv4>] \
  [--remote-source-address <IPv4>] \
  [--remote-interface <interface>]
EOF
    exit 2
}

require_command() {
    command -v "$1" >/dev/null 2>&1 \
        || fail "required command is unavailable: $1"
}

require_absolute_executable() {
    local label=$1 path=$2
    [[ $path == /* && -f $path && ! -L $path && -x $path ]] \
        || fail "$label is not an absolute executable regular file: $path"
}

validate_ipv4() {
    local value=$1 part
    [[ $value =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || return 1
    IFS=. read -r -a parts <<<"$value"
    for part in "${parts[@]}"; do
        [[ $part =~ ^[0-9]+$ ]] || return 1
        ((10#$part <= 255)) || return 1
    done
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

remote_control() {
    ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" \
        "$REMOTE_DIR/bundle/remote-control.sh" "$@"
}

ssh_bounded() {
    local limit=$1
    shift
    timeout --foreground --signal=TERM --kill-after=2s "$limit" \
        ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" "$@"
}

remote_control_bounded() {
    local limit=$1
    shift
    ssh_bounded "$limit" "$REMOTE_DIR/bundle/remote-control.sh" "$@"
}

wait_capture_controller() {
    local signal state status
    [[ -n $CAPTURE_CONTROLLER_PID ]] || return 0
    for signal in none TERM KILL; do
        if [[ $signal != none ]]; then
            kill "-$signal" "$CAPTURE_CONTROLLER_PID" 2>/dev/null || true
        fi
        for _ in {1..100}; do
            if [[ ! -r /proc/$CAPTURE_CONTROLLER_PID/stat ]]; then
                break 2
            fi
            state=$(awk '{print $3}' "/proc/$CAPTURE_CONTROLLER_PID/stat") \
                || return 1
            [[ $state == Z ]] && break 2
            sleep 0.02
        done
    done
    if [[ -r /proc/$CAPTURE_CONTROLLER_PID/stat ]] \
        && [[ $(awk '{print $3}' "/proc/$CAPTURE_CONTROLLER_PID/stat") != Z ]]; then
        printf 'attached packet-capture controller survived bounded KILL teardown\n' \
            >&2
        return 1
    fi
    if wait "$CAPTURE_CONTROLLER_PID"; then
        status=0
    else
        status=$?
    fi
    CAPTURE_CONTROLLER_PID=
    if ((status != 0)); then
        printf 'attached packet-capture controller exited unsuccessfully: %s\n' \
            "$status" >&2
        return 1
    fi
}

stop_remote() {
    local output tag
    ((REMOTE_STARTED == 1)) || return 0
    tag=${SUFFIX:+aw-rt05-$SUFFIX}
    if output=$(remote_control_bounded 60s stop 2>&1); then
        REMOTE_STARTED=0
        wait_capture_controller || return 1
        return 0
    fi
    printf '%s\n' "$output" | sanitize_diagnostics >&2
    printf 'remote firewall cleanup failed; host=%s tag=%s control_dir=%s (preserved)\n' \
        "$REMOTE_HOST" "${tag:-unknown}" "$REMOTE_DIR" >&2
    REMOTE_CLEANUP_FAILED=1
    return 1
}

cleanup() {
    local status=$?
    trap - EXIT
    set +e
    if ((status == 0 && SUCCESS != 1)); then
        printf '%s\n' \
            'TLS Access Flow cross-host smoke exited without its success marker' >&2
        status=1
    fi
    if ((status != 0)); then
        if [[ -n $AGENT_CONTAINER ]]; then
            for log in /tmp/agent.stdout /tmp/agent.stderr; do
                docker cp "$AGENT_CONTAINER:$log" - 2>/dev/null \
                    | tar -xO 2>/dev/null \
                    | sanitize_diagnostics >&2 || true
            done
            docker logs --tail 100 "$AGENT_CONTAINER" 2>&1 \
                | sanitize_diagnostics >&2 || true
        fi
        if [[ -n $WORKLOAD_CONTAINER ]]; then
            for log in /tmp/firewall.stdout /tmp/firewall.stderr; do
                docker cp "$WORKLOAD_CONTAINER:$log" - 2>/dev/null \
                    | tar -xO 2>/dev/null \
                    | sanitize_diagnostics >&2 || true
            done
            docker logs --tail 100 "$WORKLOAD_CONTAINER" 2>&1 \
                | sanitize_diagnostics >&2 || true
        fi
        if [[ -n $REMOTE_DIR ]]; then
            remote_control_bounded 15s diagnostics 2>&1 \
                | sanitize_diagnostics >&2 || true
        fi
    fi
    if [[ -n $IDENTITY_WRITER_PID ]] \
        && kill -0 "$IDENTITY_WRITER_PID" 2>/dev/null; then
        kill -TERM "$IDENTITY_WRITER_PID" 2>/dev/null || true
        wait "$IDENTITY_WRITER_PID" 2>/dev/null || true
    fi
    if [[ -n $AGENT_CONTAINER ]]; then
        docker rm -f "$AGENT_CONTAINER" >/dev/null 2>&1 || status=1
        if docker inspect "$AGENT_CONTAINER" >/dev/null 2>&1; then
            printf 'agent carrier container remains after cleanup: %s\n' \
                "$AGENT_CONTAINER" >&2
            status=1
        fi
    fi
    if [[ -n $WORKLOAD_CONTAINER ]]; then
        docker rm -f "$WORKLOAD_CONTAINER" >/dev/null 2>&1 || status=1
        if docker inspect "$WORKLOAD_CONTAINER" >/dev/null 2>&1; then
            printf 'workload container remains after cleanup: %s\n' \
                "$WORKLOAD_CONTAINER" >&2
            status=1
        fi
    fi
    if [[ -n $NETWORK ]]; then
        docker network rm "$NETWORK" >/dev/null 2>&1 || status=1
        if docker network inspect "$NETWORK" >/dev/null 2>&1; then
            printf 'local workload network remains after cleanup: %s\n' \
                "$NETWORK" >&2
            status=1
        fi
    fi
    if ((REMOTE_STARTED == 1 && REMOTE_CLEANUP_FAILED == 0)) \
        && [[ -n $REMOTE_DIR ]]; then
        stop_remote || status=1
    fi
    if [[ -n $CAPTURE_CONTROLLER_PID ]]; then
        wait_capture_controller || status=1
    fi
    if [[ -n $REMOTE_DIR && $REMOTE_CLEANUP_FAILED == 0 ]]; then
        if ! ssh_bounded 15s rm -rf -- "$REMOTE_DIR" >/dev/null 2>&1; then
            printf 'could not remove remote control directory; host=%s control_dir=%s\n' \
                "$REMOTE_HOST" "$REMOTE_DIR" >&2
            status=1
        fi
    fi
    [[ -z $TMP_DIR ]] || rm -rf -- "$TMP_DIR"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

while (($#)); do
    (($# >= 2)) || usage
    case "$1" in
        --acl-proxy-bin) ACL_PROXY_BIN=$2 ;;
        --agent-bin) AGENT_BIN=$2 ;;
        --acl-repo) ACL_REPO=$2 ;;
        --access-runtime-repo) ACCESS_RUNTIME_REPO=$2 ;;
        --aw-repo) AW_REPO=$2 ;;
        --expected-acl-sha) EXPECTED_ACL_SHA=$2 ;;
        --expected-access-runtime-sha) EXPECTED_ACCESS_RUNTIME_SHA=$2 ;;
        --expected-aw-sha) EXPECTED_AW_SHA=$2 ;;
        --remote-host) REMOTE_HOST=$2 ;;
        --workload-image) WORKLOAD_BASE_IMAGE=$2 ;;
        --agent-image) WORKLOAD_AGENT_IMAGE=$2 ;;
        --remote-image) REMOTE_IMAGE=$2 ;;
        --remote-address) REMOTE_ADDRESS=$2 ;;
        --remote-source-address) REMOTE_SOURCE_ADDRESS=$2 ;;
        --remote-interface) REMOTE_INTERFACE=$2 ;;
        *) usage ;;
    esac
    shift 2
done

[[ -n $ACL_PROXY_BIN && -n $AGENT_BIN && -n $ACL_REPO \
    && -n $ACCESS_RUNTIME_REPO && -n $AW_REPO && -n $EXPECTED_ACL_SHA \
    && -n $EXPECTED_ACCESS_RUNTIME_SHA && -n $EXPECTED_AW_SHA \
    && -n $REMOTE_HOST && -n $WORKLOAD_BASE_IMAGE \
    && -n $WORKLOAD_AGENT_IMAGE && -n $REMOTE_IMAGE ]] || usage
[[ $REMOTE_HOST =~ ^[A-Za-z0-9._@-]+$ ]] \
    || fail "remote-host contains unsupported SSH/scp characters"
[[ ${REMOTE_HOST,,} != localhost && ${REMOTE_HOST,,} != localhost.* ]] \
    || fail "remote-host must not name the local loopback host"
[[ -z $REMOTE_INTERFACE || $REMOTE_INTERFACE =~ ^[A-Za-z0-9._:-]+$ ]] \
    || fail "remote-interface contains unsupported characters"
[[ $WORKLOAD_BASE_IMAGE =~ ^[A-Za-z0-9][A-Za-z0-9._:/@-]*$ ]] \
    || fail "workload container image reference contains unsupported characters"
[[ $WORKLOAD_AGENT_IMAGE =~ ^[A-Za-z0-9][A-Za-z0-9._:/-]*@sha256:[0-9a-f]{64}$ ]] \
    || fail "workload agent image must use an immutable sha256 digest"
[[ $REMOTE_IMAGE =~ ^[A-Za-z0-9][A-Za-z0-9._:/-]*@sha256:[0-9a-f]{64}$ ]] \
    || fail "remote container image must use an immutable sha256 digest"

for command in awk curl docker git ip openssl python3 sha256sum ssh tar timeout; do
    require_command "$command"
done
require_absolute_executable acl-proxy "$ACL_PROXY_BIN"
require_absolute_executable aw-container-agent "$AGENT_BIN"

canonical_repo() {
    local label=$1 path=$2 canonical top
    [[ $path == /* && -d $path && ! -L $path ]] \
        || fail "$label must be an absolute Git worktree"
    canonical=$(cd -- "$path" && pwd -P)
    top=$(git -C "$canonical" rev-parse --show-toplevel 2>/dev/null) \
        || fail "$label is not a Git worktree"
    [[ $top == "$canonical" ]] || fail "$label must name the worktree root"
    printf '%s\n' "$canonical"
}

require_expected_sha() {
    local label=$1 repository=$2 expected=$3 actual
    [[ $expected =~ ^[0-9a-f]{40}$ ]] \
        || fail "$label expected SHA must be a full lowercase SHA-1"
    actual=$(git -C "$repository" rev-parse HEAD)
    [[ $actual == "$expected" ]] \
        || fail "$label HEAD is $actual, expected $expected"
}

ACL_REPO=$(canonical_repo acl-repo "$ACL_REPO")
ACCESS_RUNTIME_REPO=$(canonical_repo access-runtime-repo "$ACCESS_RUNTIME_REPO")
AW_REPO=$(canonical_repo aw-repo "$AW_REPO")
[[ $AW_REPO == "$ROOT" ]] \
    || fail "aw-repo must be the repository containing this harness"
[[ $(readlink -f -- "$ACL_REPO/../access-runtime") == "$ACCESS_RUNTIME_REPO" ]] \
    || fail "ACL Proxy path dependencies do not resolve to access-runtime-repo"
require_expected_sha acl-proxy "$ACL_REPO" "$EXPECTED_ACL_SHA"
require_expected_sha access-runtime "$ACCESS_RUNTIME_REPO" \
    "$EXPECTED_ACCESS_RUNTIME_SHA"
require_expected_sha aw-gateway "$AW_REPO" "$EXPECTED_AW_SHA"
PINNED_ACCESS_RUNTIME_SHA=$(
    python3 "$AW_REPO/scripts/validate-access-runtime-pin.py" \
        "$AW_REPO/Cargo.toml" \
        "$AW_REPO/Cargo.lock" \
        https://github.com/kcosr/access-runtime.git
)
[[ $PINNED_ACCESS_RUNTIME_SHA == "$EXPECTED_ACCESS_RUNTIME_SHA" ]] \
    || fail "AW Gateway Access Runtime pin does not match the accepted Runtime"

docker info >/dev/null 2>&1 || fail "local Docker daemon is unavailable"
docker image inspect "$WORKLOAD_BASE_IMAGE" >/dev/null 2>&1 \
    || fail "required local image is not cached: $WORKLOAD_BASE_IMAGE"
WORKLOAD_BASE_IMAGE_ID=$(
    docker image inspect "$WORKLOAD_BASE_IMAGE" --format '{{.Id}}'
)
[[ $WORKLOAD_BASE_IMAGE_ID =~ ^sha256:[0-9a-f]{64}$ ]] \
    || fail "workload image ID is not a canonical sha256 image ID"
docker run --rm --pull=never --user 0:0 "$WORKLOAD_BASE_IMAGE" sh -eu -c '
for command in bash curl ip iptables nsenter; do
    command -v "$command" >/dev/null
done
command -v apk >/dev/null
' || fail "explicit workload image lacks a required cached tool"
WORKLOAD_IMAGE_PACKAGE_MANIFEST_SHA256=$(
    docker run --rm --pull=never --user 0:0 "$WORKLOAD_BASE_IMAGE" \
        sh -eu -c 'apk info -vv' \
        | LC_ALL=C sort | sha256sum | awk '{print $1}'
) || fail "could not inventory the explicit workload image packages"
[[ $WORKLOAD_IMAGE_PACKAGE_MANIFEST_SHA256 =~ ^[0-9a-f]{64}$ ]] \
    || fail "workload image package manifest digest is not canonical sha256"
docker image inspect "$WORKLOAD_AGENT_IMAGE" >/dev/null 2>&1 \
    || fail "required workload agent image is not cached: $WORKLOAD_AGENT_IMAGE"
WORKLOAD_AGENT_IMAGE_ID=$(
    docker image inspect "$WORKLOAD_AGENT_IMAGE" --format '{{.Id}}'
)
[[ $WORKLOAD_AGENT_IMAGE_ID =~ ^sha256:[0-9a-f]{64}$ ]] \
    || fail "workload agent image ID is not a canonical sha256 image ID"
docker run --rm --pull=never --user 0:0 "$WORKLOAD_AGENT_IMAGE" \
    bash -eu -c '
command -v bash >/dev/null
command -v install >/dev/null
[[ $(getconf GNU_LIBC_VERSION) == glibc\ * ]]
' || fail "pinned agent carrier image lacks its required glibc tools"
[[ -r /etc/machine-id ]] || fail "local machine identity is unavailable"
LOCAL_MACHINE_ID_SHA256=$(sha256sum /etc/machine-id | awk '{print $1}')
ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" true \
    || fail "remote host is unavailable through noninteractive SSH"

REMOTE_FACTS=$(
    ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" 'set -eu
for command in docker ip podman python3 sha256sum stat sudo tar tcpdump \
    iptables iptables-save; do
    command -v "$command" >/dev/null || exit 20
done
sudo -n true
docker info >/dev/null
case $(docker --version) in
    "podman version "*) ;;
    *) exit 21 ;;
esac
backend=$(podman info --format json | python3 -c '"'"'
import json
import sys

info = json.load(sys.stdin)
security = info.get("host", {}).get("security", {})
if security.get("rootless") is not True:
    raise SystemExit(1)
if info.get("host", {}).get("networkBackend") != "netavark":
    raise SystemExit(1)
print("podman-rootless-netavark")
'"'"')
machine_id=$(cat /etc/machine-id)
case "$machine_id" in
    *[!0-9a-f]*|"") exit 22 ;;
esac
docker image inspect '"$(printf '%q' "$REMOTE_IMAGE")"' >/dev/null
remote_image_id=$(docker image inspect \
    '"$(printf '%q' "$REMOTE_IMAGE")"' --format "{{.Id}}")
case "$remote_image_id" in
    [0-9a-f][0-9a-f]*) remote_image_id="sha256:$remote_image_id" ;;
esac
provider_python=$(docker run --rm --pull=never --security-opt label=disable \
    --user 0:0 \
    '"$(printf '%q' "$REMOTE_IMAGE")"' sh -c "command -v python3")
printf "ssh_source=%s\n" "${SSH_CONNECTION%% *}"
printf "home=%s\n" "$HOME"
printf "provider_python=%s\n" "$provider_python"
printf "remote_image_id=%s\n" "$remote_image_id"
printf "container_backend=%s\n" "$backend"
printf "machine_id=%s\n" "$machine_id"
'
) || fail "remote prerequisites or cached image are unavailable"
SSH_SOURCE=$(awk -F= '$1 == "ssh_source" {print $2}' <<<"$REMOTE_FACTS")
REMOTE_HOME=$(awk -F= '$1 == "home" {print $2}' <<<"$REMOTE_FACTS")
REMOTE_PROVIDER_PYTHON=$(
    awk -F= '$1 == "provider_python" {print $2}' <<<"$REMOTE_FACTS"
)
REMOTE_IMAGE_ID=$(
    awk -F= '$1 == "remote_image_id" {print $2}' <<<"$REMOTE_FACTS"
)
REMOTE_CONTAINER_BACKEND=$(
    awk -F= '$1 == "container_backend" {print $2}' <<<"$REMOTE_FACTS"
)
REMOTE_MACHINE_ID=$(
    awk -F= '$1 == "machine_id" {print $2}' <<<"$REMOTE_FACTS"
)
validate_ipv4 "$SSH_SOURCE" || fail "remote SSH source discovery was invalid"
[[ $REMOTE_HOME == /* && $REMOTE_HOME =~ ^[A-Za-z0-9._/-]+$ ]] \
    || fail "remote HOME is not a safe absolute path"
[[ $REMOTE_PROVIDER_PYTHON =~ ^/[A-Za-z0-9._/-]+$ ]] \
    || fail "remote image Python path is not a safe absolute path"
[[ $REMOTE_IMAGE_ID =~ ^sha256:[0-9a-f]{64}$ ]] \
    || fail "remote image ID is not a canonical sha256 image ID"
[[ $REMOTE_CONTAINER_BACKEND == podman-rootless-netavark ]] \
    || fail "remote container backend does not match the INPUT firewall contract"
[[ $REMOTE_MACHINE_ID =~ ^[0-9a-f]{32}$ ]] \
    || fail "remote machine identity is invalid"
REMOTE_MACHINE_ID_SHA256=$(
    printf '%s\n' "$REMOTE_MACHINE_ID" | sha256sum | awk '{print $1}'
)
[[ $REMOTE_MACHINE_ID_SHA256 != "$LOCAL_MACHINE_ID_SHA256" ]] \
    || fail "remote host resolves to the local machine"

if [[ -z $REMOTE_ADDRESS ]]; then
    REMOTE_ADDRESS=$(
        python3 - "$REMOTE_HOST" <<'PY'
import socket
import sys

host = sys.argv[1].rsplit("@", 1)[-1]
addresses = sorted({
    item[4][0] for item in socket.getaddrinfo(
        host, None, socket.AF_INET, socket.SOCK_STREAM
    )
})
if len(addresses) != 1:
    raise SystemExit(
        f"remote host must resolve to exactly one IPv4 address, got {addresses!r}"
    )
print(addresses[0])
PY
    ) || fail "could not discover one remote IPv4 address"
fi
validate_ipv4 "$REMOTE_ADDRESS" || fail "remote-address must be IPv4"
[[ $REMOTE_ADDRESS != 127.* ]] || fail "remote-address must not be loopback"
if ip -o -4 addr show | awk '{print $4}' | cut -d/ -f1 \
    | grep -Fxq -- "$REMOTE_ADDRESS"; then
    fail "remote-address is assigned to the local machine"
fi
if [[ -z $REMOTE_SOURCE_ADDRESS ]]; then
    REMOTE_SOURCE_ADDRESS=$SSH_SOURCE
fi
validate_ipv4 "$REMOTE_SOURCE_ADDRESS" \
    || fail "remote-source-address must be IPv4"

if [[ -z $REMOTE_INTERFACE ]]; then
    REMOTE_ROUTE_INTERFACE=$(
        ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" \
            ip route get "$REMOTE_SOURCE_ADDRESS" \
        | awk '{for (field = 1; field <= NF; field++) if ($field == "dev") {print $(field + 1); exit}}'
    )
    [[ $REMOTE_ROUTE_INTERFACE =~ ^[A-Za-z0-9._:-]+$ ]] \
        || fail "remote route interface discovery was invalid"
    REMOTE_INTERFACE=$(
        ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" sh -s -- \
            "$REMOTE_ROUTE_INTERFACE" <<'REMOTE_INTERFACE_DISCOVERY'
set -eu
route_interface=$1
if [ ! -d "/sys/class/net/$route_interface/bridge" ]; then
    printf '%s\n' "$route_interface"
    exit 0
fi

physical_members=
for member_path in "/sys/class/net/$route_interface/brif/"*; do
    [ -e "$member_path" ] || continue
    member=${member_path##*/}
    if [ -e "/sys/class/net/$member/device" ]; then
        physical_members="${physical_members}${physical_members:+
}$member"
    fi
done

[ -n "$physical_members" ]
[ "$(printf '%s\n' "$physical_members" | wc -l)" -eq 1 ]
printf '%s\n' "$physical_members"
REMOTE_INTERFACE_DISCOVERY
    ) || fail \
        "remote route bridge must have one physical member or --remote-interface must be specified"
fi
[[ $REMOTE_INTERFACE =~ ^[A-Za-z0-9._:-]+$ ]] \
    || fail "remote capture interface discovery was invalid"

DENIED_SOURCE_ADDRESS=$(
    ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" \
        ip -o -4 addr show scope global \
    | awk -v public="$REMOTE_ADDRESS" -v allowed="$REMOTE_SOURCE_ADDRESS" '
        {
            split($4, address, "/")
            if (address[1] != public && address[1] != allowed) {
                print address[1]
                exit
            }
        }
    '
)
validate_ipv4 "$DENIED_SOURCE_ADDRESS" \
    || fail "remote host lacks a distinct source for the firewall-negative probe"

umask 077
TMP_BASE=${AW_TLS_CROSS_HOST_SMOKE_TEMP_BASE:-${HOME:-}}
[[ $TMP_BASE == /* && -d $TMP_BASE && ! -L $TMP_BASE ]] \
    || fail "a private absolute local temporary base is required"
TMP_DIR=$(mktemp -d "$TMP_BASE/.aw-tls-cross-host.XXXXXX")
mkdir -m 0700 "$TMP_DIR/bundle" "$TMP_DIR/remote-state" \
    "$TMP_DIR/local-evidence"
BEARER_HISTORY="$TMP_DIR/.bearer-history"
VALID_BEARER=$(openssl rand -hex 24)
INVALID_BEARER=$(openssl rand -hex 24)
PRIVATE_VALUE=$(openssl rand -hex 24)
printf '%s\n%s\n%s\n' "$VALID_BEARER" "$INVALID_BEARER" "$PRIVATE_VALUE" \
    >"$BEARER_HISTORY"

SUFFIX=$(openssl rand -hex 6)
[[ $SUFFIX =~ ^[0-9a-f]{12}$ ]] || fail "could not create a resource suffix"
NETWORK="aw-tls-cross-$SUFFIX"
WORKLOAD_IMAGE=$WORKLOAD_BASE_IMAGE
WORKLOAD_IMAGE_ID=$WORKLOAD_BASE_IMAGE_ID

read -r TLS_HTTP_PORT TLS_HTTPS_PORT < <(
    ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" \
        python3 - "$REMOTE_ADDRESS" <<'PY'
import socket
import sys

address = sys.argv[1]
sockets = []
ports = []
for _ in range(2):
    sock = socket.socket()
    sock.bind((address, 0))
    sockets.append(sock)
    ports.append(sock.getsockname()[1])
print(*ports)
for sock in sockets:
    sock.close()
PY
)
for port in "$TLS_HTTP_PORT" "$TLS_HTTPS_PORT"; do
    [[ $port =~ ^[0-9]+$ ]] && ((port >= 1024 && port <= 65535)) \
        || fail "remote port allocation returned an invalid port"
done
[[ $TLS_HTTP_PORT != "$TLS_HTTPS_PORT" ]] \
    || fail "remote port allocation returned duplicate ports"

cp -- "$ACL_PROXY_BIN" "$TMP_DIR/bundle/acl-proxy"
chmod 0700 "$TMP_DIR/bundle/acl-proxy"
printf '%s' "$VALID_BEARER" >"$TMP_DIR/bundle/identity-token"
printf '%s' "$PRIVATE_VALUE" >"$TMP_DIR/bundle/private-expected"
chmod 0600 "$TMP_DIR/bundle/identity-token" \
    "$TMP_DIR/bundle/private-expected"

openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 \
    -out "$TMP_DIR/bundle/mitm-ca-key.pem" >/dev/null 2>&1
openssl req -x509 -new -key "$TMP_DIR/bundle/mitm-ca-key.pem" -days 1 \
    -subj '/CN=RT05 cross-host MITM CA' \
    -addext 'basicConstraints=critical,CA:TRUE' \
    -addext 'keyUsage=critical,keyCertSign,cRLSign' \
    -out "$TMP_DIR/bundle/mitm-ca-cert.pem" >/dev/null 2>&1

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -subj '/CN=RT05 origin root' \
    -addext 'basicConstraints=critical,CA:TRUE' \
    -addext 'keyUsage=critical,keyCertSign,cRLSign' \
    -keyout "$TMP_DIR/bundle/origin-root-key.pem" \
    -out "$TMP_DIR/bundle/origin-root.pem" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes \
    -subj '/CN=origin.test' \
    -keyout "$TMP_DIR/bundle/origin-key.pem" \
    -out "$TMP_DIR/bundle/origin.csr" >/dev/null 2>&1
cat >"$TMP_DIR/bundle/origin.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:origin.test
EOF
openssl x509 -req -days 1 -sha256 \
    -in "$TMP_DIR/bundle/origin.csr" \
    -CA "$TMP_DIR/bundle/origin-root.pem" \
    -CAkey "$TMP_DIR/bundle/origin-root-key.pem" -CAcreateserial \
    -extfile "$TMP_DIR/bundle/origin.ext" \
    -out "$TMP_DIR/bundle/origin-cert.pem" >/dev/null 2>&1

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -subj '/CN=RT05 Access Flow root' \
    -addext 'basicConstraints=critical,CA:TRUE' \
    -addext 'keyUsage=critical,keyCertSign,cRLSign' \
    -keyout "$TMP_DIR/bundle/access-flow-root-key.pem" \
    -out "$TMP_DIR/bundle/access-flow-root.pem" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes \
    -subj '/CN=proxy.access-flow.test' \
    -keyout "$TMP_DIR/bundle/access-flow-server-key.pem" \
    -out "$TMP_DIR/bundle/access-flow-server.csr" >/dev/null 2>&1
cat >"$TMP_DIR/bundle/access-flow-server.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:proxy.access-flow.test
EOF
openssl x509 -req -days 1 -sha256 \
    -in "$TMP_DIR/bundle/access-flow-server.csr" \
    -CA "$TMP_DIR/bundle/access-flow-root.pem" \
    -CAkey "$TMP_DIR/bundle/access-flow-root-key.pem" -CAcreateserial \
    -extfile "$TMP_DIR/bundle/access-flow-server.ext" \
    -out "$TMP_DIR/bundle/access-flow-server-leaf.pem" >/dev/null 2>&1
cat "$TMP_DIR/bundle/access-flow-server-leaf.pem" \
    "$TMP_DIR/bundle/access-flow-root.pem" \
    >"$TMP_DIR/bundle/access-flow-server-chain.pem"
chmod 0600 "$TMP_DIR/bundle/"*-key.pem
chmod 0644 "$TMP_DIR/bundle/"*-cert.pem \
    "$TMP_DIR/bundle/"*-root.pem \
    "$TMP_DIR/bundle/access-flow-server-chain.pem"

cat >"$TMP_DIR/bundle/origin.py" <<'PY'
import http.server
import json
import ssl
import sys
import threading
import time

lock = threading.Lock()
log_path = "/state/origin.jsonl"


def record(**value):
    with lock, open(log_path, "a", encoding="utf-8") as output:
        output.write(json.dumps(value, separators=(",", ":")) + "\n")
        output.flush()


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        identity = "present" if self.headers.get("x-aw-identity-token") else "absent"
        private = "present" if self.headers.get("x-rt05-private") else "absent"
        record(path=self.path, identity=identity, private=private)
        if self.path.startswith("/stream"):
            chunks = [b"0123456789abcdef"] * 256
            self.send_response(200)
            self.send_header("Content-Length", str(sum(map(len, chunks))))
            self.end_headers()
            for chunk in chunks:
                self.wfile.write(chunk)
                self.wfile.flush()
                time.sleep(0.001)
            return
        body = f"origin:{self.path}:identity={identity}:private={private}".encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format, *_args):
        pass


http_server = http.server.ThreadingHTTPServer(("0.0.0.0", 80), Handler)
https = http.server.ThreadingHTTPServer(("0.0.0.0", 443), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain("/fixture/origin-cert.pem", "/fixture/origin-key.pem")
https.socket = context.wrap_socket(https.socket, server_side=True)
threading.Thread(target=http_server.serve_forever, daemon=True).start()
https.serve_forever()
PY

cat >"$TMP_DIR/bundle/parent.py" <<'PY'
import http.client
import http.server
import json
import select
import socket
import urllib.parse

log_path = "/state/parent.jsonl"


def record(**value):
    with open(log_path, "a", encoding="utf-8") as output:
        output.write(json.dumps(value, separators=(",", ":")) + "\n")
        output.flush()


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_CONNECT(self):
        host, port = self.path.rsplit(":", 1)
        record(event="connect", target=self.path)
        upstream = socket.create_connection((host, int(port)), timeout=5)
        self.send_response(200, "Connection established")
        self.end_headers()
        self.connection.setblocking(False)
        upstream.setblocking(False)
        streams = [self.connection, upstream]
        try:
            while streams:
                readable, _, _ = select.select(streams, [], [], 15)
                if not readable:
                    break
                for source in readable:
                    data = source.recv(16384)
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
        private = "present" if self.headers.get("x-rt05-private") else "absent"
        identity = "present" if self.headers.get("x-aw-identity-token") else "absent"
        record(event="request", target=self.path, private=private, identity=identity)
        headers = {
            name: value for name, value in self.headers.items()
            if name.lower() not in {
                "connection", "proxy-connection", "proxy-authorization",
                "transfer-encoding", "upgrade",
            }
        }
        headers["Host"] = target.netloc
        upstream = http.client.HTTPConnection(
            target.hostname, target.port or 80, timeout=5
        )
        try:
            upstream.request("GET", target.path or "/", headers=headers)
            response = upstream.getresponse()
            body = response.read()
            self.send_response(response.status)
            for name, value in response.getheaders():
                if name.lower() not in {
                    "connection", "content-length", "transfer-encoding",
                }:
                    self.send_header(name, value)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        finally:
            upstream.close()

    def log_message(self, _format, *_args):
        pass


http.server.ThreadingHTTPServer(("0.0.0.0", 8888), Handler).serve_forever()
PY

cat >"$TMP_DIR/bundle/provider.py" <<'PY'
import json
import pathlib
import sys

expected_private = pathlib.Path(sys.argv[2]).read_text(encoding="ascii")
for line in sys.stdin:
    request = json.loads(line)
    identity = request.get("identity")
    private_valid = request.get("headers", {}).get("x-rt05-private") == [
        expected_private
    ]
    identity_valid = identity == {
        "state": "authenticated",
        "principal": {
            "authority": "rt05-cross-host",
            "id": "workload-a",
            "kind": "service_account",
        },
        "groups": [
            {"authority": "rt05-cross-host", "id": "network-clients"},
        ],
    }
    sanitized = {
        "ruleId": request.get("ruleId"),
        "clientIp": request.get("clientIp"),
        "identityValid": identity_valid,
        "privateValid": private_valid,
    }
    with open(sys.argv[1], "a", encoding="utf-8") as output:
        output.write(json.dumps(sanitized, separators=(",", ":")) + "\n")
        output.flush()
    decision = "allow" if identity_valid and private_valid else "deny"
    print(json.dumps({
        "id": request["id"],
        "type": "response",
        "decision": decision,
    }, separators=(",", ":")), flush=True)
PY

cat >"$TMP_DIR/bundle/acl-proxy.toml" <<EOF
schema_version = 4

[service]
request_timeout = "15s"
shutdown_drain_timeout = "2s"

[control]
bind = "127.0.0.1:9080"
base_path = "/_acl-proxy"
request_header_timeout = "1s"
request_body_timeout = "2s"
max_connections = 8
max_request_body_bytes = 65536

[listeners.transparent_http]
http_versions = ["http1"]
max_connections = 64

[listeners.transparent_http.identity]
mode = "required"

[listeners.transparent_http.endpoint]
kind = "access_flow"
admission_timeout = "2s"
allowed_destination_ports = [80]

[listeners.transparent_http.endpoint.transport]
kind = "tls_tcp"
bind = "0.0.0.0:$TLS_HTTP_PORT"
handshake_timeout = "2s"

[listeners.transparent_http.endpoint.transport.server_identity]
certificate_chain = "/bundle/access-flow-server-chain.pem"
private_key = "/bundle/access-flow-server-key.pem"

[listeners.transparent_http.endpoint.transport.abuse_control]
max_handshakes = 16
max_connections_per_source = 32
handshakes_per_second = 100
handshake_burst = 100
source_table_capacity = 64

[listeners.transparent_https]
http_versions = ["http1"]
handshake_timeout = "2s"
max_connections = 32

[listeners.transparent_https.identity]
mode = "required"

[listeners.transparent_https.endpoint]
kind = "access_flow"
admission_timeout = "2s"
allowed_destination_ports = [443]

[listeners.transparent_https.endpoint.transport]
kind = "tls_tcp"
bind = "0.0.0.0:$TLS_HTTPS_PORT"
handshake_timeout = "2s"

[listeners.transparent_https.endpoint.transport.server_identity]
certificate_chain = "/bundle/access-flow-server-chain.pem"
private_key = "/bundle/access-flow-server-key.pem"

[listeners.transparent_https.endpoint.transport.abuse_control]
max_handshakes = 16
max_connections_per_source = 32
handshakes_per_second = 100
handshake_burst = 100
source_table_capacity = 64

[mitm]
mode = "files"
ca_certificate = "/bundle/mitm-ca-cert.pem"
ca_private_key = "/bundle/mitm-ca-key.pem"
directory = "/state/generated-certs"

[identity]
max_pending_authentications = 32
max_pending_authentications_per_connection = 4

[identity.resolver]
kind = "static"
authority = "rt05-cross-host"

[[identity.resolver.principals]]
id = "workload-a"
kind = "service_account"

[[identity.resolver.groups]]
id = "network-clients"
members = ["workload-a"]

[[identity.resolver.tokens]]
id = "workload-a-primary"
principal = "workload-a"
source = "file"
path = "/bundle/identity-token"

[authorization.providers.cross_host]
kind = "process"
command = "$REMOTE_PROVIDER_PYTHON"
args = ["/bundle/provider.py", "/state/provider.jsonl", "/bundle/private-expected"]
timeout = "2s"
inherit_environment = false
include_headers = ["x-rt05-private"]
include_identity = true
max_stdout_line_bytes = 65536
max_pending_requests = 8
restart_backoff = "100ms"
retire_timeout = "30s"

[credentials.inbound.rt05_private]
header = "x-rt05-private"
process_providers = ["cross_host"]

[egress]
route = "parent_proxy"

[egress.parent_proxy]
kind = "generic"
url = "http://parent:8888"

[egress.origin_tls]
trust = "custom"
ca_certificate = "/bundle/origin-root.pem"
http_versions = ["http1"]

[policy]
default = "deny"

[[policy.rules]]
id = "principal"
decision = "allow"
urls = ["http://origin.test/principal"]
identity_states = ["authenticated"]
identity_subjects = [
  { kind = "principal", authority = "rt05-cross-host", id = "workload-a" },
]

[[policy.rules]]
id = "group"
decision = "allow"
urls = ["http://origin.test/group"]
identity_states = ["authenticated"]
identity_subjects = [
  { kind = "group", authority = "rt05-cross-host", id = "network-clients" },
]

[[policy.rules]]
id = "delegate"
decision = "delegate"
authorization_provider = "cross_host"
urls = ["http://origin.test/delegate"]
identity_states = ["authenticated"]
identity_subjects = [
  { kind = "group", authority = "rt05-cross-host", id = "network-clients" },
]

[[policy.rules]]
id = "remaining"
decision = "allow"
urls = [
  "http://origin.test/keepalive-one",
  "http://origin.test/keepalive-two",
  "https://origin.test/secure",
]
identity_states = ["authenticated"]
identity_subjects = [
  { kind = "group", authority = "rt05-cross-host", id = "network-clients" },
]

[redaction.profiles.capture]
rules = [{ literals = ["never-present-cross-host"] }]

[observation.capture]
events = ["request.allowed"]
redaction_profile = "capture"
directory = "/state/captures"
filename = "{requestId}-{suffix}.json"
max_body_bytes = 1024
max_inflight_body_bytes = 134217728
max_pending_records = 32
max_files = 64
max_total_bytes = 4194304

[observation.logging]
level = "info"
directory = "/state/proxy-logs"
max_bytes = 2097152
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

cat >"$TMP_DIR/bundle/remote-control.sh" <<'REMOTE'
#!/usr/bin/env bash
set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
BUNDLE="$ROOT/bundle"
STATE="$ROOT/state"
ACTION=${1:-}
shift || true

load_state() {
    # shellcheck disable=SC1091
    source "$STATE/runtime.env"
}

remove_firewall() {
    [[ -f $STATE/runtime.env ]] || return 0
    load_state
    local attempt firewall
    for attempt in {1..5}; do
        sudo -n iptables -w 5 -C INPUT \
            -s "$SOURCE_ADDRESS/32" -d "$PUBLIC_ADDRESS/32" -p tcp \
            -m multiport --dports "$HTTP_PORT,$HTTPS_PORT" \
            -m comment --comment "$TAG.allow" -j ACCEPT 2>/dev/null \
            || break
        sudo -n iptables -w 5 -D INPUT \
            -s "$SOURCE_ADDRESS/32" -d "$PUBLIC_ADDRESS/32" -p tcp \
            -m multiport --dports "$HTTP_PORT,$HTTPS_PORT" \
            -m comment --comment "$TAG.allow" -j ACCEPT 2>/dev/null \
            || sleep 0.1
    done
    for attempt in {1..5}; do
        sudo -n iptables -w 5 -C INPUT \
            -d "$PUBLIC_ADDRESS/32" -p tcp \
            -m multiport --dports "$HTTP_PORT,$HTTPS_PORT" \
            -m comment --comment "$TAG.deny" -j DROP 2>/dev/null \
            || break
        sudo -n iptables -w 5 -D INPUT \
            -d "$PUBLIC_ADDRESS/32" -p tcp \
            -m multiport --dports "$HTTP_PORT,$HTTPS_PORT" \
            -m comment --comment "$TAG.deny" -j DROP 2>/dev/null \
            || sleep 0.1
    done
    firewall=$(sudo -n iptables-save) || {
        printf 'could not verify tagged firewall cleanup: %s\n' "$TAG" >&2
        return 1
    }
    if grep -Fq -- "$TAG." <<<"$firewall"; then
        printf 'tagged firewall rules remain after bounded removal: %s\n' \
            "$TAG" >&2
        return 1
    fi
}

stop_capture() {
    [[ -f $STATE/tcpdump.pid ]] || return 0
    local pid start_time current_start_time signal capture_status
    read -r pid start_time <"$STATE/tcpdump.pid"
    [[ $pid =~ ^[1-9][0-9]*$ && $start_time =~ ^[1-9][0-9]*$ ]] || {
        printf 'invalid tcpdump process identity during teardown\n' >&2
        return 1
    }
    capture_is_running() {
        [[ -r /proc/$pid/stat ]] || return 1
        current_start_time=$(awk '{print $22}' "/proc/$pid/stat") || return 2
        [[ $current_start_time == "$start_time" ]] || return 1
        sudo -n kill -0 "$pid" 2>/dev/null && return 0
        [[ -r /proc/$pid/stat ]] || return 1
        current_start_time=$(awk '{print $22}' "/proc/$pid/stat") || return 2
        [[ $current_start_time == "$start_time" ]] || return 1
        return 2
    }
    if capture_is_running; then
        :
    else
        capture_status=$?
        ((capture_status == 1)) || {
            printf 'could not verify tcpdump process identity during teardown\n' >&2
            return 1
        }
        rm -f "$STATE/tcpdump.pid"
        return 0
    fi
    for signal in INT TERM KILL; do
        if ! sudo -n kill "-$signal" "$pid" 2>/dev/null; then
            if capture_is_running; then
                printf 'could not signal tcpdump during teardown: %s\n' \
                    "$signal" >&2
                return 1
            else
                capture_status=$?
            fi
            ((capture_status == 1)) || {
                printf 'could not verify tcpdump exit during teardown\n' >&2
                return 1
            }
            rm -f "$STATE/tcpdump.pid"
            return 0
        fi
        for _ in {1..100}; do
            if capture_is_running; then
                sleep 0.05
                continue
            else
                capture_status=$?
            fi
            ((capture_status == 1)) || {
                printf 'could not verify tcpdump exit during teardown\n' >&2
                return 1
            }
            rm -f "$STATE/tcpdump.pid"
            return 0
        done
    done
    printf 'tcpdump capture remains after bounded teardown\n' >&2
    return 1
}

capture_counts() {
    sudo -n tcpdump -nn -r "$STATE/outer.pcap" \
        'tcp[tcpflags] & tcp-syn != 0 and tcp[tcpflags] & tcp-ack = 0' \
        2>/dev/null \
        | awk '
            {
                for (field = 1; field <= NF; field++) {
                    if ($field == "IP" && field + 3 <= NF) {
                        source=$(field + 1)
                        destination=$(field + 3)
                        sub(/:$/, "", destination)
                        print source ">" destination
                        break
                    }
                }
            }
        ' | sort -u >"$STATE/outer-tuples.txt"
    http=$(awk -v source="$SOURCE_ADDRESS." \
        -v target="$PUBLIC_ADDRESS.$HTTP_PORT" '
            {
                split($0, tuple, ">")
                if (index(tuple[1], source) == 1 && tuple[2] == target) count++
            }
            END {print count+0}
        ' \
        "$STATE/outer-tuples.txt")
    https=$(awk -v source="$SOURCE_ADDRESS." \
        -v target="$PUBLIC_ADDRESS.$HTTPS_PORT" '
            {
                split($0, tuple, ">")
                if (index(tuple[1], source) == 1 && tuple[2] == target) count++
            }
            END {print count+0}
        ' \
        "$STATE/outer-tuples.txt")
    printf 'http=%s\nhttps=%s\ntotal=%s\n' "$http" "$https" \
        "$((http + https))"
}

verify_remote_topology_absent() {
    local containers networks name failed=0
    containers=$(docker container ls -a --format '{{.Names}}') || {
        printf 'could not inventory remote containers during teardown\n' >&2
        return 1
    }
    for name in "$PROXY_CONTAINER" "$PARENT_CONTAINER" "$ORIGIN_CONTAINER"; do
        if grep -Fxq -- "$name" <<<"$containers"; then
            printf 'remote container remains after teardown: %s\n' "$name" >&2
            failed=1
        fi
    done
    networks=$(docker network ls --format '{{.Name}}') || {
        printf 'could not inventory remote networks during teardown\n' >&2
        return 1
    }
    if grep -Fxq -- "$NETWORK" <<<"$networks"; then
        printf 'remote network remains after teardown: %s\n' "$NETWORK" >&2
        failed=1
    fi
    ((failed == 0))
}

stop_all() {
    stop_capture
    if [[ -f $STATE/runtime.env ]]; then
        load_state
        docker rm -f "$PROXY_CONTAINER" "$PARENT_CONTAINER" \
            "$ORIGIN_CONTAINER" >/dev/null 2>&1 || true
        docker network rm "$NETWORK" >/dev/null 2>&1 || true
        verify_remote_topology_absent || {
            printf 'remote topology teardown is unverified; retaining protective firewall rules\n' >&2
            return 1
        }
    fi
    remove_firewall
}

case "$ACTION" in
    start)
        (($# == 7)) || exit 2
        PUBLIC_ADDRESS=$1
        SOURCE_ADDRESS=$2
        CAPTURE_INTERFACE=$3
        HTTP_PORT=$4
        HTTPS_PORT=$5
        SUFFIX=$6
        REMOTE_IMAGE=$7
        [[ $SUFFIX =~ ^[0-9a-f]{12}$ ]] || exit 2
        mkdir -m 0700 -p "$STATE" "$STATE/captures" \
            "$STATE/proxy-logs" "$STATE/generated-certs"
        : >"$STATE/origin.jsonl"
        : >"$STATE/parent.jsonl"
        : >"$STATE/provider.jsonl"
        mkdir -m 0700 -p "$STATE/validate/generated-certs" \
            "$STATE/validate/captures" "$STATE/validate/proxy-logs"
        : >"$STATE/validate/provider.jsonl"
        NETWORK="aw-rt05-$SUFFIX"
        ORIGIN_CONTAINER="aw-rt05-origin-$SUFFIX"
        PARENT_CONTAINER="aw-rt05-parent-$SUFFIX"
        PROXY_CONTAINER="aw-rt05-proxy-$SUFFIX"
        TAG="aw-rt05-$SUFFIX"
        cat >"$STATE/runtime.env" <<EOF
PUBLIC_ADDRESS=$PUBLIC_ADDRESS
SOURCE_ADDRESS=$SOURCE_ADDRESS
CAPTURE_INTERFACE=$CAPTURE_INTERFACE
HTTP_PORT=$HTTP_PORT
HTTPS_PORT=$HTTPS_PORT
NETWORK=$NETWORK
ORIGIN_CONTAINER=$ORIGIN_CONTAINER
PARENT_CONTAINER=$PARENT_CONTAINER
PROXY_CONTAINER=$PROXY_CONTAINER
TAG=$TAG
EOF
        docker image inspect "$REMOTE_IMAGE" >/dev/null
        docker network create "$NETWORK" >/dev/null
        docker run -d --pull=never --user 0:0 --name "$ORIGIN_CONTAINER" \
            --security-opt label=disable \
            --network "$NETWORK" --network-alias origin.test \
            --volume "$BUNDLE/origin.py:/fixture/origin.py:ro,z" \
            --volume "$BUNDLE/origin-cert.pem:/fixture/origin-cert.pem:ro,z" \
            --volume "$BUNDLE/origin-key.pem:/fixture/origin-key.pem:ro,z" \
            --volume "$STATE/origin.jsonl:/state/origin.jsonl:rw,z" \
            "$REMOTE_IMAGE" python3 /fixture/origin.py >/dev/null
        docker run -d --pull=never --user 0:0 --name "$PARENT_CONTAINER" \
            --security-opt label=disable \
            --network "$NETWORK" --network-alias parent \
            --volume "$BUNDLE/parent.py:/fixture/parent.py:ro,z" \
            --volume "$STATE/parent.jsonl:/state/parent.jsonl:rw,z" \
            "$REMOTE_IMAGE" python3 /fixture/parent.py >/dev/null
        sudo -n iptables -w 5 -I INPUT 1 \
            -d "$PUBLIC_ADDRESS/32" -p tcp \
            -m multiport --dports "$HTTP_PORT,$HTTPS_PORT" \
            -m comment --comment "$TAG.deny" -j DROP
        sudo -n iptables -w 5 -I INPUT 1 \
            -s "$SOURCE_ADDRESS/32" -d "$PUBLIC_ADDRESS/32" -p tcp \
            -m multiport --dports "$HTTP_PORT,$HTTPS_PORT" \
            -m comment --comment "$TAG.allow" -j ACCEPT
        sudo -n iptables -w 5 -C INPUT \
            -s "$SOURCE_ADDRESS/32" -d "$PUBLIC_ADDRESS/32" -p tcp \
            -m multiport --dports "$HTTP_PORT,$HTTPS_PORT" \
            -m comment --comment "$TAG.allow" -j ACCEPT
        sudo -n iptables -w 5 -C INPUT \
            -d "$PUBLIC_ADDRESS/32" -p tcp \
            -m multiport --dports "$HTTP_PORT,$HTTPS_PORT" \
            -m comment --comment "$TAG.deny" -j DROP
        PROXY_INPUT_MOUNTS=(
            --volume "$BUNDLE/acl-proxy:/bundle/acl-proxy:ro"
            --volume "$BUNDLE/acl-proxy.toml:/bundle/acl-proxy.toml:ro"
            --volume "$BUNDLE/access-flow-server-chain.pem:/bundle/access-flow-server-chain.pem:ro"
            --volume "$BUNDLE/access-flow-server-key.pem:/bundle/access-flow-server-key.pem:ro"
            --volume "$BUNDLE/mitm-ca-cert.pem:/bundle/mitm-ca-cert.pem:ro"
            --volume "$BUNDLE/mitm-ca-key.pem:/bundle/mitm-ca-key.pem:ro"
            --volume "$BUNDLE/identity-token:/bundle/identity-token:ro"
            --volume "$BUNDLE/provider.py:/bundle/provider.py:ro"
            --volume "$BUNDLE/private-expected:/bundle/private-expected:ro"
            --volume "$BUNDLE/origin-root.pem:/bundle/origin-root.pem:ro"
        )
        docker run --rm --pull=never --user 0:0 \
            --security-opt label=disable \
            --network "$NETWORK" \
            "${PROXY_INPUT_MOUNTS[@]}" \
            --volume "$STATE/validate/generated-certs:/state/generated-certs:rw" \
            --volume "$STATE/validate/captures:/state/captures:rw" \
            --volume "$STATE/validate/proxy-logs:/state/proxy-logs:rw" \
            --volume "$STATE/validate/provider.jsonl:/state/provider.jsonl:rw" \
            --env HOME=/tmp \
            "$REMOTE_IMAGE" /bundle/acl-proxy \
            config validate --config /bundle/acl-proxy.toml \
            >"$STATE/config-validate.log" 2>&1
        docker run -d --pull=never --user 0:0 --name "$PROXY_CONTAINER" \
            --security-opt label=disable \
            --network "$NETWORK" \
            --publish "$PUBLIC_ADDRESS:$HTTP_PORT:$HTTP_PORT/tcp" \
            --publish "$PUBLIC_ADDRESS:$HTTPS_PORT:$HTTPS_PORT/tcp" \
            "${PROXY_INPUT_MOUNTS[@]}" \
            --volume "$STATE/generated-certs:/state/generated-certs:rw" \
            --volume "$STATE/captures:/state/captures:rw" \
            --volume "$STATE/proxy-logs:/state/proxy-logs:rw" \
            --volume "$STATE/provider.jsonl:/state/provider.jsonl:rw" \
            --env HOME=/tmp \
            "$REMOTE_IMAGE" /bundle/acl-proxy \
            run --config /bundle/acl-proxy.toml >/dev/null
        for _ in {1..200}; do
            if docker inspect "$PROXY_CONTAINER" --format '{{.State.Running}}' \
                    | grep -qx true \
                && docker exec -i "$PROXY_CONTAINER" python3 - <<'PY' 2>/dev/null
import urllib.request
with urllib.request.urlopen(
    "http://127.0.0.1:9080/_acl-proxy/ready", timeout=.2
) as response:
    assert response.read() == b'{"status":"ready"}'
PY
            then
                printf '%s\n' ready
                exit 0
            fi
            sleep 0.05
        done
        exit 1
        ;;
    probe-denied-source)
        (($# == 1)) || exit 2
        load_state
        denied_source=$1
        before=$(
            sudo -n iptables -w 5 -L INPUT -nvx \
                | awk -v tag="$TAG.deny" \
                    'index($0, tag) { print $1; exit }'
        )
        [[ $before =~ ^[0-9]+$ ]]
        python3 - "$denied_source" "$PUBLIC_ADDRESS" "$HTTP_PORT" <<'PY'
import socket
import sys

source, destination, port = sys.argv[1], sys.argv[2], int(sys.argv[3])
client = socket.socket()
client.settimeout(0.75)
client.bind((source, 0))
try:
    client.connect((destination, port))
except TimeoutError:
    pass
else:
    raise SystemExit("disallowed source reached the published listener")
finally:
    client.close()
PY
        after=
        for _ in {1..20}; do
            after=$(
                sudo -n iptables -w 5 -L INPUT -nvx \
                    | awk -v tag="$TAG.deny" \
                        'index($0, tag) { print $1; exit }'
            )
            [[ $after =~ ^[0-9]+$ && $after -gt $before ]] && break
            sleep 0.05
        done
        [[ $after =~ ^[0-9]+$ && $after -gt $before ]]
        printf '%s\n' denied
        ;;
    counts)
        python3 - "$STATE" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
for name in ("origin", "parent", "provider"):
    path = root / f"{name}.jsonl"
    count = len(path.read_text(encoding="utf-8").splitlines()) if path.exists() else 0
    print(f"{name}={count}")
captures = len(list((root / "captures").glob("*.json")))
print(f"captures={captures}")
PY
        ;;
    assert-marker-absent)
        (($# == 1)) || exit 2
        marker=$1
        ! grep -R -F -q -- "$marker" \
            "$STATE/origin.jsonl" "$STATE/parent.jsonl" \
            "$STATE/provider.jsonl" "$STATE/proxy-logs" "$STATE/captures"
        ;;
    capture-start)
        (($# == 0)) || exit 2
        load_state
        stop_capture
        rm -f "$STATE/outer.pcap" "$STATE/tcpdump.log"
        exec sudo -n sh -c '
            start_time=$(awk "{print \$22}" "/proc/$$/stat")
            printf "%s %s\n" "$$" "$start_time" >"$1"
            printf "capture-filter=source:%s http:%s https:%s\n" \
                "$3" "$4" "$5" >&2
            exec tcpdump --immediate-mode -i "$2" -nn -U \
                "host $3" \
                -w "$6"
        ' sh "$STATE/tcpdump.pid" "$CAPTURE_INTERFACE" "$SOURCE_ADDRESS" \
            "$HTTP_PORT" "$HTTPS_PORT" "$STATE/outer.pcap" \
            >"$STATE/tcpdump.log" 2>&1
        ;;
    capture-ready)
        (($# == 0)) || exit 2
        [[ -s $STATE/tcpdump.pid && -f $STATE/outer.pcap ]]
        read -r capture_pid capture_start_time <"$STATE/tcpdump.pid"
        [[ $capture_pid =~ ^[1-9][0-9]*$ \
            && $capture_start_time =~ ^[1-9][0-9]*$ ]]
        [[ -r /proc/$capture_pid/stat ]]
        [[ $(awk '{print $22}' "/proc/$capture_pid/stat") \
            == "$capture_start_time" ]]
        (( $(stat -c %s "$STATE/outer.pcap") > 24 ))
        printf '%s\n' ready
        ;;
    capture-counts)
        (($# == 0)) || exit 2
        load_state
        capture_counts
        ;;
    capture-stop)
        (($# == 0)) || exit 2
        load_state
        stop_capture
        capture_counts
        ;;
    export-state)
        load_state
        docker logs "$PROXY_CONTAINER" >"$STATE/proxy-container.stdout" \
            2>"$STATE/proxy-container.stderr"
        docker inspect "$PROXY_CONTAINER" >"$STATE/proxy-container-inspect.json"
        tar -C "$STATE" -cf - \
            origin.jsonl parent.jsonl provider.jsonl captures proxy-logs \
            outer-tuples.txt config-validate.log proxy-container.stdout \
            proxy-container.stderr proxy-container-inspect.json
        ;;
    executed-image-id)
        (($# == 0)) || exit 2
        load_state
        executed_image_id=
        for container in "$PROXY_CONTAINER" "$PARENT_CONTAINER" \
            "$ORIGIN_CONTAINER"; do
            container_image_id=$(docker inspect "$container" --format '{{.Image}}')
            case "$container_image_id" in
                [0-9a-f][0-9a-f]*) container_image_id="sha256:$container_image_id" ;;
            esac
            [[ $container_image_id =~ ^sha256:[0-9a-f]{64}$ ]] || {
                printf 'container has a noncanonical image ID: %s\n' \
                    "$container" >&2
                exit 1
            }
            if [[ -z $executed_image_id ]]; then
                executed_image_id=$container_image_id
            elif [[ $container_image_id != "$executed_image_id" ]]; then
                printf 'remote topology used different container images\n' >&2
                exit 1
            fi
        done
        printf '%s\n' "$executed_image_id"
        ;;
    diagnostics)
        if [[ -f $STATE/runtime.env ]]; then
            load_state
            for container in "$PROXY_CONTAINER" "$PARENT_CONTAINER" \
                "$ORIGIN_CONTAINER"; do
                printf '%s\n' "--- $container ---"
                docker logs --tail 100 "$container" 2>&1 || true
            done
        fi
        [[ ! -f $STATE/config-validate.log ]] \
            || cat "$STATE/config-validate.log"
        for log in "$STATE"/proxy-logs/*; do
            [[ ! -f $log ]] || tail -n 100 "$log"
        done
        for log in "$STATE"/origin.jsonl "$STATE"/parent.jsonl \
            "$STATE"/provider.jsonl; do
            [[ ! -f $log ]] || cat "$log"
        done
        [[ ! -f $STATE/tcpdump.log ]] || cat "$STATE/tcpdump.log"
        [[ ! -f $STATE/outer-tuples.txt ]] || cat "$STATE/outer-tuples.txt"
        ;;
    stop)
        stop_all
        ;;
    *)
        exit 2
        ;;
esac
REMOTE
chmod 0700 "$TMP_DIR/bundle/"*.py \
    "$TMP_DIR/bundle/remote-control.sh"

(
    cd "$TMP_DIR/bundle"
    find . -type f ! -name manifest.sha256 -print0 \
        | sort -z \
        | xargs -0 sha256sum >manifest.sha256
)
LOCAL_MANIFEST_SHA=$(sha256sum "$TMP_DIR/bundle/manifest.sha256" | awk '{print $1}')

REMOTE_DIR=$(
    ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" \
        mktemp -d "$REMOTE_HOME/.aw-tls-cross-host.XXXXXX"
) || fail "could not create the remote private temporary directory"
[[ $REMOTE_DIR == "$REMOTE_HOME"/.aw-tls-cross-host.* \
    && $REMOTE_DIR =~ ^[A-Za-z0-9._/-]+$ ]] \
    || fail "remote temporary directory did not match its private contract"
ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" \
    mkdir -m 0700 -- "$REMOTE_DIR/bundle" "$REMOTE_DIR/state"
tar -C "$TMP_DIR/bundle" -cf - . \
    | ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" \
        tar -C "$REMOTE_DIR/bundle" -xf - \
    || fail "could not transfer the exact remote fixture bundle"
REMOTE_MANIFEST_SHA=$(
    ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" \
        "cd '$REMOTE_DIR/bundle' && sha256sum -c manifest.sha256 >/dev/null && sha256sum manifest.sha256" \
        | awk '{print $1}'
) || fail "remote fixture manifest verification failed"
[[ $REMOTE_MANIFEST_SHA == "$LOCAL_MANIFEST_SHA" ]] \
    || fail "remote fixture manifest digest differs from local"
REMOTE_ACL_SHA=$(
    ssh "${SSH_OPTIONS[@]}" "$REMOTE_HOST" \
        sha256sum "$REMOTE_DIR/bundle/acl-proxy" | awk '{print $1}'
)
LOCAL_ACL_SHA=$(sha256sum "$ACL_PROXY_BIN" | awk '{print $1}')
[[ $REMOTE_ACL_SHA == "$LOCAL_ACL_SHA" ]] \
    || fail "remote ACL Proxy binary digest differs from local release artifact"

REMOTE_STARTED=1
remote_control_bounded 60s start "$REMOTE_ADDRESS" "$REMOTE_SOURCE_ADDRESS" \
    "$REMOTE_INTERFACE" "$TLS_HTTP_PORT" "$TLS_HTTPS_PORT" "$SUFFIX" \
    "$REMOTE_IMAGE" | grep -qx ready \
    || fail "remote Access Flow topology did not become ready"
REMOTE_EXECUTED_IMAGE_ID=$(remote_control_bounded 10s executed-image-id) \
    || fail "could not inventory the executed remote container image"
[[ $REMOTE_EXECUTED_IMAGE_ID == "$REMOTE_IMAGE_ID" ]] \
    || fail "executed remote container image differs from preflight image"
REMOTE_IMAGE_ID=$REMOTE_EXECUTED_IMAGE_ID
remote_control_bounded 10s probe-denied-source "$DENIED_SOURCE_ADDRESS" \
    | grep -qx denied \
    || fail "remote firewall did not reject and count the disallowed source"

docker network create "$NETWORK" >/dev/null
NETWORK_GATEWAY=$(docker network inspect "$NETWORK" \
    --format '{{(index .IPAM.Config 0).Gateway}}')
validate_ipv4 "$NETWORK_GATEWAY" \
    || fail "local workload network did not report an IPv4 gateway"

cat >"$TMP_DIR/container-agent.toml" <<EOF
schema_version = "1"

[container_agent]
enabled = true
control_socket = false

[container_agent.access_flow_relay]
setup_timeout = "3s"
drain_timeout = "5s"
max_connections = 64
copy_buffer_bytes_per_direction = 16384

[container_agent.access_flow_relay.presentation]
kind = "bearer_environment"
variable = "AW_IDENTITY_TOKEN"

[[container_agent.access_flow_relay.routes]]
name = "http"
listen = "127.0.0.1:3128"
allowed_destination_ports = [80]

[container_agent.access_flow_relay.routes.transport]
kind = "tls_tcp"
address = "$REMOTE_ADDRESS:$TLS_HTTP_PORT"
server_name = "proxy.access-flow.test"

[container_agent.access_flow_relay.routes.transport.trust]
kind = "pem_bundle"
path = "/run/aw-gateway/trust/access-flow-root.pem"

[[container_agent.access_flow_relay.routes]]
name = "https"
listen = "127.0.0.1:3129"
allowed_destination_ports = [443]

[container_agent.access_flow_relay.routes.transport]
kind = "tls_tcp"
address = "$REMOTE_ADDRESS:$TLS_HTTPS_PORT"
server_name = "proxy.access-flow.test"

[container_agent.access_flow_relay.routes.transport.trust]
kind = "pem_bundle"
path = "/run/aw-gateway/trust/access-flow-root.pem"
EOF
chmod 0600 "$TMP_DIR/container-agent.toml"

cat >"$TMP_DIR/workload-firewall.sh" <<EOF
#!/usr/bin/env bash
set -Eeuo pipefail
iptables -t nat -N AWRT05N
iptables -t nat -A AWRT05N -p tcp --dport 80 -j REDIRECT --to-ports 3128
iptables -t nat -A AWRT05N -p tcp --dport 443 -j REDIRECT --to-ports 3129
iptables -t nat -A AWRT05N -d 127.0.0.0/8 -j RETURN
iptables -t nat -A AWRT05N -j RETURN
iptables -t nat -I OUTPUT 1 -j AWRT05N
iptables -N AWRT05F
iptables -A AWRT05F -m conntrack --ctstate DNAT -j ACCEPT
iptables -A AWRT05F -d 127.0.0.0/8 -j ACCEPT
iptables -A AWRT05F -m owner --uid-owner 0 -d "$REMOTE_ADDRESS/32" \
    -p tcp -m multiport --dports "$TLS_HTTP_PORT,$TLS_HTTPS_PORT" -j ACCEPT
iptables -A AWRT05F -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
iptables -A AWRT05F -d "$NETWORK_GATEWAY/32" -p udp --dport 53 -j ACCEPT
iptables -A AWRT05F -d "$NETWORK_GATEWAY/32" -p tcp --dport 53 -j ACCEPT
iptables -A AWRT05F -j DROP
iptables -I OUTPUT 1 -j AWRT05F
ip6tables -N AWRT056
ip6tables -A AWRT056 -d ::1/128 -j ACCEPT
ip6tables -A AWRT056 -j DROP
ip6tables -I OUTPUT 1 -j AWRT056
touch /tmp/firewall.ready
while sleep 1; do
    iptables -t nat -C OUTPUT -j AWRT05N
    iptables -C OUTPUT -j AWRT05F
    ip6tables -C OUTPUT -j AWRT056
done
EOF
chmod 0700 "$TMP_DIR/workload-firewall.sh"

cat >"$TMP_DIR/workload.sh" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
cleanup() {
    set +e
    [[ -z ${FIREWALL_PID:-} ]] || kill -TERM "$FIREWALL_PID" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM
install -D -o 65534 -g 65534 -m 0400 \
    /mnt/aw-gateway/private-expected \
    /run/aw-gateway/private-value
/opt/aw-gateway/bin/workload-firewall \
    >/tmp/firewall.stdout 2>/tmp/firewall.stderr &
FIREWALL_PID=$!
for _ in {1..100}; do
    [[ -f /tmp/firewall.ready ]] && break
    kill -0 "$FIREWALL_PID"
    sleep 0.05
done
[[ -f /tmp/firewall.ready ]]
wait "$FIREWALL_PID"
SH
chmod 0700 "$TMP_DIR/workload.sh"

cat >"$TMP_DIR/agent-carrier.sh" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
cleanup() {
    set +e
    [[ -z ${AGENT_PID:-} ]] || kill -TERM "$AGENT_PID" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM
install -D -o 0 -g 0 -m 0644 \
    /mnt/aw-gateway/access-flow-root.pem \
    /run/aw-gateway/trust/access-flow-root.pem
IFS= read -r AW_IDENTITY_TOKEN </run/aw-gateway/identity-token.fifo
export AW_IDENTITY_TOKEN
/opt/aw-gateway/bin/aw-container-agent \
    --config /etc/aw-gateway/container-agent.toml run \
    >/tmp/agent.stdout 2>/tmp/agent.stderr &
AGENT_PID=$!
unset AW_IDENTITY_TOKEN
printf '%s\n' "$AGENT_PID" >/tmp/agent.pid
wait "$AGENT_PID"
SH
chmod 0700 "$TMP_DIR/agent-carrier.sh"

start_workload() {
    local bearer=$1 label=$2 fifo
    fifo="$TMP_DIR/$label.fifo"
    WORKLOAD_CONTAINER="aw-tls-cross-$label-$SUFFIX"
    AGENT_CONTAINER="aw-tls-cross-agent-$label-$SUFFIX"
    mkfifo -m 0600 "$fifo"
    docker run -d --name "$WORKLOAD_CONTAINER" --privileged --network "$NETWORK" \
        --ulimit nofile=4096:4096 \
        --mount "type=bind,src=$TMP_DIR/bundle/mitm-ca-cert.pem,dst=/etc/acl-proxy/mitm-ca-cert.pem,readonly" \
        --mount "type=bind,src=$TMP_DIR/bundle/private-expected,dst=/mnt/aw-gateway/private-expected,readonly" \
        --mount "type=bind,src=$TMP_DIR/workload-firewall.sh,dst=/opt/aw-gateway/bin/workload-firewall,readonly" \
        --mount "type=bind,src=$TMP_DIR/workload.sh,dst=/usr/local/bin/workload-smoke,readonly" \
        "$WORKLOAD_IMAGE" bash /usr/local/bin/workload-smoke >/dev/null
    for _ in {1..100}; do
        docker inspect "$WORKLOAD_CONTAINER" --format '{{.State.Running}}' \
            | grep -qx true \
            || fail "workload exited before firewall readiness"
        docker exec "$WORKLOAD_CONTAINER" test -f /tmp/firewall.ready \
            2>/dev/null && break
        sleep 0.05
    done
    docker exec "$WORKLOAD_CONTAINER" test -f /tmp/firewall.ready \
        || fail "workload firewall did not become ready"
    docker run -d --pull=never --name "$AGENT_CONTAINER" \
        --network "container:$WORKLOAD_CONTAINER" --user 0:0 \
        --ulimit nofile=4096:4096 \
        --mount "type=bind,src=$AGENT_BIN,dst=/opt/aw-gateway/bin/aw-container-agent,readonly" \
        --mount "type=bind,src=$TMP_DIR/container-agent.toml,dst=/etc/aw-gateway/container-agent.toml,readonly" \
        --mount "type=bind,src=$TMP_DIR/bundle/access-flow-root.pem,dst=/mnt/aw-gateway/access-flow-root.pem,readonly" \
        --mount "type=bind,src=$TMP_DIR/agent-carrier.sh,dst=/usr/local/bin/agent-carrier,readonly" \
        --mount "type=bind,src=$fifo,dst=/run/aw-gateway/identity-token.fifo" \
        "$WORKLOAD_AGENT_IMAGE" bash /usr/local/bin/agent-carrier >/dev/null
    local executed_agent_image_id
    executed_agent_image_id=$(
        docker inspect "$AGENT_CONTAINER" --format '{{.Image}}'
    )
    [[ $executed_agent_image_id == "$WORKLOAD_AGENT_IMAGE_ID" ]] \
        || fail "executed agent carrier image differs from its preflight image"
    WORKLOAD_AGENT_IMAGE_ID=$executed_agent_image_id
    (printf '%s\n' "$bearer" >"$fifo") &
    IDENTITY_WRITER_PID=$!
    for _ in {1..100}; do
        kill -0 "$IDENTITY_WRITER_PID" 2>/dev/null || break
        sleep 0.05
    done
    kill -0 "$IDENTITY_WRITER_PID" 2>/dev/null \
        && fail "workload did not consume its one-shot bearer"
    wait "$IDENTITY_WRITER_PID" \
        || fail "one-shot bearer delivery failed"
    IDENTITY_WRITER_PID=
    for _ in {1..200}; do
        docker inspect "$WORKLOAD_CONTAINER" --format '{{.State.Running}}' \
            | grep -qx true \
            || fail "workload exited before relay readiness"
        docker inspect "$AGENT_CONTAINER" --format '{{.State.Running}}' \
            | grep -qx true \
            || fail "agent carrier exited before relay readiness"
        if timeout 0.2 docker exec "$WORKLOAD_CONTAINER" \
            bash -c 'exec 3<>/dev/tcp/127.0.0.1/3128' >/dev/null 2>&1 \
            && timeout 0.2 docker exec "$WORKLOAD_CONTAINER" \
                bash -c 'exec 3<>/dev/tcp/127.0.0.1/3129' >/dev/null 2>&1; then
            return
        fi
        sleep 0.05
    done
    fail "workload relay did not become ready"
}

stop_workload() {
    local agent_container=$AGENT_CONTAINER workload_container=$WORKLOAD_CONTAINER
    docker rm -f "$agent_container" >/dev/null
    if docker inspect "$agent_container" >/dev/null 2>&1; then
        fail "agent carrier container remained after teardown"
    fi
    AGENT_CONTAINER=
    docker rm -f "$WORKLOAD_CONTAINER" >/dev/null
    if docker inspect "$workload_container" >/dev/null 2>&1; then
        fail "workload container remained after teardown"
    fi
    WORKLOAD_CONTAINER=
}

capture_local_evidence() {
    local label=$1 destination
    destination="$TMP_DIR/local-evidence/$label"
    mkdir -m 0700 "$destination"
    docker inspect "$WORKLOAD_CONTAINER" \
        >"$destination/workload-container-inspect.json"
    docker logs "$WORKLOAD_CONTAINER" >"$destination/workload-container.stdout" \
        2>"$destination/workload-container.stderr"
    docker inspect "$AGENT_CONTAINER" \
        >"$destination/agent-carrier-inspect.json"
    docker logs "$AGENT_CONTAINER" >"$destination/agent-carrier.stdout" \
        2>"$destination/agent-carrier.stderr"
    for path in firewall.stdout firewall.stderr; do
        docker cp "$WORKLOAD_CONTAINER:/tmp/$path" "$destination/$path"
    done
    for path in agent.stdout agent.stderr; do
        docker cp "$AGENT_CONTAINER:/tmp/$path" "$destination/$path"
    done
}

request_http() {
    docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
        --fail --silent --show-error --connect-timeout 3 --max-time 12 \
        --noproxy '*' --resolve 'origin.test:80:198.18.0.10' "$@"
}

request_https() {
    docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
        --fail --silent --show-error --connect-timeout 3 --max-time 12 \
        --cacert /etc/acl-proxy/mitm-ca-cert.pem --noproxy '*' \
        --resolve 'origin.test:443:198.18.0.10' "$@"
}

INVALID_COUNTS_BEFORE=$(remote_control_bounded 10s counts)
start_workload "$INVALID_BEARER" invalid
if request_http http://origin.test/invalid-bearer-marker >/dev/null 2>&1; then
    fail "an invalid bearer reached the HTTP application pipeline"
fi
INVALID_COUNTS_AFTER=$(remote_control_bounded 10s counts)
[[ $INVALID_COUNTS_AFTER == "$INVALID_COUNTS_BEFORE" ]] \
    || fail "invalid bearer changed origin, parent, provider, or capture counters"
remote_control_bounded 10s assert-marker-absent invalid-bearer-marker \
    || fail "invalid bearer application bytes reached an observable sink"
capture_local_evidence invalid
stop_workload

start_workload "$VALID_BEARER" valid
capture_local_evidence valid-start
python3 - "$TMP_DIR/local-evidence/valid-start/workload-container-inspect.json" \
    "$TMP_DIR/local-evidence/valid-start/agent-carrier-inspect.json" \
    "$BEARER_HISTORY" <<'PY'
import pathlib
import sys

data = b"".join(pathlib.Path(path).read_bytes() for path in sys.argv[1:3])
bearer = pathlib.Path(sys.argv[3]).read_bytes().splitlines()[0]
if bearer in data:
    raise SystemExit("Docker launch metadata retained the bearer")
PY
docker exec "$WORKLOAD_CONTAINER" sh -eu -c '
    test "${AW_IDENTITY_TOKEN+x}" != x
' || fail "workload launcher retained the bearer"
docker exec "$AGENT_CONTAINER" sh -eu -c '
    test "${AW_IDENTITY_TOKEN+x}" != x
    kill -0 "$(cat /tmp/agent.pid)"
' || fail "agent carrier launcher retained the bearer or agent exited"

remote_control capture-start &
CAPTURE_CONTROLLER_PID=$!
CAPTURE_READY=0
for _ in {1..20}; do
    if ssh_bounded 5s true \
        && remote_control_bounded 5s capture-ready | grep -qx ready; then
        CAPTURE_READY=1
        break
    fi
    sleep 0.05
done
((CAPTURE_READY == 1)) || fail "remote packet capture did not become ready"
principal_body=$(request_http http://origin.test/principal) \
    || fail "principal policy HTTP flow failed"
[[ $principal_body == \
    'origin:/principal:identity=absent:private=absent' ]] \
    || fail "principal policy HTTP flow returned an unexpected response"
group_body=$(request_http http://origin.test/group) \
    || fail "group policy HTTP flow failed"
[[ $group_body == 'origin:/group:identity=absent:private=absent' ]] \
    || fail "group policy HTTP flow returned an unexpected response"
delegate_body=$(
    docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" sh -eu -c '
        private=$(cat /run/aw-gateway/private-value)
        exec curl --fail --silent --show-error --connect-timeout 3 --max-time 12 \
            --noproxy "*" --resolve "origin.test:80:198.18.0.10" \
            -H "x-rt05-private: $private" http://origin.test/delegate
    '
) || fail "delegate/private-field HTTP flow failed"
[[ $delegate_body == 'origin:/delegate:identity=absent:private=absent' ]] \
    || fail "delegate/private-field HTTP flow returned an unexpected response"
https_body=$(request_https https://origin.test/secure) \
    || fail "nested HTTPS flow failed"
[[ $https_body == 'origin:/secure:identity=absent:private=absent' ]] \
    || fail "nested HTTPS flow returned an unexpected response"
keepalive_connects=$(docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --fail --silent --show-error --connect-timeout 3 --max-time 12 \
    --write-out '%{num_connects}\n' --noproxy '*' \
    --resolve 'origin.test:80:198.18.0.10' \
    --output /dev/null http://origin.test/keepalive-one \
    --output /dev/null http://origin.test/keepalive-two) \
    || fail "keepalive HTTP flow failed"
KEEPALIVE_CONNECTIONS=$(awk '{sum += $1} END {print sum + 0}' \
    <<<"$keepalive_connects")
[[ $KEEPALIVE_CONNECTIONS == 1 ]] \
    || fail "two keepalive requests used $KEEPALIVE_CONNECTIONS workload flows"
CAPTURE_DRAINED=0
for _ in {1..100}; do
    OUTER_COUNTS=$(remote_control_bounded 5s capture-counts 2>/dev/null) || true
    HTTP_OUTER_CONNECTIONS=$(
        awk -F= '$1 == "http" {print $2}' <<<"$OUTER_COUNTS"
    )
    HTTPS_OUTER_CONNECTIONS=$(
        awk -F= '$1 == "https" {print $2}' <<<"$OUTER_COUNTS"
    )
    TOTAL_OUTER_CONNECTIONS=$(
        awk -F= '$1 == "total" {print $2}' <<<"$OUTER_COUNTS"
    )
    if [[ $HTTP_OUTER_CONNECTIONS == 4 \
        && $HTTPS_OUTER_CONNECTIONS == 1 \
        && $TOTAL_OUTER_CONNECTIONS == 5 ]]; then
        CAPTURE_DRAINED=1
        break
    fi
    sleep 0.05
done
((CAPTURE_DRAINED == 1)) \
    || fail "remote packet capture did not drain the five expected outer connections"
OUTER_COUNTS=$(remote_control_bounded 15s capture-stop)
wait_capture_controller \
    || fail "attached packet-capture controller did not stop cleanly"
HTTP_OUTER_CONNECTIONS=$(awk -F= '$1 == "http" {print $2}' <<<"$OUTER_COUNTS")
HTTPS_OUTER_CONNECTIONS=$(awk -F= '$1 == "https" {print $2}' <<<"$OUTER_COUNTS")
TOTAL_OUTER_CONNECTIONS=$(awk -F= '$1 == "total" {print $2}' <<<"$OUTER_COUNTS")
[[ $HTTP_OUTER_CONNECTIONS == 4 ]] \
    || fail "four HTTP workload flows did not use four outer connections; observed: $OUTER_COUNTS"
[[ $HTTPS_OUTER_CONNECTIONS == 1 ]] \
    || fail "one HTTPS workload flow did not use one outer connection; observed: $OUTER_COUNTS"
[[ $TOTAL_OUTER_CONNECTIONS == 5 ]] \
    || fail "five workload flows did not use five outer connections; observed: $OUTER_COUNTS"

if docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --fail --silent --connect-timeout 1 --max-time 2 --noproxy '*' \
    http://198.18.0.10:8080/escape >/dev/null 2>&1; then
    fail "non-protected workload egress bypassed the fail-closed firewall"
fi
docker exec "$AGENT_CONTAINER" kill -0 "$(docker exec "$AGENT_CONTAINER" \
    cat /tmp/agent.pid)" || fail "relay exited after functional flows"
capture_local_evidence valid-final

remote_control_bounded 30s export-state \
    | tar -C "$TMP_DIR/remote-state" -xf - \
    || fail "could not retrieve sanitized remote evidence"

python3 - "$TMP_DIR/remote-state" "$REMOTE_SOURCE_ADDRESS" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
physical_source = sys.argv[2]
provider = [
    json.loads(line)
    for line in (root / "provider.jsonl").read_text(encoding="utf-8").splitlines()
]
if provider != [{
    "ruleId": "delegate",
    "clientIp": "0.0.0.0",
    "identityValid": True,
    "privateValid": True,
}]:
    raise SystemExit(f"provider parity evidence was incorrect: {provider!r}")

parents = [
    json.loads(line)
    for line in (root / "parent.jsonl").read_text(encoding="utf-8").splitlines()
]
if any(item.get("identity") == "present" or item.get("private") == "present"
       for item in parents):
    raise SystemExit("parent proxy received an identity or private carrier")
connects = [item for item in parents if item.get("event") == "connect"]
if [item.get("target") for item in connects] != ["origin.test:443"]:
    raise SystemExit(f"nested HTTPS did not produce one parent CONNECT: {connects!r}")

origins = [
    json.loads(line)
    for line in (root / "origin.jsonl").read_text(encoding="utf-8").splitlines()
]
if any(item.get("identity") != "absent" or item.get("private") != "absent"
       for item in origins):
    raise SystemExit("origin received an identity or private carrier")

captures = [
    json.loads(path.read_text(encoding="utf-8"))
    for path in (root / "captures").glob("*.json")
]
delegate = [
    item for item in captures
    if item.get("url") == "http://origin.test/delegate"
]
if len(delegate) != 1:
    raise SystemExit(f"expected one delegate capture, found {len(delegate)}")
if delegate[0].get("client") != {"address": "0.0.0.0", "port": 0}:
    raise SystemExit(f"capture client sentinel was incorrect: {delegate[0]!r}")

logs = b"".join(
    path.read_bytes() for path in (root / "proxy-logs").glob("*")
    if path.is_file()
)
if b"client_ip=0.0.0.0" not in logs:
    raise SystemExit("policy log lacks the fixed remote client sentinel")
if physical_source.encode() in logs:
    raise SystemExit("policy/product logs exposed the physical remote source")
PY

python3 - "$TMP_DIR/remote-state" "$BEARER_HISTORY" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
secrets = [
    value for value in pathlib.Path(sys.argv[2]).read_bytes().splitlines()
    if value
]
for path in root.rglob("*"):
    if path.is_file():
        data = path.read_bytes()
        if any(secret in data for secret in secrets):
            raise SystemExit("sanitized remote evidence contains a bearer/private value")
PY

python3 - "$TMP_DIR/local-evidence" "$BEARER_HISTORY" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
secrets = [
    value for value in pathlib.Path(sys.argv[2]).read_bytes().splitlines()
    if value
]
for path in root.rglob("*"):
    if path.is_file():
        data = path.read_bytes()
        if any(secret in data for secret in secrets):
            raise SystemExit("local observable evidence contains a bearer/private value")
PY

stop_workload
docker network rm "$NETWORK" >/dev/null \
    || fail "local workload network cleanup failed"
if docker network inspect "$NETWORK" >/dev/null 2>&1; then
    fail "local workload network remained after teardown"
fi
NETWORK=
stop_remote || fail "remote topology cleanup did not complete"

printf '%s\n' \
    'transport=tls_tcp' \
    'topology=real-second-linux-host' \
    'relay-consumer=integrated-agent' \
    'http-flow=passed' \
    'nested-https-flow=passed' \
    'principal-policy=passed' \
    'group-policy=passed' \
    'delegate-policy=passed' \
    'private-fields=passed' \
    'bearer-negative-ordering=passed' \
    'provider-client-sentinel=0.0.0.0' \
    'capture-client-sentinel=0.0.0.0:0' \
    'policy-log-client-sentinel=0.0.0.0' \
    'one-connection-per-flow=passed' \
    'fallback=none' \
    'remote-firewall=passed' \
    'remote-firewall-negative-source=passed' \
    'remote-firewall-cleanup=passed' \
    'local-topology-cleanup=passed' \
    'observable-secret-scan=passed' \
    "remote-host=$REMOTE_HOST" \
    "remote-address=$REMOTE_ADDRESS" \
    "remote-interface=$REMOTE_INTERFACE" \
    "remote-capture-interface=$REMOTE_INTERFACE" \
    "remote-allowed-source-address=$REMOTE_SOURCE_ADDRESS" \
    "remote-denied-source-address=$DENIED_SOURCE_ADDRESS" \
    "remote-container-backend=$REMOTE_CONTAINER_BACKEND" \
    'remote-container-security=selinux-label-disabled' \
    "local-machine-id-sha256=$LOCAL_MACHINE_ID_SHA256" \
    "remote-machine-id-sha256=$REMOTE_MACHINE_ID_SHA256" \
    "workload-base-image=$WORKLOAD_BASE_IMAGE" \
    "workload-base-image-id=$WORKLOAD_BASE_IMAGE_ID" \
    'workload-image-tools=bash,curl,ip,iptables,nsenter' \
    "workload-image-package-manifest-sha256=$WORKLOAD_IMAGE_PACKAGE_MANIFEST_SHA256" \
    'workload-image-material=cached-direct' \
    "workload-agent-image=$WORKLOAD_AGENT_IMAGE" \
    "workload-agent-image-id=$WORKLOAD_AGENT_IMAGE_ID" \
    "remote-image=$REMOTE_IMAGE" \
    "remote-image-id=$REMOTE_IMAGE_ID" \
    "acl-repository-sha=$EXPECTED_ACL_SHA" \
    "access-runtime-repository-sha=$EXPECTED_ACCESS_RUNTIME_SHA" \
    "aw-repository-sha=$EXPECTED_AW_SHA" \
    "acl-proxy-sha256=$LOCAL_ACL_SHA" \
    "aw-container-agent-sha256=$(sha256sum "$AGENT_BIN" | awk '{print $1}')" \
    "remote-bundle-manifest-sha256=$LOCAL_MANIFEST_SHA" \
    "workload-image-id=$WORKLOAD_IMAGE_ID"
printf '%s\n' 'tls-access-flow-cross-host-smoke=passed'
SUCCESS=1
