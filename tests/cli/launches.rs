use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

use crate::helpers::{
    gateway_sample_for_test, interrupted_cleanup_config, interruptible_runtime_script,
    launch_config_for_test, signal_process, wait_for_file, write_executable,
};

#[test]
fn launches_cli_lists_and_serializes_var_metadata() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, launch_config_for_test()).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("launches")
        .assert()
        .success()
        .stdout(predicate::str::contains("LAUNCH"))
        .stdout(predicate::str::contains("agent-pack-codex"))
        .stdout(predicate::str::contains("pack_id, repo"))
        .stdout(predicate::str::contains("Clone a repo"));

    let output = Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["launches", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert!(value.is_array());
    assert_eq!(value.as_array().unwrap().len(), 1);
    assert!(value[0].get("required_vars").is_none());
    assert_eq!(value[0]["name"], "agent-pack-codex");
    assert_eq!(value[0]["vars"]["repo"]["type"], "string");
    assert_eq!(value[0]["vars"]["repo"]["required"], true);
    assert_eq!(
        value[0]["vars"]["repo"]["description"],
        "Git repository URL"
    );
    assert_eq!(value[0]["vars"]["model"]["type"], "enum");
    assert_eq!(value[0]["vars"]["model"]["required"], false);
    assert_eq!(value[0]["vars"]["model"]["default"], "gpt-5.5");
    assert_eq!(value[0]["vars"]["model"]["values"][1], "gpt-5.4");
}

#[test]
fn launches_cli_reports_empty_config() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    std::fs::write(&config, gateway_sample_for_test(&dir, &workspace)).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("launches")
        .assert()
        .success()
        .stdout(predicate::str::contains("No launches configured."));
}

#[test]
fn launch_show_text_and_json_include_execution_details() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, launch_config_for_test()).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["launch", "show", "agent-pack-codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Launch: agent-pack-codex"))
        .stdout(predicate::str::contains("Target: default"))
        .stdout(predicate::str::contains("repo (string, required)"))
        .stdout(predicate::str::contains("clone [post_ready/container"))
        .stdout(predicate::str::contains(
            "argv: codex exec --model {var.model}",
        ));

    let output = Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["launch", "show", "agent-pack-codex", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["name"], "agent-pack-codex");
    assert_eq!(value["steps"][0]["name"], "clone");
    assert_eq!(value["steps"][0]["timeout"], "5m");
    assert_eq!(value["cwd"], "{container_home}/repo");
    assert_eq!(value["env"]["AGENT_PACK_ID"], "{var.pack_id}");
    assert_eq!(value["command"][0], "codex");
}

#[test]
fn launch_discovery_uses_effective_templates_from_includes() {
    let dir = tempdir().unwrap();
    let config_dir = dir.path().join("config.d");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("launches.toml"),
        r#"
[target_templates.base]
image = "ubuntu/base"
mode = "fixed"
name = "{image_slug}"

[targets.default]
use = ["base"]

[launch_templates.repo]
target = "default"
description = "Templated launch"
cwd = "{container_home}/repo"
env = { REPO = "{var.repo}" }
command = ["sh", "-lc", "echo {var.repo}"]

[launch_templates.repo.vars]
repo = { type = "string", required = true }
model = { type = "enum", values = ["gpt-5.5", "gpt-5.4"], default = "gpt-5.5" }

[[launch_templates.repo.steps]]
phase = "post_ready"
location = "container"
name = "prepare"
command = ["mkdir", "-p", "{container_home}/repo"]

[launches.templated]
use = ["repo"]
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

    let output = Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["launches", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value[0]["name"], "templated");
    assert_eq!(value[0]["target"], "default");
    assert_eq!(value[0]["description"], "Templated launch");
    assert_eq!(value[0]["vars"]["repo"]["required"], true);
    assert_eq!(value[0]["vars"]["model"]["default"], "gpt-5.5");

    let output = Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["launch", "show", "templated", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["cwd"], "{container_home}/repo");
    assert_eq!(value["env"]["REPO"], "{var.repo}");
    assert_eq!(value["command"][2], "echo {var.repo}");
    assert_eq!(value["steps"][0]["name"], "prepare");
}

#[test]
fn launch_show_json_omits_absent_optional_fields() {
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

[launches.minimal]
target = "default"
command = ["true"]
"#,
    )
    .unwrap();

    let output = Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["launch", "show", "minimal", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert!(value.get("description").is_none());
    assert!(value.get("cwd").is_none());
    assert!(value.get("env").is_none());
    assert_eq!(value["steps"].as_array().unwrap().len(), 0);

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["launch", "show", "minimal"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Description:").not());
}

#[test]
fn launch_cli_splices_passthrough_args_after_separator() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let fake_runtime = dir.path().join("runtime");
    let runtime_log = dir.path().join("runtime.log");
    write_executable(
        &fake_runtime,
        &format!(
            r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  exec)
    case "$*" in
      *aw-gateway-marker-list*|*aw-gateway-marker-sweep*)
        exit 0
        ;;
    esac
    echo "$@" >> "{runtime_log}"
    exit 0
    ;;
esac
exit 0
"#,
            user = std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
            uid = unsafe { libc::geteuid() },
            runtime_log = runtime_log.display(),
        ),
    );
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[runtime]
type = "podman"
program = "{}"

