pub const GATEWAY_ACTION_NAMES: &[&str] = &[
    "connect",
    "up",
    "run",
    "launches",
    "launch-show",
    "launch",
    "status",
    "targets",
    "stop",
    "remove",
    "set-default",
    "show-default",
    "reset-default",
    "add-key",
    "add-host-key",
    "add-container-key",
    "client-config",
    "client-bundle",
    "help",
];

pub const HTTP_ACTION_NAMES: &[&str] = &[
    "status",
    "targets",
    "up",
    "launches",
    "launch-show",
    "launch",
    "run",
    "stop",
    "remove",
];

pub fn is_gateway_action_name(name: &str) -> bool {
    GATEWAY_ACTION_NAMES.contains(&name)
}

pub fn is_http_action_name(name: &str) -> bool {
    HTTP_ACTION_NAMES.contains(&name)
}

pub fn default_enabled_actions() -> Vec<String> {
    GATEWAY_ACTION_NAMES
        .iter()
        .map(|action| (*action).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_registry_matches_approved_vocabulary() {
        assert_eq!(
            GATEWAY_ACTION_NAMES,
            &[
                "connect",
                "up",
                "run",
                "launches",
                "launch-show",
                "launch",
                "status",
                "targets",
                "stop",
                "remove",
                "set-default",
                "show-default",
                "reset-default",
                "add-key",
                "add-host-key",
                "add-container-key",
                "client-config",
                "client-bundle",
                "help",
            ]
        );
        assert!(!is_gateway_action_name("rm"));
        assert!(!is_gateway_action_name("shell"));
        assert_eq!(
            HTTP_ACTION_NAMES,
            &[
                "status",
                "targets",
                "up",
                "launches",
                "launch-show",
                "launch",
                "run",
                "stop",
                "remove",
            ]
        );
        assert!(!is_http_action_name("connect"));
        assert!(!is_http_action_name("shell"));
    }
}
