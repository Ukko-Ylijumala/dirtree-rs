// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

/*!
Trie-based directory tree, scanned and held in memory.

The module is split into several submodules for clarity:
- [`node`]:     `Data`, `Entry<T>`, `Directory`, `FileEntry`, `NodeType`,
                `NodeItem`, and `Node` - the building blocks of the trie.
- [`hash`]:     [`DirTreeXxh3Hasher`] used for [`HashMap`] keys in the trie.
- [`conf`]:     [`TreeConf`] - atomic counters and feature flags.
- [`event`]:    [`TreeOp`], [`TreeState`], [`TreeEvent`], [`EventInfo`].
- [`dirtree`]:  [`DirTree`] itself and [`DirTreeIterator`].
- [`traverse`]: free traversal helpers over `Arc<Node>`.
- [`worker`]:   the background work-queue executor.
- [`visitor`]:  per-directory visitor protocol (recognition, prune, depth).
- [`visitors`]: built-in [`Visitor`] implementations.
- [`watch`]:    inotify-based resident-mode tree following.
- [`debug`]:    developer-facing diagnostic helpers.
- [`tests`]:    unit tests for the above.
*/

mod conf;
mod debug;
mod dirtree;
mod event;
mod hash;
mod node;
mod tests;
mod traverse;
mod visitor;
mod visitors;
mod watch;
mod worker;

pub use conf::TreeConf;
pub use debug::{tree_print_debug, tree_validate_counts};
pub use dirtree::{DirTree, DirTreeIterator};
pub use event::{EventInfo, TreeEvent, TreeOp, TreeState};
pub use hash::DirTreeXxh3Hasher;
pub use node::{Directory, Entry, FileEntry, Node, NodeItem, NodeType};
pub use traverse::{traverse_from, traverse_from_par, walk_nodes};
pub use visitor::*;
pub use visitors::*;
pub use watch::TreeWatcher;
