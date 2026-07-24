use std::io;

/// Disables core generation and platform process-dump attachment.
///
/// Container entrypoints call this before argument parsing or configuration
/// loading. Failure is fatal because those processes may inherit bearer
/// material.
pub fn suppress_process_dumps() -> io::Result<()> {
    suppress_process_dumps_with(set_core_limit_zero, set_platform_dumpable_zero)
}

fn suppress_process_dumps_with(
    set_core_limit: impl FnOnce() -> io::Result<()>,
    set_dumpable: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    set_core_limit()?;
    set_dumpable()
}

fn set_core_limit_zero() -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_platform_dumpable_zero() -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_platform_dumpable_zero() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardening_fails_closed_when_core_suppression_fails() {
        let error = suppress_process_dumps_with(
            || Err(io::Error::from_raw_os_error(libc::EPERM)),
            || panic!("dumpable suppression must not run after failure"),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn hardening_fails_closed_when_dumpable_suppression_fails() {
        let error = suppress_process_dumps_with(
            || Ok(()),
            || Err(io::Error::from_raw_os_error(libc::EPERM)),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
    }
}
