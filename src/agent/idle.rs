use crate::agent_control::{IdleStateName, ProcessMatch, ReapResult};
use crate::config::{IdleCleanupAction, IdleCleanupConfig, parse_duration};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::time::{Duration, Instant, sleep};

use super::process::{
    ProcInfo, current_uid, read_process_table, signal_matching_processes, signal_number,
    signal_processes,
};
use super::service::stop_services;
use super::state::AgentState;

pub(super) async fn run_idle_cleanup(state: Arc<AgentState>) {
    let Some(config) = state.idle_cleanup.clone() else {
        return;
    };
    let poll_interval = config
        .poll_interval
        .as_deref()
        .and_then(|value| parse_duration(value).ok())
        .unwrap_or(Duration::from_secs(30));
    let idle_grace = config
        .idle_grace
        .as_deref()
        .and_then(|value| parse_duration(value).ok())
        .unwrap_or(Duration::from_secs(300));

    loop {
        if state.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let transition = evaluate_idle_state(&state, &config, idle_grace).await;
        match transition {
            IdleTransition::None => {}
            IdleTransition::ShutdownContainer => {
                state.shutting_down.store(true, Ordering::SeqCst);
                state.accepting_bridge.store(false, Ordering::SeqCst);
                stop_services(&state).await;
                std::process::exit(0);
            }
            IdleTransition::ReapProcesses => {
                let managed = managed_service_pids(&state).await;
                let dry_run = std::env::var("AW_CONTAINER_AGENT_ALLOW_PROCESS_REAP")
                    .ok()
                    .as_deref()
                    != Some("1");
                let result = run_reap_processes(&config, &managed, dry_run).await;
                let mut idle = state.idle_state.lock().await;
                idle.state = IdleStateName::ReapUnpreservedProcesses;
                idle.last_reap_result = Some(result);
                idle.idle_since = Some(Instant::now());
            }
        }
        sleep(poll_interval).await;
    }
}

enum IdleTransition {
    None,
    ShutdownContainer,
    ReapProcesses,
}

async fn evaluate_idle_state(
    state: &AgentState,
    config: &IdleCleanupConfig,
    idle_grace: Duration,
) -> IdleTransition {
    let active_streams = state.active_streams.load(Ordering::SeqCst);
    let active_sessions = state.active_sessions.load(Ordering::SeqCst);
    let process_table = read_process_table(Path::new("/proc"));
    let matched_processes = find_preserve_processes(&process_table, &config.preserve_processes);
    let preserve = !matched_processes.is_empty();
    let now = Instant::now();
    let mut idle = state.idle_state.lock().await;

    if active_streams > 0 || active_sessions > 0 {
        idle.state = IdleStateName::Attached;
        idle.idle_since = None;
        idle.preserve = false;
        idle.preserve_reason = None;
        idle.matched_processes.clear();
        return IdleTransition::None;
    }

    if preserve {
        idle.state = IdleStateName::Preserved;
        idle.idle_since = None;
        idle.preserve = true;
        idle.preserve_reason = matched_processes
            .first()
            .map(|process| format!("process:{}", process.comm));
        idle.matched_processes = matched_processes;
        return IdleTransition::None;
    }

    let idle_since = *idle.idle_since.get_or_insert(now);
    idle.state = IdleStateName::IdlePending;
    idle.preserve = false;
    idle.preserve_reason = None;
    idle.matched_processes.clear();

    if now.duration_since(idle_since) < idle_grace {
        return IdleTransition::None;
    }

    match config.action {
        IdleCleanupAction::ExitContainer => {
            idle.state = IdleStateName::ShutdownContainer;
            IdleTransition::ShutdownContainer
        }
        IdleCleanupAction::ReapProcesses => IdleTransition::ReapProcesses,
        IdleCleanupAction::None => IdleTransition::None,
    }
}

fn find_preserve_processes(processes: &[ProcInfo], names: &[String]) -> Vec<ProcessMatch> {
    if names.is_empty() {
        return Vec::new();
    }
    let names: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    let mut matches: Vec<_> = processes
        .iter()
        .filter(|process| names.contains(process.comm.as_str()))
        .map(|process| ProcessMatch {
            pid: process.pid,
            comm: process.comm.clone(),
            start_time: process.start_time,
        })
        .collect();
    matches.sort_by_key(|process| process.pid);
    matches
}

pub(super) fn reap_processes(
    config: &IdleCleanupConfig,
    managed_pids: &BTreeSet<u32>,
    dry_run: bool,
) -> ReapResult {
    let processes = read_process_table(Path::new("/proc"));
    let plan = build_reap_plan(
        &processes,
        config,
        managed_pids,
        current_uid(),
        std::process::id(),
    );
    if !dry_run {
        signal_processes(&plan.would_terminate, signal_number(&config.reap_signal));
    }
    ReapResult { dry_run, ..plan }
}

pub(super) async fn run_reap_processes(
    config: &IdleCleanupConfig,
    managed_pids: &BTreeSet<u32>,
    dry_run: bool,
) -> ReapResult {
    let result = reap_processes(config, managed_pids, dry_run);
    if !dry_run
        && signal_number(&config.reap_signal) != libc::SIGKILL
        && let Some(delay) = config
            .reap_kill_after
            .as_deref()
            .and_then(|value| parse_duration(value).ok())
    {
        let candidates = result.would_terminate.clone();
        tokio::spawn(async move {
            sleep(delay).await;
            signal_matching_processes(&candidates, libc::SIGKILL);
        });
    }
    result
}

pub(super) fn build_reap_plan(
    processes: &[ProcInfo],
    config: &IdleCleanupConfig,
    managed_pids: &BTreeSet<u32>,
    agent_uid: u32,
    agent_pid: u32,
) -> ReapResult {
    let preserve_roots = find_preserve_processes(processes, &config.preserve_processes);
    let mut preserved_pids: BTreeSet<u32> =
        preserve_roots.iter().map(|process| process.pid).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for process in processes {
            if preserved_pids.contains(&process.ppid) && preserved_pids.insert(process.pid) {
                changed = true;
            }
        }
    }

    let mut preserved: Vec<_> = processes
        .iter()
        .filter(|process| preserved_pids.contains(&process.pid))
        .map(ProcessMatch::from)
        .collect();
    preserved.sort_by_key(|process| process.pid);

    let mut would_terminate: Vec<_> = processes
        .iter()
        .filter(|process| process.pid != 1)
        .filter(|process| process.pid != agent_pid)
        .filter(|process| !managed_pids.contains(&process.pid))
        .filter(|process| !preserved_pids.contains(&process.pid))
        .filter(|process| process.uid != 0 || agent_uid != 0)
        .filter(|process| process.uid == agent_uid || agent_uid == 0)
        .map(ProcessMatch::from)
        .collect();
    would_terminate.sort_by_key(|process| process.pid);

    ReapResult {
        dry_run: true,
        would_terminate,
        preserved,
    }
}

pub(super) async fn managed_service_pids(state: &AgentState) -> BTreeSet<u32> {
    let services = state.services.lock().await.clone();
    let mut pids = BTreeSet::new();
    for service in services {
        if let Some(child) = service.child.lock().await.as_ref()
            && let Some(pid) = child.id()
        {
            pids.insert(pid);
        }
    }
    pids
}
