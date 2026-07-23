#!/usr/bin/env bash

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
IMAGE=${AW_FIREWALL_SMOKE_IMAGE:-ubuntu:24.04}
FIREWALL="$ROOT/assets/aw-transparent-uds-firewall"

docker run --rm --privileged \
    --volume "$FIREWALL:/usr/local/bin/aw-transparent-uds-firewall:ro" \
    "$IMAGE" bash -euo pipefail -c '
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq iptables util-linux python3 curl >/dev/null
DNS=$(awk '\''/^nameserver / {print $2; exit}'\'' /etc/resolv.conf)
FW=/usr/local/bin/aw-transparent-uds-firewall

cat >/tmp/pre-hook.py <<'\''PY'\''
import os
import socket
import threading
import time


probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
probe.connect(("198.51.100.1", 9))
host = probe.getsockname()[0]
probe.close()


def tcp_server(listener):
    connection, _ = listener.accept()
    while True:
        data = connection.recv(4096)
        if not data:
            return
        connection.sendall(data)


listeners = []
for port in (80, 443):
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((host, port))
    listener.listen(1)
    listeners.append(listener)
    threading.Thread(target=tcp_server, args=(listener,), daemon=True).start()

udp_server = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
udp_server.bind((host, 443))


def udp_echo():
    while True:
        data, address = udp_server.recvfrom(4096)
        udp_server.sendto(data, address)


threading.Thread(target=udp_echo, daemon=True).start()

clients = []
for port in (80, 443):
    client = socket.create_connection((host, port), timeout=2)
    client.sendall(b"before")
    if client.recv(6) != b"before":
        raise RuntimeError(f"TCP {port} pre-hook echo failed")
    clients.append((f"TCP {port}", client))

udp_client = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
udp_client.settimeout(2)
udp_client.connect((host, 443))
udp_client.send(b"before")
if udp_client.recv(6) != b"before":
    raise RuntimeError("UDP 443 pre-hook echo failed")
clients.append(("UDP 443", udp_client))

open("/tmp/pre-hook.ready", "w").close()
while not os.path.exists("/tmp/firewall-installed"):
    time.sleep(0.02)

for label, client in clients:
    client.settimeout(1)
    try:
        client.send(b"after")
        if client.recv(5) == b"after":
            raise RuntimeError(f"{label} established connection survived firewall installation")
    except (BrokenPipeError, ConnectionError, OSError, socket.timeout):
        pass
PY
python3 /tmp/pre-hook.py & PRE_HOOK_PID=$!
for _ in $(seq 1 50); do
    [[ -f /tmp/pre-hook.ready ]] && break
    kill -0 "$PRE_HOOK_PID"
    sleep 0.1
done
[[ -f /tmp/pre-hook.ready ]]
$FW install --dns-server "$DNS"
$FW check --dns-server "$DNS"
touch /tmp/firewall-installed
wait "$PRE_HOOK_PID"
python3 -m http.server 3128 --bind 127.0.0.1 >/tmp/http.log 2>&1 & HTTP_PID=$!
python3 -m http.server 3129 --bind 127.0.0.1 >/tmp/https.log 2>&1 & HTTPS_PID=$!
trap '\''kill "$HTTP_PID" "$HTTPS_PID" 2>/dev/null || true'\'' EXIT
sleep 0.2
curl --noproxy '\''*'\'' --connect-timeout 2 -fsS http://198.51.100.1/ >/dev/null
timeout 3 bash -c '\''exec 3<>/dev/tcp/198.51.100.1/443; printf "GET / HTTP/1.0\r\n\r\n" >&3; grep -q "200 OK" <&3'\''
if timeout 2 bash -c '\''exec 3<>/dev/tcp/1.1.1.1/81'\''; then
    echo "direct non-protected egress unexpectedly succeeded" >&2
    exit 1
fi

OLD=$(cat /run/aw-gateway/transparent-uds-firewall.generation)
iptables -F "AWUDSF_$OLD"
if $FW check --dns-server "$DNS" 2>/dev/null; then
    echo "drifted generation unexpectedly passed validation" >&2
    exit 1
fi
$FW repair --dns-server "$DNS"
NEW=$(cat /run/aw-gateway/transparent-uds-firewall.generation)
[[ "$NEW" != "$OLD" ]]
$FW check --dns-server "$DNS"
$FW remove --dns-server "$DNS"
! iptables -S OUTPUT | grep -q AWUDS
! iptables -t nat -S OUTPUT | grep -q AWUDS
! ip6tables -S OUTPUT | grep -q AWUDS

