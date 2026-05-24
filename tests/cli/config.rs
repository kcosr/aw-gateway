use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

#[test]
fn gateway_config_init_then_validate() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .args(["config", "init"])
        .arg(&config)
        .assert()
        .success()
        .stdout(predicate::str::contains(config.display().to_string()));

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn agent_config_init_then_validate() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("container-agent.toml");

    Command::cargo_bin("aw-container-agent")
        .unwrap()
        .args(["config", "init"])
        .arg(&config)
        .assert()
        .success()
        .stdout(predicate::str::contains(config.display().to_string()));

    Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn gateway_rejects_unknown_config_fields() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("bad.toml");
    std::fs::write(
        &config,
        r#"
schema_version = "1"
unknown = true

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["config", "validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown field"));
}

#[test]
fn gateway_config_validate_accepts_host_hook_timeouts() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/true"]
timeout = "250ms"

[[target_defaults.host_steps]]
name = "firewall"
command = ["/bin/true"]
timeout = "1m"
"#,
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn gateway_config_validate_accepts_control_socket_overrides() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        r#"
schema_version = "1"

[target_defaults.control_sockets]
host_dir = "/tmp/aw-gateway/{runtime_id}"
container_dir = "/run/global-aw"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.control_sockets]
host_dir = "/var/tmp/aw-gateway/{runtime_id}"
container_dir = "/tmp/aw-gateway"
"#,
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn gateway_config_validate_accepts_workspace_cleanup() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "worker-{session_id}"
stop_when_idle = true

[targets.default.workspace]
path = "{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"
cleanup = "always"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#,
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn gateway_config_validate_accepts_target_and_launch_templates() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        r#"
schema_version = "1"

[target_templates.base]
image = "ubuntu/base"
mode = "fixed"
name = "base"

[target_templates.worker]
use = ["base"]
container_user = "worker"

[targets.default]
use = ["worker"]
name = "default"

[launch_templates.repo]
target = "default"
cwd = "{container_home}/repo"

[launch_templates.repo.vars]
repo = { type = "string", required = true }

[launch_templates.review]
use = ["repo"]
command = ["codex", "exec", "{var.repo}"]

[launches.code-review]
use = ["review"]
command = ["codex", "exec", "review", "{var.repo}"]
"#,
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn gateway_config_validate_accepts_unified_includes() {
    let dir = tempdir().unwrap();
    let config_dir = dir.path().join("config.d");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("workspace.toml"),
        r#"
[target_templates.base]
image = "ubuntu/base"
mode = "fixed"
name = "base"

[targets.default]
use = ["base"]

[launch_templates.shell]
target = "default"
command = ["true"]

[launches.agent]
use = ["shell"]
"#,
    )
    .unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        r#"
schema_version = "1"
includes = ["config.d/*.toml"]
"#,
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn gateway_config_validate_rejects_legacy_gateway_socket_paths() {
    for (config_name, config_body, expected) in [
        (
            "control-socket.toml",
            r#"
schema_version = "1"

[container_agent]
control_socket = "/run/aw-gateway/agent.sock"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
            "unknown field `container_agent`",
        ),
        (
            "ssh-bridge.toml",
            r#"
schema_version = "1"

[target_defaults.container_agent.ssh_bridge]
enabled = true
socket = "/run/aw-gateway/ssh.sock"
target = "127.0.0.1:22"
mode = "0600"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
            "container_agent.ssh_bridge.socket is managed by control_sockets.container_dir",
        ),
    ] {
        let dir = tempdir().unwrap();
        let config = dir.path().join(config_name);
        std::fs::write(&config, config_body).unwrap();

        Command::cargo_bin("aw-gateway")
            .unwrap()
            .arg("--config")
            .arg(&config)
            .args(["config", "validate"])
            .assert()
            .failure()
            .stderr(predicate::str::contains(expected));
    }
}
