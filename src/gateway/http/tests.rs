use super::super::model::GatewayStatus;
use super::super::ops::{ExecutionOutcome, OperationError};
use super::auth::ActionAuthorizationError;
use super::output_projection::{OutputFormat, OutputFormats};
use super::response::success_data;
use super::*;
use crate::config::{HttpAuthConfig, HttpAuthType, HttpConfig};
use crate::paths::UserContext;
use axum::body::Body;
use axum::body::to_bytes;
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use std::collections::BTreeMap;
use std::io::Write;
use std::net::{Shutdown, TcpStream};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use tokio::time::{Duration, sleep};
use tokio_tungstenite::tungstenite::Message as WsMessage;
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
    if [ -f "{stopped}" ]; then
      cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":false,"Pid":0}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    else
      cat <<'JSON'
[{{"Id":"id","Name":"ubuntu-dev","State":{{"Running":true,"Pid":123}},"Config":{{"Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev"}}}}}}]
JSON
    fi
    ;;
  ps)
    cat <<'JSON'
[{{"Names":["ubuntu-dev"],"Image":"ubuntu/dev","State":"running","Labels":{{"io.aw-gateway.gateway":"true","io.aw-gateway.user":"{user}","io.aw-gateway.uid":"{uid}","io.aw-gateway.target":"default","io.aw-gateway.container_id":"ubuntu-dev","io.aw-gateway.image":"ubuntu/dev"}}}}]
JSON
    ;;
  exec)
    case "$*" in
      *aw-gateway-marker-list*|*aw-gateway-marker-sweep*)
        exit 0
        ;;
    esac
    echo "$@" >> "{log}"
    case "$*" in
      *invalid-json-output*)
        printf 'not-json'
        printf 'stderr-note' >&2
        exit 7
        ;;
      *json-output*)
        printf '{{"ok":true,"nested":{{"value":42}},"items":["a","b"]}}\n'
        exit 0
        ;;
    esac
    echo "captured stdout"
    echo "captured stderr" >&2
    exit 23
    ;;
  stop)
    echo "$@" >> "{log}"
    touch "{stopped}"
    ;;
  rm)
    echo "$@" >> "{log}"
    ;;
esac
exit 0
"#,
        user = user.user,
        uid = user.uid,
        log = log.display(),
        stopped = log.with_extension("stopped").display()
    )
}

fn fake_long_running_pty_runtime_script(
    log: &std::path::Path,
    parent_pid: &std::path::Path,
    child_pid: &std::path::Path,
    child_done: &std::path::Path,
) -> String {
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
    case "$*" in
      *aw-gateway-marker-list*|*aw-gateway-marker-sweep*)
        exit 0
        ;;
    esac
    echo "$@" >> "{log}"
    case "$*" in
      *aw-gateway-pty-cleanup*)
        pid="$(cat "{child_pid}" 2>/dev/null || true)"
        if [ -n "$pid" ]; then
          kill -KILL "$pid" 2>/dev/null || true
        fi
        echo done > "{child_done}"
        exit 0
        ;;
    esac
    echo "$$" > "{parent_pid}"
    exec sh -c 'trap "" HUP TERM; trap "echo done > \"$2\"; exit 0" INT; echo "$$" > "$1"; while :; do sleep 1; done' sh "{child_pid}" "{child_done}"
    ;;
esac
exit 0
"#,
        user = user.user,
        uid = user.uid,
        log = log.display(),
        parent_pid = parent_pid.display(),
        child_pid = child_pid.display(),
        child_done = child_done.display(),
    )
}

fn fake_long_running_wait_runtime_script(
    log: &std::path::Path,
    parent_pid: &std::path::Path,
    child_pid: &std::path::Path,
    cleanup_done: &std::path::Path,
) -> String {
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
    case "$*" in
      *aw-gateway-marker-list*|*aw-gateway-marker-sweep*)
        exit 0
        ;;
    esac
    echo "$@" >> "{log}"
    case "$*" in
      *aw-gateway-exec-cleanup*)
        pid="$(cat "{child_pid}" 2>/dev/null || true)"
        if [ -n "$pid" ]; then
          kill -KILL "$pid" 2>/dev/null || true
        fi
        echo done > "{cleanup_done}"
        exit 0
        ;;
      *aw-gateway-exec-rm*)
        exit 0
        ;;
    esac
    echo "$$" > "{parent_pid}"
    sh -c 'trap "" HUP TERM; echo "$$" > "$1"; while :; do sleep 1; done' sh "{child_pid}" &
    wait "$!"
    ;;
