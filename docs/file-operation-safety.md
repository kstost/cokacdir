# File operation safety

Local copy, move, rename, and deletion distinguish a symbolic link entry from
the object it points to. Relative link text is preserved when copying or
moving; as with filesystem rename, moving that text to a different parent can
change what it resolves to. On Unix, copying a link follows the link-preserving
semantics of `cp -R`: the stored target path is copied verbatim, without
resolving or accessing the referent. This includes dangling links, link cycles,
and links to system paths. The same rule applies inside copied directories
and to moves that fall back to copying across filesystems. Source identity
checks and directory traversal without following links still apply.

Destination conflicts retain the file manager's explicit overwrite/skip policy.
An approved overwrite replaces the destination entry, including a destination
symlink or directory; it does not write through that link or merge directories
as `cp -R` can.

Windows directory links and junctions are leaf entries for deletion and tree
snapshots. Deleting one removes the link through its own handle; pathname
cleanup uses the Windows directory attribute to select the correct unlink API.

Duplicate removal skips links. It records the identities of scanned files and
their directory ancestry, then opens that ancestry relative to the held scan
root for hashing, comparison, and deletion. A changed ancestor or file is
rejected. Deletion and rollback use the same held directory handles.

Remote deletion requires an SSH command session and POSIX Python 3 supporting
directory-relative filesystem operations. The application probes this before
moving any source. Its embedded helper verifies a private proof directory made
over SFTP so an SSH/SFTP namespace mismatch cannot select another directory.
The helper isolates the selected entry and traverses directories with
`O_NOFOLLOW` and directory descriptors. If deletion fails partway through, the
error identifies the private recovery directory; it is not recursively cleaned
through a pathname. Servers that only permit SFTP can still be browsed, but
cannot provide these deletion guarantees and receive an actionable error.

TAR stores link targets verbatim, including absolute paths, external targets,
dangling links, and cycles. Creation does not require the target to be selected
or accessible. Extraction leaves those links intact without resolving them or
changing their target's permissions. The tar backend's default protections
against writing through symlinks and parent-directory traversal remain enabled.
Opening an archive through a symlink holds the selected target file open for
extraction. There is no extra archive copy or preliminary listing/decompression
pass. The backend reads the archive once, and its displayed entry names are
used only for progress, never interpreted as filesystem paths.

TAR creation publishes the temporary archive after the tar process succeeds and
the completed file is synced. It does not list or extract the completed archive,
so creation does not require space for an extracted copy or support for extraction
options. Cancellation, tar failure, or a publication error still reports failure;
publication never overwrites an existing destination. Supported special entries,
such as Unix FIFOs and device nodes, are passed to tar without opening their
contents. Sockets and unreadable entries still require exclusion confirmation.
Extraction relies on the backend's path protections and ownership/permission
options; it does not walk and reject the restored tree afterward.

TAR progress drains stdout and stderr concurrently for GNU tar and bsdtar,
including the Windows tar implementation. Non-UTF-8 or unusually long display
lines do not stop the process or become extra failure conditions. Cancellation
stops the backend even while it is silent; errors retain the backend diagnostic.

The exact embedded remote helper has executable regression coverage:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p test_remote_delete.py -v
```

File viewers follow links to regular files, reject special file targets, and
limit actual bytes read. Directory comparisons compare stored link text without
path normalization and never treat two failed link reads as equal. Encryption
and decryption skip links, including in their confirmation counts.

Rust regression tests cover local directory replacement, verbatim link copy/move,
Windows link deletion, archive round trips, outside-write prevention, and file
reading/comparison through links. Building and running Rust tests follows the
project's explicit-build permission rule.
