# Changelog

## [Unreleased]

### Added

- Added configurable per-step `timeout` values for `lifecycle_steps` and `host_steps` ([#2](https://github.com/kcosr/aw-gateway/pull/2)).

### Changed

- Lifecycle and host hook commands now use a finite `60s` default timeout when no per-step `timeout` is configured ([#2](https://github.com/kcosr/aw-gateway/pull/2)).

## [0.1.0] - 2026-05-21

### Added

- Initial `aw-gateway` release.
