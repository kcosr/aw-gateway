use crate::agent_control::ProcessMatch;
use std::path::Path;

pub(super) fn read_process_table(proc_root: &Path) -> Vec<ProcInfo> {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = file_name.parse::<u32>() else {
            continue;
        };
        if let Some(process) = read_proc_info(proc_root, pid) {
            processes.push(process);
        }
    }
    processes.sort_by_key(|process| process.pid);
    processes
}

pub(super) fn read_proc_info(proc_root: &Path, pid: u32) -> Option<ProcInfo> {
    let dir = proc_root.join(pid.to_string());
    let comm = std::fs::read_to_string(dir.join("comm"))
        .ok()?
        .trim()
        .to_string();
    let start_time = read_proc_start_time(&dir);
    let status = std::fs::read_to_string(dir.join("status")).ok()?;
    let mut ppid = 0;
    let mut uid = 0;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("PPid:") {
            ppid = value.trim().parse().ok()?;
        } else if let Some(value) = line.strip_prefix("Uid:") {
            uid = value.split_whitespace().next()?.parse().ok()?;
        }
    }
    Some(ProcInfo {
        pid,
        ppid,
        uid,
        comm,
        start_time,
    })
}

fn read_proc_start_time(proc_dir: &Path) -> Option<u64> {
    let stat = std::fs::read_to_string(proc_dir.join("stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

pub(super) fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

pub(super) fn signal_number(name: &str) -> i32 {
    match name {
        "KILL" => libc::SIGKILL,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        _ => libc::SIGTERM,
    }
}

pub(super) fn signal_processes(processes: &[ProcessMatch], signal: i32) {
    for process in processes {
        signal_process(process, signal);
    }
}

pub(super) fn signal_matching_processes(processes: &[ProcessMatch], signal: i32) {
    for process in processes {
        match read_proc_info(Path::new("/proc"), process.pid) {
            Some(current) => {
                if current.comm == process.comm && current.start_time == process.start_time {
                    signal_process(process, signal);
                } else {
                    tracing::warn!(
                        pid = process.pid,
                        original_comm = process.comm,
                        current_comm = current.comm,
                        "skipping reap escalation because process identity changed"
                    );
                }
            }
            None => {
                tracing::debug!(
                    pid = process.pid,
                    comm = process.comm,
                    "skipping reap escalation because process exited"
                );
            }
        }
    }
}

fn signal_process(process: &ProcessMatch, signal: i32) {
    let rc = unsafe { libc::kill(process.pid as i32, signal) };
    if rc != 0 {
        tracing::warn!(
            pid = process.pid,
            comm = process.comm,
            error = %std::io::Error::last_os_error(),
            "failed to signal reap candidate"
        );
    } else {
        tracing::info!(
            pid = process.pid,
            comm = process.comm,
            signal,
            "signaled reap candidate"
        );
    }
}

pub(super) fn process_exists(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    err.raw_os_error() == Some(libc::EPERM)
}

#[derive(Debug, Clone)]
pub(super) struct ProcInfo {
    pub(super) pid: u32,
    pub(super) ppid: u32,
    pub(super) uid: u32,
    pub(super) comm: String,
    pub(super) start_time: Option<u64>,
}

impl From<&ProcInfo> for ProcessMatch {
    fn from(value: &ProcInfo) -> Self {
        Self {
            pid: value.pid,
            comm: value.comm.clone(),
            start_time: value.start_time,
        }
    }
}
