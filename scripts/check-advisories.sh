#!/usr/bin/env bash
set -Eeuo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

if [[ -n ${AW_ADVISORY_DB:-} ]]; then
    advisory_root=$(git -C "$AW_ADVISORY_DB" rev-parse --show-toplevel)
    [[ $(realpath -e -- "$AW_ADVISORY_DB") == $(realpath -e -- "$advisory_root") ]] \
        || {
            printf '%s\n' 'AW_ADVISORY_DB must name one complete Git repository' >&2
            exit 1
        }
    [[ -z $(git -C "$advisory_root" status --porcelain=v1 --untracked-files=all) ]] \
        || {
            printf '%s\n' 'AW_ADVISORY_DB must be clean' >&2
            exit 1
        }
    advisory_commit=$(git -C "$advisory_root" rev-parse --verify HEAD)
    advisory_tree=$(git -C "$advisory_root" rev-parse --verify 'HEAD^{tree}')
    printf 'advisory-db-commit=%s\nadvisory-db-tree=%s\n' \
        "$advisory_commit" "$advisory_tree"
    cargo audit \
        --db "$advisory_root" \
        --no-fetch \
        --no-yanked \
        --deny warnings
else
    cargo audit --deny warnings
fi
