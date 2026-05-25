use serde::Serialize;
use std::ffi::CStr;
use std::path::{Path, PathBuf};

pub const DEFAULT_AGENT_STATE_DIR: &str = "/tmp/aw-gateway/container";
pub const SYSTEM_GATEWAY_CONFIG_PATH: &str = "/etc/aw-gateway/gateway.toml";
const USER_GATEWAY_CONFIG_FILE: &str = "gateway.toml";

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
        std::env::var_os("AW_GATEWAY_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
            .unwrap_or_else(|| self.home.join(".config"))
            .join("aw-gateway")
    }

    pub fn state_dir(&self) -> PathBuf {
        std::env::var_os("AW_GATEWAY_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_STATE_HOME").map(PathBuf::from))
            .unwrap_or_else(|| self.home.join(".local/state"))
            .join("aw-gateway")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayConfigSource {
    ExplicitFlag,
    Environment,
    User,
    System,
    None,
}

#[derive(Debug, Clone, Serialize)]
pub struct GatewayConfigCandidate {
    pub source: GatewayConfigSource,
    pub path: PathBuf,
    pub exists: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GatewayConfigResolution {
    pub user: UserContext,
    pub user_config_dir: PathBuf,
    pub user_state_dir: PathBuf,
    pub user_config_file: PathBuf,
    pub system_config_file: PathBuf,
    pub candidates: Vec<GatewayConfigCandidate>,
    pub selected_source: GatewayConfigSource,
    pub selected_path: Option<PathBuf>,
}

impl GatewayConfigResolution {
    pub fn selected_path(&self) -> anyhow::Result<PathBuf> {
        self.selected_path.clone().ok_or_else(|| {
            let checked = self
                .candidates
                .iter()
                .map(|candidate| candidate.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!("gateway config not found; checked: {checked}")
        })
    }
}

pub fn gateway_config_path(input: Option<PathBuf>) -> PathBuf {
    resolve_gateway_config(input)
        .and_then(|resolution| resolution.selected_path())
        .unwrap_or_else(|_| PathBuf::from(SYSTEM_GATEWAY_CONFIG_PATH))
}

pub fn resolve_gateway_config(input: Option<PathBuf>) -> anyhow::Result<GatewayConfigResolution> {
    let user = UserContext::current()?;
    let user_config_dir = user.config_dir();
    let user_state_dir = user.state_dir();
    let user_config_file = user_config_dir.join(USER_GATEWAY_CONFIG_FILE);
    let system_config_file = PathBuf::from(SYSTEM_GATEWAY_CONFIG_PATH);
    let user_candidate = GatewayConfigCandidate {
        source: GatewayConfigSource::User,
        exists: user_config_file.exists(),
        path: user_config_file.clone(),
    };
    let system_candidate = GatewayConfigCandidate {
        source: GatewayConfigSource::System,
        exists: system_config_file.exists(),
        path: system_config_file.clone(),
    };

    let mut candidates = Vec::new();
    let (selected_source, selected_path) = if let Some(path) = input {
        candidates.push(GatewayConfigCandidate {
            source: GatewayConfigSource::ExplicitFlag,
            exists: path.exists(),
            path: path.clone(),
        });
        candidates.push(user_candidate);
        candidates.push(system_candidate);
        (GatewayConfigSource::ExplicitFlag, Some(path))
    } else if let Some(path) = std::env::var_os("AW_GATEWAY_CONFIG").map(PathBuf::from) {
        candidates.push(GatewayConfigCandidate {
            source: GatewayConfigSource::Environment,
            exists: path.exists(),
            path: path.clone(),
        });
        candidates.push(user_candidate);
        candidates.push(system_candidate);
        (GatewayConfigSource::Environment, Some(path))
    } else {
        let selected = if user_candidate.exists {
            (GatewayConfigSource::User, Some(user_candidate.path.clone()))
        } else if system_candidate.exists {
            (
                GatewayConfigSource::System,
                Some(system_candidate.path.clone()),
            )
        } else {
            (GatewayConfigSource::None, None)
        };
        candidates.push(user_candidate);
        candidates.push(system_candidate);
        selected
    };

    Ok(GatewayConfigResolution {
        user,
        user_config_dir,
        user_state_dir,
        user_config_file,
        system_config_file,
        candidates,
        selected_source,
        selected_path,
    })
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
    // Keep `paths` as the public boundary while the shared implementation stays crate-private.
    crate::fileutil::ensure_private_dir(path)
}
