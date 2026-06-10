use super::*;
use crate::cli::{LaunchShowArgs, LaunchesArgs, RunArgs, StatusArg, TargetsArgs};
use crate::config::{LaunchVarConfig, LaunchVarType, LaunchVarValue};
use crate::ssh_dispatch::{
    ClientBundleAction, ClientConfigAction, GatewayAction, KeyAction, KeySourceAction, RunAction,
    StatusAction, TargetSessionAction,
};

fn launch_var_config(var_type: LaunchVarType, values: Option<Vec<&str>>) -> LaunchVarConfig {
    LaunchVarConfig {
        var_type,
        required: false,
        default: None,
        values: values.map(|values| values.into_iter().map(str::to_string).collect()),
        description: None,
    }
}

fn assert_coerced_rendered(
    name: &str,
    var: LaunchVarConfig,
    value: CanonicalLaunchVarValue,
    expected: &str,
) {
    let rendered = value.coerce_for_config(name, &var).unwrap().rendered();
    assert_eq!(rendered, expected);
}

fn assert_coercion_error(
    name: &str,
    var: LaunchVarConfig,
    value: CanonicalLaunchVarValue,
    expected: &str,
) {
    let err = value.coerce_for_config(name, &var).unwrap_err();
    assert!(
        err.to_string().contains(expected),
        "expected {expected:?} in {err}"
    );
}

#[test]
fn canonical_launch_var_values_coerce_render_and_convert_defaults_for_config_types() {
    assert_coerced_rendered(
        "repo",
        launch_var_config(LaunchVarType::String, None),
        CanonicalLaunchVarValue::String("https://example.test/repo.git".into()),
        "https://example.test/repo.git",
    );
    assert_coerced_rendered(
        "mode",
        launch_var_config(LaunchVarType::Enum, Some(vec!["fast", "safe"])),
        CanonicalLaunchVarValue::String("safe".into()),
        "safe",
    );
    assert_coerced_rendered(
        "debug",
        launch_var_config(LaunchVarType::Boolean, None),
        CanonicalLaunchVarValue::String("true".into()),
        "true",
    );
    assert_coerced_rendered(
        "debug",
        launch_var_config(LaunchVarType::Boolean, None),
        CanonicalLaunchVarValue::String("false".into()),
        "false",
    );
    assert_coerced_rendered(
        "debug",
        launch_var_config(LaunchVarType::Boolean, None),
        CanonicalLaunchVarValue::Boolean(true),
        "true",
    );
    assert_coerced_rendered(
        "count",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::String("2.0".into()),
        "2",
    );
    assert_coerced_rendered(
        "count",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::String("+2".into()),
        "2",
    );
    assert_coerced_rendered(
        "count",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::String("001".into()),
        "001",
    );
    assert_coerced_rendered(
        "count",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::Number("3".into()),
        "3",
    );
    assert_coerced_rendered(
        "ratio",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::Number("1.5".into()),
        "1.5",
    );
    assert_coerced_rendered(
        "limit",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::from_config_default(&LaunchVarValue::Float(2.0)),
        "2",
    );

    assert_coercion_error(
        "count",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::String("abc".into()),
        "invalid number launch variable \"count\"",
    );
    assert_coercion_error(
        "count",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::String("NaN".into()),
        "invalid number launch variable \"count\"; expected finite number",
    );
    assert_coercion_error(
        "count",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::String("inf".into()),
        "invalid number launch variable \"count\"; expected finite number",
    );
    assert_coercion_error(
        "count",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::String("-inf".into()),
        "invalid number launch variable \"count\"; expected finite number",
    );
    assert_coercion_error(
        "debug",
        launch_var_config(LaunchVarType::Boolean, None),
        CanonicalLaunchVarValue::String("yes".into()),
        "invalid boolean launch variable \"debug\"; expected true or false",
    );
    assert_coercion_error(
        "repo",
        launch_var_config(LaunchVarType::String, None),
        CanonicalLaunchVarValue::Boolean(true),
        "invalid string launch variable \"repo\"; expected string",
    );
    assert_coercion_error(
        "repo",
        launch_var_config(LaunchVarType::String, None),
        CanonicalLaunchVarValue::String("line\nbreak".into()),
        "invalid launch variable \"repo\"; must not contain NUL, LF, or CR",
    );
    assert_coercion_error(
        "repo",
        launch_var_config(LaunchVarType::String, None),
        CanonicalLaunchVarValue::String("bad\0value".into()),
        "invalid launch variable \"repo\"; must not contain NUL, LF, or CR",
    );
    assert_coercion_error(
        "mode",
        launch_var_config(LaunchVarType::Enum, Some(vec!["fast", "safe"])),
        CanonicalLaunchVarValue::Boolean(true),
        "invalid enum launch variable \"mode\"; expected string",
    );
    assert_coercion_error(
        "mode",
        launch_var_config(LaunchVarType::Enum, Some(vec!["fast", "safe"])),
        CanonicalLaunchVarValue::String("safe\r".into()),
        "invalid launch variable \"mode\"; must not contain NUL, LF, or CR",
    );
    assert_coercion_error(
        "count",
        launch_var_config(LaunchVarType::Number, None),
        CanonicalLaunchVarValue::Boolean(true),
        "invalid number launch variable \"count\"",
    );
}

