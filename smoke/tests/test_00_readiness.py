from __future__ import annotations

from awsmoke.hosts import Host


def test_host_transport_connects(host: Host) -> None:
    result = host.run("true")
    result.assert_success()


def test_passwordless_sudo_when_required(host: Host) -> None:
    if not host.requires_sudo:
        return
    result = host.run("sudo -n true")
    result.assert_success()


def test_runtime_available_to_user(host: Host) -> None:
    if host.runtime == "docker":
        command = "docker ps --format '{{.ID}}' >/dev/null"
    elif host.runtime == "podman":
        command = "podman info >/dev/null"
    elif host.runtime == "colima":
        command = (
            f"PATH={host.local_bin_dir}:$PATH {host.colima_program} status --profile aw-gateway >/dev/null && "
            f"DOCKER_HOST={host.colima_docker_host} {host.docker_program} version >/dev/null"
        )
    elif host.runtime == "apple_container":
        command = "container system status --format json >/dev/null"
    else:
        raise AssertionError(f"unsupported runtime {host.runtime}")
    result = host.run(command, timeout=120)
    result.assert_success()


def test_git_available(host: Host) -> None:
    result = host.run("git --version")
    result.assert_success()
