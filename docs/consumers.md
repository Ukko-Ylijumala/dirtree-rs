# The tree's next consumers — needs and gaps

This is an evaluation, made before the tree module's crate split, of
what the next two users of the tree need from it and what it still
lacks:

- **The WordPress scanner.** Still in planning (`docs/design.md` in its own repo, `wp-scanner`); its
  code is a stub.
- **A web malware scanner** for shared hosting servers, currently a
  separate project with its own walker. It scans 4–6M files per host,
  runs as root, and treats the files it scans as hostile.

## WordPress scanner

The visitor protocol (`docs/implementation.md`) covers most of what it
needs: prune by name and context, marker detection, scopes, the
discovery channel, cancellation, depth limits and stored tags. Two parts
its plan depends on do not work yet:

1. **Tagging the parent is a no-op.** `MarkerTarget::Parent` is only
   informational in `MarkerVisitor` (`src/visitors.rs`). Both the
   `WalkEvent` and the stored tag land on `wp-includes`, not on the WP
   root, so `DirTree::tagged(TAG_WP_ROOT)` returns the wrong
   directories.
2. **Claiming an install does not work.** The example marker
   (`version.php` under `wp-includes`) matches only once the walk is
   inside `wp-includes`. By then the root's other subdirectories,
   including `wp-content/uploads` (possibly hundreds of GB), are already
   queued. So `descend(false)` stops the walk inside `wp-includes` and
   nowhere else.

   The fix is to check the marker at the root itself:
   - It can require several entries (`wp-includes/`, `wp-admin/`,
     `wp-load.php`).
   - It can confirm a nested path such as `wp-includes/version.php` with
     one `fstatat` on the directory's open fd. That runs only when the
     first component is present in the listing.

   Then the real root is tagged, and it can be claimed before its
   subdirectories are queued.

Smaller gaps:

- **Pruning `uploads`.** The doc's `WpContentPathPruneVisitor` does not
  exist. A built-in visitor that prunes by trailing path components
  would cover it, and the malware scanner's prune patterns as well.
- **Keep the trie.** `wp-config.php` sometimes lives one level above the
  root. The parent's listing is already in the tree, so finding it is a
  lookup with no syscalls. That is a reason to keep the tree rather
  than use a discover-only walk.

## Malware scanner

How it works today:

- **Walk:** its own walker, one rayon task per directory.
- **Syscalls:** one `lstat` per non-directory entry.
- **Paths:** candidates are collected as full `String` paths.
- **Phases:** detection starts only after every walk has finished.
- **Time:** wall time is dominated by reading files. A run with a warm
  cache still spends minutes in system time. The cache never skips a
  read, by design, because attackers backdate mtimes.
- **Planned:** a resident daemon on fanotify filesystem marks. Its
  design explicitly rejects recursive per-directory inotify watches.

What the tree would not change is the speed of the nightly walk. The
scanner's walk is already parallel, and reads dominate. A snapshot
followed by `update()` does not help much either: a directory's ctime
catches entries that came or went, not a file edited in place, and the
scanner reads every candidate anyway.

Where it would gain:

1. **The resident daemon.** fanotify reports writes as the kernel sees
   them, so backdated mtimes do not matter. Between events the tree
   holds the host's whole state, with accounts and WP roots tagged and
   prunes applied, so only the changed files are read again. Snapshots
   let the daemon restart without a full walk.
2. **Opening files relative to their directory.**
   `openat(dirfd, name)` instead of a full path:
   - saves the kernel's path resolution, for millions of files;
   - cannot be redirected by a symlink swapped into the path;
   - needs no workaround for paths past `PATH_MAX`.

   The tree already has the directory handles.
3. **Context from the tree instead of syscalls.**
   - Sibling checks such as cloak rosters (`lstat` of named siblings),
     ancestor `read_dir`s, and parent or plugin slugs become lookups in
     memory.
   - Core-directory checks now match substrings of the path, and anyone
     can create `x/wp-includes/` (inferred). A tag on a WP root that was
     recognized by its markers is a stronger signal.
4. **Memory.** Millions of candidate `String`s become a directory
   reference plus a name index.
5. **Symlink escapes.** Finding links that point outside every account
   needs no extra syscalls, because the tree already holds the interned
   symlink targets.

## Gaps, by priority

Correctness, to fix before the split:

| # | Gap | Needed by |
|---|-----|-----------|
| 1 | **Non-UTF-8 names.** Names are converted lossily. A file with such a name comes back as a different path, cannot be opened, and two names can collapse into one entry: a place for malware to hide. Fix: a reversible escape into the string store. | both; critical for the malware scanner |
| 2 | **Markers.** Tag the parent, require several entries, check a nested path, and claim the matched subtree. | WP (and WP roots for the malware scanner) |
| 3 | **Error reporting.** Errors are a counter plus text events. A scanner that reports coverage needs each hole as a kind plus a path (listing failed, vanished, open failed…), through the observer. | malware scanner |

Features, after the split:

