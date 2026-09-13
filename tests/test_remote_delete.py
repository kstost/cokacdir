"""Executable regression tests for the exact SSH helper embedded in Rust.

Run without building the application:
    python3 -m unittest discover -s tests -p test_remote_delete.py -v
"""

import base64
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock
import uuid


HELPER_PATH = Path(__file__).resolve().parents[1] / "src/services/remote_delete.py"
SPEC = importlib.util.spec_from_file_location("remote_delete_helper", HELPER_PATH)
helper = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(helper)


class RemoteDeletionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cokac-remote-delete-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "selected"
        self.outside = self.root / "outside"
        self.outside.mkdir()
        (self.outside / "keep").write_bytes(b"outside data")

    def request(self):
        stage = ".cokacdir-delete-" + uuid.uuid4().hex
        (self.root / stage).mkdir(mode=0o700)
        proof = uuid.uuid4().hex + uuid.uuid4().hex
        proof_name = "proof-" + uuid.uuid4().hex
        (self.root / stage / proof_name).write_text(proof)
        metadata = self.source.lstat()
        return {
            "parent": str(self.root), "name": self.source.name, "stage": stage,
            "proof_name": proof_name, "proof": proof, "payload": "entry-" + uuid.uuid4().hex,
            "expected": {"uid": metadata.st_uid, "gid": metadata.st_gid,
                         "size": metadata.st_size, "mtime": int(metadata.st_mtime),
                         "permissions": metadata.st_mode},
        }

    def assert_outside_preserved(self):
        self.assertEqual((self.outside / "keep").read_bytes(), b"outside data")

    def test_recursive_delete_preserves_all_link_targets(self):
        self.source.mkdir()
        (self.source / "child").mkdir()
        (self.source / "child/file").write_text("selected data")
        (self.source / "absolute").symlink_to(self.outside)
        (self.source / "relative").symlink_to("../outside")
        (self.source / "dangling").symlink_to("missing")
        (self.source / "loop").symlink_to("loop")
        request = self.request()
        helper.delete(request)
        self.assertFalse(self.source.exists())
        self.assertFalse((self.root / request["stage"]).exists())
        self.assert_outside_preserved()

    def test_top_level_directory_link_is_unlinked(self):
        self.source.symlink_to(self.outside)
        helper.delete(self.request())
        self.assertFalse(self.source.is_symlink())
        self.assert_outside_preserved()

    def test_top_level_dangling_link_is_unlinked(self):
        self.source.symlink_to("missing")
        helper.delete(self.request())
        self.assertFalse(self.source.is_symlink())

    def test_regular_file_and_other_hardlink(self):
        self.source.write_bytes(b"same inode")
        alias = self.root / "alias"
        os.link(self.source, alias)
        helper.delete(self.request())
        self.assertEqual(alias.read_bytes(), b"same inode")
        self.assertFalse(self.source.exists())

    def test_directory_swap_between_lstat_and_open_is_rejected(self):
        self.source.mkdir()
        (self.source / "child").mkdir()
        (self.source / "child/original").write_text("original")
        request = self.request()
        real_open = helper.open_directory

        def swap(parent, name, metadata):
            if name == "child":
                os.rename(name, "retained-child", src_dir_fd=parent, dst_dir_fd=parent)
                os.symlink(str(self.outside), name, dir_fd=parent)
            return real_open(parent, name, metadata)

        with mock.patch.object(helper, "open_directory", side_effect=swap):
            with self.assertRaisesRegex(RuntimeError, "retained at"):
                helper.delete(request)
        self.assert_outside_preserved()
        recovery = self.root / request["stage"] / request["payload"]
        self.assertEqual((recovery / "retained-child/original").read_text(), "original")

    def test_directory_swap_after_open_does_not_redirect_child_removal(self):
        self.source.mkdir()
        (self.source / "child").mkdir()
        (self.source / "child/keep").write_text("selected content")
        request = self.request()
        real_open = helper.open_directory

        def swap(parent, name, metadata):
            descriptor = real_open(parent, name, metadata)
            if name == "child":
                os.rename(name, "retained-child", src_dir_fd=parent, dst_dir_fd=parent)
                os.symlink(str(self.outside), name, dir_fd=parent)
            return descriptor

        with mock.patch.object(helper, "open_directory", side_effect=swap):
            with self.assertRaisesRegex(RuntimeError, "Directory changed"):
                helper.delete(request)
        self.assert_outside_preserved()

    def test_source_swap_during_quarantine_is_retained(self):
        self.source.mkdir()
        request = self.request()
        real_rename = os.rename

        def swap(source, destination, **kwargs):
            if source == self.source.name:
                real_rename(self.source, self.root / "original")
                self.source.symlink_to(self.outside)
            return real_rename(source, destination, **kwargs)

        # Capability checks compare function identities; preserve those checks
        # before replacing the syscall solely to force the race boundary.
        helper.capabilities()
        with mock.patch.object(helper, "capabilities"), mock.patch.object(helper.os, "rename", side_effect=swap):
            with self.assertRaisesRegex(RuntimeError, "Source changed while being isolated"):
                helper.delete(request)
        self.assert_outside_preserved()
        self.assertTrue((self.root / request["stage"] / request["payload"]).is_symlink())

    def test_wrong_namespace_proof_never_moves_source(self):
        self.source.mkdir()
        request = self.request()
        request["proof"] = "0" * len(request["proof"])
        with self.assertRaisesRegex(RuntimeError, "namespaces"):
            helper.delete(request)
        self.assertTrue(self.source.is_dir())
        self.assert_outside_preserved()

    def test_ssh_parent_replacement_cannot_match_sftp_proof(self):
        parent = self.root / "parent"
        parent.mkdir()
        self.source = parent / "selected"
        self.source.mkdir()
        original_root = self.root
        self.root = parent
        request = self.request()
        self.root = original_root
        parent.rename(self.root / "retained-parent")
        parent.symlink_to(self.outside)
        with self.assertRaises(OSError):
            helper.delete(request)
        self.assertTrue((self.root / "retained-parent/selected").is_dir())
        self.assert_outside_preserved()

    def test_changed_source_metadata_never_moves_source(self):
        self.source.write_text("original")
        request = self.request()
        self.source.write_text("new and different content")
        with self.assertRaisesRegex(RuntimeError, "Source changed after SFTP"):
            helper.delete(request)
        self.assertEqual(self.source.read_text(), "new and different content")

    def test_unsupported_server_does_not_mutate_source(self):
        self.source.mkdir()
        request = self.request()
        with mock.patch.object(helper.os, "supports_dir_fd", set()):
            with self.assertRaisesRegex(RuntimeError, "requires POSIX"):
                helper.delete(request)
        self.assertTrue(self.source.is_dir())

    def test_cli_command_handles_shell_metacharacters_as_data(self):
        self.source = self.root / "-한글 ' \n$(touch WRONG)`echo bad`;file"
        self.source.write_text("selected")
        request = self.request()
        script = base64.b64encode(HELPER_PATH.read_bytes()).decode("ascii")
        data = base64.b64encode(json.dumps(request).encode()).decode("ascii")
        command = "python3 -I -c 'import base64;exec(base64.b64decode(\"" + script + "\"))' '" + data + "'"
        result = subprocess.run(["sh", "-c", command], cwd=self.root, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "COKACDIR_DELETE_OK")
        self.assertFalse(self.source.exists())
        self.assertFalse((self.root / "WRONG").exists())
        self.assert_outside_preserved()


if __name__ == "__main__":
    unittest.main()
