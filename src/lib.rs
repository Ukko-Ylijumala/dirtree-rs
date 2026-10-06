// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

/*!
A parallel directory walker that keeps the tree in memory as a trie:
directories as shared nodes, files as small values in their parent,
names interned once. It can stay resident and follow the filesystem
(an inotify watcher, a diff-rescan), and lets a [`Visitor`] recognize,
tag and prune subtrees while walking. Linux only.

Design notes live in `docs/`: the visitor protocol
(`implementation.md`), planned snapshots (`snapshot.md`), and what the
next consumers need (`consumers.md`).

The crate is split into several modules for clarity:
- `node`:     the building blocks of the trie: `Directory` (a shared node), `FileEntry` (stored by value), `Child`, `FileKind`, `NodeRef`, `NodeView`.
- `hash`:     [`DirTreeXxh3Hasher`] used for [`HashMap`](std::collections::HashMap) keys in the trie.
- `conf`:     [`TreeConf`] - atomic counters and feature flags.
- `event`:    [`TreeOp`], [`TreeState`], [`TreeEvent`], [`EventInfo`].
- `dirtree`:  [`DirTree`] itself and [`DirTreeIterator`].
- `filemode`: [`FileMode`] - how a walk handles the files it finds.
- `filters`:  [`Filters`] - regex name filters for files and directories.
- `error`:    [`TreeError`] and [`TreeResult`] for the fallible API.
- `traverse`: free traversal helpers from an `Arc<Directory>`, calling back with a `NodeView`.
- `observer`: [`TreeObserver`] - progress reporting out of the tree.
- `opener`:   [`FileOpener`] - opening a tree's files by their node, relative to their directory.
- `osname`:   lossless `str` encoding of non-UTF-8 filesystem names.
- `snapshot`: saving a tree to a file and loading it back (`docs/snapshot.md`).
- `update`:   diff-rescan (the [`TreeOp::Update`] primitive).
- `utils`:    small shared helpers (path splitting, weak refs, atomics).
- `worker`:   the background work-queue executor.
- `visitor`:  per-directory visitor protocol (recognition, prune, depth).
- `visitors`: built-in [`Visitor`] implementations.
- `watch`:    inotify-based resident-mode tree following.
- `debug`:    developer-facing diagnostic helpers.
- `tests`:    unit tests for the above.
*/

mod conf;
mod debug;
mod dirtree;
mod error;
mod event;
mod filemode;
mod filters;
mod hash;
mod node;
mod observer;
mod opener;
mod osname;
mod snapshot;
mod tests;
mod traverse;
mod update;
mod utils;
mod visitor;
mod visitors;
mod watch;
mod worker;

pub use conf::{NodeCounts, TreeConf};
pub use debug::{tree_print_debug, tree_validate_counts};
pub use dirtree::{DirTree, DirTreeIterator, WalkHooks};
pub use error::{TreeError, TreeResult};
pub use event::{EventInfo, FaultKind, TreeEvent, TreeFault, TreeOp, TreeState};
pub use filemode::FileMode;
pub use filters::Filters;
pub use hash::DirTreeXxh3Hasher;
pub use node::{Child, Directory, FileEntry, FileKind, NodeRef, NodeView};
pub use observer::{NoopObserver, TreeObserver};
pub use opener::FileOpener;
pub use osname::{decode_name, decode_os, decode_path, encode_name, encode_os, is_escaped};
pub use traverse::{traverse_from, traverse_from_par, walk_nodes};
pub use update::{TreeChange, UpdateStats};
pub use visitor::*;
pub use visitors::*;
pub use watch::TreeWatcher;