$FW watch --dns-server "$DNS" --check-interval 1 >/tmp/watch.log 2>&1 & WATCH_PID=$!
for _ in $(seq 1 50); do
    [[ -f /run/aw-gateway/transparent-uds-firewall.generation.ready ]] && break
    sleep 0.1
done
[[ -f /run/aw-gateway/transparent-uds-firewall.generation.ready ]]
timeout 3 $FW check --dns-server "$DNS"
kill "$WATCH_PID"
wait "$WATCH_PID" || true
[[ ! -e /run/aw-gateway/transparent-uds-firewall.generation.ready ]]
$FW remove --dns-server "$DNS"
'

docker run --rm --privileged \
    --volume "$FIREWALL:/usr/local/bin/aw-transparent-uds-firewall:ro" \
    "$IMAGE" bash -euo pipefail -c '
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq iptables util-linux >/dev/null
DNS=$(awk '\''/^nameserver / {print $2; exit}'\'' /etc/resolv.conf)
FW=/usr/local/bin/aw-transparent-uds-firewall
output_tag_at() {
    local command="$1" table="$2" position="$3" rules
    if [[ "$table" = filter ]]; then
        rules=$($command -S OUTPUT)
    else
        rules=$($command -t "$table" -S OUTPUT)
    fi
    awk -v position="$position" '\''
        $1 == "-A" && $2 == "OUTPUT" {
            count += 1
            if (count == position) {
                for (i = 3; i < NF; i++) if ($i == "--comment") {
                    value = $(i + 1); gsub(/^"|"$/, "", value); print value; exit
                }
            }
        }
    '\'' <<<"$rules"
}
mkdir /failbin
cat >/failbin/iptables-restore <<'\''WRAPPER'\''
#!/bin/bash
set -euo pipefail
input=$(mktemp)
trap '\''rm -f "$input"'\'' EXIT
cat >"$input"
if grep -q '\'':AWUDSF_'\'' "$input"; then
    exit 70
fi
exec /usr/sbin/iptables-restore "$@" <"$input"
WRAPPER
chmod 0755 /failbin/iptables-restore

if PATH="/failbin:$PATH" $FW install --dns-server "$DNS" 2>/tmp/fresh-error; then
    echo "injected fresh generation failure unexpectedly succeeded" >&2
    exit 1
fi
iptables -C OUTPUT -m comment --comment awuds.failsafe4.hook -j AWUDS_FAIL4
ip6tables -C OUTPUT -m comment --comment awuds.failsafe6.hook -j AWUDS_FAIL6
[[ ! -e /run/aw-gateway/transparent-uds-firewall.generation ]]
[[ ! -e /run/aw-gateway/transparent-uds-firewall.generation.ready ]]
if timeout 2 bash -c '\''exec 3<>/dev/tcp/1.1.1.1/81'\''; then
    echo "fresh failure left direct egress available" >&2
    exit 1
fi
$FW remove --dns-server "$DNS"

$FW install --dns-server "$DNS"
OLD=$(cat /run/aw-gateway/transparent-uds-firewall.generation)
[[ "$OLD" =~ ^[0-9a-f]{16}$ ]]
iptables -I AWUDS_FAIL4 1 -j ACCEPT
if $FW check --dns-server "$DNS" 2>/tmp/failsafe4-content-error; then
    echo "IPv4 fail-safe chain with prepended ACCEPT unexpectedly passed validation" >&2
    exit 1
fi
iptables -D AWUDS_FAIL4 -j ACCEPT
ip6tables -A AWUDS_FAIL6 -j ACCEPT
if $FW check --dns-server "$DNS" 2>/tmp/failsafe6-content-error; then
    echo "IPv6 fail-safe chain with extra ACCEPT unexpectedly passed validation" >&2
    exit 1
fi
ip6tables -D AWUDS_FAIL6 -j ACCEPT
iptables -I "AWUDSF_$OLD" 1 -j ACCEPT
if $FW check --dns-server "$DNS" 2>/tmp/filter4-content-error; then
    echo "IPv4 generation chain with prepended ACCEPT unexpectedly passed validation" >&2
    exit 1
fi
iptables -D "AWUDSF_$OLD" -j ACCEPT
iptables -D "AWUDSF_$OLD" 1
iptables -A "AWUDSF_$OLD" -m comment --comment "awuds.gen.$OLD.filter4.dnat" -m conntrack --ctstate DNAT -j ACCEPT
if $FW check --dns-server "$DNS" 2>/tmp/filter4-order-error; then
    echo "IPv4 generation chain with reordered managed rule unexpectedly passed validation" >&2
    exit 1
