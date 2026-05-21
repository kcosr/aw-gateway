# Podman Deployment Guide

This guide shows a generic Podman setup for `aw-gateway`. It covers two
deployment modes:

- **Local Podman workstation**: the gateway runs on the same Linux workstation
  as the SSH client and exposes a loopback-only local SSH listener.
- **Remote Podman host over SSH**: the gateway runs on a remote Linux host.
  Host SSH authenticates the user, and generated SSH config connects clients to
  container SSH through `ProxyCommand`.

This guide intentionally does not configure traffic interception, custom CA
trust, command-policy wrappers, or proxy services. Those are site policy
add-ons. See [Firewall Policy](firewall.md) and
[Proxy And CA Policy](proxy.md) for reusable optional layers.

Run commands from the cloned repository root unless a section says otherwise.
Podman must be installed and available on `PATH`; rootless Podman is
recommended for managed-host deployments.

## What This Provides

The setup gives users a standard SSH endpoint for a Podman-managed development
container:

- `aw-gateway` starts or reuses a configured container.
- `aw-container-bootstrap` is mounted into the container and prepares runtime
  state before execing `aw-container-agent`.
- `aw-container-agent` supervises container `sshd` and exposes a host-visible
  SSH bridge socket.
- Generated SSH config supports `ssh`, `scp`, `sftp`, and VS Code Remote SSH
  into the container.
- Remote hosts can optionally restrict selected users with OpenSSH
  `ForceCommand`.

## Example Files

Copyable examples live under:

```text
examples/podman/
  Containerfile.ubuntu
  gateway-local.toml
  gateway-remote.toml
  sshd_config_agent
  sshd-match-restricted.conf
  start-container-sshd
```

Full-file code blocks below match the files in `examples/podman/` unless the
block is explicitly labeled as an excerpt. Adjust paths, hostname, target
names, and image names for your environment.

## Host Layout

Use one deployment root. The examples use:

```text
/opt/aw-gateway/
  bin/
    aw-gateway
  etc/
    gateway.toml
  runtime/
    linux/
      aw-container-agent
      aw-container-bootstrap
      aw-ssh-command-filter
      sshd_config_agent
      start-container-sshd
```

Install the host-side gateway:

```bash
sudo install -d -m 0755 /opt/aw-gateway/bin
sudo install -m 0755 target/release/aw-gateway /opt/aw-gateway/bin/aw-gateway
```

Install the container-side runtime files:

```bash
sudo install -d -m 0755 /opt/aw-gateway/runtime/linux
sudo install -m 0755 target/release/aw-container-agent \
  /opt/aw-gateway/runtime/linux/aw-container-agent
sudo install -m 0755 target/release/aw-container-bootstrap \
  /opt/aw-gateway/runtime/linux/aw-container-bootstrap
sudo install -m 0755 target/release/aw-ssh-command-filter \
  /opt/aw-gateway/runtime/linux/aw-ssh-command-filter
sudo install -m 0755 examples/podman/start-container-sshd \
  /opt/aw-gateway/runtime/linux/start-container-sshd
sudo install -m 0644 examples/podman/sshd_config_agent \
  /opt/aw-gateway/runtime/linux/sshd_config_agent
```

## Minimal Ubuntu Image

The image only needs base packages and OpenSSH. Gateway binaries and configs
are mounted at runtime.

Full file: `examples/podman/Containerfile.ubuntu`

```dockerfile
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

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
podman build \
  -t aw-gateway/ubuntu-base:latest \
  -f examples/podman/Containerfile.ubuntu \
  examples/podman
```

## Container SSH Files

Full file: `examples/podman/start-container-sshd`. The helper copies the
mounted base SSHD config to a runtime file, applies `[container_ssh]` transfer
policy, validates it with `sshd -t`, and starts container `sshd`.

Full file: `examples/podman/sshd_config_agent`

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

## Remote Gateway Config

Excerpt from `examples/podman/gateway-remote.toml`:

Local and remote modes use the same container target, bootstrap mounts, and
container-agent service shape. The remote file below omits `local_ssh` so
generated client config uses `ProxyCommand` through the remote host.

