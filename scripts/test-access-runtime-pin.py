#!/usr/bin/env python3

import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
VALIDATOR = ROOT / "scripts" / "validate-access-runtime-pin.py"
MANIFEST = ROOT / "Cargo.toml"
LOCK = ROOT / "Cargo.lock"
RUNTIME_URL = "https://github.com/kcosr/access-runtime.git"


def validate(
    manifest: Path, lock: Path = LOCK
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable,
            str(VALIDATOR),
            str(manifest),
            str(lock),
            RUNTIME_URL,
        ],
        check=False,
        capture_output=True,
        text=True,
    )


def main() -> None:
    baseline = validate(MANIFEST)
    if baseline.returncode != 0:
        raise SystemExit(
            f"baseline pin validation failed:\n{baseline.stdout}{baseline.stderr}"
        )

    source = MANIFEST.read_text(encoding="utf-8")
    baseline_revision = baseline.stdout.strip()
    with tempfile.TemporaryDirectory(prefix="access-runtime-pin-test.") as directory:
        root = Path(directory)
        for name in ("access-flow", "access-flow-conformance"):
            lines = source.splitlines(keepends=True)
            mutated = "".join(
                line
                for line in lines
                if not (
                    line.startswith(f"{name} = ")
                    and "access-runtime.git" in line
                )
            )
            if mutated == source:
                raise SystemExit(f"test could not remove {name}")
            manifest = root / f"{name}.toml"
            manifest.write_text(mutated, encoding="utf-8")
            result = validate(manifest)
            if result.returncode == 0:
                raise SystemExit(f"missing Linux-only pin was accepted: {name}")
            if f"{name} must be a pinned Git dependency" not in result.stderr:
                raise SystemExit(
                    f"{name} failed for the wrong reason:\n"
                    f"{result.stdout}{result.stderr}"
                )

        divergent_revision = "0" * 40
        divergent = source.replace(
            (
                'access-flow = { git = "https://github.com/kcosr/access-runtime.git", '
                f'rev = "{baseline_revision}" }}'
            ),
            (
                'access-flow = { git = "https://github.com/kcosr/access-runtime.git", '
                f'rev = "{divergent_revision}" }}'
            ),
            1,
        )
        if divergent == source:
            raise SystemExit("test could not create a divergent Linux-only pin")
        divergent_manifest = root / "divergent.toml"
        divergent_manifest.write_text(divergent, encoding="utf-8")
        result = validate(divergent_manifest)
        if result.returncode == 0:
            raise SystemExit("divergent Linux-only pin was accepted")
        if (
            "Access Runtime dependencies must pin one identical commit"
            not in result.stderr
        ):
            raise SystemExit(
                "divergent pin failed for the wrong reason:\n"
                f"{result.stdout}{result.stderr}"
            )

        lock_source = LOCK.read_text(encoding="utf-8")
        divergent_lock_source = lock_source.replace(
            f"#{baseline_revision}",
            f"#{divergent_revision}",
            1,
        )
        if divergent_lock_source == lock_source:
            raise SystemExit("test could not create a lock source mismatch")
        divergent_lock = root / "divergent.lock"
        divergent_lock.write_text(divergent_lock_source, encoding="utf-8")
        result = validate(MANIFEST, divergent_lock)
        if result.returncode == 0:
            raise SystemExit("divergent lock source was accepted")
        if "lock source mismatch" not in result.stderr:
            raise SystemExit(
                "lock source failed for the wrong reason:\n"
                f"{result.stdout}{result.stderr}"
            )

    print("access-runtime-pin-validator-tests=passed")


if __name__ == "__main__":
    main()
