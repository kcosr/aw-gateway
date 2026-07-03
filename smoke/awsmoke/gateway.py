from __future__ import annotations

from .hosts import Host
from .command import CommandResult


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
    return host.run(gateway_command(host, *args), timeout=timeout)


def gateway_check(host: Host, *args: str, timeout: int = 60) -> CommandResult:
    return host.check(gateway_command(host, *args), timeout=timeout)


def local_gateway(host: Host, *args: str, timeout: int = 60) -> CommandResult:
    return host.run(local_gateway_command(host, *args), timeout=timeout)


def runtime_exec_gateway(host: Host, *args: str, timeout: int = 60) -> CommandResult:
    return host.run(runtime_exec_gateway_command(host, *args), timeout=timeout)


def _quote(value: str) -> str:
    import shlex

    return shlex.quote(value)
