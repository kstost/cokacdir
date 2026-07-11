#!/usr/bin/env python3
"""Download PR before/after full source code for review."""

import sys
import os
import shutil
import json
import tarfile
import tempfile
import urllib.request
from contextlib import contextmanager
from pathlib import PurePosixPath

MAX_TARBALL_BYTES = 2 * 1024 * 1024 * 1024
MAX_EXTRACTED_BYTES = 4 * 1024 * 1024 * 1024


def _archive_parts(name):
    """Return a safe POSIX tar path, accounting for Windows path syntax."""
    if "\0" in name or "\\" in name:
        return None
    path = PurePosixPath(name)
    if path.is_absolute() or not path.parts or any(
        part == ".." for part in path.parts
    ):
        return None
    if os.name == "nt" and any(":" in part for part in path.parts):
        return None
    return path.parts


@contextmanager
def _snapshot_lock(output_dir):
    """Hold a crash-safe advisory lock for one output snapshot pair."""
    lock_path = os.path.join(output_dir, ".diffpr.lock")
    flags = os.O_RDWR | os.O_CREAT
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    fd = os.open(lock_path, flags, 0o600)
    lock_impl = None
    try:
        try:
            if os.name == "nt":
                import msvcrt

                if os.fstat(fd).st_size == 0:
                    os.write(fd, b"\0")
                os.lseek(fd, 0, os.SEEK_SET)
                msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
                lock_impl = msvcrt
            else:
                import fcntl

                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                lock_impl = fcntl
        except (BlockingIOError, OSError) as error:
            raise RuntimeError(
                f"another diffpr update is already using {output_dir}"
            ) from error

        yield
    finally:
        if lock_impl is not None:
            try:
                if os.name == "nt":
                    os.lseek(fd, 0, os.SEEK_SET)
                    lock_impl.locking(fd, lock_impl.LK_UNLCK, 1)
                else:
                    lock_impl.flock(fd, lock_impl.LOCK_UN)
            finally:
                os.close(fd)
        else:
            os.close(fd)


def fetch_json(url):
    req = urllib.request.Request(url, headers={"Accept": "application/vnd.github.v3+json"})
    with urllib.request.urlopen(req) as resp:
        return json.loads(resp.read())


def download_and_extract(tarball_url, dest_dir):
    """Download tarball and extract contents into dest_dir."""
    req = urllib.request.Request(tarball_url, headers={"Accept": "application/vnd.github.v3+json"})
    dest_dir = os.path.abspath(os.fspath(dest_dir))
    if os.path.lexists(dest_dir):
        raise FileExistsError(f"destination already exists: {dest_dir}")
    parent = os.path.dirname(dest_dir)
    os.makedirs(parent, exist_ok=True)
    stage_dir = tempfile.mkdtemp(prefix=".diffpr-extract-", dir=parent)

    try:
        with tempfile.SpooledTemporaryFile(max_size=16 * 1024 * 1024) as archive_file:
            with urllib.request.urlopen(req) as resp:
                downloaded = 0
                while True:
                    chunk = resp.read(1024 * 1024)
                    if not chunk:
                        break
                    downloaded += len(chunk)
                    if downloaded > MAX_TARBALL_BYTES:
                        raise ValueError(
                            f"tarball exceeds {MAX_TARBALL_BYTES} downloaded bytes"
                        )
                    archive_file.write(chunk)
            archive_file.seek(0)
            with tarfile.open(fileobj=archive_file, mode="r:gz") as tar:
                members = tar.getmembers()
                extracted_size = sum(member.size for member in members if member.isreg())
                if extracted_size > MAX_EXTRACTED_BYTES:
                    raise ValueError(
                        f"archive expands beyond {MAX_EXTRACTED_BYTES} bytes"
                    )
                member_paths = []
                for member in members:
                    parts = _archive_parts(member.name)
                    if parts is None:
                        raise ValueError(f"unsafe archive path: {member.name}")
                    member_paths.append((member, parts))
                prefixes = {parts[0] for _, parts in member_paths}
                if len(prefixes) != 1:
                    raise ValueError("tarball must contain exactly one top-level directory")
                prefix = next(iter(prefixes))

                directories = []
                for member, parts in member_paths:
                    if parts[0] != prefix:
                        raise ValueError(f"archive member is outside top-level directory: {member.name}")
                    relative_parts = parts[1:]
                    if not relative_parts:
                        if not member.isdir():
                            raise ValueError("top-level archive entry must be a directory")
                        continue
                    if member.name.startswith("/") or any(part in ("", ".", "..") for part in relative_parts):
                        raise ValueError(f"unsafe archive path: {member.name}")
                    if member.issym() or member.islnk():
                        raise ValueError(f"archive links are not allowed: {member.name}")
                    if not (member.isdir() or member.isreg()):
                        raise ValueError(f"special archive entry is not allowed: {member.name}")

                    target = os.path.join(stage_dir, *relative_parts)
                    if os.path.commonpath((stage_dir, os.path.abspath(target))) != stage_dir:
                        raise ValueError(f"unsafe archive path: {member.name}")

                    if member.isdir():
                        os.makedirs(target, exist_ok=True)
                        directories.append((target, member.mode & 0o777))
                        continue

                    os.makedirs(os.path.dirname(target), exist_ok=True)
                    source = tar.extractfile(member)
                    if source is None:
                        raise ValueError(f"could not read archive file: {member.name}")
                    with source, open(target, "xb") as output:
                        shutil.copyfileobj(source, output)
                    os.chmod(target, member.mode & 0o777)

                # Apply directory modes last so read-only directories do not block
                # extraction of their children.
                for path, mode in sorted(
                    directories,
                    key=lambda item: item[0].count(os.sep),
                    reverse=True,
                ):
                    os.chmod(path, mode)

        os.replace(stage_dir, dest_dir)
        stage_dir = None
    finally:
        if stage_dir is not None:
            shutil.rmtree(stage_dir, ignore_errors=True)


