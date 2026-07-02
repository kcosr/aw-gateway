use assert_cmd::Command;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command as StdCommand;
use tempfile::tempdir;

fn asset(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join(name)
        .display()
        .to_string()
}

fn example_asset(runtime: &str, name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(runtime)
        .join(name)
        .display()
        .to_string()
}

#[test]
fn asset_scripts_are_shell_syntax_valid() {
    for script in [
        "aw-iptables",
        "copy-skel",
        "copy-workspace-template",
        "ensure-storage-conf",
        "start-container-sshd",
    ] {
        Command::new("bash")
            .args(["-n", &asset(script)])
            .assert()
            .success();
    }
}

#[test]
fn ensure_storage_conf_writes_shared_store_fallback() {
    let dir = tempdir().unwrap();
    let home = dir.path().join("home");
    let shared_store = dir.path().join("shared-store");

    Command::new(asset("ensure-storage-conf"))
        .env("HOME", &home)
        .args(["--shared-store", shared_store.to_str().unwrap()])
        .assert()
        .success();

    let storage_conf = home.join(".config/containers/storage.conf");
    let contents = std::fs::read_to_string(&storage_conf).unwrap();
    assert!(contents.contains(&format!(
        "additionalimagestores = [\"{}\"]",
        shared_store.display()
    )));
    assert_eq!(
        std::fs::metadata(storage_conf)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn copy_skel_recursively_copies_missing_files_without_overwrite() {
    let dir = tempdir().unwrap();
    let aw_home = dir.path().join("aw");
    let skel = aw_home.join("skel");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(skel.join(".config/containers")).unwrap();
    std::fs::create_dir_all(skel.join(".config/opencode")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(skel.join(".bashrc"), "new\n").unwrap();
    std::fs::write(skel.join(".config/containers/storage.conf"), "conf\n").unwrap();
    std::fs::write(
        skel.join(".config/opencode/opencode.jsonc"),
        "{ \"provider\": {} }\n",
    )
    .unwrap();
    std::fs::write(workspace.join(".bashrc"), "old\n").unwrap();

    Command::new(asset("copy-skel"))
        .args([
            "--skel-dir",
            skel.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(workspace.join(".bashrc")).unwrap(),
        "old\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join(".config/containers/storage.conf")).unwrap(),
        "conf\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join(".config/opencode/opencode.jsonc")).unwrap(),
        "{ \"provider\": {} }\n"
    );
}

#[test]
fn aw_iptables_usage_mentions_check_action() {
    let output = StdCommand::new(asset("aw-iptables")).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("{add|check|status}"));
}

#[test]
fn sshd_config_agent_keeps_inner_ssh_policy_container_scoped() {
    let config = std::fs::read_to_string(asset("sshd_config_agent")).unwrap();

    for required in [
        "ListenAddress 127.0.0.1",
        "AuthenticationMethods publickey",
        "PasswordAuthentication no",
        "KbdInteractiveAuthentication no",
        "AuthorizedKeysFile .aw-gateway/ssh/authorized_keys",
        "AllowAgentForwarding no",
        "AllowTcpForwarding local",
        "PermitOpen 127.0.0.1:* localhost:* [::1]:*",
        "X11Forwarding no",
        "PermitTunnel no",
        "GatewayPorts no",
        "Subsystem sftp /usr/libexec/openssh/sftp-server",
        "SetEnv SHELL=/usr/bin/bash",
        "SetEnv PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
    ] {
        assert!(config.contains(required), "missing {required:?}");
    }

    for forbidden in [
        "PasswordAuthentication yes",
        "KbdInteractiveAuthentication yes",
        "AllowAgentForwarding yes",
        "AllowTcpForwarding yes",
        "GatewayPorts yes",
        "PermitUserEnvironment yes",
        "NODE_EXTRA_CA_CERTS",
    ] {
        assert!(!config.contains(forbidden), "forbidden {forbidden:?}");
    }
}

#[test]
fn example_sshd_configs_match_documented_runtime_networking_and_ubuntu_sftp_path() {
    for runtime in ["docker", "podman"] {
        let config = std::fs::read_to_string(example_asset(runtime, "sshd_config_agent")).unwrap();
        assert!(config.contains("ListenAddress 127.0.0.1"));
        assert!(config.contains("Subsystem sftp /usr/lib/openssh/sftp-server"));
        assert!(!config.contains("/usr/libexec/openssh/sftp-server"));
    }

    for runtime in ["colima", "apple-container"] {
        let config = std::fs::read_to_string(example_asset(runtime, "sshd_config_agent")).unwrap();
        assert!(config.contains("ListenAddress 0.0.0.0"));
        assert!(config.contains("Subsystem sftp /usr/lib/openssh/sftp-server"));
        assert!(!config.contains("/usr/libexec/openssh/sftp-server"));
    }
}

#[test]
fn start_container_sshd_uses_agent_config() {
    let script = std::fs::read_to_string(asset("start-container-sshd")).unwrap();
    assert!(script.contains("ssh-keygen -A"));
    assert!(script.contains("AW_SSHD_POLICY_CONFIG"));
    assert!(script.contains("sed -i '/^[[:space:]]*Subsystem"));
    assert!(script.contains("ForceCommand"));
    assert!(script.contains("/usr/sbin/sshd -t -f \"$config\""));
    assert!(script.contains("exec /usr/sbin/sshd -e -D -f \"$config\""));
}

#[test]
fn start_container_sshd_dry_run_keeps_sftp_when_allowed() {
    let dir = tempdir().unwrap();
    let base_config = dir.path().join("sshd_config_agent");
    let runtime_config = dir.path().join("runtime_sshd_config");
    let run_dir = dir.path().join("run");
    std::fs::write(
        &base_config,
        "Port 22\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
    )
    .unwrap();

    let output = StdCommand::new(asset("start-container-sshd"))
        .env("AW_SSHD_BASE_CONFIG", &base_config)
        .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
        .env("AW_SSHD_RUN_DIR", &run_dir)
        .env("AW_SSHD_DRY_RUN_CONFIG", "1")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Subsystem sftp /usr/libexec/openssh/sftp-server"));
    assert!(!stdout.contains("ForceCommand"));
}

#[test]
fn start_container_sshd_dry_run_disables_transfer_policy() {
    let dir = tempdir().unwrap();
    let base_config = dir.path().join("sshd_config_agent");
    let runtime_config = dir.path().join("runtime_sshd_config");
    let run_dir = dir.path().join("run");
    let policy = dir.path().join("ssh-command-filter.toml");
    let filter = dir.path().join("aw-ssh-command-filter");
    std::fs::write(
        &base_config,
        "Port 22\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
    )
    .unwrap();
    std::fs::write(&policy, "sftp = \"deny\"\nlegacy_scp = \"deny\"\n").unwrap();
    std::fs::write(&filter, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = StdCommand::new(asset("start-container-sshd"))
        .env("AW_SSHD_BASE_CONFIG", &base_config)
        .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
        .env("AW_SSHD_RUN_DIR", &run_dir)
        .env("AW_SSHD_POLICY_CONFIG", &policy)
        .env("AW_SSH_COMMAND_FILTER", &filter)
        .env("AW_SSHD_DRY_RUN_CONFIG", "1")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("Subsystem sftp"));
    assert!(stdout.contains(&format!(
        "ForceCommand {} --config {}",
        filter.display(),
        policy.display()
    )));
}

#[test]
fn start_container_sshd_dry_run_installs_forcecommand_for_sftp_deny() {
    let dir = tempdir().unwrap();
    let base_config = dir.path().join("sshd_config_agent");
    let runtime_config = dir.path().join("runtime_sshd_config");
    let run_dir = dir.path().join("run");
    let policy = dir.path().join("ssh-command-filter.toml");
    let filter = dir.path().join("aw-ssh-command-filter");
    std::fs::write(
        &base_config,
        "Port 22\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
    )
    .unwrap();
    std::fs::write(&policy, "sftp = \"deny\"\nlegacy_scp = \"allow\"\n").unwrap();
    std::fs::write(&filter, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = StdCommand::new(asset("start-container-sshd"))
        .env("AW_SSHD_BASE_CONFIG", &base_config)
        .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
        .env("AW_SSHD_RUN_DIR", &run_dir)
        .env("AW_SSHD_POLICY_CONFIG", &policy)
        .env("AW_SSH_COMMAND_FILTER", &filter)
        .env("AW_SSHD_DRY_RUN_CONFIG", "1")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("Subsystem sftp"));
    assert!(stdout.contains(&format!(
        "ForceCommand {} --config {}",
        filter.display(),
        policy.display()
    )));
}

#[test]
fn start_container_sshd_dry_run_installs_forcecommand_for_directional_legacy_scp() {
    for mode in ["inbound", "outbound"] {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        let policy = dir.path().join("ssh-command-filter.toml");
        let filter = dir.path().join("aw-ssh-command-filter");
        std::fs::write(
            &base_config,
            "Port 22\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
        )
        .unwrap();
        std::fs::write(
            &policy,
            format!("sftp = \"allow\"\nlegacy_scp = \"{mode}\"\n"),
        )
        .unwrap();
        std::fs::write(&filter, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();

        let output = StdCommand::new(asset("start-container-sshd"))
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_POLICY_CONFIG", &policy)
            .env("AW_SSH_COMMAND_FILTER", &filter)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(output.status.success(), "mode {mode}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains("Subsystem sftp /usr/libexec/openssh/sftp-server"),
            "mode {mode}"
        );
        assert!(
            stdout.contains(&format!(
                "ForceCommand {} --config {}",
                filter.display(),
                policy.display()
            )),
            "mode {mode}"
        );
    }
}
