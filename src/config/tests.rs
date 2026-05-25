
use super::*;
use std::time::Duration;

#[test]
fn sample_gateway_config_validates() {
    let cfg: GatewayConfig = toml::from_str(crate::gateway::DEFAULT_GATEWAY_CONFIG).unwrap();
    cfg.validate().unwrap();
}

#[test]
fn http_config_defaults_to_disabled_loopback_none_auth() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    assert!(!cfg.http.enabled);
    assert_eq!(cfg.http.listen, "127.0.0.1:8080");
    assert!(cfg.http.enabled_actions.is_empty());
    assert_eq!(cfg.http.auth.auth_type, HttpAuthType::None);
    assert!(cfg.http.auth.token_file.is_none());
}

#[test]
fn http_config_validates_bearer_token_file_rules() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[http]
enabled = true
listen = "127.0.0.1:8080"
enabled_actions = ["status"]

[http.auth]
type = "bearer"
token_file = "~/.config/aw-gateway/http-token"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let missing_file: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[http]
enabled = true
enabled_actions = ["status"]

[http.auth]
type = "bearer"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    let err = missing_file.validate().unwrap_err().to_string();
    assert!(err.contains("token_file is required"), "{err}");

    let unexpected_file: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[http]
enabled = true
enabled_actions = ["status"]

[http.auth]
type = "none"
token_file = "/tmp/token"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    let err = unexpected_file.validate().unwrap_err().to_string();
    assert!(err.contains("only valid"), "{err}");
}

#[test]
fn http_config_rejects_empty_actions_forbidden_actions_and_alias_fields() {
    let empty_actions: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[http]
enabled = true

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    let err = empty_actions.validate().unwrap_err().to_string();
    assert!(
        err.contains("http.enabled_actions must not be empty"),
        "{err}"
    );

    for action in [
        "connect",
        "add-key",
        "client-config",
        "stop",
        "remove",
        "bogus",
    ] {
        let cfg: GatewayConfig = toml::from_str(&format!(
            r#"
schema_version = "1"

[http]
enabled = true
enabled_actions = ["{action}"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#
        ))
        .unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("http.enabled_actions"), "{action}: {err}");
    }

    for alias in ["token", "bearer_token"] {
        let err = toml::from_str::<GatewayConfig>(&format!(
            r#"
schema_version = "1"

[http]
enabled = true
enabled_actions = ["status"]

[http.auth]
type = "bearer"
token_file = "/tmp/token"
{alias} = "secret"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#
        ))
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{alias}: {err}");
    }
}

#[test]
fn http_config_rejects_non_socket_listen() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[http]
enabled = true
listen = "localhost:8080"
enabled_actions = ["status"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("parse http.listen"), "{err}");
}

#[test]
fn target_workspace_cleanup_defaults_to_never() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    assert_eq!(
        cfg.effective_target("default").unwrap().workspace.cleanup,
        WorkspaceCleanup::Never
    );
}

#[test]
fn target_workspace_cleanup_accepts_ephemeral_target_workspace() {
    for value in ["success", "always"] {
        let cfg: GatewayConfig = toml::from_str(&format!(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "worker-{{session_id}}"
stop_when_idle = true
[targets.default.workspace]
path = "{{home}}/.cache/aw-gateway/workspaces/{{target}}-{{session_id}}"
cleanup = "{value}"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#
        ))
        .unwrap();
        cfg.validate().unwrap();
    }
}

#[test]
fn target_workspace_cleanup_rejects_fixed_targets() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
[targets.default.workspace]
path = "{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"
cleanup = "always"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#,
    )
    .unwrap();

    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("workspace.cleanup requires mode = \"ephemeral\""),
        "{err}"
    );
}

#[test]
fn fixed_target_rejects_inherited_cleanup_default() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults.workspace]
path = "{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"
cleanup = "always"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();

    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("workspace.cleanup requires mode = \"ephemeral\""),
        "{err}"
    );
}

#[test]
fn target_workspace_cleanup_rejects_workspace_without_session_id() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "worker-{session_id}"
stop_when_idle = true
[targets.default.workspace]
path = "{home}/.cache/aw-gateway/workspaces/{target}"
cleanup = "success"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#,
    )
    .unwrap();

    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("workspace.cleanup requires workspace.path to reference {session_id}"),
        "{err}"
    );
}

#[test]
fn target_workspace_cleanup_rejects_workspace_outside_aw_gateway_component() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "worker-{session_id}"
stop_when_idle = true
[targets.default.workspace]
path = "{home}/sessions/{target}-{session_id}"
cleanup = "always"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#,
    )
    .unwrap();

    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains(
            "workspace.cleanup requires workspace.path under an aw-gateway path component"
        ),
        "{err}"
    );
}

#[test]
fn target_workspace_cleanup_requires_gateway_owned_exit_cleanup() {
    for (name, idle_cleanup, expected) in [
        (
            "missing",
            "",
            "workspace.cleanup requires gateway-owned idle_cleanup",
        ),
        (
            "agent",
            r#"
[targets.default.idle_cleanup]
owner = "agent"
action = "exit_container"
"#,
            "workspace.cleanup requires gateway-owned exit_container idle_cleanup",
        ),
        (
            "none-action",
            r#"
[targets.default.idle_cleanup]
owner = "gateway"
action = "none"
"#,
            "workspace.cleanup requires gateway-owned exit_container idle_cleanup",
        ),
        (
            "preserve",
            r#"
[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
preserve_processes = ["tmux"]
"#,
            "workspace.cleanup does not support preserve_processes",
        ),
    ] {
        let cfg: GatewayConfig = toml::from_str(&format!(
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "worker-{{session_id}}"
stop_when_idle = true
[targets.default.workspace]
path = "{{home}}/.cache/aw-gateway/workspaces/{{target}}-{{session_id}}"
cleanup = "always"
{idle_cleanup}
"#
        ))
        .unwrap();

        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(expected), "{name}: {err}");
    }
}

#[test]
fn path_segment_names_reject_dot_segments() {
    assert!(validate_name("target", ".").is_err());
    assert!(validate_name("target", "..").is_err());
    validate_name("target", "dev.shell-1").unwrap();
}

#[test]
fn control_sockets_defaults_and_overrides_are_effective() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.custom.control_sockets]
container_dir = "/tmp/aw-gateway"

[targets.custom]
image = "ubuntu/custom"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let default_target = cfg.effective_target("default").unwrap();
    let default = &default_target.control_sockets;
    assert_eq!(default.host_dir, "/run/user/{uid}/aw-gateway/{runtime_id}");
    assert_eq!(default.container_dir, "/run/aw-gateway");

    let custom_target = cfg.effective_target("custom").unwrap();
    let custom = &custom_target.control_sockets;
    assert_eq!(custom.host_dir, "/run/user/{uid}/aw-gateway/{runtime_id}");
    assert_eq!(custom.container_dir, "/tmp/aw-gateway");
}

#[test]
fn control_sockets_global_override_can_be_overlaid_per_target() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults.control_sockets]
host_dir = "/tmp/aw/{runtime_id}"
container_dir = "/run/global"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.control_sockets]
host_dir = "/var/run/aw/{runtime_id}"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let target = cfg.effective_target("default").unwrap();
    let effective = &target.control_sockets;
    assert_eq!(effective.host_dir, "/var/run/aw/{runtime_id}");
    assert_eq!(effective.container_dir, "/run/global");
}

#[test]
fn gateway_config_rejects_old_socket_path_sources() {
    for (config, expected) in [
        (
            r#"
schema_version = "1"

[container_agent]
control_socket = "/run/aw-gateway/agent.sock"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
            "unknown field `container_agent`",
        ),
        (
            r#"
schema_version = "1"

[target_defaults.container_agent.ssh_bridge]
enabled = true
socket = "/run/aw-gateway/ssh.sock"
target = "127.0.0.1:22"
mode = "0600"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
            "container_agent.ssh_bridge.socket is managed by control_sockets.container_dir",
        ),
    ] {
        let err = match toml::from_str::<GatewayConfig>(config) {
            Ok(cfg) => format!("{:#}", cfg.validate().unwrap_err()),
            Err(err) => err.to_string(),
        };
        assert!(err.contains(expected), "{err}");
    }
}

