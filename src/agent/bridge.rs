use crate::template::{self, Vars};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::Semaphore;
use tokio::time::{Duration, sleep};

use super::socket::bind_private_unix_socket;
use super::state::AgentState;

const MAX_BRIDGE_STREAMS: usize = 1024;
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

pub(super) async fn run_bridge(
    state: Arc<AgentState>,
    socket_template: String,
    target: String,
) -> anyhow::Result<()> {
    let mut vars = Vars::new();
    vars.insert(
        "container_state_dir".into(),
        state.state_dir.display().to_string(),
    );
    let socket = PathBuf::from(template::render(&socket_template, &vars)?);
    let listener = bind_private_unix_socket(&socket, state.socket_owner).await?;
    state.bridge_ready.store(true, Ordering::SeqCst);
    let stream_slots = Arc::new(Semaphore::new(MAX_BRIDGE_STREAMS));
    loop {
        let (client, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                tracing::warn!(error = %err, "ssh bridge accept failed");
                sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        if !state.accepting_bridge.load(Ordering::SeqCst) {
            continue;
        }
        let Ok(stream_permit) = stream_slots.clone().try_acquire_owned() else {
            tracing::warn!(
                limit = MAX_BRIDGE_STREAMS,
                "ssh bridge stream limit reached; rejecting connection"
            );
            continue;
        };
        let target = target.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let _stream_permit = stream_permit;
            state.active_streams.fetch_add(1, Ordering::SeqCst);
            let result = proxy_to_tcp(client, &target).await;
            state.active_streams.fetch_sub(1, Ordering::SeqCst);
            if let Err(err) = result {
                tracing::warn!(error = %err, "ssh bridge stream failed");
            }
        });
    }
}

async fn proxy_to_tcp(mut client: UnixStream, target: &str) -> anyhow::Result<()> {
    let mut remote = TcpStream::connect(target).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}
