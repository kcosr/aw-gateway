use assert_cmd::Command;
use aw_gateway::config::{
    AccessFlowRelayTransport, ContainerAgentFile, ContainerMountMode, GatewayConfig,
};
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
        "examples/docker/gateway-access-flow-tls.toml",
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

#[test]
fn remote_tls_gateway_and_agent_examples_share_one_relay_contract() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let agent: ContainerAgentFile = toml::from_str(
        &std::fs::read_to_string(
            manifest_dir.join("examples/docker/container-agent-access-flow-tls.toml"),
        )
        .unwrap(),
    )
    .unwrap();
    let gateway: GatewayConfig = toml::from_str(
        &std::fs::read_to_string(manifest_dir.join("examples/docker/gateway-access-flow-tls.toml"))
            .unwrap(),
    )
    .unwrap();

    let agent_relay = agent.container_agent.access_flow_relay.as_ref().unwrap();
    let gateway_agent = gateway.target_defaults.container_agent.as_ref().unwrap();
    let gateway_relay = gateway_agent.access_flow_relay.as_ref().unwrap();
    assert_eq!(agent_relay.setup_timeout, gateway_relay.setup_timeout);
    assert_eq!(agent_relay.drain_timeout, gateway_relay.drain_timeout);
    assert_eq!(agent_relay.max_connections, 64);
    assert_eq!(agent_relay.max_connections, gateway_relay.max_connections);
    assert_eq!(
        agent_relay.copy_buffer_bytes_per_direction,
        gateway_relay.copy_buffer_bytes_per_direction
    );
    assert_eq!(agent_relay.presentation, gateway_relay.presentation);
    assert_eq!(agent_relay.routes.len(), 2);
    assert_eq!(agent_relay.routes.len(), gateway_relay.routes.len());
    for (agent_route, gateway_route) in agent_relay.routes.iter().zip(&gateway_relay.routes) {
        assert_eq!(agent_route.name, gateway_route.name);
        assert_eq!(agent_route.listen, gateway_route.listen);
        assert_eq!(
            agent_route.allowed_destination_ports,
            gateway_route.allowed_destination_ports
        );
        let (
            AccessFlowRelayTransport::TlsTcp {
                address: agent_address,
                server_name: agent_server_name,
                trust: agent_trust,
                ca_certificate: agent_trust_path,
            },
            AccessFlowRelayTransport::TlsTcp {
                address: gateway_address,
                server_name: gateway_server_name,
                trust: gateway_trust,
                ca_certificate: gateway_trust_path,
            },
        ) = (&agent_route.transport, &gateway_route.transport)
        else {
            panic!("remote TLS examples must contain only tls_tcp routes");
        };
        assert_eq!(agent_address, gateway_address);
        assert_eq!(agent_server_name, gateway_server_name);
        assert_eq!(agent_trust, gateway_trust);
        assert_eq!(*agent_trust, access_tls_trust::TlsClientTrustMode::Custom);
        assert_eq!(agent_trust_path, gateway_trust_path);
        assert_eq!(
            agent_trust_path.as_deref(),
            Some("/etc/aw-gateway/acl-proxy-trust/roots.pem")
        );
    }

    let trust_mount = gateway
        .target_defaults
        .container_mounts
        .iter()
        .find(|mount| mount.target == "/etc/aw-gateway/acl-proxy-trust")
        .expect("dedicated TLS example must provision its trust directory");
    assert_eq!(trust_mount.source, "/opt/aw-gateway/trust/acl-proxy");
    assert_eq!(trust_mount.mode, ContainerMountMode::Ro);
    assert!(
        gateway_agent.services.iter().any(|service| {
            service.name == "container-sshd"
                && service
                    .depends_on
                    .iter()
                    .any(|name| name == "@access-flow-relay")
        }),
        "workload readiness must depend on the TLS relay"
    );
}
