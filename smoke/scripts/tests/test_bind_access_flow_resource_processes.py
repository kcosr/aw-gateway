#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import os
import pathlib
import stat
import sys
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[1] / (
    "bind-access-flow-resource-processes.py"
)
SPEC = importlib.util.spec_from_file_location("rt06_process_binder", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ProcessBinderTest(unittest.TestCase):
    def test_current_process_maps_by_namespace_pid_and_executable(self) -> None:
        fields = MODULE.status_fields(os.getpid())
        namespace_pid = int(fields["NSpid"].split()[-1])
        self.assertEqual(
            MODULE.find_agent_host_pid(
                namespace_pid, os.getpid(), pathlib.Path(sys.executable)
            ),
            os.getpid(),
        )

    def test_process_binding_contains_stable_executable_digest(self) -> None:
        row = MODULE.bind_process(
            "gateway", os.getpid(), pathlib.Path(sys.executable)
        )
        fields = row.rstrip("\n").split("\t")
        self.assertEqual(fields[:2], ["gateway", str(os.getpid())])
        self.assertGreater(int(fields[2]), 0)
        self.assertEqual(len(fields[3]), 64)

    def test_output_is_owned_and_mode_is_independent_of_umask(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            root.chmod(0o700)
            directory_fd = MODULE.open_control_directory(
                root, os.getuid(), os.getgid()
            )
            previous = os.umask(0o077)
            try:
                MODULE.write_owned_file(
                    directory_fd,
                    "pids.tsv",
                    "content\n",
                    os.getuid(),
                    os.getgid(),
                )
            finally:
                os.umask(previous)
                os.close(directory_fd)
            output = root / "pids.tsv"
            metadata = output.stat()
            self.assertEqual(metadata.st_uid, os.getuid())
            self.assertEqual(metadata.st_gid, os.getgid())
            self.assertEqual(stat.S_IMODE(metadata.st_mode), 0o600)
            self.assertEqual(output.read_text(encoding="ascii"), "content\n")

    def test_control_directory_rejects_nonprivate_mode(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            root.chmod(0o750)
            with self.assertRaisesRegex(MODULE.BindingError, "must be private"):
                MODULE.open_control_directory(root, os.getuid(), os.getgid())

    def test_open_directory_descriptor_defeats_parent_path_replacement(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = pathlib.Path(temporary)
            control = base / "control"
            moved = base / "moved"
            replacement = base / "replacement"
            control.mkdir(mode=0o700)
            replacement.mkdir(mode=0o700)
            directory_fd = MODULE.open_control_directory(
                control, os.getuid(), os.getgid()
            )
            control.rename(moved)
            control.symlink_to(replacement, target_is_directory=True)
            try:
                MODULE.write_owned_file(
                    directory_fd,
                    "pids.tsv",
                    "bound\n",
                    os.getuid(),
                    os.getgid(),
                )
            finally:
                os.close(directory_fd)
            self.assertEqual(
                (moved / "pids.tsv").read_text(encoding="ascii"), "bound\n"
            )
            self.assertFalse((replacement / "pids.tsv").exists())

    def test_request_is_read_through_validated_directory_descriptor(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            root.chmod(0o700)
            request = root / "process-request.tsv"
            request.write_text(
                "field\tvalue\n"
                "namespace_pid\t1\n"
                "container_init_pid\t1\n"
                f"agent_bin\t{pathlib.Path(sys.executable).resolve()}\n"
                "proxy_pid\t1\n"
                f"proxy_bin\t{pathlib.Path(sys.executable).resolve()}\n",
                encoding="ascii",
            )
            request.chmod(0o600)
            directory_fd = MODULE.open_control_directory(
                root, os.getuid(), os.getgid()
            )
            try:
                values = MODULE.read_owned_request(
                    directory_fd, os.getuid(), os.getgid()
                )
            finally:
                os.close(directory_fd)
            self.assertEqual(
                values["agent_bin"], str(pathlib.Path(sys.executable).resolve())
            )


if __name__ == "__main__":
    unittest.main()
