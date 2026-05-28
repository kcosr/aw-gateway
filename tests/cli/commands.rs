use assert_cmd::Command;
use aw_gateway::cli::{GatewayArgs, GatewayCommand};
use clap::Parser;
use predicates::prelude::*;
use tempfile::tempdir;

use crate::helpers::{
    gateway_sample_for_test, interrupted_cleanup_config,
    interrupted_cleanup_config_with_idle_grace, interruptible_runtime_script, signal_process,
    wait_for_file, write_executable,
};

#[test]
fn status_all_cli_parses_without_touching_up() {
    let args = GatewayArgs::try_parse_from(["aw-gateway", "status", "--all", "--json"]).unwrap();
    match args.command {
        Some(GatewayCommand::Status(status)) => {
            assert!(status.all);
            assert!(status.json);
            assert_eq!(status.target, None);
            assert_eq!(status.session_id, None);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    assert!(GatewayArgs::try_parse_from(["aw-gateway", "up", "--all"]).is_err());
}

#[test]
fn http_cli_parses_and_rejects_disabled_config() {
    let args = GatewayArgs::try_parse_from(["aw-gateway", "http"]).unwrap();
    assert!(matches!(args.command, Some(GatewayCommand::Http)));

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
"#,
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("http")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "http listener is disabled in config",
        ));
}

