# Changelog

## [Unreleased]

### Changed

- Restrictive container SSH transfer policies now install the command filter
  when either SFTP or legacy SCP is restricted, and reject shell-composed
  commands before they can bypass denied SFTP/SCP server invocations.
- HTTP wait-mode command output, container-agent control responses, and HTTP
  health-probe responses are now size bounded; truncated wait streams are
  reported with `output_truncated`.
- Enabled HTTP listeners now require bearer auth when `http.listen` is
  non-loopback.
- Container-agent mutating control-socket methods now fail closed when no
  control token is configured, and control-token checks use shared
  constant-time comparison.
- Container-agent startup now rejects malformed `AW_AUTHENTICATED_UID` /
  `AW_AUTHENTICATED_GID` values instead of silently disabling peer validation.
- Command health checks now run with a configurable timeout, defaulting to
  `5s`, and timed command stderr draining is bounded after process exit.
- Container-agent service restarts now keep backing off for immediate crash
  loops, and service shutdown no longer holds the child lock while waiting.
- Container-agent SSH bridge and control socket accept loops now continue after
  transient accept errors and enforce fixed connection/session limits.
- File logging now creates private log directories/files, rejects
  `logging.max_files` above `1024`, and redacts HTTP bearer tokens from Debug
  output.
- Private file writes now use atomic replace semantics, and existing control
  token files must have private permissions before they are trusted.
- Runtime-provided container and exec environment values are now passed through
  the spawned container-runtime process environment instead of embedding their
  values in `podman`/`docker` argv.
- In-container cancel marker files for cancelable exec/PTY sessions are now
  created with owner-only permissions.
- Session marker reads now tolerate pre-launch-field marker JSON, and marker
  liveness checks use the same self-process fast path as listener status.

## [0.6.0] - 2026-06-10

### Added

