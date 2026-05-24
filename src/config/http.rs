use super::validation::*;
use crate::action;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_http_listen")]
    pub listen: String,
    #[serde(default)]
    pub enabled_actions: Vec<String>,
    #[serde(default)]
    pub auth: HttpAuthConfig,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: default_http_listen(),
            enabled_actions: Vec::new(),
            auth: HttpAuthConfig::default(),
        }
    }
}

impl HttpConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.listen
            .parse::<SocketAddr>()
            .with_context(|| format!("parse http.listen {:?}", self.listen))?;
        if self.enabled && self.enabled_actions.is_empty() {
            anyhow::bail!("http.enabled_actions must not be empty when http.enabled = true");
        }
        for enabled_action in &self.enabled_actions {
            if !action::is_http_action_name(enabled_action) {
                anyhow::bail!(
                    "unknown or unsupported http.enabled_actions entry {enabled_action:?}"
                );
            }
        }
        self.auth.validate()
    }

    pub fn listen_addr(&self) -> anyhow::Result<SocketAddr> {
        self.listen
            .parse::<SocketAddr>()
            .with_context(|| format!("parse http.listen {:?}", self.listen))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpAuthConfig {
    #[serde(default, rename = "type")]
    pub auth_type: HttpAuthType,
    pub token_file: Option<String>,
}

impl Default for HttpAuthConfig {
    fn default() -> Self {
        Self {
            auth_type: HttpAuthType::None,
            token_file: None,
        }
    }
}

impl HttpAuthConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self.auth_type {
            HttpAuthType::None => {
                if self.token_file.is_some() {
                    anyhow::bail!(
                        "http.auth.token_file is only valid when http.auth.type = \"bearer\""
                    );
                }
            }
            HttpAuthType::Bearer => {
                let Some(token_file) = &self.token_file else {
                    anyhow::bail!(
                        "http.auth.token_file is required when http.auth.type = \"bearer\""
                    );
                };
                if token_file.is_empty() {
                    anyhow::bail!("http.auth.token_file must not be empty");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpAuthType {
    #[default]
    None,
    Bearer,
}
