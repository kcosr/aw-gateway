use super::Runtime;
use super::model::{LocalSshReady, ReadyStatus, SshTarget, TcpEndpoint};
use super::session::LocalListenerGuard;
use crate::runtime;
use anyhow::Context;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};

pub(super) async fn proxy_ready_to_stdio(ready: &ReadyStatus) -> anyhow::Result<()> {
    match ready.ssh_target() {
        SshTarget::Unix(socket) => {
            runtime::socket_is_safe(&socket)?;
            let stream = UnixStream::connect(&socket)
                .await
                .with_context(|| format!("connect {}", socket.display()))?;
            proxy_stdio(stream).await
        }
        SshTarget::Tcp(endpoint) => {
            let stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
                .await
                .with_context(|| format!("connect {}:{}", endpoint.host, endpoint.port))?;
            proxy_stdio(stream).await
        }
    }
}

pub(super) async fn bind_local_ssh(runtime: &Runtime) -> anyhow::Result<BoundLocalListener> {
    let local_ssh = runtime
        .target
        .local_ssh
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("target does not configure local_ssh"))?;
    let _lock = runtime.acquire_lifecycle_lock().await?;
    if let Some(active) = runtime.active_local_listener_status()? {
        anyhow::bail!(
            "local SSH listener is already active on {}:{}",
            active.host,
            active.port
        );
    }
    let port = local_ssh.port.unwrap_or(0);
    let listener = TcpListener::bind((local_ssh.host.as_str(), port))
        .await
        .with_context(|| format!("bind {}:{port}", local_ssh.host))?;
    let actual_port = listener.local_addr()?.port();
    let guard = runtime.write_local_listener_status(&local_ssh.host, actual_port)?;
    Ok(BoundLocalListener {
        listener,
        _guard: guard,
        ready: LocalSshReady {
            host: local_ssh.host.clone(),
            port: actual_port,
        },
    })
}

pub(super) async fn serve_local_ssh(
    bound: BoundLocalListener,
    target: SshTarget,
) -> anyhow::Result<()> {
    let BoundLocalListener {
        listener,
        _guard,
        ready: _,
    } = bound;
    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((client, _)) => {
                        let target = target.clone();
                        tokio::spawn(async move {
                            if let Err(err) = proxy_local_ssh(client, target).await {
                                tracing::warn!(error = %err, "local SSH listener connection failed");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "local SSH listener accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                return Ok(());
            }
        }
    }
}

pub(super) struct BoundLocalListener {
    pub(super) listener: TcpListener,
    pub(super) _guard: LocalListenerGuard,
    pub(super) ready: LocalSshReady,
}

async fn proxy_stdio<S>(stream: S) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut socket_read, mut socket_write) = tokio::io::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let client_to_socket = async {
        tokio::io::copy(&mut stdin, &mut socket_write).await?;
        socket_write.shutdown().await
    };
    let socket_to_client = async {
        tokio::io::copy(&mut socket_read, &mut stdout).await?;
        stdout.shutdown().await
    };
    let _ = tokio::try_join!(client_to_socket, socket_to_client)?;
    Ok(())
}

async fn proxy_local_ssh(client: TcpStream, target: SshTarget) -> anyhow::Result<()> {
    match target {
        SshTarget::Unix(socket) => proxy_tcp_to_unix(client, socket).await,
        SshTarget::Tcp(endpoint) => proxy_tcp_to_tcp(client, endpoint).await,
    }
}

async fn proxy_tcp_to_unix(mut client: TcpStream, socket: PathBuf) -> anyhow::Result<()> {
    runtime::socket_is_safe(&socket)?;
    let mut remote = UnixStream::connect(&socket).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}

async fn proxy_tcp_to_tcp(mut client: TcpStream, endpoint: TcpEndpoint) -> anyhow::Result<()> {
    let mut remote = TcpStream::connect((endpoint.host.as_str(), endpoint.port)).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}
