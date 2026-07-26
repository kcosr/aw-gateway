#![cfg(unix)]

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};
use tempfile::tempdir;

struct KillOnDrop(Child);

impl std::ops::Deref for KillOnDrop {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn container_agent_answers_status_on_control_socket() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"

[container_agent.idle_cleanup]
owner = "agent"
action = "exit_container"
idle_grace = "5m"
preserve_processes = []
poll_interval = "30s"
"#,
            control_socket.display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&control_socket);

    let mut stream = UnixStream::connect(&control_socket).unwrap();
    stream
        .write_all(br#"{"id":"1","method":"status"}"#)
        .unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], "1");
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["ready"], true);
    assert_eq!(response["result"]["ssh_bridge"]["enabled"], false);
    assert_eq!(response["result"]["ssh_bridge"]["ready"], true);
    assert_eq!(response["result"]["idle_cleanup"]["owner"], "agent");
    assert_eq!(response["result"]["idle_cleanup"]["state"], "idle_pending");

    let mut stream = UnixStream::connect(&control_socket).unwrap();
    stream
        .write_all(br#"{"id":"2","method":"reap_now","params":{"dry_run":true}}"#)
        .unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], "2");
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "unauthorized");
    assert_eq!(response["error"]["message"], "control token is required");

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn container_agent_not_ready_until_bridge_binds() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let blocked_bridge_socket = dir.path().join("not-a-dir").join("ssh.sock");
    std::fs::write(dir.path().join("not-a-dir"), "block").unwrap();
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"

[container_agent.ssh_bridge]
enabled = true
socket = "{}"
target = "127.0.0.1:22"
mode = "0600"
"#,
            control_socket.display(),
            blocked_bridge_socket.display(),
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&control_socket);

    let mut stream = UnixStream::connect(&control_socket).unwrap();
    stream
        .write_all(br#"{"id":"1","method":"status"}"#)
        .unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["ready"], false);
    assert_eq!(response["result"]["ssh_bridge"]["enabled"], true);
    assert_eq!(response["result"]["ssh_bridge"]["ready"], false);

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn container_agent_requires_token_for_mutating_control_methods_when_configured() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"

[container_agent.idle_cleanup]
owner = "agent"
action = "reap_processes"
idle_grace = "5m"
preserve_processes = []
poll_interval = "30s"
"#,
            control_socket.display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .env("AW_CONTAINER_CONTROL_TOKEN", "secret")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&control_socket);

    let response = control_request(&control_socket, br#"{"id":"status","method":"status"}"#);
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["ready"], true);

    let response = control_request(
        &control_socket,
        br#"{"id":"1","method":"reap_now","params":{"dry_run":true}}"#,
    );
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "unauthorized");

    let response = control_request(
        &control_socket,
        br#"{"id":"1b","method":"shutdown","params":{"reason":"test"}}"#,
    );
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "unauthorized");

    let response = control_request(
        &control_socket,
        br#"{"id":"wrong","method":"reap_now","params":{"dry_run":true,"token":"wrong"}}"#,
    );
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "unauthorized");

    let response = control_request(
        &control_socket,
        br#"{"id":"2","method":"reap_now","params":{"dry_run":true,"token":"secret"}}"#,
    );
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["dry_run"], true);

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn container_agent_rejects_mutating_control_methods_without_configured_token() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"
"#,
            control_socket.display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&control_socket);

    let response = control_request(&control_socket, br#"{"id":"status","method":"status"}"#);
    assert_eq!(response["ok"], true);

    let response = control_request(
        &control_socket,
        br#"{"id":"shutdown","method":"shutdown","params":{"reason":"test"}}"#,
    );
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "unauthorized");
    assert_eq!(response["error"]["message"], "control token is required");

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn container_agent_returns_unknown_method_when_method_is_missing_or_non_string() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"
"#,
            control_socket.display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&control_socket);

    let response = control_request(&control_socket, br#"{"id":"missing"}"#);
    assert_eq!(response["id"], "missing");
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "unknown_method");
    assert_eq!(response["error"]["message"], "unknown control method");

    let response = control_request(&control_socket, br#"{"id":"non-string","method":123}"#);
    assert_eq!(response["id"], "non-string");
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "unknown_method");
    assert_eq!(response["error"]["message"], "unknown control method");

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn container_agent_session_hold_counts_as_active_session() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"

