use anyhow::Context;
use std::ffi::CString;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use tokio::net::{UnixListener, UnixStream};

use super::state::SocketOwner;

pub(super) async fn unlink_socket_if_present(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                if !meta.file_type().is_socket() {
                    anyhow::bail!("refusing to unlink non-socket {}", path.display());
                }
            }
            tokio::fs::remove_file(path).await?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    }
    Ok(())
}

pub(super) async fn bind_private_unix_socket(
    path: &Path,
    owner: Option<SocketOwner>,
) -> anyhow::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        crate::fileutil::ensure_private_dir(parent)?;
        apply_path_owner(parent, owner)?;
    }
    unlink_socket_if_present(path).await?;
    let listener = UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    apply_path_owner(path, owner)?;
    Ok(listener)
}

pub(super) fn apply_path_owner(path: &Path, owner: Option<SocketOwner>) -> anyhow::Result<()> {
    let Some(owner) = owner else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("path contains NUL byte: {}", path.display()))?;
        let rc = unsafe { libc::chown(c_path.as_ptr(), owner.uid, owner.gid) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("chown {}", path.display()));
        }
    }
    Ok(())
}

pub(super) fn validate_control_peer(
    stream: &UnixStream,
    expected_uid: Option<u32>,
) -> anyhow::Result<()> {
    let Some(expected_uid) = expected_uid else {
        return Ok(());
    };
    let peer = unix_peer_credentials(stream)?;
    if peer.uid != expected_uid {
        anyhow::bail!(
            "control peer uid mismatch: expected {}, got {}",
            expected_uid,
            peer.uid
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct PeerCredentials {
    uid: u32,
}

#[cfg(target_os = "linux")]
fn unix_peer_credentials(stream: &UnixStream) -> anyhow::Result<PeerCredentials> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            std::ptr::addr_of_mut!(len),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("getsockopt SO_PEERCRED");
    }
    Ok(PeerCredentials { uid: cred.uid })
}

#[cfg(target_os = "macos")]
fn unix_peer_credentials(stream: &UnixStream) -> anyhow::Result<PeerCredentials> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("getpeereid");
    }
    Ok(PeerCredentials { uid })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unix_peer_credentials(_stream: &UnixStream) -> anyhow::Result<PeerCredentials> {
    anyhow::bail!("Unix peer credential validation is not supported on this platform")
}
