from __future__ import annotations

from pathlib import Path

from .command import DEFAULT_TIMEOUT, CommandResult, SshRunner, check, run


def remote(host: str, command: str, *, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
    return SshRunner(host).run(command, timeout=timeout)


def remote_check(host: str, command: str, *, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
    return remote(host, command, timeout=timeout).assert_success()


def scp_to(host: str, source: Path, destination: str, *, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
    return SshRunner(host).copy_to(source, destination, timeout=timeout)