[container_agent.idle_cleanup]
owner = "agent"
action = "exit_container"
idle_grace = "5m"
preserve_processes = []
poll_interval = "30s"
"#,
            control_socket.display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .env("AW_CONTAINER_CONTROL_TOKEN", "secret")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&control_socket);

    let mut hold = UnixStream::connect(&control_socket).unwrap();
    hold.write_all(
        br#"{"id":"hold","method":"session_hold","params":{"token":"secret","kind":"test"}}"#,
    )
    .unwrap();
    hold.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(hold.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["held"], true);

    let response = control_request(&control_socket, br#"{"id":"status","method":"status"}"#);
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["ssh_bridge"]["active_sessions"], 1);

    drop(hold);
    let response = control_request(&control_socket, br#"{"id":"status","method":"status"}"#);
    assert_eq!(response["result"]["ssh_bridge"]["active_sessions"], 0);

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn container_agent_returns_parse_error_for_invalid_control_json() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"
"#,
            control_socket.display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&control_socket);

    let response = control_request(&control_socket, b"not-json");
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "parse_error");

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn service_only_identity_is_preloaded_and_scoped_to_the_approved_service() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let approved_env_out = dir.path().join("approved-service.env");
    let unrelated_env_out = dir.path().join("unrelated-service.env");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"

[[container_agent.services]]
name = "approved"
command = ["/bin/sh", "-c", "/usr/bin/env > '{}'; sleep 30"]
restart = "never"

[container_agent.services.env]
AW_IDENTITY_TOKEN = {{ inherit = "AW_IDENTITY_TOKEN" }}

[[container_agent.services]]
name = "unrelated"
command = ["/bin/sh", "-c", "/usr/bin/env > '{}'; sleep 30"]
restart = "never"
"#,
            control_socket.display(),
            approved_env_out.display(),
            unrelated_env_out.display(),
        ),
    )
    .unwrap();

    let bearer = "abcdefghijklmnopqrstuvwxyzABCDEF";
    let _child = KillOnDrop(
        Command::cargo_bin("aw-container-agent")
            .unwrap()
            .arg("--config")
            .arg(&config)
            .env("AW_CONTAINER_STATE_DIR", &state_dir)
            .env("AW_CONTAINER_CONTROL_TOKEN", "secret")
            .env("AW_IDENTITY_TOKEN", bearer)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    wait_for_path(&control_socket);
    wait_for_file_contains(&approved_env_out, "PATH=");
    wait_for_file_contains(&unrelated_env_out, "PATH=");

    let approved_env = std::fs::read_to_string(&approved_env_out).unwrap();
    assert!(approved_env.contains(&format!("AW_IDENTITY_TOKEN={bearer}")));
    assert!(!approved_env.contains("AW_CONTAINER_CONTROL_TOKEN"));
    let unrelated_env = std::fs::read_to_string(&unrelated_env_out).unwrap();
    assert!(!unrelated_env.contains("AW_CONTAINER_CONTROL_TOKEN"));
    assert!(!unrelated_env.contains("AW_IDENTITY_TOKEN"));
    assert!(!unrelated_env.contains(bearer));

    #[cfg(target_os = "linux")]
    match std::fs::read(format!("/proc/{}/environ", _child.id())) {
        Ok(agent_env) => {
            assert!(
                !agent_env
                    .windows("AW_IDENTITY_TOKEN".len())
                    .any(|value| value == b"AW_IDENTITY_TOKEN")
            );
            assert!(
                !agent_env
                    .windows(bearer.len())
                    .any(|value| value == bearer.as_bytes())
            );
        }
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied),
    }
}

#[test]
fn bearer_relay_consumes_source_before_runtime_and_child_start() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let env_out = dir.path().join("service.env");
    let endpoint = dir.path().join("access-flow.sock");
    let _endpoint_listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = source.local_addr().unwrap();
    drop(source);
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"

[container_agent.access_flow_relay]
setup_timeout = "2s"
drain_timeout = "10s"
max_connections = 32
copy_buffer_bytes_per_direction = 16384

[container_agent.access_flow_relay.presentation]
kind = "bearer_environment"
variable = "AW_ACCESS_FLOW_TEST_TOKEN"

[[container_agent.access_flow_relay.routes]]
name = "http"
listen = "{listen}"
allowed_destination_ports = [80]

[container_agent.access_flow_relay.routes.transport]
kind = "unix"
path = "{}"

