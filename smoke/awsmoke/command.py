from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO
import shlex
import subprocess


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


class HostRunner:
    label = "host"

    def run(self, command: str, *, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
        raise NotImplementedError

    def check(self, command: str, *, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
        return self.run(command, timeout=timeout).assert_success()

    def copy_to(
        self,
        source: Path,
        destination: str,
        *,
        timeout: int = DEFAULT_TIMEOUT,
    ) -> CommandResult:
        raise NotImplementedError

    def tar_to_command(
        self,
        source_dir: Path,
        command: str,
        *,
        excludes: list[str] | None = None,
        timeout: int = DEFAULT_TIMEOUT,
    ) -> CommandResult:
        raise NotImplementedError

    def http_process(
        self,
        command: str,
        *,
        local_port: int,
        remote_port: int,
        stderr_file: BinaryIO,
    ) -> subprocess.Popen[str]:
        raise NotImplementedError


@dataclass(frozen=True)
class LocalRunner(HostRunner):
    label = "local"

    def run(self, command: str, *, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
        return run(["bash", "-lc", command], timeout=timeout)

    def copy_to(
        self,
        source: Path,
        destination: str,
        *,
        timeout: int = DEFAULT_TIMEOUT,
    ) -> CommandResult:
        return run(["cp", str(source), destination], timeout=timeout)

    def tar_to_command(
        self,
        source_dir: Path,
        command: str,
        *,
        excludes: list[str] | None = None,
        timeout: int = DEFAULT_TIMEOUT,
    ) -> CommandResult:
        tar_args = " ".join(shlex.quote(f"--exclude={exclude}") for exclude in (excludes or []))
        shell_command = (
            f"tar -C {shlex.quote(str(source_dir))} {tar_args} -cf - . | "
            f"bash -lc {shlex.quote(command)}"
        )
        return run(["bash", "-lc", shell_command], timeout=timeout)

    def http_process(
        self,
        command: str,
        *,
        local_port: int,
        remote_port: int,
        stderr_file: BinaryIO,
    ) -> subprocess.Popen[str]:
        return subprocess.Popen(
            ["bash", "-lc", command],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=stderr_file,
            text=True,
        )


@dataclass(frozen=True)
class SshRunner(HostRunner):
    host: str
    label = "ssh"

    def run(self, command: str, *, timeout: int = DEFAULT_TIMEOUT) -> CommandResult:
        return run(self.ssh_argv(command), timeout=timeout)

    def copy_to(
        self,
        source: Path,
        destination: str,
        *,
        timeout: int = DEFAULT_TIMEOUT,
    ) -> CommandResult:
        return run(
            [
                "scp",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                str(source),
                f"{self.host}:{destination}",
            ],
            timeout=timeout,
        )

    def tar_to_command(
        self,
        source_dir: Path,
        command: str,
        *,
        excludes: list[str] | None = None,
        timeout: int = DEFAULT_TIMEOUT,
    ) -> CommandResult:
        tar_args = " ".join(shlex.quote(f"--exclude={exclude}") for exclude in (excludes or []))
        shell_command = (
            f"tar -C {shlex.quote(str(source_dir))} {tar_args} -cf - . | "
            f"ssh -o BatchMode=yes -o ConnectTimeout=10 {shlex.quote(self.host)} {shlex.quote(command)}"
        )
        return run(["bash", "-lc", shell_command], timeout=timeout)

    def http_process(
        self,
        command: str,
        *,
        local_port: int,
        remote_port: int,
        stderr_file: BinaryIO,
    ) -> subprocess.Popen[str]:
        return subprocess.Popen(
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ExitOnForwardFailure=yes",
                "-L",
                f"127.0.0.1:{local_port}:127.0.0.1:{remote_port}",
                self.host,
                command,
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=stderr_file,
            text=True,
        )

    def ssh_argv(self, command: str) -> list[str]:
        return [
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            self.host,
            command,
        ]
