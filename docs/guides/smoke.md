# Smoke Test Harness

The smoke harness lives in [`smoke/`](../../smoke). It is an opt-in live test
suite for real remote hosts and is not part of normal `cargo test` execution.

The harness deploys the current checkout over SSH, refreshes the remote
gateway install and container image, then runs pytest scenarios against these
access paths:

- controller SSH invoking the host gateway;
- host-local SSH from the remote host into the managed container;
- Linux restricted OpenSSH `ForceCommand` users;
- JSON HTTP API requests through SSH local forwards.

Restricted-user deployment validates the generated `sshd` config on every run
and reloads `sshd` only when the managed `Match` snippet changes.

Use the committed `smoke/inventory.example.toml` as a starting point:

```bash
cd smoke
python3 -m venv .venv
.venv/bin/python -m pip install -e .
cp inventory.example.toml inventory.toml
.venv/bin/awsmoke hosts
```

`smoke/inventory.toml`, generated configs, and virtualenv artifacts are ignored
because they are local-environment state. The example inventory uses SSH
aliases `ubuntu`, `rocky10`, and `mac`.

Run the full enabled suite:

```bash
cd smoke
.venv/bin/python -m pytest -q
```

Run one host:

```bash
cd smoke
.venv/bin/python -m pytest --host macos-colima -q
```

The HTTP tests keep the remote HTTP listener on host loopback. For each test,
the harness creates a temporary remote config with a unique HTTP port, starts
`aw-gateway http` over SSH, and sends controller-side HTTP requests through an
SSH local forward. No firewall changes are required.

See `smoke/README.md` for setup details and `smoke/SCENARIOS.md` for the
implemented coverage matrix.
