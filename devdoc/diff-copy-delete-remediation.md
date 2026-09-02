# DIFF Copy/Delete Defect Remediation Record

## Status

- Review date: 2026-09-03
- Review baseline: `2ecf210` (`Fix MD5 filename verification edge cases`)
- Scope: uncommitted DIFF directional copy/delete work for version 0.8.22
- Result: all defects found during this review were corrected; no remaining
  defect was found in the reviewed scope after the final audit

This record describes the defect review and remediation work. The behavioral
contract and user-facing safety rules remain documented in
[`docs/diff-copy-delete-safety.md`](../docs/diff-copy-delete-safety.md).

## Findings and Corrections

### 1. Recursive directory authorization

**Problem:** Copy and delete revalidated only the selected top-level directory.
A descendant changed after confirmation could consequently be copied,
overwritten, or deleted without belonging to the confirmed snapshot.

**Correction:** A prompt-time `TreeAuthorization` now records the selected
directory and all descendants without following symlinks. The source tree is
verified before and after copy staging, and an overwrite destination is checked
before backup, after relocation, and again during cleanup. Directory deletion
removes only approved entries from the captured tree, bottom-up. Changed or
new descendants stop the operation and leave remaining data in recovery
staging.

Hard-linked descendants receive a narrowly scoped authorization refresh after
an approved alias is removed, avoiding a false failure caused only by the Unix
inode `ctime` update.

### 2. Identity-aliased and overlapping roots

**Problem:** Canonical path comparisons alone do not identify every Linux bind
mount or namespace alias. Two strings can describe the same directory, or a
selected directory can contain the other comparison root through such an
alias.

**Correction:** Root overlap checks now compare stable filesystem identities in
addition to resolved paths. Selected directory trees are rejected when they
contain the opposite comparison root. The copy backend also walks ancestor
identities when checking whether a source and destination contain one another.

### 3. Operation results hidden by automatic refresh

**Problem:** Copy/delete completion immediately starts a new comparison. The
comparison progress screen did not render the global result message, while its
timer continued to expire.

**Correction:** The comparison progress screen renders the result message and
the timer is paused while that progress screen is visible. The normal timeout
resumes when comparison finishes.

### 4. `Delete Both` with a previously absent side

**Problem:** `Delete Both` built its target list only from sides present when
the confirmation opened. If an absent side appeared later, the other side
could still be deleted while the new copy was silently left behind.

**Correction:** Present and absent sides are validated before scheduling any
deletion. Absence proofs are passed to the worker and checked again immediately
before its first mutation, covering changes while the worker thread is being
scheduled. A conflict preserves every requested target and is reported to the
user.

### 5. Partial creation of missing destination parents

**Problem:** Creating missing parent directories one component at a time could
leave a partially visible hierarchy when a later component failed.

**Correction:** All missing parent components are built beneath one private,
mode-0700 staging name in the last authorized existing parent. The completed
hierarchy is published using one atomic no-replace rename. On Unix, the staging
root receives its public permissions through its already verified directory
handle immediately before publication. Failures before publication cannot
leave a partial public hierarchy; post-publication verification failures are
marked unsafe to retry until DIFF is refreshed.

### 6. Stale selections after comparison refresh

**Problem:** Entries removed by a refreshed comparison could remain in
`selected_files`, leaving an incorrect selection count and stale state.

**Correction:** Both asynchronous and synchronous comparison rebuilds retain
only selections whose relative paths still exist in the new result.

## Main Implementation Areas

- `src/services/file_ops.rs`
  - tree capture, verification, and exact authorized deletion;
  - overwrite backup verification and recovery preservation;
  - identity-based source/destination ancestry checks;
  - atomic missing-parent staging and publication.
- `src/ui/diff_screen.rs`
  - prompt-time tree and absence authorization;
  - worker-bound copy/delete authorization;
  - overlap enforcement, result refresh, and selection pruning.
- `src/ui/draw.rs`
  - result-message rendering and timer behavior during comparison refresh.
- `docs/diff-copy-delete-safety.md` and `CHANGELOG.md`
  - the resulting behavior and safety guarantees.

## Regression Coverage

The remediation added or extended tests for:

- same-length descendant rewrites that leave the selected root unchanged;
- source and overwrite-destination descendants changed after confirmation;
- overwrite descendants changed after the original destination is backed up;
- changed descendants and late unapproved children during directory deletion;
- hard-linked descendants in an approved deletion tree;
- privately staged missing-parent failures without partial public directories;
- an absent side appearing after `Delete Both` confirmation and again before
  the delete worker starts;
- result-message visibility throughout automatic comparison refresh;
- stale-selection pruning when a refreshed entry disappears.

## Verification Results

The final native verification was run after the last remediation change:

| Check | Result |
| --- | --- |
| `cargo test --all-targets -- --test-threads=1` | 982 passed, 0 failed, 1 ignored |
| `cargo check --all-targets` | Passed |
| `cargo clippy --all-targets --message-format=short` | Passed; existing project warnings remain |
| `cargo fmt -- --check` | Passed |
| `git diff --check` | Passed |

The ignored test is the existing live external-integration test and is not part
of the DIFF copy/delete path.

Windows and macOS cross-target checks could not be completed in this Linux
environment because the required MinGW and Apple C compilers were unavailable;
the build stopped while compiling the `ring` dependency, before checking this
project's target-specific Rust code.

## Final Audit Conclusion

The final audit rechecked every original finding against its prompt-time,
pre-worker, worker, staging, publication, cleanup, refresh, and UI-result
boundaries. No remaining defect was found in the reviewed DIFF copy/delete
scope. Revalidation failures continue to fail closed, and uncertain committed
states preserve recovery data rather than deleting it.
