# Visitor protocol — design and rationale

This document describes the visitor protocol added to `DirTree` to support
domain-specific scans (WordPress installations, code repositories, build
artifacts, etc.) without baking any domain knowledge into the tree itself.

The driving use case is the WordPress scanner sketched in the `wp-scanner`
repo's `docs/design.md`, but the protocol is intentionally general: the
same trait shape supports any "walk a tree, recognize subtrees, prune
aggressively, emit results as you go" workload.

## Goals and non-goals

Goals:

- Per-directory hook with full visibility into the just-read dirent list,
  fired before recursion. Lets a caller decide whether the current directory
  matches a marker pattern (e.g. "contains `version.php`").
- Per-child prune callback that operates on already-interned `u32` name
  indices, so common pruning rules (`.git`, `node_modules`, `wp-content/uploads`)
  cost a handful of `u32` equality checks per dirent and zero allocations.
- A "scope" concept that flows from a recognized ancestor down to its
  descendants, so prune decisions can be context-aware ("`uploads` is only
  pruned when the parent is `wp-content`").
- A streaming discovery channel so consumers can dispatch per-match work
  (rayon spawn, async task) before the walk completes.
- Cooperative cancellation so a "we found enough, stop" signal can be raised
  from the visitor or from outside.
- Per-scope max-depth so phase-2 scans (e.g. enumerating top-level entries
  of `plugins/`) can be expressed as part of the same walk if desired.

Non-goals:

- Reading file contents during the walk. Bounded reads (e.g. the 8 KB plugin
  header read in the WP scanner) belong in a downstream consumer driven by
  the discovery channel.
- DB enrichment, header parsing, or any other domain processing.
- Replacing `Filters` (regex include/exclude). The two coexist; visitors
  are evaluated alongside, not instead of, the regex filters.
- Hooks on the synchronous `populate` path. The visitor protocol is
  parallel-walker only (`populate_par_inner`). Sync mode stays a baseline.

## Why this shape

DirTree's existing parallel walker has three perf properties worth
preserving:

1. Low-level `nix::dir` iteration via `DirHandle` / `EntryExt` — no
   round-trip through `std::fs::DirEntry` and no `stat()` calls in the hot
   path (entry type and inode come from the `dirent` struct directly).
2. The string interner deduplicates path components and gives every name
   a stable `u32` index. Most comparisons in the hot path can be `u32`-eq.
3. `DirHandle::iter()` biases toward returning directory entries first via
   a small lookahead buffer, so subdirs reach rayon's queue earlier and
   work-stealing kicks in faster.

The visitor protocol is shaped so that all three properties survive:

- **Hot-path stays allocation-light.** The visitor's prune callback takes
  a pre-interned `u32` for the child name, not a string. The walker hoists
  the `strings.insert` for child directory names to one call per dirent,
  which it would have done anyway during `insert()`. Visitors that compare
  against a known set (e.g. `.git`, `node_modules`) cache the indices once
  at construction time.
- **No new `stat()` calls.** The visitor sees `&[EntryExt]` which already
  carries type and inode from `readdir`. Visitors that need `stat()` (for
  size or mode) can call it on the entries they care about; the framework
  doesn't call it on their behalf.
- **Dirs-first preserved.** When a visitor is configured the walker
  collects entries to a `Vec<EntryExt>` (so the visitor sees the whole
  list before recursion), then sorts dirs ahead of files before driving
  the parallel iterator. The sort is a single partition pass, not a full
  comparison sort.

The `Vec` collection is the only real cost added to the hot path, and it
is paid only when a visitor is configured. With no visitor set, the walker
runs the original `handle.iter().par_bridge().for_each(...)` path
unchanged.

## Trait shape

```rust
pub type ScopeTag = u16;
pub const SCOPE_NONE: ScopeTag = 0;

pub struct WalkContext<'a> {
    pub path: &'a Path,
    pub name_idx: u32,
    pub parent_name_idx: Option<u32>,
    pub depth: usize,
    pub scope: ScopeTag,
    pub strings: &'a UniqueStrStore,
}

pub struct DirContext<'a> {
    pub walk: &'a WalkContext<'a>,
    pub entries: &'a [EntryExt],
    pub dirfd: BorrowedFd<'a>,  // for type_at(): one fstatat below the dir
}

pub enum Verdict {
    Continue,
    SkipChildren,
    Tag { tag: ScopeTag, new_scope: ScopeTag, descend: bool },
}

pub trait Visitor: Send + Sync {
    fn visit_dir(&self, _ctx: DirContext<'_>) -> Verdict { Verdict::Continue }
    fn prune_child(&self, _parent: &WalkContext<'_>, _child_name_idx: u32, _is_dir: bool) -> bool { false }
    fn max_depth(&self, _scope: ScopeTag) -> usize { 0 }
}
```

`Verdict::Tag` rolls the previous `Tag`/`Claim` distinction into a single
variant with a `descend: bool`. `descend == false` means "the visitor owns
this subtree; do not recurse." Pass `walk.scope` as `new_scope` to keep
the active scope unchanged.

## Walk integration

When a visitor is configured on the tree, `populate_par_inner` runs the
following extra steps per directory:

1. Cancellation check. If `TreeConf::cancelled` is set, return early.
2. Depth cap. If `visitor.max_depth(scope) > 0` and `depth >= cap`, return.
3. Open the directory handle (unchanged).
4. Collect entries to `Vec<EntryExt>` and partition dirs-first.
5. Call `visitor.visit_dir(DirContext { walk: &ctx, entries: &v })`.
6. Apply the verdict:
    - `Continue`: recurse normally.
    - `SkipChildren`: do not recurse into children of this dir.
    - `Tag { descend: false }`: tag this dir, emit a `WalkEvent`, do not
      recurse.
    - `Tag { descend: true }`: tag this dir, emit a `WalkEvent`, recurse
      under the (possibly new) scope.
7. For each child entry, before insertion / recursion, call
   `visitor.prune_child(&ctx, child_name_idx, is_dir)`. If it returns
   `true`, the child is dropped.

The existing `Filters` regex include/exclude still runs alongside the
visitor's `prune_child`. They compose: prune wins, then filter. This means
existing CLI flags (`--exclude-dirs` etc.) keep working when a visitor is
enabled.

## Discovery channel

`TreeConf` gains an optional `Sender<WalkEvent>`. When the visitor returns
`Verdict::Tag`, the walker sends:

```rust
pub struct WalkEvent {
    pub path: PathBuf,
    pub tag: ScopeTag,
    pub scope: ScopeTag,
    pub depth: usize,
    pub claimed: bool,   // !descend
}
```

This is the single mechanism for streaming results out of the walker mid-scan.
A typed channel for richer payloads (e.g. `WpRoot { root: PathBuf, ... }`) is
the consumer's responsibility — the visitor can capture its own `Sender` and
emit alongside.

## Stored tags

The tag of every `Verdict::Tag` is also kept on the directory itself
(`Directory::tag: AtomicU16`, `SCOPE_NONE` = untagged), so a consumer can
query the finished tree instead of (or as well as) draining the channel:
`DirTree::tagged(tag)` iterates the directories carrying a tag. Each visit
of a directory refreshes its tag, so a rescan of it picks up a marker that
came or went. A diff-rescan (`update()`) or a watcher event does not re-run
the visitor on directories it does not scan in full, so their tags stay as
of their last walk. Re-evaluating them there would also need each
directory's scope and walk depth stored. A `FileEntry` has spare bytes for
a tag of its own, should visitors ever tag files.

## Cooperative cancellation

`TreeConf::cancelled: AtomicBool` is checked at the top of every
`populate_par_inner` call. To raise it from outside the walk:

```rust
tree.conf().cancel();
```

To raise it from inside a visitor: the visitor holds an `Arc<AtomicBool>`
or similar shared state and flips it directly. The framework does not own
the cancel signal beyond the flag check; the visitor owns the *policy*
("cancel after N matches", "cancel on first match", etc.).

## Built-in visitors

The library ships four small, composable visitors so common patterns
don't need a custom `Visitor` impl:

- **`NamePruneVisitor`** — prune a fixed set of dir names. Pre-interns
  the name set at construction; `prune_child` is a `u32` membership test.
  Use case: skip `.git`, `node_modules`, `vendor`, `__pycache__`, `target`,
  `.venv` etc. globally.

- **`MarkerVisitor`** — recognize directories by marker entries that
  must all be present: names in the entry list, or nested paths below
  the directory (`wp-includes/version.php`, one `fstatat` on the
  directory's fd, made only when the first component is listed as a
  directory). Each marker carries a `ScopeTag`, a `descend: bool`, and
  an optional `MarkerTarget` (`Self_` vs `Parent`) to distinguish "tag
  this dir" from "tag the parent of the dir holding the entries" (the
  WP pattern). Every marker is evaluated at the directory it tags,
  before that directory's subdirectories are walked, so a tag always
  lands where it belongs and `descend(false)` claims the whole subtree
  (a `Parent` marker looks one level down to do so: one subdirectory
  with `when_parent_is`, every subdirectory without). Multiple markers
  can be registered on one visitor; the first match wins.