def update_snapshots(tarball_base, tarball_head, output_dir):
    """Build both snapshots in staging, then replace them as one transaction."""
    output_dir = os.path.abspath(os.fspath(output_dir))
    os.makedirs(output_dir, exist_ok=True)
    with _snapshot_lock(output_dir):
        _update_snapshots_locked(tarball_base, tarball_head, output_dir)


def _update_snapshots_locked(tarball_base, tarball_head, output_dir):
    """Update snapshots while the caller holds the output lock."""
    stage_dir = tempfile.mkdtemp(prefix=".diffpr-snapshots-", dir=output_dir)
    backup_dir = os.path.join(stage_dir, "backups")
    os.makedirs(backup_dir)
    states = {
        "before": {"backed_up": False, "published": False},
        "after": {"backed_up": False, "published": False},
    }
    preserve_stage = False

    try:
        staged_before = os.path.join(stage_dir, "before")
        staged_after = os.path.join(stage_dir, "after")
        download_and_extract(tarball_base, staged_before)
        download_and_extract(tarball_head, staged_after)

        try:
            for name in ("before", "after"):
                destination = os.path.join(output_dir, name)
                backup = os.path.join(backup_dir, name)
                staged = os.path.join(stage_dir, name)
                if os.path.lexists(destination):
                    os.replace(destination, backup)
                    states[name]["backed_up"] = True
                os.replace(staged, destination)
                states[name]["published"] = True
        except Exception as publish_error:
            rollback_errors = []
            for name in reversed(("before", "after")):
                destination = os.path.join(output_dir, name)
                backup = os.path.join(backup_dir, name)
                if states[name]["published"] and os.path.lexists(destination):
                    failed_new = os.path.join(stage_dir, f"failed-{name}")
                    try:
                        os.replace(destination, failed_new)
                    except Exception as error:
                        rollback_errors.append(
                            f"could not preserve failed {name} snapshot: {error}"
                        )
                if states[name]["backed_up"] and os.path.lexists(backup):
                    if os.path.lexists(destination):
                        rollback_errors.append(
                            f"could not restore {name}: destination is still occupied"
                        )
                    else:
                        try:
                            os.replace(backup, destination)
                        except Exception as error:
                            rollback_errors.append(
                                f"could not restore {name} backup: {error}"
                            )
            if rollback_errors:
                preserve_stage = True
                details = "; ".join(rollback_errors)
                raise RuntimeError(
                    f"snapshot publish failed ({publish_error}); rollback was incomplete. "
                    f"Recovery data is preserved at {stage_dir}: {details}"
                ) from publish_error
            raise
    finally:
        if not preserve_stage:
            shutil.rmtree(stage_dir, ignore_errors=True)


def main():
    if len(sys.argv) != 3:
        print(f"Usage: {sys.argv[0]} <pr_number> <output_dir>")
        sys.exit(1)

    pr_number = sys.argv[1]
    output_dir = sys.argv[2]
    if not pr_number.isascii() or not pr_number.isdecimal() or int(pr_number) <= 0:
        print("PR number must be a positive integer", file=sys.stderr)
        sys.exit(2)

    owner = "kstost"
    repo = "cokacdir"

    # Fetch PR info
    pr_url = f"https://api.github.com/repos/{owner}/{repo}/pulls/{pr_number}"
    print(f"Fetching PR #{pr_number} info...")
    pr_info = fetch_json(pr_url)

    base_sha = pr_info["base"]["sha"]
    head_sha = pr_info["head"]["sha"]
    print(f"Base SHA: {base_sha[:10]}  Head SHA: {head_sha[:10]}")

    tarball_base = f"https://api.github.com/repos/{owner}/{repo}/tarball/{base_sha}"
    tarball_head = f"https://api.github.com/repos/{owner}/{repo}/tarball/{head_sha}"

    print("Downloading before (base) and after (head)...")
    update_snapshots(tarball_base, tarball_head, output_dir)

    print(f"Done. Files saved to {output_dir}/before/ and {output_dir}/after/")


if __name__ == "__main__":
    main()
