from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import base64
import json
import shlex

from .hosts import Host, Inventory
from .ssh import check, remote_check, run, scp_to


BINARIES = [
    "aw-gateway",
    "aw-container-agent",
    "aw-container-bootstrap",
    "aw-ssh-command-filter",
]


@dataclass(frozen=True)
class DeployOptions:
    build: bool = True
    image: bool = True


def deploy_host(inventory: Inventory, host: Host, options: DeployOptions) -> None:
    if options.build:
        build_release(inventory)

    generated_host_dir = inventory.generated_dir / host.name
    generated_host_dir.mkdir(parents=True, exist_ok=True)
    rendered_config = render_gateway_config(inventory, host, generated_host_dir)
    rendered_local_config = render_gateway_config(
        inventory,
        host,
        generated_host_dir,
        config_example=host.local_config_example,
        output_name="gateway-local.toml",
    )
    rendered_limited_http_config = render_gateway_config(
        inventory,
        host,
        generated_host_dir,
        http_actions=["status", "targets"],
        output_name="gateway-http-limited.toml",
    )

    remote_tmp = f"/tmp/aw-gateway-smoke-{host.name}"
    remote_check(host.ssh, f"rm -rf {shlex.quote(remote_tmp)} && mkdir -p {shlex.quote(remote_tmp)}")

    if host.runtime == "colima":
        if options.build:
            build_remote_macos_gateway(inventory, host, remote_tmp)
        else:
            remote_check(
                host.ssh,
                f"cp {shlex.quote(host.gateway_path)} {shlex.quote(remote_tmp)}/aw-gateway",
            )
        runtime_binaries = [binary for binary in BINARIES if binary != "aw-gateway"]
    else:
        runtime_binaries = BINARIES

    for binary in runtime_binaries:
        source = inventory.repo_root / "target" / "release" / binary
        if not source.exists():
            raise FileNotFoundError(f"missing built binary: {source}")
        scp_to(host.ssh, source, f"{remote_tmp}/{binary}").assert_success()

    helper_dir = inventory.repo_root / host.image_context
    for helper in ["start-container-sshd", "sshd_config_agent"]:
        scp_to(host.ssh, helper_dir / helper, f"{remote_tmp}/{helper}").assert_success()
    scp_to(host.ssh, rendered_config, f"{remote_tmp}/gateway.toml").assert_success()
    scp_to(host.ssh, rendered_local_config, f"{remote_tmp}/gateway-local.toml").assert_success()
    scp_to(host.ssh, rendered_limited_http_config, f"{remote_tmp}/gateway-http-limited.toml").assert_success()

    install_remote_files(host, remote_tmp)

    if options.image:
        build_remote_image(inventory, host)