esac
exit 0
"#,
        user = user.user,
        uid = user.uid,
        log = log.display(),
        parent_pid = parent_pid.display(),
        child_pid = child_pid.display(),
        cleanup_done = cleanup_done.display(),
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
enabled_actions = ["status", "targets", "launches", "run", "launch-show", "launch", "up", "stop", "remove"]

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

[launches.args]
target = "default"
allow_args = true
command = ["launch-command", "before", "{{args}}", "after"]

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
        pty_leases: Arc::new(PtyLeaseManager::default()),
        pty_shutdown: PtyShutdown::default(),
        wait_shutdown: WaitShutdown::default(),
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

async fn serve_live_app(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("ws://{addr}"), handle)
}

async fn serve_live_http_app(app: Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, handle)
}

async fn wait_for_path(path: &std::path::Path) {
    for _ in 0..50 {
        if path.exists() {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {}", path.display());
}

fn read_pid(path: &std::path::Path) -> libc::pid_t {
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn process_is_alive(pid: libc::pid_t) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[derive(Clone)]
struct DropProbe {
    events: tokio::sync::mpsc::UnboundedSender<&'static str>,
}

struct DropProbeGuard {
    events: tokio::sync::mpsc::UnboundedSender<&'static str>,
}

impl Drop for DropProbeGuard {
    fn drop(&mut self) {
        let _ = self.events.send("dropped");
    }
}

async fn phase0_disconnect_probe(State(probe): State<DropProbe>, _body: Bytes) -> Response {
    let _guard = DropProbeGuard {
        events: probe.events.clone(),
    };
    let _ = probe.events.send("entered");
    std::future::pending::<Response>().await
}

async fn serve_drop_probe() -> (
    std::net::SocketAddr,
    tokio::sync::mpsc::UnboundedReceiver<&'static str>,
    tokio::task::JoinHandle<()>,
) {
    let (events, rx) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new()
        .route("/probe", post(phase0_disconnect_probe))
        .with_state(DropProbe { events });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, rx, handle)
}

fn write_probe_request(stream: &mut TcpStream) {
    stream
        .write_all(
            b"POST /probe HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        )
        .unwrap();
    stream.flush().unwrap();
}

#[cfg(unix)]
fn enable_abortive_close(stream: &TcpStream) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    let result = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const libc::linger as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(result, 0, "setsockopt SO_LINGER failed");
}

async fn assert_probe_drop_after_disconnect(abortive: bool) {
    let (addr, mut events, server) = serve_drop_probe().await;
    let mut stream = TcpStream::connect(addr).unwrap();
    write_probe_request(&mut stream);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap(),
        Some("entered")
    );

    if abortive {
        #[cfg(unix)]
        enable_abortive_close(&stream);
    } else {
        stream.shutdown(Shutdown::Both).unwrap();
    }
    drop(stream);

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap(),
        Some("dropped")
    );
    server.abort();
}