[[container_agent.services]]
name = "env-capture"
command = ["/bin/sh", "-c", "/usr/bin/env > '{}'; sleep 30"]
restart = "never"
"#,
            control_socket.display(),
            endpoint.display(),
            env_out.display(),
        ),
    )
    .unwrap();

    let bearer = "abcdefghijklmnopqrstuvwxyzABCDEF";
    let _child = KillOnDrop(
        Command::cargo_bin("aw-container-agent")
            .unwrap()
            .arg("--config")
            .arg(&config)
            .env("AW_CONTAINER_STATE_DIR", &state_dir)
            .env("AW_ACCESS_FLOW_TEST_TOKEN", bearer)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    wait_for_path(&control_socket);
    wait_for_file_contains(&env_out, "PATH=");

    let service_env = std::fs::read_to_string(&env_out).unwrap();
    assert!(!service_env.contains("AW_ACCESS_FLOW_TEST_TOKEN"));
    assert!(!service_env.contains(bearer));

    #[cfg(target_os = "linux")]
    {
        match std::fs::read(format!("/proc/{}/environ", _child.id())) {
            Ok(agent_env) => {
                assert!(
                    !agent_env
                        .windows("AW_ACCESS_FLOW_TEST_TOKEN".len())
                        .any(|value| value == b"AW_ACCESS_FLOW_TEST_TOKEN")
                );
                assert!(
                    !agent_env
                        .windows(bearer.len())
                        .any(|value| value == bearer.as_bytes())
                );
            }
            Err(error) => assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "non-dumpable agent must either hide or sanitize proc environ"
            ),
        }
        let limits = std::fs::read_to_string(format!("/proc/{}/limits", _child.id())).unwrap();
        let core_limit = limits
            .lines()
            .find(|line| line.starts_with("Max core file size"))
            .expect("agent limits must report core size");
        assert!(core_limit.split_whitespace().any(|value| value == "0"));
    }

    let response = control_request(&control_socket, br#"{"id":"status","method":"status"}"#);
    let rendered = serde_json::to_string(&response).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["access_flow_relay"]["ready"], true);
    assert!(!rendered.contains("AW_ACCESS_FLOW_TEST_TOKEN"));
    assert!(!rendered.contains(bearer));
}

#[test]
fn combined_relay_and_service_identity_consumes_canonical_or_distinct_locators() {
    for relay_source in ["AW_IDENTITY_TOKEN", "AW_ACCESS_FLOW_COMBINED_TOKEN"] {
        let dir = tempdir().unwrap();
        let control_socket = dir.path().join("agent.sock");
        let state_dir = dir.path().join("state");
        let endpoint = dir.path().join("access-flow.sock");
        let _endpoint_listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        let source = TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = source.local_addr().unwrap();
        drop(source);
        let approved_env_out = dir.path().join("approved.env");
        let unrelated_env_out = dir.path().join("unrelated.env");
        let config = dir.path().join("container-agent.toml");
        std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[container_agent]
control_socket = "{}"

[container_agent.access_flow_relay]
setup_timeout = "2s"
drain_timeout = "10s"
max_connections = 32
copy_buffer_bytes_per_direction = 16384

[container_agent.access_flow_relay.presentation]
kind = "bearer_environment"
variable = "{relay_source}"

[[container_agent.access_flow_relay.routes]]
name = "http"
listen = "{listen}"
allowed_destination_ports = [80]

[container_agent.access_flow_relay.routes.transport]
kind = "unix"
path = "{}"

[[container_agent.services]]
name = "approved"
command = ["/bin/sh", "-c", "/usr/bin/env > '{}'; sleep 30"]
restart = "never"

[container_agent.services.env]
AW_IDENTITY_TOKEN = {{ inherit = "AW_IDENTITY_TOKEN" }}

[[container_agent.services]]
name = "unrelated"
command = ["/bin/sh", "-c", "/usr/bin/env > '{}'; sleep 30"]
restart = "never"
"#,
                control_socket.display(),
                endpoint.display(),
                approved_env_out.display(),
                unrelated_env_out.display(),
            ),
        )
        .unwrap();

        let canonical_bearer = if relay_source == "AW_IDENTITY_TOKEN" {
            "canonical-bearer-ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        } else {
            "service-bearer-ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        };
        let relay_bearer = if relay_source == "AW_IDENTITY_TOKEN" {
            canonical_bearer
        } else {
            "relay-bearer-ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        };
        let mut command = Command::cargo_bin("aw-container-agent").unwrap();
        command
            .arg("--config")
            .arg(&config)
            .env("AW_CONTAINER_STATE_DIR", &state_dir)
            .env_remove("AW_IDENTITY_TOKEN")
            .env_remove("AW_ACCESS_FLOW_COMBINED_TOKEN")
            .env(relay_source, relay_bearer)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if relay_source != "AW_IDENTITY_TOKEN" {
            command.env("AW_IDENTITY_TOKEN", canonical_bearer);
        }
        let _child = KillOnDrop(command.spawn().unwrap());

        wait_for_path(&control_socket);
        wait_for_file_contains(&approved_env_out, "PATH=");
        wait_for_file_contains(&unrelated_env_out, "PATH=");
        assert!(
            std::fs::read_to_string(&approved_env_out)
                .unwrap()
                .contains(&format!("AW_IDENTITY_TOKEN={canonical_bearer}"))
        );
        let unrelated_env = std::fs::read_to_string(&unrelated_env_out).unwrap();
        for forbidden in [
            "AW_IDENTITY_TOKEN",
            "AW_ACCESS_FLOW_COMBINED_TOKEN",
            canonical_bearer,
            relay_bearer,
        ] {
            assert!(!unrelated_env.contains(forbidden), "{unrelated_env}");
        }

        #[cfg(target_os = "linux")]
        match std::fs::read(format!("/proc/{}/environ", _child.id())) {
            Ok(agent_env) => {
                for forbidden in [
                    "AW_IDENTITY_TOKEN".as_bytes(),
                    "AW_ACCESS_FLOW_COMBINED_TOKEN".as_bytes(),
                    canonical_bearer.as_bytes(),
                    relay_bearer.as_bytes(),
                ] {
                    assert!(
                        !agent_env
                            .windows(forbidden.len())
                            .any(|value| value == forbidden)
                    );
                }
            }
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied),
        }

        let response = control_request(&control_socket, br#"{"id":"status","method":"status"}"#);
        let rendered = serde_json::to_string(&response).unwrap();
        assert_eq!(response["result"]["access_flow_relay"]["ready"], true);
        assert!(!rendered.contains(relay_source));
        assert!(!rendered.contains(canonical_bearer));
        assert!(!rendered.contains(relay_bearer));
    }
}

