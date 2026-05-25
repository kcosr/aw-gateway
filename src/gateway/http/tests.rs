use super::super::model::GatewayStatus;
use super::super::ops::{ExecutionOutcome, OperationError};
use super::auth::ActionAuthorizationError;
use super::response::success_data;
use super::*;
use crate::config::{HttpAuthConfig, HttpAuthType, HttpConfig};
use crate::paths::UserContext;
use axum::body::Body;
use axum::body::to_bytes;
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn write_fake_runtime(path: &std::path::Path, script: &str) {
    std::fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

fn fake_running_runtime_script(log: &std::path::Path) -> String {
    let user = UserContext::current().unwrap();
    format!(
        r#"#!/bin/sh
case "$1" in
  inspect)
    cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    ;;
  ps)
    cat <<'JSON'
[{{"Names":["ubuntu-dev"],"Image":"ubuntu/dev","State":"running","Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev","io.aw-gateway.image":"ubuntu/dev"}}}}]
JSON
    ;;
  exec)
    echo "$@" >> "{log}"
    echo "captured stdout"
    echo "captured stderr" >&2
    exit 23
    ;;
esac
exit 0
"#,
        user = user.user,
        uid = user.uid,
        log = log.display()
    )
}

fn http_operation_config(dir: &tempfile::TempDir, fake_runtime: &std::path::Path) -> PathBuf {
    let config = dir.path().join("gateway.toml");
    std::fs::write(
            &config,
            format!(
                r#"
schema_version = "1"

[http]
enabled = true
listen = "127.0.0.1:0"
enabled_actions = ["status", "targets", "launches", "run", "launch", "up"]

[runtime]
type = "podman"
program = "{program}"

[target_defaults.workspace]
path = "{workspace}"
state_dir = ".aw-gateway"
cleanup = "never"

[target_defaults.container_agent]
enabled = false

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "ubuntu-dev"
stop_when_idle = false

[launches.echo]
target = "default"
command = ["launch-command"]

[launches.typed]
target = "default"
cwd = "/repo-{{var.repo}}-{{var.count}}"
env = {{ REPO = "{{var.repo}}", DEBUG = "{{var.debug}}", COUNT = "{{var.count}}", MODE = "{{var.mode}}", RATIO = "{{var.ratio}}" }}
command = ["launch-command", "{{var.repo}}", "{{var.mode}}", "{{var.debug}}", "{{var.count}}", "{{var.ratio}}"]

[launches.typed.vars]
repo = {{ type = "string", required = true }}
mode = {{ type = "enum", values = ["fast", "safe"], default = "fast" }}
debug = {{ type = "boolean", default = false }}
count = {{ type = "number", default = 1 }}
ratio = {{ type = "number", default = 1 }}

[[launches.typed.steps]]
phase = "post_ready"
location = "container"
name = "prepare"
required = false
cwd = "/step-{{var.mode}}-{{var.count}}"
env = {{ STEP_REPO = "{{var.repo}}", STEP_DEBUG = "{{var.debug}}", STEP_RATIO = "{{var.ratio}}" }}
command = ["step-command", "{{var.repo}}", "{{var.count}}", "{{var.ratio}}"]
            "#,
                program = fake_runtime.display(),
                workspace = dir.path().join("workspace").display()
            ),
        )
        .unwrap();
    config
}

fn app_for_config(config: PathBuf) -> Router {
    let cfg = GatewayConfig::load(&config).unwrap();
    router(AppState {
        config_path: Some(config),
        config: Arc::new(cfg),
    })
}

