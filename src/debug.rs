// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::conf::NodeCounts;
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
firstly validate that the counts of directories, files and special files
add up to the total number of nodes. Then we compare those to the counts we get by
traversing the tree with:
- `count_from()` (`traverse_from()` -> count, name-only files included)
- `iter_count()` (`iter()` -> count)
- `dirs().len()` and `files().len()` (`walk()` -> count)

We will also print the time it took to count the nodes using each method.

This is a debugging function using asserts, hence it will panic if the
counts do not match.
*/
pub fn tree_validate_counts(tree: &DirTree) {
    let want: NodeCounts = tree.conf().counts();
    let name_only: bool = tree.filemode().is_name();
    if name_only {
        assert_eq!(want.nodes, want.dirs, "master node count != dirs [dirsonly]");
        assert_eq!(want.specials, 0, "master specials != 0 [dirsonly]");
    } else {
        assert_eq!(
            want.nodes,
            want.dirs + want.files + want.specials,
            "master node count != dirs+files+specials"
        );
    }

    /* ------------------------- */

    let start: Instant = Instant::now();
    assert_eq!(tree.count_from(tree.root()), want, "count_from() != master counts");
    eprintln!(" --> count_from() = {:?}", start.elapsed());

    // the iterators see nodes only, and a name-only file is not one
    let want: NodeCounts = match name_only {
        true => NodeCounts { files: 0, ..want },
        false => want,
    };

    let start: Instant = Instant::now();
    assert_eq!(tree.iter_count(), want, "iter_count() != master counts");
    eprintln!(" --> iter_count() = {:?}", start.elapsed());

    /* ------------------------- */

    let n: &str = "walk()";
    let start: Instant = Instant::now();
    let dirs: u32 = tree.dirs().count() as u32;
    eprintln!(" --> {n} dirs  = {:?}", start.elapsed());

    let start: Instant = Instant::now();
    // file nodes of every kind
    let files: u32 = tree.files().count() as u32;
    eprintln!(" --> {n} files = {:#?}", start.elapsed());

    assert_eq!(want.dirs, dirs, "{n} dirs do not match");
    assert_eq!(want.files + want.specials, files, "{n} files+specials do not match");
}
