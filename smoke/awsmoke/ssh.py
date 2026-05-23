from __future__ import annotations

from dataclasses import dataclass
import shlex
import subprocess
from pathlib import Path


DEFAULT_TIMEOUT = 60


@dataclass(frozen=True)
class CommandResult:
    argv: list[str]
    returncode: int
    stdout: str
    stderr: str

    @property
    def command_text(self) -> str:
        return " ".join(shlex.quote(part) for part in self.argv)

    def assert_success(self) -> "CommandResult":
        if self.returncode != 0:
            raise AssertionError(
                f"command failed ({self.returncode}): {self.command_text}\n"
                f"stdout:\n{self.stdout}\n"
                f"stderr:\n{self.stderr}"
            )
        return self


def run(argv: list[str], *, cwd: Path | None = None, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
    completed = subprocess.run(
        argv,
        cwd=cwd,
        timeout=timeout,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    return CommandResult(
        argv=argv,
        returncode=completed.returncode,
        stdout=completed.stdout,
        stderr=completed.stderr,
    )


def check(argv: list[str], *, cwd: Path | None = None, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
    return run(argv, cwd=cwd, timeout=timeout).assert_success()


def remote(host: str, command: str, *, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
    return run(
        [
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            host,
            command,
        ],
        timeout=timeout,
    )


def remote_check(host: str, command: str, *, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
    return remote(host, command, timeout=timeout).assert_success()


def scp_to(host: str, source: Path, destination: str, *, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
    return run(
        [
            "scp",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            str(source),
            f"{host}:{destination}",
        ],
        timeout=timeout,
    )