def setup_restricted_user(inventory: Inventory, host: Host, public_key: Path) -> None:
    if not public_key.exists():
        raise FileNotFoundError(public_key)

    if host.runtime == "colima":
        raise NotImplementedError("macOS restricted setup is pending host-specific details")

    user = host.restricted_user
    user_q = shlex.quote(user)
    install_root = host.restricted_install_root
    generated_host_dir = inventory.generated_dir / host.name / "restricted"
    generated_host_dir.mkdir(parents=True, exist_ok=True)
    rendered_config = render_gateway_config(
        inventory,
        host,
        generated_host_dir,
        install_root=install_root,
        ephemeral_target=True,
    )
    match_config = render_sshd_match_config(host, generated_host_dir, install_root=install_root)

    pubkey_b64 = base64.b64encode(public_key.read_bytes()).decode("ascii")
    extra_group = "docker" if host.runtime == "docker" else ""
    create_user = f"""
set -euo pipefail
sudo groupadd -f aw-gateway-users
if ! id -u {user_q} >/dev/null 2>&1; then
  sudo useradd -m -s /bin/bash {user_q}
fi
sudo usermod -aG aw-gateway-users {user_q}
if [ -n {shlex.quote(extra_group)} ]; then
  sudo usermod -aG {shlex.quote(extra_group)} {user_q}
fi
sudo install -d -m 0700 -o {user_q} -g {user_q} /home/{user_q}/.ssh
key_tmp=$(mktemp)
trap 'rm -f "${{key_tmp}}"' EXIT
printf %s {shlex.quote(pubkey_b64)} | base64 -d >"${{key_tmp}}"
if ! sudo test -f /home/{user_q}/.ssh/authorized_keys || ! sudo cmp -s "${{key_tmp}}" /home/{user_q}/.ssh/authorized_keys; then
  sudo install -m 0600 -o {user_q} -g {user_q} "${{key_tmp}}" /home/{user_q}/.ssh/authorized_keys
fi
"""
    remote_check(host.ssh, create_user, timeout=120)

    remote_tmp = f"/tmp/aw-gateway-smoke-{host.name}-restricted"
    remote_check(host.ssh, f"rm -rf {shlex.quote(remote_tmp)} && mkdir -p {shlex.quote(remote_tmp)}")
    match_config_name = f"99-aw-gateway-smoke-{user}.conf"
    match_config_remote = f"{remote_tmp}/{match_config_name}"
    match_config_remote_q = shlex.quote(match_config_remote)
    sshd_config_q = shlex.quote(f"/etc/ssh/sshd_config.d/{match_config_name}")

    for binary in BINARIES:
        scp_to(host.ssh, inventory.repo_root / "target" / "release" / binary, f"{remote_tmp}/{binary}").assert_success()
    helper_dir = inventory.repo_root / host.image_context
    for helper in ["start-container-sshd", "sshd_config_agent"]:
        scp_to(host.ssh, helper_dir / helper, f"{remote_tmp}/{helper}").assert_success()
    scp_to(host.ssh, rendered_config, f"{remote_tmp}/gateway.toml").assert_success()
    scp_to(host.ssh, match_config, match_config_remote).assert_success()

    if install_root.startswith(f"/home/{user}/"):
        owner_args = f"-o {user_q} -g {user_q}"
    else:
        owner_args = ""
    install = f"""
set -euo pipefail
sudo install -d -m 0755 {owner_args} {shlex.quote(install_root)}/bin {shlex.quote(install_root)}/etc {shlex.quote(install_root)}/runtime/linux
sudo install -m 0755 {owner_args} {shlex.quote(remote_tmp)}/aw-gateway {shlex.quote(install_root)}/bin/aw-gateway
sudo install -m 0755 {owner_args} {shlex.quote(remote_tmp)}/aw-container-agent {shlex.quote(install_root)}/runtime/linux/aw-container-agent
sudo install -m 0755 {owner_args} {shlex.quote(remote_tmp)}/aw-container-bootstrap {shlex.quote(install_root)}/runtime/linux/aw-container-bootstrap
sudo install -m 0755 {owner_args} {shlex.quote(remote_tmp)}/aw-ssh-command-filter {shlex.quote(install_root)}/runtime/linux/aw-ssh-command-filter
sudo install -m 0755 {owner_args} {shlex.quote(remote_tmp)}/start-container-sshd {shlex.quote(install_root)}/runtime/linux/start-container-sshd
sudo install -m 0644 {owner_args} {shlex.quote(remote_tmp)}/sshd_config_agent {shlex.quote(install_root)}/runtime/linux/sshd_config_agent
sudo install -m 0644 {owner_args} {shlex.quote(remote_tmp)}/gateway.toml {shlex.quote(install_root)}/etc/gateway.toml
sshd_config_changed=0
sshd_config={sshd_config_q}
if ! sudo test -f "${{sshd_config}}" || ! sudo cmp -s {match_config_remote_q} "${{sshd_config}}"; then
  sudo install -m 0644 {match_config_remote_q} "${{sshd_config}}"
  sshd_config_changed=1
fi
sudo sshd -t
if [ "${{sshd_config_changed}}" = "1" ]; then
  sudo systemctl reload sshd 2>/dev/null || sudo systemctl reload ssh 2>/dev/null || sudo service sshd reload 2>/dev/null || sudo service ssh reload
fi
"""
    remote_check(host.ssh, install, timeout=120)

    if host.runtime == "podman":
        configure_restricted_podman_user(host, user)
        build_remote_image_for_user(inventory, host, user)


def build_release(inventory: Inventory) -> None:
    check(["cargo", "build", "--release"], cwd=inventory.repo_root, timeout=1200)


