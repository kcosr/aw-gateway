# aw-gateway Smoke Test Scenarios

This document describes the live smoke scenarios implemented under
`smoke/tests/`. The suite is controller-driven: pytest runs on the controller,
uses SSH to operate each configured host, and verifies `aw-gateway` against
Docker, rootless Podman, and Colima.

## Host And Access Matrix

| Inventory host | SSH alias | Runtime | Install layout | Covered access paths |
| --- | --- | --- | --- | --- |
| `ubuntu` | `ubuntu` | Docker | `/opt/aw-gateway` | operator SSH, restricted SSH, host-local container SSH, HTTP API tunnel |
| `rocky10` | `rocky10` | rootless Podman | `/home/kevin/aw-gateway` | operator SSH, restricted SSH, host-local container SSH, HTTP API tunnel |
| `macos-colima` | `mac` | Colima/Docker | `/Users/kevin/aw-gateway` | operator SSH, host-local container SSH, HTTP API tunnel |

Linux restricted access uses an `awsmoke` user in the `aw-gateway-users`
group. Host `sshd` applies a `ForceCommand` that invokes `aw-gateway`, matching
the repository's restricted SSH example pattern. The deploy helper validates
the generated `sshd` config every run and reloads `sshd` only when that managed
snippet changes.

macOS coverage focuses on the Colima runtime and home-directory install. It
does not currently configure a restricted `ForceCommand` user because that is
not the expected primary deployment mode for Mac hosts.

## Readiness

Implemented in `tests/test_00_readiness.py`.

| Scenario | Hosts |
| --- | --- |
| SSH alias accepts non-interactive `BatchMode` connections | all enabled hosts |
| Passwordless `sudo -n true` works where the inventory marks sudo as required | `ubuntu`; any other host with `requires_sudo = true` |
| Runtime CLI is available to the configured host user | all enabled hosts |
| `git` is available for remote build and deployment tasks | all enabled hosts |

## Gateway Lifecycle

Implemented in `tests/test_10_lifecycle.py`.

| Scenario | Hosts |
| --- | --- |
| Deployed gateway config validates with `config validate` | all enabled hosts |
| `config paths --json` reports the deployed explicit config and checked paths | all enabled hosts |
| Gateway target listing succeeds and includes the smoke target | all enabled hosts |
| `run <target> -- id` starts or reuses the container and executes inside it | all enabled hosts |
| Status JSON reports the target after execution | all enabled hosts |
| Stop succeeds and leaves the target in a stopped or absent state | all enabled hosts |

## Host-Local Container SSH

Implemented in `tests/test_15_host_local_container_ssh.py`.

These tests validate the path where the controller SSHes into the host, the
host runs `aw-gateway`, and the host then SSHes into the container through a
generated client bundle. This is the primary Mac/Colima path and is also
covered on the Linux hosts.

| Scenario | Hosts |
| --- | --- |
| Local gateway config validates | all enabled hosts |
| `add-container-key` installs a controller key for container SSH | all enabled hosts |
| `client-config` generates a usable host-local SSH bundle | all enabled hosts |
| Host-local `ssh -F <bundle> aw-<target> id` executes inside the container | all enabled hosts |

## Container SSH Transfer Policy

Implemented in `tests/test_25_transfer_policy.py`.

These tests use the same host-local generated client bundle as the container
SSH smoke test. Each case rewrites a temporary host config with a specific
`[container_ssh.transfer]` policy, starts the target, and verifies upload and
download behavior for SFTP, default OpenSSH `scp`, and legacy `scp -O`.

| `sftp` | `legacy_scp` | Expected SFTP | Expected default `scp` | Expected legacy upload | Expected legacy download |
| --- | --- | --- | --- | --- | --- |
| `allow` | `allow` | allow | allow | allow | allow |
| `allow` | `deny` | allow | allow | deny | deny |
| `deny` | `allow` | deny | deny | allow | allow |
| `allow` | `inbound` | allow | allow | allow | deny |
| `allow` | `outbound` | allow | allow | deny | allow |
| `deny` | `deny` | deny | deny | deny | deny |

