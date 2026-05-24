use std::collections::BTreeMap;

pub type Vars = BTreeMap<String, String>;

pub fn validate_keys(input: &str, allowed: &[&str]) -> anyhow::Result<()> {
    for key in referenced_keys(input)? {
        if !allowed.contains(&key) {
            anyhow::bail!("unknown interpolation variable {{{key}}} in {input:?}");
        }
    }
    Ok(())
}

pub fn referenced_keys(input: &str) -> anyhow::Result<Vec<&str>> {
    let mut keys = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find('{') {
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('}') else {
            anyhow::bail!("unclosed interpolation in {input:?}");
        };
        let key = &tail[..end];
        if key.is_empty() {
            anyhow::bail!("empty interpolation in {input:?}");
        }
        keys.push(key);
        rest = &tail[end + 1..];
    }
    Ok(keys)
}

pub fn render(input: &str, vars: &Vars) -> anyhow::Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('}') else {
            anyhow::bail!("unclosed interpolation in {input:?}");
        };
        let key = &tail[..end];
        if key.is_empty() {
            anyhow::bail!("empty interpolation in {input:?}");
        }
        let value = vars.get(key).ok_or_else(|| {
            anyhow::anyhow!("unknown interpolation variable {{{key}}} in {input:?}")
        })?;
        out.push_str(value);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

pub(crate) fn render_argv(command: &[String], vars: &Vars) -> anyhow::Result<Vec<String>> {
    command.iter().map(|arg| render(arg, vars)).collect()
}

pub fn image_slug(image: &str) -> String {
    image
        .trim_start_matches("localhost/")
        .trim_end_matches(":latest")
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch,
            '/' | ':' => '-',
            _ => '-',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_known_vars() {
        let mut vars = Vars::new();
        vars.insert("image_slug".into(), "ubuntu-dev".into());
        assert_eq!(render("{image_slug}/x", &vars).unwrap(), "ubuntu-dev/x");
    }

    #[test]
    fn renders_argv_elements_independently() {
        let vars = BTreeMap::from([("workspace".to_string(), "/work tree".to_string())]);
        let command = vec![
            "printf".to_string(),
            "{workspace}".to_string(),
            "literal arg".to_string(),
        ];

        assert_eq!(
            render_argv(&command, &vars).unwrap(),
            vec![
                "printf".to_string(),
                "/work tree".to_string(),
                "literal arg".to_string()
            ]
        );
        assert!(render_argv(&["{missing}".to_string()], &vars).is_err());
    }

    #[test]
    fn rejects_unknown_vars() {
        assert!(render("{missing}", &Vars::new()).is_err());
    }

    #[test]
    fn validates_allowed_keys() {
        validate_keys("{image_slug}-{session_id}", &["image_slug", "session_id"]).unwrap();
        assert!(validate_keys("{missing}", &["image_slug"]).is_err());
    }

    #[test]
    fn extracts_referenced_keys() {
        assert_eq!(
            referenced_keys("{image_slug}-{session_id}").unwrap(),
            vec!["image_slug", "session_id"]
        );
    }

    #[test]
    fn slug_normalizes_image_ref() {
        assert_eq!(image_slug("localhost/ubuntu/dev:latest"), "ubuntu-dev");
    }
}
