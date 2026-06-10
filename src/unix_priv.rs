use std::ffi::CStr;
use std::io;

#[cfg(target_vendor = "apple")]
type GroupListEntry = libc::c_int;
#[cfg(not(target_vendor = "apple"))]
type GroupListEntry = libc::gid_t;

pub(crate) fn resolve_user_groups(user: &CStr, gid: u32) -> io::Result<Vec<libc::gid_t>> {
    let base_gid = initgroups_base_gid(gid)?;
    let mut groups = vec![zero_group_list_entry(); 16];
    loop {
        let mut count = getgrouplist_count(groups.len())?;
        let rc = unsafe { get_user_group_list(user, base_gid, groups.as_mut_ptr(), &mut count) };
        let needed = usize::try_from(count)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "group count overflow"))?;
        if rc >= 0 {
            groups.truncate(needed);
            let mut groups = groups
                .into_iter()
                .map(group_list_entry_to_gid_t)
                .collect::<io::Result<Vec<_>>>()?;
            if groups.is_empty() {
                groups.push(gid_to_gid_t(gid)?);
            }
            return Ok(groups);
        }
        let next_len = next_group_list_len(groups.len(), needed)?;
        groups.resize(next_len, zero_group_list_entry());
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

#[cfg(target_vendor = "apple")]
fn getgrouplist_count(count: usize) -> io::Result<libc::c_int> {
    libc::c_int::try_from(count)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "group count exceeds c_int"))
}

#[cfg(not(target_vendor = "apple"))]
fn getgrouplist_count(count: usize) -> io::Result<libc::c_int> {
    libc::c_int::try_from(count)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "group count exceeds c_int"))
}

#[cfg(target_vendor = "apple")]
fn setgroups_count(count: usize) -> io::Result<libc::c_int> {
    libc::c_int::try_from(count)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "group count exceeds c_int"))
}

#[cfg(not(target_vendor = "apple"))]
fn setgroups_count(count: usize) -> io::Result<libc::size_t> {
    Ok(count as libc::size_t)
}

#[cfg(target_vendor = "apple")]
fn initgroups_base_gid(gid: u32) -> io::Result<libc::c_int> {
    libc::c_int::try_from(gid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "gid exceeds c_int"))
}

#[cfg(not(target_vendor = "apple"))]
fn initgroups_base_gid(gid: u32) -> io::Result<libc::gid_t> {
    gid_to_gid_t(gid)
}

#[cfg(target_vendor = "apple")]
unsafe fn get_user_group_list(
    user: &CStr,
    base_gid: libc::c_int,
    groups: *mut GroupListEntry,
    count: *mut libc::c_int,
) -> libc::c_int {
    unsafe { libc::getgrouplist(user.as_ptr(), base_gid, groups, count) }
}

#[cfg(not(target_vendor = "apple"))]
unsafe fn get_user_group_list(
    user: &CStr,
    base_gid: libc::gid_t,
    groups: *mut GroupListEntry,
    count: *mut libc::c_int,
) -> libc::c_int {
    unsafe { libc::getgrouplist(user.as_ptr(), base_gid, groups, count) }
}

fn gid_to_gid_t(gid: u32) -> io::Result<libc::gid_t> {
    libc::gid_t::try_from(gid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "gid exceeds gid_t"))
}

fn zero_group_list_entry() -> GroupListEntry {
    0 as GroupListEntry
}

#[cfg(target_vendor = "apple")]
fn next_group_list_len(current: usize, needed: usize) -> io::Result<usize> {
    let max = apple_groups_max()?;
    if current >= max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "group list exceeds NGROUPS_MAX",
        ));
    }
    Ok(current.saturating_mul(2).max(needed).min(max))
}

#[cfg(not(target_vendor = "apple"))]
fn next_group_list_len(current: usize, needed: usize) -> io::Result<usize> {
    if needed <= current {
        return Err(io::Error::last_os_error());
    }
    Ok(needed)
}

#[cfg(target_vendor = "apple")]
fn apple_groups_max() -> io::Result<usize> {
    let value = unsafe { libc::sysconf(libc::_SC_NGROUPS_MAX) };
    if value <= 0 {
        return Ok(1024);
    }
    usize::try_from(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "NGROUPS_MAX out of range"))
}

#[cfg(target_vendor = "apple")]
fn group_list_entry_to_gid_t(value: GroupListEntry) -> io::Result<libc::gid_t> {
    Ok(value as libc::gid_t)
}

#[cfg(not(target_vendor = "apple"))]
fn group_list_entry_to_gid_t(value: GroupListEntry) -> io::Result<libc::gid_t> {
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn initgroups_base_gid_uses_gid_t_cast() {
        let expected: libc::gid_t = 42;
        assert_eq!(initgroups_base_gid(42).unwrap(), expected);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn initgroups_base_gid_rejects_c_int_overflow() {
        let err = initgroups_base_gid(u32::MAX).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "gid exceeds c_int");
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn apple_group_retry_grows_when_count_reports_current_capacity() {
        assert!(next_group_list_len(1, 1).unwrap() > 1);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn apple_group_entries_preserve_negative_gid_bit_patterns() {
        assert_eq!(
            group_list_entry_to_gid_t(-2).unwrap(),
            (-2_i32) as libc::gid_t
        );
    }

    #[test]
    fn setgroups_count_accepts_small_group_counts() {
        let _ = setgroups_count(1).unwrap();
    }

    #[test]
    fn resolves_current_user_groups() {
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let passwd = unsafe { libc::getpwuid(uid) };
        assert!(!passwd.is_null());
        let user = unsafe { CStr::from_ptr((*passwd).pw_name) };
        let gid: u32 = gid;

        let groups = resolve_user_groups(user, gid).unwrap();

        assert!(groups.contains(&gid_to_gid_t(gid).unwrap()));
    }
}
