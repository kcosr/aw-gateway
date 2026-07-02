# Apple Container Deployment Guide

This guide shows a local macOS setup for `aw-gateway` using Apple's
`container` runtime.

> [!WARNING]
> Apple container support is experimental in `aw-gateway`. The deterministic
> Linux test suite covers config validation, command rendering, parser
> scaffolding, preflight checks, and published-port state behavior. Real
> Apple CLI fixtures and a full macOS smoke pass are still required before this
> runtime should be treated as production-ready.

Apple `container` runs Linux containers as lightweight virtual machines on an
Apple silicon Mac. This guide uses `local_ssh.backend = "published_port"` and
`readiness = "ssh_only"` because the phase-one gateway integration does not use
Apple socket publishing. The container agent supervises the inner SSH service,
but its control socket is disabled because the host gateway does not use an
agent control channel in this mode.

This guide intentionally does not configure traffic interception, custom CA
trust, command-policy wrappers, or proxy services. Those are site policy
add-ons. See [Firewall Policy](firewall.md) and
[Proxy And CA Policy](proxy.md) for reusable optional layers.

## Requirements

- Apple silicon Mac.
- macOS 26 or newer.
- Apple `container` CLI 1.0.0 or newer on the non-interactive shell `PATH`, or
  an explicit `[runtime].program`.
- Apple container system services started with `container system start`.
- macOS arm64 `aw-gateway` host binary.
- Linux arm64 container-side `aw-container-agent`,
  `aw-container-bootstrap`, and `aw-ssh-command-filter`.

Verify the runtime before starting the gateway:

```bash
container system start
container system version --format json
container system status --format json
```

`aw-gateway` runs the version and status checks before Apple runtime
operations. It does not run them during `config validate`, so Apple configs can
still be parsed on Linux CI hosts.

## Example Files

Copyable examples live under:

```text
examples/apple-container/
  Containerfile.ubuntu
  gateway-local.toml
  sshd_config_agent
  start-container-sshd
```

Full-file code blocks below match files in `examples/apple-container/` unless
the block is explicitly labeled as an excerpt.

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

Install the macOS host binary from the macOS archive and Linux container-side
runtime files from the Linux arm64 archive:

```bash
HOST_RELEASE_ROOT=/path/to/aw-gateway-VERSION-macos-arm64
LINUX_RELEASE_ROOT=/path/to/aw-gateway-VERSION-linux-arm64
install -d -m 0755 ~/aw-gateway/bin ~/aw-gateway/runtime/linux
install -m 0755 "$HOST_RELEASE_ROOT/bin/aw-gateway" ~/aw-gateway/bin/aw-gateway
install -m 0755 "$LINUX_RELEASE_ROOT/bin/aw-container-agent" \
  ~/aw-gateway/runtime/linux/aw-container-agent
install -m 0755 "$LINUX_RELEASE_ROOT/bin/aw-container-bootstrap" \
  ~/aw-gateway/runtime/linux/aw-container-bootstrap
install -m 0755 "$LINUX_RELEASE_ROOT/bin/aw-ssh-command-filter" \
  ~/aw-gateway/runtime/linux/aw-ssh-command-filter
install -m 0755 "$LINUX_RELEASE_ROOT/examples/apple-container/start-container-sshd" \
  ~/aw-gateway/runtime/linux/start-container-sshd
install -m 0644 "$LINUX_RELEASE_ROOT/examples/apple-container/sshd_config_agent" \
  ~/aw-gateway/runtime/linux/sshd_config_agent
```

## Minimal Ubuntu Image

Full file: `examples/apple-container/Containerfile.ubuntu`

```dockerfile
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN if id ubuntu >/dev/null 2>&1; then userdel ubuntu && rm -rf /home/ubuntu; fi

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        bubblewrap \
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
    && mkdir -p /opt/aw-gateway/bin /run/sshd /var/lib/codex

WORKDIR /root

CMD ["/bin/bash"]
```

Build it with Apple `container`:

```bash
container build \
  -t aw-gateway/ubuntu-base:latest \
  -f examples/apple-container/Containerfile.ubuntu \
  examples/apple-container
```

## Container SSH Files