| # | Gap | Needed by |
|---|-----|-----------|
| 4 | `open_at(&NodeRef)`, and iterating candidates grouped by directory | both |
| 5 | **A per-file visitor hook.** It would get the kind and the entry, and could select a candidate and stream it, so detection overlaps the walk. Today `prune_child` gets only `is_dir`. | malware scanner; WP phase 2 |
| 6 | **Pruning by path-component globs**, built in (`*/domains/*/logs`, `wp-content/uploads`) | both |
| 7 | **Several roots** per tree, or one string store shared by several trees. Hosting servers spread accounts over several roots. | malware scanner |
| 8 | **A pluggable change source.** fanotify reports a directory by file handle, so this needs a directory-by-inode index or a lookup by path. The applying side mostly exists (`update()` of one path). | malware daemon |
| 9 | **Snapshots**, as planned in `docs/snapshot.md` | malware daemon |
| 10 | **Staying on one filesystem**, optionally. This costs one `fstat` per directory, on the fd the walker already holds. | nice to have |

Deliberately not proposed: per-file stat data (size, mode, mtime) on
every `FileEntry`. That would cost about 100 MB on a 4.6M-file host.
Both consumers need stat data only for candidates, and the per-file
hook (#5) can stat them lazily.

## Status

Gaps 1–3 were fixed before the split:

1. Lossless names: `src/osname.rs`. Names are interned with
   non-UTF-8 bytes as private-use escapes, the same scheme the malware
   scanner uses for its path strings.
2. Markers: every marker is evaluated at the directory it tags.
   Markers take several required entries and nested paths, `Parent`
   looks one level down, and `descend(false)` claims the subtree.
3. Faults: `TreeObserver::fault(&TreeFault)` receives a `FaultKind`,
   the exact path and the errno for every hole: a directory not opened
   or not fully listed, an entry not stat'ed, a symlink target not
   read, a directory not watched, or events lost.

### Batch 1, after the split (0.5.0)

Shaped by the scanner side's review of adopting the tree
(`zoner-malware-scan`, `doc/2026-10-06-dirtree-integration-review.md`):

- **Per-file hook (#5):** `Visitor::visit_file(&FileContext) ->
  FileVerdict`, for every non-directory entry the prunes and filters let
  through, whatever the file mode. The context has the entry's kind, a
  lazy cached stat, its interned name, its directory's `WalkContext`
  (with the directory's fd) and the whole listing for sibling lookups.
  `FileVerdict::Store` keeps a selected file in a directory skeleton
  (`FileMode::UNSET`).
- **Opening relative to the directory (#4, during the walk):**
  `WalkContext::dirfd`, and dirhandle 0.6.3's `EntryExt::open_regular()`
  / `open_regular_at()`: no symlink followed, no blocking on a FIFO,
  regular files only, with the stat of what was opened. Opening a file
  of a finished tree by its node, and grouping candidates per directory,
  are still open.
- **Per-walk hooks (part of #7):** `populate_par_with(path, recursive,
  &WalkHooks)` gives one walk a visitor and an observer of its own, so
  several walks into one tree at once (one per account) keep their
  findings, counts and faults apart while sharing the tree and its
  string store. Several roots per tree work this way; a shared store
  across trees is not needed for it.
- **No opens by path below a root:** the walker's spawned descents and
  `update()` open each directory relative to its parent's fd, so trees
  deeper than `PATH_MAX` walk and update, and a symlink swapped in for an
  ancestor cannot redirect them. (The watcher followed in batch 2.)

### Batch 2 (0.5.1)

- **No path below a walk root is trusted, anywhere:** `update()`'s root,
  `DirTree::handle()` and the watcher now open from the nearest pooled
  handle or caller-named walk root (`Directory::is_walk_root()`) with no
  symlink in any component (dirhandle 0.6.4's `path_fd_beneath()`). The
  watcher watches through `/proc/self/fd` links of fds it holds, opens
  each directory of a subtree from its parent's fd, and stats, reads
  and scans created entries from the parent's fd. A directory swapped
  for a symlink fails with `ELOOP` / `ENOTDIR` instead of leading
  elsewhere, and trees deeper than `PATH_MAX` are watched.
- **Opening files of a finished tree (#4):** `DirTree::path_fd(dir)`,
  `FileOpener` (keeps the last directory's fd, so candidates in walk or
  grouped order cost one directory open per directory) and
  `open_file(dir, name)`. Candidates kept as `(Arc<Directory>, name
  index)` open with no path built at all; the scanner's `open_stat`,
  `reaching` and `through_proc` have a replacement.
- **Path-component prunes (#6):** `PathPruneVisitor`, with the
  scanner's `path_match` semantics (trailing components, `*` within one
  component), on bytes. `matches()` answers outside a walk too (symlink
  targets, `pruned_below`).
- **Change sources (#8, the applying side):** `TreeChange`
  (`Dir`/`Subtree`/`Lost`), `DirTree::apply_changes()` (coalesces a
  batch, skips what is gone), `dir_by_fd()` (a fanotify file handle,
  opened, to its node, checked by inode) and `update_node()`. The
  fanotify source itself belongs to the daemon.
- **Snapshots (#9):** `save_to()` / `load_from()` and their background
  forms, as `docs/snapshot.md` describes.
- **One filesystem (#10):** `with_one_filesystem(true)`; each mount
  point skipped reaches `TreeObserver::mount_skipped()`.

### 0.5.2

Found while the WordPress scanner planned its incremental mode on
snapshots: `update()` did not consult the visitor. Every directory the
visitor had pruned looked new to the diff and was walked in full, out of
context (the first diff after a walk took in all of `uploads`), and with
a file mode that stores none, a file the visitor selected was never
stored when it came new or replaced.

- **`update()` honours the visitor:** each directory keeps the scope its
  entries were walked under (`Directory::children_scope()`, saved in
  snapshot format version 2). The diff rebuilds a directory's walk
  context from the tree, runs `visit_dir` on the fresh listing (its tag
  and scope refreshed, a declined directory keeping no entries), prunes
  and visits the files it adds or replaces as the walk does, walks new
  subtrees in their parent's context, and walks a subtree again in full
  when its scope changed. Depth caps hold as in the walk.
- **The walk stamps what it lists:** one `fstat` per directory, on the
  handle it holds, so a snapshot saved straight after a walk is a warm
  start. A node that held entries before the walk listed it keeps its
  stamp, since a walk adds but never removes.

### 0.5.3

Asked for by the quarantine tool beside the scanner (`zoner-quarantine`,
its design doc's §11: a convicted plugin directory is walked, packed and
unlinked entry by entry, and no path below the customer's home may be
trusted on the way):

- **A walk below an fd (#11):** `populate_par_beneath(path, &anchor,
  rel, recursive, &hooks)`. The walked directory is opened beneath an
  `O_PATH` fd the caller holds, with no symlink in any component
  (`DirHandle::open_beneath`), known to the tree by the name the caller
  gives and never opened by it. It stays pooled under its node, so
  `path_fd()`, `FileOpener`, `handle()` and `update()` below it resolve
  beneath the same open directory. The node comes back, for
  `traverse_from`.
- Removal primitives (#12 in that doc, `unlink_at` / `rmdir_at` on
  dirhandle) turned out not to be needed: nix's `unlinkat`, re-exported
  through dirhandle, takes the `O_PATH` fds the tree hands out.

### 0.5.4

Found while the WordPress scanner measured its incremental mode on
`/usr` (37,858 directories, warm cache): `update()` went through each
directory's subdirectories one after another and spawned tasks only past
`MAX_RECURSE_DEPTH` frames, so a whole tree's diff ran on one thread. An
update that skipped every directory took as long as the walk.

- **`update()` fans out like the walk:** the subdirectories to diff
  next, below a directory the pre-check skips and below one that is
  diffed, go to the pool with `par_iter`. A diff no longer holds a
  directory's listing handle while the subtrees below it are diffed.
  On `/usr`, an update skipping everything went from 145 ms to 27 ms,
  and a full diff from 1,206 ms to 69 ms.

### 0.5.5

Gaps found while the WordPress scanner's incremental mode was built,
where the docs said one thing and the code did another:

- **A diff walks a new subdirectory as the walk would:** in a tree that
  walks recursively, a diff that does not recurse (`update(path,
  Some(false))`, a `TreeChange::Dir`) now walks a subdirectory that
  appeared, as its docs said, instead of only inserting it. A directory
  moved in from outside shows only as an entry of its parent, so nothing
  else would list what it holds. A tree that walks one level still only
  inserts it.
- **The watcher consults the visitor:** with a visitor set, an entry
  created or moved in is left to a non-recursive diff of its directory,
  once per batch of events read. The visitor decides on it from the
  listing, in the scope and at the depth the walk gave the directory: a
  pruned directory stays out, a file it stores goes in whatever the file
  mode, a new subdirectory is walked in that context, and a marker
  created changes the directory's own verdict. A directory moved to
  another parent is walked again by a diff of that parent. The cost is a
  listing of each directory a batch created entries in, where the
  watcher inserted each entry on its own; without a visitor it still
  does.
- **The sync walk stamps:** `populate` stamps each directory it fills
  from empty with a complete listing, as the parallel walk does since
  0.5.2, so a diff after a sync walk skips what did not change.
- **`SCOPE_NONE` in a `Tag` leaves every scope:** the docs of
  `Verdict::Tag` said it kept the current one, which the walker never
  did (`docs/implementation.md` had it right). They now say so, and that
  `Verdict::tag()` starts its subtree under no scope.

Still open: a fanotify change source (the daemon's), and measuring the
memory of a real host's tree (Scale, below).

## Scale

On synthetic trees where every name is unique, the tree costs about
94 B per entry, strings included. A 6M-file host could then take
300–550 MB of resident memory, which matters on shared hosting servers.
Real trees reuse names far more, so this needs measuring on a real
host before the daemon is designed around it.
