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

fn start_container_sshd_scripts() -> Vec<(&'static str, String)> {
    let mut scripts = vec![("assets", asset("start-container-sshd"))];
    scripts.extend(
        ["apple-container", "colima", "docker", "podman"]
            .into_iter()
            .map(|runtime| (runtime, example_asset(runtime, "start-container-sshd"))),
    );
    scripts
}

#[test]
fn asset_scripts_are_shell_syntax_valid() {
    for script in [
        "aw-iptables",
        "aw-transparent-uds-firewall",
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

    for runtime in ["apple-container", "colima", "docker", "podman"] {
        Command::new("bash")
            .args(["-n", &example_asset(runtime, "start-container-sshd")])
            .assert()
            .success();
    }

    for script in [
        "run-host-socket-exposure-smoke.sh",
        "run-transparent-firewall-smoke.sh",
        "run-transparent-uds-stack-smoke.sh",
    ] {
        Command::new("bash")
            .args([
                "-n",
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("smoke/scripts")
                    .join(script)
                    .display()
                    .to_string(),
            ])
            .assert()
            .success();
    }
}

#[test]
fn host_socket_exposure_smoke_drives_the_gateway_lifecycle() {
    let script = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("smoke/scripts/run-host-socket-exposure-smoke.sh"),
    )
    .unwrap();

    for required in [
        "gateway up socket-smoke --json",
        "io.aw-gateway.host-socket-exposures.v1",
        "host_socket_exposures.echo",
        "container_path = \"/run/acl-proxy/echo.sock\"",
        "gateway run socket-smoke -- python3",
        "remove socket-smoke",
        "pinned-inode-rebind-fail-closed=passed",
        "gateway-remove=passed",
        "workload-recreate-recovery=passed",
        "second-uds-exchange=passed",
    ] {
        assert!(script.contains(required), "missing {required:?}");
    }
    for forbidden in ["docker run", "--mount", "PROXY Protocol", "AWID"] {
        assert!(!script.contains(forbidden), "forbidden {forbidden:?}");
    }
    assert_eq!(script.matches("gateway remove socket-smoke").count(), 2);
    assert!(!script.contains("gateway remove socket-smoke || true"));
}

#[test]
fn example_start_container_sshd_scripts_match_canonical_asset() {
    let canonical = std::fs::read_to_string(asset("start-container-sshd")).unwrap();
    for runtime in ["apple-container", "colima", "docker", "podman"] {
        let example =
            std::fs::read_to_string(example_asset(runtime, "start-container-sshd")).unwrap();
        assert_eq!(example, canonical, "{runtime}");
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
fn transparent_uds_firewall_is_fail_closed_and_has_no_proxy_uid_bypass() {
    let script = std::fs::read_to_string(asset("aw-transparent-uds-firewall")).unwrap();

    for required in [
        "iptables-restore --test --noflush",
        "ip6tables-restore --test --noflush",
        "AWUDS_FAIL4",
        "AWUDS_FAIL6",
        "--dport 80 -j REDIRECT",
        "--dport 443 -j REDIRECT",
        "-p udp --dport 443 -j DROP",
        "-j DROP",
        "READY_FILE=\"$STATE_FILE.ready\"",
    ] {
        assert!(script.contains(required), "missing {required:?}");
    }
    for forbidden in ["--uid-owner", "PROXY_UID", "--publish-socket"] {
        assert!(!script.contains(forbidden), "forbidden {forbidden:?}");
    }
}

#[test]
fn transparent_uds_stack_smoke_structural_contract_is_explicit() {
    let script = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("smoke/scripts/run-transparent-uds-stack-smoke.sh"),
    )
    .unwrap();

    for required in [
        "src=$HTTP_SOCKET,dst=/run/acl-proxy/transparent-http.sock",
        "src=$HTTPS_SOCKET,dst=/run/acl-proxy/transparent-https.sock",
        "src=$TMP_DIR/config/mitm-ca-cert.pem,dst=/etc/acl-proxy/mitm-ca-cert.pem",
        "--privileged --network",
        "--user 65534:65534",
        "--expected-acl-sha",
        "--expected-access-runtime-sha",
        "--expected-aw-sha",
        "status --porcelain --untracked-files=all",
        "cargo build --quiet --locked --release",
        "%{num_connects}",
        "access-path=iptables-redirect-so-original-dst-proxy-v2-unix",
        "https-mitm=passed",
        "proxy-loss-fail-closed=passed",
        "incremental-streaming=passed",
        "active-stream-proxy-loss=passed",
        "workload-isolation=passed",
        "linux-socket-realization=pinned_inode",
        "pinned-inode-rebind=passed",
        "workload-recreate-recovery=passed",
    ] {
        assert!(script.contains(required), "missing {required:?}");
    }
    for forbidden in [
        "src=$TMP_DIR/socket-runtime,dst=/run/acl-proxy",
        "src=$ACL_CONFIG",
        "src=$TMP_DIR/config,dst=",
        "Re-using existing connection",
    ] {
        assert!(!script.contains(forbidden), "forbidden {forbidden:?}");
    }
    assert_eq!(script.matches("apt-get install").count(), 1);

    let readme =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("smoke/README.md"))
            .unwrap();
    assert!(readme.contains("Transparent UDS Privileged Gate"));
    assert!(readme.contains("Linux-only T04"));
    assert!(readme.contains("structural contract and shell syntax"));
    assert!(readme.contains("privileged gate."));
}

