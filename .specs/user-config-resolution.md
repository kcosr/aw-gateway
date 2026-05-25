# User Config Resolution and Path Symmetry

## Problem

`aw-gateway` currently selects the host gateway config from only three sources:

1. `--config`
2. `AW_GATEWAY_CONFIG`
3. `/etc/aw-gateway/gateway.toml`

This works for managed hosts, but it makes per-user overrides awkward. A user who
has a normal host shell may want a private target or launch config without
changing the system config or always passing `--config`.

At the same time, the code already has XDG-aware user config and state directory
helpers, but the default gateway config lookup does not use the user config dir.
The resulting path model is not symmetrical.

## Design

Add a deterministic user config lookup before the system config fallback. Do not
merge configs implicitly. The selected file is the one gateway config to load.

Gateway config file precedence:

1. `--config <path>`
2. `AW_GATEWAY_CONFIG`
3. User config file, if present: `{user_config_dir}/gateway.toml`
4. System config file, if present: `/etc/aw-gateway/gateway.toml`
5. Fail with an error listing the paths that were checked.

User config and state directory resolution should be symmetrical:

| Concept | Resolution |
| --- | --- |
| User config dir | `AW_GATEWAY_CONFIG_HOME`, else `XDG_CONFIG_HOME/aw-gateway`, else `~/.config/aw-gateway` |
| User state dir | `AW_GATEWAY_STATE_HOME`, else `XDG_STATE_HOME/aw-gateway`, else `~/.local/state/aw-gateway` |
| Default user config file | `{user_config_dir}/gateway.toml` |
| System config file | `/etc/aw-gateway/gateway.toml` |

`~` comes from the effective user's passwd entry, matching the existing
`UserContext::current()` behavior.

## Non-Goals

- Do not implicitly merge user and system configs.
- Do not automatically include `/etc/aw-gateway` from a user config.
- Do not change container workspace state resolution. Target workspace state
  remains config-driven through `[workspace] path` and `state_dir`.
- Do not move the generated identity token as part of this change.

## Security and Operations

Restricted SSH deployments should keep passing an explicit system config in
`ForceCommand` when per-user overrides are not allowed:

```sshconfig
ForceCommand /opt/aw-gateway/bin/aw-gateway --config /etc/aw-gateway/gateway.toml
```

Unrestricted users running `aw-gateway` directly get their own config by default
when `~/.config/aw-gateway/gateway.toml` exists.

The identity token should remain in the user config dir because it is durable
identity/credential material, not disposable runtime state.

## Diagnostics

Add or plan a diagnostic command such as:

```bash
aw-gateway config paths
```

It should print:

- Resolved effective user and home.
- User config dir.
- User state dir.
- User config file path and whether it exists.
- System config file path and whether it exists.
- Final selected gateway config path.

This is not required for the first implementation, but it should be included if
the change becomes hard to support operationally.
