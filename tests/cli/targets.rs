use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

use crate::helpers::gateway_sample_for_test;

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
fn targets_json_uses_effective_targets_from_templates_and_includes() {
    let dir = tempdir().unwrap();
    let config_dir = dir.path().join("config.d");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("targets.toml"),
        r#"
[target_templates.base]
image = "ubuntu/base"
mode = "fixed"
name = "{image_slug}"

[targets.default]
use = ["base"]
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
        .args(["targets", "--json"])
        .env("AW_GATEWAY_TEST_HOME", &home)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value[0]["target"], "default");
    assert_eq!(value[0]["image"], "ubuntu/base");
    assert_eq!(value[0]["mode"], "fixed");
    assert_eq!(value[0]["container"], "ubuntu-base");
    assert_eq!(value[0]["default"], true);
}