#[test]
fn canonical_launch_var_json_constructors_preserve_http_scalar_rules() {
    assert_eq!(
        CanonicalLaunchVarValue::from_json_string("repo".into()),
        CanonicalLaunchVarValue::String("repo".into())
    );
    assert_eq!(
        CanonicalLaunchVarValue::from_json_bool(true),
        CanonicalLaunchVarValue::Boolean(true)
    );
    assert_eq!(
        CanonicalLaunchVarValue::from_json_number("count", Some(3), Some(3.0)).unwrap(),
        CanonicalLaunchVarValue::Number("3".into())
    );
    assert_eq!(
        CanonicalLaunchVarValue::from_json_number("ratio", None, Some(1.5)).unwrap(),
        CanonicalLaunchVarValue::Number("1.5".into())
    );

    let err = CanonicalLaunchVarValue::from_json_number("count", None, None).unwrap_err();
    assert_eq!(
        err,
        "invalid launch variable \"count\": number must be finite"
    );
    let err =
        CanonicalLaunchVarValue::from_json_number("count", None, Some(f64::INFINITY)).unwrap_err();
    assert_eq!(
        err,
        "invalid launch variable \"count\": number must be finite"
    );
}

#[test]
fn operation_error_display_preserves_messages_and_source() {
    for (err, expected) in [
        (
            OperationError::invalid_request("missing argument"),
            "missing argument",
        ),
        (
            OperationError::disabled_action("http action \"run\" is disabled"),
            "http action \"run\" is disabled",
        ),
        (
            OperationError::unknown_launch("unknown launch \"repo\""),
            "unknown launch \"repo\"",
        ),
        (
            OperationError::invalid_launch_variable("missing required launch variable \"repo\""),
            "missing required launch variable \"repo\"",
        ),
        (
            OperationError::invalid_session("invalid session id \"../bad\""),
            "invalid session id \"../bad\"",
        ),
    ] {
        assert_eq!(err.to_string(), expected);
        assert!(std::error::Error::source(&err).is_none());
    }

    let err = OperationError::operation_failed(anyhow::anyhow!("runtime failed"));
    assert_eq!(err.to_string(), "runtime failed");
    assert!(std::error::Error::source(&err).is_some());
}

#[test]
fn operation_failure_classifier_preserves_typed_runtime_failures() {
    let err = OperationError::operation_failed(anyhow::Error::new(
        crate::gateway::failures::AgentNotReady,
    ));
    assert!(matches!(err, OperationError::AgentNotReady { .. }));
    assert_eq!(err.to_string(), "container agent did not become ready");
    assert!(std::error::Error::source(&err).is_some());

    let err = OperationError::operation_failed(anyhow::Error::new(
        crate::gateway::failures::ContainerNotFound::after_start(),
    ));
    assert!(matches!(err, OperationError::ContainerNotFound { .. }));
    assert_eq!(err.to_string(), "container did not exist after start");

    let err = OperationError::operation_failed(anyhow::Error::new(
        crate::runtime::GatewayLabelError::Missing {
            key: "io.aw-gateway.target".into(),
        },
    ));
    assert!(matches!(err, OperationError::ContainerLabelMismatch { .. }));
    assert_eq!(
        err.to_string(),
        "container missing required label \"io.aw-gateway.target\""
    );
}

#[test]
fn operation_error_constructors_set_expected_variants() {
    assert!(matches!(
        OperationError::invalid_request("x"),
        OperationError::InvalidRequest { .. }
    ));
    assert!(matches!(
        OperationError::disabled_action("x"),
        OperationError::DisabledAction { .. }
    ));
    assert!(matches!(
        OperationError::unknown_launch("x"),
        OperationError::UnknownLaunch { .. }
    ));
    assert!(matches!(
        OperationError::invalid_launch_variable("x"),
        OperationError::InvalidLaunchVariable { .. }
    ));
    assert!(matches!(
        OperationError::invalid_session("x"),
        OperationError::InvalidSession { .. }
    ));
    assert!(matches!(
        OperationError::operation_failed(anyhow::anyhow!("x")),
        OperationError::OperationFailed { .. }
    ));
}

