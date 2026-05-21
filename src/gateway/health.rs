use crate::config::{HealthCheck, parse_duration};
use crate::template::{self, Vars};
use anyhow::Context;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;

pub(super) fn render_command(command: &[String], vars: &Vars) -> anyhow::Result<Vec<String>> {
    command
        .iter()
        .map(|arg| template::render(arg, vars))
        .collect()
}

pub(super) async fn run_argv(command: &[String]) -> anyhow::Result<()> {
    let output = Command::new(&command[0]).args(&command[1..]).output().await;
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim()),
        Err(err) => Err(err).with_context(|| format!("run {:?}", command)),
    }
}

pub(super) async fn run_health_check(
    health_check: &HealthCheck,
    vars: &Vars,
) -> anyhow::Result<()> {
    match health_check {
        HealthCheck::Process => Ok(()),
        HealthCheck::Command { command } => {
            let command = render_command(command, vars)?;
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
    let Some(rest) = url.strip_prefix("http://") else {
        anyhow::bail!("only http:// health checks are currently supported");
    };
    let (host_port, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match host_port.split_once(':') {
        Some((host, port)) => (host, port.parse::<u16>()?),
        None => (host_port, 80),
    };
    let mut stream = TcpStream::connect((host, port)).await?;
    let request = format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut buf = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut buf).await?;
    let response = String::from_utf8_lossy(&buf);
    let ok = response.starts_with(&format!("HTTP/1.1 {expect_status} "))
        || response.starts_with(&format!("HTTP/1.0 {expect_status} "));
    if !ok {
        anyhow::bail!(
            "HTTP health check expected {expect_status}, got {:?}",
            response.lines().next()
        );
    }
    if !expect_json.is_empty() {
        let Some((_, body)) = response.split_once("\r\n\r\n") else {
            anyhow::bail!("HTTP health check response did not include a body");
        };
        let value: serde_json::Value = serde_json::from_str(body)?;
        for (key, expected) in expect_json {
            match value.get(key).and_then(serde_json::Value::as_str) {
                Some(actual) if actual == expected => {}
                Some(actual) => anyhow::bail!(
                    "HTTP health check JSON field {key:?} expected {expected:?}, got {actual:?}"
                ),
                None => anyhow::bail!("HTTP health check JSON field {key:?} missing"),
            }
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
}