```toml
schema_version = "1"
default_target = "ubuntu"

[runtime]
type = "podman"

[logging]
level = "info"
directory = "{state}/logs/gateway"
max_bytes = 104857600
max_files = 5
console = false

[workspace]
path = "workspace"
state_dir = ".aw-gateway"

[ssh_dispatch]
allow_interactive_shell = true
allow_container_commands = true
enabled_gateway_actions = [
  "connect",
  "up",
  "status",
  "targets",
  "stop",
  "set-default",
  "show-default",
  "reset-default",
  "add-key",
  "add-host-key",
  "add-container-key",
  "client-config",
  "client-bundle",
  "help",
]

[client_config]
inner_alias_template = "aw-{target}"
container_host_template = "aw-container-{target}"
host = "dev.example.com"
gateway_path = "/opt/aw-gateway/bin/aw-gateway"
default_identity_dir = "~/.ssh/aw-gateway"

[targets.ubuntu]
image = "aw-gateway/ubuntu-base"
mode = "fixed"
name = "{image_slug}"
stop_when_idle = true
remove_on_stop = false

[targets.ubuntu.identity]
bootstrap_user = "root"
session_user = "{user}"
session_uid = "{uid}"
session_gid = "{gid}"
session_home = "/home/{user}"
session_shell = "/bin/bash"

[targets.ubuntu.idle_cleanup]
owner = "agent"
action = "exit_container"
idle_grace = "5m"
preserve_processes = ["tmux", "screen"]
poll_interval = "30s"
shutdown_timeout = "20s"

[[lifecycle_steps]]
phase = "pre_start"
name = "ensure-workspace"
required = true
command = ["/usr/bin/mkdir", "-p", "{workspace}"]

[[container_mounts]]
source = "/opt/aw-gateway/runtime/linux/aw-container-bootstrap"
target = "/opt/aw-gateway/bin/aw-container-bootstrap"
mode = "ro"

[[container_mounts]]
source = "/opt/aw-gateway/runtime/linux/aw-container-agent"
target = "/opt/aw-gateway/bin/aw-container-agent"
mode = "ro"

[[container_mounts]]
source = "/opt/aw-gateway/runtime/linux/aw-ssh-command-filter"
target = "/opt/aw-gateway/bin/aw-ssh-command-filter"
mode = "ro"

[[container_mounts]]
source = "/opt/aw-gateway/runtime/linux/start-container-sshd"
target = "/opt/aw-gateway/bin/start-container-sshd"
mode = "ro"

[[container_mounts]]
source = "/opt/aw-gateway/runtime/linux/sshd_config_agent"
target = "/etc/ssh/sshd_config_agent"
mode = "ro"

[container_bootstrap]
enabled = true
entrypoint = "/opt/aw-gateway/bin/aw-container-bootstrap"
agent_program = "/opt/aw-gateway/bin/aw-container-agent"

[container_ssh.transfer]
sftp = "allow"
legacy_scp = "allow"

[container_agent]
enabled = true

[[container_agent.services]]
name = "container-sshd"
required = true
user = "root"
command = ["/opt/aw-gateway/bin/start-container-sshd"]
restart = "always"
depends_on = []

[container_agent.services.health_check]
type = "tcp"
host = "127.0.0.1"
port = 22
interval = "2s"
timeout = "1s"

[container_agent.ssh_bridge]
enabled = true
socket = "{container_state_dir}/ssh.sock"
target = "127.0.0.1:22"
mode = "0600"
```

## Local Podman Workstation

Use full file: `examples/podman/gateway-local.toml`

The local file is the same base shape as the remote file, with a local host and
this target-local SSH listener excerpt:

```toml
[targets.ubuntu.local_ssh]
mode = "listen"
backend = "socket"
readiness = "agent_control"
host = "127.0.0.1"
```

Install the local config:

```bash
sudo install -d -m 0755 /opt/aw-gateway/etc
sudo install -m 0644 examples/podman/gateway-local.toml \
  /opt/aw-gateway/etc/gateway.toml
```

Validate:

```bash
/opt/aw-gateway/bin/aw-gateway \
  --config /opt/aw-gateway/etc/gateway.toml \
  config validate
```

Start the target and local listener:

```bash
/opt/aw-gateway/bin/aw-gateway \
  --config /opt/aw-gateway/etc/gateway.toml \
  up ubuntu --json
```

The command prints readiness details and writes generated SSH config under the
workspace state directory. Keep this process running while local SSH-compatible
tools connect through the listener.

Install your workstation public key into the container authorized-key file:

```bash
/opt/aw-gateway/bin/aw-gateway \
  --config /opt/aw-gateway/etc/gateway.toml \
  add-container-key ubuntu \
  --public-key ~/.ssh/id_ed25519.pub
```

Then use the generated alias with `ssh`, `scp`, `sftp`, or VS Code Remote SSH.

## Remote Podman Host Over SSH

For remote mode, omit `[targets.<name>.local_ssh]`. The generated client config
uses `ProxyCommand` to run the gateway on the remote host and proxy bytes into
container SSH.

