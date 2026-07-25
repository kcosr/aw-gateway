#!/usr/bin/env bash
set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

if [[ -n ${AW_ADVISORY_DB:-} ]]; then
    cargo audit \
        --db "$AW_ADVISORY_DB" \
        --no-fetch \
        --no-yanked \
        --deny warnings
else
    cargo audit --deny warnings
fi
