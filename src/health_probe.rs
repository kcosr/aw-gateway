use std::collections::BTreeMap;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub(crate) struct HttpHealthResponse {
    response: String,
}

impl HttpHealthResponse {
    pub(crate) fn status_matches(&self, expect_status: u16) -> bool {
        self.response
            .starts_with(&format!("HTTP/1.1 {expect_status} "))
            || self
                .response
                .starts_with(&format!("HTTP/1.0 {expect_status} "))
    }

    pub(crate) fn status_line(&self) -> Option<&str> {
        self.response.lines().next()
    }

    pub(crate) fn body(&self) -> Option<&str> {
        self.response.split_once("\r\n\r\n").map(|(_, body)| body)
    }
}

pub(crate) async fn http_get(url: &str) -> anyhow::Result<HttpHealthResponse> {
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
    stream.read_to_end(&mut buf).await?;
    Ok(HttpHealthResponse {
        response: String::from_utf8_lossy(&buf).into_owned(),
    })
}

pub(crate) enum JsonFieldCheck {
    Match,
    Missing {
        key: String,
    },
    Mismatch {
        key: String,
        expected: String,
        actual: String,
    },
}

pub(crate) fn check_json_fields(
    body: &str,
    expected: &BTreeMap<String, String>,
) -> anyhow::Result<JsonFieldCheck> {
    let value: serde_json::Value = serde_json::from_str(body)?;
    for (key, expected_value) in expected {
        let Some(actual) = value.get(key).and_then(serde_json::Value::as_str) else {
            return Ok(JsonFieldCheck::Missing { key: key.clone() });
        };
        if actual != expected_value {
            return Ok(JsonFieldCheck::Mismatch {
                key: key.clone(),
                expected: expected_value.clone(),
                actual: actual.to_string(),
            });
        }
    }
    Ok(JsonFieldCheck::Match)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn status_matches_http_1_0_and_1_1() {
        let response = HttpHealthResponse {
            response: "HTTP/1.1 204 No Content\r\n\r\n".into(),
        };
        assert!(response.status_matches(204));
        assert!(!response.status_matches(200));

        let response = HttpHealthResponse {
            response: "HTTP/1.0 200 OK\r\n\r\n".into(),
        };
        assert!(response.status_matches(200));
    }

    #[test]
    fn json_field_check_reports_match_missing_and_mismatch() {
        let expected = BTreeMap::from([("status".to_string(), "ready".to_string())]);

        assert!(matches!(
            check_json_fields(r#"{"status":"ready"}"#, &expected).unwrap(),
            JsonFieldCheck::Match
        ));
        assert!(matches!(
            check_json_fields(r#"{"state":"ready"}"#, &expected).unwrap(),
            JsonFieldCheck::Missing { key } if key == "status"
        ));
        assert!(matches!(
            check_json_fields(r#"{"status":"starting"}"#, &expected).unwrap(),
            JsonFieldCheck::Mismatch { key, expected, actual }
                if key == "status" && expected == "ready" && actual == "starting"
        ));
    }

    #[tokio::test]
    async fn http_get_sends_expected_probe_request_and_reads_response() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 256];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\n\r\n{\"status\":\"ready\"}")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let response = http_get(&format!("http://127.0.0.1:{port}/ready"))
            .await
            .unwrap();
        let request = server.await.unwrap();

        assert_eq!(
            request,
            "GET /ready HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        );
        assert!(response.status_matches(200));
        assert_eq!(response.body(), Some("{\"status\":\"ready\"}"));
    }
}
