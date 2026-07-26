use std::ffi::{CStr, CString, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

const FALLBACK_PASSWD_BUFFER_BYTES: usize = 16 * 1024;
const MAX_PASSWD_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) struct PasswdRecord {
    pub(crate) uid: libc::uid_t,
    pub(crate) gid: libc::gid_t,
    pub(crate) name: OsString,
    pub(crate) home: PathBuf,
}

pub(crate) fn passwd_by_uid(uid: libc::uid_t) -> anyhow::Result<PasswdRecord> {
    lookup_passwd(
        &format!("uid {uid}"),
        |entry, buffer, buffer_len, result| unsafe {
            libc::getpwuid_r(uid, entry, buffer, buffer_len, result)
        },
    )
}

pub(crate) fn passwd_by_name(name: &str) -> anyhow::Result<PasswdRecord> {
    let c_name = CString::new(name).map_err(|_| anyhow::anyhow!("user name contains NUL byte"))?;
    lookup_passwd(
        &format!("user {name:?}"),
        |entry, buffer, buffer_len, result| unsafe {
            libc::getpwnam_r(c_name.as_ptr(), entry, buffer, buffer_len, result)
        },
    )
}

fn lookup_passwd(
    label: &str,
    mut lookup: impl FnMut(
        *mut libc::passwd,
        *mut libc::c_char,
        libc::size_t,
        *mut *mut libc::passwd,
    ) -> libc::c_int,
) -> anyhow::Result<PasswdRecord> {
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buffer_len = usize::try_from(suggested)
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(FALLBACK_PASSWD_BUFFER_BYTES)
        .min(MAX_PASSWD_BUFFER_BYTES);

    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_len];
        let status = lookup(
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        );
        if status == libc::ERANGE {
            if buffer_len == MAX_PASSWD_BUFFER_BYTES {
                anyhow::bail!(
                    "passwd entry for {label} exceeds {MAX_PASSWD_BUFFER_BYTES}-byte lookup limit"
                );
            }
            buffer_len = buffer_len.saturating_mul(2).min(MAX_PASSWD_BUFFER_BYTES);
            continue;
        }
        if status != 0 {
            anyhow::bail!(
                "could not resolve passwd entry for {label}: {}",
                std::io::Error::from_raw_os_error(status)
            );
        }
        if result.is_null() {
            anyhow::bail!("passwd entry for {label} does not exist");
        }

        let entry = unsafe { entry.assume_init() };
        if entry.pw_name.is_null() || entry.pw_dir.is_null() {
            anyhow::bail!("passwd entry for {label} is incomplete");
        }
        let name = unsafe { CStr::from_ptr(entry.pw_name) };
        let home = unsafe { CStr::from_ptr(entry.pw_dir) };
        return Ok(PasswdRecord {
            uid: entry.pw_uid,
            gid: entry.pw_gid,
            name: OsString::from_vec(name.to_bytes().to_vec()),
            home: PathBuf::from(OsString::from_vec(home.to_bytes().to_vec())),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn lookup_retries_erange_and_owns_exact_record_bytes() {
        let mut calls = 0;
        let record = lookup_passwd("fixture", |entry, buffer, buffer_len, result| {
            calls += 1;
            if calls == 1 {
                return libc::ERANGE;
            }
            write_record(entry, buffer, buffer_len, result, b"fixture", b"/home/\xff");
            0
        })
        .unwrap();

        assert_eq!(calls, 2);
        assert_eq!(record.uid, 1234);
        assert_eq!(record.gid, 5678);
        assert_eq!(record.name.as_bytes(), b"fixture");
        assert_eq!(record.home.as_os_str().as_bytes(), b"/home/\xff");
    }

    #[test]
    fn lookup_rejects_limit_error_not_found_and_incomplete_records() {
        let limit = lookup_passwd("fixture", |_, _, _, _| libc::ERANGE)
            .unwrap_err()
            .to_string();
        assert!(
            limit.contains("exceeds 1048576-byte lookup limit"),
            "{limit}"
        );

        let error = lookup_passwd("fixture", |_, _, _, _| libc::EIO)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Input/output error"), "{error}");

        let missing = lookup_passwd("fixture", |_, _, _, result| {
            unsafe { *result = std::ptr::null_mut() };
            0
        })
        .unwrap_err()
        .to_string();
        assert!(missing.contains("does not exist"), "{missing}");

        let incomplete = lookup_passwd("fixture", |entry, _, _, result| {
            unsafe {
                entry.write(std::mem::zeroed());
                *result = entry;
            }
            0
        })
        .unwrap_err()
        .to_string();
        assert!(incomplete.contains("is incomplete"), "{incomplete}");
    }

    fn write_record(
        entry: *mut libc::passwd,
        buffer: *mut libc::c_char,
        buffer_len: usize,
        result: *mut *mut libc::passwd,
        name: &[u8],
        home: &[u8],
    ) {
        let required = name.len() + 1 + home.len() + 1;
        assert!(buffer_len >= required);
        unsafe {
            std::ptr::copy_nonoverlapping(name.as_ptr(), buffer.cast(), name.len());
            *buffer.add(name.len()) = 0;
            let home_ptr = buffer.add(name.len() + 1);
            std::ptr::copy_nonoverlapping(home.as_ptr(), home_ptr.cast(), home.len());
            *home_ptr.add(home.len()) = 0;
            let mut record: libc::passwd = std::mem::zeroed();
            record.pw_name = buffer;
            record.pw_uid = 1234;
            record.pw_gid = 5678;
            record.pw_dir = home_ptr;
            entry.write(record);
            *result = entry;
        }
    }
}
