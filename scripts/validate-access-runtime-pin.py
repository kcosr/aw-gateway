#!/usr/bin/env python3

import re
import sys
import tomllib
from pathlib import Path


PRODUCTION_NAMES = {
    "access-async-contracts",
    "access-flow-relay",
    "access-flow-unix",
    "access-identity",
}
LINUX_TEST_NAMES = {
    "access-flow",
    "access-flow-conformance",
}
LINUX_TARGET = 'cfg(target_os = "linux")'


def dependency_table(manifest: dict, path: tuple[str, ...]) -> dict:
    value = manifest
    for component in path:
        value = value.get(component)
        if not isinstance(value, dict):
            raise SystemExit(f"missing dependency table: {'.'.join(path)}")
    return value


def main() -> None:
    if len(sys.argv) != 4:
        raise SystemExit(
            "usage: validate-access-runtime-pin.py MANIFEST LOCK EXPECTED_URL"
        )

    manifest_path, lock_path, expected_url = sys.argv[1:]
    with Path(manifest_path).open("rb") as handle:
        manifest = tomllib.load(handle)
    with Path(lock_path).open("rb") as handle:
        lock = tomllib.load(handle)

    tables = (
        (
            dependency_table(manifest, ("dependencies",)),
            PRODUCTION_NAMES,
        ),
        (
            dependency_table(
                manifest,
                ("target", LINUX_TARGET, "dev-dependencies"),
            ),
            LINUX_TEST_NAMES,
        ),
    )
    revisions = set()
    for dependencies, names in tables:
        for name in names:
            dependency = dependencies.get(name)
            if not isinstance(dependency, dict):
                raise SystemExit(f"{name} must be a pinned Git dependency")
            if dependency.get("git") != expected_url:
                raise SystemExit(f"{name} must use {expected_url}")
            revision = dependency.get("rev")
            if not isinstance(revision, str) or not re.fullmatch(
                r"[0-9a-f]{40}", revision
            ):
                raise SystemExit(f"{name} must pin one full lowercase Git commit")
            if set(dependency) != {"git", "rev"}:
                raise SystemExit(f"{name} has an unexpected dependency selector")
            revisions.add(revision)

    if len(revisions) != 1:
        raise SystemExit("Access Runtime dependencies must pin one identical commit")
    revision = revisions.pop()

    names = PRODUCTION_NAMES | LINUX_TEST_NAMES
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


if __name__ == "__main__":
    main()
