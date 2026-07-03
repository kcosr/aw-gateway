from __future__ import annotations

from .hosts import Host
from .ssh import CommandResult, remote, remote_check


def gateway_command(host: Host, *args: str) -> str:
    return gateway_command_for_config(host, host.config_path, *args)


def local_gateway_command(host: Host, *args: str) -> str:
    return gateway_command_for_config(host, host.local_config_path, *args)


def runtime_exec_gateway_command(host: Host, *args: str) -> str:
    return gateway_command_for_config(host, host.runtime_exec_config_path, *args)


def gateway_command_for_config(host: Host, config_path: str, *args: str) -> str:
    quoted_args = " ".join(_quote(arg) for arg in args)
    return f"{_quote(host.gateway_path)} --config {_quote(config_path)} {quoted_args}".strip()


def gateway(host: Host, *args: str, timeout: int = 60) -> CommandResult:
    return remote(host.ssh, gateway_command(host, *args), timeout=timeout)


def gateway_check(host: Host, *args: str, timeout: int = 60) -> CommandResult:
    return remote_check(host.ssh, gateway_command(host, *args), timeout=timeout)


def local_gateway(host: Host, *args: str, timeout: int = 60) -> CommandResult:
    return remote(host.ssh, local_gateway_command(host, *args), timeout=timeout)


def runtime_exec_gateway(host: Host, *args: str, timeout: int = 60) -> CommandResult:
    return remote(host.ssh, runtime_exec_gateway_command(host, *args), timeout=timeout)


def _quote(value: str) -> str:
    import shlex

    return shlex.quote(value)
