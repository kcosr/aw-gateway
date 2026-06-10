use std::ffi::CStr;
use std::io;

pub(crate) fn resolve_user_groups(user: &CStr, gid: u32) -> io::Result<Vec<libc::gid_t>> {
    let base_gid = initgroups_base_gid(gid)?;
    let mut groups = vec![0 as libc::gid_t; 16];
    loop {
        let mut count = getgrouplist_count(groups.len())?;
        let rc =
            unsafe { libc::getgrouplist(user.as_ptr(), base_gid, groups.as_mut_ptr(), &mut count) };
        let needed = usize::try_from(count)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "group count overflow"))?;
        if rc == 0 {
            groups.truncate(needed);
            if groups.is_empty() {
                groups.push(gid as libc::gid_t);
            }
            return Ok(groups);
        }
        if needed <= groups.len() {
            return Err(io::Error::last_os_error());
        }
        groups.resize(needed, 0 as libc::gid_t);
    }
}

pub(crate) fn drop_to_user_pre_exec(uid: u32, gid: u32, groups: &[libc::gid_t]) -> io::Result<()> {
    let group_count = setgroups_count(groups.len())?;
    if unsafe { libc::setgroups(group_count, groups.as_ptr()) } != 0 {
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
fn getgrouplist_count(count: usize) -> io::Result<libc::c_int> {
    libc::c_int::try_from(count)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "group count exceeds c_int"))
}

#[cfg(not(target_os = "macos"))]
fn getgrouplist_count(count: usize) -> io::Result<libc::c_int> {
    libc::c_int::try_from(count)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "group count exceeds c_int"))
}

#[cfg(target_os = "macos")]
fn setgroups_count(count: usize) -> io::Result<libc::c_int> {
    libc::c_int::try_from(count)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "group count exceeds c_int"))
}

#[cfg(not(target_os = "macos"))]
fn setgroups_count(count: usize) -> io::Result<libc::size_t> {
    Ok(count as libc::size_t)
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

    #[test]
    fn setgroups_count_accepts_small_group_counts() {
        let _ = setgroups_count(1).unwrap();
    }
}
