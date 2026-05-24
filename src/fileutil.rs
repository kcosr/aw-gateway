use anyhow::Context;
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub(crate) enum FileModePolicy {
    Fixed(u32),
    PreserveExisting,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DurabilityPolicy {
    pub(crate) fsync_file: bool,
    pub(crate) fsync_parent_dir: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AtomicWritePolicy {
    pub(crate) mode: FileModePolicy,
    pub(crate) durability: DurabilityPolicy,
}

impl AtomicWritePolicy {
    pub(crate) const fn new(mode: FileModePolicy, durability: DurabilityPolicy) -> Self {
        Self { mode, durability }
    }
}

pub(crate) fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    set_mode(path, 0o700)
}

pub(crate) fn remove_if_exists(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

pub(crate) fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {:o} {}", mode, path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

pub(crate) fn write_private_file(path: &Path, contents: &[u8], mode: u32) -> anyhow::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write {}", path.display()))?;
    set_mode(path, mode)
}

pub(crate) fn atomic_write_file(
    path: &Path,
    contents: &[u8],
    policy: AtomicWritePolicy,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("{} has invalid file name", path.display()))?;
    let mode = resolve_file_mode(path, policy.mode)?;
    let temp = temp_path(parent, name)?;
    let result = (|| -> anyhow::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options
            .open(&temp)
            .with_context(|| format!("open {}", temp.display()))?;
        set_mode(&temp, mode)?;
        file.write_all(contents)
            .with_context(|| format!("write {}", temp.display()))?;
        if policy.durability.fsync_file {
            file.sync_all()
                .with_context(|| format!("fsync {}", temp.display()))?;
        }
        drop(file);
        std::fs::rename(&temp, path)
            .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
        if policy.durability.fsync_parent_dir
            && let Ok(parent_file) = std::fs::File::open(parent)
        {
            let _ = parent_file.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

pub(crate) fn atomic_write_toml<T: Serialize>(
    path: &Path,
    value: &T,
    policy: AtomicWritePolicy,
) -> anyhow::Result<()> {
    let raw = toml::to_string_pretty(value)?;
    atomic_write_file(path, raw.as_bytes(), policy)
}

fn resolve_file_mode(path: &Path, policy: FileModePolicy) -> anyhow::Result<u32> {
    match policy {
        FileModePolicy::Fixed(mode) => Ok(mode),
        FileModePolicy::PreserveExisting => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let metadata =
                    std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
                Ok(metadata.permissions().mode())
            }
            #[cfg(not(unix))]
            {
                std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
                Ok(0)
            }
        }
    }
}

fn temp_path(parent: &Path, name: &str) -> anyhow::Result<PathBuf> {
    Ok(parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        random_hex_suffix()?
    )))
}

fn random_hex_suffix() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut bytes)
        .context("read /dev/urandom")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn file_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_file_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn ensure_private_dir_sets_private_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");

        ensure_private_dir(&path).unwrap();

        assert!(path.is_dir());
        #[cfg(unix)]
        assert_eq!(file_mode(&path), 0o700);
    }

    #[test]
    fn write_private_file_truncates_and_applies_fixed_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, "old contents").unwrap();
        #[cfg(unix)]
        set_file_mode(&path, 0o644);

        write_private_file(&path, b"new", 0o600).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        #[cfg(unix)]
        assert_eq!(file_mode(&path), 0o600);
    }

    #[test]
    fn atomic_write_fixed_mode_replaces_file_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        set_file_mode(&path, 0o644);

        atomic_write_file(&path, b"new", fixed_no_fsync(0o600)).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        #[cfg(unix)]
        assert_eq!(file_mode(&path), 0o600);
        let temp_count = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".config.toml.")
            })
            .count();
        assert_eq!(temp_count, 0);
    }

    #[test]
    fn atomic_write_preserve_existing_mode_keeps_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passwd");
        std::fs::write(&path, "root:x:0:0\n").unwrap();
        #[cfg(unix)]
        set_file_mode(&path, 0o640);

        atomic_write_file(
            &path,
            b"root:x:0:0\nawuser:x:2450:2450\n",
            AtomicWritePolicy::new(
                FileModePolicy::PreserveExisting,
                DurabilityPolicy {
                    fsync_file: true,
                    fsync_parent_dir: true,
                },
            ),
        )
        .unwrap();

        assert!(std::fs::read_to_string(&path).unwrap().contains("awuser"));
        #[cfg(unix)]
        assert_eq!(file_mode(&path), 0o640);
    }

    #[test]
    fn atomic_write_preserve_existing_mode_requires_destination() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing");

        let err = atomic_write_file(
            &path,
            b"contents",
            AtomicWritePolicy::new(
                FileModePolicy::PreserveExisting,
                DurabilityPolicy {
                    fsync_file: false,
                    fsync_parent_dir: false,
                },
            ),
        )
        .unwrap_err();

        assert!(err.to_string().contains("stat"));
        assert!(!path.exists());
    }

    #[test]
    fn atomic_write_toml_serializes_pretty_and_applies_mode() {
        #[derive(Serialize)]
        struct Fixture {
            enabled: bool,
            name: String,
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fixture = Fixture {
            enabled: true,
            name: "default".into(),
        };

        atomic_write_toml(&path, &fixture, fixed_no_fsync(0o600)).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("enabled = true"));
        assert!(raw.contains("name = \"default\""));
        #[cfg(unix)]
        assert_eq!(file_mode(&path), 0o600);
    }

    fn fixed_no_fsync(mode: u32) -> AtomicWritePolicy {
        AtomicWritePolicy::new(
            FileModePolicy::Fixed(mode),
            DurabilityPolicy {
                fsync_file: false,
                fsync_parent_dir: false,
            },
        )
    }
}
