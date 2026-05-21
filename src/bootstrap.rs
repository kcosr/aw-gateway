use crate::cli::BootstrapArgs;
use crate::config::{ContainerBootstrapFile, RenderedContainerBootstrapStep, parse_duration};
use anyhow::Context;
use std::ffi::CStr;
use std::ffi::CString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

pub fn run(args: BootstrapArgs) -> anyhow::Result<()> {
    let cfg =
        ContainerBootstrapFile::load(&crate::paths::bootstrap_config_path(args.bootstrap_config))?;
    if !cfg.skip_identity_prepare {
        prepare_identity(&cfg)?;
    }
    unsafe {
        libc::endpwent();
        libc::endgrent();
    }
    for step in &cfg.steps {
        run_step(step, &cfg)?;
    }
    exec_agent(&cfg)
}

fn prepare_identity(cfg: &ContainerBootstrapFile) -> anyhow::Result<()> {
    ensure_group(&cfg.identity.session_user, cfg.identity.session_gid)?;
    ensure_passwd_entry(
        &cfg.identity.session_user,
        cfg.identity.session_uid,
        cfg.identity.session_gid,
        &cfg.identity.session_home,
        &cfg.identity.session_shell,
    )?;
    ensure_dir_owned_if_created(
        Path::new(&cfg.identity.session_home),
        cfg.identity.session_uid,
        cfg.identity.session_gid,
    )?;
    ensure_dir_owned_if_created(
        Path::new(&cfg.identity.state_dir),
        cfg.identity.session_uid,
        cfg.identity.session_gid,
    )?;
    Ok(())
}

fn ensure_group(name: &str, gid: u32) -> anyhow::Result<()> {
    ensure_group_at(Path::new("/etc/group"), name, gid)
}

fn ensure_group_at(path: &Path, name: &str, gid: u32) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path).context("read /etc/group")?;
    for line in raw.lines() {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() >= 3 {
            if fields[0] == name {
                let existing_gid = fields[2].parse::<u32>().unwrap_or(u32::MAX);
                if existing_gid != gid {
                    anyhow::bail!("group {name:?} exists with gid {existing_gid}, expected {gid}");
                }
                return Ok(());
            }
            if fields[2] == gid.to_string() {
                anyhow::bail!(
                    "gid {gid} already used by group {:?}; session_gid must be unique or session group must match",
                    fields[0]
                );
            }
        }
    }
    let mut updated = raw;
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("{name}:x:{gid}:\n"));
    atomic_write_preserve_mode(path, updated.as_bytes()).context("write /etc/group")?;
    Ok(())
}

fn ensure_passwd_entry(
    name: &str,
    uid: u32,
    gid: u32,
    home: &str,
    shell: &str,
) -> anyhow::Result<()> {
    ensure_passwd_entry_at(Path::new("/etc/passwd"), name, uid, gid, home, shell)
}

fn ensure_passwd_entry_at(
    path: &Path,
    name: &str,
    uid: u32,
    gid: u32,
    home: &str,
    shell: &str,
) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path).context("read /etc/passwd")?;
    for line in raw.lines() {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() >= 7 && fields[0] == name {
            let existing_uid = fields[2].parse::<u32>().unwrap_or(u32::MAX);
            let existing_gid = fields[3].parse::<u32>().unwrap_or(u32::MAX);
            if existing_uid != uid || existing_gid != gid {
                anyhow::bail!(
                    "user {name:?} exists with uid:gid {existing_uid}:{existing_gid}, expected {uid}:{gid}"
                );
            }
            return Ok(());
        }
        if fields.len() >= 7 && fields[2] == uid.to_string() {
            anyhow::bail!(
                "uid {uid} already used by user {:?}; session_uid must be unique or session_user must match",
                fields[0]
            );
        }
    }
    let mut updated = raw;
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("{name}:x:{uid}:{gid}:{name}:{home}:{shell}\n"));
    atomic_write_preserve_mode(path, updated.as_bytes()).context("write /etc/passwd")?;
    Ok(())
}

fn ensure_dir_owned_if_created(path: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    if path
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        anyhow::bail!("refusing to chown symlink {}", path.display());
    }
    std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    chown(path, uid, gid).with_context(|| format!("chown {}", path.display()))?;
    Ok(())
}