async fn request_json(
    app: Router,
    method: &str,
    uri: &str,
    body: Body,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn response_json(response: Response) -> (StatusCode, serde_json::Value) {
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn post_json(app: Router, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    request_json(app, "POST", uri, Body::from(body.to_string())).await
}

async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
    request_json(app, "GET", uri, Body::empty()).await
}

fn test_state(http: HttpConfig) -> AppState {
    let mut cfg: GatewayConfig = toml::from_str(crate::gateway::DEFAULT_GATEWAY_CONFIG).unwrap();
    cfg.http = http;
    AppState {
        config_path: None,
        config: Arc::new(cfg),
    }
}

#[cfg(unix)]
fn set_private_file_mode(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(not(unix))]
fn set_private_file_mode(_path: &std::path::Path) {}

#[tokio::test]
async fn bearer_auth_allows_matching_token_and_rejects_missing_or_wrong_header() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, "secret-token\n").unwrap();
    set_private_file_mode(&token_file);
    let state = test_state(HttpConfig {
        enabled: true,
        listen: "127.0.0.1:0".into(),
        enabled_actions: vec!["status".into()],
        auth: HttpAuthConfig {
            auth_type: HttpAuthType::Bearer,
            token_file: Some(token_file.display().to_string()),
        },
    });

    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, "Bearer secret-token".parse().unwrap());
    authorize(&state, &headers).await.unwrap();

    let missing = authorize(&state, &HeaderMap::new()).await.unwrap_err();
    assert_eq!(missing.status, StatusCode::UNAUTHORIZED);
    assert_eq!(missing.code, ErrorCode::Unauthorized);

    let mut wrong_scheme = HeaderMap::new();
    wrong_scheme.insert(AUTHORIZATION, "Basic secret-token".parse().unwrap());
    assert_eq!(
        authorize(&state, &wrong_scheme).await.unwrap_err().code,
        ErrorCode::Unauthorized
    );

    let mut wrong_token = HeaderMap::new();
    wrong_token.insert(AUTHORIZATION, "Bearer other".parse().unwrap());
    assert_eq!(
        authorize(&state, &wrong_token).await.unwrap_err().code,
        ErrorCode::Unauthorized
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bearer_auth_rejects_group_or_world_readable_token_file() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, "secret-token\n").unwrap();
    let mut permissions = std::fs::metadata(&token_file).unwrap().permissions();
    permissions.set_mode(0o644);
    std::fs::set_permissions(&token_file, permissions).unwrap();
    let state = test_state(HttpConfig {
        enabled: true,
        listen: "127.0.0.1:0".into(),
        enabled_actions: vec!["status".into()],
        auth: HttpAuthConfig {
            auth_type: HttpAuthType::Bearer,
            token_file: Some(token_file.display().to_string()),
        },
    });

    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, "Bearer secret-token".parse().unwrap());
    let err = authorize(&state, &headers).await.unwrap_err();
    assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    assert_eq!(err.code, ErrorCode::Unauthorized);
}

#[tokio::test]
async fn none_auth_allows_missing_authorization() {
    let state = test_state(HttpConfig {
        enabled: true,
        listen: "127.0.0.1:0".into(),
        enabled_actions: vec!["status".into()],
        auth: HttpAuthConfig::default(),
    });
    authorize(&state, &HeaderMap::new()).await.unwrap();
}

#[test]
fn mode_and_output_parsing_accepts_wait_detach_and_rejects_bad_values() {
    let wait = operation_options(None, None).unwrap();
    assert_eq!(wait.mode, OperationMode::Wait);
    assert_eq!(wait.output, OutputSelection::BOTH);

    let output = vec!["stdout".to_string()];
    let detach = operation_options(Some("detach"), Some(&output)).unwrap();
    assert_eq!(detach.mode, OperationMode::Detach);
    assert_eq!(
        detach.output,
        OutputSelection {
            stdout: true,
            stderr: false,
        }
    );

    assert_eq!(
        operation_options(Some("stream"), None).unwrap_err().code,
        ErrorCode::InvalidMode
    );
    let duplicate = vec!["stdout".to_string(), "stdout".to_string()];
    assert_eq!(
        operation_options(Some("wait"), Some(&duplicate))
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );
    let unknown = vec!["log".to_string()];
    assert_eq!(
        operation_options(Some("wait"), Some(&unknown))
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );
    let empty: Vec<String> = Vec::new();
    assert_eq!(
        operation_options(Some("wait"), Some(&empty))
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );
}

