use super::Runtime;
use super::token::random_hex_token;
use crate::fileutil::{
    AtomicWritePolicy, DurabilityPolicy, FileModePolicy, atomic_write_file, remove_if_exists,
    set_mode, write_private_file,
};
use crate::paths;
use anyhow::Context;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tokio::process::Command;

impl Runtime {
    pub(super) fn workspace_state_dir(&self) -> PathBuf {
        self.workspace.join(&self.target.workspace.state_dir)
    }

    pub(super) fn ssh_dir(&self) -> PathBuf {
        self.workspace_state_dir().join("ssh")
    }

    pub(super) fn inner_private_key(&self) -> PathBuf {
        self.ssh_dir().join("inner_ed25519")
    }

    fn inner_public_key(&self) -> PathBuf {
        self.ssh_dir().join("inner_ed25519.pub")
    }

    fn inner_authorized_keys(&self) -> PathBuf {
        self.ssh_dir().join("authorized_keys")
    }

    pub(super) fn client_private_key_name(&self) -> String {
        format!("aw-{}-{}_inner_ed25519", self.user.user, self.target_name)
    }

    pub(super) async fn ensure_inner_keypair(&self, rotate: bool) -> anyhow::Result<InnerKeypair> {
        let ssh_dir = self.ssh_dir();
        paths::ensure_private_dir(&ssh_dir)?;
        let private_key = self.inner_private_key();
        let public_key = self.inner_public_key();
        if rotate {
            remove_if_exists(&private_key)?;
            remove_if_exists(&public_key)?;
        }
        if private_key.exists() {
            validate_private_key_permissions(&private_key)?;
            validate_private_key(&private_key, &self.target_name).await?;
            if !public_key.exists() {
                write_public_key_from_private(&private_key, &public_key).await?;
            }
        } else {
            generate_inner_keypair(&private_key, &self.user.user, &self.target_name).await?;
        }
        set_mode(&private_key, 0o600)?;
        set_mode(&public_key, 0o644)?;
        let public = std::fs::read_to_string(&public_key)
            .with_context(|| format!("read {}", public_key.display()))?;
        ensure_authorized_key(&self.inner_authorized_keys(), public.trim())?;
        Ok(InnerKeypair {
            private_key,
            public_key,
        })
    }

    pub(super) fn write_inner_config(&self, config: &str) -> anyhow::Result<PathBuf> {
        paths::ensure_private_dir(&self.ssh_dir())?;
        let path = self.ssh_dir().join("config");
        write_private_file(&path, config.as_bytes(), 0o600)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    pub(super) fn write_client_bundle(
        &self,
        keypair: &InnerKeypair,
        config: &str,
    ) -> anyhow::Result<PathBuf> {
        let bundle_dir = self.ssh_dir().join("bundle").join(&self.target_name);
        paths::ensure_private_dir(&bundle_dir)?;
        let private_name = self.client_private_key_name();
        let private_out = bundle_dir.join(&private_name);
        let public_out = bundle_dir.join(format!("{private_name}.pub"));
        let config_out = bundle_dir.join("ssh_config");
        std::fs::copy(&keypair.private_key, &private_out)
            .with_context(|| format!("copy {}", private_out.display()))?;
        std::fs::copy(&keypair.public_key, &public_out)
            .with_context(|| format!("copy {}", public_out.display()))?;
        write_private_file(&config_out, config.as_bytes(), 0o600)
            .with_context(|| format!("write {}", config_out.display()))?;
        set_mode(&private_out, 0o600)?;
        set_mode(&public_out, 0o644)?;
        set_mode(&config_out, 0o600)?;
        Ok(bundle_dir)
    }

    pub(super) fn ensure_identity_token(&self) -> anyhow::Result<String> {
        // The identity token provider is intentionally tied to services that
        // request `AW_IDENTITY_TOKEN` via EnvValue::inherit. Keep this in sync
        // with ContainerAgentConfig::needs_identity_token when adding new token
        // consumer mechanisms.
        if let Ok(value) = std::env::var("AW_IDENTITY_TOKEN") {
            return validate_identity_token_content(&value, Path::new("AW_IDENTITY_TOKEN"));
        }
        let path = self.user.config_dir().join("identity-token");
        ensure_identity_token_file(&path)
    }

    pub(super) fn control_token_path(&self) -> PathBuf {
        self.container_state_dir.join("control.token")
    }

    pub(super) fn ensure_control_token(&self) -> anyhow::Result<String> {
        let path = self.control_token_path();
        match std::fs::read_to_string(&path) {
            Ok(value) if !value.trim().is_empty() => return Ok(value.trim().to_string()),
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        }
        let token = random_hex_token()?;
        write_private_file(&path, format!("{token}\n").as_bytes(), 0o600)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(token)
    }

    pub(super) fn control_token(&self) -> anyhow::Result<String> {
        let path = self.control_token_path();
        let value =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let token = value.trim();
        if token.is_empty() {
            anyhow::bail!("control token file {} is empty", path.display());
        }
        Ok(token.to_string())
    }
}

#[derive(Debug, Clone)]
pub(super) struct InnerKeypair {
    pub(super) private_key: PathBuf,
    pub(super) public_key: PathBuf,
}

fn random_identity_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 16];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut bytes)
        .context("read /dev/urandom")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

