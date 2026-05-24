use anyhow::Context;
use std::io::Read;

pub(super) fn random_hex_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut bytes)
        .context("read /dev/urandom")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