#[test]
fn apple_host_proxy_profile_reuses_bootstrap_and_service_dependencies() {
    let profile =
        std::fs::read_to_string(example_asset("apple-container", "gateway-host-proxy.toml"))
            .unwrap();

    assert!(profile.contains("name = \"install-transparent-firewall\""));
    assert!(profile.contains("\n  \"repair\","));
    assert!(!profile.contains("acl-relay"));
    assert!(!profile.contains("startup_phase"));
    assert!(profile.contains(
        "[target_defaults.host_socket_exposures.transparent_http]\n\
         host_path = \"/Users/example/Library/Application Support/AW Gateway/runtime/transparent-http.sock\"\n\
         container_path = \"/run/acl-proxy/transparent-http.sock\"\n\
         selinux_relabel = \"none\""
    ));
    assert!(profile.contains(
        "[target_defaults.host_socket_exposures.transparent_https]\n\
         host_path = \"/Users/example/Library/Application Support/AW Gateway/runtime/transparent-https.sock\"\n\
         container_path = \"/run/acl-proxy/transparent-https.sock\"\n\
         selinux_relabel = \"none\""
    ));
    assert!(
        profile.contains(
            "NODE_EXTRA_CA_CERTS = \"/usr/local/share/ca-certificates/acl-proxy-ca.crt\""
        )
    );
    assert!(profile.contains("target = \"/etc/acl-proxy/transparent-uds-relay.json\""));
    assert!(profile.contains("\"--config\", \"/etc/acl-proxy/transparent-uds-relay.json\""));
    assert!(profile.contains("name = \"transparent-relay\"\nrequired = true\nuser = \"root\""));
    assert!(profile.contains("name = \"transparent-firewall\"\nrequired = true\nuser = \"root\""));
    assert!(profile.contains("type = \"process\""));
    assert!(profile.contains("depends_on = [\"transparent-firewall\"]"));
    assert!(profile.contains("depends_on = [\"transparent-relay\"]"));
    assert!(!profile.contains("/opt/acl-proxy/bin/acl-proxy"));
    assert!(!profile.contains("mitm-ca.key"));
    assert!(!profile.contains("AW_IDENTITY_TOKEN"));
}

#[test]
fn transparent_relay_example_is_the_canonical_unversioned_contract() {
    let path = example_asset("apple-container", "transparent-uds-relay.json");
    let raw = std::fs::read_to_string(&path).unwrap();
    let config: serde_json::Value = serde_json::from_str(&raw).unwrap();

    assert!(config.get("schemaVersion").is_none());
    assert_eq!(config["setupTimeout"], "2s");
    assert_eq!(
        config["routes"][0]["upstreamSocket"],
        "/run/acl-proxy/transparent-http.sock"
    );
    assert_eq!(
        config["routes"][1]["upstreamSocket"],
        "/run/acl-proxy/transparent-https.sock"
    );

    let smoke = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("smoke/scripts/run-transparent-uds-stack-smoke.sh"),
    )
    .unwrap();
    assert!(smoke.contains("examples/apple-container/transparent-uds-relay.json"));
    assert!(smoke.contains("cp -- \"$RELAY_CONFIG\" \"$TMP_DIR/relay.json\""));
    assert!(!smoke.contains("cat >\"$TMP_DIR/relay.json\""));
    assert!(!smoke.contains("setpriv --reuid"));
    assert!(smoke.contains("/etc/acl-proxy/transparent-uds-relay.json"));
    assert!(!smoke.contains("/etc/aw-gateway/transparent-uds-relay.json"));
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
    assert!(script.contains("AW_SSHD_SETENV_CONFIG"));
    assert!(script.contains("AW_SSHD_LISTEN_ADDRESS"));
    assert!(script.contains("set_listen_address"));
    assert!(script.contains("merge_setenv_config"));
    assert!(script.contains("sed -i '/^[[:space:]]*Subsystem"));
    assert!(script.contains("ForceCommand"));
    assert!(script.contains("/usr/sbin/sshd -t -f \"$config\""));
    assert!(script.contains("exec /usr/sbin/sshd -e -D -f \"$config\""));
}

