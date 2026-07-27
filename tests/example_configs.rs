use assert_cmd::Command;
use std::path::Path;

#[test]
fn apple_host_proxy_example_schema_validates_without_runtime_side_effects() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/apple-container/gateway-host-proxy.toml");
    let raw = std::fs::read_to_string(path).unwrap();
    let config: aw_gateway::config::GatewayConfig = toml::from_str(&raw).unwrap();
    config.validate().unwrap();
}

#[test]
fn example_gateway_configs_validate() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    if !manifest_dir.join("examples").exists() {
        return;
    }
    for config in [
        "aw-gateway.sample.toml",
        "examples/podman/gateway-local.toml",
        "examples/podman/gateway-runtime-exec.toml",
        "examples/podman/gateway-remote.toml",
        "examples/docker/gateway-local.toml",
        "examples/docker/gateway-runtime-exec.toml",
        "examples/docker/gateway-remote.toml",
        "examples/colima/gateway-local.toml",
        "examples/colima/gateway-runtime-exec.toml",
        "examples/apple-container/gateway-local.toml",
        "examples/apple-container/gateway-host-proxy.toml",
        "examples/apple-container/gateway-runtime-exec.toml",
    ] {
        let config = manifest_dir.join(config);
        let mut value: toml::Value = toml::from_str(&std::fs::read_to_string(&config).unwrap())
            .unwrap_or_else(|err| panic!("parse {}: {err}", config.display()));
        let temp = tempfile::tempdir().unwrap();
        value
            .as_table_mut()
            .unwrap()
            .entry("logging")
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .unwrap()
            .insert(
                "directory".into(),
                temp.path().join("logs").display().to_string().into(),
            );
        let hermetic_config = temp.path().join("gateway.toml");
        std::fs::write(&hermetic_config, toml::to_string(&value).unwrap()).unwrap();
        Command::cargo_bin("aw-gateway")
            .unwrap()
            .arg("--config")
            .arg(hermetic_config)
            .args(["config", "validate"])
            .assert()
            .success();
    }
}

#[test]
fn remote_tls_agent_example_schema_validates_without_source_access() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/docker/container-agent-access-flow-tls.toml");
    Command::cargo_bin("aw-container-agent")
        .unwrap()
        .arg("--config")
        .arg(path)
        .args(["config", "validate"])
        .assert()
        .success();
}
