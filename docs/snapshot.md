# Tree snapshots — plan and file format

Status: **planned, not implemented.** It is to be built after the tree
module is split into its own crate. Implementing it means replacing
the `TreeOp::Serialize` / `TreeOp::Deserialize` stubs (`src/tree/event.rs`,
`src/tree/worker.rs`) with `TreeOp::Save` / `TreeOp::Load`.

## Purpose

A snapshot saves a `DirTree` to a file so that a later process can
start from it instead of walking the filesystem again:

1. Load the snapshot. This makes no filesystem calls apart from
   reading the file and one `stat` of the root.
2. Run `update()` on the root. The diff-rescan's pre-check
   (`src/tree/update.rs`) skips a directory, on a single `stat`, when
   its ctime still equals the stamp stored for it. So only the
   directories that changed since the save are listed and diffed.

Everything else follows from this use: a snapshot holds what the
pre-check and the diff need, and nothing that only lives in a running
process.

## What is stored, what is not

Stored:

- **The root path, the root's device number (`st_dev`) and the
  filemode.**
- **The string table.** All entries are names and symlink targets, by
  index.
- **Per directory:**
  - name;
  - inode;
  - scan stamp (the directory's own ctime as of its last settled full
    diff, 0 = none; see `MTIME_SLACK_SECS`);
  - visitor tag.
- **Per file entry:** name, inode, `FileKind` and symlink target index.
- **The counts and the depth.** These are checked against what was
  actually loaded.

Not stored:

- **Open directory handles.** A resident tree reopens them by walking or
  updating.
- **The worker, the watcher, the observer, and the error and event
  logs.**
- **The filters and the visitor.** The caller configures these again on
  the new tree. The filters cannot be compared reliably (regexes), so
  they are the caller's responsibility.
  - A visitor's tags come back as saved, with the same limits as
    stored tags anyway: they are refreshed by walks only (see
    "Stored tags" in `implementation.md`).

A tree that has only been walked has no stamps yet; only `update()`
sets them. Saving such a tree is valid, but the first `update()` after
loading it diffs every directory in full. For a fast warm start, save
after an `update()`.

## Format (version 1)

The file is a stream, written and read through buffered I/O and never
memory-mapped. All integers are little-endian with no padding. Strings
and paths are a `u32` length plus that many bytes.

### Header

