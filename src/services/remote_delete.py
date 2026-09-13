"""SSH deletion helper, embedded by remote.rs; requires POSIX Python 3.

SFTP has no unlinkat/openat operations. A private proof directory created over
SFTP binds the SSH command to that same namespace before any source is moved.
All subsequent traversal and removal use open directory descriptors.
"""

import base64
import json
import os
import stat
import sys


def capabilities():
    required = (os.open, os.stat, os.rename, os.unlink, os.rmdir)
    if (os.name != "posix" or not hasattr(os, "O_NOFOLLOW")
            or not hasattr(os, "O_DIRECTORY")
            or any(function not in os.supports_dir_fd for function in required)
            or os.scandir not in os.supports_fd or os.listdir not in os.supports_fd):
        raise RuntimeError("Safe remote deletion requires POSIX Python 3 with directory-relative filesystem operations")


def component(name):
    if not isinstance(name, str) or name in ("", ".", "..") or "/" in name or "\0" in name:
        raise ValueError("Deletion requires one normal filename")
    return name


def identity(metadata):
    return metadata.st_dev, metadata.st_ino, stat.S_IFMT(metadata.st_mode)


def lstat(parent, name):
    return os.stat(name, dir_fd=parent, follow_symlinks=False)


def open_directory(parent, name, expected):
    descriptor = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                         dir_fd=parent)
    try:
        if identity(os.fstat(descriptor)) != identity(expected):
            raise RuntimeError("Directory changed while opening " + repr(name))
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def remove_tree(parent, name):
    """Delete an entry without following links, including concurrent swaps."""
    metadata = lstat(parent, name)
    if not stat.S_ISDIR(metadata.st_mode):
        os.unlink(name, dir_fd=parent)
        return
    directory = open_directory(parent, name, metadata)
    stack = []
    try:
        stack.append((parent, name, metadata, directory, os.scandir(directory)))
    except BaseException:
        os.close(directory)
        raise
    try:
        while stack:
            parent, name, before, directory, entries = stack[-1]
            entry = next(entries, None)
            if entry is None:
                if identity(lstat(parent, name)) != identity(before):
                    raise RuntimeError("Directory changed during deletion: " + repr(name))
                os.rmdir(name, dir_fd=parent)
                entries.close()
                os.close(directory)
                stack.pop()
                continue
            child_name = component(entry.name)
            child_metadata = lstat(directory, child_name)
            if stat.S_ISDIR(child_metadata.st_mode):
                child = open_directory(directory, child_name, child_metadata)
                try:
                    child_entries = os.scandir(child)
                except BaseException:
                    os.close(child)
                    raise
                stack.append((directory, child_name, child_metadata, child, child_entries))
            else:
                # unlinkat removes a link entry; it never follows its target.
                os.unlink(child_name, dir_fd=directory)
    finally:
        for _, _, _, directory, entries in reversed(stack):
            entries.close()
            os.close(directory)


def delete(request):
    capabilities()
    if request.get("probe") is True:
        return
    name = component(request["name"])
    stage_name = component(request["stage"])
    proof_name = component(request["proof_name"])
    payload_name = component(request["payload"])
    if len({name, stage_name, proof_name, payload_name}) != 4:
        raise ValueError("Deletion control names must be distinct")
    parent = os.open(request["parent"], os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    stage = None
    isolated = False
    try:
        stage_metadata = lstat(parent, stage_name)
        stage = open_directory(parent, stage_name, stage_metadata)
        if stage_metadata.st_uid != os.geteuid() or stat.S_IMODE(stage_metadata.st_mode) != 0o700:
            raise RuntimeError("Private deletion directory ownership or permissions changed")
        proof = os.open(proof_name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=stage)
        try:
            proof_metadata = os.fstat(proof)
            expected_proof = request["proof"].encode("ascii")
            if (not stat.S_ISREG(proof_metadata.st_mode)
                    or proof_metadata.st_uid != os.geteuid()
                    or proof_metadata.st_size != len(expected_proof)
                    or os.read(proof, len(expected_proof) + 1) != expected_proof):
                raise RuntimeError("SFTP and SSH deletion namespaces could not be matched")
        finally:
            os.close(proof)
        if set(os.listdir(stage)) != {proof_name}:
            raise RuntimeError("Private deletion directory contains unexpected entries")

        # Capture the exact source through the parent matched by the proof.
        before = lstat(parent, name)
        expected = request["expected"]
        values = {"uid": before.st_uid, "gid": before.st_gid,
                  "size": before.st_size, "mtime": int(before.st_mtime),
                  "permissions": before.st_mode}
        for field, value in expected.items():
            if value is not None and values[field] != value:
                raise RuntimeError("Source changed after SFTP inspection: " + repr(name))
        os.rename(name, payload_name, src_dir_fd=parent, dst_dir_fd=stage)
        isolated = True
        after = lstat(stage, payload_name)
        if (identity(after) != identity(before) or after.st_size != before.st_size
                or after.st_mtime_ns != before.st_mtime_ns):
            raise RuntimeError("Source changed while being isolated; retained for recovery")
        remove_tree(stage, payload_name)
        isolated = False
        # Cleanup also uses the bound descriptors. Never recursively clean a
        # sidecar by pathname after an error or an uncertain SSH response.
        if identity(lstat(stage, proof_name)) != identity(proof_metadata):
            raise RuntimeError("Deletion completed but its proof entry changed")
        os.unlink(proof_name, dir_fd=stage)
        if identity(lstat(parent, stage_name)) != identity(stage_metadata):
            raise RuntimeError("Deletion completed but its private directory moved")
        os.rmdir(stage_name, dir_fd=parent)
    except BaseException as error:
        if isolated:
            recovery = os.path.join(request["parent"], stage_name, payload_name)
            raise RuntimeError(str(error) + "; remaining data retained at " + repr(recovery)) from error
        raise
    finally:
        if stage is not None:
            os.close(stage)
        os.close(parent)


def main():
    try:
        request = json.loads(base64.b64decode(sys.argv[1], validate=True))
        delete(request)
        print("COKACDIR_DELETE_OK", flush=True)
    except Exception as error:
        print("Safe remote deletion failed: " + str(error), file=sys.stderr, flush=True)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