The default `scp` expectation assumes modern OpenSSH behavior, where default
`scp` uses the SFTP protocol. Legacy direction checks use `scp -O`: upload into
the container maps to `legacy_scp = "inbound"`, and download from the container
maps to `legacy_scp = "outbound"`.

## Restricted SSH Users

Implemented in `tests/test_20_restricted_user.py`.

| Scenario | Hosts |
| --- | --- |
| Restricted user can invoke gateway help through host `sshd` `ForceCommand` | `ubuntu`, `rocky10` |
| Restricted user can list gateway targets | `ubuntu`, `rocky10` |
| Restricted user can run a container command through the gateway | `ubuntu`, `rocky10` |

The restricted-user tests are Linux-only. The inventory still includes
`macos-colima`, but those cases are skipped by design.

## HTTP API Transport

Implemented in `tests/test_30_http_api.py`.

The HTTP listener remains bound to host loopback. Each HTTP test writes a
temporary remote config with a unique `127.0.0.1:<port>` listener, starts
`aw-gateway http` over SSH, opens an SSH local forward, and sends controller
HTTP requests to the forwarded local port.

| Scenario | Hosts |
| --- | --- |
| Missing and incorrect bearer tokens return `401 unauthorized` | all enabled hosts |
| Unknown API routes return the stable JSON error envelope | all enabled hosts |
| `GET /api/v1/targets` lists the configured target | all enabled hosts |
| `GET /api/v1/status?target=<target>` returns target status | all enabled hosts |
| `GET /api/v1/status/all` returns a status list | all enabled hosts |
| `POST /api/v1/up` starts supported targets, with expected unsupported-operation behavior for local-listen Colima targets | all enabled hosts |
| `POST /api/v1/run` supports wait mode, output stream selection, nonzero exit reporting, and detach mode | all enabled hosts |
| Invalid run requests return stable validation errors | all enabled hosts |
| `GET /api/v1/launches` and launch detail routes expose launch metadata | all enabled hosts |
| `POST /api/v1/launches/smoke-echo/run` validates typed variables and executes | all enabled hosts |
| Unknown launches and invalid launch variables return stable errors | all enabled hosts |
| A limited HTTP config blocks disabled actions with `403 disabled_action` | all enabled hosts |

## Cleanup, Timeouts, And Reaping

Implemented in `tests/test_35_cleanup_timeouts.py`.

These tests write temporary remote configs derived from each host's deployed
host-local config, with idle grace and poll intervals reduced to one or two
seconds. The deployed smoke configs are not mutated.

| Scenario | Hosts |
| --- | --- |
| Gateway-owned `exit_container` idle cleanup stops the target after a short grace period | all enabled hosts |
| Agent-owned `exit_container` idle cleanup stops the target after a short grace period | `ubuntu`, `rocky10` |
| Agent-owned cleanup preserves the container while a `tmux`-named process appears in the container process table | `ubuntu`, `rocky10` |
| Agent-owned `reap_processes` remains paused while `tmux` is preserved, then terminates an unpreserved session process after the preserve process exits | `ubuntu`, `rocky10` |
| Ephemeral target `workspace.cleanup = "always"` removes the session workspace after a successful command | all enabled hosts |

The preserve-process test starts a short-lived executable whose process
`comm` is `tmux`; this exercises the same process-table matching path as a
real tmux session without requiring package installation in the base smoke
image.

## Not Yet Covered

The current suite does not yet exercise every gateway behavior. The following
areas are intentionally left out of the first in-repo smoke harness:

- Controller-side generated SSH config use from the controller into Linux
  remote hosts.
- Concurrent lifecycle requests and stronger idempotence checks.
- Failure-diagnostics cases such as missing runtime binaries, missing images,
  bad lifecycle steps, and copied-back host logs.
