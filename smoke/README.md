# aw-gateway Smoke Tests

This directory contains live, controller-driven smoke tests for `aw-gateway`.
These tests are intentionally separate from `cargo test`: they deploy the
current checkout to real hosts, build or refresh target images, and then
exercise the gateway against Docker, rootless Podman, and Colima. A host can be
driven through SSH or through local process execution on the machine running the
harness.

Scenario coverage: [SCENARIOS.md](SCENARIOS.md).

## Scope

The controller is this machine. Tests run from here and drive each configured
host through its inventory `transport`, then exercise `aw-gateway` on that
host.

Current hosts:

| Inventory host | Transport | Runtime | Primary install |
| --- | --- | --- | --- |
| `ubuntu` | SSH alias `ubuntu` | Docker | `/opt/aw-gateway` |
| `rocky10` | SSH alias `rocky10` | rootless Podman | `/home/kevin/aw-gateway` |
| `macos-colima` | SSH alias `mac` | Colima/Docker | `/Users/kevin/aw-gateway` |
| `macos-apple-container` | local process | Apple Container | generated temp install |

## Access Modes

The shared baseline is host-local gateway behavior on every platform:

1. The harness runs a shell command through the configured host transport.
2. It runs that host's installed `aw-gateway`.
3. It verifies container lifecycle and SSH into the container from that host.

For `transport = "local"` hosts, step 1 is replaced with a local shell command
on the same machine. This is intended for environments where another launcher,
such as a build service, has already arranged to execute the smoke harness on
the target host.

Linux hosts also cover restricted OpenSSH `ForceCommand` users:

```text
ssh awsmoke@ubuntu ...
ssh awsmoke@rocky10 ...
```

Mac restricted `sshd` gateway behavior is intentionally not the primary case;
Mac coverage focuses on Colima over SSH and Apple Container through a local
process transport on the Mac host.

The HTTP API smoke path keeps the gateway listener loopback-only on each host.
For every HTTP test, the harness:

1. Copies the deployed config to a temporary host config with a unique
   `127.0.0.1:<port>` HTTP listener.
2. Starts `aw-gateway --config <temp> http` on the host.
3. For SSH hosts, opens an SSH local forward from the controller to that
   host-local port. Local hosts connect directly to the loopback listener.
4. Sends JSON HTTP requests from the controller to the selected local port.

## Harness Commands

Run these commands from `smoke/`.

Create and use the virtualenv:

```bash
python3 -m venv .venv
.venv/bin/python -m pip install -e .
```

Create a local inventory:

```bash
cp inventory.example.toml inventory.toml
```

`inventory.toml` is ignored because it contains local host aliases and install
paths. The committed example expects SSH aliases named `ubuntu`, `rocky10`, and
`mac`. Hosts default to `transport = "ssh"`; set `transport = "local"` for a
host whose smoke commands should run directly on the same machine as the
harness.

List hosts:

```bash
.venv/bin/awsmoke hosts
```

Deploy a host:

```bash
.venv/bin/awsmoke deploy ubuntu
.venv/bin/awsmoke deploy rocky10 --skip-build
.venv/bin/awsmoke deploy macos-colima --skip-image
```

The example inventory points at the repository root:

```toml
repo_root = ".."
```

Refresh Linux restricted users:

```bash
.venv/bin/awsmoke setup-restricted ubuntu
.venv/bin/awsmoke setup-restricted rocky10
```

Restricted-user setup validates the generated `sshd` config on every run and
reloads `sshd` only when the managed `Match` snippet changes.

Run all enabled tests:

```bash
.venv/bin/python -m pytest -q
```

Run one host:

```bash
.venv/bin/python -m pytest --host macos-colima -q
```

## What Gets Installed

Linux operator layout:

```text
<install-root>/
  bin/aw-gateway
  etc/gateway.toml
  etc/gateway-local.toml
  etc/gateway-runtime-exec.toml
  etc/gateway-http-limited.toml
  runtime/linux/
    aw-container-agent
    aw-container-bootstrap
    aw-ssh-command-filter
    sshd_config_agent
    start-container-sshd
```

Restricted Linux users use separate home installs under
`/home/awsmoke/aw-gateway` so their ephemeral test configs do not overwrite the
operator config.

Mac Colima uses:

```text
/Users/kevin/aw-gateway
/Users/kevin/.local/bin/colima
/Users/kevin/.local/bin/limactl
/Users/kevin/.local/bin/docker
```

No Mac files are installed under `/opt`.

Apple Container smoke uses a generated local-transport inventory on the Mac.
The harness builds the native macOS `aw-gateway` binary, builds Linux arm64
container helper binaries inside an Apple Container Rust image, installs all
files under a temporary smoke install root, builds the Apple Container Ubuntu
image, and then runs the same pytest smoke scenarios against that host entry.

## Host-Specific Notes

`ubuntu` uses Docker with the production-style `/opt/aw-gateway` install.

`rocky10` uses rootless Podman from a user-owned install. Rootless Podman on
SELinux could not relabel root-owned `/opt/aw-gateway` bind mounts, so the
smoke layout uses `/home/kevin/aw-gateway` for the operator and
`/home/awsmoke/aw-gateway` for the restricted user. Runtime helper sources
come from that host install, but their in-container mount targets remain under
`/opt/aw-gateway` so helper mountpoints do not land inside the workspace
mounted at the session home.

`macos-colima` uses Colima profile `aw-gateway` and Docker socket:

```text
unix:///Users/kevin/.colima/aw-gateway/docker.sock
```

The rendered Mac config uses an absolute Docker CLI path and a macOS-safe
control socket directory:

```toml
[runtime]
program = "/Users/kevin/.local/bin/docker"

[target_defaults.control_sockets]
host_dir = "/Users/kevin/.cache/aw-gateway/sockets/{runtime_id}"
```

`macos-apple-container` requires Apple silicon macOS with Apple Container
installed and `container system start` already completed. It runs through
`transport = "local"` because the smoke harness is expected to execute on the
Mac itself, either directly or through a local build-service command.

## Current Coverage

The current suite covers:

- Host readiness.
- Runtime readiness.
- Config validation.
- Target listing.
- Clean fixed-target lifecycle.
- Runtime-exec access for config validation, command execution, shell
  execution, status access labels, and SSH-only operation rejection, including
  Apple Container runtime-exec when run on an Apple Container host.
- Host-local container SSH through generated client bundles.
- Container SSH transfer policy for `sftp`, default `scp`, and legacy
  `scp -O` upload/download paths.
- Linux restricted `ForceCommand` help, target listing, and `run`.
- HTTP API bearer auth, metadata, lifecycle, command execution, launch
  execution, wait-mode JSON output projection, validation errors, and action
  allow-listing over SSH tunnels.
- Short-timer cleanup behavior for gateway-owned idle stop, Linux agent-owned
  idle stop, preserve-process handling, process reaping, and ephemeral
  workspace cleanup, including interrupted foreground launch cleanup.

Additional coverage notes are tracked in [SCENARIOS.md](SCENARIOS.md).