#[test]
fn start_container_sshd_dry_run_sets_managed_authorized_keys_file() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        std::fs::write(
            &base_config,
            "Port 22\nAuthorizedKeysFile .aw-gateway/ssh/authorized_keys\nMatch User nobody\n  X11Forwarding no\n",
        )
        .unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env(
                "AW_SSHD_AUTHORIZED_KEYS_FILE",
                "/var/lib/aw-gateway/ssh/authorized_keys",
            )
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let managed = "AuthorizedKeysFile /var/lib/aw-gateway/ssh/authorized_keys";
        assert_eq!(stdout.matches(managed).count(), 1, "{label}");
        assert!(
            stdout.find(managed).unwrap() < stdout.find("Match User nobody").unwrap(),
            "{label}"
        );
        assert!(
            !stdout.contains("AuthorizedKeysFile .aw-gateway"),
            "{label}"
        );
    }
}

#[test]
fn start_container_sshd_rejects_invalid_authorized_keys_override() {
    for (label, script) in start_container_sshd_scripts() {
        for invalid in ["relative/path", "/path with spaces/authorized_keys"] {
            let dir = tempdir().unwrap();
            let base_config = dir.path().join("sshd_config_agent");
            let runtime_config = dir.path().join("runtime_sshd_config");
            let run_dir = dir.path().join("run");
            std::fs::write(&base_config, "Port 22\n").unwrap();

            let output = StdCommand::new(&script)
                .env("AW_SSHD_BASE_CONFIG", &base_config)
                .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
                .env("AW_SSHD_RUN_DIR", &run_dir)
                .env("AW_SSHD_AUTHORIZED_KEYS_FILE", invalid)
                .env("AW_SSHD_DRY_RUN_CONFIG", "1")
                .output()
                .unwrap();

            assert!(!output.status.success(), "{label}: {invalid}");
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("invalid AW_SSHD_AUTHORIZED_KEYS_FILE"),
                "{label}: {invalid}"
            );
        }
    }
}

#[test]
fn start_container_sshd_dry_run_rewrites_listen_address_before_match_blocks() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        std::fs::write(
            &base_config,
            "Port 22\nListenAddress 127.0.0.1\nMatch User nobody\n    ListenAddress 127.0.0.1\n",
        )
        .unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_LISTEN_ADDRESS", "0.0.0.0")
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let listen_lines = stdout
            .lines()
            .filter(|line| line.trim_start().starts_with("ListenAddress "))
            .collect::<Vec<_>>();
        assert_eq!(
            listen_lines,
            vec!["ListenAddress 0.0.0.0", "    ListenAddress 127.0.0.1"],
            "{label}"
        );
        assert!(
            stdout.find("ListenAddress 0.0.0.0").unwrap()
                < stdout.find("Match User nobody").unwrap(),
            "{label}"
        );
    }
}

#[test]
fn start_container_sshd_rejects_invalid_listen_address_override() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        std::fs::write(&base_config, "Port 22\nListenAddress 127.0.0.1\n").unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_LISTEN_ADDRESS", "192.0.2.10")
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(!output.status.success(), "{label}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid AW_SSHD_LISTEN_ADDRESS"),
            "{label}"
        );
    }
}

#[test]
fn start_container_sshd_dry_run_merges_setenv_into_single_directive() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        let setenv_config = dir.path().join("sshd-session-env.conf");
        std::fs::write(
            &base_config,
            "Port 22\nSetEnv SHELL=/usr/bin/bash PATH=/base # base defaults\nSetEnv EXTRA=base\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
        )
        .unwrap();
        std::fs::write(
            &setenv_config,
            "# Generated by aw-gateway.\nSetEnv CODEX_HOME=/var/lib/codex PATH=/session\n",
        )
        .unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_SETENV_CONFIG", &setenv_config)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let setenv_lines = stdout
            .lines()
            .filter(|line| line.starts_with("SetEnv "))
            .collect::<Vec<_>>();
        assert_eq!(
            setenv_lines,
            vec!["SetEnv CODEX_HOME=/var/lib/codex PATH=/session SHELL=/usr/bin/bash EXTRA=base"],
            "{label}"
        );
        assert!(!stdout.contains("PATH=/base"), "{label}");
    }
}