#[test]
fn standalone_agent_config_still_accepts_explicit_socket_paths() {
    let cfg: ContainerAgentFile = toml::from_str(
        r#"
schema_version = "1"

[container_agent]
control_socket = "/run/aw-gateway/agent.sock"

[container_agent.ssh_bridge]
enabled = true
socket = "/run/aw-gateway/ssh.sock"
target = "127.0.0.1:22"
mode = "0600"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
}

#[test]
fn container_ssh_policy_defaults_to_allowing_transfers() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    let target = cfg.effective_target("default").unwrap();
    assert_eq!(target.container_ssh.transfer.sftp, SftpTransferMode::Allow);
    assert_eq!(
        target.container_ssh.transfer.legacy_scp,
        LegacyScpTransferMode::Allow
    );
    cfg.validate().unwrap();
}

#[test]
fn container_ssh_policy_allows_independent_transfer_controls() {
    for (sftp, legacy_scp) in [
        (SftpTransferMode::Allow, LegacyScpTransferMode::Allow),
        (SftpTransferMode::Allow, LegacyScpTransferMode::Deny),
        (SftpTransferMode::Deny, LegacyScpTransferMode::Allow),
        (SftpTransferMode::Deny, LegacyScpTransferMode::Inbound),
        (SftpTransferMode::Deny, LegacyScpTransferMode::Outbound),
    ] {
        let cfg: GatewayConfig = toml::from_str(&format!(
            r#"
schema_version = "1"

[target_defaults.container_ssh.transfer]
sftp = "{}"
legacy_scp = "{}"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
            toml_transfer_mode(sftp),
            toml_legacy_scp_mode(legacy_scp),
        ))
        .unwrap();
        cfg.validate().unwrap();
        let target = cfg.effective_target("default").unwrap();
        assert_eq!(target.container_ssh.transfer.sftp, sftp);
        assert_eq!(target.container_ssh.transfer.legacy_scp, legacy_scp);
    }
}

fn toml_transfer_mode(mode: SftpTransferMode) -> &'static str {
    match mode {
        SftpTransferMode::Allow => "allow",
        SftpTransferMode::Deny => "deny",
    }
}

fn toml_legacy_scp_mode(mode: LegacyScpTransferMode) -> &'static str {
    match mode {
        LegacyScpTransferMode::Allow => "allow",
        LegacyScpTransferMode::Deny => "deny",
        LegacyScpTransferMode::Inbound => "inbound",
        LegacyScpTransferMode::Outbound => "outbound",
    }
}

#[test]
fn bootstrap_mounts_and_identity_validate() {
    let cfg = r#"
schema_version = "1"

[[target_defaults.container_mounts]]
source = "{state_dir}/bootstrap/aw-container-agent"
target = "/opt/aw-gateway/bin/aw-container-agent"
mode = "ro"

[[targets.default.container_mounts]]
source = "{state_dir}/bootstrap/target-only"
target = "/opt/aw-gateway/target-only"
mode = "ro"

[target_defaults.container_bootstrap]
enabled = true
entrypoint = "/opt/aw-gateway/bin/aw-container-bootstrap"
agent_program = "/opt/aw-gateway/bin/aw-container-agent"

[[target_defaults.container_bootstrap_steps]]
name = "validate-agent"
required = true
user = "root"
command = ["/usr/bin/test", "-x", "/opt/aw-gateway/bin/aw-container-agent"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.identity]
bootstrap_user = "root"
session_user = "awuser"
session_uid = "{uid}"
session_gid = "{gid}"
session_home = "/home/awuser"
session_shell = "/bin/bash"
"#;
    let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
    cfg.validate().unwrap();
}

#[test]
fn literal_session_user_requires_explicit_uid_and_gid() {
    let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.identity]
session_user = "awuser"
"#;
    let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
    let err = cfg.validate().unwrap_err();
    assert!(
        err.to_string()
            .contains("literal session_user requires explicit session_uid and session_gid")
    );
}

#[test]
fn bootstrap_identity_rejects_passwd_delimiters() {
    for (field, home, shell, state_dir) in [
        ("session_home", "/home/aw:user", "/bin/bash", "/state"),
        ("session_home", "/home/aw\nuser", "/bin/bash", "/state"),
        ("session_shell", "/home/awuser", "/bin/ba:sh", "/state"),
        ("session_shell", "/home/awuser", "/bin/ba\rsh", "/state"),
        ("state_dir", "/home/awuser", "/bin/bash", "/sta\0te"),
    ] {
        let identity = BootstrapIdentity {
            session_user: "awuser".into(),
            session_uid: 2450,
            session_gid: 2450,
            session_home: home.into(),
            session_shell: shell.into(),
            state_dir: state_dir.into(),
        };
        let err = identity.validate().unwrap_err();
        assert!(
            err.to_string().contains(field),
            "expected {field} in error, got {err}"
        );
    }
}

#[test]
fn rejects_missing_default_target() {
    let cfg = r#"
schema_version = "1"
default_target = "missing"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#;
    let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn rejects_unknown_interpolation_variables() {
    let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{missing}"
"#;
    let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn accepts_gateway_owned_exit_cleanup() {
    let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.idle_cleanup]
owner = "gateway"
action = "exit_container"
"#;
    let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
    assert!(cfg.validate().is_ok());
}

#[test]
fn rejects_gateway_owned_process_reaping() {
    let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.idle_cleanup]
owner = "gateway"
action = "reap_processes"
"#;
    let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn rejects_unsupported_ssh_bridge_group_mode() {
    let cfg = r#"
schema_version = "1"

[container_agent.ssh_bridge]
enabled = true
socket = "{container_state_dir}/ssh.sock"
mode = "0660"
"#;
    let cfg: ContainerAgentFile = toml::from_str(cfg).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn listen_mode_allows_omitted_port_for_dynamic_allocation() {
    let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.local_ssh]
mode = "listen"
host = "127.0.0.1"
"#;
    let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
    cfg.validate().unwrap();
}

#[test]
fn local_ssh_allows_published_port_backend() {
    let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.local_ssh]
mode = "listen"
backend = "published_port"
readiness = "ssh_only"
host = "127.0.0.1"
"#;
    let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
    cfg.validate().unwrap();
}

#[test]
fn local_ssh_rejects_ssh_only_with_socket_backend() {
    let cfg = r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.local_ssh]
mode = "listen"
backend = "socket"
readiness = "ssh_only"
host = "127.0.0.1"
"#;
    let cfg: GatewayConfig = toml::from_str(cfg).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn env_value_requires_exactly_one_source() {
    let value = EnvValue {
        value: Some("a".into()),
        file: Some("/tmp/a".into()),
        inherit: None,
        interpolate: true,
        required: true,
    };
    assert!(value.validate().is_err());
}

#[test]
fn env_value_renders_file_path_and_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, "token-{name}\n").unwrap();
    let value = EnvValue {
        value: None,
        file: Some("{dir}/token".into()),
        inherit: None,
        interpolate: true,
        required: true,
    };
    let vars = BTreeMap::from([
        ("dir".into(), dir.path().display().to_string()),
        ("name".into(), "workspace".into()),
    ]);

    assert_eq!(
        value.resolve(&vars).unwrap(),
        Some("token-workspace".into())
    );
}

#[test]
fn parses_duration_units() {
    assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
    assert!(parse_duration("5").is_err());
    assert!(parse_duration("1d").is_err());
    assert!(parse_duration("5000000000000000000h").is_err());
}

#[test]
fn partial_logging_config_keeps_console_default() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[logging]
level = "debug"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    assert!(cfg.logging.console);
}

#[test]
fn host_steps_reject_process_health_checks() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.host_steps]]
name = "bad"
command = ["/bin/true"]
health_check = { type = "process" }
"#,
    )
    .unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn lifecycle_and_host_step_timeouts_validate() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/true"]
