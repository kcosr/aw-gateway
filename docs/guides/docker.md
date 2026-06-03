# Docker Deployment Guide

This guide shows a generic Docker setup for `aw-gateway`. It covers native
Linux Docker in two modes:

- **Local Docker workstation**: the gateway runs on the same Linux workstation
  as the SSH client and exposes a loopback-only local SSH listener.
- **Remote Docker host over SSH**: the gateway runs on a remote Linux host.
  Host SSH authenticates the user, and generated SSH config connects clients to
  container SSH through `ProxyCommand`.

This guide intentionally does not configure traffic interception, custom CA
trust, command-policy wrappers, or proxy services. Those are site policy
add-ons. See [Firewall Policy](firewall.md) and
[Proxy And CA Policy](proxy.md) for reusable optional layers.

Run commands from the extracted release archive unless a section says
otherwise. Docker must be installed and available on `PATH`.

Use the Colima guide instead of this guide for macOS Colima. Colima uses Docker
inside a Linux VM and needs a different SSH endpoint mode.

## Example Files

Copyable examples live under:

```text
examples/docker/
  Containerfile.ubuntu
  gateway-local.toml
  gateway-remote.toml
  sshd_config_agent
  sshd-match-restricted.conf
  start-container-sshd
```

Full-file code blocks below match files in `examples/docker/` unless the block
is explicitly labeled as an excerpt.

## Host Layout

Use one deployment root:

```text
/opt/aw-gateway/
  bin/aw-gateway
  etc/gateway.toml
  runtime/linux/
    aw-container-agent
    aw-container-bootstrap
    aw-ssh-command-filter
    sshd_config_agent
    start-container-sshd
```

Install the host-side gateway and Linux container runtime files:

```bash
RELEASE_ROOT=/path/to/aw-gateway-VERSION-linux-x86_64
sudo install -d -m 0755 /opt/aw-gateway/bin /opt/aw-gateway/runtime/linux
sudo install -m 0755 "$RELEASE_ROOT/bin/aw-gateway" \
  /opt/aw-gateway/bin/aw-gateway
sudo install -m 0755 "$RELEASE_ROOT/runtime/linux/aw-container-agent" \
  /opt/aw-gateway/runtime/linux/aw-container-agent
sudo install -m 0755 "$RELEASE_ROOT/runtime/linux/aw-container-bootstrap" \
  /opt/aw-gateway/runtime/linux/aw-container-bootstrap
sudo install -m 0755 "$RELEASE_ROOT/runtime/linux/aw-ssh-command-filter" \
  /opt/aw-gateway/runtime/linux/aw-ssh-command-filter
sudo install -m 0755 "$RELEASE_ROOT/runtime/linux/start-container-sshd" \
  /opt/aw-gateway/runtime/linux/start-container-sshd
sudo install -m 0644 "$RELEASE_ROOT/runtime/linux/sshd_config_agent" \
  /opt/aw-gateway/runtime/linux/sshd_config_agent
```

`/opt/aw-gateway` is the recommended production layout. For local evaluation
without sudo, use the same relative layout under a user-owned directory and pass
an explicit `--config` path whose runtime mounts point at that directory.

## Minimal Ubuntu Image

Full file: `examples/docker/Containerfile.ubuntu`

```dockerfile
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN if id ubuntu >/dev/null 2>&1; then userdel ubuntu && rm -rf /home/ubuntu; fi

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        git \
        less \
        lsof \
        nodejs \
        npm \
        openssh-client \
        openssh-server \
        procps \
        python3 \
        sudo \
        tar \
        vim-tiny \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /opt/aw-gateway/bin /run/sshd

WORKDIR /root

CMD ["/bin/bash"]
```

Build it:

```bash
docker build \
  -t aw-gateway/ubuntu-base:latest \
  -f examples/docker/Containerfile.ubuntu \
  examples/docker
```

## Container SSH Files

Full file: `examples/docker/start-container-sshd`. The helper copies the
mounted base SSHD config to a runtime file, applies `[target_defaults.container_ssh]` transfer
policy, validates it with `sshd -t`, and starts container `sshd`.

Full file: `examples/docker/sshd_config_agent`

```sshconfig
Port 22
ListenAddress 127.0.0.1
PidFile /run/sshd-agent.pid

AuthenticationMethods publickey
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AuthorizedKeysFile .aw-gateway/ssh/authorized_keys

AllowAgentForwarding no
AllowTcpForwarding local
PermitOpen 127.0.0.1:* localhost:* [::1]:*
AllowStreamLocalForwarding yes
X11Forwarding no
PermitTunnel no
PermitTTY yes
PermitUserEnvironment no
GatewayPorts no

Subsystem sftp /usr/lib/openssh/sftp-server

SetEnv SHELL=/usr/bin/bash
SetEnv PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
```

## Remote Docker Config

Excerpt from `examples/docker/gateway-remote.toml`:

