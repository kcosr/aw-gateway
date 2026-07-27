#!/usr/bin/env python3
"""Bind RT06 evidence to the exact Proxy and Gateway processes."""

from __future__ import annotations

import argparse
import hashlib
import os
import pathlib
import stat
import time


class BindingError(RuntimeError):
    pass


WAIT_TIMEOUT_SECONDS = 120
WAIT_INTERVAL_SECONDS = 0.02


def positive_integer(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be positive")
    return parsed


def nonnegative_integer(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("value must be nonnegative")
    return parsed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--control-dir", required=True, type=pathlib.Path)
    parser.add_argument("--output-uid", required=True, type=nonnegative_integer)
    parser.add_argument("--output-gid", required=True, type=nonnegative_integer)
    return parser.parse_args()


def status_fields(pid: int) -> dict[str, str]:
    fields: dict[str, str] = {}
    status = pathlib.Path("/proc") / str(pid) / "status"
    for line in status.read_text(encoding="ascii").splitlines():
        key, separator, value = line.partition(":")
        if separator:
            fields[key] = value.strip()
    return fields


def belongs_to_container(pid: int, container_init_pid: int) -> bool:
    seen: set[int] = set()
    while pid > 1 and pid not in seen:
        if pid == container_init_pid:
            return True
        seen.add(pid)
        pid = int(status_fields(pid)["PPid"])
    return False


def find_agent_host_pid(
    namespace_pid: int, container_init_pid: int, expected_exe: pathlib.Path
) -> int:
    matches: list[int] = []
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            fields = status_fields(pid)
            nspid = [int(value) for value in fields.get("NSpid", "").split()]
            if (
                nspid
                and nspid[-1] == namespace_pid
                and belongs_to_container(pid, container_init_pid)
                and os.path.samefile(entry / "exe", expected_exe)
            ):
                matches.append(pid)
        except (
            FileNotFoundError,
            PermissionError,
            ProcessLookupError,
            KeyError,
            ValueError,
        ):
            continue
    if len(matches) != 1:
        raise BindingError(
            f"expected one namespace/executable-bound agent PID, found {len(matches)}"
        )
    return matches[0]


def process_start_time(pid: int) -> int:
    process_stat = (pathlib.Path("/proc") / str(pid) / "stat").read_text(
        encoding="ascii"
    )
    end_name = process_stat.rfind(")")
    fields = process_stat[end_name + 2 :].split() if end_name >= 0 else []
    if len(fields) < 20:
        raise BindingError("process stat is truncated")
    return int(fields[19])


def bind_process(name: str, pid: int, expected_exe: pathlib.Path) -> str:
    proc_exe = pathlib.Path("/proc") / str(pid) / "exe"
    start_before = process_start_time(pid)
    if not os.path.samefile(proc_exe, expected_exe):
        raise BindingError(f"{name} executable does not match the built binary")
    with proc_exe.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    start_after = process_start_time(pid)
    if start_before != start_after:
        raise BindingError(f"{name} process identity changed while binding")
    return f"{name}\t{pid}\t{start_before}\t{digest}\n"


def open_control_directory(
    control: pathlib.Path, output_uid: int, output_gid: int
) -> int:
    if not control.is_absolute() or control.is_symlink():
        raise BindingError("control directory must be an absolute non-symlink path")
    resolved = control.resolve(strict=True)
    descriptor = os.open(
        resolved, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    )
    metadata = os.fstat(descriptor)
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or metadata.st_uid != output_uid
        or metadata.st_gid != output_gid
        or stat.S_IMODE(metadata.st_mode) != 0o700
    ):
        os.close(descriptor)
        raise BindingError(
            "control directory must be private and owned by the output user"
        )
    return descriptor


def wait_for_owned_marker(
    directory_fd: int, name: str, output_uid: int, output_gid: int
) -> None:
    deadline = time.monotonic() + WAIT_TIMEOUT_SECONDS
    while True:
        try:
            metadata = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            if (
                stat.S_ISREG(metadata.st_mode)
                and metadata.st_uid == output_uid
                and metadata.st_gid == output_gid
                and stat.S_IMODE(metadata.st_mode) == 0o600
                and metadata.st_size == 0
            ):
                return
            raise BindingError(f"{name} has an invalid identity or mode")
        except FileNotFoundError:
            if time.monotonic() >= deadline:
                raise BindingError(f"timed out waiting for {name}") from None
            time.sleep(WAIT_INTERVAL_SECONDS)


def read_owned_request(
    directory_fd: int, output_uid: int, output_gid: int
) -> dict[str, str]:
    descriptor = os.open(
        "process-request.tsv",
        os.O_RDONLY | os.O_NOFOLLOW,
        dir_fd=directory_fd,
    )
    try:
        metadata = os.fstat(descriptor)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != output_uid
            or metadata.st_gid != output_gid
            or stat.S_IMODE(metadata.st_mode) != 0o600
            or metadata.st_size > 4096
        ):
            raise BindingError("process request has an invalid identity, mode, or size")
        with os.fdopen(descriptor, "r", encoding="ascii", closefd=False) as source:
            lines = source.read().splitlines()
    finally:
        os.close(descriptor)
    if lines[:1] != ["field\tvalue"]:
        raise BindingError("process request has an invalid header")
    values: dict[str, str] = {}
    for line in lines[1:]:
        fields = line.split("\t")
        if len(fields) != 2 or fields[0] in values:
            raise BindingError("process request has an invalid row")
        values[fields[0]] = fields[1]
    expected = {
        "namespace_pid",
        "container_init_pid",
        "agent_bin",
        "proxy_pid",
        "proxy_bin",
    }
    if set(values) != expected:
        raise BindingError("process request has an invalid field set")
    for name in ("namespace_pid", "container_init_pid", "proxy_pid"):
        positive_integer(values[name])
    for name in ("agent_bin", "proxy_bin"):
        path = pathlib.Path(values[name])
        if not path.is_absolute() or path.is_symlink() or not path.is_file():
            raise BindingError(f"{name} must be an absolute non-symlink regular file")
    return values