#[test]
fn bearer_relay_missing_or_invalid_source_fails_without_disclosure() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        r#"
schema_version = "1"

[container_agent]
control_socket = false

[container_agent.access_flow_relay]
setup_timeout = "2s"
drain_timeout = "10s"
max_connections = 32
copy_buffer_bytes_per_direction = 16384

[container_agent.access_flow_relay.presentation]
kind = "bearer_environment"
variable = "AW_ACCESS_FLOW_TEST_TOKEN"

[[container_agent.access_flow_relay.routes]]
name = "http"
listen = "127.0.0.1:3128"
allowed_destination_ports = [80]

[container_agent.access_flow_relay.routes.transport]
kind = "unix"
path = "/tmp/not-used.sock"
"#,
    )
    .unwrap();

    for value in [None, Some("invalid secret with spaces")] {
        let mut command = Command::cargo_bin("aw-container-agent").unwrap();
        command
            .arg("--config")
            .arg(&config)
            .env_remove("AW_ACCESS_FLOW_TEST_TOKEN");
        if let Some(value) = value {
            command.env("AW_ACCESS_FLOW_TEST_TOKEN", value);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("AW_ACCESS_FLOW_TEST_TOKEN"), "{stderr}");
        assert!(!stderr.contains("invalid secret with spaces"), "{stderr}");
    }
}

#[test]
fn service_identity_missing_or_invalid_source_fails_without_disclosure() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        r#"
schema_version = "1"

[container_agent]
control_socket = false

[[container_agent.services]]
name = "approved"
command = ["/bin/true"]
restart = "never"

[container_agent.services.env]
AW_IDENTITY_TOKEN = { inherit = "AW_IDENTITY_TOKEN" }
"#,
    )
    .unwrap();

    for value in [None, Some("invalid secret with spaces")] {
        let mut command = Command::cargo_bin("aw-container-agent").unwrap();
        command
            .arg("--config")
            .arg(&config)
            .env_remove("AW_IDENTITY_TOKEN");
        if let Some(value) = value {
            command.env("AW_IDENTITY_TOKEN", value);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("AW_IDENTITY_TOKEN"), "{stderr}");
        assert!(!stderr.contains("invalid secret with spaces"), "{stderr}");
    }
}

