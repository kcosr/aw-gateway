# Changelog

## [Unreleased]

### Breaking Changes

- Gateway configs now reject direct `container_agent.control_socket = "/path"`
  and `container_agent.ssh_bridge.socket = "/path"` socket path settings; use
  `[control_sockets]` for gateway-managed runtime socket directories
  ([#6](https://github.com/kcosr/aw-gateway/pull/6)).

### Added

- Added target-level `workspace_cleanup` config for best-effort cleanup of
  resolved ephemeral session workspaces after success or after any outcome
  ([#7](https://github.com/kcosr/aw-gateway/pull/7)).
- Added `[control_sockets]` and `[targets.<name>.control_sockets]` config for
  short runtime-only gateway control and SSH bridge socket directories
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