timeout = "250ms"

[[target_defaults.host_steps]]
name = "firewall"
command = ["/bin/true"]
timeout = "2m"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    assert_eq!(target.lifecycle_steps[0].timeout.as_deref(), Some("250ms"));
    assert_eq!(target.host_steps[0].timeout.as_deref(), Some("2m"));
}

#[test]
fn lifecycle_and_host_step_timeouts_reject_invalid_durations() {
    for (config, expected) in [
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/true"]
timeout = "5"
"#,
            "missing an explicit unit",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.host_steps]]
name = "firewall"
command = ["/bin/true"]
timeout = "1d"
"#,
            "unsupported duration unit",
        ),
    ] {
        let cfg: GatewayConfig = toml::from_str(config).unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(expected), "{err}");
    }
}

#[test]
fn client_config_rejects_newline_scalars() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[client_config]
host = "example.com\nProxyCommand bad"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn disabled_agent_allows_no_services_or_bridge() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[target_defaults.container_agent]
enabled = false

[target_defaults.container_agent.ssh_bridge]
enabled = false
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
}

#[test]
fn enabled_agent_allows_disabled_control_socket() {
    let cfg: ContainerAgentFile = toml::from_str(
        r#"
schema_version = "1"

[container_agent]
control_socket = false

[[container_agent.services]]
name = "sshd"
command = ["/bin/true"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    assert_eq!(
        cfg.container_agent.control_socket,
        Some(ControlSocketConfig::Enabled(false))
    );
}

#[test]
fn target_runtime_and_env_knobs_validate() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu"
mode = "fixed"
name = "{image_slug}"

[targets.default.runtime]
extra_run_args = ["--cap-add", "SYS_ADMIN"]

[targets.default.container_env]
CODEX_HOME = "/var/lib/codex"

[targets.default.session_env]
CODEX_HOME = "/var/lib/codex"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
}

#[test]
fn ssh_dispatch_defaults_include_launch_actions() {
    let cfg = SshDispatchConfig::default();
    assert!(
        cfg.enabled_actions
            .iter()
            .any(|action| action == "launches")
    );
    assert!(cfg.enabled_actions.iter().any(|action| action == "launch"));
    cfg.validate().unwrap();
}

#[test]
fn ssh_dispatch_enabled_actions_accepts_current_action_set() {
    let cfg: SshDispatchConfig = toml::from_str(
        r#"
enabled_actions = [
  "connect",
  "up",
  "run",
  "launches",
  "launch",
  "status",
  "targets",
  "stop",
  "remove",
  "set-default",
  "show-default",
  "reset-default",
  "add-key",
  "add-host-key",
  "add-container-key",
  "client-config",
  "client-bundle",
  "help",
]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.enabled_actions, default_enabled_actions());
}

#[test]
fn ssh_dispatch_rejects_retired_enabled_gateway_actions_key() {
    let err = toml::from_str::<SshDispatchConfig>(
        r#"
enabled_gateway_actions = ["connect"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn ssh_dispatch_validation_reports_enabled_actions() {
    let cfg: SshDispatchConfig = toml::from_str(
        r#"
enabled_actions = ["connect", "bogus"]
"#,
    )
    .unwrap();
    let err = cfg.validate().unwrap_err();
    assert!(
        err.to_string()
            .contains("unknown enabled_actions entry \"bogus\""),
        "{err}"
    );
}

#[test]
fn target_workspace_template_validates() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
[targets.default.workspace]
path = "{home}/workspace-internal"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    assert_eq!(
        cfg.effective_target("default").unwrap().workspace.path,
        "{home}/workspace-internal"
    );
}

#[test]
fn target_container_agent_service_overrides_global_service() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.container_agent.services]]
name = "acl-proxy"
command = ["acl-proxy", "--config", "/etc/acl-proxy/acl-proxy.toml"]

[[target_defaults.container_agent.services]]
name = "container-sshd"
command = ["/opt/aw-gateway/bin/start-container-sshd"]
depends_on = ["acl-proxy"]

[[targets.default.container_agent.services]]
name = "acl-proxy"
command = ["acl-proxy", "--config", "/etc/acl-proxy/internal-acl-proxy.toml"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let effective = &target.container_agent;
    assert_eq!(effective.services.len(), 2);
    let acl_proxy = effective
        .services
        .iter()
        .find(|service| service.name == "acl-proxy")
        .unwrap();
    assert_eq!(
        acl_proxy.command,
        [
            "acl-proxy",
            "--config",
            "/etc/acl-proxy/internal-acl-proxy.toml"
        ]
    );
}

#[test]
fn container_agent_services_replace_in_place_and_append_without_order_controls() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
use = ["policy"]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.container_agent.services]]
name = "first"
command = ["/bin/first"]

[[target_defaults.container_agent.services]]
name = "replace-me"
command = ["/bin/old"]

[[target_templates.policy.container_agent.services]]
name = "replace-me"
command = ["/bin/template"]

[[target_templates.policy.container_agent.services]]
name = "template-new"
command = ["/bin/template-new"]

[[targets.default.container_agent.services]]
name = "concrete-new"
command = ["/bin/concrete-new"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let services: Vec<_> = target
        .container_agent
        .services
        .iter()
        .map(|service| (service.name.as_str(), service.command[0].as_str()))
        .collect();
    assert_eq!(
        services,
        [
            ("first", "/bin/first"),
            ("replace-me", "/bin/template"),
            ("template-new", "/bin/template-new"),
            ("concrete-new", "/bin/concrete-new")
        ]
    );
}

#[test]
fn target_container_ssh_transfer_replaces_global_policy() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults.container_ssh.transfer]
sftp = "allow"
legacy_scp = "allow"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.container_ssh.transfer]
sftp = "deny"
legacy_scp = "outbound"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let effective = &target.container_ssh;
    assert_eq!(effective.transfer.sftp, SftpTransferMode::Deny);
    assert_eq!(
        effective.transfer.legacy_scp,
        LegacyScpTransferMode::Outbound
    );
}

#[test]
fn target_container_ssh_transfer_overlays_fields_independently() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults.container_ssh.transfer]
sftp = "allow"
legacy_scp = "inbound"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.container_ssh.transfer]
sftp = "deny"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let effective = cfg.effective_target("default").unwrap().container_ssh;
    assert_eq!(effective.transfer.sftp, SftpTransferMode::Deny);
    assert_eq!(
        effective.transfer.legacy_scp,
        LegacyScpTransferMode::Inbound
    );
}

#[test]
fn target_container_bootstrap_overlays_global_fields() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults.container_bootstrap]
enabled = false
entrypoint = "/global/bootstrap"
agent_program = "/global/agent"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.container_bootstrap]
enabled = true
agent_program = "/target/agent"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let effective = &target.container_bootstrap;
    assert!(effective.enabled);
    assert_eq!(effective.entrypoint, "/global/bootstrap");
    assert_eq!(effective.agent_program, "/target/agent");
}

#[test]
fn target_lifecycle_steps_replace_remove_append_and_order_by_phase() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "first"
command = ["/bin/first"]

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "replace-me"
command = ["/bin/old"]

[[target_defaults.lifecycle_steps]]
phase = "post_stop"
name = "first"
command = ["/bin/post"]

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "replace-me"
command = ["/bin/new"]

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "before-replace"
before = "replace-me"
command = ["/bin/before"]

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "after-first"
after = "first"
command = ["/bin/after"]

[[targets.default.lifecycle_steps]]
phase = "post_stop"
name = "first"
enabled = false
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let effective = &target.lifecycle_steps;
    let pre_start: Vec<_> = effective
        .iter()
        .filter(|step| step.phase == LifecyclePhase::PreStart)
        .map(|step| (step.name.as_str(), step.command[0].as_str()))
        .collect();
    assert_eq!(
        pre_start,
        [
            ("first", "/bin/first"),
            ("after-first", "/bin/after"),
            ("before-replace", "/bin/before"),
            ("replace-me", "/bin/new")
        ]
    );
    assert!(
        !effective
            .iter()
            .any(|step| step.phase == LifecyclePhase::PostStop)
    );
}