#[test]
fn start_container_sshd_dry_run_preserves_quoted_setenv_tokens() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        let setenv_config = dir.path().join("sshd-session-env.conf");
        std::fs::write(
            &base_config,
            "Port 22\nSetEnv FOO=\"hello world\" HASH=a#b PATH=/base # base defaults\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
        )
        .unwrap();
        std::fs::write(
            &setenv_config,
            "# Generated by aw-gateway.\nSetEnv PATH=/session CODEX_HOME=/var/lib/codex\n",
        )
        .unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_SETENV_CONFIG", &setenv_config)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let setenv_lines = stdout
            .lines()
            .filter(|line| line.starts_with("SetEnv "))
            .collect::<Vec<_>>();
        assert_eq!(
            setenv_lines,
            vec!["SetEnv PATH=/session CODEX_HOME=/var/lib/codex FOO=\"hello world\" HASH=a#b"],
            "{label}"
        );
        assert!(!stdout.contains("PATH=/base"), "{label}");
    }
}

#[test]
fn start_container_sshd_dry_run_preserves_setenv_escapes_and_single_quotes() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        std::fs::write(
            &base_config,
            "Port 22\nSetEnv RE=\\d+ MSG=a\\tb SPACE=a\\ b SINGLE='hello world' ESCAPED=\\\"quoted\\\"\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
        )
        .unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let setenv_lines = stdout
            .lines()
            .filter(|line| line.starts_with("SetEnv "))
            .collect::<Vec<_>>();
        assert_eq!(
            setenv_lines,
            vec![
                "SetEnv RE=\\d+ MSG=a\\tb SPACE=a\\ b SINGLE='hello world' ESCAPED=\\\"quoted\\\""
            ],
            "{label}"
        );
    }
}

#[test]
fn start_container_sshd_dry_run_merges_lowercase_setenv_keyword() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        let setenv_config = dir.path().join("sshd-session-env.conf");
        std::fs::write(
            &base_config,
            "Port 22\nsetenv PATH=/base\nSetEnv SHELL=/usr/bin/bash\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
        )
        .unwrap();
        std::fs::write(&setenv_config, "SetEnv PATH=/session\n").unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_SETENV_CONFIG", &setenv_config)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let setenv_lines = stdout
            .lines()
            .filter(|line| line.starts_with("SetEnv "))
            .collect::<Vec<_>>();
        assert_eq!(
            setenv_lines,
            vec!["SetEnv PATH=/session SHELL=/usr/bin/bash"],
            "{label}"
        );
        assert!(!stdout.contains("setenv PATH=/base"), "{label}");
    }
}

#[test]
fn start_container_sshd_dry_run_merges_equals_separator_setenv_and_match_keywords() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        let setenv_config = dir.path().join("sshd-session-env.conf");
        std::fs::write(
            &base_config,
            "Port 22\nSetEnv = PATH=/base\nSetEnv= SHELL=/usr/bin/bash\nMatch = User nobody\n    SetEnv = PATH=/match\n",
        )
        .unwrap();
        std::fs::write(
            &setenv_config,
            "SetEnv =PATH=/session CODEX_HOME=/var/lib/codex\n",
        )
        .unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_SETENV_CONFIG", &setenv_config)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let merged = "SetEnv PATH=/session CODEX_HOME=/var/lib/codex SHELL=/usr/bin/bash";
        assert!(stdout.contains(merged), "{label}");
        assert!(stdout.contains("Match = User nobody"), "{label}");
        assert!(stdout.contains("    SetEnv = PATH=/match"), "{label}");
        assert!(stdout.find(merged).unwrap() < stdout.find("Match = User nobody").unwrap());
        assert!(!stdout.contains("SetEnv = PATH=/base"), "{label}");
    }
}

#[test]
fn start_container_sshd_dry_run_keeps_setenv_global_when_base_has_match_block() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        let setenv_config = dir.path().join("sshd-session-env.conf");
        std::fs::write(
            &base_config,
            "Port 22\nSetEnv SHELL=/usr/bin/bash\nMatch User nobody\n    SetEnv PATH=/match\n    ForceCommand internal-sftp\n",
        )
        .unwrap();
        std::fs::write(&setenv_config, "SetEnv CODEX_HOME=/var/lib/codex\n").unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_SETENV_CONFIG", &setenv_config)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let merged = "SetEnv CODEX_HOME=/var/lib/codex SHELL=/usr/bin/bash";
        assert!(stdout.contains(merged), "{label}");
        assert!(stdout.contains("    SetEnv PATH=/match"), "{label}");
        assert!(
            stdout.find(merged).unwrap() < stdout.find("Match User nobody").unwrap(),
            "{label}"
        );
    }
}

