use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

pub(crate) type ControlRequestId = serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ControlEnvelope {
    pub(crate) id: ControlRequestId,
    pub(crate) request: ControlRequest,
}

impl ControlEnvelope {
    pub(crate) fn status(id: ControlRequestId) -> Self {
        Self {
            id,
            request: ControlRequest::Status,
        }
    }

    pub(crate) fn session_hold(id: ControlRequestId, params: SessionHoldParams) -> Self {
        Self {
            id,
            request: ControlRequest::SessionHold(params),
        }
    }

    pub(crate) fn shutdown(id: ControlRequestId, params: ShutdownParams) -> Self {
        Self {
            id,
            request: ControlRequest::Shutdown(params),
        }
    }

    #[cfg(test)]
    pub(crate) fn reap_now(id: ControlRequestId, params: ReapNowParams) -> Self {
        Self {
            id,
            request: ControlRequest::ReapNow(params),
        }
    }

    pub(crate) fn decode(value: &serde_json::Value) -> DecodedControlEnvelope {
        let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let Some(method) = value.get("method").and_then(serde_json::Value::as_str) else {
            return DecodedControlEnvelope::UnknownMethod(id);
        };
        let params = value.get("params");
        let request = match method {
            "status" => ControlRequest::Status,
            "session_hold" => ControlRequest::SessionHold(SessionHoldParams {
                token: string_param(params, "token"),
                kind: string_param(params, "kind"),
            }),
            "shutdown" => ControlRequest::Shutdown(ShutdownParams {
                token: string_param(params, "token"),
                reason: string_param(params, "reason"),
            }),
            "reap_now" => ControlRequest::ReapNow(ReapNowParams {
                token: string_param(params, "token"),
                dry_run: bool_param(params, "dry_run"),
            }),
            _ => return DecodedControlEnvelope::UnknownMethod(id),
        };
        DecodedControlEnvelope::Request(Self { id, request })
    }
}