#[test]
fn launch_vars_parse_typed_values_and_reject_duplicates_and_structured_values() {
    let parsed: LaunchRunRequest = serde_json::from_str(
        r#"{"vars":{"repo":"https://example.test/repo.git","debug":true,"count":3,"ratio":1.5}}"#,
    )
    .unwrap();
    let vars = parsed.vars.unwrap();
    assert!(matches!(
        vars.get("repo"),
        Some(CanonicalLaunchVarValue::String(value))
            if value == "https://example.test/repo.git"
    ));
    assert!(matches!(
        vars.get("debug"),
        Some(CanonicalLaunchVarValue::Boolean(true))
    ));
    assert!(matches!(
        vars.get("count"),
        Some(CanonicalLaunchVarValue::Number(value)) if value == "3"
    ));
    assert!(matches!(
        vars.get("ratio"),
        Some(CanonicalLaunchVarValue::Number(value)) if value == "1.5"
    ));

    let duplicate = serde_json::from_str::<LaunchRunRequest>(r#"{"vars":{"repo":"a","repo":"b"}}"#)
        .unwrap_err()
        .to_string();
    assert!(
        duplicate.contains("duplicate launch variable"),
        "{duplicate}"
    );

    let object = serde_json::from_str::<LaunchRunRequest>(r#"{"vars":{"repo":{"url":"x"}}}"#)
        .unwrap_err()
        .to_string();
    assert!(object.contains("invalid launch variable"), "{object}");
}

#[tokio::test]
async fn execution_response_projects_wait_detach_and_invalid_utf8() {
    let response = execution_response(ExecutionOutcome::captured(7, Some(b"out".to_vec()), None));
    assert_eq!(response.status(), StatusCode::OK);

    let response = execution_response(ExecutionOutcome::detached("abc123".into()));
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let object = body.as_object().unwrap();
    assert_eq!(object.len(), 4);
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "detach");
    assert_eq!(body["status"], "accepted");
    assert_eq!(body["operation_id"], "abc123");

    let response = execution_response(ExecutionOutcome::captured(0, Some(vec![0xff]), None));
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn operation_errors_map_launch_validation_to_invalid_launch_var() {
    let response = operation_error_response(OperationError::invalid_launch_variable(
        "missing required launch variable \"repo\"",
    ));
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_launch_var");

    let response = operation_error_response(OperationError::invalid_launch_variable(
        "unknown launch variable \"repo\"",
    ));
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_launch_var");
}

#[tokio::test]
async fn operation_errors_map_all_typed_variants_to_http_codes() {
    let cases = vec![
        (
            OperationError::invalid_request("bad request"),
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "bad request",
        ),
        (
            OperationError::disabled_action("http action \"run\" is disabled"),
            StatusCode::FORBIDDEN,
            "disabled_action",
            "http action \"run\" is disabled",
        ),
        (
            OperationError::unknown_launch("unknown launch \"repo\""),
            StatusCode::NOT_FOUND,
            "not_found",
            "unknown launch \"repo\"",
        ),
        (
            OperationError::invalid_launch_variable("invalid enum launch variable \"mode\""),
            StatusCode::BAD_REQUEST,
            "invalid_launch_var",
            "invalid enum launch variable \"mode\"",
        ),
        (
            OperationError::invalid_session("invalid session id \"../bad\""),
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid session id \"../bad\"",
        ),
        (
            OperationError::operation_failed(anyhow::anyhow!("runtime failed")),
            StatusCode::INTERNAL_SERVER_ERROR,
            "operation_failed",
            "runtime failed",
        ),
    ];

    for (err, expected_status, expected_code, expected_message) in cases {
        let response = operation_error_response(err);
        let (status, body) = response_json(response).await;
        assert_eq!(status, expected_status);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], expected_code);
        assert_eq!(body["error"]["message"], expected_message);
    }
}

