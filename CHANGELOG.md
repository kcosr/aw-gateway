# Changelog

## [Unreleased]

### Breaking Changes

- Replaced `target_includes` and `launch_includes` with a unified `includes`
  config array for target/launch templates and concrete definitions
  ([#10](https://github.com/kcosr/aw-gateway/pull/10)).
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
- Renamed `[ssh_dispatch].enabled_gateway_actions` to
  `[ssh_dispatch].enabled_actions`. The retired key is rejected during config
  validation
  ([#11](https://github.com/kcosr/aw-gateway/pull/11)).

### Added

- Added a live smoke test suite for Docker/Ubuntu, rootless Podman/Rocky 10,
  and macOS/Colima hosts, covering unrestricted SSH, restricted forced-command
  SSH, host-local container SSH, lifecycle operations, and the JSON HTTP API
  ([#15](https://github.com/kcosr/aw-gateway/pull/15)).
- Added live SCP/SFTP transfer-policy smoke coverage for default SFTP-backed
  `scp`, legacy `scp -O`, and upload/download policy variants
  ([#16](https://github.com/kcosr/aw-gateway/pull/16)).
- Added an initial JSON HTTP API daemon with explicit `[http]` config,
  optional bearer auth, HTTP action allow-listing, metadata routes, wait/detach
  run and launch execution, typed JSON launch variables, and stable JSON
  success/error envelopes
  ([#14](https://github.com/kcosr/aw-gateway/pull/14)).
- Added nested unified config includes for `[target_templates.<name>]`,
  `[launch_templates.<name>]`, `[targets.<name>]`, and `[launches.<name>]`,
  with relative path resolution, sorted glob expansion, cycle detection, and
  duplicate definition rejection
  ([#10](https://github.com/kcosr/aw-gateway/pull/10)).
- Added named `[target_templates.<name>]` and `[launch_templates.<name>]`
  config sections with ordered `use = [...]` composition for concrete targets,
  concrete launches, and same-kind nested templates
  ([#9](https://github.com/kcosr/aw-gateway/pull/9)).
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
- Normalized CLI and SSH management actions through shared operation handling
  for target discovery, status, launch discovery, launch execution, lifecycle
  actions, default selection, and client config rendering
  ([#11](https://github.com/kcosr/aw-gateway/pull/11)).
- Lifecycle and host hook commands now use a finite `60s` default timeout when no per-step `timeout` is configured ([#2](https://github.com/kcosr/aw-gateway/pull/2)).

### Fixed

- Allowed container bootstrap to use an existing group with the requested
  session gid when creating the session passwd entry, which supports macOS
  users whose primary gid collides with a default Linux group name
  ([#15](https://github.com/kcosr/aw-gateway/pull/15)).
- Removed Ubuntu's default `ubuntu` account from the Podman and Colima example
  images so keep-id/user provisioning can create the configured session user
  without a UID collision
  ([#15](https://github.com/kcosr/aw-gateway/pull/15)).
- Preserved live control socket paths when reusing already-running containers,
  while still removing stale paths before stopped or missing container startup
  ([#12](https://github.com/kcosr/aw-gateway/pull/12)).

## [0.1.0] - 2026-05-21

### Added

- Initial `aw-gateway` release.
