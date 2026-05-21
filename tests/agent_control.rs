#![cfg(unix)]

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};
use tempfile::tempdir;

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
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["dry_run"], true);

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
        br#"{"id":"2","method":"reap_now","params":{"dry_run":true,"token":"secret"}}"#,
    );
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"]["dry_run"], true);

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
fn container_agent_does_not_leak_control_token_to_services() {
    let dir = tempdir().unwrap();
    let control_socket = dir.path().join("agent.sock");
    let state_dir = dir.path().join("state");
    let env_out = dir.path().join("service.env");
    let config = dir.path().join("container-agent.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[container_agent]
control_socket = "{}"

[[container_agent.services]]
name = "env-capture"
command = ["/bin/sh", "-c", "/usr/bin/env > '{}'; sleep 30"]
restart = "never"
"#,
            control_socket.display(),
            env_out.display(),
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .env("AW_CONTAINER_STATE_DIR", &state_dir)
        .env("AW_CONTAINER_CONTROL_TOKEN", "secret")
        .env("AW_IDENTITY_TOKEN", "identity-secret")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_for_path(&control_socket);
    wait_for_file_contains(&env_out, "PATH=");

    let service_env = std::fs::read_to_string(&env_out).unwrap();
    assert!(!service_env.contains("AW_CONTAINER_CONTROL_TOKEN"));
    assert!(!service_env.contains("AW_IDENTITY_TOKEN"));
    assert!(service_env.contains("PATH="));

    child.kill().unwrap();
    let _ = child.wait();
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
