# File operation safety

Local copy, move, rename, and deletion distinguish a symbolic link entry from
the object it points to. Relative link text is preserved when copying or
moving; as with filesystem rename, moving that text to a different parent can
change what it resolves to. Existing restrictions on protected link targets
remain in place and resolve links before applying `..` components.

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

TAR creation checks the completed temporary archive with the same extraction
backend and link validation as normal extraction before publishing it. This
detects links changed after source preflight. Verification uses a private
temporary extraction directory, requires enough free space for the extracted
contents, and adds a read/extraction pass. Cancellation or validation failure
prevents publication; cleanup is restricted to the owned temporary data.

The exact embedded remote helper has executable regression coverage:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p test_remote_delete.py -v
```

Rust regression tests cover local directory replacement, link copy/move,
protected target resolution, and TAR publication after link changes. Building
and running Rust tests follows the project's explicit-build permission rule.
