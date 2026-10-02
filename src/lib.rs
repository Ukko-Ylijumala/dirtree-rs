// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

/*!
Trie-based directory tree, scanned and held in memory.

The module is split into several submodules for clarity:
- `node`:     the building blocks of the trie: `Data`, `Entry<T>`, `Directory`, `FileEntry`, `FileKind`, `NodeType`, `NodeItem`, `Node`.
- `hash`:     [`DirTreeXxh3Hasher`] used for [`HashMap`](std::collections::HashMap) keys in the trie.
- `conf`:     [`TreeConf`] - atomic counters and feature flags.
- `event`:    [`TreeOp`], [`TreeState`], [`TreeEvent`], [`EventInfo`].
- `dirtree`:  [`DirTree`] itself and [`DirTreeIterator`].
- `filemode`: [`FileMode`] - how a walk handles the files it finds.
- `filters`:  [`Filters`] - regex name filters for files and directories.
- `error`:    [`TreeError`] and [`TreeResult`] for the fallible API.
- `traverse`: free traversal helpers over `Arc<Node>`.
- `observer`: [`TreeObserver`] - progress reporting out of the tree.
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
pub use dirtree::{DirTree, DirTreeIterator};
pub use error::{TreeError, TreeResult};
pub use event::{EventInfo, TreeEvent, TreeOp, TreeState};
pub use filemode::FileMode;
pub use filters::Filters;
pub use hash::DirTreeXxh3Hasher;
pub use node::{Directory, Entry, FileEntry, FileKind, Node, NodeItem, NodeType};
pub use observer::{NoopObserver, TreeObserver};
pub use traverse::{traverse_from, traverse_from_par, walk_nodes};
pub use update::UpdateStats;
pub use visitor::*;
pub use visitors::*;
pub use watch::TreeWatcher;