Important Docker-specific fields:

```toml
[runtime]
type = "docker"
docker_host = "unix:///var/run/docker.sock"

[targets.ubuntu]
container_user = "root"
container_home = "/home/{user}"

[targets.ubuntu.identity]
bootstrap_user = "root"
session_user = "{user}"
session_uid = "{uid}"
session_gid = "{gid}"
session_home = "/home/{user}"
session_shell = "/bin/bash"
```

Unlike Podman `--userns=keep-id`, Docker does not automatically map the host
user into the container. The bootstrap step creates the configured session user
inside the container so bind-mounted workspace ownership is predictable.
The example Ubuntu image removes the stock `ubuntu` account first because
Ubuntu 24.04 reserves UID/GID 1000 for that account, which commonly conflicts
with the first host user on Linux workstations.

Do not replace that line with a host-specific user such as `alice`. The
gateway should create the session identity from the authenticated host user
or target config at runtime; the image only needs to avoid known UID/GID
collisions.

Install and validate:

```bash
sudo install -d -m 0755 /etc/aw-gateway
sudo install -m 0644 examples/docker/gateway-remote.toml /etc/aw-gateway/gateway.toml
/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml config validate
```

`/etc/aw-gateway/gateway.toml` is the system gateway config path. This guide
passes `--config` explicitly so managed-host commands do not depend on whether
the user also has `~/.config/aw-gateway/gateway.toml`.

Install the workstation public key from a remote host:

```bash
ssh user@dev.example.com \
  '/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml add-container-key ubuntu --public-key ~/.ssh/id_ed25519.pub'
```

Then print the SSH client config:

```bash
ssh user@dev.example.com \
  '/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml client-config ubuntu'
```

The printed alias uses `ProxyCommand` through the remote Docker host:

```sshconfig
Host aw-ubuntu
    HostName aw-container-ubuntu
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    ProxyCommand ssh -T dev.example.com /opt/aw-gateway/bin/aw-gateway connect ubuntu
```

## Local Docker Workstation

Use full file: `examples/docker/gateway-local.toml`

Native Linux Docker can use the same socket bridge mode as Podman because the
gateway creates a short runtime socket directory on the Linux host and bind
mounts it into the container:

```toml
[targets.ubuntu.local_ssh]
mode = "listen"
backend = "socket"
readiness = "agent_control"
host = "127.0.0.1"
```

Install, validate, and start:

```bash
sudo install -d -m 0755 /opt/aw-gateway/etc
sudo install -m 0644 examples/docker/gateway-local.toml /opt/aw-gateway/etc/gateway.toml
/opt/aw-gateway/bin/aw-gateway --config /opt/aw-gateway/etc/gateway.toml config validate
/opt/aw-gateway/bin/aw-gateway --config /opt/aw-gateway/etc/gateway.toml up ubuntu --json
```

Keep the `up` process running while local SSH-compatible clients use the
generated loopback config.

## Smoke Test

After installing the runtime files, building the image, and validating the
config, verify Docker bootstrap and SSH independently:

```bash
# Confirms that bootstrap created the expected session identity.
/opt/aw-gateway/bin/aw-gateway --config /opt/aw-gateway/etc/gateway.toml run ubuntu -- id

# Prints the SSH alias to use from another terminal or workstation.
/opt/aw-gateway/bin/aw-gateway --config /opt/aw-gateway/etc/gateway.toml client-config ubuntu

# With the printed SSH config installed, confirms the container SSH path.
ssh aw-ubuntu 'pwd && id && hostname'
```

For a remote Docker host, use `/etc/aw-gateway/gateway.toml` if that is where
the host config was installed. For a local user-owned test layout, keep the
same commands but point `--config` at the test config.

## Optional Restricted Remote Users

Use full file: `examples/docker/sshd-match-restricted.conf`

```sshconfig
Match Group aw-gateway-users
    ForceCommand /opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml
    PermitTTY yes
    AllowTcpForwarding no
    AllowStreamLocalForwarding no
    PermitTunnel no
    X11Forwarding no
    AllowAgentForwarding no
```

Restricted users get container entry on normal `ssh user@host`. Unrestricted
users keep their host shell and use the generated `aw-ubuntu` alias for direct
container SSH/SCP/SFTP/VS Code.

## Network Policy Hooks

Do not put proxy, CA, or firewall setup in the basic Docker guide. If a
deployment needs policy, add site-managed lifecycle or host steps and document
the concrete commands using [Firewall Policy](firewall.md) and
[Proxy And CA Policy](proxy.md).

## Validation Checklist

```bash
/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml config validate
/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml status ubuntu
ssh aw-ubuntu 'pwd && id && hostname'
scp ./README.md aw-ubuntu:~/README.md
sftp aw-ubuntu
```

For VS Code Remote SSH, select the generated `aw-ubuntu` host alias.
