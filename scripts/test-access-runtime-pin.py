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


def validate(manifest: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable,
            str(VALIDATOR),
            str(manifest),
            str(LOCK),
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

    print("access-runtime-pin-validator-tests=passed")


if __name__ == "__main__":
    main()