async fn wait_for_process_exit(pid: libc::pid_t, marker: &std::path::Path) {
    for _ in 0..100 {
        if marker.exists() || !process_is_alive(pid) {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    panic!("process {pid} remained alive after websocket close");
}

fn test_state(http: HttpConfig) -> AppState {
    let mut cfg: GatewayConfig = toml::from_str(crate::gateway::DEFAULT_GATEWAY_CONFIG).unwrap();
    cfg.http = http;
    AppState {
        config_path: None,
        config: Arc::new(cfg),
        pty_leases: Arc::new(PtyLeaseManager::default()),
        pty_shutdown: PtyShutdown::default(),
        wait_shutdown: WaitShutdown::default(),
    }
}

#[tokio::test]
async fn real_tcp_handler_future_drops_on_fin_disconnect() {
    assert_probe_drop_after_disconnect(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn real_tcp_handler_future_drops_on_rst_disconnect() {
    assert_probe_drop_after_disconnect(true).await;
}

#[tokio::test]
async fn bearer_auth_allows_matching_token_and_rejects_missing_or_wrong_header() {
    let state = test_state(HttpConfig {
        enabled: true,
        listen: "127.0.0.1:0".into(),
        enabled_actions: vec!["status".into()],
        auth: HttpAuthConfig {
            auth_type: HttpAuthType::Bearer,
            token: Some("secret-token".into()),
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
    let wait = execution_request_options(None, None, None).unwrap();
    assert_eq!(wait.options.mode, OperationMode::Wait);
    assert_eq!(wait.options.output, OutputSelection::BOTH);
    assert_eq!(wait.output_formats, OutputFormats::TEXT);

    let output = vec!["stdout".to_string()];
    let wait_stdout = execution_request_options(Some("wait"), Some(&output), None).unwrap();
    assert_eq!(
        wait_stdout.options.output,
        OutputSelection {
            stdout: true,
            stderr: false,
        }
    );

    let detach = execution_request_options(Some("detach"), None, None).unwrap();
    assert_eq!(detach.options.mode, OperationMode::Detach);

    assert_eq!(
        execution_request_options(Some("stream"), None, None)
            .unwrap_err()
            .code,
        ErrorCode::InvalidMode
    );
    let duplicate = vec!["stdout".to_string(), "stdout".to_string()];
    assert_eq!(
        execution_request_options(Some("wait"), Some(&duplicate), None)
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );
    let unknown = vec!["log".to_string()];
    assert_eq!(
        execution_request_options(Some("wait"), Some(&unknown), None)
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );
    let empty: Vec<String> = Vec::new();
    assert_eq!(
        execution_request_options(Some("wait"), Some(&empty), None)
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );
    assert_eq!(
        execution_request_options(Some("detach"), Some(&output), None)
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );
}

#[test]
fn output_format_validation_uses_effective_output_selection() {
    let mut format = BTreeMap::new();
    format.insert("stdout".to_string(), "json".to_string());
    let default_output = execution_request_options(Some("wait"), None, Some(&format)).unwrap();
    assert_eq!(default_output.output_formats.stdout, OutputFormat::Json);

    let output = vec!["stderr".to_string()];
    assert_eq!(
        execution_request_options(Some("wait"), Some(&output), Some(&format))
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );

    format.clear();
    format.insert("log".to_string(), "json".to_string());
    assert_eq!(
        execution_request_options(Some("wait"), None, Some(&format))
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );

    format.clear();
    format.insert("stdout".to_string(), "yaml".to_string());
    assert_eq!(
        execution_request_options(Some("wait"), None, Some(&format))
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );

    assert_eq!(
        execution_request_options(Some("detach"), None, Some(&format))
            .unwrap_err()
            .code,
        ErrorCode::InvalidOutput
    );
}

#[test]
fn status_query_accepts_flattened_context_parameters() {
    let query = parse_status_query(Some(
        "target=default&session_id=s1&context.tenant=acme&context.workspace=web",
    ))
    .unwrap();

    assert_eq!(query.target.as_deref(), Some("default"));
    assert_eq!(query.session_id.as_deref(), Some("s1"));
    assert_eq!(
        query.context.as_map(),
        &BTreeMap::from([
            ("tenant".into(), "acme".into()),
            ("workspace".into(), "web".into()),
        ])
    );
}

#[test]
fn status_query_rejects_nested_context_encoding() {
    let err = parse_status_query(Some("context%5Btenant%5D=acme")).unwrap_err();

    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(err.message.contains("unknown status query parameter"));
}

#[test]
fn status_query_rejects_duplicate_context_parameters() {
    let err = parse_status_query(Some("context.tenant=acme&context.tenant=other")).unwrap_err();

    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(err.message.contains("duplicate context key"));
}

#[test]
fn http_body_context_rejects_duplicate_keys() {
    let err = parse_body::<RunRequest>(
        br#"{"command":["x"],"context":{"tenant":"acme","tenant":"other"}}"#,
        ErrorCode::InvalidRequest,
    )
    .unwrap_err();

    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(err.message.contains("duplicate context key"));
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

#[test]
fn launch_args_parse_string_arrays_and_reject_invalid_values() {
    let parsed: LaunchRunRequest =
        serde_json::from_str(r#"{"args":["--skill","fresh-eyes","review this"]}"#).unwrap();
    assert_eq!(
        parsed.args.unwrap().as_slice(),
        ["--skill", "fresh-eyes", "review this"]
    );

    let number = serde_json::from_str::<LaunchRunRequest>(r#"{"args":[1]}"#)
        .unwrap_err()
        .to_string();
    assert!(number.contains("invalid launch args"), "{number}");

    let empty = serde_json::from_str::<LaunchRunRequest>(r#"{"args":[""]}"#)
        .unwrap_err()
        .to_string();
    assert!(
        empty.contains("launch args must not contain empty strings"),
        "{empty}"
    );
}

#[tokio::test]
async fn execution_response_projects_wait_detach_and_invalid_utf8() {
    let response = execution_response(
        ExecutionOutcome::captured(7, Some(b"out".to_vec()), None),
        OutputFormats::TEXT,
    );
    assert_eq!(response.status(), StatusCode::OK);

    let response = execution_response(
        ExecutionOutcome::detached("abc123".into()),
        OutputFormats::TEXT,
    );
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let object = body.as_object().unwrap();
    assert_eq!(object.len(), 4);
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "detach");
    assert_eq!(body["status"], "accepted");
    assert_eq!(body["operation_id"], "abc123");

    let response = execution_response(
        ExecutionOutcome::captured(0, Some(vec![0xff]), None),
        OutputFormats::TEXT,
    );
    let (status, body) = response_json(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "wait");
    assert_eq!(body["exit_code"], 0);
    assert!(body.get("stdout").is_none());
    assert_eq!(body["output_errors"]["stdout"]["format"], "text");
    assert_eq!(body["output_errors"]["stdout"]["code"], "invalid_utf8");
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
            "internal operation failed",
        ),
        (
            OperationError::operation_failed(anyhow::Error::new(
                crate::gateway::failures::AgentNotReady,
            )),
            StatusCode::INTERNAL_SERVER_ERROR,
            "operation_failed",
            "internal operation failed",
        ),
        (
            OperationError::operation_failed(anyhow::Error::new(
                crate::gateway::failures::ContainerNotFound::after_start(),
            )),
            StatusCode::INTERNAL_SERVER_ERROR,
            "operation_failed",
            "internal operation failed",
        ),
        (
            OperationError::operation_failed(anyhow::Error::new(
                crate::runtime::GatewayLabelError::Missing {
                    key: "io.aw-gateway.target".into(),
                },
            )),
            StatusCode::INTERNAL_SERVER_ERROR,
            "operation_failed",
            "internal operation failed",
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
async fn launch_show_and_run_use_separate_http_actions() {
    let state = test_state(HttpConfig {
        enabled: true,
        listen: "127.0.0.1:0".into(),
        enabled_actions: vec!["launch-show".into()],
        auth: HttpAuthConfig::default(),
    });

    authorize_action(&state, &HeaderMap::new(), "launch-show")
        .await
        .unwrap();
    let response = authorize_action(&state, &HeaderMap::new(), "launch")
        .await
        .unwrap_err();
    let ActionAuthorizationError::Operation(OperationError::DisabledAction { message }) = response
    else {
        panic!("expected disabled-action operation error");
    };
    assert_eq!(message, "http action \"launch\" is disabled");

    let state = test_state(HttpConfig {
        enabled: true,
        listen: "127.0.0.1:0".into(),
        enabled_actions: vec!["launch".into()],
        auth: HttpAuthConfig::default(),
    });

    authorize_action(&state, &HeaderMap::new(), "launch")
        .await
        .unwrap();
    let response = authorize_action(&state, &HeaderMap::new(), "launch-show")
        .await
        .unwrap_err();
    let ActionAuthorizationError::Operation(OperationError::DisabledAction { message }) = response
    else {
        panic!("expected disabled-action operation error");
    };
    assert_eq!(message, "http action \"launch-show\" is disabled");
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
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/remove")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
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
        app.clone(),
        "/api/v1/run",
        r#"{"command":["run-detach"],"mode":"detach"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "detach");
    assert_eq!(body["status"], "accepted");
    assert!(body["operation_id"].as_str().is_some());

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["run-detach"],"mode":"detach","output":["stdout"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_output");

    let (status, body) = post_json(
        app,
        "/api/v1/run",
        r#"{"command":["run-wait"],"output_format":{"stdout":5}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn run_wait_disconnect_cancels_operation_and_runs_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    let parent_pid = dir.path().join("parent.pid");
    let child_pid = dir.path().join("child.pid");
    let cleanup_done = dir.path().join("cleanup.done");
    write_fake_runtime(
        &fake_runtime,
        &fake_long_running_wait_runtime_script(&log, &parent_pid, &child_pid, &cleanup_done),
    );
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));
    let (addr, server) = serve_live_http_app(app).await;

    let mut stream = TcpStream::connect(addr).unwrap();
    let body = r#"{"command":["long-running"],"mode":"wait"}"#;
    write!(
        stream,
        "POST /api/v1/run HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body,
    )
    .unwrap();
    stream.flush().unwrap();

    wait_for_path(&parent_pid).await;
    drop(stream);
    wait_for_path(&cleanup_done).await;

    let log = std::fs::read_to_string(log).unwrap();
    assert!(log.contains("aw-gateway-exec-cleanup"), "{log}");
    server.abort();
}

#[tokio::test]
async fn run_wait_completion_preserves_status_codes_after_spawn() {
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
    assert_eq!(body["exit_code"], 23);

    let (status, body) = post_json(app, "/api/v1/launches/missing/run", r#"{"mode":"wait"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn wait_shutdown_cancels_active_waits_after_grace() {
    let shutdown = WaitShutdown::default();
    let (token, active) = shutdown.register();

    let shutdown_task = tokio::spawn(async move {
        shutdown
            .cancel_active_after_grace(Duration::from_millis(10), Duration::from_millis(500))
            .await;
    });

    tokio::time::timeout(Duration::from_secs(1), token.cancelled())
        .await
        .expect("shutdown did not cancel active wait token");
    drop(active);
    tokio::time::timeout(Duration::from_secs(1), shutdown_task)
        .await
        .expect("shutdown wait did not finish")
        .expect("shutdown task failed");
}

#[tokio::test]
async fn run_wait_can_project_stdout_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app,
        "/api/v1/run",
        r#"{"command":["json-output"],"mode":"wait","output":["stdout"],"output_format":{"stdout":"json"}}"#,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "wait");
    assert_eq!(body["exit_code"], 0);
    assert_eq!(body["stdout_json"]["ok"], true);
    assert_eq!(body["stdout_json"]["nested"]["value"], 42);
    assert_eq!(body["stdout_json"]["items"], serde_json::json!(["a", "b"]));
    assert!(body.get("stdout").is_none());
    assert!(body.get("output_errors").is_none());
}

#[tokio::test]
async fn run_wait_json_projection_failure_preserves_raw_output() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app,
        "/api/v1/run",
        r#"{"command":["invalid-json-output"],"mode":"wait","output":["stdout","stderr"],"output_format":{"stdout":"json"}}"#,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "wait");
    assert_eq!(body["exit_code"], 7);
    assert_eq!(body["stdout"], "not-json");
    assert_eq!(body["stderr"], "stderr-note");
    assert!(body.get("stdout_json").is_none());
    assert_eq!(body["output_errors"]["stdout"]["format"], "json");
    assert_eq!(body["output_errors"]["stdout"]["code"], "invalid_json");
}

#[tokio::test]
async fn stop_and_remove_routes_use_shared_operations() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(app.clone(), "/api/v1/stop", r#"{"target":"default"}"#).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["container"], "ubuntu-dev");
    assert_eq!(body["data"]["stopped"], true);

    let (status, body) = post_json(app, "/api/v1/remove", r#"{"target":"default"}"#).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["container"], "ubuntu-dev");
    assert_eq!(body["data"]["removed"], true);

    let log = std::fs::read_to_string(log).unwrap();
    assert!(log.contains("ubuntu-dev"), "{log}");
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
async fn run_pty_creates_attach_lease_without_running_final_exec() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app,
        "/api/v1/run",
        r#"{"command":["bash","-lc","exec bash"],"mode":"pty","terminal":{"cols":120,"rows":34,"cell_width_px":9,"cell_height_px":18}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "pty");
    assert_eq!(body["status"], "prepared");
    assert!(body["pty_id"].as_str().unwrap().starts_with("pty_"));
    assert!(body["attach_token"].as_str().unwrap().starts_with("awpt_"));
    assert_eq!(
        body["attach_url"].as_str().unwrap(),
        format!("/api/v1/pty/{}", body["pty_id"].as_str().unwrap())
    );
    assert!(
        !log.exists(),
        "final exec should not run before websocket attach"
    );
}

#[tokio::test]
async fn run_pty_uses_server_config_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let config = http_operation_config(&dir, &fake_runtime);
    let app = app_for_config(config.clone());
    std::fs::write(&config, "this is not valid toml").unwrap();

    let (status, body) = post_json(
        app,
        "/api/v1/run",
        r#"{"command":["bash"],"mode":"pty","terminal":{"cols":80,"rows":24}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["mode"], "pty");
}

#[tokio::test]
async fn launch_pty_creates_attach_lease() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app,
        "/api/v1/launches/echo/run",
        r#"{"mode":"pty","terminal":{"cols":100,"rows":30}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["mode"], "pty");
    assert!(body["pty_id"].as_str().unwrap().starts_with("pty_"));
    assert!(body["attach_token"].as_str().unwrap().starts_with("awpt_"));
}

#[tokio::test]
async fn launch_detail_reports_ephemeral_target_mode_without_container() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    write_fake_runtime(
        &fake_runtime,
        &fake_running_runtime_script(&dir.path().join("runtime.log")),
    );
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[http]
enabled = true
listen = "127.0.0.1:0"
enabled_actions = ["launches", "launch-show"]

[runtime]
type = "podman"
program = "{program}"

[target_defaults.container_agent]
enabled = false

[targets.default]
image = "ubuntu/dev"
mode = "ephemeral"
ephemeral_name = "worker-{{session_id}}"
stop_when_idle = true

[launches.ephemeral]
target = "default"
command = ["launch-command"]
"#,
            program = fake_runtime.display(),
        ),
    )
    .unwrap();
    let app = app_for_config(config);

    let (status, body) = get_json(app, "/api/v1/launches/ephemeral").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["target_mode"], "ephemeral");
    assert!(body["data"].get("target_container").is_none());
}

#[tokio::test]
async fn pty_mode_rejects_output_options_and_requires_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["x"],"mode":"pty","output":["stdout"],"terminal":{"cols":80,"rows":24}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_output");

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["x"],"mode":"pty","output_format":{"stdout":"json"},"terminal":{"cols":80,"rows":24}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_output");

    let (status, body) = post_json(app, "/api/v1/run", r#"{"command":["x"],"mode":"pty"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn websocket_attach_authenticates_starts_pty_and_streams_output() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["printf","hello"],"mode":"pty","terminal":{"cols":80,"rows":24}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pty_id = body["pty_id"].as_str().unwrap().to_string();
    let attach_token = body["attach_token"].as_str().unwrap().to_string();

    let (base, server) = serve_live_app(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("{base}/api/v1/pty/{pty_id}"))
        .await
        .unwrap();
    ws.send(WsMessage::Text(
        serde_json::json!({"type":"auth","token":attach_token})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    let ready = ws.next().await.unwrap().unwrap();
    let WsMessage::Text(ready) = ready else {
        panic!("expected ready text frame, got {ready:?}");
    };
    let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["target"], "default");
    assert_eq!(ready["target_mode"], "fixed");

    let mut output = Vec::new();
    let mut exit = None;
    while let Some(message) = ws.next().await {
        match message.unwrap() {
            WsMessage::Binary(bytes) => output.extend_from_slice(&bytes),
            WsMessage::Text(text) => {
                let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                if value["type"] == "exit" {
                    exit = Some(value);
                    break;
                }
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
    let output = String::from_utf8_lossy(&output);
    assert!(output.contains("captured stdout"), "{output}");
    assert!(output.contains("captured stderr"), "{output}");
    assert_eq!(exit.unwrap()["exit_code"], 23);

    server.abort();
}

#[tokio::test]
async fn websocket_close_control_terminates_running_pty_exec() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    let parent_pid = dir.path().join("parent.pid");
    let child_pid = dir.path().join("child.pid");
    let child_done = dir.path().join("child.done");
    write_fake_runtime(
        &fake_runtime,
        &fake_long_running_pty_runtime_script(&log, &parent_pid, &child_pid, &child_done),
    );
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["bash"],"mode":"pty","terminal":{"cols":80,"rows":24}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pty_id = body["pty_id"].as_str().unwrap().to_string();
    let attach_token = body["attach_token"].as_str().unwrap().to_string();

    let (base, server) = serve_live_app(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("{base}/api/v1/pty/{pty_id}"))
        .await
        .unwrap();
    ws.send(WsMessage::Text(
        serde_json::json!({"type":"auth","token":attach_token})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    let ready = ws.next().await.unwrap().unwrap();
    assert!(matches!(ready, WsMessage::Text(_)));
    wait_for_path(&child_pid).await;
    let child_pid = read_pid(&child_pid);
    assert!(process_is_alive(child_pid));

    ws.send(WsMessage::Text(
        serde_json::json!({"type":"close"}).to_string().into(),
    ))
    .await
    .unwrap();
    drop(ws);

    wait_for_process_exit(child_pid, &child_done).await;
    server.abort();
}

#[tokio::test]
async fn websocket_disconnect_terminates_running_pty_exec() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    let parent_pid = dir.path().join("parent.pid");
    let child_pid = dir.path().join("child.pid");
    let child_done = dir.path().join("child.done");
    write_fake_runtime(
        &fake_runtime,
        &fake_long_running_pty_runtime_script(&log, &parent_pid, &child_pid, &child_done),
    );
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["bash"],"mode":"pty","terminal":{"cols":80,"rows":24}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pty_id = body["pty_id"].as_str().unwrap().to_string();
    let attach_token = body["attach_token"].as_str().unwrap().to_string();

    let (base, server) = serve_live_app(app).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("{base}/api/v1/pty/{pty_id}"))
        .await
        .unwrap();
    ws.send(WsMessage::Text(
        serde_json::json!({"type":"auth","token":attach_token})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    let ready = ws.next().await.unwrap().unwrap();
    assert!(matches!(ready, WsMessage::Text(_)));
    wait_for_path(&child_pid).await;
    let child_pid = read_pid(&child_pid);
    assert!(process_is_alive(child_pid));

    drop(ws);

    wait_for_process_exit(child_pid, &child_done).await;
    server.abort();
}

#[tokio::test]
async fn websocket_failed_auth_does_not_consume_lease() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app.clone(),
        "/api/v1/run",
        r#"{"command":["printf","hello"],"mode":"pty","terminal":{"cols":80,"rows":24}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pty_id = body["pty_id"].as_str().unwrap().to_string();
    let attach_token = body["attach_token"].as_str().unwrap().to_string();

    let (base, server) = serve_live_app(app).await;
    let (mut bad_ws, _) = tokio_tungstenite::connect_async(format!("{base}/api/v1/pty/{pty_id}"))
        .await
        .unwrap();
    bad_ws
        .send(WsMessage::Text(
            serde_json::json!({"type":"auth","token":"wrong"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let error = bad_ws.next().await.unwrap().unwrap();
    let WsMessage::Text(error) = error else {
        panic!("expected error frame, got {error:?}");
    };
    let error: serde_json::Value = serde_json::from_str(&error).unwrap();
    assert_eq!(error["type"], "error");
    assert_eq!(error["code"], "unauthorized");

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("{base}/api/v1/pty/{pty_id}"))
        .await
        .unwrap();
    ws.send(WsMessage::Text(
        serde_json::json!({"type":"auth","token":attach_token})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let ready = ws.next().await.unwrap().unwrap();
    let WsMessage::Text(ready) = ready else {
        panic!("expected ready text frame, got {ready:?}");
    };
    let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
    assert_eq!(ready["type"], "ready");

    server.abort();
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
    assert!(log.contains("--env STEP_DEBUG"), "{log}");
    assert!(log.contains("--env STEP_REPO"), "{log}");
    assert!(log.contains("--env STEP_RATIO"), "{log}");
    assert!(log.contains("ubuntu-dev step-command alpha 3 1.5"), "{log}");
    assert!(log.contains("--workdir /repo-alpha-3"), "{log}");
    assert!(log.contains("--env COUNT"), "{log}");
    assert!(log.contains("--env DEBUG"), "{log}");
    assert!(log.contains("--env MODE"), "{log}");
    assert!(log.contains("--env RATIO"), "{log}");
    assert!(log.contains("--env REPO"), "{log}");
    assert!(log.contains("aw-gateway-exec"), "{log}");
    assert!(
        log.contains("launch-command alpha safe true 3 1.5"),
        "{log}"
    );
    assert!(log.contains("aw-gateway-exec-rm"), "{log}");

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
async fn launch_route_splices_passthrough_args() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app,
        "/api/v1/launches/args/run",
        r#"{"args":["--skill","fresh-eyes","review this"],"mode":"wait"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);

    let log = std::fs::read_to_string(&log).unwrap();
    assert!(log.contains("aw-gateway-exec"), "{log}");
    assert!(
        log.contains("launch-command before --skill fresh-eyes review this after"),
        "{log}"
    );
    assert!(log.contains("aw-gateway-exec-rm"), "{log}");
}

#[tokio::test]
async fn launch_route_rejects_disallowed_args_with_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let app = app_for_config(http_operation_config(&dir, &fake_runtime));

    let (status, body) = post_json(
        app,
        "/api/v1/launches/echo/run",
        r#"{"args":["--skill","fresh-eyes"],"mode":"wait"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_launch_args");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not allow passthrough args"),
        "{body}"
    );
    assert!(!log.exists());
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
    let launches = body["data"].as_array().unwrap();
    assert!(
        launches
            .iter()
            .any(|entry| { entry["name"] == "args" && entry["allow_args"] == true })
    );
    assert!(
        launches
            .iter()
            .any(|entry| { entry["name"] == "echo" && entry["allow_args"] == false })
    );

    let (status, body) = get_json(app, "/api/v1/launches/echo").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["name"], "echo");
    assert_eq!(body["data"]["target_mode"], "fixed");
    assert_eq!(body["data"]["target_container"], "ubuntu-dev");
    assert_eq!(body["data"]["allow_args"], false);
    assert_eq!(body["data"]["command"][0], "launch-command");
}

#[tokio::test]
async fn status_all_accepts_required_context_from_flattened_query() {
    let dir = tempfile::tempdir().unwrap();
    let fake_runtime = dir.path().join("runtime");
    let log = dir.path().join("runtime.log");
    write_fake_runtime(&fake_runtime, &fake_running_runtime_script(&log));
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        format!(
            r#"
schema_version = "1"

[http]
enabled = true
listen = "127.0.0.1:0"
enabled_actions = ["status"]

[runtime]
type = "podman"
program = "{program}"

[context_vars.tenant]
required = true

[target_defaults.workspace]
path = "{workspace}"
state_dir = ".aw-gateway"
cleanup = "never"

[target_defaults.container_agent]
enabled = false

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "ubuntu-dev-{{context.tenant}}"
stop_when_idle = false
"#,
            program = fake_runtime.display(),
            workspace = dir.path().join("workspace").display()
        ),
    )
    .unwrap();
    let app = app_for_config(config);

    let (status, body) = get_json(app.clone(), "/api/v1/status/all").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing required context key")
    );

    let (status, body) = get_json(app, "/api/v1/status/all?context.tenant=acme").await;
    assert_eq!(status, StatusCode::OK, "{body}");
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
        host_socket_exposures: Vec::new(),
        target: "default".into(),
        session_id: None,
        launch: None,
        mode: "fixed".into(),
        user: "alice".into(),
        image: "ubuntu/dev".into(),
        access: "ssh".into(),
        container: Some("ubuntu-dev".into()),
        context: BTreeMap::new(),
        container_pid: Some(123),
        active_sessions: 0,
        sessions: Vec::new(),
        agent_ready: true,
        ssh_socket: Some(PathBuf::from("/tmp/ssh.sock")),
        ssh_tcp: None,
        local_ssh: None,
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