def render_gateway_config(
    inventory: Inventory,
    host: Host,
    output_dir: Path,
    *,
    install_root: str | None = None,
    ephemeral_target: bool = False,
    config_example: str | None = None,
    output_name: str = "gateway.toml",
    http_actions: list[str] | None = None,
) -> Path:
    source = inventory.repo_root / (config_example or host.config_example)
    root = install_root or host.install_root
    text = source.read_text()
    if host.runtime == "colima":
        text = text.replace("/Users/example/aw-gateway", root)
        text = set_toml_string(text, "program", host.docker_program, section="runtime")
        text = append_control_socket_override(text, host)
    else:
        text = replace_mount_sources(text, "/opt/aw-gateway", root)
    if ephemeral_target:
        text = text.replace('mode = "fixed"', 'mode = "ephemeral"', 1)
        text = text.replace('name = "{image_slug}"', 'ephemeral_name = "{image_slug}-{session_id}"', 1)
        text = text.replace("remove_on_stop = false", "remove_on_stop = true", 1)
    text = ensure_enabled_action(text, "run")
    text = ensure_ssh_command_filter_env(text)
    text = append_http_smoke_config(text, host, http_actions=http_actions)
    text = append_smoke_launch(text, host)
    text = replace_toml_string(text, "host", host.ssh, section="client_config")
    text = replace_toml_string(text, "gateway_path", f"{root}/bin/aw-gateway", section="client_config")
    output = output_dir / output_name
    output.write_text(text)
    return output


def build_remote_macos_gateway(inventory: Inventory, host: Host, remote_tmp: str) -> None:
    remote_source = f"/tmp/aw-gateway-source-{host.name}"
    remote_command = (
        f"rm -rf {remote_source} && "
        f"mkdir -p {remote_source} && "
        f"tar -C {remote_source} -xf - && "
        f"cd {remote_source} && "
        f"cargo build --release --bin aw-gateway && "
        f"cp target/release/aw-gateway {shlex.quote(remote_tmp)}/aw-gateway"
    )
    command = (
        f"tar -C {shlex.quote(str(inventory.repo_root))} "
        "--exclude ./.git --exclude ./target -cf - . | "
        f"ssh -o BatchMode=yes -o ConnectTimeout=10 {shlex.quote(host.ssh)} "
        f"{shlex.quote(remote_command)}"
    )
    result = run(["bash", "-lc", command], timeout=1800)
    result.assert_success()


def render_sshd_match_config(host: Host, output_dir: Path, *, install_root: str) -> Path:
    user = host.restricted_user
    text = f"""# Managed by aw-gateway-smoke.
Match User {user}
    ForceCommand {install_root}/bin/aw-gateway --config {install_root}/etc/gateway.toml
    PermitTTY yes
    AllowTcpForwarding no
    AllowStreamLocalForwarding no
    PermitTunnel no
    X11Forwarding no
    AllowAgentForwarding no
"""
    output = output_dir / f"99-aw-gateway-smoke-{user}.conf"
    output.write_text(text)
    return output


def replace_mount_sources(text: str, old_root: str, new_root: str) -> str:
    lines = text.splitlines()
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith('source = "') and old_root in stripped:
            lines[index] = line.replace(old_root, new_root, 1)
    return "\n".join(lines) + "\n"


def replace_toml_string(text: str, key: str, value: str, *, section: str) -> str:
    lines = text.splitlines()
    in_section = False
    replaced = False
    header = f"[{section}]"
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_section = stripped == header
        if in_section and stripped.startswith(f"{key} "):
            prefix = line[: len(line) - len(line.lstrip())]
            lines[index] = f'{prefix}{key} = "{value}"'
            replaced = True
    if not replaced:
        raise ValueError(f"did not find {key!r} in section {section!r}")
    return "\n".join(lines) + "\n"


def set_toml_string(text: str, key: str, value: str, *, section: str) -> str:
    lines = text.splitlines()
    in_section = False
    header_index: int | None = None
    header = f"[{section}]"
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            if in_section:
                lines.insert(index, f'{key} = "{value}"')
                return "\n".join(lines) + "\n"
            in_section = stripped == header
            if in_section:
                header_index = index
                continue
        if in_section and stripped.startswith(f"{key} "):
            prefix = line[: len(line) - len(line.lstrip())]
            lines[index] = f'{prefix}{key} = "{value}"'
            return "\n".join(lines) + "\n"
    if header_index is None:
        raise ValueError(f"did not find section {section!r}")
    lines.insert(header_index + 1, f'{key} = "{value}"')
    return "\n".join(lines) + "\n"