- **`MaxDepthVisitor`** — enforce a global or per-scope depth cap.
  Useful when composed with markers to limit phase-2 scans.

- **`CompositeVisitor`** — fan out to N visitors. Verdict combination
  rule: most restrictive wins (`SkipChildren` > `Tag{descend:false}` >
  `Tag{descend:true}` > `Continue`). Multiple `Tag`s emit multiple
  `WalkEvent`s. Prune is OR (any inner pruning prunes). Max-depth is min
  (any inner depth cap caps).

## Worked example: WordPress scanner

```rust
let strings = tree.strings();
let visitor = CompositeVisitor::new()
    .add(NamePruneVisitor::new(strings, &[".git", "node_modules", "vendor"]))
    .add(WpContentPathPruneVisitor::new(strings))   // wp-content/uploads, wp-content/cache
    .add(
        MarkerVisitor::new()
            // checked at the WP root itself, so the claim stops the walk there
            .marker(Marker::file_named("wp-includes/version.php")
                .and_dir("wp-admin")
                .tag(TAG_WP_ROOT)
                .descend(false)),
    );

let (tx, rx) = crossbeam_channel::unbounded();
let tree = DirTree::new(filemode, filters)
    .from_path("/var/www")
    .with_visitor(Arc::new(visitor))
    .with_discovery_sink(tx)
    .build()?;

// Consumer pipeline (separate file):
for event in rx {
    if event.tag == TAG_WP_ROOT {
        rayon::spawn(move || scan_install(event.path));
    }
}
```

