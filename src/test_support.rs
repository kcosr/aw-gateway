use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) fn write_executable_fixture(path: &Path, contents: impl AsRef<[u8]>) {
    let parent = path.parent().unwrap();
    let mut input = tempfile::Builder::new()
        .prefix(".aw-gateway-test-executable-input.")
        .tempfile_in(parent)
        .unwrap();
    input.write_all(contents.as_ref()).unwrap();
    input.as_file().sync_all().unwrap();
    let input = input.into_temp_path();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "test_support::tests::executable_fixture_writer_process",
            "--ignored",
            "--nocapture",
        ])
        .env("AW_GATEWAY_FIXTURE_WRITER_TARGET", path)
        .env("AW_GATEWAY_FIXTURE_WRITER_INPUT", &input)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "executable fixture writer timed out after 10s: {}",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(
        status.success(),
        "executable fixture writer failed for {}: {status}",
        path.display()
    );
}

fn write_executable_fixture_in_writer_process(path: &Path, input: &Path) {
    let parent = path.parent().unwrap();
    let mut temporary = tempfile::Builder::new()
        .prefix(".aw-gateway-test-executable.")
        .tempfile_in(parent)
        .unwrap();
    std::io::copy(&mut std::fs::File::open(input).unwrap(), &mut temporary).unwrap();
    temporary.as_file().sync_all().unwrap();
    let temporary = temporary.into_temp_path();
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755)).unwrap();
    temporary.persist(path).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    #[ignore = "entry point for the bounded fixture writer subprocess"]
    fn executable_fixture_writer_process() {
        let target = std::env::var_os("AW_GATEWAY_FIXTURE_WRITER_TARGET").unwrap();
        let input = std::env::var_os("AW_GATEWAY_FIXTURE_WRITER_INPUT").unwrap();
        write_executable_fixture_in_writer_process(
            Path::new(target.as_os_str()),
            Path::new(input.as_os_str()),
        );
    }

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

    #[test]
    fn executable_fixture_remains_exec_ready_under_parallel_publication() {
        let directory = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let root = directory.path();
                scope.spawn(move || {
                    for iteration in 0..25 {
                        let executable = root.join(format!("fixture-{worker}-{iteration}"));
                        write_executable_fixture(&executable, b"#!/bin/sh\nexit 0\n");
                        assert!(Command::new(&executable).status().unwrap().success());
                    }
                });
            }
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_writable_description_deterministically_blocks_exec() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fixture");
        let mut temporary = tempfile::Builder::new()
            .prefix(".aw-gateway-test-negative-control.")
            .tempfile_in(directory.path())
            .unwrap();
        temporary.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
        temporary.as_file().sync_all().unwrap();
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let (writer, temporary) = temporary.into_parts();
        temporary.persist(&executable).unwrap();

        let mut release = [0; 2];
        assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
        let inherited_writer = writer.as_raw_fd();
        let child = unsafe { libc::fork() };
        assert!(
            child >= 0,
            "fork failed: {}",
            std::io::Error::last_os_error()
        );
        if child == 0 {
            unsafe {
                libc::close(release[1]);
                let mut byte = 0_u8;
                let _ = libc::read(
                    release[0],
                    std::ptr::from_mut(&mut byte).cast(),
                    std::mem::size_of_val(&byte),
                );
                libc::close(release[0]);
                libc::close(inherited_writer);
                libc::_exit(0);
            }
        }

        unsafe {
            libc::close(release[0]);
        }
        drop(writer);
        let blocked = Command::new(&executable).status();
        unsafe {
            let byte = 1_u8;
            assert_eq!(
                libc::write(
                    release[1],
                    std::ptr::from_ref(&byte).cast(),
                    std::mem::size_of_val(&byte),
                ),
                1
            );
            libc::close(release[1]);
            let mut status = 0;
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
        }

        let blocked = blocked.expect_err("exec unexpectedly succeeded with an inherited writer");
        assert_eq!(blocked.raw_os_error(), Some(libc::ETXTBSY));
        assert!(Command::new(&executable).status().unwrap().success());
    }
}
