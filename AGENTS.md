# aw-gateway Agent Instructions

This repository contains `aw-gateway`, a gateway for disposable or reusable
container workspaces exposed through the host CLI, SSH-compatible clients, and
an optional JSON HTTP API.

## What This Repo Is

`aw-gateway` is a Rust gateway for disposable or reusable container workspaces.
It starts or reuses a configured container target, supervises in-container
services, exposes lifecycle and command operations through the host CLI and
optional JSON HTTP API, and uses SSH-compatible clients as the standard
interactive attach path to container-local SSH instead of the host shell or
host filesystem.

## Fast Bootstrap

1. Build: `cargo build`
2. Format: `cargo fmt`
3. Test: `cargo test`
4. Lint: `cargo clippy --all-targets --all-features`
5. Release build: `cargo build --release`
6. Validate gateway config: `cargo run --bin aw-gateway -- --config aw-gateway.sample.toml config validate`
7. Validate agent config: `cargo run --bin aw-container-agent -- --config container-agent.sample.toml config validate`

## Development

- Use `cargo fmt`, `cargo test`, and `cargo clippy --all-targets --all-features`
  before handing off substantial changes.
- Run `cargo build --release` before release-oriented or packaging changes.
- Keep protocol/proxy modes quiet on stdout. Use stderr/logs for diagnostics.
- Keep implementation generic where possible. Deployment-specific behavior
  should usually be expressed through config, not hard-coded.
- Prefer deterministic offline tests for config, dispatch, path, and lifecycle
  decisions.
- Update `README.md` for user-facing behavior, config, or workflow changes.

## Layout

- `src/bin/` contains small binary entrypoints.
- `src/config.rs` owns TOML schema and validation.
- `src/gateway.rs` owns host-side CLI dispatch and lifecycle orchestration.
- `src/gateway/` contains focused gateway support modules for client config,
  file utilities, health checks, HTTP API surface, identity/key handling,
  listener proxying, shared operation dispatch, status models, and session
  marker state.
- `src/agent.rs` owns in-container agent entrypoint dispatch.
- `src/agent/` contains focused container-agent support modules for SSH bridge,
  control socket dispatch, idle cleanup/reaper, process helpers, service
  supervision, shared agent state, socket helpers, and status projection.
- `src/ssh_dispatch.rs` owns `SSH_ORIGINAL_COMMAND` parsing.
- `src/runtime.rs` owns Podman, Docker, and Colima command construction.
- `aw-gateway.sample.toml` and `container-agent.sample.toml` are the canonical
  sample configs.

## Changelog

Location: `CHANGELOG.md` (root)

### Format

Use these sections under `## [Unreleased]`:
- `### Breaking Changes` - API/config changes requiring migration
- `### Added` - New features
- `### Changed` - Changes to existing behavior
- `### Fixed` - Bug fixes
- `### Removed` - Removed features

### Rules

- New entries always go under `## [Unreleased]`.
- Append to existing subsections; do not create duplicate subsection headers.
- Do not edit already released version sections.
- Use inline PR links when a PR exists:
  `([#123](https://github.com/kcosr/aw-gateway/pull/123))`.

## Releasing

Use `current` when `Cargo.toml` already has the intended release version, or
use `patch`, `minor`, `major`, or an explicit semantic version:

```bash
node scripts/release.mjs current  # Release current Cargo.toml version
node scripts/release.mjs patch    # Bug fixes, e.g. 0.1.0 -> 0.1.1
node scripts/release.mjs minor    # New features, e.g. 0.1.1 -> 0.2.0
node scripts/release.mjs major    # Breaking changes, e.g. 0.2.0 -> 1.0.0
node scripts/release.mjs 0.2.3    # Explicit version
```

The release script verifies a clean `main` branch, optionally bumps
`Cargo.toml` and `Cargo.lock`, stamps `CHANGELOG.md`, commits and tags the
release, pushes to origin, creates a GitHub release from the changelog
notes, then opens a new `## [Unreleased]` section for the next cycle.

If GitHub release creation fails after the commit and tag are pushed, create
the GitHub release manually for the existing tag instead of rerunning the
script.

Release archives are packaged separately after the GitHub release exists. Use
the README release section as the source of truth for archive names, contents,
and platform handling. Each archive is a single-platform build output; do not
bundle Linux runtime binaries into macOS archives. Mixed host/container
deployments, such as a macOS arm64 host with a Linux arm64 Colima VM, are
assembled from multiple platform archives. SSHD helper/config files are
runtime-specific; deployment guides install them from `examples/<runtime>/` in
the extracted release archive.
