# dirtree — a parallel directory walker with an in-memory trie

> [!WARNING]
> **WORK IN PROGRESS — pre-1.0, no stability guarantees.**
> The public API is **actively churning**. Breaking changes come with a
> minor version bump (`0.4.x` → `0.5.0`); patch releases stay non-breaking.
> The test suite only runs on the developer's machine (no CI), and the
> crate is **not published to crates.io**.

> [!IMPORTANT]
> **Linux-only.** The walker reads directories through
> [`dirhandle`](https://github.com/Ukko-Ylijumala/dirhandle-rs) (`nix`,
> `openat`, `fstatat`), the watcher uses raw inotify, and `readlinkat`
> reads symlink targets. It will not build on Windows, and macOS is untested.

## Overview

**dirtree** walks a directory tree in parallel (rayon) and keeps it in
memory as a trie. Directories are shared nodes (`Arc<Directory>`). A file,
of any kind, is a 16-byte value in its parent's map. Every name is
interned once in a string store. A tree can then stay resident and follow
the filesystem, and a visitor can recognize and prune subtrees during the
walk.

It started life as the tree mode of
[`statter`](https://github.com/Ukko-Ylijumala/statter), a filesystem
metadata precacher, and was extracted into its own crate at `0.4.0`. The
history and tags before that are statter's, filtered down to the tree
code. Each tag marks the tree code as it stood in that statter release.

## Features

- **Parallel walker** on `openat`-style directory handles: `d_type` only,
  no `stat()` per entry, and never follows a symlink below the root.
- **Compact trie:** about 25 bytes per file entry, with names interned as
  `u32` indices.
- **Lossless names:** bytes that are not UTF-8 are kept as private-use
  escapes (`encode_name` / `decode_name`), never replaced.
- **Special files:** symlinks, FIFOs, sockets and devices are recorded
  with their `FileKind`. Symlink targets are read with `readlinkat` and
  interned; links are never followed.
- **Resident mode:** directory handles can be kept open to pin the
  dentries. The inotify `TreeWatcher` follows changes, and `update()`
  diff-rescans a subtree, skipping unchanged directories on one `stat`
  each.
- **Visitor protocol:** markers, including nested paths such as
  `wp-includes/version.php`, tag and claim subtrees. Prunes and depth
  caps apply per scope. Discoveries stream out as `WalkEvent`s, and tags
  are stored on the directories. A per-file hook sees every file with its
  kind, a lazy stat and its siblings, can open it relative to its
  directory while the walk goes on, and can have it stored even when the
  tree keeps only directories.
- **Several walks into one tree** at once, each with a visitor and an
  observer of its own (`populate_par_with`, `WalkHooks`).
- **No path below a root is opened by name:** each directory is opened
  relative to its parent's fd, so trees deeper than `PATH_MAX` walk and
  update, and a symlink swapped in for an ancestor cannot redirect them.
- **Observer:** progress and every fault, as a `FaultKind` with the exact
  path and errno, go to a `TreeObserver`, so a consumer can account for
  coverage.
- **Optional memory accounting** via the `size_of` cargo feature.

## Usage

```rust
use dirtree::{DirTree, FileMode, Filters};

let tree: DirTree = DirTree::new(FileMode::NODE, Filters::default())
    .from_path("/srv/www")
    .with_recursive(true);
tree.walk()?;
eprintln!("{tree}");
```

`build()` instead of `walk()` starts a background worker that runs queued
`scan()` / `rescan()` operations. `TreeWatcher::start()` makes a built
tree follow the filesystem.

## Design notes

- [`docs/implementation.md`](docs/implementation.md): the visitor protocol
- [`docs/snapshot.md`](docs/snapshot.md): planned save/load of a tree
- [`docs/consumers.md`](docs/consumers.md): what the next consumers need,
  and the gaps still open

## License

MIT OR Apache-2.0
