# Changelog

## [Unreleased]

### Breaking Changes

- Gateway configs now reject direct `container_agent.control_socket = "/path"`
  and `container_agent.ssh_bridge.socket = "/path"` socket path settings; use
  `[target_defaults.control_sockets]` or `[targets.<name>.control_sockets]`
  for gateway-managed runtime socket directories
  ([#6](https://github.com/kcosr/aw-gateway/pull/6)).
- Replaced root target-behavior config sections with target-shaped
  `[target_defaults]` sections. Use `[target_defaults.workspace]`,
  `[target_defaults.control_sockets]`, `[[target_defaults.lifecycle_steps]]`,
  `[[target_defaults.host_steps]]`, `[[target_defaults.container_mounts]]`,
  `[target_defaults.container_bootstrap]`,
  `[[target_defaults.container_bootstrap_steps]]`,
  `[target_defaults.container_agent]`, and
  `[target_defaults.container_ssh]` instead of the former root sections.
  ([#8](https://github.com/kcosr/aw-gateway/pull/8)).
- Replaced flat target workspace fields with nested
  `[targets.<name>.workspace]` fields. Use `path`, `state_dir`, and `cleanup`
  under the nested table instead of `workspace` and `workspace_cleanup`
  ([#8](https://github.com/kcosr/aw-gateway/pull/8)).

### Added

- Added named `[target_templates.<name>]` and `[launch_templates.<name>]`
  config sections with ordered `use = [...]` composition for concrete targets,
  concrete launches, and same-kind nested templates.
- Added explicit `[target_defaults]` and `[launch_defaults]` config sections
  that overlay into concrete targets and launches before validation
  ([#8](https://github.com/kcosr/aw-gateway/pull/8)).
- Added target-level `workspace.cleanup` config for best-effort cleanup of
  resolved ephemeral session workspaces after success or after any outcome
  ([#7](https://github.com/kcosr/aw-gateway/pull/7)).
- Added `[target_defaults.control_sockets]` and
  `[targets.<name>.control_sockets]` config for short runtime-only gateway
  control and SSH bridge socket directories
  ([#6](https://github.com/kcosr/aw-gateway/pull/6)).
- Added explicit `--session-id` support to `connect`, `run`, and executable
  `launch` commands, including matching SSH-dispatch parsing for deterministic
  ephemeral target sessions
  ([#5](https://github.com/kcosr/aw-gateway/pull/5)).
- Added named launches with strict launch/includes config, typed `{var.<name>}`
  variables, discovery/detail CLI output, post-ready setup steps, launch
  execution, and minimal launch provenance in status surfaces
  ([#3](https://github.com/kcosr/aw-gateway/pull/3)).
- Added configurable per-step `timeout` values for `lifecycle_steps` and `host_steps` ([#2](https://github.com/kcosr/aw-gateway/pull/2)).

### Changed

- Gateway-managed container-agent control and SSH bridge sockets now live in
  the configured runtime socket directory instead of durable workspace state
  ([#6](https://github.com/kcosr/aw-gateway/pull/6)).
- Lifecycle and host hook commands now use a finite `60s` default timeout when no per-step `timeout` is configured ([#2](https://github.com/kcosr/aw-gateway/pull/2)).

## [0.1.0] - 2026-05-21

### Added

- Initial `aw-gateway` release.
