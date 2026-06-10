pub(super) fn random_hex_token() -> anyhow::Result<String> {
    crate::random::random_hex(32)
}
