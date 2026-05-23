# Colima Deployment Guide

This guide shows a generic local macOS Colima setup for `aw-gateway`.

> [!WARNING]
> Colima support has had lighter real-world testing than the Linux
> Podman and Docker paths. Treat this guide as a local-workstation starting
> point and validate the target profile, architecture, SSH path, and any site
> policy hooks before relying on it operationally.

Colima runs Docker inside a Linux VM. Because the VM boundary makes
host-visible Unix sockets awkward, this guide uses `local_ssh.backend =
"published_port"` and `readiness = "ssh_only"` so the gateway connects to a
loopback-published container SSH port. The container agent still supervises the
inner SSH service, but its control socket is disabled because the host gateway
does not use agent control in this mode.

This guide intentionally does not configure traffic interception, custom CA
trust, command-policy wrappers, or proxy services. Those are site policy
add-ons. See [Firewall Policy](firewall.md) and
[Proxy And CA Policy](proxy.md) for reusable optional layers.

## Example Files

Copyable examples live under:

```text
examples/colima/
  Containerfile.ubuntu
  gateway-local.toml
  sshd_config_agent
  start-container-sshd
```

Full-file code blocks below match files in `examples/colima/` unless the block
is explicitly labeled as an excerpt.

## Install Colima And Docker CLI

Colima is the macOS VM/runtime layer. The Docker CLI is still used to build
images and talk to the Colima Docker runtime.

Homebrew is the most common install path:

```bash
brew install colima docker
```

Non-Homebrew Colima options include MacPorts, Nix, and Mise:

```bash
# MacPorts
sudo port install colima

# Nix
nix-env -iA nixpkgs.colima

# Mise
mise use -g colima@latest
```

If you do not install Docker CLI through Homebrew, install it through your
preferred package manager and verify it is on `PATH`:

```bash
colima version
docker version --client
```

## Start Colima

Use a dedicated profile so gateway experiments do not affect other Docker
workloads:

```bash
colima start \
  --profile aw-gateway \
  --runtime docker \
  --vm-type vz \
  --arch x86_64 \
  --cpu 2 \
  --memory 4 \
  --disk 20
```

Use `--arch x86_64` if you are staging Linux x86_64 container-side binaries.
On Apple Silicon, a native arm64 profile requires Linux arm64 builds of
`aw-container-agent` and `aw-container-bootstrap`.

Set Docker to use that profile:

```bash
export DOCKER_HOST="unix://$HOME/.colima/aw-gateway/docker.sock"
```

`aw-gateway` can also derive this value from:

```toml
[runtime]
type = "colima"
profile = "aw-gateway"
```

## Local Layout

Use a user-owned local root. The examples use `/Users/example/aw-gateway`; in a
real setup replace that with your path, for example `$HOME/aw-gateway`.

```text
~/aw-gateway/
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
  workspace/
```

Install the macOS host binary and Linux container-side binaries:

```bash
install -d -m 0755 ~/aw-gateway/bin ~/aw-gateway/runtime/linux
install -m 0755 target/release/aw-gateway ~/aw-gateway/bin/aw-gateway
install -m 0755 /path/to/linux/aw-container-agent ~/aw-gateway/runtime/linux/aw-container-agent
install -m 0755 /path/to/linux/aw-container-bootstrap ~/aw-gateway/runtime/linux/aw-container-bootstrap
install -m 0755 /path/to/linux/aw-ssh-command-filter ~/aw-gateway/runtime/linux/aw-ssh-command-filter
install -m 0755 examples/colima/start-container-sshd ~/aw-gateway/runtime/linux/start-container-sshd
install -m 0644 examples/colima/sshd_config_agent ~/aw-gateway/runtime/linux/sshd_config_agent
```

## Minimal Ubuntu Image

Full file: `examples/colima/Containerfile.ubuntu`

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

Build it through Colima Docker:

```bash
DOCKER_HOST="unix://$HOME/.colima/aw-gateway/docker.sock" \
docker build \
  -t aw-gateway/ubuntu-base:latest \
  -f examples/colima/Containerfile.ubuntu \
  examples/colima
```

## Container SSH Files

Full file: `examples/colima/start-container-sshd`. The helper copies the
mounted base SSHD config to a runtime file, applies `[target_defaults.container_ssh]` transfer
policy, validates it with `sshd -t`, and starts container `sshd`.

Full file: `examples/colima/sshd_config_agent`

```sshconfig
Port 22
ListenAddress 0.0.0.0
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

## Gateway Config

Full file: `examples/colima/gateway-local.toml`

Important Colima-specific fields:

```toml
[runtime]
type = "colima"
profile = "aw-gateway"

[targets.ubuntu.local_ssh]
mode = "listen"
backend = "published_port"
readiness = "ssh_only"
host = "127.0.0.1"