#[test]
fn host_steps_insert_before_and_after_inherited_steps() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.host_steps]]
name = "first"
command = ["/bin/first"]

[[target_defaults.host_steps]]
name = "last"
command = ["/bin/last"]

[[targets.default.host_steps]]
name = "after-first"
after = "first"
command = ["/bin/after-first"]

[[targets.default.host_steps]]
name = "before-last"
before = "last"
command = ["/bin/before-last"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let steps: Vec<_> = target
        .host_steps
        .iter()
        .map(|step| (step.name.as_str(), step.command[0].as_str()))
        .collect();
    assert_eq!(
        steps,
        [
            ("first", "/bin/first"),
            ("after-first", "/bin/after-first"),
            ("before-last", "/bin/before-last"),
            ("last", "/bin/last")
        ]
    );
}

#[test]
fn container_bootstrap_steps_insert_before_and_after_inherited_steps() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.container_bootstrap_steps]]
name = "first"
command = ["/bin/first"]

[[target_defaults.container_bootstrap_steps]]
name = "last"
command = ["/bin/last"]

[[targets.default.container_bootstrap_steps]]
name = "after-first"
after = "first"
command = ["/bin/after-first"]

[[targets.default.container_bootstrap_steps]]
name = "before-last"
before = "last"
command = ["/bin/before-last"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let steps: Vec<_> = target
        .container_bootstrap_steps
        .iter()
        .map(|step| (step.name.as_str(), step.command[0].as_str()))
        .collect();
    assert_eq!(
        steps,
        [
            ("first", "/bin/first"),
            ("after-first", "/bin/after-first"),
            ("before-last", "/bin/before-last"),
            ("last", "/bin/last")
        ]
    );
}

#[test]
fn lifecycle_step_before_after_references_are_phase_scoped() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "anchor"
command = ["/bin/pre-anchor"]

[[target_defaults.lifecycle_steps]]
phase = "post_stop"
name = "anchor"
command = ["/bin/post-anchor"]

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "after-anchor"
after = "anchor"
command = ["/bin/pre-after"]

[[targets.default.lifecycle_steps]]
phase = "post_stop"
name = "before-anchor"
before = "anchor"
command = ["/bin/post-before"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let pre_start: Vec<_> = target
        .lifecycle_steps
        .iter()
        .filter(|step| step.phase == LifecyclePhase::PreStart)
        .map(|step| (step.name.as_str(), step.command[0].as_str()))
        .collect();
    let post_stop: Vec<_> = target
        .lifecycle_steps
        .iter()
        .filter(|step| step.phase == LifecyclePhase::PostStop)
        .map(|step| (step.name.as_str(), step.command[0].as_str()))
        .collect();
    assert_eq!(
        pre_start,
        [
            ("anchor", "/bin/pre-anchor"),
            ("after-anchor", "/bin/pre-after")
        ]
    );
    assert_eq!(
        post_stop,
        [
            ("before-anchor", "/bin/post-before"),
            ("anchor", "/bin/post-anchor")
        ]
    );
}

#[test]
fn target_step_timeout_only_overrides_keep_inherited_payload() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/prep"]
timeout = "10s"

[[target_defaults.host_steps]]
name = "firewall"
command = ["/bin/firewall"]
timeout = "10s"

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "prep"
timeout = "20s"

[[targets.default.host_steps]]
name = "firewall"
timeout = "30s"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let lifecycle = &target.lifecycle_steps;
    assert_eq!(lifecycle[0].command, ["/bin/prep"]);
    assert_eq!(lifecycle[0].timeout.as_deref(), Some("20s"));
    let host = &target.host_steps;
    assert_eq!(host[0].command, ["/bin/firewall"]);
    assert_eq!(host[0].timeout.as_deref(), Some("30s"));
}

#[test]
fn container_bootstrap_step_replacement_does_not_inherit_payload() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.container_bootstrap_steps]]
name = "bootstrap"
required = false
user = "worker"
command = ["/bin/old"]
timeout = "10s"

[[targets.default.container_bootstrap_steps]]
name = "bootstrap"
command = ["/bin/new"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let target = cfg.effective_target("default").unwrap();
    let step = &target.container_bootstrap_steps[0];
    assert_eq!(step.name, "bootstrap");
    assert!(step.required);
    assert_eq!(step.user, "root");
    assert_eq!(step.command, ["/bin/new"]);
    assert_eq!(step.timeout, None);
}

#[test]
fn target_step_merge_rejects_invalid_controls() {
    for (config, expected) in [
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.host_steps]]
name = "missing"
enabled = false
"#,
            "disabled but does not match an inherited entry",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.host_steps]]
name = "firewall"
command = ["/bin/old"]

[[targets.default.host_steps]]
name = "firewall"
before = "other"
command = ["/bin/new"]
"#,
            "must not set before or after",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.container_bootstrap_steps]]
name = "disabled"
enabled = false
command = ["/bin/bad"]
"#,
            "disabled but includes command payload",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "new"
after = "missing"
command = ["/bin/new"]
"#,
            "references missing after",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "new"
before = "missing"
command = ["/bin/new"]
"#,
            "references missing before",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.host_steps]]
name = "duplicate"
command = ["/bin/one"]

[[targets.default.host_steps]]
name = "duplicate"
command = ["/bin/two"]
"#,
            "defines duplicate host_steps duplicate",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.host_steps]]
name = "duplicate"
command = ["/bin/one"]

[[target_defaults.host_steps]]
name = "duplicate"
command = ["/bin/two"]
"#,
            "target \"target_defaults\" defines duplicate host_steps duplicate",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[targets.default.host_steps]]
name = "one"
before = "a"
after = "b"
command = ["/bin/one"]
"#,
            "sets both before and after",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.host_steps]]
name = "firewall"
command = ["/bin/old"]

[[targets.default.host_steps]]
name = "firewall"
enabled = false
timeout = "1s"
"#,
            "disabled but includes command payload",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/old"]

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "prep"
enabled = false
timeout = "1s"
"#,
            "disabled but includes command payload",
        ),
    ] {
        let cfg: GatewayConfig = toml::from_str(config).unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(expected), "{err}");
    }
}

#[test]
fn target_defaults_overlay_into_effective_target_shape() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults]
image = "ubuntu/base"
mode = "ephemeral"
ephemeral_name = "worker-{session_id}"
stop_when_idle = true

[target_defaults.workspace]
path = "{home}/.cache/aw-gateway/workspaces/{target}-{session_id}"
state_dir = ".state"
cleanup = "success"

[target_defaults.runtime]
extra_run_args = ["--default"]

[target_defaults.container_env]
KEEP = "default"
OVERRIDE = "default"

[target_defaults.session_env]
SESSION = "default"

[[target_defaults.container_mounts]]
source = "/tmp/default"
target = "/mnt/default"
mode = "ro"

[target_defaults.idle_cleanup]
owner = "gateway"
action = "exit_container"

[target_defaults.container_ssh.transfer]
sftp = "deny"
legacy_scp = "inbound"

[target_defaults.control_sockets]
host_dir = "/tmp/aw/{runtime_id}"
container_dir = "/run/default"

[target_defaults.container_bootstrap]
enabled = true
entrypoint = "/default/bootstrap"
agent_program = "/default/agent"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/default-prep"]
timeout = "10s"

[[target_defaults.host_steps]]
name = "host-prep"
command = ["/bin/default-host"]

[[target_defaults.container_bootstrap_steps]]
name = "bootstrap-default"
command = ["/bin/default-bootstrap"]

[target_defaults.container_agent]
enabled = true

[[target_defaults.container_agent.services]]
name = "svc"
command = ["/bin/default-service"]

[targets.default]
image = "ubuntu/dev"
name = "{image_slug}"

[targets.default.container_env]
OVERRIDE = "target"
TARGET_ONLY = "target"

[[targets.default.container_mounts]]
source = "/tmp/target"
target = "/mnt/target"
mode = "rw"

[targets.default.runtime]
extra_run_args = ["--target"]

