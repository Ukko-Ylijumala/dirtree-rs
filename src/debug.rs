// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::dirtree::DirTree;
use super::node::Node;
use std::{sync::Arc, time::Instant};
use tracing::{Level, debug, info};

/// Print the full contents of a [[DirTree]] recursively, using [tracing]'s
/// facilities. This is a (very verbose) debugging function.
pub fn tree_print_debug(tree: &DirTree) {
    eprintln!("\n{tree:?}\n");
    tree.traverse(|node: &Arc<Node>| {
        if node.node_t.has_data() {
            if tracing::level_enabled!(Level::DEBUG) {
                debug!("{:?}", node.construct_path(tree.strings()));
            } else {
                info!("{}", tree.fs_path(node).unwrap().display());
            }
        }
    });
}

/**
Validate the counts of nodes, dirs, and files in a [[DirTree]].

We take the counts from the tree's [[TreeConf](super::TreeConf)] as master data and
firstly validate that the counts of directories and files add up to the
total number of nodes. Then we compare those to the counts we get by
traversing the tree with:
- `count_from()` (`traverse_from()` -> count)
- `iter_count()` (`iter()` -> count)
- `dirs().len()` and `files().len()` (`walk()` -> count)

We will also print the time it took to count the nodes using each method.

This is a debugging function using asserts, hence it will panic if the
counts do not match.
*/
pub fn tree_validate_counts(tree: &DirTree) {
    let conf = tree.conf();
    let want_n: u32 = conf.nodes();
    let want_d: u32 = conf.dirs();
    let want_f: u32 = conf.files();
    let d_o: &str = "[dirsonly]";
    if tree.filemode().is_name() {
        assert_eq!(want_n, want_d, "master node count != dirs {d_o}")
    } else {
        assert_eq!(want_n, want_d + want_f, "master node count != dirs+files")
    }

    /* ------------------------- */

    let start: Instant = Instant::now();
    let (nodes, dirs, files) = tree.count_from(tree.root());
    let n: &str = "count_from()";
    if tree.filemode().is_name() {
        assert_eq!(nodes, dirs, "{n} node count != dirs {d_o}");
        assert_eq!(files, 0, "{n} files != 0 {d_o}");
    } else {
        assert_eq!(nodes, dirs + files, "{n} node count != dirs+files");
        assert_eq!(want_f, files, "{n} files do not match");
    }
    assert_eq!(want_n, nodes, "{n} node count != master count");
    assert_eq!(want_d, dirs, "{n} dirs do not match");
    eprintln!(" --> {n} = {:?}", start.elapsed());

    /* ------------------------- */

    let start: Instant = Instant::now();
    let (nodes, dirs, files) = tree.iter_count();
    let n: &str = "iter_count()";
    if tree.filemode().is_name() {
        assert_eq!(nodes, dirs, "{n} node count != dirs {d_o}");
        assert_eq!(files, 0, "{n} files != 0 {d_o}");
    } else {
        assert_eq!(nodes, dirs + files, "{n} node count != dirs+files");
        assert_eq!(want_f, files, "{n} files do not match");
    }
    assert_eq!(want_n, nodes, "{n} node count != master count");
    assert_eq!(want_d, dirs, "{n} dirs do not match");
    eprintln!(" --> {n} = {:?}", start.elapsed());

    /* ------------------------- */

    let n: &str = "walk()";
    let start: Instant = Instant::now();
    let dirs: u32 = tree.dirs().count() as u32;
    eprintln!(" --> {n} dirs  = {:?}", start.elapsed());

    let start: Instant = Instant::now();
    let files: u32 = tree.files().count() as u32;
    eprintln!(" --> {n} files = {:#?}", start.elapsed());

    assert_eq!(want_d, dirs, "{n} dirs do not match");
    if tree.filemode().is_name() {
        assert_eq!(files, 0, "{n} files != 0 {d_o}");
    } else {
        assert_eq!(want_f, files, "{n} files do not match");
    }
}
