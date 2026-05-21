use super::identity::{ensure_authorized_key, validate_public_key_content};
use super::{Runtime, load_config};
use crate::cli::{
    AddContainerKeyArgs, AddHostKeyArgs, AddKeyArgs, ClientBundleArgs, ClientConfigArgs,
    SetDefaultArgs,
};
use crate::config::{GatewayConfig, LocalSshMode};
use crate::paths::{self, UserContext};
use crate::template::{self, Vars};
use anyhow::Context;
use std::fs::File;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

const MAX_PUBLIC_KEY_BYTES: u64 = 16 * 1024;

pub(super) async fn set_default(
    config_path: Option<PathBuf>,
    args: SetDefaultArgs,
) -> anyhow::Result<()> {
    let cfg = load_config(config_path)?;
    let user = UserContext::current()?;
    if args.reset {
        let path = user.config_dir().join("default-target");
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("remove {}", path.display())),
        }
        println!("{}", configured_default_display(&cfg));
        return Ok(());
    }
    let default = args
        .target_or_image
        .ok_or_else(|| anyhow::anyhow!("target or image is required unless --reset is used"))?;
    let _ = resolve_target_selection(&cfg, Some(&default))
        .with_context(|| format!("validate default selection {default:?}"))?;
    paths::ensure_private_dir(&user.config_dir())?;
    let path = user.config_dir().join("default-target");
    std::fs::write(&path, format!("{default}\n"))?;
    println!("{default}");
    Ok(())
}

pub(super) async fn show_default(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = load_config(config_path)?;
    let user = UserContext::current()?;
    let selection = read_default_selection(&user)
        .transpose()?
        .unwrap_or_else(|| configured_default_display(&cfg));
    let _ = resolve_target_selection(&cfg, Some(&selection))
        .with_context(|| format!("validate default selection {selection:?}"))?;
    println!("{selection}");
    Ok(())
}

pub(super) fn configured_default_display(cfg: &GatewayConfig) -> String {
    cfg.default_target.clone()
}

