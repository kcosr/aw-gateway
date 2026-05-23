from __future__ import annotations

import argparse
import sys

from pathlib import Path

from .deploy import DeployOptions, deploy_host, setup_restricted_user
from .hosts import load_inventory
from .ssh import remote


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="awsmoke")
    parser.add_argument("--inventory", default="inventory.toml")
    subcommands = parser.add_subparsers(dest="command", required=True)

    subcommands.add_parser("hosts")

    check_parser = subcommands.add_parser("check")
    check_parser.add_argument("host", nargs="?")

    deploy_parser = subcommands.add_parser("deploy")
    deploy_parser.add_argument("host")
    deploy_parser.add_argument("--skip-build", action="store_true")
    deploy_parser.add_argument("--skip-image", action="store_true")

    restricted_parser = subcommands.add_parser("setup-restricted")
    restricted_parser.add_argument("host")
    restricted_parser.add_argument("--public-key", default="~/.ssh/id_rsa.pub")

    args = parser.parse_args(argv)
    inventory = load_inventory(args.inventory)

    if args.command == "hosts":
        for host in inventory.hosts.values():
            state = "enabled" if host.enabled else "disabled"
            print(f"{host.name}\t{state}\tssh={host.ssh}\truntime={host.runtime}\tinstall={host.install_root}")
        return 0

    if args.command == "check":
        ok = True
        for host in inventory.selected_hosts(args.host):
            result = remote(host.ssh, "true")
            if result.returncode == 0:
                print(f"{host.name}: ssh ok")
            else:
                ok = False
                print(f"{host.name}: ssh failed", file=sys.stderr)
                print(result.stderr, file=sys.stderr)
        return 0 if ok else 1

    if args.command == "deploy":
        host = inventory.hosts[args.host]
        deploy_host(
            inventory,
            host,
            DeployOptions(build=not args.skip_build, image=not args.skip_image),
        )
        print(f"{host.name}: deployed")
        return 0

    if args.command == "setup-restricted":
        host = inventory.hosts[args.host]
        setup_restricted_user(inventory, host, Path(args.public_key).expanduser())
        print(f"{host.name}: restricted user ready")
        return 0

    raise AssertionError(f"unhandled command {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
