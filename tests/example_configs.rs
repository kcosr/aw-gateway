use assert_cmd::Command;
use std::path::Path;

#[test]
fn example_gateway_configs_validate() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    if !manifest_dir.join("examples").exists() {
        return;
    }
    for config in [
        "aw-gateway.sample.toml",
        "examples/podman/gateway-local.toml",
        "examples/podman/gateway-remote.toml",
        "examples/docker/gateway-local.toml",
        "examples/docker/gateway-remote.toml",
        "examples/colima/gateway-local.toml",
        "examples/apple-container/gateway-local.toml",
    ] {
        let config = manifest_dir.join(config);
        Command::cargo_bin("aw-gateway")
            .unwrap()
            .arg("--config")
            .arg(config)
            .args(["config", "validate"])
            .assert()
            .success();
    }
}
