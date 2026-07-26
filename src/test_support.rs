use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub(crate) fn write_executable_fixture(path: &Path, contents: impl AsRef<[u8]>) {
    let parent = path.parent().unwrap();
    let mut temporary = tempfile::Builder::new()
        .prefix(".aw-gateway-test-executable.")
        .tempfile_in(parent)
        .unwrap();
    temporary.write_all(contents.as_ref()).unwrap();
    temporary.as_file().sync_all().unwrap();

    let temporary = temporary.into_temp_path();
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755)).unwrap();
    temporary.persist(path).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::process::Command;

    #[test]
    fn executable_fixture_is_closed_and_exact_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fixture");
        let contents = b"#!/bin/sh\nprintf ready\n";

        write_executable_fixture(&executable, contents);

        assert_eq!(std::fs::read(&executable).unwrap(), contents);
        assert_eq!(
            std::fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let output = Command::new(&executable).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ready");
        assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .as_bytes()
                .starts_with(b".aw-gateway-test-executable.")
        }));
    }

    #[test]
    fn executable_fixture_replaces_symlink_without_touching_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let executable = directory.path().join("fixture");
        std::fs::write(&target, b"target").unwrap();
        std::os::unix::fs::symlink(&target, &executable).unwrap();

        write_executable_fixture(&executable, b"#!/bin/sh\nexit 0\n");

        assert_eq!(std::fs::read(&target).unwrap(), b"target");
        assert!(
            !std::fs::symlink_metadata(&executable)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
