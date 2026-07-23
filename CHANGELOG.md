# Changelog

## [Unreleased]

### Breaking Changes

- Generic `container_mounts` now reject Unix socket, symlink, FIFO, and device
  sources. Existing host socket integrations must use the typed
  `host_socket_exposures` target map.
- `workspace.state_dir` must now remain relative to the workspace mount;
  absolute, home-relative, and parent-traversing values are rejected. Fixed
  containers also record their resolved workspace layout and must be removed
  before reuse after that layout changes
  ([#63](https://github.com/kcosr/aw-gateway/pull/63)).

### Added

- Added typed host-to-container Unix socket exposure for Apple Container 1.1+
  and native local Linux Docker/Podman, including strict source validation,
  backend-specific realization, existing-container manifests, and readiness
  status. Added an optional Apple host-proxy example with a supervised
  transparent relay and fail-closed IPv4/IPv6 firewall policy.
- Host socket targets can optionally select the in-container readiness identity
  used only for bounded endpoint probes; it defaults to root and does not alter
  realization or container reuse identity. The host-proxy profile reuses
  required container bootstrap steps for fail-closed firewall installation and
  ordinary service dependencies for watcher, relay, and SSH startup ordering.
- Added an opt-in privileged Linux stack smoke that pins the reviewed
  Access Runtime, ACL Proxy, and AW Gateway commits and exercises real
  HTTP/HTTPS transparent redirects through the release relay, host UDS proxy,
  parent proxy, fail-closed loss, and native-Linux pinned-inode recovery.
- Added an opt-in native Linux Docker smoke that drives typed host socket
  exposure through AW Gateway's real create, readiness, manifest, runtime-exec,
  pinned-inode replacement refusal, explicit removal, recreation, and second
  UDS exchange without duplicating the proxy protocol.
- Added `workspace.container_path` so the host workspace can be mounted at a
  path separate from `container_home`, including persistent-home deployments
  that expose only a dedicated shared directory
  ([#63](https://github.com/kcosr/aw-gateway/pull/63)).

### Fixed

- Managed state and SSH authorized keys now resolve consistently on both sides
  of custom workspace mounts. The conventional `container-sshd` service
  receives the resolved key path, and the SSHD helper installs it as a global
  `AuthorizedKeysFile` directive
  ([#63](https://github.com/kcosr/aw-gateway/pull/63)).
- Restrictive transfer policy no longer rejects ordinary shell composition.
  Composed commands are blocked only when they contain a recognizable denied
  SCP or SFTP server invocation, and rejections include a bounded, escaped copy
  of `SSH_ORIGINAL_COMMAND`
  ([#65](https://github.com/kcosr/aw-gateway/pull/65)).

## [0.9.0] - 2026-07-08

### Added

- Added `local_ssh.mode = "direct"` for fixed SSH targets using
  `local_ssh.backend = "published_port"`. Direct mode starts or reuses the
  target, waits for the runtime-published loopback SSH endpoint, persists the
  endpoint across stops, renders local client config without a `ProxyCommand`,
  and rejects SSH-dispatched direct client config where loopback would point at
  the wrong machine. Direct mode also rejects agent-owned idle cleanup because
  direct SSH sessions bypass the gateway and agent session counters.
  ([#62](https://github.com/kcosr/aw-gateway/pull/62)).
- Added direct published-port smoke coverage and an `AW_SSHD_LISTEN_ADDRESS`
  override for `start-container-sshd`, allowing configs that publish container
  port 22 to make sshd listen on the container network interface while leaving
  the default Docker/Podman loopback-only examples unchanged
  ([#62](https://github.com/kcosr/aw-gateway/pull/62)).

## [0.8.1] - 2026-07-03

### Added

- Added explicit target access modes with `access.method = "runtime_exec"` for
  no-SSH runtime execution targets. Runtime-exec targets support lifecycle,
  status, run, launch, and the new local `shell` command without exposing
  container SSH, and Apple container targets can now use this mode without a
  published SSH port ([#60](https://github.com/kcosr/aw-gateway/pull/60)).
- Added runtime-exec sample configs for local Podman, Docker, and Colima
  deployments ([#60](https://github.com/kcosr/aw-gateway/pull/60)).
- Added local-transport smoke harness support and Apple Container smoke
  automation, including a repo-owned macOS runner, Python 3.9-compatible smoke
  packaging, env-driven pytest host selection, and reusable Apple smoke
  `--skip-build` / `--skip-image` modes
  ([#61](https://github.com/kcosr/aw-gateway/pull/61)).

### Changed

- Gateway status, target listing, and runtime labels now include the effective
  target access mode. Existing configs default to `access.method = "ssh"`;
  target-specific container reuse now fails closed when a labeled container's
  access mode differs from the effective target config
  ([#60](https://github.com/kcosr/aw-gateway/pull/60)).

### Fixed

- Published-port SSH targets now report only `ssh_tcp` in status JSON instead
  of also exposing a Unix `ssh_socket` path
  ([#60](https://github.com/kcosr/aw-gateway/pull/60)).

## [0.8.0] - 2026-07-02

### Added

- Added experimental `apple_container` runtime support for Apple silicon macOS
  26+ hosts, including Apple container CLI/system preflight checks and
  published-port SSH endpoint handling, object-shaped Apple `status` and
  `configuration.image` JSON parsing, validated bare-key exec env inheritance,
  plus a local Apple container deployment guide and sample config
  ([direct commits](https://github.com/kcosr/aw-gateway/compare/a53dfa9...03ae7d5)).

### Changed

- Bumped the internal container bootstrap config `schema_version` to `2`.
  `aw-gateway` and the bind-mounted `aw-container-bootstrap` binary must be
  upgraded together; mismatches fail container startup with an unsupported
  bootstrap schema error
  ([ecb40f7](https://github.com/kcosr/aw-gateway/commit/ecb40f7064e427203a09e008024ac6ec372435bb)).

### Fixed

- Fixed Apple container bootstrap identity preparation so pre-existing session
  home and state directories are not chowned, which avoids Apple virtiofs
  bind-mount `EPERM` failures during Apple container startup
  ([ecb40f7](https://github.com/kcosr/aw-gateway/commit/ecb40f7064e427203a09e008024ac6ec372435bb)).
- Fixed container SSHD helpers so generated `session_env` values are merged
  with base `SHELL`/`PATH` defaults into a single `SetEnv` directive, allowing
  variables such as `CODEX_HOME` to reach SSH sessions. Runtime example helpers
  now also match the documented transfer-policy `ForceCommand` behavior when
  SFTP is denied ([#59](https://github.com/kcosr/aw-gateway/pull/59)).

## [0.7.0] - 2026-06-25

### Breaking Changes

- Launch execution now requires `launch` in `ssh_dispatch.enabled_actions` or
  `http.enabled_actions`; `launch-show` permits inspecting one launch
  definition without granting execution
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Several unsafe or ambiguous configs now fail validation or startup instead of
  being accepted: enabled non-loopback HTTP listeners require bearer auth,
  include patterns must match files, include definition sections must be TOML
  tables, config identifiers may not start with `.` or `-`, malformed image
  references are rejected, `logging.max_files` is capped at `1024`, service env
  entries must declare exactly one source at config load, rendered bind mount
  paths may not contain `:` or `,`, read-write bind mount sources may not be
  world-writable, and string launch var values may not contain NUL, LF, or CR
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).

### Changed

- Restrictive container SSH transfer policies now install the command filter
  when either SFTP or legacy SCP is restricted, and reject shell-composed,
  assignment-prefixed, or known wrapper-command invocations before they can
  bypass denied SFTP/SCP server invocations. The shell-composition check rejects
  only chaining, substitution, and subshell bytes (`;`, `|`, `&`, `(`, `)`,
  `` ` ``, newlines); bare redirection (`<`, `>`) and variable expansion (`$`)
  are now allowed so ordinary commands such as `echo "$HOME"` or `cmd > out` are
  not blocked under restrictive transfer policy. Transfer policy is a best-effort
  convenience control, not a security boundary
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- HTTP wait-mode command output, container-agent control responses, and HTTP
  health-probe responses are now size bounded; truncated wait streams are
  reported with `output_truncated`
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Enabled HTTP listeners now require bearer auth when `http.listen` is
  non-loopback ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- HTTP 500-class operation errors now return a generic client-facing message
  while logging detailed internal error sources server-side
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Container-agent mutating control-socket methods now fail closed when no
  control token is configured, and control-token checks use shared
  constant-time comparison ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Container-agent startup now rejects malformed `AW_AUTHENTICATED_UID` /
  `AW_AUTHENTICATED_GID` values instead of silently disabling peer validation
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Container-agent service environment entries now validate the exactly-one
  `value` / `file` / `inherit` source rule during gateway config validation
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Container-agent service environment interpolation now renders literal
  `value` entries and `file` paths only; values loaded from files or inherited
  environment variables are passed through literally
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Command health checks now run with a configurable timeout, defaulting to
  `5s`, and timed command stderr draining is bounded after process exit
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Container-agent service restarts now keep backing off for immediate crash
  loops, and service shutdown no longer holds the child lock while waiting
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Container-agent SSH bridge and control socket accept loops now continue after
  transient accept errors and enforce fixed connection/session limits
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- File logging now creates private log directories/files, rejects
  `logging.max_files` above `1024`, and redacts HTTP bearer tokens from Debug
  output ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- File logging now recovers from a poisoned writer mutex instead of panicking on
  subsequent log writes ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Private file writes now use atomic replace semantics, and existing control
  token files must have private permissions before they are trusted
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Runtime-provided container and exec environment values are now normally passed
  through the spawned container-runtime process environment instead of embedding
  their values in `podman`/`docker` argv; host-runtime-sensitive names such as
  `PATH`, `LD_PRELOAD`, and `DOCKER_HOST` still use explicit `KEY=value`
  runtime arguments so they cannot alter the runtime client process itself
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Container-agent service user groups are now resolved before spawning the
  child process, so the post-fork `pre_exec` path only performs setgroups,
  setgid, and setuid ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Runtime process signaling now uses checked PID conversions to avoid
  out-of-range PID wraparound
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- In-container cancel marker files for cancelable exec/PTY sessions are now
  created with owner-only permissions
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Session marker reads now tolerate pre-launch-field marker JSON, and marker
  liveness checks use the same self-process fast path as listener status
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Session marker creation for active run, launch, connect, and local-listen
  sessions is now serialized with readiness under the lifecycle lock so idle
  cleanup cannot stop a container after a new session has registered
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Session marker creation and active-marker enumeration in async gateway paths
  now run on the blocking task pool instead of on async worker threads
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Container startup now resolves configured bind mounts once and reuses the
  checked mount set for safety warnings and the runtime run spec
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Rendered container bind mount sources and targets, including generated
  workspace and control-socket mounts, now reject `:` and `,` separators, and
  configured mount targets must render to absolute paths
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Container startup now rejects read-write bind mounts whose resolved source is
  world-writable ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Container inspect results now fail closed when the runtime returns more than
  one inspected container for a single-name inspect request
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Target image references now reject malformed values such as leading flags,
  whitespace, empty path components, invalid tags, and invalid digests during
  config validation ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- String launch variable values now reject NUL, LF, and CR characters, and the
  launch documentation now calls out `{var.*}` values as untrusted when used in
  host-side launch step environment values
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Include patterns now fail config loading when they match no files
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Root `extends` chains now fail config loading after 64 root config files
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Config identifiers such as target, launch, template, service, launch-var, and
  runtime-profile names now reject leading `.` or `-`
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- `local_ssh.host` loopback validation now applies only when
  `local_ssh.mode = "listen"`, matching the effective config rules
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Local SSH listener startup now serializes status-file checks and writes with
  the lifecycle lock, and transient accept errors no longer terminate the
  listener ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Stale in-container cancel marker sweeping no longer treats inaccessible or
  out-of-range host PIDs as active
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- PTY lease tokens, cancel marker tokens, and temporary suffix generation now
  use `getrandom` directly instead of opening `/dev/urandom`
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).

### Fixed

- Smoke-generated HTTP configs now enable `launch`, so HTTP launch execution
  coverage matches the split launch catalog/detail/execution action model
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- Size-based log rotation (gateway and container-agent) now reopens the rotated
  file in append mode instead of truncating, tolerates already-moved generation
  files, and always resets its byte counter after a rotation attempt, so a
  failed reopen can no longer leave the writer attached to the rotated-away file
  and re-rotating on every write
  ([#58](https://github.com/kcosr/aw-gateway/pull/58)).
- The container-agent control socket now caps held sessions below the connection
  limit, so a burst of session holds can no longer exhaust all control
  connections and starve `status`/shutdown requests; the `too_many_sessions`
  response is now reachable ([#58](https://github.com/kcosr/aw-gateway/pull/58)).

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
  `launch ... -- <args...>`, and HTTP launch request `args`
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