- Added delegated runtime context declarations, CLI/HTTP context handoff,
  context labels/session metadata, context-aware status output, and
  fail-closed lifecycle filtering for scoped containers
  ([#57](https://github.com/kcosr/aw-gateway/pull/57)).

### Changed

- Documented single-platform release archives and mixed host/container
  deployment installs assembled from separate host and Linux runtime archives.
- Relaxed Colima HTTP close/disconnect smoke timing to account for delegated
  runtime cancellation over SSH-forwarded API connections
  ([#57](https://github.com/kcosr/aw-gateway/pull/57)).

## [0.5.2] - 2026-06-09

### Added

- Added target `session_env_inherit` allow-lists for copying selected
  non-secret gateway process environment variables into gateway-managed session
  command environments
  ([#55](https://github.com/kcosr/aw-gateway/pull/55)).

### Fixed

- Hardened container-agent shutdown so repeated shutdown triggers are
  idempotent and stalled service shutdown cannot leave the agent alive
  indefinitely
  ([#56](https://github.com/kcosr/aw-gateway/pull/56)).
- Final streamed `run` and `launch` container execs now keep stdin attached
  even when `aw-gateway` is spawned with non-interactive stdin, while
  wait/detach execution keeps stdin detached for noninteractive capture paths
  ([#55](https://github.com/kcosr/aw-gateway/pull/55)).

## [0.5.1] - 2026-06-03

### Changed

- Release automation now creates normal GitHub releases
  ([#53](https://github.com/kcosr/aw-gateway/pull/53)).
- Documented release download/install guidance and archive packaging for Linux
  x86_64 and macOS arm64, including Linux container-side runtime binaries and
  runtime-specific SSHD helper/config files
  ([#53](https://github.com/kcosr/aw-gateway/pull/53)).

## [0.5.0] - 2026-05-30

### Added

- Added HTTP `mode = "pty"` for `run` and `launch` operations, with
  short-lived WebSocket attach leases for interactive terminal sessions
  ([#49](https://github.com/kcosr/aw-gateway/pull/49)).
- Added resolved target metadata to launch detail responses
  ([#49](https://github.com/kcosr/aw-gateway/pull/49)).
- Added opt-in launch passthrough args with `allow_args = true`, CLI/SSH
  `launch ... -- <args...>`, and HTTP launch-run `args`
  ([#50](https://github.com/kcosr/aw-gateway/pull/50)).
- Added `assets/acl-proxy.example.toml` as a starter allowlist for common
  coding-agent proxy egress
  ([#51](https://github.com/kcosr/aw-gateway/pull/51)).

### Changed

- Foreground `run` and `launch` now handle `SIGINT`, `SIGTERM`, and `SIGHUP`
  by canceling the active session and routing through normal idle/workspace
  cleanup; a second handled signal during cleanup aborts immediately
  ([#46](https://github.com/kcosr/aw-gateway/pull/46)).
- Foreground `run` and `launch` signal cancellation now terminates the marked
  in-container process tree for the final command
  ([#51](https://github.com/kcosr/aw-gateway/pull/51)).
- HTTP wait-mode `run` and `launch` responses can now project selected captured
  streams as JSON with `output_format`
  ([#47](https://github.com/kcosr/aw-gateway/pull/47)).

### Fixed

- HTTP wait-mode client disconnects now cancel the operation and run bounded
  in-container process cleanup for the final command instead of leaving the
  command to outlive the request
  ([#51](https://github.com/kcosr/aw-gateway/pull/51)).
- Gateway readiness now best-effort sweeps stale in-container cancellation
  marker files left by crashed or killed gateway processes once per
  runtime/container/user tuple
  ([#51](https://github.com/kcosr/aw-gateway/pull/51)).
- HTTP PTY close and WebSocket disconnect now terminate the in-container
  attached process tree instead of relying only on the host-side runtime client
  ([#49](https://github.com/kcosr/aw-gateway/pull/49)).
- HTTP daemon shutdown now waits briefly for active PTY sessions to run the same
  in-container cleanup path before the listener exits
  ([#49](https://github.com/kcosr/aw-gateway/pull/49)).
- HTTP PTY cleanup retries now keep the in-container marker until after the
  termination sequence completes
  ([#49](https://github.com/kcosr/aw-gateway/pull/49)).

## [0.4.1] - 2026-05-26

### Added

- Added `aw-gateway remove --session-id` and SSH-dispatched
  `remove --session-id` for removing one concrete ephemeral session container
  ([#44](https://github.com/kcosr/aw-gateway/pull/44)).
- Added JSON HTTP API `stop` and `remove` lifecycle routes gated by
  `http.enabled_actions` ([#44](https://github.com/kcosr/aw-gateway/pull/44)).
- Added scoped template support for `target.container_home` identity variables
  and gateway-managed `container_agent.services[].user = "{container_user}"`
  ([#41](https://github.com/kcosr/aw-gateway/pull/41)).
- Added `assets/copy-workspace-template` for preparing empty ephemeral
  workspaces from a template directory
  ([#42](https://github.com/kcosr/aw-gateway/pull/42)).

### Changed

- Explicit ephemeral `remove --session-id` now applies configured session
  workspace cleanup when cleanup is not `never`
  ([#44](https://github.com/kcosr/aw-gateway/pull/44)).
- Reworked the README structure, onboarding flow, architecture diagrams, and
  config reference wording for clearer documentation
  ([#42](https://github.com/kcosr/aw-gateway/pull/42)).

### Removed

- Removed the SSH-dispatch `rm` alias; use `remove` for gateway removal
  commands ([#42](https://github.com/kcosr/aw-gateway/pull/42)).

## [0.4.0] - 2026-05-25

### Breaking Changes

- Gateway commands without `--config` or `AW_GATEWAY_CONFIG` now select
  `~/.config/aw-gateway/gateway.toml` when present before falling back to
  `/etc/aw-gateway/gateway.toml`; managed SSH deployments that must ignore
  per-user configs should pass an explicit system config.
  ([#39](https://github.com/kcosr/aw-gateway/pull/39))
- Removed `aw-gateway config init`; use the platform-specific deployment
  examples for working configs and `aw-gateway.sample.toml` as a minimal syntax
  sample. ([#39](https://github.com/kcosr/aw-gateway/pull/39))

### Added

- Added root-level `extends` for selected gateway configs to inherit a base
  root config after each file composes its own includes.
  ([#40](https://github.com/kcosr/aw-gateway/pull/40))
- Added user-level gateway config discovery before the system config fallback:
  `--config`, `AW_GATEWAY_CONFIG`, user config, then
  `/etc/aw-gateway/gateway.toml`.
  ([#39](https://github.com/kcosr/aw-gateway/pull/39))
- Added `aw-gateway config paths [--json]` to inspect effective user paths,
  checked gateway config files, and the selected config source.
  ([#39](https://github.com/kcosr/aw-gateway/pull/39))

### Changed

- Slimmed `aw-gateway.sample.toml` to a minimal valid gateway config while
  leaving deployment-ready configs in the platform-specific examples and
  guides. ([#39](https://github.com/kcosr/aw-gateway/pull/39))

## [0.3.0] - 2026-05-25

### Breaking Changes

- Session marker JSON now requires the `launch` key; non-launch sessions are
  written as `launch = null`, and stale marker files from older unreleased
  builds should be removed before relying on idle-session accounting
  ([#38](https://github.com/kcosr/aw-gateway/pull/38)).
- Removed retired config aliases. Use `client_config.inner_alias_template`
  instead of `client_config.alias_template`; bearer HTTP auth now uses
  `http.auth.token` directly in config, and the retired `token_file` and
  `bearer_token` keys are rejected
  ([#38](https://github.com/kcosr/aw-gateway/pull/38)).
- Configured file logging is strict. If a configured logging directory template
  cannot be rendered or the file log writer cannot be initialized, gateway and
  container-agent startup fail instead of silently falling back to console
  logging
  ([#38](https://github.com/kcosr/aw-gateway/pull/38)).

### Added

- Added live smoke coverage for short-timer idle cleanup, preserve-process,
  reaping, and ephemeral workspace cleanup behavior
  ([#17](https://github.com/kcosr/aw-gateway/pull/17)).

### Changed

- Refactored gateway, runtime, config, HTTP, logging, and container-agent
  internals into smaller focused modules while preserving user-facing behavior
  except for the breaking changes called out above
  ([#38](https://github.com/kcosr/aw-gateway/pull/38)).
- Malformed container-agent status responses now report the agent as
  unavailable instead of present-but-not-ready
  ([#27](https://github.com/kcosr/aw-gateway/pull/27)).
- Restricted-user smoke setup now avoids unnecessary `sshd` reloads when the
  managed `Match` snippet is unchanged, while still validating `sshd` config on
  every run
  ([#38](https://github.com/kcosr/aw-gateway/pull/38)).

### Fixed

- Improved HTTP and SSH error classification for gateway operation failures so
  key container/runtime failures map to more specific responses
  ([#38](https://github.com/kcosr/aw-gateway/pull/38)).
- Fixed gateway-side parsing of successful typed container-agent status
  responses so agent readiness is detected correctly after container startup
  ([#31](https://github.com/kcosr/aw-gateway/pull/31)).
- Fixed rootless Podman session workspace cleanup for workspaces containing
  subuid-owned files by removing them through `podman unshare`
  ([#17](https://github.com/kcosr/aw-gateway/pull/17)).
- Fixed macOS smoke deployment packaging so the expected source path is sent to
  the remote host
  ([#38](https://github.com/kcosr/aw-gateway/pull/38)).

## [0.2.0] - 2026-05-23

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

- Refactored shared gateway operation error handling to use typed internal
  errors for stable HTTP projection
  ([#25](https://github.com/kcosr/aw-gateway/pull/25)).

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
