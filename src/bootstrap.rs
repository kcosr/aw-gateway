use crate::cli::BootstrapArgs;
use crate::config::{ContainerBootstrapFile, RenderedContainerBootstrapStep, parse_duration};
use crate::fileutil::{self, AtomicWritePolicy};
use crate::unix_account::passwd_by_name;
use anyhow::Context;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
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
        cfg.chown_existing_identity_dirs,
    )?;
    ensure_dir_owned_if_created(
        Path::new(&cfg.identity.state_dir),
        cfg.identity.session_uid,
        cfg.identity.session_gid,
        cfg.chown_existing_identity_dirs,
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
            if group_gid_collision_policy_allows_existing_gid(fields[2], gid) {
                return Ok(());
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

fn group_gid_collision_policy_allows_existing_gid(existing_gid: &str, requested_gid: u32) -> bool {
    // A session gid may already exist under a different group name in base
    // images. Reusing that gid preserves host identity mapping without adding
    // an alias group.
    existing_gid.parse::<u32>().ok() == Some(requested_gid)
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

fn ensure_dir_owned_if_created(
    path: &Path,
    uid: u32,
    gid: u32,
    chown_existing: bool,
) -> anyhow::Result<()> {
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!("refusing to chown symlink {}", path.display());
        }
        Ok(meta) if meta.is_dir() => {
            if chown_existing {
                chown_dir_no_follow(path, uid, gid)
                    .with_context(|| format!("chown {}", path.display()))?;
            } else if meta.uid() != uid || meta.gid() != gid {
                tracing::warn!(
                    path = %path.display(),
                    owner_uid = meta.uid(),
                    owner_gid = meta.gid(),
                    requested_uid = uid,
                    requested_gid = gid,
                    "pre-existing identity directory ownership differs; leaving ownership unchanged",
                );
            }
            return Ok(());
        }
        Ok(_) => {
            anyhow::bail!("{} exists but is not a directory", path.display());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("stat {}", path.display()));
        }
    }
    std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    chown_dir_no_follow(path, uid, gid).with_context(|| format!("chown {}", path.display()))?;
    Ok(())
}

fn atomic_write_preserve_mode(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    fileutil::atomic_write_file(
        path,
        contents,
        AtomicWritePolicy::preserve_existing_with_full_durability(),
    )
}

fn chown_dir_no_follow(path: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path contains NUL: {}", path.display()))?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open directory");
    }
    let dir = unsafe { std::fs::File::from_raw_fd(fd) };
    let rc = unsafe { libc::fchown(dir.as_raw_fd(), uid, gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("fchown");
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
        let groups = crate::unix_priv::resolve_user_groups(&user, identity.gid)
            .with_context(|| format!("resolve groups for bootstrap user {:?}", step.user))?;
        unsafe {
            command.pre_exec(move || {
                crate::unix_priv::drop_to_user_pre_exec(identity.uid, identity.gid, &groups)
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
    let passwd = passwd_by_name(name)?;
    Ok(UserIdentity {
        uid: passwd.uid,
        gid: passwd.gid,
    })
}

fn resolve_user_home(name: &str) -> anyhow::Result<UserHome> {
    let passwd = passwd_by_name(name)?;
    let home = passwd
        .home
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("home directory for user {name:?} is not valid UTF-8"))?;
    Ok(UserHome { home })
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
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn ensure_group_allows_gid_collision_with_different_name() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path().join("group");
        std::fs::write(&group, "root:x:0:\nwheel:x:2450:\n").unwrap();

        ensure_group_at(&group, "awuser", 2450).unwrap();

        let raw = std::fs::read_to_string(&group).unwrap();
        assert_eq!(raw, "root:x:0:\nwheel:x:2450:\n");
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
    fn ensure_group_allows_existing_gid_with_different_name() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path().join("group");
        std::fs::write(&group, "root:x:0:\ndialout:x:20:\n").unwrap();

        ensure_group_at(&group, "staff", 20).unwrap();

        let raw = std::fs::read_to_string(&group).unwrap();
        assert_eq!(raw, "root:x:0:\ndialout:x:20:\n");
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
    fn ensure_dir_owned_if_created_can_leave_existing_directory_ownership_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing");
        std::fs::create_dir(&path).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let requested_uid = if before.uid() == 0 { 1 } else { 0 };
        let requested_gid = if before.gid() == 0 { 1 } else { 0 };

        ensure_dir_owned_if_created(&path, requested_uid, requested_gid, false).unwrap();

        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(after.uid(), before.uid());
        assert_eq!(after.gid(), before.gid());
    }

    #[test]
    fn ensure_dir_owned_if_created_creates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("created");
        let owner = std::fs::metadata(dir.path()).unwrap();

        ensure_dir_owned_if_created(&path, owner.uid(), owner.gid(), true).unwrap();

        let created = std::fs::metadata(&path).unwrap();
        assert!(created.is_dir());
        assert_eq!(created.uid(), owner.uid());
        assert_eq!(created.gid(), owner.gid());
    }

    #[test]
    fn ensure_dir_owned_if_created_rejects_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "not a directory").unwrap();

        let err = ensure_dir_owned_if_created(&path, 0, 0, true).unwrap_err();

        assert!(err.to_string().contains("exists but is not a directory"));
    }

    #[test]
    #[cfg(unix)]
    fn ensure_dir_owned_if_created_rejects_existing_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = ensure_dir_owned_if_created(&link, 0, 0, true).unwrap_err();

        assert!(err.to_string().contains("refusing to chown symlink"));
    }

    #[test]
    fn run_with_timeout_kills_hung_step() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        let err = run_with_timeout(command, Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }
}