#[tokio::test]
async fn operation_error_mapping_distinguishes_launch_var_from_unknown_launch() {
    let response = operation_error_response(OperationError::invalid_launch_variable(
        "unknown launch variable \"repo\"",
    ));
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_launch_var");

    let response =
        operation_error_response(OperationError::unknown_launch("unknown launch \"repo\""));
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn route_auth_and_disabled_action_return_json_errors() {
    let state = test_state(HttpConfig {
        enabled: true,
        listen: "127.0.0.1:0".into(),
        enabled_actions: vec!["targets".into()],
        auth: HttpAuthConfig::default(),
    });

    let response = authorize_action(&state, &HeaderMap::new(), "status")
        .await
        .unwrap_err();
    let ActionAuthorizationError::Operation(OperationError::DisabledAction { message }) = response
    else {
        panic!("expected disabled-action operation error");
    };
    assert_eq!(message, "http action \"status\" is disabled");

    let response = not_found(
        State(state),
        HeaderMap::new(),
        "/api/v1/missing".parse().unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn router_exposes_declared_routes_without_aliases() {
    let app = router(test_state(HttpConfig {
        enabled: true,
        listen: "127.0.0.1:0".into(),
        enabled_actions: vec!["targets".into()],
        auth: HttpAuthConfig::default(),
    }));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/statuses")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn run_wait_and_detach_routes_use_shared_operations() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["run-wait"],"mode":"wait","output":["stdout"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "wait");
    assert_eq!(body["exit_code"], 23);
    assert_eq!(body["stdout"], "captured stdout\n");
    assert!(body.get("stderr").is_none());

    let (status, body) = post_json(
        app,
        "/api/v1/run",
        r#"{"command":["run-detach"],"mode":"detach"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "detach");
    assert_eq!(body["status"], "accepted");
    assert!(body["operation_id"].as_str().is_some());
}

#[tokio::test]
async fn launch_wait_and_detach_routes_use_shared_operations() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/launches/echo/run",
        r#"{"mode":"wait","output":["stderr"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "wait");
    assert_eq!(body["exit_code"], 23);
    assert!(body.get("stdout").is_none());
    assert_eq!(body["stderr"], "captured stderr\n");

    let (status, body) = post_json(app, "/api/v1/launches/echo/run", r#"{"mode":"detach"}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "detach");
    assert_eq!(body["status"], "accepted");
    assert!(body["operation_id"].as_str().is_some());
}

#[tokio::test]
async fn launch_route_renders_typed_json_vars_into_steps_and_final_exec() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
            app.clone(),
            "/api/v1/launches/typed/run",
            r#"{"vars":{"repo":"alpha","mode":"safe","debug":true,"count":3,"ratio":1.5},"mode":"wait"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);

    let log = std::fs::read_to_string(&log).unwrap();
    assert!(log.contains("--workdir /step-safe-3"), "{log}");
    assert!(log.contains("--env STEP_DEBUG=true"), "{log}");
    assert!(log.contains("--env STEP_REPO=alpha"), "{log}");
    assert!(log.contains("--env STEP_RATIO=1.5"), "{log}");
    assert!(log.contains("ubuntu-dev step-command alpha 3 1.5"), "{log}");
    assert!(log.contains("--workdir /repo-alpha-3"), "{log}");
    assert!(log.contains("--env COUNT=3"), "{log}");
    assert!(log.contains("--env DEBUG=true"), "{log}");
    assert!(log.contains("--env MODE=safe"), "{log}");
    assert!(log.contains("--env RATIO=1.5"), "{log}");
    assert!(log.contains("--env REPO=alpha"), "{log}");
    assert!(
        log.contains("ubuntu-dev launch-command alpha safe true 3 1.5"),
        "{log}"
    );

    let (status, body) = post_json(
        app,
        "/api/v1/launches/typed/run",
        r#"{"vars":{"repo":"alpha","mode":"bad","debug":true,"count":3}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_launch_var");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid enum launch variable \"mode\""),
        "{body}"
    );
}

#[tokio::test]
async fn metadata_routes_return_data_envelopes() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = get_json(app.clone(), "/api/v1/status?target=default").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["target"], "default");

    let (status, body) = get_json(app.clone(), "/api/v1/status/all").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"][0]["target"], "default");

    let (status, body) = get_json(app.clone(), "/api/v1/targets").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"][0]["target"], "default");

    let (status, body) = post_json(app.clone(), "/api/v1/up", r#"{"target":"default"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["target"], "default");

    let (status, body) = get_json(app.clone(), "/api/v1/launches").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"][0]["name"], "echo");

    let (status, body) = get_json(app, "/api/v1/launches/echo").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["name"], "echo");
    assert_eq!(body["data"]["command"][0], "launch-command");
}

#[tokio::test]
async fn unknown_launch_route_returns_not_found_json_error() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = get_json(app, "/api/v1/launches/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], "not_found");
    assert_eq!(body["error"]["message"], "unknown launch \"missing\"");
}

#[tokio::test]
async fn malformed_json_mode_output_and_launch_var_errors_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(app.clone(), "/api/v1/run", r#"{"command":["x"]"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["x"],"mode":"stream"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_mode");

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["x"],"output":["stdout","stdout"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_output");

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["x"],"output":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_output");

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/launches/echo/run",
        r#"{"vars":{"bogus":1}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_launch_var");

    let (status, body) = post_json(
        app,
        "/api/v1/launches/echo/run",
        r#"{"vars":{"repo":null}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_launch_var");
}

#[tokio::test]
async fn status_shape_can_be_wrapped_in_data_envelope() {
    let status = GatewayStatus {
        target: "default".into(),
        session_id: None,
        launch: None,
        mode: "fixed".into(),
        user: "alice".into(),
        image: "ubuntu/dev".into(),
        container: Some("ubuntu-dev".into()),
        container_pid: Some(123),
        active_sessions: 0,
        sessions: Vec::new(),
        agent_ready: true,
        ssh_socket: PathBuf::from("/tmp/ssh.sock"),
        status: "ready".into(),
        agent: Some(Box::new(crate::agent_control::AgentStatus {
            ready: true,
            version: "0.2.0".into(),
            services: Vec::new(),
            ssh_bridge: crate::agent_control::BridgeStatus {
                enabled: true,
                ready: true,
                active_streams: 0,
                active_sessions: 0,
            },
            idle_cleanup: Some(crate::agent_control::IdleCleanupStatus {
                owner: "agent".into(),
                action: "exit_container".into(),
                state: crate::agent_control::IdleStateName::IdlePending,
                idle_for_ms: None,
                preserve: false,
                preserve_reason: None,
                matched_processes: Vec::new(),
                last_reap_result: None,
            }),
            shutting_down: false,
        })),
    };
    let response = success_data(status);
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["agent"]["ready"], true);
    assert_eq!(body["data"]["agent"]["ssh_bridge"]["enabled"], true);
    assert_eq!(body["data"]["agent"]["ssh_bridge"]["ready"], true);
    assert_eq!(
        body["data"]["agent"]["idle_cleanup"]["state"],
        "idle_pending"
    );
}