fn atomic_write_preserve_mode(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mode = metadata.permissions().mode();
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid file name {}", path.display()))?;
    let tmp = parent.join(format!(".{file_name}.aw-tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("open {}", tmp.display()))?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))?;
        if let Ok(parent_file) = std::fs::File::open(parent) {
            let _ = parent_file.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn chown(path: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path contains NUL: {}", path.display()))?;
    let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("chown");
    }
    Ok(())
}

fn run_step(
    step: &RenderedContainerBootstrapStep,
    cfg: &ContainerBootstrapFile,
) -> anyhow::Result<()> {
    let mut command = Command::new(&step.command[0]);
    command.args(&step.command[1..]);
    command.env_clear();
    command.env(
        "PATH",
        std::env::var("PATH")
            .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into()),
    );
    command.env("HOME", step_home(&step.user, cfg)?);
    if step.user != "root" {
        let identity = resolve_user(&step.user)?;
        let user = CString::new(step.user.clone())
            .with_context(|| format!("bootstrap user {:?} contains NUL", step.user))?;
        unsafe {
            command.pre_exec(move || {
                #[cfg(target_os = "macos")]
                let base_gid = libc::c_int::try_from(identity.gid).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "gid exceeds c_int")
                })?;
                #[cfg(not(target_os = "macos"))]
                let base_gid = identity.gid as libc::gid_t;
                if libc::initgroups(user.as_ptr(), base_gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgid(identity.gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(identity.uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let status = run_with_timeout(
        command,
        step.timeout
            .as_deref()
            .map(parse_duration)
            .transpose()?
            .unwrap_or_else(|| Duration::from_secs(60)),
    )
    .with_context(|| format!("run bootstrap step {:?}", step.name))?;
    if !status.success() {
        let message = format!("bootstrap step {:?} failed with {status}", step.name);
        if step.required {
            anyhow::bail!("{message}");
        }
        tracing::warn!("{message}");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct UserIdentity {
    uid: u32,
    gid: u32,
}

#[derive(Debug, Clone)]
struct UserHome {
    home: String,
}

fn resolve_user(name: &str) -> anyhow::Result<UserIdentity> {
    let c_name = CString::new(name).context("user contains NUL byte")?;
    unsafe {
        libc::endpwent();
        libc::endgrent();
        let pw = libc::getpwnam(c_name.as_ptr());
        if pw.is_null() {
            anyhow::bail!("user {name:?} does not exist");
        }
        Ok(UserIdentity {
            uid: (*pw).pw_uid,
            gid: (*pw).pw_gid,
        })
    }
}

fn resolve_user_home(name: &str) -> anyhow::Result<UserHome> {
    let c_name = CString::new(name).context("user contains NUL byte")?;
    unsafe {
        libc::endpwent();
        let pw = libc::getpwnam(c_name.as_ptr());
        if pw.is_null() {
            anyhow::bail!("user {name:?} does not exist");
        }
        Ok(UserHome {
            home: CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned(),
        })
    }
}

fn step_home(user: &str, cfg: &ContainerBootstrapFile) -> anyhow::Result<String> {
    if user == "root" {
        return Ok("/root".into());
    }
    if user == cfg.identity.session_user {
        return Ok(cfg.identity.session_home.clone());
    }
    Ok(resolve_user_home(user)?.home)
}

fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> anyhow::Result<std::process::ExitStatus> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("bootstrap step timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn exec_agent(cfg: &ContainerBootstrapFile) -> anyhow::Result<()> {
    let err = Command::new(&cfg.agent_program)
        .arg("--config")
        .arg(&cfg.agent_config)
        .arg("run")
        .exec();
    Err(err).with_context(|| format!("exec {}", cfg.agent_program))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_group_rejects_gid_collision_with_different_name() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path().join("group");
        std::fs::write(&group, "root:x:0:\nwheel:x:2450:\n").unwrap();

        let err = ensure_group_at(&group, "awuser", 2450).unwrap_err();
        assert!(err.to_string().contains("gid 2450 already used"));
    }

    #[test]
    fn ensure_group_accepts_matching_existing_group() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path().join("group");
        std::fs::write(&group, "root:x:0:\nawuser:x:2450:\n").unwrap();

        ensure_group_at(&group, "awuser", 2450).unwrap();
        assert_eq!(
            std::fs::read_to_string(&group).unwrap(),
            "root:x:0:\nawuser:x:2450:\n"
        );
    }

    #[test]
    fn ensure_group_appends_group_atomically_preserving_mode() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path().join("group");
        std::fs::write(&group, "root:x:0:\n").unwrap();
        std::fs::set_permissions(&group, std::fs::Permissions::from_mode(0o644)).unwrap();

        ensure_group_at(&group, "awuser", 2450).unwrap();

        let raw = std::fs::read_to_string(&group).unwrap();
        assert!(raw.contains("awuser:x:2450:"));
        assert_eq!(
            std::fs::metadata(&group).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn ensure_passwd_rejects_uid_collision_with_different_name() {
        let dir = tempfile::tempdir().unwrap();
        let passwd = dir.path().join("passwd");
        std::fs::write(
            &passwd,
            "root:x:0:0:root:/root:/bin/bash\nother:x:2450:2450:other:/home/other:/bin/bash\n",
        )
        .unwrap();

        let err =
            ensure_passwd_entry_at(&passwd, "awuser", 2450, 2450, "/home/awuser", "/bin/bash")
                .unwrap_err();
        assert!(err.to_string().contains("uid 2450 already used"));
    }

    #[test]
    fn ensure_passwd_appends_user_atomically_preserving_mode() {
        let dir = tempfile::tempdir().unwrap();
        let passwd = dir.path().join("passwd");
        std::fs::write(&passwd, "root:x:0:0:root:/root:/bin/bash\n").unwrap();
        std::fs::set_permissions(&passwd, std::fs::Permissions::from_mode(0o644)).unwrap();

        ensure_passwd_entry_at(&passwd, "awuser", 2450, 2450, "/home/awuser", "/bin/bash").unwrap();

        let raw = std::fs::read_to_string(&passwd).unwrap();
        assert!(raw.contains("awuser:x:2450:2450:awuser:/home/awuser:/bin/bash"));
        assert_eq!(
            std::fs::metadata(&passwd).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn run_with_timeout_kills_hung_step() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        let err = run_with_timeout(command, Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }
}
