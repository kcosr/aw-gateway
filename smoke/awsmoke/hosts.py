from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import tomllib

SAFE_RESTRICTED_USER = re.compile(r"^[A-Za-z0-9_-]+$")


@dataclass(frozen=True)
class Host:
    name: str
    ssh: str
    runtime: str
    install_root: str
    install_mode: str
    requires_sudo: bool
    config_example: str
    image_context: str
    image: str
    target: str
    enabled: bool
    restricted_user: str
    restricted_install_root: str
    http_port: int

    @property
    def gateway_path(self) -> str:
        return f"{self.install_root}/bin/aw-gateway"

    @property
    def config_path(self) -> str:
        return f"{self.install_root}/etc/gateway.toml"

    @property
    def local_config_path(self) -> str:
        return f"{self.install_root}/etc/gateway-local.toml"

    @property
    def runtime_exec_config_path(self) -> str:
        return f"{self.install_root}/etc/gateway-runtime-exec.toml"

    @property
    def http_limited_config_path(self) -> str:
        return f"{self.install_root}/etc/gateway-http-limited.toml"

    @property
    def http_token(self) -> str:
        return f"aw-gateway-smoke-http-{self.name}"

    @property
    def local_config_example(self) -> str:
        if self.runtime == "docker":
            return "examples/docker/gateway-local.toml"
        if self.runtime == "podman":
            return "examples/podman/gateway-local.toml"
        if self.runtime == "colima":
            return "examples/colima/gateway-local.toml"
        raise ValueError(f"unsupported runtime {self.runtime!r}")

    @property
    def runtime_exec_config_example(self) -> str:
        if self.runtime == "docker":
            return "examples/docker/gateway-runtime-exec.toml"
        if self.runtime == "podman":
            return "examples/podman/gateway-runtime-exec.toml"
        if self.runtime == "colima":
            return "examples/colima/gateway-runtime-exec.toml"
        raise ValueError(f"unsupported runtime {self.runtime!r}")

    @property
    def home_dir(self) -> str:
        if self.install_root.startswith("/Users/"):
            parts = self.install_root.split("/")
            return f"/Users/{parts[2]}"
        if self.install_root.startswith("/home/"):
            parts = self.install_root.split("/")
            return f"/home/{parts[2]}"
        return "~"

    @property
    def local_bin_dir(self) -> str:
        return f"{self.home_dir}/.local/bin"

    @property
    def docker_program(self) -> str:
        if self.runtime == "colima":
            return f"{self.local_bin_dir}/docker"
        return "docker"

    @property
    def colima_program(self) -> str:
        return f"{self.local_bin_dir}/colima"

    @property
    def colima_docker_host(self) -> str:
        return f"unix://{self.home_dir}/.colima/aw-gateway/docker.sock"


@dataclass(frozen=True)
class Inventory:
    root: Path
    repo_root: Path
    generated_dir: Path
    hosts: dict[str, Host]

    def enabled_hosts(self) -> list[Host]:
        return [host for host in self.hosts.values() if host.enabled]

    def selected_hosts(self, name: str | None) -> list[Host]:
        if name:
            if name not in self.hosts:
                known = ", ".join(sorted(self.hosts))
                raise KeyError(f"unknown host {name!r}; known hosts: {known}")
            return [self.hosts[name]]
        return self.enabled_hosts()


def load_inventory(path: str | Path = "inventory.toml") -> Inventory:
    inventory_path = Path(path).resolve()
    data = tomllib.loads(inventory_path.read_text())
    defaults = data.get("defaults", {})
    root = inventory_path.parent
    repo_root = (root / defaults.get("repo_root", "../aw-gateway")).resolve()
    generated_dir = (root / defaults.get("generated_dir", "generated")).resolve()
    default_target = defaults.get("target", "ubuntu")

    hosts: dict[str, Host] = {}
    for name, raw in data.get("hosts", {}).items():
        restricted_user = raw.get("restricted_user", "awsmoke")
        if not SAFE_RESTRICTED_USER.fullmatch(restricted_user):
            raise ValueError(
                f"hosts.{name}.restricted_user must contain only letters, digits, '_' or '-'"
            )
        hosts[name] = Host(
            name=name,
            ssh=raw["ssh"],
            runtime=raw["runtime"],
            install_root=raw["install_root"],
            install_mode=raw.get("install_mode", "sudo"),
            requires_sudo=bool(raw.get("requires_sudo", False)),
            config_example=raw["config_example"],
            image_context=raw["image_context"],
            image=raw["image"],
            target=raw.get("target", default_target),
            enabled=bool(raw.get("enabled", True)),
            restricted_user=restricted_user,
            restricted_install_root=raw.get("restricted_install_root", raw["install_root"]),
            http_port=int(raw.get("http_port", 18080)),
        )

    return Inventory(
        root=root,
        repo_root=repo_root,
        generated_dir=generated_dir,
        hosts=hosts,
    )
