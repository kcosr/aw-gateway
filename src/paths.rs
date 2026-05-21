use anyhow::Context;
use serde::Serialize;
use std::ffi::CStr;
use std::path::{Path, PathBuf};

pub const DEFAULT_AGENT_STATE_DIR: &str = "/tmp/aw-gateway/container";

#[derive(Debug, Clone, Serialize)]
pub struct UserContext {
    pub uid: u32,
    pub gid: u32,
    pub user: String,
    pub home: PathBuf,
}

impl UserContext {
    pub fn current() -> anyhow::Result<Self> {
        unsafe {
            let uid = libc::geteuid();
            let pw = libc::getpwuid(uid);
            if pw.is_null() {
                anyhow::bail!("could not resolve passwd entry for uid {uid}");
            }
            let pw = *pw;
            let user = CStr::from_ptr(pw.pw_name).to_string_lossy().into_owned();
            let home = CStr::from_ptr(pw.pw_dir).to_string_lossy().into_owned();
            #[cfg(debug_assertions)]
            let home = std::env::var_os("AW_GATEWAY_TEST_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(home));
            #[cfg(not(debug_assertions))]
            let home = PathBuf::from(home);
            Ok(Self {
                uid,
                gid: pw.pw_gid,
                user,
                home,
            })
        }
    }

    pub fn config_dir(&self) -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home.join(".config"))
            .join("aw-gateway")
    }

    pub fn state_dir(&self) -> PathBuf {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home.join(".local/state"))
            .join("aw-gateway")
    }
}

pub fn gateway_config_path(input: Option<PathBuf>) -> PathBuf {
    input
        .or_else(|| std::env::var_os("AW_GATEWAY_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/etc/aw-gateway/gateway.toml"))
}

pub fn agent_config_path(input: Option<PathBuf>) -> PathBuf {
    input
        .or_else(|| std::env::var_os("AW_CONTAINER_AGENT_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/etc/aw-gateway/container-agent.toml"))
}

pub fn bootstrap_config_path(input: Option<PathBuf>) -> PathBuf {
    input
        .or_else(|| std::env::var_os("AW_CONTAINER_BOOTSTRAP_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/etc/aw-gateway/container-bootstrap.toml"))
}

pub fn resolve_workspace(home: &Path, configured: &str) -> PathBuf {
    let path = expand_home(home, configured);
    if path.is_absolute() {
        path
    } else {
        home.join(path)
    }
}

pub fn expand_home(home: &Path, input: &str) -> PathBuf {
    if input == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(input)
}

pub fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}
