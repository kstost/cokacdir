# DIFF Copy and Delete Safety

The directory comparison screen can copy or delete the focused difference
without leaving DIFF. These operations use the comparison snapshot, plus a
full selected-directory snapshot captured when confirmation opens, as an
authorization boundary. If a root, parent, item, descendant, or previously
missing path no longer matches its snapshot, the operation stops instead of
acting on the new filesystem object.

## Controls

Press `Shift+C` on a focused difference to open the copy dialog.

| Key | Action |
| --- | --- |
| `Left` | Copy the right-side item to the left side |
| `Right` | Copy the left-side item to the right side |
| `Up`, `Down`, `Enter`, or `Esc` | Cancel |

An existing same-name destination is replaced only after its identity is
revalidated. A direction with no source item is unavailable.

Press `Delete` on a focused difference to open the delete dialog.

| Key | Action |
| --- | --- |
| `Left` | Delete the left-side item |
| `Right` | Delete the right-side item |
| `Down` | Delete every copy currently present |
| `Up`, `Enter`, or `Esc` | Cancel |

Directory deletion is recursive and cannot be undone. A direction for a side
where the item is absent is unavailable.

## Safety Invariants

The filesystem may change after a comparison is displayed or while a
confirmation dialog is open. DIFF therefore applies the following checks:

1. When comparison starts, it records the resolved identity of both roots and
   the identity of every existing item. For an absent item, it records the
   deepest existing real parent directory and the remaining missing path
   components.
2. Before opening a copy or delete confirmation, it verifies that the roots
   and selected items still match the comparison. An item that appeared,
   disappeared, changed type, or was replaced under the same name is rejected.
   For each selected real directory, it also records every descendant without
   following symlinks.
3. When the user chooses a direction, it verifies the roots, parents, source,
   every captured directory descendant, and any overwrite destination again
   before starting the worker. The worker repeats these checks around staging
   and commit boundaries.
4. For an absent copy destination, it requires the recorded parent to be the
   exact same directory and the first missing component to remain absent. It
   builds all confirmed-missing parents under one private name and publishes
   the completed hierarchy with one no-replace rename. A failure cannot leave
   a partially published parent hierarchy, and a component that appeared in
   the meantime is not reused.
5. Relative directory traversal refuses symlinks and `..` components. Deleting
   a selected symlink removes the link itself without following its target.
6. The copy backend rejects a source located inside an existing destination
   directory that the copy would replace. This check also applies when the
   source is a file or symlink, not only a directory.
7. Delete revalidates each requested target immediately before removing it,
   isolates a directory under a private name, and removes only descendants in
   the approved snapshot. A late or changed child stops cleanup and leaves the
   remaining recovery data intact. For a two-sided delete, present and absent
   sides are all checked before the first deletion; any later per-side failure
   is reported explicitly.
8. Every completed copy or delete attempt rebuilds the comparison, including
   cancelled and partially failed operations. Its result message remains
   visible during that rebuild, and selections for vanished entries are
   discarded when the new result arrives.

These checks are fail-closed. A rejection detected before the worker starts
leaves the confirmation dialog open with an error. A later rejection is
reported as an operation failure; neither case silently accepts the replacement
path.

## Overlapping Comparison Roots

Copy and delete are disabled when the resolved comparison roots are equal or
one is inside the other. For example, comparing `/data` with `/data/archive`
remains useful for viewing differences, but neither root is a safe independent
copy or delete boundary.

The check uses resolved paths and stable filesystem identities. Equal bind
mount aliases are rejected even when their path strings differ; a selected
directory snapshot is also rejected if it contains the other comparison root.
The copy backend independently checks ancestor identities before replacing a
directory. This prevents replacing a destination directory from also deleting
the source contained below it.

To perform mutations, reopen DIFF with two disjoint roots, such as sibling
directories under a common parent.

## Handling a Revalidation Error

Messages containing `changed`, `replaced`, `appeared`, or `overlap` indicate
that the displayed comparison is no longer a safe basis for the requested
mutation.

1. Cancel or close the confirmation dialog.
2. Refresh or reopen the comparison to capture the current filesystem state.
3. Review the selected difference again before copying or deleting it.
4. If the roots overlap, choose two disjoint comparison roots instead.

Do not retry against a stale dialog after another process has renamed,
replaced, or created part of the selected path.

## Implementation and Regression Coverage

The DIFF snapshot, prompt-time checks, worker preparation, and automatic
refresh are implemented in `src/ui/diff_screen.rs`. Reusable path identity,
missing-path authorization, symlink-safe traversal, and source/destination
relationship checks are implemented in `src/services/file_ops.rs`.

The findings, corrections, regression coverage, and final verification from
the 2026-09-03 defect review are recorded in
[`devdoc/diff-copy-delete-remediation.md`](../devdoc/diff-copy-delete-remediation.md).

Regression tests cover:

- replacement of an existing parent after comparison and after confirmation;
- creation of a path component that was previously missing;
- failure while privately staging a missing parent hierarchy;
- refusal to follow a symlink while resolving a missing destination;
- equal, nested, and identity-aliased comparison boundaries;
- a source contained by the destination directory it would replace;
- replacement, type changes, and descendant changes of copy and delete targets;
- late unapproved children after directory isolation and hard-linked children;
- a previously absent side appearing after a two-sided delete confirmation;
- operation-result visibility during refresh and stale-selection pruning;
- normal overwrite, missing-parent creation, directional copy, directional
  delete, cancellation, and comparison refresh behavior.
