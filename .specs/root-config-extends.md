# Root Config Extends and Semantic Merge

## Problem

Per-user config lookup lets a user select a private root config by default, but a
private root config still has to repeat root-level settings such as `[runtime]`,
`[logging]`, `[ssh_dispatch]`, `[client_config]`, and `includes`.

For common customizations, the user should be able to inherit the managed system
config and override or add only the parts they own. Example: add one supervised
service to `rocky8-sip` for one user without editing `/etc/aw-gateway`.

The existing `includes` feature is not meant for this. Includes compose named
targets, templates, launches, and launch templates. They intentionally reject
root-only sections and duplicate target names.

## Design

Add a root-level `extends` field:

```toml
extends = "/etc/aw-gateway/gateway.toml"
```

When present, config loading should:

1. Load the base config path as raw TOML.
2. Load the extending config as raw TOML.
3. Merge the extending TOML over the base TOML.
4. Parse includes from the final merged root config.
5. Parse and validate the final composed gateway config normally.

The selected config file remains the root config. `extends` is inheritance for
that selected root config, not implicit discovery and not an include alias.

## Merge Rules

Merge behavior should be deterministic and schema-aware enough to avoid
surprising list appends.

| TOML value | Merge rule |
| --- | --- |
| Tables | Deep merge by key. |
| Scalars | Extending config replaces base. |
| Arrays of scalars | Extending config replaces base. |
| Arrays of tables with stable `name` fields | Merge by `name`. |
| Arrays of tables without stable `name` fields | Extending config replaces base. |

Named arrays should merge by `name` for these existing config lists:

- `container_agent.services`
- `lifecycle_steps`
- `host_steps`
- `container_bootstrap_steps`
- `launch.steps`

Arrays that should replace by default:

- `includes`
- `ssh_dispatch.enabled_actions`
- `http.enabled_actions`
- `target.use`
- `launch.use`
- `container_mounts`
- service `command`
- service `depends_on`
- `idle_cleanup.preserve_processes`
- `runtime.extra_run_args`
- `launch.command`
- launch variable `values`
- health-check command arrays

`container_mounts` should not merge until mounts have an explicit stable key,
for example a future optional `name` field. Merging unnamed mounts by `target` or
`source` would be too easy to get wrong.

## Example

User config:

```toml
extends = "/etc/aw-gateway/gateway.toml"

[[targets.rocky8-sip.container_agent.services]]
name = "agent-runner"
required = false
user = "{container_user}"
cwd = "{container_home}/git/agent-runner"
command = ["agent-runner", "serve"]
restart = "always"
depends_on = ["acl-proxy"]
```

If the base config defines `rocky8-sip` through the managed templates, the final
target contains inherited services:

- `acl-proxy`
- `container-sshd`

and the user-added service:

- `agent-runner`

Dependency validation runs after the final merge, so `depends_on =
["acl-proxy"]` resolves against the inherited service.

## Cycles and Safety

- Reject `extends` cycles.
- Resolve relative `extends` paths relative to the file that declares them.
- Allow only one inheritance chain for a selected config file; do not support
  multiple base configs in `extends`.
- Produce clear errors that include both the extending path and base path.
- Keep `includes` cycle detection unchanged after the merged root config is
  produced.

## Non-Goals

- Do not use `extends` as a replacement for `includes`.
- Do not append scalar arrays by default.
- Do not silently merge duplicate include-defined targets. Include duplicate
  detection should remain strict.
- Do not add mount merging until mounts have stable names.