Install the remote config:

```bash
sudo install -d -m 0755 /etc/aw-gateway
sudo install -m 0644 examples/podman/gateway-remote.toml \
  /etc/aw-gateway/gateway.toml
```

`/etc/aw-gateway/gateway.toml` is the gateway's default host config path.
These commands still pass `--config` so the guide remains explicit.

Set the real host in `[client_config]`:

```toml
[client_config]
host = "dev.example.com"
gateway_path = "/opt/aw-gateway/bin/aw-gateway"
```

Validate on the remote host:

```bash
/opt/aw-gateway/bin/aw-gateway \
  --config /etc/aw-gateway/gateway.toml \
  config validate
```

An unrestricted host user can keep their normal host shell and still generate
an explicit container SSH alias:

```bash
ssh user@dev.example.com \
  '/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml add-container-key ubuntu --public-key ~/.ssh/id_ed25519.pub'
```

Then print the SSH client config:

```bash
ssh user@dev.example.com \
  '/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml client-config ubuntu'
```

The printed config has this shape:

```sshconfig
Host aw-ubuntu
    HostName aw-container-ubuntu
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    ProxyCommand ssh -T dev.example.com /opt/aw-gateway/bin/aw-gateway connect ubuntu
```

Add the printed stanza to the workstation's SSH config. If the workstation
needs a specific username or identity to reach the real host, put that in a
normal host stanza:

```sshconfig
Host dev.example.com
    User user
    IdentityFile ~/.ssh/id_ed25519
```

Then connect directly to the container:

```bash
ssh aw-ubuntu
scp ./file.txt aw-ubuntu:~/file.txt
sftp aw-ubuntu
```

## Optional Restricted Remote Users

Restricted remote users can be forced into the gateway for normal host SSH
login. Direct container SSH/SCP/SFTP still uses the generated `ProxyCommand`
alias above.

Full file: `examples/podman/sshd-match-restricted.conf`

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

Install it as a site-specific SSHD include, then reload SSHD:

```bash
sudo install -m 0644 examples/podman/sshd-match-restricted.conf \
  /etc/ssh/sshd_config.d/90-aw-gateway.conf
sudo sshd -t
sudo systemctl reload sshd
```

With this block:

- `ssh user@dev.example.com` enters the configured container path.
- `ssh user@dev.example.com status` runs an allowed gateway action.
- `ssh aw-ubuntu` uses container SSH through the generated alias.

Without this block:

- `ssh user@dev.example.com` remains a normal host shell.
- `ssh aw-ubuntu` still uses container SSH through the generated alias.

## Network Policy Hooks

Do not put proxy, CA, or firewall setup in the basic Podman guide. Keep those
as optional site policy. Use [Firewall Policy](firewall.md) for egress rules
and [Proxy And CA Policy](proxy.md) for proxy services, CA trust, and
transparent redirect patterns.

If a deployment needs network policy, model it as a host step or bootstrap step
that calls a site-managed script:

```toml
[[host_steps]]
name = "network-policy"
required = true
timeout = "30s"
command = ["/opt/aw-gateway/bin/network-policy", "add", "{container_pid}"]

[host_steps.health_check]
type = "command"
command = ["/opt/aw-gateway/bin/network-policy", "check", "{container_pid}"]
```

Keeping those details out of this guide makes the core Podman setup usable
without site-specific policy assumptions.

## Validation Checklist

On the host:

```bash
/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml config validate
/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml targets
/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml status ubuntu
/opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml help
```

On the workstation:

```bash
ssh aw-ubuntu 'pwd && id && hostname'
scp ./README.md aw-ubuntu:~/README.md
sftp aw-ubuntu
```

For VS Code Remote SSH, select the generated `aw-ubuntu` host alias.

## Troubleshooting

- If `config validate` fails, fix the TOML before testing SSH.
- If container startup fails, check `podman ps -a`, `podman logs <container>`,
  and the gateway log under `{workspace}/{state_dir}/logs/gateway`.
- If container SSH fails, exec into the container and verify
  `/usr/sbin/sshd -T -f /etc/ssh/sshd_config_agent` and that
  `~/.aw-gateway/ssh/authorized_keys` exists for the session user.
- If `ssh aw-ubuntu` reaches the host instead of the container, check the local
  SSH config order and confirm the generated `ProxyCommand` stanza is being
  matched.
- If an unrestricted remote user gets a host shell with `ssh user@host`, that
  is expected. Use the generated `aw-ubuntu` alias for container SSH.