[targets.default.workspace]
cleanup = "always"

[[targets.default.lifecycle_steps]]
phase = "pre_start"
name = "prep"
timeout = "20s"

[[targets.default.host_steps]]
name = "host-prep"
command = ["/bin/target-host"]

[[targets.default.container_bootstrap_steps]]
name = "bootstrap-default"
enabled = false

[[targets.default.container_agent.services]]
name = "svc"
command = ["/bin/target-service"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let target = cfg.effective_target("default").unwrap();
    assert_eq!(target.image, "ubuntu/dev");
    assert_eq!(target.mode, TargetMode::Ephemeral);
    assert_eq!(target.workspace.state_dir, ".state");
    assert_eq!(target.workspace.cleanup, WorkspaceCleanup::Always);
    assert_eq!(target.runtime.extra_run_args, ["--target"]);
    assert_eq!(target.container_env["KEEP"], "default");
    assert_eq!(target.container_env["OVERRIDE"], "target");
    assert_eq!(target.container_mounts.len(), 2);
    assert_eq!(target.container_ssh.transfer.sftp, SftpTransferMode::Deny);
    assert_eq!(target.control_sockets.container_dir, "/run/default");
    assert_eq!(target.container_bootstrap.entrypoint, "/default/bootstrap");
    assert_eq!(target.lifecycle_steps[0].command, ["/bin/default-prep"]);
    assert_eq!(target.lifecycle_steps[0].timeout.as_deref(), Some("20s"));
    assert_eq!(target.host_steps[0].command, ["/bin/target-host"]);
    assert!(target.container_bootstrap_steps.is_empty());
    assert_eq!(
        target.container_agent.services[0].command,
        ["/bin/target-service"]
    );
}

#[test]
fn target_templates_overlay_in_use_order_and_allow_concrete_overrides() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults]
image = "ubuntu/default"
mode = "fixed"
name = "default-name"

[target_defaults.container_env]
KEEP = "default"
OVERRIDE = "default"

[target_templates.runtime]
image = "ubuntu/runtime"
container_user = "worker"

[target_templates.runtime.container_env]
RUNTIME = "true"
OVERRIDE = "runtime"

[target_templates.policy]
name = "policy-name"

[target_templates.policy.container_env]
POLICY = "true"
OVERRIDE = "policy"

[targets.default]
use = ["runtime", "policy"]
image = "ubuntu/final"
name = "final-name"

[targets.default.container_env]
TARGET = "true"
OVERRIDE = "target"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let target = cfg.effective_target("default").unwrap();
    assert_eq!(target.image, "ubuntu/final");
    assert_eq!(target.name.as_deref(), Some("final-name"));
    assert_eq!(target.container_user.as_deref(), Some("worker"));
    assert_eq!(target.container_env["KEEP"], "default");
    assert_eq!(target.container_env["RUNTIME"], "true");
    assert_eq!(target.container_env["POLICY"], "true");
    assert_eq!(target.container_env["TARGET"], "true");
    assert_eq!(target.container_env["OVERRIDE"], "target");
}

#[test]
fn target_templates_can_nest_and_override_inherited_steps() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults]
image = "ubuntu/default"
mode = "fixed"
name = "default-name"

[[target_defaults.host_steps]]
name = "firewall"
command = ["/bin/firewall"]
timeout = "10s"

[target_templates.timeout-policy]

[[target_templates.timeout-policy.host_steps]]
name = "firewall"
timeout = "30s"

[target_templates.runtime]
use = ["timeout-policy"]
container_home = "/home/worker"

[targets.default]
use = ["runtime"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let target = cfg.effective_target("default").unwrap();
    assert_eq!(
        target.container_home.as_deref(),
        Some(Path::new("/home/worker"))
    );
    assert_eq!(target.host_steps[0].command, ["/bin/firewall"]);
    assert_eq!(target.host_steps[0].timeout.as_deref(), Some("30s"));
}

#[test]
fn target_chain_overlays_defaults_nested_templates_and_concrete_target() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults]
image = "ubuntu/default"
mode = "fixed"
name = "default-name"

[target_defaults.container_env]
KEEP = "default"
OVERRIDE = "default"

[[target_defaults.container_mounts]]
source = "/tmp/default"
target = "/mnt/default"
mode = "ro"

[target_templates.base]
container_user = "base-user"

[target_templates.base.container_env]
BASE = "true"
OVERRIDE = "base"

[target_templates.runtime]
use = ["base"]
image = "ubuntu/runtime"

[target_templates.runtime.container_env]
RUNTIME = "true"
OVERRIDE = "runtime"

[target_templates.policy]
name = "policy-name"

[target_templates.policy.container_env]
POLICY = "true"
OVERRIDE = "policy"

[[target_templates.policy.container_mounts]]
source = "/tmp/policy"
target = "/mnt/policy"
mode = "rw"

[targets.default]
use = ["runtime", "policy"]
image = "ubuntu/final"
name = "final-name"

[targets.default.container_env]
TARGET = "true"
OVERRIDE = "target"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let target = cfg.effective_target("default").unwrap();
    assert_eq!(target.image, "ubuntu/final");
    assert_eq!(target.name.as_deref(), Some("final-name"));
    assert_eq!(target.container_user.as_deref(), Some("base-user"));
    assert_eq!(target.container_env["KEEP"], "default");
    assert_eq!(target.container_env["BASE"], "true");
    assert_eq!(target.container_env["RUNTIME"], "true");
    assert_eq!(target.container_env["POLICY"], "true");
    assert_eq!(target.container_env["TARGET"], "true");
    assert_eq!(target.container_env["OVERRIDE"], "target");
    assert_eq!(target.container_mounts.len(), 2);
    assert_eq!(target.container_mounts[0].target, "/mnt/default");
    assert_eq!(target.container_mounts[1].target, "/mnt/policy");
}

#[test]
fn target_template_cycles_and_unknown_names_are_rejected() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_templates.a]
use = ["b"]

[target_templates.b]
use = ["a"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("target template cycle: a -> b -> a"), "{err}");

    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
use = ["missing"]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("target \"default\" uses target template \"missing\""),
        "{err}"
    );
    assert!(err.contains("unknown target template \"missing\""), "{err}");

    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_templates.outer]
use = ["missing"]

[targets.default]
use = ["outer"]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("target template \"outer\" uses target template \"missing\""),
        "{err}"
    );
    assert!(err.contains("unknown target template \"missing\""), "{err}");
    let err = format!("{:#}", cfg.effective_target("default").unwrap_err());
    assert!(
        err.contains("target \"default\" uses target template \"outer\""),
        "{err}"
    );
    assert!(
        err.contains("target template \"outer\" uses target template \"missing\""),
        "{err}"
    );
    assert!(err.contains("unknown target template \"missing\""), "{err}");
}

#[test]
fn target_template_effective_validation_runs_after_overlay() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_templates.ephemeral]
mode = "ephemeral"

[targets.default]
use = ["ephemeral"]
image = "ubuntu/dev"
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("ephemeral target \"default\" requires ephemeral_name"),
        "{err}"
    );
}

#[test]
fn target_defaults_do_not_support_use() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults]
use = ["base"]

[target_templates.base]
image = "ubuntu/dev"

[targets.default]
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("target_defaults does not support use"),
        "{err}"
    );
}

#[test]
fn template_names_use_target_and_launch_name_validation() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_templates."bad name"]
image = "ubuntu/dev"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("target template"), "{err}");
    assert!(err.contains("bad name"), "{err}");

    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_templates."bad name"]
target = "default"
command = ["true"]

[launches.agent]
target = "default"
command = ["true"]
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("launch template"), "{err}");
    assert!(err.contains("bad name"), "{err}");
}

#[test]
fn target_defaults_can_supply_required_image() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[target_defaults]
image = "ubuntu/default"

[targets.default]
mode = "fixed"
name = "{image_slug}"
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    assert_eq!(
        cfg.effective_target("default").unwrap().image,
        "ubuntu/default"
    );
}

#[test]
fn target_defaults_validate_present_fields_before_overlay() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"
default_target = "default"