Full file: `examples/apple-container/start-container-sshd`. The helper copies
the mounted base SSHD config to a runtime file, applies
`[target_defaults.container_ssh]` transfer policy, validates it with
`sshd -t`, and starts container `sshd`.

Full file: `examples/apple-container/sshd_config_agent`

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

Full file: `examples/apple-container/gateway-local.toml`

Important Apple-specific fields:

```toml
[runtime]
type = "apple_container"

[target_defaults.control_sockets]
host_dir = "/Users/example/aw-gateway/cache/sockets/{runtime_id}"
container_dir = "/run/aw-gateway"

[targets.ubuntu.local_ssh]
mode = "listen"
backend = "published_port"
readiness = "ssh_only"
host = "127.0.0.1"

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

The published-port backend lets the gateway allocate a loopback host TCP port
before `container run`, pass it as `--publish 127.0.0.1:<port>:22/tcp`, and
persist that port in the target state directory for reuse. The SSH client talks
to the gateway's stable local listener, not directly to the Apple-published
backend port.

```text
ssh aw-ubuntu
  -> 127.0.0.1:<gateway listener port>
  -> 127.0.0.1:<Apple published backend port>
  -> container:22
```

`control_socket = false` tells `aw-container-agent` to supervise services
without creating an unused Unix control socket. The phase-one Apple integration
rejects `local_ssh.backend = "socket"`.

The identity block starts bootstrap as root, then creates the session login
from the host user, UID, and GID macros. If the macOS numeric uid or gid
collides with an existing Linux account or group in your base image, set
explicit in-container values instead:

```toml
[targets.ubuntu.identity]
session_user = "{user}"
session_uid = "24501"
session_gid = "24501"
session_home = "/home/{user}"
session_shell = "/bin/bash"
```

The sample uses `/Users/example/aw-gateway` as a placeholder. Replace every
`/Users/example/aw-gateway` path with your real local root before installing.

Install and validate:

```bash
install -d -m 0755 ~/aw-gateway/etc
sed "s#/Users/example/aw-gateway#$HOME/aw-gateway#g" \
  examples/apple-container/gateway-local.toml > ~/aw-gateway/etc/gateway.toml
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

## Apple Container Constraints

- Use Apple silicon and macOS 26 or newer.
- Run the gateway process on the Apple silicon macOS host. The Apple runtime is
  not supported from WSL, Linux, or a remote non-macOS gateway host.
- Run `container system start` before runtime operations.
- Use `local_ssh.backend = "published_port"`; socket backend is not supported
  in phase one.
- Do not configure Docker/Colima-specific `target.runtime.extra_run_args`.
  Runtime extra args are passed directly to `container run` and are not
  portable across runtimes.
- Container creation uses Apple `container run --env KEY` host inheritance for
  gateway-provisioned run environment where Apple documents that form, so
  values such as identity tokens are not placed in the runtime argv. Apple
  `container exec` currently documents only `KEY=value` env entries; avoid
  putting secrets in Apple `session_env`, launch env, or launch step env until
  an env-file exec path is validated.
- Apple `inspect` output may not expose a host PID. Gateway templates that
  require `{container_pid}` fail clearly when the runtime does not provide it.
- Fixed-target stop/start reuse depends on Apple preserving the published host
  port. The gateway persists the selected port and reports remove/recreate
  guidance if a running target lacks the persisted state.
- Read-only bind mount enforcement is load-bearing for gateway-mounted helper
  files. Validate it on your installed `container` version before relying on
  the runtime operationally.

## Validation Checklist

```bash
container system version --format json
container system status --format json
~/aw-gateway/bin/aw-gateway --config ~/aw-gateway/etc/gateway.toml config validate
~/aw-gateway/bin/aw-gateway --config ~/aw-gateway/etc/gateway.toml status ubuntu
ssh aw-ubuntu 'pwd && id && hostname'
scp ./README.md aw-ubuntu:~/README.md
sftp aw-ubuntu
```

Before production use, also run the repository fixture capture script and the
macOS smoke checklist from the Apple runtime support spec. Those steps confirm
the installed Apple CLI's JSON shapes, read-only mount behavior, bind-conflict
stderr, and fixed-target published-port reuse semantics.