impl Serialize for ControlEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let has_params = !matches!(self.request, ControlRequest::Status);
        let mut state =
            serializer.serialize_struct("ControlEnvelope", if has_params { 3 } else { 2 })?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("method", self.request.method())?;
        match &self.request {
            ControlRequest::Status => {}
            ControlRequest::SessionHold(params) => state.serialize_field("params", params)?,
            ControlRequest::Shutdown(params) => state.serialize_field("params", params)?,
            ControlRequest::ReapNow(params) => state.serialize_field("params", params)?,
        }
        state.end()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DecodedControlEnvelope {
    Request(ControlEnvelope),
    UnknownMethod(ControlRequestId),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ControlRequest {
    Status,
    SessionHold(SessionHoldParams),
    Shutdown(ShutdownParams),
    ReapNow(ReapNowParams),
}

impl ControlRequest {
    pub(crate) fn method(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::SessionHold(_) => "session_hold",
            Self::Shutdown(_) => "shutdown",
            Self::ReapNow(_) => "reap_now",
        }
    }

    pub(crate) fn auth_requirement(&self) -> AuthRequirement {
        match self {
            Self::Status => AuthRequirement::None,
            Self::SessionHold(_) | Self::Shutdown(_) | Self::ReapNow(_) => {
                AuthRequirement::ControlToken
            }
        }
    }

    pub(crate) fn token(&self) -> Option<&str> {
        match self {
            Self::Status => None,
            Self::SessionHold(params) => params.token.as_deref(),
            Self::Shutdown(params) => params.token.as_deref(),
            Self::ReapNow(params) => params.token.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct SessionHoldParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ShutdownParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ReapNowParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dry_run: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthRequirement {
    None,
    ControlToken,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ControlErrorBody {
    pub(crate) code: String,
    pub(crate) message: String,
}

impl ControlErrorBody {
    pub(crate) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub(crate) struct ControlSuccess<T> {
    pub(crate) id: ControlRequestId,
    pub(crate) ok: bool,
    pub(crate) result: T,
}

impl<T> ControlSuccess<T> {
    pub(crate) fn new(id: ControlRequestId, result: T) -> Self {
        Self {
            id,
            ok: true,
            result,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub(crate) struct ControlFailure {
    pub(crate) id: ControlRequestId,
    pub(crate) ok: bool,
    pub(crate) error: ControlErrorBody,
}

impl ControlFailure {
    pub(crate) fn new(
        id: ControlRequestId,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id,
            ok: false,
            error: ControlErrorBody::new(code, message),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct SessionHoldResult {
    pub(crate) held: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ShutdownResult {
    pub(crate) shutting_down: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub(crate) struct AgentStatus {
    pub(crate) ready: bool,
    pub(crate) version: String,
    pub(crate) services: Vec<ServiceStatus>,
    pub(crate) ssh_bridge: BridgeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) access_flow_relay: Option<AccessFlowRelayStatus>,
    pub(crate) idle_cleanup: Option<IdleCleanupStatus>,
    pub(crate) shutting_down: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct AccessFlowRelayStatus {
    pub(crate) state: AccessFlowRelayStateName,
    pub(crate) ready: bool,
    pub(crate) active_flows: usize,
    pub(crate) routes: Vec<AccessFlowRelayRouteStatus>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct AccessFlowRelayRouteStatus {
    pub(crate) name: String,
    pub(crate) accepting: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccessFlowRelayStateName {
    Preparing,
    Accepting,
    Draining,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct BridgeStatus {
    pub(crate) enabled: bool,
    pub(crate) ready: bool,
    pub(crate) active_streams: usize,
    pub(crate) active_sessions: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub(crate) struct IdleCleanupStatus {
    pub(crate) owner: String,
    pub(crate) action: String,
    pub(crate) state: IdleStateName,
    pub(crate) idle_for_ms: Option<u128>,
    pub(crate) preserve: bool,
    pub(crate) preserve_reason: Option<String>,
    pub(crate) matched_processes: Vec<ProcessMatch>,
    pub(crate) last_reap_result: Option<ReapResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ServiceStatus {
    pub(crate) name: String,
    pub(crate) required: bool,
    pub(crate) state: String,
    pub(crate) pid: Option<u32>,
    pub(crate) healthy: bool,
    pub(crate) restart_count: usize,
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ProcessMatch {
    pub(crate) pid: u32,
    pub(crate) comm: String,
    #[serde(default, skip)]
    pub(crate) start_time: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ReapResult {
    pub(crate) dry_run: bool,
    pub(crate) would_terminate: Vec<ProcessMatch>,
    pub(crate) preserved: Vec<ProcessMatch>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IdleStateName {
    #[default]
    IdlePending,
    Attached,
    Preserved,
    ShutdownContainer,
    ReapUnpreservedProcesses,
}

fn string_param(params: Option<&serde_json::Value>, key: &str) -> Option<String> {
    params
        .and_then(|params| params.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn bool_param(params: Option<&serde_json::Value>, key: &str) -> Option<bool> {
    params
        .and_then(|params| params.get(key))
        .and_then(serde_json::Value::as_bool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn decodes_status_request_with_id() {
        let decoded = ControlEnvelope::decode(&json!({"id":"status","method":"status"}));
        assert_eq!(
            decoded,
            DecodedControlEnvelope::Request(ControlEnvelope::status(json!("status")))
        );
    }

    #[test]
    fn decodes_session_hold_request_params() {
        let decoded = ControlEnvelope::decode(
            &json!({"id":"session_hold","method":"session_hold","params":{"token":"secret","kind":"run"}}),
        );
        assert_eq!(
            decoded,
            DecodedControlEnvelope::Request(ControlEnvelope::session_hold(
                json!("session_hold"),
                SessionHoldParams {
                    token: Some("secret".into()),
                    kind: Some("run".into()),
                },
            ))
        );
    }

    #[test]
    fn decodes_shutdown_request_params() {
        let decoded = ControlEnvelope::decode(
            &json!({"id":"shutdown","method":"shutdown","params":{"token":"secret","reason":"gateway-stop"}}),
        );
        assert_eq!(
            decoded,
            DecodedControlEnvelope::Request(ControlEnvelope::shutdown(
                json!("shutdown"),
                ShutdownParams {
                    token: Some("secret".into()),
                    reason: Some("gateway-stop".into()),
                },
            ))
        );
    }

    #[test]
    fn decodes_reap_now_request_params() {
        let decoded = ControlEnvelope::decode(
            &json!({"id":"reap_now","method":"reap_now","params":{"token":"secret","dry_run":true}}),
        );
        assert_eq!(
            decoded,
            DecodedControlEnvelope::Request(ControlEnvelope::reap_now(
                json!("reap_now"),
                ReapNowParams {
                    token: Some("secret".into()),
                    dry_run: Some(true),
                },
            ))
        );
    }

    #[test]
    fn decode_uses_null_id_when_missing_and_echoes_present_id() {
        let decoded = ControlEnvelope::decode(&json!({"method":"status"}));
        assert_eq!(
            decoded,
            DecodedControlEnvelope::Request(ControlEnvelope::status(Value::Null))
        );

        let id = json!({"nested":["id",1]});
        let decoded = ControlEnvelope::decode(&json!({"id":id,"method":"status"}));
        assert_eq!(
            decoded,
            DecodedControlEnvelope::Request(ControlEnvelope::status(json!({"nested":["id",1]})))
        );
    }

    #[test]
    fn serializes_success_and_failure_responses() {
        let success = ControlSuccess::new(json!("status"), SessionHoldResult { held: true });
        assert_eq!(
            serde_json::to_value(success).unwrap(),
            json!({"id":"status","ok":true,"result":{"held":true}})
        );

        let failure = ControlFailure::new(Value::Null, "unknown_method", "unknown control method");
        assert_eq!(
            serde_json::to_value(failure).unwrap(),
            json!({"id":null,"ok":false,"error":{"code":"unknown_method","message":"unknown control method"}})
        );
    }

    #[test]
    fn serializes_gateway_requests_with_stable_method_and_params_names() {
        let status = ControlEnvelope::status(json!("status"));
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({"id":"status","method":"status"})
        );

        let hold = ControlEnvelope::session_hold(
            json!("session_hold"),
            SessionHoldParams {
                token: Some("secret".into()),
                kind: Some("run".into()),
            },
        );
        assert_eq!(
            serde_json::to_value(hold).unwrap(),
            json!({"id":"session_hold","method":"session_hold","params":{"token":"secret","kind":"run"}})
        );
    }

    #[test]
    fn agent_status_field_names_round_trip() {
        let value = json!({
            "ready": true,
            "version": "0.2.0",
            "services": [{
                "name": "container-sshd",
                "required": true,
                "state": "running",
                "pid": 42,
                "healthy": true,
                "restart_count": 1,
                "last_error": null
            }],
            "ssh_bridge": {
                "enabled": true,
                "ready": true,
                "active_streams": 2,
                "active_sessions": 1
            },
            "idle_cleanup": {
                "owner": "agent",
                "action": "reap_processes",
                "state": "reap_unpreserved_processes",
                "idle_for_ms": 100,
                "preserve": false,
                "preserve_reason": null,
                "matched_processes": [{"pid": 1000, "comm": "tmux"}],
                "last_reap_result": {
                    "dry_run": true,
                    "would_terminate": [{"pid": 2000, "comm": "bash"}],
                    "preserved": []
                }
            },
            "shutting_down": false
        });
        let status: AgentStatus = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(status.ssh_bridge.active_sessions, 1);
        assert_eq!(
            status.idle_cleanup.as_ref().unwrap().state,
            IdleStateName::ReapUnpreservedProcesses
        );
        assert_eq!(serde_json::to_value(status).unwrap(), value);
    }

    #[test]
    fn idle_state_names_use_expected_snake_case_wire_values() {
        let cases = [
            (IdleStateName::IdlePending, "idle_pending"),
            (IdleStateName::Attached, "attached"),
            (IdleStateName::Preserved, "preserved"),
            (IdleStateName::ShutdownContainer, "shutdown_container"),
            (
                IdleStateName::ReapUnpreservedProcesses,
                "reap_unpreserved_processes",
            ),
        ];
        for (state, wire) in cases {
            assert_eq!(serde_json::to_value(state).unwrap(), json!(wire));
            assert_eq!(
                serde_json::from_value::<IdleStateName>(json!(wire)).unwrap(),
                state
            );
        }
    }

    #[test]
    fn missing_non_string_and_unknown_method_decode_as_unknown_method() {
        for value in [
            json!({"id":"missing"}),
            json!({"id":"non-string","method":123}),
            json!({"id":"unknown","method":"bogus"}),
        ] {
            let expected_id = value.get("id").cloned().unwrap();
            assert_eq!(
                ControlEnvelope::decode(&value),
                DecodedControlEnvelope::UnknownMethod(expected_id)
            );
        }
    }

    #[test]
    fn malformed_token_params_become_none_instead_of_decode_errors() {
        let decoded =
            ControlEnvelope::decode(&json!({"id":"hold","method":"session_hold","params":false}));
        assert_eq!(
            decoded,
            DecodedControlEnvelope::Request(ControlEnvelope::session_hold(
                json!("hold"),
                SessionHoldParams {
                    token: None,
                    kind: None,
                },
            ))
        );
    }
}