[target_defaults]
image = "scratch/dev"
name = "scratch-dev"

[target_defaults.workspace]
path = "{bad}"

[targets.default]

[targets.default.workspace]
path = "workspace"
"#,
    )
    .unwrap();

    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("unknown interpolation variable"), "{err}");
    assert!(err.contains("target.workspace.path"), "{err}");
}

#[test]
fn root_only_rejects_target_shaped_sections_at_root() {
    for config in [
        r#"schema_version = "1"
[workspace]
path = "workspace"
"#,
        r#"schema_version = "1"
[control_sockets]
container_dir = "/run/aw-gateway"
"#,
        r#"schema_version = "1"
[[lifecycle_steps]]
phase = "pre_start"
name = "prep"
command = ["/bin/true"]
"#,
        r#"schema_version = "1"
[container_agent]
enabled = false
"#,
    ] {
        let err = toml::from_str::<GatewayConfig>(config).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }
}

#[test]
fn disabled_agent_rejects_published_port_ssh_backend() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.local_ssh]
backend = "published_port"
readiness = "ssh_only"

[target_defaults.container_agent]
enabled = false
"#,
    )
    .unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn disabled_bridge_still_validates_shape() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[target_defaults.container_agent.ssh_bridge]
enabled = false
target = "missing-port"
"#,
    )
    .unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn launch_schema_validates_vars_templates_and_steps() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
target = "default"
description = "Run agent"
cwd = "{container_home}/{var.repo}"
env = { FLAG = "{var.flag}", LIMIT = "{var.limit}", PID = "{container_pid}" }
command = ["agent", "--mode", "{var.mode}"]

[launches.agent.vars]
repo = { type = "string", required = true }
flag = { type = "boolean", default = true }
limit = { type = "number", default = 2.5 }
mode = { type = "enum", values = ["fast", "safe"], default = "fast" }

[[launches.agent.steps]]
phase = "post_ready"
location = "container"
name = "prep"
required = false
timeout = "5s"
cwd = "{container_home}"
env = { STEP_FLAG = "{var.flag}" }
command = ["prep", "{var.repo}"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
}

#[test]
fn launch_defaults_overlay_into_effective_launch_shape() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
target = "default"
description = "Default description"
cwd = "{container_home}/default"
env = { KEEP = "default", OVERRIDE = "default" }
command = ["default-command"]

[launch_defaults.vars]
repo = { type = "string", required = true }
mode = { type = "enum", values = ["fast", "safe"], default = "fast" }

[[launch_defaults.steps]]
phase = "post_ready"
location = "container"
name = "prep"
command = ["default-prep"]
env = { STEP = "default" }

[launches.agent]
cwd = "{container_home}/agent"
env = { OVERRIDE = "launch", LAUNCH_ONLY = "launch" }
command = ["agent", "{var.repo}"]

[launches.agent.vars]
branch = { type = "string", default = "main" }

[[launches.agent.steps]]
phase = "post_ready"
location = "container"
name = "prep"
command = ["launch-prep"]

[[launches.agent.steps]]
phase = "post_ready"
location = "host"
name = "host-prep"
command = ["host-prep"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let launch = cfg.effective_launch("agent").unwrap();
    assert_eq!(launch.target, "default");
    assert_eq!(launch.description.as_deref(), Some("Default description"));
    assert_eq!(launch.cwd.as_deref(), Some("{container_home}/agent"));
    assert_eq!(launch.command, ["agent", "{var.repo}"]);
    assert_eq!(launch.env["KEEP"], "default");
    assert_eq!(launch.env["OVERRIDE"], "launch");
    assert!(launch.vars.contains_key("repo"));
    assert!(launch.vars.contains_key("branch"));
    assert_eq!(launch.steps.len(), 2);
    assert_eq!(launch.steps[0].name, "prep");
    assert_eq!(launch.steps[0].command, ["launch-prep"]);
    assert_eq!(launch.steps[1].name, "host-prep");
}

#[test]
fn launch_templates_overlay_in_use_order_and_allow_concrete_overrides() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
target = "default"
description = "Default description"
cwd = "{container_home}/default"
env = { KEEP = "default", OVERRIDE = "default" }
command = ["default-command"]

[launch_templates.repo]
cwd = "{container_home}/repo"
env = { REPO = "{var.repo}", OVERRIDE = "repo" }

[launch_templates.repo.vars]
repo = { type = "string", required = true }

[[launch_templates.repo.steps]]
phase = "post_ready"
location = "container"
name = "prep"
command = ["repo-prep", "{var.repo}"]

[launch_templates.codex]
command = ["codex", "exec", "{var.repo}"]
env = { CODEX_HOME = "{container_home}/.codex", OVERRIDE = "codex" }

[launches.review]
use = ["repo", "codex"]
description = "Review repo"
command = ["codex", "exec", "review", "{var.repo}"]
env = { OVERRIDE = "launch" }

[[launches.review.steps]]
phase = "post_ready"
location = "container"
name = "prep"
command = ["launch-prep", "{var.repo}"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let launch = cfg.effective_launch("review").unwrap();
    assert_eq!(launch.target, "default");
    assert_eq!(launch.description.as_deref(), Some("Review repo"));
    assert_eq!(launch.cwd.as_deref(), Some("{container_home}/repo"));
    assert_eq!(launch.command, ["codex", "exec", "review", "{var.repo}"]);
    assert_eq!(launch.env["KEEP"], "default");
    assert_eq!(launch.env["REPO"], "{var.repo}");
    assert_eq!(launch.env["CODEX_HOME"], "{container_home}/.codex");
    assert_eq!(launch.env["OVERRIDE"], "launch");
    assert!(launch.vars.contains_key("repo"));
    assert_eq!(launch.steps.len(), 1);
    assert_eq!(launch.steps[0].command, ["launch-prep", "{var.repo}"]);
}

#[test]
fn launch_templates_can_nest() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_templates.base]
target = "default"

[launch_templates.command]
use = ["base"]
command = ["true"]

[launches.agent]
use = ["command"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let launch = cfg.effective_launch("agent").unwrap();
    assert_eq!(launch.target, "default");
    assert_eq!(launch.command, ["true"]);
}

#[test]
fn launch_chain_overlays_defaults_nested_templates_and_concrete_launch() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
target = "default"
description = "Default description"
cwd = "{container_home}/default"
env = { KEEP = "default", OVERRIDE = "default" }
command = ["default-command"]

[launch_defaults.vars]
repo = { type = "string", required = true }

[[launch_defaults.steps]]
phase = "post_ready"
location = "container"
name = "prep"
command = ["default-prep"]

[launch_templates.base]
cwd = "{container_home}/base"
env = { BASE = "true", OVERRIDE = "base" }

[launch_templates.runtime]
use = ["base"]
command = ["runtime-command", "{var.repo}"]
env = { RUNTIME = "true", OVERRIDE = "runtime" }

[launch_templates.policy]
description = "Policy description"
env = { POLICY = "true", OVERRIDE = "policy" }

[launch_templates.policy.vars]
mode = { type = "enum", values = ["fast", "safe"], default = "fast" }

[launches.review]
use = ["runtime", "policy"]
description = "Review description"
command = ["review", "{var.repo}", "{var.mode}"]
env = { LAUNCH = "true", OVERRIDE = "launch" }

[[launches.review.steps]]
phase = "post_ready"
location = "container"
name = "prep"
command = ["launch-prep", "{var.repo}"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let launch = cfg.effective_launch("review").unwrap();
    assert_eq!(launch.target, "default");
    assert_eq!(launch.description.as_deref(), Some("Review description"));
    assert_eq!(launch.cwd.as_deref(), Some("{container_home}/base"));
    assert_eq!(launch.command, ["review", "{var.repo}", "{var.mode}"]);
    assert_eq!(launch.env["KEEP"], "default");
    assert_eq!(launch.env["BASE"], "true");
    assert_eq!(launch.env["RUNTIME"], "true");
    assert_eq!(launch.env["POLICY"], "true");
    assert_eq!(launch.env["LAUNCH"], "true");
    assert_eq!(launch.env["OVERRIDE"], "launch");
    assert!(launch.vars.contains_key("repo"));
    assert!(launch.vars.contains_key("mode"));
    assert_eq!(launch.steps.len(), 1);
    assert_eq!(launch.steps[0].command, ["launch-prep", "{var.repo}"]);
}