async fn generate_inner_keypair(path: &Path, user: &str, target: &str) -> anyhow::Result<()> {
    let output = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", ""])
        .arg("-C")
        .arg(format!("aw-gateway:{user}:{target}"))
        .arg("-f")
        .arg(path)
        .output()
        .await
        .with_context(|| "run ssh-keygen")?;
    if !output.status.success() {
        anyhow::bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn validate_private_key(path: &Path, target: &str) -> anyhow::Result<()> {
    let output = Command::new("ssh-keygen")
        .args(["-y", "-f"])
        .arg(path)
        .output()
        .await
        .with_context(|| format!("validate private key {}", path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "inner private key {} is not usable; rerun `aw-gateway client-bundle {} --rotate-key`: {}",
            path.display(),
            target,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn write_public_key_from_private(
    private_key: &Path,
    public_key: &Path,
) -> anyhow::Result<()> {
    let output = Command::new("ssh-keygen")
        .args(["-y", "-f"])
        .arg(private_key)
        .output()
        .await
        .with_context(|| format!("derive public key from {}", private_key.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to derive public key: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    std::fs::write(public_key, output.stdout)
        .with_context(|| format!("write {}", public_key.display()))?;
    Ok(())
}

pub(super) fn ensure_authorized_key(path: &Path, public_key: &str) -> anyhow::Result<bool> {
    let public_key = validate_public_key_content(public_key)?;
    let (mut keys, present) = read_authorized_keys(path, &public_key)?;
    if !present {
        keys.push(public_key);
    }
    atomic_write_file(
        path,
        format!("{}\n", keys.join("\n")).as_bytes(),
        AtomicWritePolicy::new(
            FileModePolicy::Fixed(0o644),
            DurabilityPolicy {
                fsync_file: false,
                fsync_parent_dir: false,
            },
        ),
    )?;
    Ok(!present)
}

fn read_authorized_keys(path: &Path, public_key: &str) -> anyhow::Result<(Vec<String>, bool)> {
    let existing = match std::fs::read_to_string(path) {
        Ok(value) => value,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let mut keys = Vec::new();
    let mut present = false;
    for line in existing.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let key = validate_public_key_content(line)
            .with_context(|| format!("validate existing authorized key in {}", path.display()))?;
        if key == public_key {
            present = true;
        }
        keys.push(key);
    }
    Ok((keys, present))
}

pub(super) fn validate_public_key_content(value: &str) -> anyhow::Result<String> {
    // Keep validation errors generic: callers may validate arbitrary readable
    // paths supplied by users, and errors must not echo rejected file content.
    let value = value.strip_suffix('\n').unwrap_or(value);
    if value.contains(['\n', '\r']) {
        anyhow::bail!("SSH public key must be exactly one line");
    }
    if value.trim() != value {
        anyhow::bail!("SSH public key must not have leading or trailing whitespace");
    }
    if !is_plausible_public_key(value) {
        anyhow::bail!("input does not look like a supported SSH public key");
    }
    Ok(value.to_string())
}

fn validate_private_key_permissions(path: &Path) -> anyhow::Result<()> {
    validate_secret_file_permissions(path)
}

fn validate_secret_file_permissions(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "secret file {} has unsafe mode {:o}; expected 0600 or stricter",
                path.display(),
                mode
            );
        }
    }
    Ok(())
}

pub(super) fn ensure_identity_token_file(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(value) => {
            validate_secret_file_permissions(path)?;
            return validate_identity_token_content(&value, path);
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    }

    if let Some(parent) = path.parent() {
        paths::ensure_private_dir(parent)?;
    }

    let token = random_identity_token()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    match options.open(path) {
        Ok(mut file) => {
            writeln!(file, "{token}").with_context(|| format!("write {}", path.display()))?;
            set_mode(path, 0o600)?;
            Ok(token)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let value = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            validate_secret_file_permissions(path)?;
            validate_identity_token_content(&value, path)
        }
        Err(err) => Err(err).with_context(|| format!("create {}", path.display())),
    }
}

pub(super) fn validate_identity_token_content(value: &str, path: &Path) -> anyhow::Result<String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        anyhow::bail!("identity token file {} is empty", path.display());
    }
    if value.len() > 4096 {
        anyhow::bail!("identity token file {} is too large", path.display());
    }
    if value.lines().count() != 1 {
        anyhow::bail!(
            "identity token file {} must contain exactly one line",
            path.display()
        );
    }
    Ok(value)
}

pub(super) fn is_plausible_public_key(key: &str) -> bool {
    let mut parts = key.split_whitespace();
    let Some(kind) = parts.next() else {
        return false;
    };
    let Some(body) = parts.next() else {
        return false;
    };
    matches!(
        kind,
        "ssh-ed25519"
            | "ssh-rsa"
            | "ecdsa-sha2-nistp256"
            | "ecdsa-sha2-nistp384"
            | "ecdsa-sha2-nistp521"
    ) && !body.is_empty()
}
