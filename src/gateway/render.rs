use super::model::{
    AllStatusEntry, GatewayStatus, LaunchDetail, LaunchSummary, LaunchVarMetadata, ReadyStatus,
    TargetEntry,
};
use super::ops::{RemoveResult, StopResult};
use std::collections::BTreeMap;

pub(super) fn render_up_result(ready: ReadyStatus) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(&ready)?);
    Ok(())
}

pub(super) fn render_stop_result(result: &StopResult) {
    if result.stopped {
        println!("{}", stop_result_text(result));
    } else {
        println!("not running");
    }
}

pub(super) fn stop_result_text(result: &StopResult) -> String {
    format!("stopped {}", result.container)
}

pub(super) fn render_remove_result(result: &RemoveResult) {
    if result.removed {
        println!("{}", remove_result_text(result));
    } else {
        println!("not found");
    }
}

pub(super) fn remove_result_text(result: &RemoveResult) -> String {
    format!("removed {}", result.container)
}

pub(super) fn render_default_selection(selection: &str) {
    println!("{selection}");
}

pub(super) fn render_status_result(result: GatewayStatus, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "{}: {} ({})",
            result.target,
            result.status,
            result.container.unwrap_or_else(|| "not-created".into())
        );
        if let Some(launch) = &result.launch {
            println!("launch: {launch}");
        }
    }
    Ok(())
}

pub(super) fn render_status_all(summaries: Vec<AllStatusEntry>, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&summaries)?);
    } else if summaries.is_empty() {
        println!("No aw-gateway-managed containers found for this user.");
    } else {
        println!(
            "{:<15} {:<11} {:<16} {:<11} {:<22} STATUS",
            "TARGET", "SESSION", "LAUNCH", "MODE", "CONTAINER"
        );
        for entry in summaries {
            println!(
                "{:<15} {:<11} {:<16} {:<11} {:<22} {}",
                entry.target,
                entry.session_id.as_deref().unwrap_or("-"),
                entry.launch.as_deref().unwrap_or("-"),
                entry.mode,
                entry.container,
                entry.status
            );
        }
    }
    Ok(())
}

pub(super) fn render_targets(entries: Vec<TargetEntry>, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        println!("{:<24} {:<24} {:<10} CONTAINER", "TARGET", "IMAGE", "MODE");
        for entry in entries {
            let default_marker = if entry.default { " *" } else { "" };
            println!(
                "{:<24} {:<24} {:<10} {}{}",
                entry.target, entry.image, entry.mode, entry.container, default_marker
            );
        }
    }
    Ok(())
}

pub(super) fn render_launches(entries: Vec<LaunchSummary>, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if entries.is_empty() {
        println!("No launches configured.");
    } else {
        println!(
            "{:<24} {:<24} {:<25} DESCRIPTION",
            "LAUNCH", "TARGET", "REQUIRED VARS"
        );
        for entry in entries {
            let required = entry
                .vars
                .iter()
                .filter_map(|(name, var)| var.required.then_some(name.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "{:<24} {:<24} {:<25} {}",
                entry.name,
                entry.target,
                required,
                entry.description.unwrap_or_default()
            );
        }
    }
    Ok(())
}

pub(super) fn render_launch_detail(detail: LaunchDetail, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
    } else {
        print_launch_detail(&detail);
    }
    Ok(())
}

fn print_launch_detail(detail: &LaunchDetail) {
    println!("Launch: {}", detail.name);
    println!("Target: {}", detail.target);
    println!("Target mode: {}", detail.target_mode);
    println!(
        "Passthrough args: {}",
        if detail.allow_args { "yes" } else { "no" }
    );
    if let Some(container) = &detail.target_container {
        println!("Target container: {container}");
    }
    if let Some(description) = &detail.description {
        println!("Description: {description}");
    }
    if !detail.vars.is_empty() {
        println!("\nVariables:");
        for (name, var) in &detail.vars {
            println!(
                "  {name} ({}){}",
                launch_var_text(var),
                launch_var_description(var)
            );
        }
    }
    if !detail.steps.is_empty() {
        println!("\nSteps:");
        for (index, step) in detail.steps.iter().enumerate() {
            let required = if step.required {
                "required"
            } else {
                "optional"
            };
            let timeout = step
                .timeout
                .as_deref()
                .map(|value| format!(", timeout: {value}"))
                .unwrap_or_default();
            println!(
                "  {}. {} [{}/{}, {}{}]",
                index + 1,
                step.name,
                step.phase,
                step.location,
                required,
                timeout
            );
            if let Some(cwd) = &step.cwd {
                println!("     cwd: {cwd}");
            }
            if !step.env.is_empty() {
                println!("     env: {}", env_summary(&step.env));
            }
            println!("     argv: {}", step.command.join(" "));
        }
    }
    println!("\nCommand:");
    if let Some(cwd) = &detail.cwd {
        println!("  cwd: {cwd}");
    }
    if !detail.env.is_empty() {
        println!("  env: {}", env_summary(&detail.env));
    }
    println!("  argv: {}", detail.command.join(" "));
}

fn launch_var_text(var: &LaunchVarMetadata) -> String {
    let mut parts = Vec::new();
    match (var.var_type, &var.values) {
        ("enum", Some(values)) => parts.push(format!("enum: {}", values.join(", "))),
        (var_type, _) => parts.push(var_type.to_string()),
    }
    if var.required {
        parts.push("required".into());
    } else if let Some(default) = &var.default {
        parts.push(format!("default: {}", default.rendered()));
    }
    parts.join(", ")
}

fn launch_var_description(var: &LaunchVarMetadata) -> String {
    var.description
        .as_deref()
        .map(|description| format!(" - {description}"))
        .unwrap_or_default()
}

fn env_summary(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}
