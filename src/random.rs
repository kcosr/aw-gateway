use anyhow::Context;

pub(crate) fn random_hex(byte_len: usize) -> anyhow::Result<String> {
    let mut bytes = vec![0_u8; byte_len];
    getrandom::fill(&mut bytes).context("get random bytes")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