#[test]
fn launch_lookup_distinguishes_missing_launch_from_invalid_launch_config() {
    let cfg: GatewayConfig = toml::from_str(
        r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent]
use = ["missing-template"]
target = "default"
command = ["true"]
"#,
    )
    .unwrap();

    let err = lookup_launch(&cfg, "missing").unwrap_err();
    assert!(matches!(
        err,
        OperationError::UnknownLaunch { ref message }
            if message == "unknown launch \"missing\""
    ));

    let err = lookup_launch(&cfg, "agent").unwrap_err();
    assert!(matches!(err, OperationError::OperationFailed { .. }));
    assert!(
        err.to_string()
            .contains("launch \"agent\" uses launch template \"missing-template\""),
        "{err}"
    );
}

#[test]
fn constructs_targets_request_without_rendering_flags() {
    assert_eq!(
        GatewayOperation::from_targets_args(TargetsArgs { json: true }),
        GatewayOperation::Targets
    );
}

#[test]
fn constructs_status_and_status_all_requests_without_json() {
    assert_eq!(
        GatewayOperation::from_status_args(StatusArg {
            target: Some("dev".into()),
            all: false,
            json: true,
            session_id: Some("abc123".into()),
        }),
        GatewayOperation::Status {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
        }
    );
    assert_eq!(
        GatewayOperation::from_status_args(StatusArg {
            target: None,
            all: true,
            json: true,
            session_id: None,
        }),
        GatewayOperation::StatusAll
    );
}

#[test]
fn constructs_run_request() {
    let operation = GatewayOperation::from_run_args(RunArgs {
        target: Some("dev".into()),
        session_id: Some("abc123".into()),
        cwd: Some("/work".into()),
        command: vec!["cargo".into(), "test".into()],
    })
    .unwrap();
    assert_eq!(
        operation,
        GatewayOperation::Run {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
            cwd: Some("/work".into()),
            command: vec!["cargo".into(), "test".into()],
            options: OperationExecutionOptions::STREAM,
        }
    );
}

#[test]
fn constructs_launch_discovery_requests_without_json() {
    assert_eq!(
        GatewayOperation::from_launches_args(LaunchesArgs { json: true }),
        GatewayOperation::Launches
    );
    assert_eq!(
        GatewayOperation::from_launch_show_args(LaunchShowArgs {
            name: "repo-shell".into(),
            json: true,
        }),
        GatewayOperation::LaunchShow {
            name: "repo-shell".into(),
        }
    );
}

#[test]
fn constructs_launch_run_request() {
    assert_eq!(
        GatewayOperation::launch_run(
            "repo-shell".into(),
            Some("abc123".into()),
            SuppliedLaunchVars::from_cli_pairs(vec!["repo=https://example.test/repo.git".into()])
                .unwrap(),
            LaunchPassthroughArgs::default(),
        ),
        GatewayOperation::Launch {
            name: "repo-shell".into(),
            session_id: Some("abc123".into()),
            vars: SuppliedLaunchVars::from_cli_pairs(vec![
                "repo=https://example.test/repo.git".into()
            ])
            .unwrap(),
            args: LaunchPassthroughArgs::default(),
            options: OperationExecutionOptions::STREAM,
        }
    );
}

fn assert_ssh_operation_request(
    action: GatewayAction,
    operation: GatewayOperation,
    render: SshRenderOptions,
) {
    assert_eq!(
        SshGatewayOperation::from_action(&action).unwrap(),
        Some(SshGatewayOperation { operation, render })
    );
}

