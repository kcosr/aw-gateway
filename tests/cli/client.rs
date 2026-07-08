use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

use crate::helpers::{gateway_sample_for_test, write_executable};

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
fn client_config_honors_local_direct_published_port_mode() {
    let dir = tempdir().unwrap();
    let runtime = dir.path().join("runtime");
    write_executable(
        &runtime,
        r#"#!/bin/sh
case "$1" in
  inspect)
    echo '[]'
    ;;
esac
exit 0
"#,
    );
    let config = dir.path().join("gateway.toml");
    let workspace = dir.path().join("workspace");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"
default_target = "default"

[runtime]
type = "docker"
program = "{runtime}"

[target_defaults.workspace]
path = "{workspace}"
state_dir = ".aw-gateway"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"

[targets.default.local_ssh]
mode = "direct"
backend = "published_port"
readiness = "ssh_only"
host = "127.0.0.1"
port = 40222
"#,
            runtime = runtime.display(),
            workspace = workspace.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("aw-gateway")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["client-config", "default"])
        .assert()
        .success()
        .stdout(predicate::str::contains("HostName 127.0.0.1"))
        .stdout(predicate::str::contains("Port 40222"))
        .stdout(predicate::str::contains("ProxyCommand").not())
        .stdout(predicate::str::contains("IdentityFile").not());

    let inner_config = std::fs::read_to_string(workspace.join(".aw-gateway/ssh/config")).unwrap();
    assert!(inner_config.contains("Port 40222"));
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
    let inner_config = std::fs::read_to_string(workspace.join(".aw-gateway/ssh/config")).unwrap();
    assert!(inner_config.contains("Host aw-default"));
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
