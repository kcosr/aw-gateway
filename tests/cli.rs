use assert_cmd::Command;
use aw_gateway::cli::{GatewayArgs, GatewayCommand};
use clap::Parser;
use predicates::prelude::*;
use tempfile::{TempDir, tempdir};

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

[[lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/true"]
timeout = "250ms"

[[host_steps]]
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
fn client_config_honors_local_listen_mode() {
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
        .args(["client-config", "default", "--identity-file"])
        .arg(dir.path().join("inner_ed25519"))
        .assert()
        .success()
        .stdout(predicate::str::contains("HostName 127.0.0.1"))
        .stdout(predicate::str::contains("Port 40222"))
        .stdout(predicate::str::contains("IdentityFile"))
        .stdout(predicate::str::contains("StrictHostKeyChecking no"))
        .stdout(predicate::str::contains("UserKnownHostsFile /dev/null"));
}

#[test]
fn client_config_default_managed_server_uses_single_proxy_command() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-config", "default"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Host aw-default"))
        .stdout(predicate::str::contains("HostName aw-container-default"))
        .stdout(predicate::str::contains(
            "ProxyCommand ssh -T gateway.example.com /opt/aw-gateway/bin/aw-gateway connect default",
        ))
        .stdout(predicate::str::contains("Host aw-host-").not())
        .stdout(predicate::str::contains("IdentityFile").not())
        .stdout(predicate::str::contains("    User ").not())
        .stdout(predicate::str::contains("Port 40222").not());
}

#[test]
fn client_config_default_does_not_generate_inner_key_material() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-config", "default"])
        .assert()
        .success()
        .stdout(predicate::str::contains("IdentityFile").not());

    let ssh_dir = workspace.join(".aw-gateway/ssh");
    assert!(!ssh_dir.join("inner_ed25519").exists());
    assert!(!ssh_dir.join("inner_ed25519.pub").exists());
    assert!(!ssh_dir.join("authorized_keys").exists());
}

#[test]
fn client_config_honors_local_proxy_command_mode() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let mut sample = gateway_sample_for_test(&dir, &workspace);
    sample.push_str(
        r#"
[targets.default.local_ssh]
mode = "proxy_command"
"#,
    );
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-config", "default"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "ProxyCommand /opt/aw-gateway/bin/aw-gateway connect default",
        ))
        .stdout(predicate::str::contains("aw-host-").not())
        .stdout(predicate::str::contains("IdentityFile").not());
}

#[test]
fn client_config_explicit_identity_file_emits_identity_lines() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    let identity = dir.path().join("id_ed25519");
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-config", "default", "--identity-file"])
        .arg(&identity)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "IdentityFile {}",
            identity.display()
        )))
        .stdout(predicate::str::contains("IdentitiesOnly yes"));
}

#[test]
fn client_config_rejects_bundle_options() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-config", "default", "--rotate-key"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn client_bundle_writes_inner_key_material() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-bundle", "default"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bundle/default"));

    let ssh_dir = workspace.join(".aw-gateway/ssh");
    assert!(ssh_dir.join("inner_ed25519").exists());
    assert!(ssh_dir.join("inner_ed25519.pub").exists());
    assert!(ssh_dir.join("authorized_keys").exists());
    assert!(ssh_dir.join("config").exists());
    let bundle_config = ssh_dir.join("bundle/default/ssh_config");
    assert!(bundle_config.exists());
    let bundle_config = std::fs::read_to_string(bundle_config).unwrap();
    assert!(bundle_config.contains("~/.ssh/aw-gateway/aw-"));
    assert!(!bundle_config.contains("REPLACE_WITH_LOCAL_INNER_KEY_PATH"));
}

#[test]
fn client_bundle_rotate_key_replaces_existing_keypair() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-bundle", "default"])
        .assert()
        .success();
    let private_key = workspace.join(".aw-gateway/ssh/inner_ed25519");
    let before = std::fs::read_to_string(&private_key).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-bundle", "default", "--rotate-key"])
        .assert()
        .success();

    let after = std::fs::read_to_string(private_key).unwrap();
    assert_ne!(before, after);
}

#[test]
fn client_bundle_honors_explicit_identity_file() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    let identity = dir.path().join("custom_inner_key");
    std::fs::write(&config, sample).unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-bundle", "default", "--identity-file"])
        .arg(&identity)
        .assert()
        .success();

    let bundle_config =
        std::fs::read_to_string(workspace.join(".aw-gateway/ssh/bundle/default/ssh_config"))
            .unwrap();
    assert!(bundle_config.contains(&format!("IdentityFile {}", identity.display())));
}

#[test]
fn add_container_key_does_not_generate_server_private_key() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();
    let pubkey = dir.path().join("workstation.pub");
    std::fs::write(
        &pubkey,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKey test\n",
    )
    .unwrap();
    let ssh_dir = workspace.join(".aw-gateway/ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::fs::write(
        ssh_dir.join("authorized_keys"),
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOldManagedKey old\n",
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["add-container-key", "default", "--public-key"])
        .arg(&pubkey)
        .assert()
        .success()
        .stdout(predicate::str::contains("added"));

    assert!(!ssh_dir.join("inner_ed25519").exists());
    assert!(!ssh_dir.join("inner_ed25519.pub").exists());
    let authorized_keys = std::fs::read_to_string(ssh_dir.join("authorized_keys")).unwrap();
    assert!(authorized_keys.contains("ITestKey"));
    assert!(authorized_keys.contains("IOldManagedKey"));
}

