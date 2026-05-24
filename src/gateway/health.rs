use crate::config::{HealthCheck, parse_duration};
use crate::health_probe::{JsonFieldCheck, check_json_fields, http_get};
use crate::template::{self, Vars};
use anyhow::Context;
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;

pub(super) async fn run_argv(command: &[String]) -> anyhow::Result<()> {
    run_argv_inner(command, None, None, &BTreeMap::new()).await
}

pub(super) async fn run_argv_with_timeout(
    command: &[String],
    timeout_duration: Duration,
) -> anyhow::Result<()> {
    run_argv_inner(command, Some(timeout_duration), None, &BTreeMap::new()).await
}

pub(super) async fn run_argv_with_options(
    command: &[String],
    timeout_duration: Duration,
    cwd: Option<&std::path::Path>,
    env: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    run_argv_inner(command, Some(timeout_duration), cwd, env).await
}

async fn run_argv_inner(
    command: &[String],
    timeout_duration: Option<Duration>,
    cwd: Option<&std::path::Path>,
    env: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let output = command_output(command, timeout_duration, cwd, env).await?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!(
        "command {:?} failed with {}: {}",
        command,
        output.status,
        stderr.trim()
    )
}

struct CommandOutput {
    status: std::process::ExitStatus,
    stderr: Vec<u8>,
}

async fn command_output(
    command: &[String],
    timeout_duration: Option<Duration>,
    cwd: Option<&std::path::Path>,
    env: &BTreeMap<String, String>,
) -> anyhow::Result<CommandOutput> {
    if timeout_duration.is_none() {
        let mut command_builder = Command::new(&command[0]);
        command_builder.args(&command[1..]).envs(env);
        if let Some(cwd) = cwd {
            command_builder.current_dir(cwd);
        }
        return command_builder
            .output()
            .await
            .map(|output| CommandOutput {
                status: output.status,
                stderr: output.stderr,
            })
            .with_context(|| format!("run {:?}", command));
    }

    let timeout_duration = timeout_duration.expect("checked above");
    let mut command_builder = Command::new(&command[0]);
    command_builder
        .args(&command[1..])
        .envs(env)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command_builder.current_dir(cwd);
    }
    command_builder.as_std_mut().process_group(0);
    let mut child = command_builder
        .spawn()
        .with_context(|| format!("run {:?}", command))?;
    let child_pid = child.id();
    let stderr = child.stderr.take();
    let mut stderr_reader = tokio::spawn(read_pipe(stderr));

    match timeout(timeout_duration, child.wait()).await {
        Ok(Ok(status)) => {
            let stderr = stderr_reader
                .await
                .context("join stderr reader")?
                .context("read stderr")?;
            Ok(CommandOutput { status, stderr })
        }
        Ok(Err(err)) => {
            let _ = stderr_reader.await;
            Err(err).with_context(|| format!("wait for {:?}", command))
        }
        Err(_) => {
            let kill_result = kill_child_process_group(child_pid);
            let wait_result = timeout(Duration::from_secs(5), child.wait()).await;
            let stderr = match timeout(Duration::from_secs(1), &mut stderr_reader).await {
                Ok(Ok(Ok(stderr))) => stderr,
                Ok(Ok(Err(err))) => {
                    tracing::warn!(command = ?command, error = %err, "timed-out command stderr read failed");
                    Vec::new()
                }
                Ok(Err(err)) => {
                    tracing::warn!(command = ?command, error = %err, "timed-out command stderr reader join failed");
                    Vec::new()
                }
                Err(_) => {
                    stderr_reader.abort();
                    Vec::new()
                }
            };
            if let Err(err) = kill_result {
                tracing::warn!(command = ?command, error = %err, "timed-out command kill failed");
            }
            match wait_result {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    tracing::warn!(command = ?command, error = %err, "timed-out command reap failed");
                }
                Err(_) => {
                    tracing::warn!(command = ?command, "timed-out command reap did not finish after SIGKILL");
                }
            }
            let stderr = String::from_utf8_lossy(&stderr);
            let detail = stderr.trim();
            if detail.is_empty() {
                anyhow::bail!(
                    "command {:?} timed out after {:?}",
                    command,
                    timeout_duration
                );
            }
            anyhow::bail!(
                "command {:?} timed out after {:?}: {}",
                command,
                timeout_duration,
                detail
            );
        }
    }
}