def ensure_enabled_action(text: str, action: str) -> str:
    lines = text.splitlines()
    in_actions = False
    insert_at: int | None = None
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped == "enabled_actions = [":
            in_actions = True
            continue
        if in_actions:
            if stripped == f'"{action}",':
                return text
            if stripped == "]":
                insert_at = index
                break
    if insert_at is None:
        raise ValueError("did not find ssh_dispatch.enabled_actions")
    lines.insert(insert_at, f'  "{action}",')
    return "\n".join(lines) + "\n"


def append_control_socket_override(text: str, host: Host) -> str:
    if "[target_defaults.control_sockets]" in text:
        return set_toml_string(
            text,
            "host_dir",
            f"{host.home_dir}/.cache/aw-gateway/sockets/{{runtime_id}}",
            section="target_defaults.control_sockets",
        )
    return (
        text.rstrip()
        + "\n\n[target_defaults.control_sockets]\n"
        + f'host_dir = "{host.home_dir}/.cache/aw-gateway/sockets/{{runtime_id}}"\n'
    )


def ensure_ssh_command_filter_env(text: str) -> str:
    filter_path: str | None = None
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("target = ") and "aw-ssh-command-filter" in stripped:
            parts = stripped.split('"')
            if len(parts) >= 3:
                filter_path = parts[1]
                break
    if filter_path is None or "AW_SSH_COMMAND_FILTER" in text:
        return text

    lines = text.splitlines()
    in_services = False
    in_container_sshd = False
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped == "[[target_defaults.container_agent.services]]":
            if in_container_sshd:
                lines.insert(index, ssh_command_filter_env_block(filter_path))
                return "\n".join(lines) + "\n"
            in_services = True
            in_container_sshd = False
            continue
        if stripped.startswith("[") and not stripped.startswith("[target_defaults.container_agent.services."):
            if in_container_sshd:
                lines.insert(index, ssh_command_filter_env_block(filter_path))
                return "\n".join(lines) + "\n"
            in_services = False
            in_container_sshd = False
            continue
        if in_services and stripped == 'name = "container-sshd"':
            in_container_sshd = True
    if in_container_sshd:
        lines.append(ssh_command_filter_env_block(filter_path))
        return "\n".join(lines) + "\n"
    raise ValueError("did not find container-sshd service")


def ssh_command_filter_env_block(filter_path: str) -> str:
    return (
        "\n[target_defaults.container_agent.services.env.AW_SSH_COMMAND_FILTER]"
        f'\nvalue = "{filter_path}"'
    )


def append_http_smoke_config(text: str, host: Host, *, http_actions: list[str] | None = None) -> str:
    actions = http_actions or [
        "status",
        "targets",
        "launches",
        "launch",
        "launch-run",
        "up",
        "run",
        "stop",
        "remove",
    ]
    action_lines = "\n".join(f'  "{action}",' for action in actions)
    return (
        text.rstrip()
        + "\n\n[http]\n"
        + "enabled = true\n"
        + f'listen = "127.0.0.1:{host.http_port}"\n'
        + "enabled_actions = [\n"
        + action_lines
        + "\n]\n\n"
        + "[http.auth]\n"
        + 'type = "bearer"\n'
        + f"token = {json.dumps(host.http_token)}\n"
    )


def append_smoke_launch(text: str, host: Host) -> str:
    return (
        text.rstrip()
        + "\n\n[launches.smoke-echo]\n"
        + f'target = "{host.target}"\n'
        + 'description = "HTTP API smoke launch."\n'
        + 'env = { SMOKE_NAME = "{var.name}", SMOKE_FLAG = "{var.flag}" }\n'
        + 'command = ["/bin/bash", "-lc", "printf \'smoke:%s:%s\' \\"$SMOKE_NAME\\" \\"$SMOKE_FLAG\\""]\n\n'
        + "[launches.smoke-echo.vars]\n"
        + 'name = { type = "string", required = true }\n'
        + 'flag = { type = "boolean", default = false }\n'
        + "\n\n[launches.smoke-args]\n"
        + f'target = "{host.target}"\n'
        + 'description = "HTTP API smoke launch with passthrough args."\n'
        + "allow_args = true\n"
        + 'command = ["/bin/sh", "-c", "printf \'args:%s:%s\' \\"$1\\" \\"$2\\"", "aw-smoke", "{args}"]\n'
    )