#[test]
fn launch_step_replacement_preserves_position_and_append_order_across_chain() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
target = "default"
command = ["default-command"]

[[launch_defaults.steps]]
phase = "post_ready"
location = "container"
name = "first"
command = ["default-first"]

[[launch_defaults.steps]]
phase = "post_ready"
location = "container"
name = "replace-me"
command = ["default-replace"]

[launch_templates.runtime]

[[launch_templates.runtime.steps]]
phase = "post_ready"
location = "container"
name = "replace-me"
command = ["template-replace"]

[[launch_templates.runtime.steps]]
phase = "post_ready"
location = "container"
name = "template-new"
command = ["template-new"]

[launches.review]
use = ["runtime"]
command = ["review"]

[[launches.review.steps]]
phase = "post_ready"
location = "container"
name = "concrete-new"
command = ["concrete-new"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let launch = cfg.effective_launch("review").unwrap();
    let steps: Vec<_> = launch
        .steps
        .iter()
        .map(|step| (step.name.as_str(), step.command[0].as_str()))
        .collect();
    assert_eq!(
        steps,
        [
            ("first", "default-first"),
            ("replace-me", "template-replace"),
            ("template-new", "template-new"),
            ("concrete-new", "concrete-new")
        ]
    );
}

#[test]
fn launch_template_cycles_and_unknown_names_are_rejected() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_templates.a]
use = ["b"]

[launch_templates.b]
use = ["a"]

[launches.agent]
target = "default"
command = ["true"]
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("launch template cycle: a -> b -> a"), "{err}");

    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
use = ["missing"]
target = "default"
command = ["true"]
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("launch \"agent\" uses launch template \"missing\""),
        "{err}"
    );
    assert!(err.contains("unknown launch template \"missing\""), "{err}");

    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_templates.outer]
use = ["missing"]

[launches.agent]
use = ["outer"]
target = "default"
command = ["true"]
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("launch template \"outer\" uses launch template \"missing\""),
        "{err}"
    );
    assert!(err.contains("unknown launch template \"missing\""), "{err}");
    let err = format!("{:#}", cfg.effective_launch("agent").unwrap_err());
    assert!(
        err.contains("launch \"agent\" uses launch template \"outer\""),
        "{err}"
    );
    assert!(
        err.contains("launch template \"outer\" uses launch template \"missing\""),
        "{err}"
    );
    assert!(err.contains("unknown launch template \"missing\""), "{err}");
}

#[test]
fn launch_template_effective_validation_runs_after_overlay() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_templates.target]
target = "default"

[launches.agent]
use = ["target"]
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("validate effective launch \"agent\""), "{err}");
    assert!(
        err.contains("launch \"agent\" command is required after defaults"),
        "{err}"
    );

    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
command = ["true"]

[launch_templates.target]
target = "missing"

[launches.agent]
use = ["target"]
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("validate effective launch \"agent\""), "{err}");
    assert!(
        err.contains("launch \"agent\" references unknown target \"missing\""),
        "{err}"
    );
}

#[test]
fn launch_defaults_do_not_support_use() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
use = ["base"]

[launch_templates.base]
target = "default"
command = ["true"]

[launches.agent]
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("launch_defaults does not support use"),
        "{err}"
    );
}

#[test]
fn launch_defaults_are_validated_after_overlay() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
target = "default"

[launches.agent]
command = ["true"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
command = ["true"]
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("target is required after defaults"), "{err}");
}

#[test]
fn launch_defaults_validate_present_fields_without_launches() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"
default_target = "default"

[target_defaults]
image = "scratch/dev"
name = "scratch-dev"

[targets.default]

[launch_defaults.vars."bad name"]
type = "string"
"#,
    )
    .unwrap();

    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("launch var"), "{err}");
    assert!(err.contains("bad name"), "{err}");
}

#[test]
fn launch_partial_allows_later_var_bindings_but_effective_rejects_unbound_vars() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launch_defaults]
target = "default"
command = ["echo", "{var.future}"]
"#,
    )
    .unwrap();
    cfg.validate().unwrap();

    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["echo", "{var.future}"]
"#,
    )
    .unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(
        err.contains("unknown interpolation variable {var.future}"),
        "{err}"
    );
}

#[test]
fn launch_schema_rejects_bad_vars_and_templates() {
    for (config, expected) in [
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "missing"
command = ["true"]
"#,
            "unknown target",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true", "{repo}"]

[launches.bad.vars]
repo = { type = "string", required = true }
"#,
            "unknown interpolation variable",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true", "{var.repo}"]

[launches.bad.vars]
repo = { type = "string" }
"#,
            "must define default",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true"]

[launches.bad.vars]
mode = { type = "enum", values = [] }
"#,
            "values must not be empty",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true"]

[launches.bad.vars]
repo = { type = "string", required = true, default = "main" }
"#,
            "cannot set both required and default",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true"]

[launches.bad.vars]
repo = { type = "string", values = ["main"] }
"#,
            "values are only valid for enum variables",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true"]

[launches.bad.vars]
debug = { type = "boolean", values = ["true"] }
"#,
            "values are only valid for enum variables",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.bad]
target = "default"
command = ["true"]

[launches.bad.vars]
count = { type = "number", values = ["1"] }
"#,
            "values are only valid for enum variables",
        ),
    ] {
        let cfg: GatewayConfig = toml::from_str(config).unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(expected), "{err}");
    }
}

#[test]
fn pre_pid_templates_reject_container_pid() {
    for (config, expected) in [
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[targets.default.container_env]
PID = "{container_pid}"
"#,
            "target.container_env.PID",
        ),
        (
            r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[[target_defaults.lifecycle_steps]]
phase = "pre_start"
name = "pre"
command = ["echo", "{container_pid}"]
"#,
            "target.lifecycle_steps",
        ),
    ] {
        let cfg: GatewayConfig = toml::from_str(config).unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(expected), "{err}");
        assert!(err.contains("unknown interpolation variable"), "{err}");
    }
}

#[test]
fn includes_parse_and_compose_targets_and_launches() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config.d");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("workspace.toml"),
        r#"
[target_templates.base]
image = "ubuntu/base"
mode = "fixed"
name = "base"

[launch_templates.shell]
target = "default"
command = ["true"]

[targets.default]
use = ["base"]
name = "default"

[launches.agent]
use = ["shell"]
"#,
    )
    .unwrap();
    let root = dir.path().join("gateway.toml");
    std::fs::write(
        &root,
        r#"
schema_version = "1"
includes = ["config.d/*.toml"]
"#,
    )
    .unwrap();

    let cfg = GatewayConfig::load(&root).unwrap();
    assert_eq!(
        cfg.effective_target("default").unwrap().image,
        "ubuntu/base"
    );
    assert_eq!(cfg.effective_launch("agent").unwrap().command, ["true"]);
}

#[test]
fn includes_resolve_relative_to_declaring_file_and_support_nested() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config.d");
    let nested_dir = config_dir.join("nested");
    std::fs::create_dir_all(&nested_dir).unwrap();
    std::fs::write(
        config_dir.join("root-fragment.toml"),
        r#"
includes = ["nested/*.toml"]

[target_templates.base]
image = "ubuntu/base"
mode = "fixed"
name = "base"
"#,
    )
    .unwrap();
    std::fs::write(
        nested_dir.join("target.toml"),
        r#"
[targets.default]
use = ["base"]
"#,
    )
    .unwrap();
    let root = dir.path().join("gateway.toml");
    std::fs::write(
        &root,
        r#"
schema_version = "1"
includes = ["config.d/root-fragment.toml"]
"#,
    )
    .unwrap();

    let cfg = GatewayConfig::load(&root).unwrap();
    assert_eq!(
        cfg.effective_target("default").unwrap().image,
        "ubuntu/base"
    );
}

