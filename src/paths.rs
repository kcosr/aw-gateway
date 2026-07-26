use crate::unix_account::passwd_by_uid;
use serde::Serialize;
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
        let uid = unsafe { libc::geteuid() };
        let passwd = passwd_by_uid(uid)?;
        let user = passwd
            .name
            .into_string()
            .map_err(|_| anyhow::anyhow!("passwd user name for uid {uid} is not valid UTF-8"))?;
        #[cfg(debug_assertions)]
        let home = std::env::var_os("AW_GATEWAY_TEST_HOME")
            .map(PathBuf::from)
            .unwrap_or(passwd.home);
        #[cfg(not(debug_assertions))]
        let home = passwd.home;
        Ok(Self {
            uid,
            gid: passwd.gid,
            user,
            home,
        })
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
    pub user: Option<UserContext>,
    pub user_config_dir: Option<PathBuf>,
    pub user_state_dir: Option<PathBuf>,
    pub user_config_file: Option<PathBuf>,
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
    let system_config_file = PathBuf::from(SYSTEM_GATEWAY_CONFIG_PATH);
    let system_candidate = GatewayConfigCandidate {
        source: GatewayConfigSource::System,
        exists: system_config_file.exists(),
        path: system_config_file.clone(),
    };

    let mut candidates = Vec::new();
    let mut user = None;
    let (selected_source, selected_path) = if let Some(path) = input {
        candidates.push(GatewayConfigCandidate {
            source: GatewayConfigSource::ExplicitFlag,
            exists: path.exists(),
            path: path.clone(),
        });
        if let Ok(context) = UserContext::current() {
            let (_, _, user_candidate) = user_gateway_paths(&context);
            candidates.push(user_candidate);
            user = Some(context);
        }
        candidates.push(system_candidate.clone());
        (GatewayConfigSource::ExplicitFlag, Some(path))
    } else if let Some(path) = std::env::var_os("AW_GATEWAY_CONFIG").map(PathBuf::from) {
        candidates.push(GatewayConfigCandidate {
            source: GatewayConfigSource::Environment,
            exists: path.exists(),
            path: path.clone(),
        });
        if let Ok(context) = UserContext::current() {
            let (_, _, user_candidate) = user_gateway_paths(&context);
            candidates.push(user_candidate);
            user = Some(context);
        }
        candidates.push(system_candidate.clone());
        (GatewayConfigSource::Environment, Some(path))
    } else {
        let context = UserContext::current()?;
        let (_, _, user_candidate) = user_gateway_paths(&context);
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
        candidates.push(system_candidate.clone());
        user = Some(context);
        selected
    };
    let (user_config_dir, user_state_dir, user_config_file) = user
        .as_ref()
        .map(|user| {
            let (config_dir, state_dir, candidate) = user_gateway_paths(user);
            (Some(config_dir), Some(state_dir), Some(candidate.path))
        })
        .unwrap_or((None, None, None));

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

fn user_gateway_paths(user: &UserContext) -> (PathBuf, PathBuf, GatewayConfigCandidate) {
    let user_config_dir = user.config_dir();
    let user_state_dir = user.state_dir();
    let user_config_file = user_config_dir.join(USER_GATEWAY_CONFIG_FILE);
    let user_candidate = GatewayConfigCandidate {
        source: GatewayConfigSource::User,
        exists: user_config_file.exists(),
        path: user_config_file,
    };
    (user_config_dir, user_state_dir, user_candidate)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwd_lookup_is_stable_under_parallel_uid_and_name_load() {
        let current_uid = unsafe { libc::geteuid() };
        let expected = passwd_by_uid(current_uid).unwrap();
        let name = expected.name.clone().into_string().unwrap();

        std::thread::scope(|scope| {
            for worker in 0..8 {
                let expected = &expected;
                let name = &name;
                scope.spawn(move || {
                    for iteration in 0..100 {
                        let actual = if (worker + iteration) % 2 == 0 {
                            passwd_by_uid(current_uid).unwrap()
                        } else {
                            crate::unix_account::passwd_by_name(name).unwrap()
                        };
                        assert_eq!(actual.uid, expected.uid);
                        assert_eq!(actual.gid, expected.gid);
                        assert_eq!(actual.name, expected.name);
                        assert_eq!(actual.home, expected.home);
                    }
                });
            }
        });
    }
}
