#!/usr/bin/env python3

import re
import sys
import tomllib
from pathlib import Path


PRODUCTION_NAMES = {
    "access-async-contracts",
    "access-flow-relay",
    "access-flow-tls",
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


def direct_dependency_tables(manifest: dict):
    for name in ("dependencies", "dev-dependencies", "build-dependencies"):
        dependencies = manifest.get(name)
        if isinstance(dependencies, dict):
            yield (name,), dependencies

    targets = manifest.get("target")
    if not isinstance(targets, dict):
        return
    for target, target_config in targets.items():
        if not isinstance(target_config, dict):
            continue
        for name in ("dependencies", "dev-dependencies", "build-dependencies"):
            dependencies = target_config.get(name)
            if isinstance(dependencies, dict):
                yield ("target", target, name), dependencies


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

    if "patch" in manifest:
        raise SystemExit("Cargo patch overrides are forbidden")

    tables = (
        (
            ("dependencies",),
            dependency_table(manifest, ("dependencies",)),
            PRODUCTION_NAMES,
        ),
        (
            ("target", LINUX_TARGET, "dev-dependencies"),
            dependency_table(
                manifest,
                ("target", LINUX_TARGET, "dev-dependencies"),
            ),
            LINUX_TEST_NAMES,
        ),
    )
    approved_locations = {
        (*path, name) for path, _, names in tables for name in names
    }
    for path, dependencies in direct_dependency_tables(manifest):
        for name, dependency in dependencies.items():
            if (
                isinstance(dependency, dict)
                and dependency.get("git") == expected_url
                and (*path, name) not in approved_locations
            ):
                raise SystemExit(
                    f"unexpected direct Access Runtime dependency: {'.'.join((*path, name))}"
                )

    revisions = set()
    for _, dependencies, names in tables:
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
    packages = lock.get("package")
    if not isinstance(packages, list):
        raise SystemExit("Cargo.lock package inventory is missing")
    runtime_source_prefix = f"git+{expected_url}"
    for package in packages:
        name = package.get("name")
        source = package.get("source")
        if (
            isinstance(source, str)
            and source.startswith(runtime_source_prefix)
            and name not in names
        ):
            raise SystemExit(f"unexpected Access Runtime lock package: {name!r}")

    expected_source = f"git+{expected_url}?rev={revision}#{revision}"
    for name in names:
        matching = [package for package in packages if package.get("name") == name]
        if len(matching) != 1:
            raise SystemExit(
                f"{name} must have exactly one Cargo.lock package entry; found {len(matching)}"
            )
        source = matching[0].get("source")
        if source != expected_source:
            raise SystemExit(f"{name} lock source mismatch: {source!r}")

    print(revision)


if __name__ == "__main__":
    main()
