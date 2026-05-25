use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetContainerSshConfig {
    pub transfer: Option<TargetContainerSshTransferConfig>,
}

impl Default for TargetContainerSshConfig {
    fn default() -> Self {
        Self {
            transfer: Some(TargetContainerSshTransferConfig {
                sftp: Some(SftpTransferMode::Allow),
                legacy_scp: Some(LegacyScpTransferMode::Allow),
            }),
        }
    }
}

impl TargetContainerSshConfig {
    fn overlay(mut self, later: &Self) -> Self {
        if let Some(transfer) = &later.transfer {
            self.transfer = Some(self.transfer.take().unwrap_or_default().overlay(transfer));
        }
        self
    }

    pub(in crate::config) fn to_effective_config(&self) -> anyhow::Result<ContainerSshConfig> {
        let transfer = match &self.transfer {
            Some(transfer) => transfer.to_effective()?,
            None => TargetContainerSshTransferConfig::default().to_effective()?,
        };
        Ok(ContainerSshConfig { transfer })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetContainerSshTransferConfig {
    pub sftp: Option<SftpTransferMode>,
    pub legacy_scp: Option<LegacyScpTransferMode>,
}

impl Default for TargetContainerSshTransferConfig {
    fn default() -> Self {
        Self {
            sftp: Some(SftpTransferMode::Allow),
            legacy_scp: Some(LegacyScpTransferMode::Allow),
        }
    }
}

impl TargetContainerSshTransferConfig {
    fn overlay(mut self, later: &Self) -> Self {
        if let Some(sftp) = later.sftp {
            self.sftp = Some(sftp);
        }
        if let Some(legacy_scp) = later.legacy_scp {
            self.legacy_scp = Some(legacy_scp);
        }
        self
    }

    fn to_effective(&self) -> anyhow::Result<ContainerSshTransferConfig> {
        let sftp = self
            .sftp
            .ok_or_else(|| anyhow::anyhow!("target container_ssh.transfer.sftp is required"))?;
        let legacy_scp = self.legacy_scp.ok_or_else(|| {
            anyhow::anyhow!("target container_ssh.transfer.legacy_scp is required")
        })?;
        Ok(ContainerSshTransferConfig { sftp, legacy_scp })
    }
}

pub(super) fn overlay_target_container_ssh(
    current: Option<TargetContainerSshConfig>,
    later: &TargetContainerSshConfig,
) -> TargetContainerSshConfig {
    current.unwrap_or_default().overlay(later)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContainerSshConfig {
    #[serde(default)]
    pub transfer: ContainerSshTransferConfig,
}

impl ContainerSshConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContainerSshTransferConfig {
    #[serde(default)]
    pub sftp: SftpTransferMode,
    #[serde(default)]
    pub legacy_scp: LegacyScpTransferMode,
}

impl Default for ContainerSshTransferConfig {
    fn default() -> Self {
        Self {
            sftp: SftpTransferMode::Allow,
            legacy_scp: LegacyScpTransferMode::Allow,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SftpTransferMode {
    #[default]
    Allow,
    Deny,
}

impl SftpTransferMode {
    pub fn allows(self) -> bool {
        matches!(self, Self::Allow)
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LegacyScpTransferMode {
    #[default]
    Allow,
    Deny,
    Inbound,
    Outbound,
}

impl LegacyScpTransferMode {
    pub fn allows_inbound(self) -> bool {
        matches!(self, Self::Allow | Self::Inbound)
    }

    pub fn allows_outbound(self) -> bool {
        matches!(self, Self::Allow | Self::Outbound)
    }
}
