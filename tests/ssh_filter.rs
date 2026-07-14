use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

fn restrictive_policy() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let config = dir.path().join("ssh-command-filter.toml");
    std::fs::write(&config, "sftp = \"deny\"\nlegacy_scp = \"deny\"\n").unwrap();
    (dir, config)
}

#[test]
fn prints_rejected_original_command_for_shell_composition() {
    let (_dir, config) = restrictive_policy();

    Command::cargo_bin("aw-ssh-command-filter")
        .unwrap()
        .arg("--config")
        .arg(config)
        .env("SSH_ORIGINAL_COMMAND", "printf hello; scp -t /tmp/file")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "blocked by policy: shell composition is not allowed when transfer policy is restrictive",
        ))
        .stderr(predicate::str::contains(
            "rejected SSH_ORIGINAL_COMMAND: printf hello; scp -t /tmp/file",
        ));
}

#[test]
fn allowed_command_still_executes_normally() {
    let (_dir, config) = restrictive_policy();

    Command::cargo_bin("aw-ssh-command-filter")
        .unwrap()
        .arg("--config")
        .arg(config)
        .env("SHELL", "/bin/sh")
        .env("SSH_ORIGINAL_COMMAND", "printf allowed")
        .assert()
        .success()
        .stdout("allowed")
        .stderr(predicate::str::is_empty());
}
