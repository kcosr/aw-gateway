use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OutputFormats {
    pub(super) stdout: OutputFormat,
    pub(super) stderr: OutputFormat,
}

impl OutputFormats {
    pub(super) const TEXT: Self = Self {
        stdout: OutputFormat::Text,
        stderr: OutputFormat::Text,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CapturedOutputProjectionError {
    InvalidUtf8 { stream: &'static str },
}

impl CapturedOutputProjectionError {
    pub(super) fn message(self) -> String {
        match self {
            Self::InvalidUtf8 { stream } => format!("captured {stream} is not valid UTF-8"),
        }
    }
}

pub(super) fn project_wait_payload(
    exit_code: i32,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
    formats: OutputFormats,
) -> Result<serde_json::Value, CapturedOutputProjectionError> {
    let mut payload = serde_json::Map::new();
    let mut output_errors = serde_json::Map::new();
    payload.insert("ok".into(), json!(true));
    payload.insert("mode".into(), json!("wait"));
    payload.insert("exit_code".into(), json!(exit_code));
    if let Some(stdout) = stdout {
        project_stream(
            &mut payload,
            &mut output_errors,
            "stdout",
            stdout,
            formats.stdout,
        )?;
    }
    if let Some(stderr) = stderr {
        project_stream(
            &mut payload,
            &mut output_errors,
            "stderr",
            stderr,
            formats.stderr,
        )?;
    }
    if !output_errors.is_empty() {
        payload.insert(
            "output_errors".into(),
            serde_json::Value::Object(output_errors),
        );
    }
    Ok(serde_json::Value::Object(payload))
}

fn project_stream(
    payload: &mut serde_json::Map<String, serde_json::Value>,
    output_errors: &mut serde_json::Map<String, serde_json::Value>,
    stream: &'static str,
    bytes: Vec<u8>,
    format: OutputFormat,
) -> Result<(), CapturedOutputProjectionError> {
    let text = String::from_utf8(bytes)
        .map_err(|_| CapturedOutputProjectionError::InvalidUtf8 { stream })?;
    match format {
        OutputFormat::Text => {
            payload.insert(stream.into(), json!(text));
        }
        OutputFormat::Json => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => {
                payload.insert(format!("{stream}_json"), value);
            }
            Err(_) => {
                payload.insert(stream.into(), json!(text));
                output_errors.insert(
                    stream.into(),
                    json!({
                        "format": "json",
                        "code": "invalid_json",
                        "message": format!("captured {stream} is not valid JSON"),
                    }),
                );
            }
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_text_output_without_changing_fields() {
        let payload = project_wait_payload(
            7,
            Some(b"out\n".to_vec()),
            Some(b"err\n".to_vec()),
            OutputFormats::TEXT,
        )
        .unwrap();

        assert_eq!(payload["ok"], true);
        assert_eq!(payload["mode"], "wait");
        assert_eq!(payload["exit_code"], 7);
        assert_eq!(payload["stdout"], "out\n");
        assert_eq!(payload["stderr"], "err\n");
        assert!(payload.get("stdout_json").is_none());
        assert!(payload.get("output_errors").is_none());
    }

    #[test]
    fn projects_json_output_to_json_fields() {
        let payload = project_wait_payload(
            0,
            Some(br#"{"nested":{"value":42}}"#.to_vec()),
            None,
            OutputFormats {
                stdout: OutputFormat::Json,
                stderr: OutputFormat::Text,
            },
        )
        .unwrap();

        assert_eq!(payload["stdout_json"]["nested"]["value"], 42);
        assert!(payload.get("stdout").is_none());
        assert!(payload.get("output_errors").is_none());
    }

    #[test]
    fn invalid_json_falls_back_to_text_with_output_error() {
        let payload = project_wait_payload(
            1,
            Some(b"not-json".to_vec()),
            Some(b"diagnostic".to_vec()),
            OutputFormats {
                stdout: OutputFormat::Json,
                stderr: OutputFormat::Text,
            },
        )
        .unwrap();

        assert_eq!(payload["ok"], true);
        assert_eq!(payload["exit_code"], 1);
        assert_eq!(payload["stdout"], "not-json");
        assert_eq!(payload["stderr"], "diagnostic");
        assert!(payload.get("stdout_json").is_none());
        assert_eq!(payload["output_errors"]["stdout"]["format"], "json");
        assert_eq!(payload["output_errors"]["stdout"]["code"], "invalid_json");
    }

    #[test]
    fn invalid_utf8_is_projection_error() {
        let err = project_wait_payload(0, Some(vec![0xff]), None, OutputFormats::TEXT).unwrap_err();

        assert_eq!(
            err,
            CapturedOutputProjectionError::InvalidUtf8 { stream: "stdout" }
        );
        assert_eq!(err.message(), "captured stdout is not valid UTF-8");
    }
}