#[test]
fn constructs_operation_requests_from_ssh_actions() {
    assert_ssh_operation_request(
        GatewayAction::Up(Some("dev".into())),
        GatewayOperation::Up {
            target: Some("dev".into()),
            session_id: None,
        },
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::Run(RunAction {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
            cwd: Some("/work".into()),
            command: vec!["cargo".into(), "test".into()],
        }),
        GatewayOperation::from_run_args(RunArgs {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
            cwd: Some("/work".into()),
            command: vec!["cargo".into(), "test".into()],
        })
        .unwrap(),
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::LaunchRun {
            name: "repo-shell".into(),
            session_id: Some("abc123".into()),
            vars: vec!["repo=https://example.test/repo.git".into()],
            args: vec!["--skill".into(), "fresh-eyes".into()],
        },
        GatewayOperation::launch_run(
            "repo-shell".into(),
            Some("abc123".into()),
            SuppliedLaunchVars::from_cli_pairs(vec!["repo=https://example.test/repo.git".into()])
                .unwrap(),
            LaunchPassthroughArgs::from_strings(vec!["--skill".into(), "fresh-eyes".into()])
                .unwrap(),
        ),
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::Status(StatusAction {
            target: Some("dev".into()),
            all: false,
        }),
        GatewayOperation::from_status_args(StatusArg {
            target: Some("dev".into()),
            all: false,
            json: false,
            session_id: None,
        }),
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::Status(StatusAction {
            target: None,
            all: true,
        }),
        GatewayOperation::from_status_args(StatusArg {
            target: None,
            all: true,
            json: false,
            session_id: None,
        }),
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::Stop(crate::ssh_dispatch::TargetSessionAction {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
        }),
        GatewayOperation::Stop {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
        },
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::Remove(crate::ssh_dispatch::TargetSessionAction {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
        }),
        GatewayOperation::Remove {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
        },
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::SetDefault("fedora-dev".into()),
        GatewayOperation::SetDefault {
            target_or_image: "fedora-dev".into(),
        },
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::ShowDefault,
        GatewayOperation::ShowDefault,
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::ResetDefault,
        GatewayOperation::ResetDefault,
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::ClientConfig(ClientConfigAction {
            target: Some("dev".into()),
            identity_file: Some("/tmp/id".into()),
        }),
        GatewayOperation::ClientConfig {
            target: Some("dev".into()),
            identity_file: Some(PathBuf::from("/tmp/id")),
        },
        SshRenderOptions::default(),
    );
}

#[test]
fn preserves_ssh_render_options_for_metadata_actions() {
    for json in [false, true] {
        assert_ssh_operation_request(
            GatewayAction::Launches { json },
            GatewayOperation::from_launches_args(LaunchesArgs { json: false }),
            SshRenderOptions { json },
        );
        assert_ssh_operation_request(
            GatewayAction::Targets { json },
            GatewayOperation::from_targets_args(TargetsArgs { json: false }),
            SshRenderOptions { json },
        );
        assert_ssh_operation_request(
            GatewayAction::LaunchShow {
                name: "repo-shell".into(),
                json,
            },
            GatewayOperation::from_launch_show_args(LaunchShowArgs {
                name: "repo-shell".into(),
                json: false,
            }),
            SshRenderOptions { json },
        );
    }
}

#[test]
fn status_ssh_requests_ignore_transport_json_and_render_as_status_operations() {
    assert_ssh_operation_request(
        GatewayAction::Status(StatusAction {
            target: Some("dev".into()),
            all: false,
        }),
        GatewayOperation::Status {
            target: Some("dev".into()),
            session_id: None,
        },
        SshRenderOptions::default(),
    );
    assert_ssh_operation_request(
        GatewayAction::Status(StatusAction {
            target: None,
            all: true,
        }),
        GatewayOperation::StatusAll,
        SshRenderOptions::default(),
    );
}

#[test]
fn non_operation_ssh_actions_are_deferred() {
    for action in [
        GatewayAction::Connect(TargetSessionAction {
            target: Some("dev".into()),
            session_id: Some("abc123".into()),
        }),
        GatewayAction::AddKey(KeyAction {
            target: Some("dev".into()),
            public_key: Some("-".into()),
        }),
        GatewayAction::AddHostKey(KeySourceAction {
            public_key: Some("-".into()),
        }),
        GatewayAction::AddContainerKey(KeyAction {
            target: Some("dev".into()),
            public_key: Some("-".into()),
        }),
        GatewayAction::ClientBundle(ClientBundleAction {
            target: Some("dev".into()),
            identity_file: Some("/tmp/id".into()),
            rotate_key: true,
        }),
        GatewayAction::Help,
    ] {
        assert_eq!(SshGatewayOperation::from_action(&action).unwrap(), None);
    }
}

#[test]
fn ssh_launch_var_conversion_errors_remain_typed() {
    let err = SshGatewayOperation::from_action(&GatewayAction::LaunchRun {
        name: "repo-shell".into(),
        session_id: None,
        vars: vec!["repo=a".into(), "repo=b".into()],
        args: Vec::new(),
    })
    .unwrap_err();
    assert!(matches!(err, OperationError::InvalidLaunchVariable { .. }));
    assert_eq!(err.to_string(), "duplicate launch variable \"repo\"");
}

#[test]
fn launch_var_conversion_uses_existing_cli_pair_validation() {
    let err = SshGatewayOperation::from_action(&GatewayAction::LaunchRun {
        name: "repo-shell".into(),
        session_id: None,
        vars: vec!["repo".into()],
        args: Vec::new(),
    })
    .unwrap_err();
    assert!(matches!(err, OperationError::InvalidLaunchVariable { .. }));
    assert_eq!(err.to_string(), "--var must be key=value");
}