fi
iptables -D "AWUDSF_$OLD" -m comment --comment "awuds.gen.$OLD.filter4.dnat" -m conntrack --ctstate DNAT -j ACCEPT
iptables -I "AWUDSF_$OLD" 1 -m comment --comment "awuds.gen.$OLD.filter4.dnat" -m conntrack --ctstate DNAT -j ACCEPT
ip6tables -I "AWUDS6_$OLD" 1 -j ACCEPT
if $FW check --dns-server "$DNS" 2>/tmp/filter6-content-error; then
    echo "IPv6 generation chain with prepended ACCEPT unexpectedly passed validation" >&2
    exit 1
fi
ip6tables -D "AWUDS6_$OLD" -j ACCEPT
iptables -t nat -I "AWUDSN_$OLD" 1 -j ACCEPT
if $FW check --dns-server "$DNS" 2>/tmp/nat-content-error; then
    echo "NAT generation chain with prepended ACCEPT unexpectedly passed validation" >&2
    exit 1
fi
iptables -t nat -D "AWUDSN_$OLD" -j ACCEPT
$FW check --dns-server "$DNS"
iptables -I OUTPUT 1 -j ACCEPT
iptables -t nat -I OUTPUT 1 -j ACCEPT
ip6tables -I OUTPUT 1 -j ACCEPT
if $FW check --dns-server "$DNS" 2>/tmp/order-error; then
    echo "preceding ACCEPT rules unexpectedly passed validation" >&2
    exit 1
fi
if PATH="/failbin:$PATH" $FW repair --dns-server "$DNS" 2>/tmp/update-error; then
    echo "injected repair failure unexpectedly succeeded" >&2
    exit 1
fi
[[ "$(cat /run/aw-gateway/transparent-uds-firewall.generation)" = "$OLD" ]]
iptables -C OUTPUT -m comment --comment awuds.failsafe4.hook -j AWUDS_FAIL4
ip6tables -C OUTPUT -m comment --comment awuds.failsafe6.hook -j AWUDS_FAIL6
[[ "$(output_tag_at iptables filter 1)" = awuds.failsafe4.hook ]]
[[ "$(output_tag_at ip6tables filter 1)" = awuds.failsafe6.hook ]]
if timeout 2 bash -c '\''exec 3<>/dev/tcp/1.1.1.1/81'\''; then
    echo "failed drift repair left direct egress available" >&2
    exit 1
fi
$FW repair --dns-server "$DNS"
NEW=$(cat /run/aw-gateway/transparent-uds-firewall.generation)
[[ "$NEW" != "$OLD" ]]
$FW check --dns-server "$DNS"
[[ "$(output_tag_at iptables filter 1)" = "awuds.gen.$NEW.filter4.hook" ]]
[[ "$(output_tag_at iptables filter 2)" = awuds.failsafe4.hook ]]
[[ "$(output_tag_at iptables nat 1)" = "awuds.gen.$NEW.nat.hook" ]]
[[ "$(output_tag_at ip6tables filter 1)" = "awuds.gen.$NEW.filter6.hook" ]]
[[ "$(output_tag_at ip6tables filter 2)" = awuds.failsafe6.hook ]]
$FW remove --dns-server "$DNS"

AW_FIREWALL_TEST_PID=1048577 $FW install --dns-server "$DNS"
ORPHAN=$(cat /run/aw-gateway/transparent-uds-firewall.generation)
[[ "$ORPHAN" =~ ^[0-9a-f]{16}$ ]]
[[ "${ORPHAN:8:4}" = 0001 ]]
$FW check --dns-server "$DNS"
$FW remove --dns-server "$DNS"

$FW install --dns-server "$DNS"
[[ "$(cat /run/aw-gateway/transparent-uds-firewall.generation)" != "$ORPHAN" ]]
iptables -N "AWUDSF_$ORPHAN"
iptables -A "AWUDSF_$ORPHAN" -j DROP
iptables -A OUTPUT -j "AWUDSF_$ORPHAN"
iptables -t nat -N "AWUDSN_$ORPHAN"
iptables -t nat -A "AWUDSN_$ORPHAN" -j RETURN
iptables -t nat -A OUTPUT -j "AWUDSN_$ORPHAN"
ip6tables -N "AWUDS6_$ORPHAN"
ip6tables -A "AWUDS6_$ORPHAN" -j DROP
ip6tables -A OUTPUT -j "AWUDS6_$ORPHAN"
printf '\''invalid-state\n'\'' >/run/aw-gateway/transparent-uds-firewall.generation
$FW remove --dns-server "$DNS"
! iptables -S | grep -Eq '\''AWUDS(F|_FAIL4)'\''
! iptables -t nat -S | grep -q AWUDSN
! ip6tables -S | grep -Eq '\''AWUDS(6|_FAIL6)'\''
'

echo "transparent firewall smoke: ok"