#[test]
fn connect_and_run_cli_parse_session_id_forms() {
    for args in [
        vec![
            "aw-gateway",
            "connect",
            "--session-id",
            "abc123def456",
            "code-review-worker",
        ],
        vec![
            "aw-gateway",
            "connect",
            "--session-id=abc123def456",
            "code-review-worker",
        ],
        vec![
            "aw-gateway",
            "connect",
            "code-review-worker",
            "--session-id",
            "abc123def456",
        ],
    ] {
        let args = GatewayArgs::try_parse_from(args).unwrap();
        match args.command {
            Some(GatewayCommand::Connect(connect)) => {
                assert_eq!(connect.target.as_deref(), Some("code-review-worker"));
                assert_eq!(connect.session_id.as_deref(), Some("abc123def456"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    for args in [
        vec![
            "aw-gateway",
            "run",
            "--session-id",
            "abc123def456",
            "code-review-worker",
            "--",
            "bash",
            "-l",
        ],
        vec![
            "aw-gateway",
            "run",
            "--session-id=abc123def456",
            "code-review-worker",
            "--",
            "bash",
            "-l",
        ],
        vec![
            "aw-gateway",
            "run",
            "code-review-worker",
            "--session-id",
            "abc123def456",
            "--",
            "bash",
            "-l",
        ],
    ] {
        let args = GatewayArgs::try_parse_from(args).unwrap();
        match args.command {
            Some(GatewayCommand::Run(run)) => {
                assert_eq!(run.target.as_deref(), Some("code-review-worker"));
                assert_eq!(run.session_id.as_deref(), Some("abc123def456"));
                assert_eq!(run.command, vec!["bash".to_string(), "-l".to_string()]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}

#[test]
fn remove_cli_parses_session_id_forms() {
    for args in [
        vec![
            "aw-gateway",
            "remove",
            "--session-id",
            "abc123def456",
            "code-review-worker",
        ],
        vec![
            "aw-gateway",
            "remove",
            "--session-id=abc123def456",
            "code-review-worker",
        ],
        vec![
            "aw-gateway",
            "remove",
            "code-review-worker",
            "--session-id",
            "abc123def456",
        ],
    ] {
        let args = GatewayArgs::try_parse_from(args).unwrap();
        match args.command {
            Some(GatewayCommand::Remove(remove)) => {
                assert_eq!(remove.target.as_deref(), Some("code-review-worker"));
                assert_eq!(remove.session_id.as_deref(), Some("abc123def456"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}

#[test]
fn status_all_cli_rejects_target_and_session_id_at_handler_boundary() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["status", "--all", "default"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--all cannot be combined with a target",
        ));

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["status", "--all", "--session-id", "x9k2p"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--all cannot be combined with --session-id",
        ));
}

#[test]
fn default_selection_commands_use_operation_results() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let home = dir.path().join("home");
    let mut sample = gateway_sample_for_test(&dir, &workspace);
    sample.push_str(
        r#"
[targets.other]
image = "fedora/dev"
mode = "fixed"
name = "fedora-dev"
"#,
    );
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["set-default", "other"])
        .env("AW_GATEWAY_TEST_HOME", &home)
        .assert()
        .success()
        .stdout(predicate::str::contains("other"));

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("show-default")
        .env("AW_GATEWAY_TEST_HOME", &home)
        .assert()
        .success()
        .stdout(predicate::str::contains("other"));

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("reset-default")
        .env("AW_GATEWAY_TEST_HOME", &home)
        .assert()
        .success()
        .stdout(predicate::str::contains("default"));
    assert!(!home.join(".config/aw-gateway/default-target").exists());
}

#[test]
fn gateway_writes_configured_log_file() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-config", "default", "--identity-file"])
        .arg(dir.path().join("inner_ed25519"))
        .assert()
        .success();

    assert!(dir.path().join("logs/aw-gateway.log").exists());
}

#[test]
fn help_cli_prints_allowed_commands() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("help")
        .assert()
        .success()
        .stdout(predicate::str::contains("AW Gateway commands:"))
        .stdout(predicate::str::contains("up [target]"))
        .stdout(predicate::str::contains(
            "run [--session-id ID] [target] [--cwd DIR] -- <command>",
        ))
        .stdout(predicate::str::contains("client-config [target]"))
        .stdout(predicate::str::contains("help"));
}

#[test]
fn run_cli_requires_command() {
    Command::cargo_bin("aw-gateway")
        .unwrap()
        .args(["run", "default"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "run requires -- followed by a command",
        ));

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .args(["run", "--session-id", "abc123def456", "default"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "run requires -- followed by a command",
        ));
}

#[cfg(unix)]
#[test]
fn interrupted_run_cleans_ephemeral_workspace() {
    let dir = tempdir().unwrap();
    let session_id = "abc123def456";
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    let started = dir.path().join("started");
    write_executable(&fake_runtime, &interruptible_runtime_script(&log, &started));
    let workspace_template = dir
        .path()
        .join("aw-gateway/workspaces/default-{session_id}");
    let workspace = dir
        .path()
        .join("aw-gateway/workspaces")
        .join(format!("default-{session_id}"));
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        interrupted_cleanup_config(&dir, &fake_runtime, &workspace_template),
    )
    .unwrap();

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("aw-gateway"))
        .arg("--config")
        .arg(&config)
        .args([
            "run",
            "default",
            "--session-id",
            session_id,
            "--",
            "sleep",
            "30",
        ])
        .spawn()
        .unwrap();
    wait_for_file(&started);
    signal_process(child.id(), libc::SIGHUP);
    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(129));
    assert!(
        !workspace.exists(),
        "workspace still exists: {}",
        workspace.display()
    );
    let log = std::fs::read_to_string(log).unwrap();
    assert!(log.contains(&format!("stop worker-{session_id}")), "{log}");
    assert!(log.contains(&format!("rm worker-{session_id}")), "{log}");
}

#[cfg(unix)]
#[test]
fn second_signal_aborts_interrupted_run_cleanup() {
    let dir = tempdir().unwrap();
    let session_id = "abc123def456";
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    let started = dir.path().join("started");
    write_executable(&fake_runtime, &interruptible_runtime_script(&log, &started));
    let workspace_template = dir
        .path()
        .join("aw-gateway/workspaces/default-{session_id}");
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        interrupted_cleanup_config_with_idle_grace(&dir, &fake_runtime, &workspace_template, "5s"),
    )
    .unwrap();

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("aw-gateway"))
        .arg("--config")
        .arg(&config)
        .args([
            "run",
            "default",
            "--session-id",
            session_id,
            "--",
            "sleep",
            "30",
        ])
        .spawn()
        .unwrap();
    wait_for_file(&started);
    signal_process(child.id(), libc::SIGHUP);
    std::thread::sleep(std::time::Duration::from_millis(200));
    signal_process(child.id(), libc::SIGTERM);
    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(143));
}
