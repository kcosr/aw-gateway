use crate::fileutil;
use crate::template::{self, Vars};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::net::{TcpStream, UnixListener, UnixStream};

use super::socket::{apply_path_owner, unlink_socket_if_present};
use super::state::AgentState;

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
    if let Some(parent) = socket.parent() {
        fileutil::ensure_private_dir(parent)?;
        apply_path_owner(parent, state.socket_owner)?;
    }
    unlink_socket_if_present(&socket).await?;
    let listener = UnixListener::bind(&socket)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).await?;
    }
    apply_path_owner(&socket, state.socket_owner)?;
    state.bridge_ready.store(true, Ordering::SeqCst);
    loop {
        let (client, _) = listener.accept().await?;
        if !state.accepting_bridge.load(Ordering::SeqCst) {
            continue;
        }
        let target = target.clone();
        let state = state.clone();
        tokio::spawn(async move {
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