pub(super) fn read_default_selection(user: &UserContext) -> Option<anyhow::Result<String>> {
    let path = user.config_dir().join("default-target");
    match std::fs::read_to_string(&path) {
        Ok(value) => {
            let value = value.trim().to_string();
            if value.is_empty() {
                Some(Err(anyhow::anyhow!(
                    "default selection file {} is empty",
                    path.display()
                )))
            } else {
                Some(Ok(value))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => Some(Err(err).with_context(|| format!("read {}", path.display()))),
    }
}

pub(super) fn resolve_target_selection(
    cfg: &GatewayConfig,
    selection: Option<&str>,
) -> anyhow::Result<String> {
    let selection = selection.unwrap_or(&cfg.default_target);
    if cfg.targets.contains_key(selection) {
        return Ok(selection.to_string());
    }
    let normalized = normalize_image_selection(selection);
    let matches: Vec<_> = cfg
        .targets
        .iter()
        .filter(|(_, target)| normalize_image_selection(&target.image) == normalized)
        .map(|(name, _)| name.clone())
        .collect();
    match matches.as_slice() {
        [target] => Ok(target.clone()),
        [] => anyhow::bail!("unknown target or configured image {selection:?}"),
        _ => anyhow::bail!(
            "selection {selection:?} matches multiple targets: {}",
            matches.join(", ")
        ),
    }
}

pub(super) fn normalize_image_selection(value: &str) -> String {
    let value = value
        .trim()
        .strip_prefix("localhost/")
        .unwrap_or(value.trim())
        .to_string();
    if let Some(stripped) = value.strip_suffix(":latest") {
        stripped.to_string()
    } else {
        value
    }
}

pub(super) async fn add_key(config_path: Option<PathBuf>, args: AddKeyArgs) -> anyhow::Result<()> {
    let runtime = Runtime::load(config_path, args.target.as_deref(), None, false).await?;
    let user = UserContext::current()?;
    let key = read_public_key_arg(args.public_key.as_deref(), "Paste one SSH public key:")?;
    let host_added = install_host_public_key(&user, &key)?;
    let container_added = install_inner_public_key(&runtime, &key)
        .await
        .with_context(|| format!("host={}; container install failed", key_status(host_added)))?;
    println!(
        "host={}; container={}",
        key_status(host_added),
        key_status(container_added)
    );
    Ok(())
}

pub(super) async fn add_host_key(args: AddHostKeyArgs) -> anyhow::Result<()> {
    let user = UserContext::current()?;
    let key = read_public_key_arg(args.public_key.as_deref(), "Paste one SSH public key:")?;
    let added = install_host_public_key(&user, &key)?;
    println!("{}", key_status(added));
    Ok(())
}

pub(super) async fn add_container_key(
    config_path: Option<PathBuf>,
    args: AddContainerKeyArgs,
) -> anyhow::Result<()> {
    let runtime = Runtime::load(config_path, args.target.as_deref(), None, false).await?;
    let key = read_public_key_arg(args.public_key.as_deref(), "Paste one SSH public key:")?;
    let added = install_inner_public_key(&runtime, &key).await?;
    println!("{}", key_status(added));
    Ok(())
}

fn install_host_public_key(user: &UserContext, key: &str) -> anyhow::Result<bool> {
    let ssh_dir = user.home.join(".ssh");
    paths::ensure_private_dir(&ssh_dir)?;
    let auth = ssh_dir.join("authorized_keys");
    ensure_authorized_key(&auth, key)
}

fn key_status(added: bool) -> &'static str {
    if added { "added" } else { "duplicate" }
}

pub(super) async fn client_config(
    config_path: Option<PathBuf>,
    args: ClientConfigArgs,
) -> anyhow::Result<()> {
    let runtime = Runtime::load(config_path, args.target.as_deref(), None, false).await?;
    let config = runtime.render_client_config(args.identity_file.as_deref())?;
    runtime.write_inner_config(&config)?;
    println!("{config}");
    Ok(())
}

pub(super) async fn client_bundle(
    config_path: Option<PathBuf>,
    args: ClientBundleArgs,
) -> anyhow::Result<()> {
    let runtime = Runtime::load(config_path, args.target.as_deref(), None, false).await?;
    let keypair = runtime.ensure_inner_keypair(args.rotate_key).await?;
    let identity = match args.identity_file {
        Some(identity_file) => identity_file,
        None => runtime.default_client_identity_file()?,
    };
    let config = runtime.render_client_config(Some(&identity))?;
    runtime.write_inner_config(&config)?;
    let bundle = runtime.write_client_bundle(&keypair, &config)?;
    println!("{}", bundle.display());
    Ok(())
}

async fn install_inner_public_key(runtime: &Runtime, key: &str) -> anyhow::Result<bool> {
    let ssh_dir = runtime.ssh_dir();
    paths::ensure_private_dir(&ssh_dir)?;
    let auth = ssh_dir.join("authorized_keys");
    ensure_authorized_key(&auth, key)
}

fn read_public_key_arg(path: Option<&Path>, prompt: &str) -> anyhow::Result<String> {
    let mut input = String::new();
    match path {
        Some(path) if path == Path::new("-") => {
            std::io::stdin()
                .take(MAX_PUBLIC_KEY_BYTES + 1)
                .read_to_string(&mut input)?;
        }
        Some(path) => {
            File::open(path)
                .with_context(|| format!("open public key {}", path.display()))?
                .take(MAX_PUBLIC_KEY_BYTES + 1)
                .read_to_string(&mut input)
                .with_context(|| format!("read public key {}", path.display()))?;
        }
        None => {
            eprintln!("{prompt}");
            std::io::stdin().lock().read_line(&mut input)?;
        }
    }
    if input.len() as u64 > MAX_PUBLIC_KEY_BYTES {
        anyhow::bail!("SSH public key input exceeds {MAX_PUBLIC_KEY_BYTES} bytes");
    }
    validate_public_key_content(&input)
}

impl Runtime {
    fn default_client_identity_file(&self) -> anyhow::Result<PathBuf> {
        let dir = template::render(
            &self.cfg.client_config.default_identity_dir,
            &self.client_vars(),
        )?;
        Ok(PathBuf::from(dir).join(self.client_private_key_name()))
    }

    pub(super) fn render_client_config(
        &self,
        identity_file: Option<&Path>,
    ) -> anyhow::Result<String> {
        let vars = self.client_vars();
        let alias = template::render(&self.cfg.client_config.inner_alias_template, &vars)?;
        let host_name = template::render(&self.cfg.client_config.container_host_template, &vars)?;
        let identity_lines = ssh_identity_lines(identity_file);
        if let Some(local_ssh) = &self.target.local_ssh
            && local_ssh.mode == LocalSshMode::Listen
        {
            let port = self.local_ssh_port(local_ssh)?;
            return Ok(format!(
                r#"Host {alias}
    HostName {host}
    Port {port}
{identity_lines}    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
"#,
                alias = alias,
                host = local_ssh.host,
                port = port,
                identity_lines = identity_lines,
            ));
        }
        if let Some(local_ssh) = &self.target.local_ssh
            && local_ssh.mode == LocalSshMode::ProxyCommand
        {
            return Ok(format!(
                r#"Host {alias}
    HostName {host_name}
{identity_lines}    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    ProxyCommand {gateway} connect {target}
"#,
                alias = alias,
                host_name = host_name,
                target = self.target_name,
                identity_lines = identity_lines,
                gateway = self.cfg.client_config.gateway_path,
            ));
        }
        Ok(format!(
            r#"Host {alias}
    HostName {host_name}
{identity_lines}    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    ProxyCommand ssh -T {host} {gateway} connect {target}
"#,
            host = self.cfg.client_config.host,
            host_name = host_name,
            alias = alias,
            target = self.target_name,
            identity_lines = identity_lines,
            gateway = self.cfg.client_config.gateway_path,
        ))
    }

    fn client_vars(&self) -> Vars {
        let mut vars = self.vars(None);
        vars.insert("target".into(), self.target_name.clone());
        vars.insert("host".into(), self.cfg.client_config.host.clone());
        vars
    }
}

fn ssh_identity_lines(identity_file: Option<&Path>) -> String {
    identity_file
        .map(|identity| {
            format!(
                "    IdentityFile {}\n    IdentitiesOnly yes\n",
                identity.display()
            )
        })
        .unwrap_or_default()
}
