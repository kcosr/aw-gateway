use std::ffi::CStr;
use std::io;

pub(crate) fn drop_to_user_pre_exec(user: &CStr, uid: u32, gid: u32) -> io::Result<()> {
    let base_gid = initgroups_base_gid(gid)?;
    if unsafe { libc::initgroups(user.as_ptr(), base_gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::setgid(gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::setuid(uid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn initgroups_base_gid(gid: u32) -> io::Result<libc::c_int> {
    libc::c_int::try_from(gid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "gid exceeds c_int"))
}

#[cfg(not(target_os = "macos"))]
fn initgroups_base_gid(gid: u32) -> io::Result<libc::gid_t> {
    Ok(gid as libc::gid_t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn initgroups_base_gid_uses_gid_t_cast() {
        let expected: libc::gid_t = 42;
        assert_eq!(initgroups_base_gid(42).unwrap(), expected);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn initgroups_base_gid_rejects_c_int_overflow() {
        let err = initgroups_base_gid(u32::MAX).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "gid exceeds c_int");
    }
}
