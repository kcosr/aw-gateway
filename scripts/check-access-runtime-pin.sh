#!/usr/bin/env bash

set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME_URL=https://github.com/kcosr/access-runtime.git
TARGET_DIR=${AW_REPRO_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/aw-gateway/access-runtime-reproduction.$$}

cleanup() {
    rm -rf -- "$TARGET_DIR"
}
trap cleanup EXIT

python3 - "$ROOT/Cargo.toml" "$ROOT/Cargo.lock" "$RUNTIME_URL" <<'PY'
import re
import sys
import tomllib

manifest_path, lock_path, expected_url = sys.argv[1:]
with open(manifest_path, "rb") as handle:
    manifest = tomllib.load(handle)
with open(lock_path, "rb") as handle:
    lock = tomllib.load(handle)

names = {
    "access-async-contracts",
    "access-flow-relay",
    "access-flow-unix",
    "access-identity",
}
dependencies = manifest["dependencies"]
revisions = set()
for name in names:
    dependency = dependencies.get(name)
    if not isinstance(dependency, dict):
        raise SystemExit(f"{name} must be a pinned Git dependency")
    if dependency.get("git") != expected_url:
        raise SystemExit(f"{name} must use {expected_url}")
    revision = dependency.get("rev")
    if not isinstance(revision, str) or not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise SystemExit(f"{name} must pin one full lowercase Git commit")
    if set(dependency) != {"git", "rev"}:
        raise SystemExit(f"{name} has an unexpected dependency selector")
    revisions.add(revision)
if len(revisions) != 1:
    raise SystemExit("Access Runtime dependencies must pin one identical commit")
revision = revisions.pop()

locked = {
    package["name"]: package.get("source")
    for package in lock["package"]
    if package["name"] in names
}
for name in names:
    source = locked.get(name)
    expected = f"git+{expected_url}?rev={revision}#{revision}"
    if source != expected:
        raise SystemExit(f"{name} lock source mismatch: {source!r}")
print(revision)
PY

mkdir -p "$TARGET_DIR"
CARGO_TARGET_DIR="$TARGET_DIR" cargo metadata \
    --manifest-path "$ROOT/Cargo.toml" \
    --locked \
    --format-version 1 >/dev/null
CARGO_TARGET_DIR="$TARGET_DIR" cargo build \
    --manifest-path "$ROOT/Cargo.toml" \
    --locked \
    --release \
    --bin aw-container-agent

printf '%s\n' "access-runtime-pin-reproduction=passed"
