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
PRODUCTION_NAMES = (
    "access-async-contracts",
    "access-execution-context",
    "access-flow-relay",
    "access-flow-tls",
    "access-flow-unix",
    "access-identity",
    "access-tls-trust",
)
LINUX_TEST_NAMES = ("access-flow", "access-flow-conformance")


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


def replace_once(source: str, old: str, new: str, description: str) -> str:
    mutated = source.replace(old, new, 1)
    if mutated == source:
        raise SystemExit(f"test could not create {description}")
    return mutated


def dependency_line(source: str, name: str) -> str:
    prefix = f"{name} = "
    matches = [line for line in source.splitlines() if line.startswith(prefix)]
    if len(matches) != 1:
        raise SystemExit(f"test expected exactly one dependency line for {name}")
    return matches[0]


def write_mutation(root: Path, name: str, source: str, suffix: str) -> Path:
    path = root / f"{name}.{suffix}"
    path.write_text(source, encoding="utf-8")
    return path


def require_rejected(
    result: subprocess.CompletedProcess[str],
    description: str,
    diagnostic: str,
) -> None:
    if result.returncode == 0:
        raise SystemExit(f"{description} was accepted")
    if diagnostic not in result.stderr:
        raise SystemExit(
            f"{description} failed for the wrong reason:\n"
            f"{result.stdout}{result.stderr}"
        )


def main() -> None:
    baseline = validate(MANIFEST)
    if baseline.returncode != 0:
        raise SystemExit(
            f"baseline pin validation failed:\n{baseline.stdout}{baseline.stderr}"
        )

    source = MANIFEST.read_text(encoding="utf-8")
    lock_source = LOCK.read_text(encoding="utf-8")
    baseline_revision = baseline.stdout.strip()
    with tempfile.TemporaryDirectory(prefix="access-runtime-pin-test.") as directory:
        root = Path(directory)
        for name in (*PRODUCTION_NAMES, *LINUX_TEST_NAMES):
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
            manifest = write_mutation(root, f"missing-{name}", mutated, "toml")
            result = validate(manifest)
            require_rejected(
                result,
                f"missing dependency pin {name}",
                f"{name} must be a pinned Git dependency",
            )

        selected_name = "access-flow-tls"
        selected_line = dependency_line(source, selected_name)
        wrong_url_line = selected_line.replace(
            RUNTIME_URL, "https://example.invalid/access-runtime.git"
        )
        wrong_url = replace_once(
            source, selected_line, wrong_url_line, "wrong Runtime URL"
        )
        require_rejected(
            validate(write_mutation(root, "wrong-url", wrong_url, "toml")),
            "wrong Runtime URL",
            f"{selected_name} must use {RUNTIME_URL}",
        )

        for selector, value in (
            ("branch", '"main"'),
            ("tag", '"v0.1.0"'),
            ("path", '"../access-runtime/crates/access-flow-tls"'),
            ("features", '["fixture"]'),
        ):
            mutated_line = selected_line.removesuffix(" }") + f", {selector} = {value} }}"
            mutated = replace_once(
                source,
                selected_line,
                mutated_line,
                f"{selector} dependency selector",
            )
            require_rejected(
                validate(write_mutation(root, f"selector-{selector}", mutated, "toml")),
                f"{selector} dependency selector",
                f"{selected_name} has an unexpected dependency selector",
            )

        patched = (
            source
            + f'\n[patch."{RUNTIME_URL}"]\n'
            + 'access-flow = { path = "../access-runtime/crates/access-flow" }\n'
        )
        require_rejected(
            validate(write_mutation(root, "patch", patched, "toml")),
            "Cargo patch override",
            "Cargo patch overrides are forbidden",
        )

        unexpected_line = (
            f'access-unreviewed = {{ git = "{RUNTIME_URL}", '
            f'rev = "{baseline_revision}" }}\n'
        )
        unexpected = replace_once(
            source,
            "[dependencies]\n",
            f"[dependencies]\n{unexpected_line}",
            "unexpected direct Runtime dependency",
        )
        require_rejected(
            validate(write_mutation(root, "unexpected-direct", unexpected, "toml")),
            "unexpected direct Runtime dependency",
            "unexpected direct Access Runtime dependency",
        )

        divergent_revision = "0" * 40
        divergent_line = dependency_line(source, "access-flow")
        divergent = replace_once(
            source,
            divergent_line,
            divergent_line.replace(baseline_revision, divergent_revision),
            "divergent Linux-only pin",
        )
        require_rejected(
            validate(write_mutation(root, "divergent", divergent, "toml")),
            "divergent Linux-only pin",
            "Access Runtime dependencies must pin one identical commit",
        )

        exact_lock_source = (
            f"git+{RUNTIME_URL}?rev={baseline_revision}#{baseline_revision}"
        )
        for component, wrong_source in (
            (
                "query",
                f"git+{RUNTIME_URL}?rev={divergent_revision}#{baseline_revision}",
            ),
            (
                "fragment",
                f"git+{RUNTIME_URL}?rev={baseline_revision}#{divergent_revision}",
            ),
        ):
            mutated = replace_once(
                lock_source,
                exact_lock_source,
                wrong_source,
                f"lock {component} mismatch",
            )
            require_rejected(
                validate(MANIFEST, write_mutation(root, f"lock-{component}", mutated, "lock")),
                f"lock {component} mismatch",
                "lock source mismatch",
            )

        duplicate_package = (
            "\n[[package]]\n"
            'name = "access-flow-tls"\n'
            'version = "0.1.0"\n'
            f'source = "{exact_lock_source}"\n'
        )
        require_rejected(
            validate(
                MANIFEST,
                write_mutation(
                    root, "duplicate-lock-package", lock_source + duplicate_package, "lock"
                ),
            ),
            "duplicate Runtime lock package",
            "must have exactly one Cargo.lock package entry; found 2",
        )

        unexpected_package = (
            "\n[[package]]\n"
            'name = "access-unreviewed"\n'
            'version = "0.1.0"\n'
            f'source = "{exact_lock_source}"\n'
        )
        require_rejected(
            validate(
                MANIFEST,
                write_mutation(
                    root, "unexpected-lock-package", lock_source + unexpected_package, "lock"
                ),
            ),
            "unexpected Runtime lock package",
            "unexpected Access Runtime lock package",
        )

    print("access-runtime-pin-validator-tests=passed")


if __name__ == "__main__":
    main()
