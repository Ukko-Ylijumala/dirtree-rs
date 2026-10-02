// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::node::Node;
use crossbeam::queue::SegQueue;
use rayon::prelude::*;
use std::sync::Arc;

/**
Walk a tree's nodes recursively from a [[Node]] and put all found nodes into
the given [SegQueue]. The `dirs` and `files` flags control whether to include
directory and/or file nodes.
*/
pub fn walk_nodes(node: &Arc<Node>, q: &SegQueue<Arc<Node>>, dirs: bool, files: bool) {
    if node.is_traversable() && node.children().is_some() {
        node.children()
            .unwrap()
            .read()
            .values()
            .flatten()
            .par_bridge()
            .for_each(|child: &Arc<Node>| {
                if (dirs && child.node_t.is_dir()) || (files && child.node_t.is_file()) {
                    q.push(child.clone());
                }
                if child.is_traversable() {
                    walk_nodes(child, q, dirs, files);
                }
            });
    }
}

/// Traverses a [[DirTree](super::DirTree)] recursively from a [[Node]] and applies function `f`
/// to each child node, AND the starting node itself.
pub fn traverse_from<F>(node: &Arc<Node>, f: &mut F)
where
    F: FnMut(&Arc<Node>),
{
    // trace!(target: "traverse_from", "{}", node.path().display());
    f(node);
    if node.is_traversable() && node.children().is_some() {
        node.children()
            .unwrap()
            .read()
            .values()
            .flatten()
            .for_each(|child: &Arc<Node>| {
                if child.is_traversable() {
                    // traverse directories first (depth-first search)
                    traverse_from(child, f);
                } else {
                    f(child);
                }
            });
    }
}

/**
Traverses a [[DirTree](super::DirTree)] recursively from a [[Node]] and applies function `f`
to each child node, AND the starting node itself. Parallel version.

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
pub fn traverse_from_par<F>(node: &Arc<Node>, f: &F)
where
    F: Fn(&Arc<Node>) + Send + Sync,
{
    // trace!(target: "traverse_par", "{}", node.path().display());
    f(node);
    if node.is_traversable() && node.children().is_some() {
        node.children()
            .unwrap()
            .read()
            .values()
            .flatten()
            .par_bridge()
            .for_each(|child: &Arc<Node>| {
                if child.is_traversable() {
                    // traverse directories first (depth-first search)
                    traverse_from_par(child, f);
                } else {
                    f(child);
                }
            });
    }
}