`scan_install` is downstream code that lives in the WP scanner crate. It
walks `wp-content/plugins/`, `wp-content/themes/`, `mu-plugins/`, reads
the first 8 KB of each candidate `.php` / `style.css`, and assembles a
`WpInstall` record. It never touches DirTree.

## What's not changed

- The synchronous `populate` path — sync mode walks unchanged. A
  configured visitor forces the parallel walker whatever the sync
  setting, as the sync walker would never invoke it.
- The `Filters` API and its CLI flags. They still apply.
- `TreeOp::Build/Scan/Remove/...` and `tree_worker`. The worker thread
  walks through `populate_auto`, like `walk()`: the parallel walker
  unless sync mode is on, and always when a visitor is set.
- Resident mode (`-R`) and `OpenHandles`. Orthogonal to the protocol.
- `FileMode` (`Node`/`Name`/`Stat`/`Ignore`/`Size`). The visitor sees
  the same entry list regardless.
- Memory accounting under the `size_of` feature.

## Future work (not in this PR)

- Tree snapshots (save to / load from a file, then `update()` to
  resync), planned next: see `docs/snapshot.md`.
- Removal hooks on `TreeObserver` (`dirs_removed` / `files_removed`,
  default no-ops), so an observer can track the live tree size through
  `update()` and the watcher. `UpdateStats` already carries the removal
  counts of an update.
- A lighter "discover-only" walk mode that does not retain the trie at
  all — the visitor-recognized paths are streamed and the tree itself
  is not built. Useful when the discovery is the only output.