def write_owned_file(
    directory_fd: int,
    name: str,
    content: str,
    output_uid: int,
    output_gid: int,
) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
    descriptor = os.open(name, flags, 0o600, dir_fd=directory_fd)
    try:
        os.fchown(descriptor, output_uid, output_gid)
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "w", encoding="ascii", closefd=False) as sink:
            sink.write(content)
            sink.flush()
            os.fsync(descriptor)
    finally:
        os.close(descriptor)


def main() -> int:
    args = parse_args()
    sudo_uid = os.environ.get("SUDO_UID")
    sudo_gid = os.environ.get("SUDO_GID")
    if (
        os.geteuid() != 0
        or sudo_uid is None
        or sudo_gid is None
        or args.output_uid != int(sudo_uid)
        or args.output_gid != int(sudo_gid)
    ):
        raise BindingError(
            "process binder must run through sudo and return evidence to the invoking user"
        )
    control_fd = open_control_directory(
        args.control_dir, args.output_uid, args.output_gid
    )
    try:
        wait_for_owned_marker(
            control_fd, "process-request-ready", args.output_uid, args.output_gid
        )
        request = read_owned_request(control_fd, args.output_uid, args.output_gid)
        agent_host_pid = find_agent_host_pid(
            int(request["namespace_pid"]),
            int(request["container_init_pid"]),
            pathlib.Path(request["agent_bin"]),
        )
        content = "process\tpid\tstarttime\texe_sha256\n"
        content += bind_process(
            "proxy", int(request["proxy_pid"]), pathlib.Path(request["proxy_bin"])
        )
        content += bind_process(
            "gateway", agent_host_pid, pathlib.Path(request["agent_bin"])
        )
        write_owned_file(
            control_fd,
            "pids.tsv",
            content,
            args.output_uid,
            args.output_gid,
        )
        write_owned_file(
            control_fd,
            "pids-bound",
            "",
            args.output_uid,
            args.output_gid,
        )
    finally:
        os.close(control_fd)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, argparse.ArgumentTypeError, BindingError) as error:
        raise SystemExit(f"process binding failed: {error}") from error
