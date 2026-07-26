use std::ffi::{CStr, CString, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

const FALLBACK_PASSWD_BUFFER_BYTES: usize = 16 * 1024;
const MAX_PASSWD_BUFFER_BYTES: usize = 1024 * 1024;

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