#[test]
fn add_container_key_reports_duplicate_and_reads_stdin() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();
    let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKey test\n";

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["add-container-key", "default", "--public-key", "-"])
        .write_stdin(key)
        .assert()
        .success()
        .stdout(predicate::str::contains("added"));

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["add-container-key", "default", "--public-key", "-"])
        .write_stdin(key)
        .assert()
        .success()
        .stdout(predicate::str::contains("duplicate"));
}

#[test]
fn add_key_installs_to_host_and_container() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let home = dir.path().join("home");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();
    let pubkey = dir.path().join("workstation.pub");
    std::fs::write(
        &pubkey,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKey test\n",
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["add-key", "default", "--public-key"])
        .arg(&pubkey)
        .env("AW_GATEWAY_TEST_HOME", &home)
        .assert()
        .success()
        .stdout(predicate::str::contains("host=added; container=added"));

    let host_authorized = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap();
    let container_authorized =
        std::fs::read_to_string(workspace.join(".aw-gateway/ssh/authorized_keys")).unwrap();
    assert!(host_authorized.contains("ITestKey"));
    assert!(container_authorized.contains("ITestKey"));

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["add-key", "default", "--public-key"])
        .arg(&pubkey)
        .env("AW_GATEWAY_TEST_HOME", &home)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "host=duplicate; container=duplicate",
        ));
}

#[test]
fn add_host_key_installs_only_host() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let home = dir.path().join("home");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();
    let pubkey = dir.path().join("workstation.pub");
    std::fs::write(
        &pubkey,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKey test\n",
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["add-host-key", "--public-key"])
        .arg(&pubkey)
        .env("AW_GATEWAY_TEST_HOME", &home)
        .assert()
        .success()
        .stdout(predicate::str::contains("added"));

    let host_authorized = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap();
    assert!(host_authorized.contains("ITestKey"));
    assert!(!workspace.join(".aw-gateway/ssh").exists());
}

#[test]
fn add_container_key_rejects_multiple_lines() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace);
    std::fs::write(&config, sample).unwrap();
    let pubkey = dir.path().join("workstation.pub");
    std::fs::write(
        &pubkey,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKey test\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAISecondKey second\n",
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["add-container-key", "default", "--public-key"])
        .arg(&pubkey)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "SSH public key must be exactly one line",
        ));

    assert!(!workspace.join(".aw-gateway/ssh/authorized_keys").exists());
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
            "run [target] [--cwd DIR] -- <command>",
        ))
        .stdout(predicate::str::contains("launches"))
        .stdout(predicate::str::contains("launch show <name>"))
        .stdout(predicate::str::contains("launch <name> [--var key=value]"))
        .stdout(predicate::str::contains("client-config [target]"))
        .stdout(predicate::str::contains("remove [target]"))
        .stdout(predicate::str::contains("help"));
}

#[test]
fn ssh_dispatch_rejects_host_side_transfer_commands_when_policy_disallows_them() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    let sample = gateway_sample_for_test(&dir, &workspace)
        .replace("sftp = \"allow\"", "sftp = \"deny\"")
        .replace("legacy_scp = \"allow\"", "legacy_scp = \"deny\"");
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
            "run [target] [--cwd DIR] -- <command>",
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
}

#[test]
fn targets_cli_lists_configured_targets_without_starting_them() {
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
        .arg("targets")
        .env("AW_GATEWAY_TEST_HOME", &home)
        .assert()
        .success()
        .stdout(predicate::str::contains("TARGET"))
        .stdout(predicate::str::contains("CONTAINER"))
        .stdout(predicate::str::contains("default"))
        .stdout(predicate::str::contains("ubuntu/dev"))
        .stdout(predicate::str::contains("fixed"))
        .stdout(predicate::str::contains("ubuntu-dev"))
        .stdout(predicate::str::contains(" *"));
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

fn launch_config_for_test() -> &'static str {
    r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent-pack-codex]
target = "default"
description = "Clone a repo, initialize agent-pack, and run Codex."
cwd = "{container_home}/repo"
env = { AGENT_PACK_ID = "{var.pack_id}" }
command = ["codex", "exec", "--model", "{var.model}"]

[launches.agent-pack-codex.vars]
repo = { type = "string", required = true, description = "Git repository URL" }
pack_id = { type = "string", required = true }
model = { type = "enum", values = ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"], default = "gpt-5.5" }
debug = { type = "boolean", default = false }
count = { type = "number", default = 1 }

[[launches.agent-pack-codex.steps]]
phase = "post_ready"
location = "container"
name = "clone"
timeout = "5m"
cwd = "{container_home}"
command = ["git", "clone", "--branch", "main", "--single-branch", "{var.repo}", "repo"]
"#
}

fn gateway_sample_for_test(dir: &TempDir, workspace: &std::path::Path) -> String {
    include_str!("../aw-gateway.sample.toml")
        .replace(
            "directory = \"{state}/logs/gateway\"",
            &format!("directory = \"{}\"", dir.path().join("logs").display()),
        )
        .replace(
            "path = \"workspace\"",
            &format!("path = \"{}\"", workspace.display()),
        )
}