[targets.ubuntu.runtime]
extra_run_args = [
  "--cap-add", "SYS_ADMIN",
  "--security-opt", "seccomp=unconfined",
  "--security-opt", "apparmor=unconfined",
]

[targets.ubuntu.container_env]
CODEX_HOME = "/var/lib/codex"

[targets.ubuntu.session_env]
CODEX_HOME = "/var/lib/codex"

[targets.ubuntu.identity]
bootstrap_user = "root"
session_user = "{user}"
session_uid = "{uid}"
session_gid = "{gid}"
session_home = "/home/{user}"
session_shell = "/bin/bash"

[target_defaults.container_agent]
enabled = true
control_socket = false
```

The published-port backend avoids relying on a Unix-domain socket crossing the
macOS-to-Colima-VM boundary. Docker publishes container port 22 to a random
loopback-only backend port on macOS, and the gateway's stable local listener
proxies to that runtime-reported backend. Inside the container, `sshd` listens
on `0.0.0.0:22` so Docker can reach it through the container network namespace;
Docker still binds the host-side backend port to `127.0.0.1`, and the SSH
client talks to the gateway listener, not directly to the container.

```text
ssh aw-ubuntu
  -> 127.0.0.1:<gateway listener port>
  -> 127.0.0.1:<Docker published backend port>
  -> container:22
```

`control_socket = false` tells `aw-container-agent` to supervise services
without creating an unused Unix control socket. This avoids creating Linux
socket files on macOS bind mounts while keeping `container-sshd` managed by the
agent.

The identity block starts bootstrap as root, then creates the session login
from the host user, UID, and GID macros. That makes `ssh aw-ubuntu` land in the
container as the same numeric identity that owns the bind-mounted workspace.

The runtime `extra_run_args` shown above allow bubblewrap-based tools to create
the namespaces they need under Docker/Colima. Those flags relax the container
sandbox, so keep them target-specific and remove them for images that do not
need bubblewrap. `CODEX_HOME` is set to `/var/lib/codex` so Codex app-server
state stays on container-local storage instead of the macOS bind-mounted home.

The sample uses `/Users/example/aw-gateway` as a placeholder. Replace every
`/Users/example/aw-gateway` path with your real local root before installing.

Install and validate:

```bash
install -d -m 0755 ~/aw-gateway/etc
sed "s#/Users/example/aw-gateway#$HOME/aw-gateway#g" \
  examples/colima/gateway-local.toml > ~/aw-gateway/etc/gateway.toml
~/aw-gateway/bin/aw-gateway --config ~/aw-gateway/etc/gateway.toml config validate
```

## Run

Start the target and local listener:

```bash
~/aw-gateway/bin/aw-gateway \
  --config ~/aw-gateway/etc/gateway.toml \
  up ubuntu --json
```

Keep the `up` process running while local SSH-compatible clients use the
generated config.

Install your workstation public key into the container authorized-key file:

```bash
~/aw-gateway/bin/aw-gateway \
  --config ~/aw-gateway/etc/gateway.toml \
  add-container-key ubuntu \
  --public-key ~/.ssh/id_ed25519.pub
```

Then use the generated alias:

```bash
ssh aw-ubuntu
scp ./file.txt aw-ubuntu:~/file.txt
sftp aw-ubuntu
```

For VS Code Remote SSH, select the generated `aw-ubuntu` host alias.

## Colima Constraints

Earlier Colima experiments used VM-level firewall and proxy setup. Those
deployment-specific hooks are intentionally not part of this basic guide.

Durable conclusions kept here:

- Colima is treated as Docker with a profile-derived `DOCKER_HOST`.
- Use a dedicated profile such as `aw-gateway`.
- Match container-side binary architecture to the Colima VM architecture.
- Prefer `published_port` for local SSH because host-visible Unix-domain socket
  paths do not naturally cross from macOS into the Colima VM.
- Disable `target_defaults.container_agent.control_socket` for published-port `ssh_only`
  targets unless another host-visible control channel is configured.
- Keep persistent workspace and gateway state under a user-owned local root.

## Network Policy Hooks

Do not put proxy, CA, or firewall setup in the basic Colima guide. If a
deployment needs policy, add site-managed lifecycle or host steps and document
the concrete commands using [Firewall Policy](firewall.md) and
[Proxy And CA Policy](proxy.md).

## Validation Checklist

```bash
~/aw-gateway/bin/aw-gateway --config ~/aw-gateway/etc/gateway.toml config validate
~/aw-gateway/bin/aw-gateway --config ~/aw-gateway/etc/gateway.toml status ubuntu
ssh aw-ubuntu 'pwd && id && hostname'
scp ./README.md aw-ubuntu:~/README.md
sftp aw-ubuntu
```

If ownership looks wrong, confirm the target identity UID/GID settings and the
Colima profile architecture before adding proxy or policy layers.
