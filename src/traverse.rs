// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::node::{Child, Directory, NodeRef, NodeView};
use crossbeam::queue::SegQueue;
use rayon::prelude::*;
use std::sync::Arc;

/**
Walk a tree's entries recursively from a [[Directory]] and put all found
entries into the given [SegQueue]. The `dirs` and `files` flags control
whether to include directories and/or files.
*/
pub fn walk_nodes(dir: &Arc<Directory>, q: &SegQueue<NodeRef>, dirs: bool, files: bool) {
    dir.children()
        .read()
        .iter()
        .par_bridge()
        .for_each(|(name, child): (&u32, &Child)| match child {
            Child::Dir(d) => {
                if dirs {
                    q.push(NodeRef::Dir(d.clone()));
                }
                walk_nodes(d, q, dirs, files);
            }
            Child::File(file) => {
                if files {
                    q.push(NodeRef::File { parent: dir.clone(), name: *name, file: *file });
                }
            }
        });
}

/**
Traverses a [[DirTree](super::DirTree)] recursively from a [[Directory]]
and applies function `f` to each entry, AND the starting directory itself.

`f` is called while the read lock of the directory whose children are
visited is held: it must not modify that directory (see [NodeView]).
*/
pub fn traverse_from<F>(dir: &Arc<Directory>, f: &mut F)
where
    F: FnMut(NodeView<'_>),
{
    // trace!(target: "traverse_from", "{}", node.path().display());
    f(NodeView::Dir(dir));
    dir.children()
        .read()
        .iter()
        .for_each(|(name, child): (&u32, &Child)| match child {
            // traverse directories first (depth-first search)
            Child::Dir(d) => traverse_from(d, f),
            Child::File(file) => f(NodeView::File { parent: dir, name: *name, file }),
        });
}

/**
Traverses a [[DirTree](super::DirTree)] recursively from a [[Directory]]
and applies function `f` to each entry, AND the starting directory
itself. Parallel version.

In contrast to `traverse_from()`, this function requires that the
fn `f` is `Send` and `Sync` since it will be sent to other threads.

Basically, to make this work you must use Atomic types or other thread-safe
primitives ([Mutex](parking_lot::Mutex), [RwLock](parking_lot::RwLock),
[AtomicCell](crossbeam::atomic::AtomicCell) etc) for any variables in `f`.
IOW, no interior mutability or shared mutable state.

Testing shows that this traversal is slower than the sequential version
for `f` which do just a simple operation on each node. This makes sense
since the overhead of moving stuff between threads can be significant.
*/
pub fn traverse_from_par<F>(dir: &Arc<Directory>, f: &F)
where
    F: Fn(NodeView<'_>) + Send + Sync,
{
    // trace!(target: "traverse_par", "{}", node.path().display());
    f(NodeView::Dir(dir));
    dir.children()
        .read()
        .iter()
        .par_bridge()
        .for_each(|(name, child): (&u32, &Child)| match child {
            // traverse directories first (depth-first search)
            Child::Dir(d) => traverse_from_par(d, f),
            Child::File(file) => f(NodeView::File { parent: dir, name: *name, file }),
        });
}