#[test]
fn bootstrap_steps_receive_a_cleared_environment_before_agent_exec() {
    let dir = tempdir().unwrap();
    let agent_config = dir.path().join("container-agent.toml");
    let bootstrap_config = dir.path().join("container-bootstrap.toml");
    let step_env = dir.path().join("bootstrap-step.env");
    let core_limit = dir.path().join("bootstrap-step.core-limit");
    std::fs::write(
        &agent_config,
        "schema_version = \"1\"\n[container_agent]\nenabled = false\n",
    )
    .unwrap();
    std::fs::write(
        &bootstrap_config,
        format!(
            r#"
schema_version = "2"
agent_program = "/bin/true"
agent_config = "{}"
skip_identity_prepare = true
chown_existing_identity_dirs = false

[identity]
session_user = "awuser"
session_uid = 2450
session_gid = 2450
session_home = "/home/awuser"
session_shell = "/bin/bash"
state_dir = "/tmp/aw-gateway-test"

[[steps]]
name = "capture"
user = "root"
command = ["/bin/sh", "-c", "/usr/bin/env > '{}'; ulimit -c > '{}'"]
"#,
            agent_config.display(),
            step_env.display(),
            core_limit.display(),
        ),
    )
    .unwrap();

    let bearer = "abcdefghijklmnopqrstuvwxyzABCDEF";
    for source in [
        None,
        Some("AW_IDENTITY_TOKEN"),
        Some("AW_ACCESS_FLOW_BOOTSTRAP_TOKEN"),
    ] {
        let mut command = Command::cargo_bin("aw-container-bootstrap").unwrap();
        command
            .arg("--config")
            .arg(&agent_config)
            .arg("--bootstrap-config")
            .arg(&bootstrap_config)
            .env_remove("AW_IDENTITY_TOKEN")
            .env_remove("AW_ACCESS_FLOW_BOOTSTRAP_TOKEN");
        if let Some(source) = source {
            command.env(source, bearer);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let environment = std::fs::read_to_string(&step_env).unwrap();
        assert!(!environment.contains("AW_IDENTITY_TOKEN"));
        assert!(!environment.contains("AW_ACCESS_FLOW_BOOTSTRAP_TOKEN"));
        assert!(!environment.contains(bearer));
        assert!(environment.contains("PATH="));
        assert!(environment.contains("HOME=/root"));
        assert_eq!(std::fs::read_to_string(&core_limit).unwrap().trim(), "0");
    }
}

#[test]
fn container_agent_can_run_services_without_control_socket() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let env_out = dir.path().join("service.env");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = false

[[container_agent.services]]
name = "env-capture"
command = ["/bin/sh", "-c", "/usr/bin/env > '{}'; sleep 30"]
restart = "never"
"#,
            env_out.display(),
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&env_out);
    assert!(!state_dir.join("agent.sock").exists());

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn container_agent_shutdown_exits_after_authorized_request() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"
"#,
            control_socket.display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .env("AW_CONTAINER_CONTROL_TOKEN", "secret")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&control_socket);

    let response = control_request(
        &control_socket,
        br#"{"id":"1","method":"shutdown","params":{"reason":"test","token":"secret"}}"#,
    );
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["shutting_down"], true);

    let status = wait_for_child_exit(&mut child);
    assert!(status.success());
}

#[test]
fn container_agent_bridge_proxies_to_tcp_target() {
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = tcp.local_addr().unwrap();
    let echo = std::thread::spawn(move || {
        let (mut stream, _) = tcp.accept().unwrap();
        let mut buf = [0_u8; 64];
        let n = stream.read(&mut buf).unwrap();
        stream.write_all(&buf[..n]).unwrap();
    });

    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let bridge_socket = dir.path().join("ssh.sock");
    let state_dir = dir.path().join("state");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"

[container_agent.ssh_bridge]
enabled = true
socket = "{}"
target = "{}"
mode = "0600"
"#,
            control_socket.display(),
            bridge_socket.display(),
            addr,
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&control_socket);
    wait_for_path(&bridge_socket);

    let mut stream = UnixStream::connect(&bridge_socket).unwrap();
    stream.write_all(b"ping").unwrap();
    let mut response = [0_u8; 4];
    stream.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"ping");

    echo.join().unwrap();
    child.kill().unwrap();
    let _ = child.wait();
}

fn control_request(path: &std::path::Path, request: &[u8]) -> Value {
    let mut stream = UnixStream::connect(path).unwrap();
    stream.write_all(request).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn wait_for_path(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

fn wait_for_file_contains(path: &std::path::Path, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::fs::read_to_string(path).is_ok_and(|contents| contents.contains(needle)) {
            return;
        }
        sleep(Duration::from_millis(25));
    }
    panic!(
        "timed out waiting for {} to contain {needle:?}",
        path.display()
    );
}

fn wait_for_child_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        sleep(Duration::from_millis(25));
    }
    child.kill().unwrap();
    panic!("timed out waiting for child exit");
}
