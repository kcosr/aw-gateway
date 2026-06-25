use super::Runtime;
use super::failures::AgentNotReady;
use crate::agent_control::{
    AgentStatus, ControlEnvelope, ControlFailure, ControlSuccess, SessionHoldParams,
    SessionHoldResult, ShutdownParams, ShutdownResult,
};
use crate::config::{IdleCleanupAction, IdleCleanupOwner};
use anyhow::Context;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{Duration, Instant, sleep};

const MAX_AGENT_RESPONSE_BYTES: usize = 64 * 1024;

pub(super) struct AgentSessionHold {
    pub(super) _reader: BufReader<UnixStream>,
}

impl Runtime {
    pub(super) async fn wait_agent_ready(&self) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(status) = self.agent_status().await
                && status.ready
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(AgentNotReady.into());
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    pub(super) async fn agent_status(&self) -> anyhow::Result<AgentStatus> {
        let response = self
            .agent_request::<AgentStatus>(&ControlEnvelope::status(serde_json::json!("status")))
            .await?;
        Ok(response.result)
    }

    pub(super) async fn agent_shutdown(&self) -> anyhow::Result<()> {
        let token = self.control_token()?;
        let _response = self
            .agent_request::<ShutdownResult>(&ControlEnvelope::shutdown(
                serde_json::json!("shutdown"),
                ShutdownParams {
                    token: Some(token),
                    reason: Some("gateway-stop".into()),
                },
            ))
            .await?;
        Ok(())
    }

    async fn agent_request<T: DeserializeOwned>(
        &self,
        request: &ControlEnvelope,
    ) -> anyhow::Result<ControlSuccess<T>> {
        let (response, _reader) = self
            .send_typed_agent_request(
                request,
                "timed out waiting for container agent control response",
            )
            .await?;
        Ok(response)
    }

    async fn send_typed_agent_request<T: DeserializeOwned>(
        &self,
        request: &ControlEnvelope,
        timeout_message: &'static str,
    ) -> anyhow::Result<(ControlSuccess<T>, BufReader<UnixStream>)> {
        tokio::time::timeout(Duration::from_secs(5), async {
            self.validate_agent_socket().await?;
            let mut stream = UnixStream::connect(self.agent_socket()).await?;
            let mut payload = serde_json::to_vec(&request)?;
            payload.push(b'\n');
            stream.write_all(&payload).await?;
            let mut reader = BufReader::new(stream);
            let line = read_agent_response(&mut reader).await?;
            let response = Self::parse_agent_control_success(&line)?;
            Ok((response, reader))
        })
        .await
        .context(timeout_message)?
    }

    pub(super) fn parse_agent_control_success<T: DeserializeOwned>(
        line: &str,
    ) -> anyhow::Result<ControlSuccess<T>> {
        let value: serde_json::Value = serde_json::from_str(line)?;
        match value.get("ok").and_then(serde_json::Value::as_bool) {
            Some(true) => Ok(serde_json::from_value::<ControlSuccess<T>>(value)?),
            Some(false) if value.get("error").is_some() => {
                let failure = serde_json::from_value::<ControlFailure>(value)?;
                Err(Self::agent_control_failure(failure))
            }
            Some(false) => {
                let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                anyhow::bail!("agent control request returned ok=false without error: {id:?}")
            }
            None => Ok(serde_json::from_value::<ControlSuccess<T>>(value)?),
        }
    }

    fn agent_control_failure(failure: ControlFailure) -> anyhow::Error {
        anyhow::anyhow!(
            "agent control request failed: {}: {}",
            failure.error.code,
            failure.error.message
        )
    }

    pub(super) async fn agent_session_hold(
        &self,
        kind: &str,
    ) -> anyhow::Result<Option<AgentSessionHold>> {
        if !self.uses_agent_idle_cleanup() {
            return Ok(None);
        }
        let token = self.control_token()?;
        let request = ControlEnvelope::session_hold(
            serde_json::json!("session_hold"),
            SessionHoldParams {
                token: Some(token),
                kind: Some(kind.to_string()),
            },
        );
        let (response, reader) = self
            .send_typed_agent_request::<SessionHoldResult>(
                &request,
                "timed out opening container agent session hold",
            )
            .await?;
        if !response.result.held {
            anyhow::bail!("agent session hold response did not confirm hold");
        }
        Ok(Some(AgentSessionHold { _reader: reader }))
    }

    fn uses_agent_idle_cleanup(&self) -> bool {
        self.agent_control_enabled()
            && self.target.idle_cleanup.as_ref().is_some_and(|cleanup| {
                cleanup.owner == IdleCleanupOwner::Agent
                    && cleanup.action != IdleCleanupAction::None
            })
    }
}

async fn read_agent_response(reader: &mut BufReader<UnixStream>) -> anyhow::Result<String> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            anyhow::bail!("empty agent control response");
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        if line.len() + end > MAX_AGENT_RESPONSE_BYTES {
            anyhow::bail!("agent control response exceeds {MAX_AGENT_RESPONSE_BYTES} bytes");
        }
        line.extend_from_slice(&available[..end]);
        reader.consume(end);
        if line.ends_with(b"\n") {
            return String::from_utf8(line).context("agent control response is not UTF-8");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_agent_response_rejects_oversized_line() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let writer = tokio::spawn(async move {
            server
                .write_all(&vec![b'x'; MAX_AGENT_RESPONSE_BYTES + 1])
                .await
                .unwrap();
        });
        let mut reader = BufReader::new(client);

        let err = read_agent_response(&mut reader).await.unwrap_err();
        drop(reader);
        writer.await.unwrap();

        assert!(err.to_string().contains("response exceeds"), "{err:#}");
    }
}