fn kill_child_process_group(child_pid: Option<u32>) -> std::io::Result<()> {
    let Some(child_pid) = child_pid else {
        return Ok(());
    };
    let process_group = libc::pid_t::try_from(child_pid)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let rc = unsafe { libc::killpg(process_group, libc::SIGKILL) };
    if rc == 0 {
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(err)
        }
    }
}

async fn read_pipe(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut output).await?;
    }
    Ok(output)
}

pub(super) async fn run_health_check(
    health_check: &HealthCheck,
    vars: &Vars,
) -> anyhow::Result<()> {
    match health_check {
        HealthCheck::Process => Ok(()),
        HealthCheck::Command { command } => {
            let command = template::render_argv(command, vars)?;
            run_argv(&command).await
        }
        HealthCheck::Tcp {
            host,
            port,
            timeout: configured_timeout,
            ..
        } => {
            timeout(
                health_check_timeout(configured_timeout.as_deref()),
                TcpStream::connect((host.as_str(), *port)),
            )
            .await
            .context("tcp health check timed out")??;
            Ok(())
        }
        HealthCheck::Http {
            url,
            expect_status,
            expect_json,
            timeout: configured_timeout,
            ..
        } => timeout(
            health_check_timeout(configured_timeout.as_deref()),
            http_health(url, expect_status.unwrap_or(200), expect_json),
        )
        .await
        .context("http health check timed out")?,
    }
}

fn health_check_timeout(configured: Option<&str>) -> Duration {
    configured
        .and_then(|value| parse_duration(value).ok())
        .unwrap_or_else(|| Duration::from_secs(5))
}

async fn http_health(
    url: &str,
    expect_status: u16,
    expect_json: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let response = http_get(url).await?;
    if !response.status_matches(expect_status) {
        anyhow::bail!(
            "HTTP health check expected {expect_status}, got {:?}",
            response.status_line()
        );
    }
    if !expect_json.is_empty() {
        let Some(body) = response.body() else {
            anyhow::bail!("HTTP health check response did not include a body");
        };
        match check_json_fields(body, expect_json)? {
            JsonFieldCheck::Match => {}
            JsonFieldCheck::Missing { key } => {
                anyhow::bail!("HTTP health check JSON field {key:?} missing");
            }
            JsonFieldCheck::Mismatch {
                key,
                expected,
                actual,
            } => anyhow::bail!(
                "HTTP health check JSON field {key:?} expected {expected:?}, got {actual:?}"
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::time::sleep;

    #[tokio::test]
    async fn http_health_check_uses_configured_timeout() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            sleep(Duration::from_secs(5)).await;
        });

        let check = HealthCheck::Http {
            url: format!("http://127.0.0.1:{port}/ready"),
            expect_status: Some(200),
            expect_json: BTreeMap::new(),
            interval: None,
            timeout: Some("50ms".into()),
        };

        let started = std::time::Instant::now();
        let result = run_health_check(&check, &Vars::new()).await;

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn health_check_timeout_defaults_to_five_seconds() {
        assert_eq!(health_check_timeout(None), Duration::from_secs(5));
        assert_eq!(
            health_check_timeout(Some("250ms")),
            Duration::from_millis(250)
        );
    }

    #[tokio::test]
    async fn run_argv_with_timeout_kills_hung_command() {
        let started = std::time::Instant::now();
        let err = run_argv_with_timeout(
            &["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("timed out"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn run_argv_with_timeout_kills_descendant_holding_stderr() {
        let started = std::time::Instant::now();
        let err = run_argv_with_timeout(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "sleep 30 & echo started >&2; wait".into(),
            ],
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("timed out"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn run_argv_with_timeout_reports_status_and_stderr() {
        let err = run_argv_with_timeout(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "echo expected-error >&2; exit 7".into(),
            ],
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        let err = format!("{err:#}");

        assert!(err.contains("exit status: 7"), "{err}");
        assert!(err.contains("expected-error"), "{err}");
    }

    #[tokio::test]
    async fn run_argv_without_timeout_still_supports_command_health_checks() {
        run_argv(&["/bin/sh".into(), "-c".into(), "sleep 1".into()])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_argv_with_options_honors_cwd_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        let command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "printf '%s:%s' \"$PWD\" \"$LAUNCH_TEST\" > {}",
                output.display()
            ),
        ];
        let env = BTreeMap::from([("LAUNCH_TEST".to_string(), "from-env".to_string())]);

        run_argv_with_options(&command, Duration::from_secs(5), Some(dir.path()), &env)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            format!("{}:from-env", dir.path().display())
        );
    }
}