#[test]
fn includes_load_shared_fragments_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config.d");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("frag1.toml"),
        r#"
includes = ["shared.toml"]
"#,
    )
    .unwrap();
    std::fs::write(
        config_dir.join("frag2.toml"),
        r#"
includes = ["shared.toml"]
"#,
    )
    .unwrap();
    std::fs::write(
        config_dir.join("shared.toml"),
        r#"
[targets.default]
image = "ubuntu/shared"
mode = "fixed"
name = "shared"
"#,
    )
    .unwrap();
    let root = dir.path().join("gateway.toml");
    std::fs::write(
        &root,
        r#"
schema_version = "1"
includes = ["config.d/frag1.toml", "config.d/frag2.toml"]
"#,
    )
    .unwrap();

    let cfg = GatewayConfig::load(&root).unwrap();
    assert_eq!(cfg.targets.len(), 1);
    assert_eq!(
        cfg.effective_target("default").unwrap().image,
        "ubuntu/shared"
    );
}

#[test]
fn includes_expand_globs_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config.d");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("a.toml"),
        r#"
[targets.dup]
image = "ubuntu/a"
mode = "fixed"
name = "a"
"#,
    )
    .unwrap();
    std::fs::write(
        config_dir.join("b.toml"),
        r#"
[targets.dup]
image = "ubuntu/b"
mode = "fixed"
name = "b"
"#,
    )
    .unwrap();
    let root = dir.path().join("gateway.toml");
    std::fs::write(
        &root,
        r#"
schema_version = "1"
default_target = "dup"
includes = ["config.d/*.toml"]
"#,
    )
    .unwrap();

    let err = GatewayConfig::load(&root).unwrap_err().to_string();
    assert!(err.contains("duplicate target"), "{err}");
    assert!(
        err.contains(config_dir.join("b.toml").to_str().unwrap()),
        "{err}"
    );
}

#[test]
fn included_templates_can_be_used_by_root_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config.d");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("templates.toml"),
        r#"
[target_templates.base]
image = "ubuntu/base"
mode = "fixed"
name = "base"

[launch_templates.base]
target = "default"
command = ["true"]
"#,
    )
    .unwrap();
    let root = dir.path().join("gateway.toml");
    std::fs::write(
        &root,
        r#"
schema_version = "1"
includes = ["config.d/*.toml"]

[targets.default]
use = ["base"]

[launches.agent]
use = ["base"]
"#,
    )
    .unwrap();

    let cfg = GatewayConfig::load(&root).unwrap();
    assert_eq!(
        cfg.effective_target("default").unwrap().image,
        "ubuntu/base"
    );
    assert_eq!(cfg.effective_launch("agent").unwrap().command, ["true"]);
}

#[test]
fn includes_reject_duplicate_names() {
    let dir = tempfile::tempdir().unwrap();
    for (case, root_definition, include_definition, expected) in [
        (
            "target-template",
            "[target_templates.base]\nimage = \"ubuntu/root\"\n",
            "[target_templates.base]\nimage = \"ubuntu/include\"\n",
            "duplicate target template",
        ),
        (
            "launch-template",
            "[launch_templates.base]\ncommand = [\"root\"]\n",
            "[launch_templates.base]\ncommand = [\"include\"]\n",
            "duplicate launch template",
        ),
        (
            "target",
            "[targets.extra]\nimage = \"ubuntu/root\"\nmode = \"fixed\"\nname = \"root\"\n",
            "[targets.extra]\nimage = \"ubuntu/include\"\nmode = \"fixed\"\nname = \"include\"\n",
            "duplicate target",
        ),
        (
            "launch",
            "[launches.agent]\ntarget = \"default\"\ncommand = [\"root\"]\n",
            "[launches.agent]\ntarget = \"default\"\ncommand = [\"include\"]\n",
            "duplicate launch",
        ),
    ] {
        let case_dir = dir.path().join(case);
        let config_dir = case_dir.join("config.d");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("fragment.toml"), include_definition).unwrap();
        let root = case_dir.join("gateway.toml");
        std::fs::write(
            &root,
            format!(
                r#"
schema_version = "1"
includes = ["config.d/*.toml"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "default"

{root_definition}
"#
            ),
        )
        .unwrap();
        let err = GatewayConfig::load(&root).unwrap_err().to_string();
        assert!(err.contains(expected), "{case}: {err}");
    }
}

#[test]
fn includes_reject_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.toml");
    let b = dir.path().join("b.toml");
    std::fs::write(
        &a,
        format!(
            r#"
includes = ["{}"]
"#,
            b.display()
        ),
    )
    .unwrap();
    std::fs::write(
        &b,
        format!(
            r#"
includes = ["{}"]
"#,
            a.display()
        ),
    )
    .unwrap();
    let root = dir.path().join("gateway.toml");
    std::fs::write(
        &root,
        format!(
            r#"
schema_version = "1"
includes = ["{}"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{{image_slug}}"
"#,
            a.display()
        ),
    )
    .unwrap();
    let err = format!("{:#}", GatewayConfig::load(&root).unwrap_err());
    assert!(err.contains("cycle"), "{err}");
}

#[test]
fn includes_reject_root_only_sections_and_unknown_fields() {
    for (case, fragment, expected) in [
        ("schema-version", "schema_version = \"1\"\n", "root-only"),
        (
            "default-target",
            "default_target = \"other\"\n",
            "root-only",
        ),
        ("runtime", "[runtime]\ntype = \"podman\"\n", "root-only"),
        ("logging", "[logging]\nlevel = \"debug\"\n", "root-only"),
        ("http", "[http]\nenabled = true\n", "root-only"),
        (
            "ssh-dispatch",
            "[ssh_dispatch]\nallow_interactive_shell = false\n",
            "root-only",
        ),
        (
            "client-config",
            "[client_config]\nhost = \"example.test\"\n",
            "root-only",
        ),
        (
            "target-defaults",
            "[target_defaults]\nimage = \"ubuntu\"\n",
            "root-only",
        ),
        (
            "launch-defaults",
            "[launch_defaults]\ntarget = \"default\"\n",
            "root-only",
        ),
        ("unknown", "unknown = true\n", "unknown field"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("fragment.toml"), fragment).unwrap();
        let root = dir.path().join("gateway.toml");
        std::fs::write(
            &root,
            r#"
schema_version = "1"
includes = ["config.d/*.toml"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "default"
"#,
        )
        .unwrap();

        let err = format!("{:#}", GatewayConfig::load(&root).unwrap_err());
        assert!(err.contains(expected), "{case}: {err}");
    }
}

#[test]
fn legacy_split_include_keys_are_rejected() {
    for (case, root_extra, include_extra) in [
        ("root-target", "target_includes = []\n", ""),
        ("root-launch", "launch_includes = []\n", ""),
        ("include-target", "", "target_includes = []\n"),
        ("include-launch", "", "launch_includes = []\n"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config.d");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("fragment.toml"), include_extra).unwrap();
        let root = dir.path().join("gateway.toml");
        std::fs::write(
            &root,
            format!(
                r#"
schema_version = "1"
{root_extra}
includes = ["config.d/*.toml"]

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "default"
"#
            ),
        )
        .unwrap();

        let err = format!("{:#}", GatewayConfig::load(&root).unwrap_err());
        assert!(
            err.contains("target_includes") || err.contains("launch_includes"),
            "{case}: {err}"
        );
    }
}

#[test]
fn container_agent_rejects_service_dependency_cycles() {
    let cfg: ContainerAgentFile = toml::from_str(
        r#"
schema_version = "1"

[[container_agent.services]]
name = "acl-proxy"
command = ["/bin/true"]
depends_on = ["container-sshd"]

[[container_agent.services]]
name = "container-sshd"
command = ["/bin/true"]
depends_on = ["acl-proxy"]
"#,
    )
    .unwrap();

    let err = cfg.validate().unwrap_err().to_string();
    assert!(err.contains("dependency cycle"), "{err}");
}
