pub const GATEWAY_ACTION_NAMES: &[&str] = &[
    "connect",
    "up",
    "run",
    "launches",
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

pub fn is_gateway_action_name(name: &str) -> bool {
    GATEWAY_ACTION_NAMES.contains(&name)
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
    }
}
