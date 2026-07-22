#!/usr/bin/env bash

set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
FIREWALL="$ROOT/assets/aw-transparent-uds-firewall"
RELAY_CONFIG="$ROOT/examples/apple-container/transparent-uds-relay.json"
BASE_IMAGE=${AW_UDS_STACK_SMOKE_IMAGE:-ubuntu:24.04}

ACL_PROXY_BIN=
RELAY_BIN=
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
ACTIVE_CLIENT_PID=
SUCCESS=0

fail() {
    printf 'transparent UDS stack smoke failed: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat >&2 <<'EOF'
Usage: run-transparent-uds-stack-smoke.sh \
  --acl-proxy-bin <absolute-path> \
  --relay-bin <absolute-path> \
  --acl-repo <absolute-path> \
  --access-runtime-repo <absolute-path> \
  --aw-repo <absolute-path> \
  --expected-acl-sha <full-sha> \
  --expected-access-runtime-sha <full-sha> \
  --expected-aw-sha <full-sha>
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
                tail -n 80 "$log" >&2
            done
        fi
        for container in "$WORKLOAD_CONTAINER" "$PARENT_CONTAINER" "$ORIGIN_CONTAINER"; do
            [[ -n $container ]] || continue
            docker logs --tail 80 "$container" >&2 2>/dev/null || true
        done
    fi
    if (( status != 0 )) && [[ ${AW_UDS_STACK_SMOKE_KEEP_FAILED:-0} == 1 ]]; then
        printf 'preserving failed smoke resources: temp=%s network=%s workload=%s parent=%s origin=%s\n' \
            "$TMP_DIR" "$NETWORK" "$WORKLOAD_CONTAINER" "$PARENT_CONTAINER" "$ORIGIN_CONTAINER" >&2
        exit "$status"
    fi
    if [[ -n $ACL_PID ]] && kill -0 "$ACL_PID" 2>/dev/null; then
        kill -TERM "$ACL_PID" 2>/dev/null || true
        for _ in {1..100}; do
            kill -0 "$ACL_PID" 2>/dev/null || break
            sleep 0.05
        done
        kill -KILL "$ACL_PID" 2>/dev/null || true
        wait "$ACL_PID" 2>/dev/null || true
    fi
    if [[ -n $ACTIVE_CLIENT_PID ]] && kill -0 "$ACTIVE_CLIENT_PID" 2>/dev/null; then
        kill -TERM "$ACTIVE_CLIENT_PID" 2>/dev/null || true
        wait "$ACTIVE_CLIENT_PID" 2>/dev/null || true
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
            ACL_PROXY_BIN=$2
            shift 2
            ;;
        --relay-bin)
            (($# >= 2)) || usage
            RELAY_BIN=$2
            shift 2
            ;;
        --acl-repo)
            (($# >= 2)) || usage
            ACL_REPO=$2
            shift 2
            ;;
        --access-runtime-repo)
            (($# >= 2)) || usage
            ACCESS_RUNTIME_REPO=$2
            shift 2
            ;;
        --aw-repo)
            (($# >= 2)) || usage
            AW_REPO=$2
            shift 2
            ;;
        --expected-acl-sha)
            (($# >= 2)) || usage
            EXPECTED_ACL_SHA=$2
            shift 2
            ;;
        --expected-access-runtime-sha)
            (($# >= 2)) || usage
            EXPECTED_ACCESS_RUNTIME_SHA=$2
            shift 2
            ;;
        --expected-aw-sha)
            (($# >= 2)) || usage
            EXPECTED_AW_SHA=$2
            shift 2
            ;;
        *) usage ;;
    esac
done

[[ -n $ACL_PROXY_BIN && -n $RELAY_BIN && -n $ACL_REPO \
    && -n $ACCESS_RUNTIME_REPO && -n $AW_REPO && -n $EXPECTED_ACL_SHA \
    && -n $EXPECTED_ACCESS_RUNTIME_SHA && -n $EXPECTED_AW_SHA ]] || usage

for command in awk cargo curl docker git openssl python3 sha256sum stat timeout; do
    require_command "$command"
done

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
[[ -z $(git -C "$ACL_REPO" status --porcelain --untracked-files=all) ]] \
    || fail "acl-repo must be clean for provenance-bound release builds"
[[ -z $(git -C "$ACCESS_RUNTIME_REPO" status --porcelain --untracked-files=all) ]] \
    || fail "access-runtime-repo must be clean for provenance-bound release builds"
[[ -z $(git -C "$AW_REPO" status --porcelain --untracked-files=all) ]] \
    || fail "aw-repo must be clean for provenance-bound smoke evidence"

EXPECTED_ACL_PROXY_BIN="$ACL_REPO/target/release/acl-proxy"
EXPECTED_RELAY_BIN="$ACL_REPO/target/release/acl-proxy-transparent-uds-relay"
[[ $(readlink -m -- "$ACL_PROXY_BIN") == "$EXPECTED_ACL_PROXY_BIN" ]] \
    || fail "acl-proxy-bin must be $EXPECTED_ACL_PROXY_BIN"
[[ $(readlink -m -- "$RELAY_BIN") == "$EXPECTED_RELAY_BIN" ]] \
    || fail "relay-bin must be $EXPECTED_RELAY_BIN"
cargo build --quiet --locked --release --manifest-path "$ACL_REPO/Cargo.toml" \
    --bin acl-proxy \
    || fail "locked ACL Proxy release build failed"
cargo build --quiet --locked --release --manifest-path "$ACL_REPO/Cargo.toml" \
    --package acl-proxy-transparent-uds-relay \
    --bin acl-proxy-transparent-uds-relay \
    || fail "locked ACL Proxy release build failed"
require_absolute_file acl-proxy "$EXPECTED_ACL_PROXY_BIN"
require_absolute_file relay "$EXPECTED_RELAY_BIN"
ACL_PROXY_BIN=$EXPECTED_ACL_PROXY_BIN
RELAY_BIN=$EXPECTED_RELAY_BIN
[[ -x $FIREWALL && ! -L $FIREWALL ]] || fail "firewall asset is missing or not executable"
[[ -f $RELAY_CONFIG && ! -L $RELAY_CONFIG ]] \
    || fail "checked-in relay config is missing: $RELAY_CONFIG"

docker info >/dev/null 2>&1 || fail "Docker daemon is unavailable"
docker image inspect "$BASE_IMAGE" >/dev/null 2>&1 \
    || fail "required local image is unavailable (refusing an implicit pull): $BASE_IMAGE"
docker image inspect python:3.12-slim >/dev/null 2>&1 \
    || fail "required local image is unavailable (refusing an implicit pull): python:3.12-slim"

umask 077
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/aw-transparent-uds-stack.XXXXXX")
mkdir -m 0700 "$TMP_DIR/home" "$TMP_DIR/socket-runtime" "$TMP_DIR/config" \
    "$TMP_DIR/logs" "$TMP_DIR/parent-logs"
touch "$TMP_DIR/parent-logs/events.jsonl"
chmod 0666 "$TMP_DIR/parent-logs/events.jsonl"

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
        ca-certificates curl iproute2 iptables util-linux \
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
import ssl
import sys
import threading
import time


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        identity = self.headers.get("x-aw-identity-token", "absent")
        if self.path in ("/stream", "/active-stream"):
            chunk = b"0123456789abcdef" * 1024
            chunk_count = 64 if self.path == "/stream" else 512
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(chunk) * chunk_count))
            self.end_headers()
            for _ in range(chunk_count):
                self.wfile.write(chunk)
                self.wfile.flush()
                time.sleep(0.02)
            return
        body = f"origin:{self.path}:identity={identity}".encode("ascii")
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
HTTP_SOCKET="$TMP_DIR/socket-runtime/transparent-http.sock"
HTTPS_SOCKET="$TMP_DIR/socket-runtime/transparent-https.sock"
ACL_CONFIG="$TMP_DIR/config/acl-proxy.toml"
cat >"$ACL_CONFIG" <<EOF
schema_version = 3

[service]
request_timeout = "30s"
shutdown_drain_timeout = "2s"

[control]
bind = "127.0.0.1:$CONTROL_PORT"
base_path = "/_acl-proxy"
request_body_timeout = "2s"

[listeners.transparent_http]
endpoint.kind = "unix_proxy_v2"
endpoint.path = "$HTTP_SOCKET"
endpoint.mode = "0606"
proxy_header_timeout = "1s"
http_versions = ["http1"]
max_connections = 64

[listeners.transparent_https]
endpoint.kind = "unix_proxy_v2"
endpoint.path = "$HTTPS_SOCKET"
endpoint.mode = "0606"
proxy_header_timeout = "1s"
http_versions = ["http1"]
max_connections = 64

[mitm]
mode = "files"
ca_certificate = "$TMP_DIR/config/mitm-ca-cert.pem"
ca_private_key = "$TMP_DIR/config/mitm-ca-key.pem"
directory = "$TMP_DIR/config/generated-certs"

[egress]
route = "parent_proxy"

[egress.parent_proxy]
kind = "generic"
url = "http://$PARENT_IP:8888"

[egress.origin_tls]
trust = "custom"
ca_certificate = "$TMP_DIR/config/origin-ca-cert.pem"
http_versions = ["http1"]

[[egress.credential_actions]]
kind = "custom_header"
transport_security = "cleartext_allowed"
header = "x-aw-identity-token"
credential = "workstation_identity"

[policy]
default = "deny"

[[policy.rules]]
id = "deny-fixture"
decision = "deny"
urls = ["http://origin.test/denied"]

[[policy.rules]]
id = "allow-fixture"
decision = "allow"
urls = ["http://origin.test/**", "https://origin.test/**"]

[credentials]
allowed_custom_headers = ["x-aw-identity-token"]

[credentials.sources.workstation_identity]
source = "environment"
variable = "AW_IDENTITY_TOKEN"

[observation.logging]
level = "info"
directory = "$TMP_DIR/logs"
max_bytes = 1048576
max_files = 2
console = false

[safety.loop_detection]
inject = true
header = "x-acl-proxy-request-id"
EOF

env -i HOME="$TMP_DIR/home" PATH=/usr/bin:/bin AW_IDENTITY_TOKEN=host-only-identity \
    "$ACL_PROXY_BIN" config validate --config "$ACL_CONFIG" \
    >"$TMP_DIR/acl-proxy.validate" 2>&1 \
    || fail "ephemeral ACL Proxy configuration did not validate"
start_acl_proxy() {
    local generation=$1
    env -i HOME="$TMP_DIR/home" PATH=/usr/bin:/bin AW_IDENTITY_TOKEN=host-only-identity \
        "$ACL_PROXY_BIN" run --config "$ACL_CONFIG" \
        >"$TMP_DIR/acl-proxy.$generation.stdout" \
        2>"$TMP_DIR/acl-proxy.$generation.stderr" &
    ACL_PID=$!
    : >"$TMP_DIR/acl-proxy.stderr"
    for _ in {1..200}; do
        kill -0 "$ACL_PID" 2>/dev/null || fail "ACL Proxy exited before readiness"
        if [[ -S $HTTP_SOCKET && -S $HTTPS_SOCKET ]] \
            && curl --fail --silent --noproxy '*' \
                "http://127.0.0.1:$CONTROL_PORT/_acl-proxy/ready" \
                | grep -qx '{"status":"ready"}'; then
            return
        fi
        sleep 0.05
    done
    fail "ACL Proxy did not publish both traffic sockets"
}

stop_acl_proxy() {
    kill -TERM "$ACL_PID"
    for _ in {1..100}; do
        kill -0 "$ACL_PID" 2>/dev/null || break
        sleep 0.05
    done
    kill -0 "$ACL_PID" 2>/dev/null \
        && fail "ACL Proxy did not stop within the shutdown bound"
    wait "$ACL_PID" || fail "ACL Proxy exited unsuccessfully"
    ACL_PID=
}

start_acl_proxy first

cp -- "$RELAY_CONFIG" "$TMP_DIR/relay.json"
chmod 0644 "$TMP_DIR/relay.json"

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
    [[ -z ${FIREWALL_PID:-} ]] || kill "$FIREWALL_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    exit "$status"
}
trap cleanup EXIT INT TERM
/opt/aw-gateway/bin/aw-transparent-uds-firewall watch \
    --dns-server "$SMOKE_DNS" --http-port 3128 --https-port 3129 &
FIREWALL_PID=$!
for _ in {1..100}; do
    [[ -f /run/aw-gateway/transparent-uds-firewall.generation.ready ]] && break
    kill -0 "$FIREWALL_PID"
    sleep 0.05
done
[[ -f /run/aw-gateway/transparent-uds-firewall.generation.ready ]]
/opt/aw-gateway/bin/acl-proxy-transparent-uds-relay \
    --config /etc/acl-proxy/transparent-uds-relay.json \
    >/tmp/relay.stdout 2>/tmp/relay.stderr &
RELAY_PID=$!
for _ in {1..100}; do
    if timeout 0.2 bash -c 'exec 3<>/dev/tcp/127.0.0.1/3128' \
        && timeout 0.2 bash -c 'exec 3<>/dev/tcp/127.0.0.1/3129'; then
        touch /tmp/stack.ready
        break
    fi
    kill -0 "$RELAY_PID"
    sleep 0.05
done
[[ -f /tmp/stack.ready ]]
wait "$RELAY_PID"
SH
chmod 0700 "$TMP_DIR/workload.sh"

start_workload() {
    docker run -d --name "$WORKLOAD_CONTAINER" --privileged --network "$NETWORK" \
        --env "SMOKE_DNS=$NETWORK_GATEWAY" \
        --mount "type=bind,src=$HTTP_SOCKET,dst=/run/acl-proxy/transparent-http.sock" \
        --mount "type=bind,src=$HTTPS_SOCKET,dst=/run/acl-proxy/transparent-https.sock" \
        --mount "type=bind,src=$TMP_DIR/config/mitm-ca-cert.pem,dst=/etc/acl-proxy/mitm-ca-cert.pem,readonly" \
        --mount "type=bind,src=$RELAY_BIN,dst=/opt/aw-gateway/bin/acl-proxy-transparent-uds-relay,readonly" \
        --mount "type=bind,src=$FIREWALL,dst=/opt/aw-gateway/bin/aw-transparent-uds-firewall,readonly" \
        --mount "type=bind,src=$TMP_DIR/relay.json,dst=/etc/acl-proxy/transparent-uds-relay.json,readonly" \
        --mount "type=bind,src=$TMP_DIR/workload.sh,dst=/usr/local/bin/workload-smoke,readonly" \
        "$WORKLOAD_IMAGE" bash /usr/local/bin/workload-smoke >/dev/null

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

start_workload

python3 - "$WORKLOAD_CONTAINER" "$HTTP_SOCKET" "$HTTPS_SOCKET" \
    "$TMP_DIR/config/mitm-ca-cert.pem" "$RELAY_BIN" "$FIREWALL" \
    "$TMP_DIR/relay.json" "$TMP_DIR/workload.sh" <<'PY'
import json
import pathlib
import subprocess
import sys

container, http_socket, https_socket, public_ca, relay, firewall, config, script = sys.argv[1:]
inspect = json.loads(subprocess.check_output(["docker", "inspect", container]))[0]
sources = {mount["Source"] for mount in inspect["Mounts"]}
expected = {http_socket, https_socket, public_ca, relay, firewall, config, script}
if sources != expected:
    raise SystemExit(f"unexpected workload mount inventory: {sources!r}")
if any("acl-proxy.toml" in source or "key" in pathlib.Path(source).name.lower() for source in sources):
    raise SystemExit("workload received ACL Proxy configuration or private-key material")
if any(item.startswith("AW_IDENTITY_TOKEN=") for item in inspect["Config"]["Env"]):
    raise SystemExit("workload received AW_IDENTITY_TOKEN")
PY
docker exec "$WORKLOAD_CONTAINER" sh -eu -c '
    test ! -e /opt/aw-gateway/bin/acl-proxy
    test ! -e /etc/acl-proxy/acl-proxy.toml
    test -S /run/acl-proxy/transparent-http.sock
    test -S /run/acl-proxy/transparent-https.sock
    test -r /etc/acl-proxy/mitm-ca-cert.pem
    test "${AW_IDENTITY_TOKEN+x}" != x
'

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

body=$(request_http http://origin.test/allow)
[[ $body == 'origin:/allow:identity=host-only-identity' ]] \
    || fail "redirected HTTP response or identity injection was incorrect: $body"
https_body=$(request_https https://origin.test/secure)
[[ $https_body == 'origin:/secure:identity=host-only-identity' ]] \
    || fail "redirected HTTPS response or identity injection was incorrect: $https_body"

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
if len(matches) != 1 or matches[0].get("identity") != "host-only-identity":
    raise SystemExit(f"parent proxy did not observe the injected identity once: {matches!r}")
connects = [record for record in records if record.get("event") == "connect"]
if [record.get("target") for record in connects] != ["origin.test:443"]:
    raise SystemExit(f"parent proxy did not observe one HTTPS CONNECT: {connects!r}")
PY

deny_status=$(docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --silent --show-error --connect-timeout 2 --max-time 10 --noproxy '*' \
    --resolve "origin.test:80:$ORIGIN_IP" --output /tmp/denied.body \
    --write-out '%{http_code}' http://origin.test/denied)
[[ $deny_status == 403 ]] || fail "denied request returned HTTP $deny_status"
! grep -q '/denied' "$TMP_DIR/parent-logs/events.jsonl" \
    || fail "denied request reached the parent proxy"

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

if docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --fail --silent --connect-timeout 1 --max-time 2 --noproxy '*' \
    "http://$ORIGIN_IP:8080/escape" >/dev/null 2>&1; then
    fail "direct non-protected egress bypassed the fail-closed firewall"
fi

OLD_HOST_HTTP_ID=$(stat -Lc '%d:%i' "$HTTP_SOCKET")
OLD_HOST_HTTPS_ID=$(stat -Lc '%d:%i' "$HTTPS_SOCKET")
OLD_WORKLOAD_HTTP_ID=$(docker exec "$WORKLOAD_CONTAINER" \
    stat -Lc '%d:%i' /run/acl-proxy/transparent-http.sock)
OLD_WORKLOAD_HTTPS_ID=$(docker exec "$WORKLOAD_CONTAINER" \
    stat -Lc '%d:%i' /run/acl-proxy/transparent-https.sock)

ACTIVE_STREAM_SIZE=$((16 * 1024 * 512))
docker exec "$WORKLOAD_CONTAINER" rm -f /tmp/active-stream.body
docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --fail --silent --show-error --connect-timeout 2 --max-time 30 \
    --noproxy '*' --resolve "origin.test:80:$ORIGIN_IP" \
    --output /tmp/active-stream.body http://origin.test/active-stream \
    >"$TMP_DIR/active-client.stdout" 2>"$TMP_DIR/active-client.stderr" &
ACTIVE_CLIENT_PID=$!
ACTIVE_STREAM_STARTED=0
for _ in {1..100}; do
    active_size=$(docker exec "$WORKLOAD_CONTAINER" \
        stat -c '%s' /tmp/active-stream.body 2>/dev/null || printf '0\n')
    if (( active_size > 0 && active_size < ACTIVE_STREAM_SIZE )) \
        && kill -0 "$ACTIVE_CLIENT_PID" 2>/dev/null; then
        ACTIVE_STREAM_STARTED=1
        break
    fi
    kill -0 "$ACTIVE_CLIENT_PID" 2>/dev/null || break
    sleep 0.05
done
(( ACTIVE_STREAM_STARTED == 1 )) || fail "active stream did not start before proxy shutdown"

stop_acl_proxy
[[ -S $HTTP_SOCKET ]] || fail "ACL Proxy unexpectedly removed its published socket path"
[[ -S $HTTPS_SOCKET ]] || fail "ACL Proxy unexpectedly removed its HTTPS socket path"

for _ in {1..100}; do
    kill -0 "$ACTIVE_CLIENT_PID" 2>/dev/null || break
    sleep 0.05
done
kill -0 "$ACTIVE_CLIENT_PID" 2>/dev/null \
    && fail "active stream did not terminate promptly after proxy shutdown"
if wait "$ACTIVE_CLIENT_PID"; then
    fail "active stream unexpectedly completed after proxy shutdown"
fi
ACTIVE_CLIENT_PID=
active_size=$(docker exec "$WORKLOAD_CONTAINER" stat -c '%s' /tmp/active-stream.body)
(( active_size > 0 && active_size < ACTIVE_STREAM_SIZE )) \
    || fail "active stream was not observably incomplete after proxy shutdown"

if request_http http://origin.test/after-proxy-loss >/dev/null 2>&1; then
    fail "HTTP traffic succeeded after host ACL Proxy loss"
fi
if request_https https://origin.test/after-proxy-loss >/dev/null 2>&1; then
    fail "HTTPS traffic succeeded after host ACL Proxy loss"
fi
if docker exec --user 65534:65534 "$WORKLOAD_CONTAINER" curl \
    --fail --silent --connect-timeout 1 --max-time 2 --noproxy '*' \
    "http://$ORIGIN_IP:8080/escape" >/dev/null 2>&1; then
    fail "host proxy loss enabled a direct-network fallback"
fi

rm -f -- "$HTTP_SOCKET" "$HTTPS_SOCKET"
start_acl_proxy second
NEW_HOST_HTTP_ID=$(stat -Lc '%d:%i' "$HTTP_SOCKET")
NEW_HOST_HTTPS_ID=$(stat -Lc '%d:%i' "$HTTPS_SOCKET")
[[ $NEW_HOST_HTTP_ID != "$OLD_HOST_HTTP_ID" ]] \
    || fail "rebound HTTP socket reused the stale host inode"
[[ $NEW_HOST_HTTPS_ID != "$OLD_HOST_HTTPS_ID" ]] \
    || fail "rebound HTTPS socket reused the stale host inode"

[[ $(docker exec "$WORKLOAD_CONTAINER" stat -Lc '%d:%i' \
    /run/acl-proxy/transparent-http.sock) == "$OLD_WORKLOAD_HTTP_ID" ]] \
    || fail "running workload HTTP mount stopped referencing its pinned inode"
[[ $(docker exec "$WORKLOAD_CONTAINER" stat -Lc '%d:%i' \
    /run/acl-proxy/transparent-https.sock) == "$OLD_WORKLOAD_HTTPS_ID" ]] \
    || fail "running workload HTTPS mount stopped referencing its pinned inode"
if request_http http://origin.test/still-pinned >/dev/null 2>&1; then
    fail "running workload unexpectedly reconnected to the rebound HTTP socket"
fi
if request_https https://origin.test/still-pinned >/dev/null 2>&1; then
    fail "running workload unexpectedly reconnected to the rebound HTTPS socket"
fi

docker rm -f "$WORKLOAD_CONTAINER" >/dev/null
start_workload
NEW_WORKLOAD_HTTP_ID=$(docker exec "$WORKLOAD_CONTAINER" \
    stat -Lc '%d:%i' /run/acl-proxy/transparent-http.sock)
NEW_WORKLOAD_HTTPS_ID=$(docker exec "$WORKLOAD_CONTAINER" \
    stat -Lc '%d:%i' /run/acl-proxy/transparent-https.sock)
[[ $NEW_WORKLOAD_HTTP_ID != "$OLD_WORKLOAD_HTTP_ID" ]] \
    || fail "recreated workload retained the stale HTTP socket inode"
[[ $NEW_WORKLOAD_HTTPS_ID != "$OLD_WORKLOAD_HTTPS_ID" ]] \
    || fail "recreated workload retained the stale HTTPS socket inode"
body=$(request_http http://origin.test/recovered)
[[ $body == 'origin:/recovered:identity=host-only-identity' ]] \
    || fail "HTTP did not recover after workload recreation: $body"
https_body=$(request_https https://origin.test/recovered)
[[ $https_body == 'origin:/recovered:identity=host-only-identity' ]] \
    || fail "HTTPS did not recover after workload recreation: $https_body"

ACL_SHA=$(git -C "$ACL_REPO" rev-parse HEAD)
printf '%s\n' \
    "access-path=iptables-redirect-so-original-dst-proxy-v2-unix" \
    "http-allow=passed" \
    "https-mitm=passed" \
    "parent-proxy=passed" \
    "parent-connect=passed" \
    "identity-injection=passed" \
    "http1-downstream-keepalive=passed" \
    "deny-before-parent=passed" \
    "incremental-streaming=passed" \
    "active-stream-proxy-loss=passed" \
    "proxy-loss-fail-closed=passed" \
    "workload-isolation=passed" \
    "linux-socket-realization=pinned_inode" \
    "pinned-inode-rebind=passed" \
    "workload-recreate-recovery=passed" \
    "acl-repository-sha=$ACL_SHA" \
    "access-runtime-repository-sha=$EXPECTED_ACCESS_RUNTIME_SHA" \
    "aw-repository-sha=$EXPECTED_AW_SHA" \
    "base-image-id=$BASE_IMAGE_ID" \
    "workload-image-id=$WORKLOAD_IMAGE_ID" \
    "acl-proxy-sha256=$(sha256sum "$ACL_PROXY_BIN" | awk '{print $1}')" \
    "relay-sha256=$(sha256sum "$RELAY_BIN" | awk '{print $1}')" \
    "aw-firewall-sha256=$(sha256sum "$FIREWALL" | awk '{print $1}')"
printf '%s\n' 'transparent-uds-stack-smoke=passed'
SUCCESS=1