[target_defaults.container_agent]
enabled = false

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "ubuntu-dev"

[launches.args]
target = "default"
allow_args = true
command = ["launch-command", "before", "{{args}}", "after"]
"#,
            fake_runtime.display(),
        ),
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args([
            "launch",
            "args",
            "--",
            "--skill",
            "fresh-eyes",
            "review this",
        ])
        .assert()
        .success();

    let log = std::fs::read_to_string(runtime_log).unwrap();
    assert!(log.contains("aw-gateway-exec"), "{log}");
    assert!(
        log.contains("launch-command before --skill fresh-eyes review this after"),
        "{log}"
    );
    assert!(log.contains("aw-gateway-exec-rm"), "{log}");
}

#[test]
fn launch_cli_inherits_allowed_session_env_into_exec_argv() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let fake_runtime = dir.path().join("runtime");
    let runtime_log = dir.path().join("runtime.log");
    write_executable(
        &fake_runtime,
        &format!(
            r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  exec)
    case "$*" in
      *aw-gateway-marker-list*|*aw-gateway-marker-sweep*)
        exit 0
        ;;
    esac
    printf '%s\n' "$@" >> "{runtime_log}"
    exit 0
    ;;
esac
exit 0
"#,
            user = std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
            uid = unsafe { libc::geteuid() },
            runtime_log = runtime_log.display(),
        ),
    );
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[runtime]
type = "podman"
program = "{}"

[target_defaults.container_agent]
enabled = false

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "ubuntu-dev"
session_env_inherit = ["AW_GATEWAY_TEST_ARGV_ENV"]

[launches.inherit]
target = "default"
command = ["launch-command"]
"#,
            fake_runtime.display(),
        ),
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .env("AW_GATEWAY_TEST_ARGV_ENV", "argv-value")
        .arg("--config")
        .arg(&config)
        .args(["launch", "inherit"])
        .assert()
        .success();

    let log = std::fs::read_to_string(runtime_log).unwrap();
    assert!(log.contains("--env\nAW_GATEWAY_TEST_ARGV_ENV"), "{log}");
    assert!(
        !log.contains("AW_GATEWAY_TEST_ARGV_ENV=argv-value"),
        "{log}"
    );
}

#[test]
fn launch_cli_rejects_bad_vars_before_startup() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, launch_config_for_test()).unwrap();

    for (args, expected) in [
        (
            vec!["launch", "agent-pack-codex", "--var", "repo"],
            "--var must be key=value",
        ),
        (
            vec![
                "launch",
                "agent-pack-codex",
                "--var",
                "repo=a",
                "--var",
                "repo=b",
            ],
            "duplicate launch variable",
        ),
        (
            vec!["launch", "agent-pack-codex", "--var", "extra=x"],
            "unknown launch variable",
        ),
        (
            vec!["launch", "agent-pack-codex", "--var", "repo=a"],
            "missing required launch variable",
        ),
        (
            vec![
                "launch",
                "agent-pack-codex",
                "--var",
                "repo=a",
                "--var",
                "pack_id=p",
                "--var",
                "model=bad",
            ],
            "invalid enum launch variable",
        ),
        (
            vec![
                "launch",
                "agent-pack-codex",
                "--var",
                "repo=a",
                "--var",
                "pack_id=p",
                "--var",
                "debug=yes",
            ],
            "invalid boolean launch variable",
        ),
        (
            vec![
                "launch",
                "agent-pack-codex",
                "--var",
                "repo=a",
                "--var",
                "pack_id=p",
                "--var",
                "count=NaN",
            ],
            "invalid number launch variable",
        ),
        (
            vec!["launch", "agent-pack-codex", "--json"],
            "launch execution does not support --json",
        ),
        (
            vec!["launch", "agent-pack-codex", "extra"],
            "unexpected extra launch argument",
        ),
    ] {
        Command::cargo_bin("aw-gateway")
            .unwrap()
            .arg("--config")
            .arg(&config)
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains(expected));
    }

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["launch", "show", "agent-pack-codex", "--var", "x=y"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn launch_cli_accepts_session_id_forms_before_runtime_validation() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, launch_config_for_test()).unwrap();

    for args in [
        vec![
            "launch",
            "agent-pack-codex",
            "--session-id",
            "abc123def456",
            "--var",
            "repo=a",
            "--var",
            "pack_id=p",
        ],
        vec![
            "launch",
            "agent-pack-codex",
            "--session-id=abc123def456",
            "--var=repo=a",
            "--var",
            "pack_id=p",
        ],
    ] {
        Command::cargo_bin("aw-gateway")
            .unwrap()
            .arg("--config")
            .arg(&config)
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "--session-id is only valid for ephemeral targets",
            ));
    }
}

#[test]
fn launch_cli_reports_unknown_launch() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, launch_config_for_test()).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["launch", "missing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown launch \"missing\""));
}

#[cfg(unix)]
#[test]
fn interrupted_launch_cleans_ephemeral_workspace() {
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
        .args(["launch", "long", "--session-id", session_id])
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
