# Changelog

## [Unreleased]

### Added

- Added named launches with strict launch/includes config, typed `{var.<name>}`
  variables, discovery/detail CLI output, post-ready setup steps, launch
  execution, and minimal launch provenance in status surfaces
  ([#3](https://github.com/kcosr/aw-gateway/pull/3)).
- Added configurable per-step `timeout` values for `lifecycle_steps` and `host_steps` ([#2](https://github.com/kcosr/aw-gateway/pull/2)).

### Changed

- Lifecycle and host hook commands now use a finite `60s` default timeout when no per-step `timeout` is configured ([#2](https://github.com/kcosr/aw-gateway/pull/2)).

## [0.1.0] - 2026-05-21

### Added

- Initial `aw-gateway` release.
