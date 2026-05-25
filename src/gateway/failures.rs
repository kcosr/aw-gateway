use std::fmt;

#[derive(Debug)]
pub(in crate::gateway) struct AgentNotReady;

impl fmt::Display for AgentNotReady {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("container agent did not become ready")
    }
}

impl std::error::Error for AgentNotReady {}

#[derive(Debug)]
pub(in crate::gateway) struct ContainerNotFound {
    message: String,
}

impl ContainerNotFound {
    pub(in crate::gateway) fn after_start() -> Self {
        Self {
            message: "container did not exist after start".into(),
        }
    }
}

impl fmt::Display for ContainerNotFound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ContainerNotFound {}