#[test]
fn start_container_sshd_dry_run_keeps_forcecommand_global_when_base_has_match_block() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        let policy = dir.path().join("ssh-command-filter.toml");
        let filter = dir.path().join("aw-ssh-command-filter");
        std::fs::write(
            &base_config,
            "Port 22\nSubsystem sftp /usr/libexec/openssh/sftp-server\nMatch=User nobody\n    ForceCommand internal-sftp\n",
        )
        .unwrap();
        std::fs::write(&policy, "sftp = \"deny\"\nlegacy_scp = \"deny\"\n").unwrap();
        std::fs::write(&filter, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_POLICY_CONFIG", &policy)
            .env("AW_SSH_COMMAND_FILTER", &filter)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let inserted = format!(
            "ForceCommand {} --config {}",
            filter.display(),
            policy.display()
        );
        assert!(!stdout.contains("Subsystem sftp"), "{label}");
        assert!(stdout.contains(&inserted), "{label}");
        assert!(stdout.contains("    ForceCommand internal-sftp"), "{label}");
        assert!(
            stdout.find(&inserted).unwrap() < stdout.find("Match=User nobody").unwrap(),
            "{label}"
        );
    }
}

#[test]
fn start_container_sshd_dry_run_coalesces_base_setenv_without_generated_config() {
    for (label, script) in start_container_sshd_scripts() {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        std::fs::write(
            &base_config,
            "Port 22\nSetEnv SHELL=/usr/bin/bash\nSetEnv PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
        )
        .unwrap();

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let setenv_lines = stdout
            .lines()
            .filter(|line| line.starts_with("SetEnv "))
            .collect::<Vec<_>>();
        assert_eq!(
            setenv_lines,
            vec!["SetEnv SHELL=/usr/bin/bash PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"],
            "{label}"
        );
    }
}

#[test]
fn start_container_sshd_dry_run_fails_when_setenv_config_is_unreadable() {
    let dir = tempdir().unwrap();
    let base_config = dir.path().join("sshd_config_agent");
    let runtime_config = dir.path().join("runtime_sshd_config");
    let run_dir = dir.path().join("run");
    let missing_setenv_config = dir.path().join("missing-sshd-session-env.conf");
    std::fs::write(
        &base_config,
        "Port 22\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
    )
    .unwrap();

    let output = StdCommand::new(asset("start-container-sshd"))
        .env("AW_SSHD_BASE_CONFIG", &base_config)
        .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
        .env("AW_SSHD_RUN_DIR", &run_dir)
        .env("AW_SSHD_SETENV_CONFIG", &missing_setenv_config)
        .env("AW_SSHD_DRY_RUN_CONFIG", "1")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("sshd SetEnv config is not readable"));
}

#[test]
fn start_container_sshd_dry_run_fails_on_malformed_setenv_tokens() {
    for (base, expected) in [
        (
            "Port 22\nSetEnv NOEQUALS\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
            "invalid SetEnv token: NOEQUALS",
        ),
        (
            "Port 22\nSetEnv BAD=\"unterminated\nSubsystem sftp /usr/libexec/openssh/sftp-server\n",
            "invalid SetEnv directive: unterminated quote",
        ),
    ] {
        let dir = tempdir().unwrap();
        let base_config = dir.path().join("sshd_config_agent");
        let runtime_config = dir.path().join("runtime_sshd_config");
        let run_dir = dir.path().join("run");
        std::fs::write(&base_config, base).unwrap();

        let output = StdCommand::new(asset("start-container-sshd"))
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(!output.status.success(), "{base}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(expected), "{stderr}");
    }
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
    for (label, script) in start_container_sshd_scripts() {
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

        let output = StdCommand::new(&script)
            .env("AW_SSHD_BASE_CONFIG", &base_config)
            .env("AW_SSHD_RUNTIME_CONFIG", &runtime_config)
            .env("AW_SSHD_RUN_DIR", &run_dir)
            .env("AW_SSHD_POLICY_CONFIG", &policy)
            .env("AW_SSH_COMMAND_FILTER", &filter)
            .env("AW_SSHD_DRY_RUN_CONFIG", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.contains("Subsystem sftp"), "{label}");
        assert!(
            stdout.contains(&format!(
                "ForceCommand {} --config {}",
                filter.display(),
                policy.display()
            )),
            "{label}"
        );
    }
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
