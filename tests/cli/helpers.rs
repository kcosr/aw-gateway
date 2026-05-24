use tempfile::TempDir;

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
    include_str!("../../aw-gateway.sample.toml")
        .replace(
            "directory = \"{state}/logs/gateway\"",
            &format!("directory = \"{}\"", dir.path().join("logs").display()),
        )
        .replace(
            "path = \"workspace\"",
            &format!("path = \"{}\"", workspace.display()),
        )
}
