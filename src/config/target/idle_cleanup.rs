use crate::config::validation::{default_reap_signal, parse_duration, validate_name};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdleCleanupConfig {
    #[serde(default)]
    pub owner: IdleCleanupOwner,
    #[serde(default)]
    pub action: IdleCleanupAction,
    pub idle_grace: Option<String>,
    #[serde(default)]
    pub preserve_processes: Vec<String>,
    pub poll_interval: Option<String>,
    pub shutdown_timeout: Option<String>,
    #[serde(default = "default_reap_signal")]
    pub reap_signal: String,
    pub reap_kill_after: Option<String>,
}

impl Default for IdleCleanupConfig {
    fn default() -> Self {
        Self {
            owner: IdleCleanupOwner::default(),
            action: IdleCleanupAction::default(),
            idle_grace: None,
            preserve_processes: Vec::new(),
            poll_interval: None,
            shutdown_timeout: None,
            reap_signal: default_reap_signal(),
            reap_kill_after: None,
        }
    }
}

impl IdleCleanupConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        for value in [
            &self.idle_grace,
            &self.poll_interval,
            &self.shutdown_timeout,
            &self.reap_kill_after,
        ]
        .into_iter()
        .flatten()
        {
            parse_duration(value)?;
        }
        for process in &self.preserve_processes {
            validate_name("preserve_processes", process)?;
        }
        match self.reap_signal.as_str() {
            "TERM" | "KILL" | "INT" | "HUP" => {}
            _ => anyhow::bail!("unsupported reap_signal {:?}", self.reap_signal),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdleCleanupConfigInput {
    pub owner: Option<IdleCleanupOwner>,
    pub action: Option<IdleCleanupAction>,
    pub idle_grace: Option<String>,
    pub preserve_processes: Option<Vec<String>>,
    pub poll_interval: Option<String>,
    pub shutdown_timeout: Option<String>,
    pub reap_signal: Option<String>,
    pub reap_kill_after: Option<String>,
}

impl IdleCleanupConfigInput {
    pub(in crate::config) fn overlay(mut self, later: &Self) -> Self {
        if let Some(owner) = later.owner {
            self.owner = Some(owner);
        }
        if let Some(action) = later.action {
            self.action = Some(action);
        }
        if let Some(idle_grace) = &later.idle_grace {
            self.idle_grace = Some(idle_grace.clone());
        }
        if let Some(preserve_processes) = &later.preserve_processes {
            self.preserve_processes = Some(preserve_processes.clone());
        }
        if let Some(poll_interval) = &later.poll_interval {
            self.poll_interval = Some(poll_interval.clone());
        }
        if let Some(shutdown_timeout) = &later.shutdown_timeout {
            self.shutdown_timeout = Some(shutdown_timeout.clone());
        }
        if let Some(reap_signal) = &later.reap_signal {
            self.reap_signal = Some(reap_signal.clone());
        }
        if let Some(reap_kill_after) = &later.reap_kill_after {
            self.reap_kill_after = Some(reap_kill_after.clone());
        }
        self
    }

    pub(in crate::config) fn into_effective(self) -> anyhow::Result<IdleCleanupConfig> {
        let cleanup = IdleCleanupConfig {
            owner: self.owner.unwrap_or_default(),
            action: self.action.unwrap_or_default(),
            idle_grace: self.idle_grace,
            preserve_processes: self.preserve_processes.unwrap_or_default(),
            poll_interval: self.poll_interval,
            shutdown_timeout: self.shutdown_timeout,
            reap_signal: self.reap_signal.unwrap_or_else(default_reap_signal),
            reap_kill_after: self.reap_kill_after,
        };
        cleanup.validate()?;
        Ok(cleanup)
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdleCleanupOwner {
    None,
    Gateway,
    #[default]
    Agent,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdleCleanupAction {
    None,
    #[default]
    ExitContainer,
    ReapProcesses,
}
