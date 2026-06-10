use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

use crate::helpers::{
    gateway_sample_for_test, gateway_sample_with_transfer_denied, launch_config_for_test,
};

#[test]
fn ssh_up_rejects_local_listen_mode_before_serving_listener() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let mut sample = gateway_sample_for_test(&dir, &workspace);
    sample.push_str(
        r#"
[targets.default.local_ssh]
mode = "listen"
host = "127.0.0.1"
port = 40222
"#,
    );
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("SSH_ORIGINAL_COMMAND", "up default")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "gateway action \"up\" over SSH is not supported for local_ssh.mode = \"listen\" targets",
        ));
}

#[test]
fn help_over_ssh_prints_allowed_commands() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("SSH_ORIGINAL_COMMAND", "help")
        .assert()
        .success()
        .stdout(predicate::str::contains("AW Gateway commands:"))
        .stdout(predicate::str::contains("up [target]"))
        .stdout(predicate::str::contains(
            "run [--session-id ID] [target] [--cwd DIR] -- <command>",
        ))
        .stdout(predicate::str::contains("launches"))
        .stdout(predicate::str::contains("launch show <name>"))
        .stdout(predicate::str::contains(
            "launch <name> [--session-id ID] [--var key=value]",
        ))
        .stdout(predicate::str::contains("client-config [target]"))
        .stdout(predicate::str::contains("remove [target]"))
        .stdout(predicate::str::contains("help"));
}

#[test]
fn ssh_dispatch_rejects_host_side_transfer_commands_when_policy_disallows_them() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_with_transfer_denied(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("SSH_ORIGINAL_COMMAND", "scp -t /tmp/file")
        .assert()
        .failure()
        .stderr(predicate::str::contains("legacy scp is not allowed"));

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("SSH_ORIGINAL_COMMAND", "internal-sftp")
        .assert()
        .failure()
        .stderr(predicate::str::contains("sftp is not allowed"));

    for original_command in [
        "true; scp -t /tmp/file",
        "true && scp -t /tmp/file",
        "printf hi | scp -t /tmp/file",
        "x=$(scp -t /tmp/file)",
    ] {
        Command::cargo_bin("aw-gateway")
            .unwrap()
            .arg("--config")
            .arg(&config)
            .env("SSH_ORIGINAL_COMMAND", original_command)
            .assert()
            .failure()
            .stderr(predicate::str::contains("shell composition is not allowed"));
    }
}

#[test]
fn ssh_dispatch_transfer_gate_uses_defaults_not_user_default_target_override() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let home = dir.path().join("home");
    let sample = gateway_sample_with_transfer_denied(&dir, &workspace)
        + r#"

[targets.permissive]
image = "ubuntu/dev"
mode = "fixed"
name = "ubuntu-dev-permissive"

[targets.permissive.container_ssh.transfer]
sftp = "allow"
legacy_scp = "allow"
"#;
    std::fs::write(&config, sample).unwrap();
    let default_dir = home.join(".config/aw-gateway");
    std::fs::create_dir_all(&default_dir).unwrap();
    std::fs::write(default_dir.join("default-target"), "permissive\n").unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("SSH_ORIGINAL_COMMAND", "internal-sftp")
        .env("AW_GATEWAY_TEST_HOME", &home)
        .assert()
        .failure()
        .stderr(predicate::str::contains("sftp is not allowed"));
}

#[test]
fn ssh_dispatch_accepts_session_id_forms_before_runtime_validation() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    for original_command in [
        "connect default --session-id abc123def456",
        "connect --session-id abc123def456 default",
        "connect default --session-id=abc123def456",
        "run default --session-id abc123def456 -- pwd",
        "run --session-id=abc123def456 default -- pwd",
        "stop default --session-id abc123def456",
        "remove default --session-id=abc123def456",
    ] {
        Command::cargo_bin("aw-gateway")
            .unwrap()
            .arg("--config")
            .arg(&config)
            .env("SSH_ORIGINAL_COMMAND", original_command)
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "--session-id is only valid for ephemeral targets",
            ));
    }
}

#[test]
fn launch_over_ssh_accepts_session_id_forms_before_runtime_validation() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, launch_config_for_test()).unwrap();

    for original_command in [
        "launch agent-pack-codex --session-id abc123def456 --var repo=a --var pack_id=p",
        "launch agent-pack-codex --session-id=abc123def456 --var=repo=a --var pack_id=p",
    ] {
        Command::cargo_bin("aw-gateway")
            .unwrap()
            .arg("--config")
            .arg(&config)
            .env("SSH_ORIGINAL_COMMAND", original_command)
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "--session-id is only valid for ephemeral targets",
            ));
    }
}

#[test]
fn launch_over_ssh_rejects_duplicate_vars_without_panic() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, launch_config_for_test()).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env(
            "SSH_ORIGINAL_COMMAND",
            "launch agent-pack-codex --var repo=a --var repo=b --var pack_id=p",
        )
        .assert()
        .failure()
        .stderr(predicate::str::contains("duplicate launch variable"))
        .stderr(predicate::str::contains("panicked").not());
}

#[test]
fn targets_over_ssh_supports_json() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let home = dir.path().join("home");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("SSH_ORIGINAL_COMMAND", "targets --json")
        .env("AW_GATEWAY_TEST_HOME", &home)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"target\": \"default\""))
        .stdout(predicate::str::contains("\"image\": \"ubuntu/dev\""))
        .stdout(predicate::str::contains("\"container\": \"ubuntu-dev\""))
        .stdout(predicate::str::contains("\"default\": true"));
}

#[test]
fn launch_discovery_over_ssh_dispatch_uses_same_handlers() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, launch_config_for_test()).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("SSH_ORIGINAL_COMMAND", "launches --json")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"name\": \"agent-pack-codex\""))
        .stdout(predicate::str::contains("\"type\": \"enum\""));

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env(
            "SSH_ORIGINAL_COMMAND",
            "launch show agent-pack-codex --json",
        )
        .assert()
        .success()
        .stdout(predicate::str::contains("\"name\": \"agent-pack-codex\""))
        .stdout(predicate::str::contains("\"command\""));
}
