#!/usr/bin/env bash

set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME_URL=https://github.com/kcosr/access-runtime.git
TARGET_DIR=${AW_REPRO_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/aw-gateway/access-runtime-reproduction.$$}

cleanup() {
    rm -rf -- "$TARGET_DIR"
}
trap cleanup EXIT

python3 "$ROOT/scripts/validate-access-runtime-pin.py" \
    "$ROOT/Cargo.toml" \
    "$ROOT/Cargo.lock" \
    "$RUNTIME_URL"

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
