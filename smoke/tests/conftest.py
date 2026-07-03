from __future__ import annotations

import os

import pytest

from awsmoke.hosts import Host, load_inventory


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.addoption(
        "--inventory",
        default=os.environ.get("AWSMOKE_INVENTORY", "inventory.toml"),
        help="Path to smoke inventory TOML",
    )
    parser.addoption("--host", action="append", help="Host name from inventory; repeatable")
    parser.addoption("--include-disabled", action="store_true", help="Allow disabled inventory hosts")


def selected_host_names(pytestconfig: pytest.Config) -> list[str] | None:
    names = pytestconfig.getoption("--host")
    if names:
        return names
    raw = os.environ.get("AWSMOKE_HOSTS") or os.environ.get("AWSMOKE_HOST")
    if not raw:
        return None
    return [name.strip() for name in raw.split(",") if name.strip()]


@pytest.fixture(scope="session")
def inventory(pytestconfig: pytest.Config):
    return load_inventory(pytestconfig.getoption("--inventory"))


@pytest.fixture(scope="session")
def selected_hosts(pytestconfig: pytest.Config, inventory) -> list[Host]:
    names = selected_host_names(pytestconfig)
    include_disabled = pytestconfig.getoption("--include-disabled")
    if not names:
        return inventory.enabled_hosts()

    selected: list[Host] = []
    for name in names:
        host = inventory.hosts[name]
        if not host.enabled and not include_disabled:
            pytest.skip(f"host {name} is disabled in inventory")
        selected.append(host)
    return selected


def pytest_generate_tests(metafunc: pytest.Metafunc) -> None:
    if "host" not in metafunc.fixturenames:
        return
    inventory = load_inventory(metafunc.config.getoption("--inventory"))
    names = selected_host_names(metafunc.config)
    include_disabled = metafunc.config.getoption("--include-disabled")
    if names:
        hosts = [inventory.hosts[name] for name in names]
        if not include_disabled:
            hosts = [host for host in hosts if host.enabled]
    else:
        hosts = inventory.enabled_hosts()
    metafunc.parametrize("host", hosts, ids=[host.name for host in hosts])