| Field         | Type     | Notes                                           |
|---------------|----------|-------------------------------------------------|
| magic         | `[u8;8]` | `b"DIRTREE\0"`                                  |
| version       | `u16`    | `1`; a reader rejects any version it does not know |
| flags         | `u16`    | reserved, `0`                                   |
| filemode      | `u8`     | `FileMode` bits                                 |
| string_base   | `u32`    | first user string index (stringstore's `LATIN1_NUM`, 256) |
| root_dev      | `u64`    | `st_dev` of the root                            |
| root_ino      | `u64`    | inode of the root                               |
| created       | `u64`    | the tree's creation time, seconds since epoch   |
| saved         | `u64`    | the time of the save, seconds since epoch       |
| nodes         | `u32`    | `TreeConf` counts, root excluded                |
| dirs          | `u32`    |                                                 |
| files         | `u32`    |                                                 |
| specials      | `u32`    |                                                 |
| depth         | `u8`     |                                                 |
| root_path     | bytes    | the tree's `from` path                          |

### String table

| Field   | Type          | Notes                                    |
|---------|---------------|------------------------------------------|
| count   | `u32`         | number of strings that follow            |
| strings | count × bytes | the strings at indices `string_base ..`, in index order |

Indices below `string_base` are stringstore's built-in Latin-1
single-character entries. They are the same in every store, so they are
not written and pass through unchanged on load. A reader whose
stringstore has a different `string_base` rejects the file.

When loading, each string is inserted in order, and the index it gets
goes into an old-to-new remap table (`Vec<u32>`). Every name and target
index in the tree section is translated through that table. So loading
does not depend on how the store assigns indices, and works into a
store that already holds strings. The table is dropped once loading
finishes.

### Tree section

The directories are written depth-first, each one before its
children, starting with the root. A directory record is followed by its
file records, then by its subdirectories' records, each with its own
files and subdirectories after it. The counts in each record give the
structure, so no parent links are stored. A reader rebuilds them with
a stack of `(Arc<Directory>, subdirectories left)`.

Directory record (30 bytes):

| Field   | Type  | Notes                                  |
|---------|-------|----------------------------------------|
| name    | `u32` | string index (the root's own name too) |
| inode   | `u64` |                                        |
| stamp   | `u64` | scan stamp, `0` = no baseline          |
| tag     | `u16` | `SCOPE_NONE` (0) = untagged            |
| n_files | `u32` | file records that follow directly      |
| n_dirs  | `u32` | subdirectory records after those       |

File record (17 bytes):

| Field  | Type  | Notes                                   |
|--------|-------|-----------------------------------------|
| name   | `u32` | string index                            |
| inode  | `u64` |                                         |
| kind   | `u8`  | `FileKind` as `repr(u8)`; a reader rejects an unknown value |
| target | `u32` | string index, `NO_TARGET` (`u32::MAX`) = not read / not a symlink |

`NO_TARGET` is written and read as is; it is never remapped.

As a size estimate, ignoring the strings: 100k directories and 1M files
take about 3 MB + 17 MB.

### Trailer

| Field    | Type     | Notes                                      |
|----------|----------|--------------------------------------------|
| checksum | `u64`    | xxh3-64 (default secret, seed 0) of every byte before the trailer |
| end      | `[u8;8]` | `b"DTREEEND"`                              |

The checksum is computed as the file is written and as it is read
(streaming `Hasher`, as `custom_xxh3::CustomXxh3Hasher::new_xxh3_defaults()`
provides). A truncated or corrupted file fails the check. That only
shows at the end of the file, so a reader builds the tree first and
discards it on failure; it never hands out a partial tree.

### Load-time checks

Any of these fails the load with `TreeError::BadSnapshot(reason)`:

- **The file itself:** bad magic, an unknown version, a different
  `string_base`, an unknown `FileKind`, a string index out of range, or
  a structure that does not close (records left over, or missing).
- **Integrity:** a checksum mismatch, or counts and depth that differ
  from the tree actually loaded.
- **The filemode:** it differs from the one the new tree was created
  with.
- **The root:** its current `st_dev` or inode differs from the header.
  Inode numbers mean nothing on another filesystem, so the caller should
  fall back to a fresh walk.
  - Caveat: `st_dev` is not stable across reboots on every setup (some
    device-mapper, NFS or btrfs subvolume setups). If that turns out to
    matter, a later version can add a flag to accept such a file and
    clear all stamps, so `update()` diffs everything once.
- **The target tree:** a snapshot loads only into a tree that is still
  uninitialized or empty. Otherwise the load fails with
  `TreeError::NotEmpty`.

I/O errors come back as `TreeError::Io(io::Error)`.

## Saving a live tree

A save holds each directory's children read lock only while writing
that directory's own record and its files. It clones the subdirectory
`Arc`s out, and recurses without the lock held. Writers are blocked for
one directory at a time, never for the whole tree.

A snapshot is therefore consistent per directory, not as a whole: the
watcher, or an operation that is not on the worker, can change the tree
between two directories. Ops queued on the worker do not interleave
with a background save, because the worker runs one op at a time.

The difference is harmless for the intended use. A directory that
changes after it was saved gets a newer ctime than its stored stamp, so
the `update()` after loading diffs it. The header counts are summed
while writing, not taken from `TreeConf`, so they always match the
records. `tree_validate_counts` after a load is the test for this.

## API

- `DirTree::save(&self, w: impl Write) -> TreeResult<()>` writes the
  snapshot to `w`.
- `DirTree::save_to(&self, path) -> TreeResult<()>` writes a temporary
  file next to `path`, syncs it, and renames it over `path`, so a crash
  never leaves a torn snapshot.
- `DirTree::load(&self, r: impl Read) -> TreeResult<()>` and
  `load_from(&self, path)` fill an uninitialized or empty tree. They set
  the root path from the header; if `from_path()` was also called, the
  two paths must match.
- Background forms, through the worker like `scan()` / `rescan()`:
  - `tree.save_bg(path)` and `tree.load_bg(path)` queue
    `TreeOp::Save(PathBuf)` and `TreeOp::Load(PathBuf)`.
  - Both fail with `TreeError::WorkerNotRunning` without a worker.
  - The tree is `Active(Save)` / `Active(Load)` while the op runs, and
    `Ready` once it is done. A failure is recorded as a tree error.
    A load that fails part-way leaves the tree empty, never partial.
- Builder use: `DirTree::new(mode, filters).build()?`, then
  `load_bg(path)` and `rescan(root)`. Or, with the blocking forms,
  `load_from(path)?` and then `update(root, None)?`.
- New `TreeError` variants: `Io(io::Error)`, `BadSnapshot(String)`,
  `NotEmpty`.

## statter integration (later)

A `--cache <file>` option for tree mode:

- If the file exists and loads, statter runs an `update()` of the root.
  Otherwise it walks as now.
- statter saves the file on exit. In resident mode (`-R`) it also saves
  periodically, and after the watcher's overflow recovery.

## Open points for implementation time

- Whether a compressed variant (zstd, as a flag bit) is worth it. Names
  compress well, but the string table is already deduplicated.
- Optional `serde` derives on the plain value types (`FileKind`,
  `NodeCounts`, `WalkEvent`) for consumers that want JSON. This is
  separate from the snapshot format, which does not use serde.