def install_remote_files(host: Host, remote_tmp: str) -> None:
    root = shlex.quote(host.install_root)
    tmp = shlex.quote(remote_tmp)
    if host.install_mode == "sudo":
        prefix = "sudo "
    elif host.install_mode == "user":
        prefix = ""
    else:
        raise ValueError(f"unsupported install_mode {host.install_mode!r}")

    command = f"""
set -euo pipefail
{prefix}install -d -m 0755 {root}/bin {root}/etc {root}/runtime/linux
{prefix}install -m 0755 {tmp}/aw-gateway {root}/bin/aw-gateway
{prefix}install -m 0755 {tmp}/aw-container-agent {root}/runtime/linux/aw-container-agent
{prefix}install -m 0755 {tmp}/aw-container-bootstrap {root}/runtime/linux/aw-container-bootstrap
{prefix}install -m 0755 {tmp}/aw-ssh-command-filter {root}/runtime/linux/aw-ssh-command-filter
{prefix}install -m 0755 {tmp}/start-container-sshd {root}/runtime/linux/start-container-sshd
{prefix}install -m 0644 {tmp}/sshd_config_agent {root}/runtime/linux/sshd_config_agent
{prefix}install -m 0644 {tmp}/gateway.toml {root}/etc/gateway.toml
{prefix}install -m 0644 {tmp}/gateway-local.toml {root}/etc/gateway-local.toml
{prefix}install -m 0644 {tmp}/gateway-http-limited.toml {root}/etc/gateway-http-limited.toml
"""
    remote_check(host.ssh, command, timeout=120)


def build_remote_image(inventory: Inventory, host: Host) -> None:
    context = inventory.repo_root / host.image_context
    runtime = shlex.quote(runtime_program(host))
    env = ""
    if host.runtime == "colima":
        env = f"export DOCKER_HOST={shlex.quote(host.colima_docker_host)}; "
    remote_context = f"/tmp/aw-gateway-image-{host.name}"
    remote_command = (
        f"rm -rf {remote_context} && "
        f"mkdir -p {remote_context} && "
        f"tar -C {remote_context} -xf - && "
        f"{env}{runtime} build -t {host.image} -f {remote_context}/Containerfile.ubuntu {remote_context}"
    )
    command = (
        f"tar -C {shlex.quote(str(context))} -cf - . | "
        f"ssh -o BatchMode=yes -o ConnectTimeout=10 {shlex.quote(host.ssh)} "
        f"{shlex.quote(remote_command)}"
    )
    result = run(["bash", "-lc", command], timeout=1800)
    result.assert_success()


def build_remote_image_for_user(inventory: Inventory, host: Host, user: str) -> None:
    context = inventory.repo_root / host.image_context
    remote_context = f"/tmp/aw-gateway-image-{host.name}-{user}"
    remote_command = (
        f"rm -rf {remote_context} && "
        f"mkdir -p {remote_context} && "
        f"tar -C {remote_context} -xf - && "
        f"sudo -H -u {user} bash -lc "
        f"{shlex.quote(f'cd /tmp && podman build -t {host.image} -f {remote_context}/Containerfile.ubuntu {remote_context}')}"
    )
    command = (
        f"tar -C {shlex.quote(str(context))} -cf - . | "
        f"ssh -o BatchMode=yes -o ConnectTimeout=10 {shlex.quote(host.ssh)} "
        f"{shlex.quote(remote_command)}"
    )
    result = run(["bash", "-lc", command], timeout=1800)
    result.assert_success()


def configure_restricted_podman_user(host: Host, user: str) -> None:
    command = f"""
set -euo pipefail
if command -v loginctl >/dev/null 2>&1; then
  sudo loginctl enable-linger {shlex.quote(user)}
fi
sudo -H -u {shlex.quote(user)} bash -lc 'cd /tmp && podman info >/dev/null'
"""
    remote_check(host.ssh, command, timeout=120)


def runtime_program(host: Host) -> str:
    if host.runtime == "colima":
        return host.docker_program
    if host.runtime == "docker":
        return "docker"
    if host.runtime == "podman":
        return "podman"
    raise ValueError(f"unsupported runtime {host.runtime!r}")
