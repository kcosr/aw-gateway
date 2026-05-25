use tempfile::TempDir;
use toml::Value;

const GATEWAY_CLI_FIXTURE: &str = r#"
schema_version = "1"
default_target = "default"

[runtime]
type = "podman"

[logging]
level = "info"
directory = "{state}/logs/gateway"
max_bytes = 104857600
max_files = 5
console = false

[target_defaults.workspace]
path = "workspace"
state_dir = ".aw-gateway"
cleanup = "never"

[target_defaults.container_ssh.transfer]
sftp = "allow"
legacy_scp = "allow"

[ssh_dispatch]
allow_interactive_shell = true
allow_container_commands = true
enabled_actions = [
  "connect",
  "up",
  "run",
  "launches",
  "launch",
  "status",
  "targets",
  "stop",
  "remove",
  "set-default",
  "show-default",
  "reset-default",
  "add-key",
  "add-host-key",
  "add-container-key",
  "client-config",
  "client-bundle",
  "help",
]

[client_config]
inner_alias_template = "aw-{target}"
container_host_template = "aw-container-{target}"
host = "gateway.example.com"
gateway_path = "/opt/aw-gateway/bin/aw-gateway"
default_identity_dir = "~/.ssh/aw-gateway"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"
stop_when_idle = true
remove_on_stop = false
"#;

pub(crate) fn launch_config_for_test() -> &'static str {
    r#"
schema_version = "1"

[targets.default]
image = "ubuntu/dev"
mode = "fixed"
name = "{image_slug}"

[launches.agent-pack-codex]
target = "default"
description = "Clone a repo, initialize agent-pack, and run Codex."
cwd = "{container_home}/repo"
env = { AGENT_PACK_ID = "{var.pack_id}" }
command = ["codex", "exec", "--model", "{var.model}"]

[launches.agent-pack-codex.vars]
repo = { type = "string", required = true, description = "Git repository URL" }
pack_id = { type = "string", required = true }
model = { type = "enum", values = ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"], default = "gpt-5.5" }
debug = { type = "boolean", default = false }
count = { type = "number", default = 1 }

[[launches.agent-pack-codex.steps]]
phase = "post_ready"
location = "container"
name = "clone"
timeout = "5m"
cwd = "{container_home}"
command = ["git", "clone", "--branch", "main", "--single-branch", "{var.repo}", "repo"]
"#
}

pub(crate) fn gateway_sample_for_test(dir: &TempDir, workspace: &std::path::Path) -> String {
    let mut sample = GatewaySampleFixture::parse();
    sample.set_string(
        &["logging", "directory"],
        dir.path().join("logs").display().to_string(),
    );
    sample.set_string(
        &["target_defaults", "workspace", "path"],
        workspace.display().to_string(),
    );
    sample.finish()
}

pub(crate) fn gateway_sample_with_transfer_denied(
    dir: &TempDir,
    workspace: &std::path::Path,
) -> String {
    let mut sample = GatewaySampleFixture::parse_from(gateway_sample_for_test(dir, workspace));
    sample.set_string(
        &["target_defaults", "container_ssh", "transfer", "sftp"],
        "deny",
    );
    sample.set_string(
        &["target_defaults", "container_ssh", "transfer", "legacy_scp"],
        "deny",
    );
    sample.finish()
}

struct GatewaySampleFixture {
    root: Value,
}

impl GatewaySampleFixture {
    fn parse() -> Self {
        Self::parse_from(GATEWAY_CLI_FIXTURE)
    }

    fn parse_from(input: impl AsRef<str>) -> Self {
        Self {
            root: toml::from_str(input.as_ref())
                .expect("canonical gateway sample fixture should parse as TOML"),
        }
    }

    fn set_string(&mut self, path: &[&str], value: impl Into<String>) {
        let Some((field, parents)) = path.split_last() else {
            panic!("test fixture mutation path must not be empty");
        };
        let table = parents.iter().fold(&mut self.root, |current, key| {
            current
                .as_table_mut()
                .and_then(|table| table.get_mut(*key))
                .unwrap_or_else(|| {
                    panic!("gateway sample fixture missing TOML table path {path:?}")
                })
        });
        let slot = table
            .as_table_mut()
            .and_then(|table| table.get_mut(*field))
            .unwrap_or_else(|| panic!("gateway sample fixture missing TOML field path {path:?}"));
        assert!(
            slot.is_str(),
            "gateway sample fixture TOML path {path:?} should contain a string"
        );
        *slot = Value::String(value.into());
    }

    fn finish(self) -> String {
        toml::to_string_pretty(&self.root)
            .expect("mutated gateway sample fixture should serialize as TOML")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_sample_fixture_mutates_named_paths() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("workspace");

        let text = gateway_sample_for_test(&dir, &workspace);
        let parsed: Value = toml::from_str(&text).unwrap();

        assert_eq!(
            parsed["logging"]["directory"].as_str(),
            Some(dir.path().join("logs").to_str().unwrap())
        );
        assert_eq!(
            parsed["target_defaults"]["workspace"]["path"].as_str(),
            Some(workspace.to_str().unwrap())
        );
    }

    #[test]
    fn transfer_denied_fixture_mutates_policy_fields() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("workspace");

        let text = gateway_sample_with_transfer_denied(&dir, &workspace);
        let parsed: Value = toml::from_str(&text).unwrap();

        let transfer = &parsed["target_defaults"]["container_ssh"]["transfer"];
        assert_eq!(transfer["sftp"].as_str(), Some("deny"));
        assert_eq!(transfer["legacy_scp"].as_str(), Some("deny"));
    }
}
