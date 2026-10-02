#![cfg(test)]
// Silence warning "creating a mutable reference to mutable static is discouraged".
// This can be done due to the way we're using the mutable reference in the tests.
#![allow(static_mut_refs)]

use super::*;
use super::utils::PATH_SEP;
use ctor::dtor;
use libc;
use parking_lot::Mutex;
use std::{
    collections::HashSet,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::Duration,
};
use miniutils::{EntryKind, PlannedEntry, Special, TreeSpec};
use tempfile::TempDir;

const TEST_NUM: [u64; 3] = [9, 11, 7];
const EXP_DIRS: u32 = (TEST_NUM[0] + TEST_NUM[0] * TEST_NUM[1]) as u32;
const EXP_FILES: u32 = (TEST_NUM[0] * TEST_NUM[1] * TEST_NUM[2] + TEST_NUM[0] + 1) as u32;
const EXP_NODES: u32 = EXP_DIRS + EXP_FILES;

// statics for all tests
static mut TESTDIR: Option<TempDir> = None;
static INITIALIZED: Mutex<bool> = Mutex::new(false);

/// Setup common test environment for all tests. Will initialize
/// the needed statics only once (due to the Mutex).
fn setup_tests() {
    let mut init = INITIALIZED.lock();
    if *init {
        // already initialized
        return;
    }
    unsafe {
        TESTDIR = Some(create_test_dirs_for_tree_test());
    }
    *init = true;
}

#[dtor]
fn teardown() {
    // println! or eprintln! in `dtor` will panic as Rust has already
    // shut down certain facilities. We can use libc::printf instead.
    unsafe {
        libc::printf(c"*** DirTree tests done, tearing down ***\n".as_ptr());
        if let Some(temp) = TESTDIR.take() {
            libc::printf(c" - Deleting temp directory...\n".as_ptr());
            temp.close().unwrap();
        }
        libc::printf(c"*** Teardown finished ***\n\n".as_ptr());
    }
}

/* --------------------------------- */

#[test]
fn test_create_empty_tree() {
    setup_tests();
    let tree: DirTree = DirTree::new(FileMode::default(), Filters::default());
    let (nodes, dirs, files, depth) = counts(&tree);

    assert_eq!(tree.root.node_t, NodeType::Root);
    assert_eq!(tree.conf.from, OnceLock::default());
    assert_eq!(tree.state(), TreeState::Uninitialized);
    assert!(tree.is_uninit(), "Tree is not uninitialized");
    tree_validate_counts(&tree);
    assert_eq!(nodes, 0, "nodes mismatch");
    assert_eq!(dirs, 0, "dirs mismatch");
    assert_eq!(files, 0, "files mismatch");
    assert_eq!(depth, 0, "depth mismatch");
    tree.conf.from.set(PathBuf::from("/foo")).ok();
    assert_eq!(tree.from().unwrap(), &PathBuf::from("/foo"));
}

#[test]
fn test_tree_new_from_path() {
    setup_tests();
    let path = unsafe { TESTDIR.as_ref().unwrap().path().to_str().unwrap() };

    let tree: DirTree = DirTree::new(FileMode::NODE, Filters::default()).from_path(path);
    let (nodes, dirs, files, depth) = counts(&tree);
    let root_depth: u8 = (path.split(PATH_SEP).count() - 1) as u8;

    tree_validate_counts(&tree);
    assert_eq!(tree.state(), TreeState::Empty);
    assert_eq!(nodes, root_depth.into(), "nodes mismatch");
    assert_eq!(dirs, root_depth.into(), "dirs mismatch");
    assert_eq!(files, 0, "files mismatch");
    assert_eq!(depth, root_depth, "depth mismatch");
}

#[test]
fn test_tree_new_from_path_recursive() {
    let (_, tree, root_depth) = create_test_tree(true);
    let (nodes, dirs, files, depth) = counts(&tree);
    check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);
}

#[test]
fn test_tree_build_thread() {
    setup_tests();
    let path = unsafe { TESTDIR.as_ref().unwrap().path().to_str().unwrap() };

    let tree: Arc<DirTree> = DirTree::new(FileMode::NODE, Filters::default())
        .from_path(path)
        .build()
        .unwrap();
    assert!(tree.worker.lock().is_some(), "Worker not initialized");

    // scan is non-blocking, so we must wait for it to finish
    tree.scan(path, Some(true));
    while !tree.is_ready() {
        std::thread::sleep(Duration::from_millis(10));
    }

    tree_validate_counts(&tree);
    let (nodes, dirs, files, depth) = counts(&tree);
    let root_depth: u8 = (path.split(PATH_SEP).count() - 1) as u8;
    check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);
    tree.quit_worker(true);
}

#[test]
fn test_tree_contains() {
    let (path, tree, root_depth) = create_test_tree(true);
    let (nodes, dirs, files, depth) = counts(&tree);
    check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);

    let mut ctr: u32 = 0;
    for p in path_generator(path) {
        assert!(tree.contains(&p), "Not found: {}", p);
        ctr += 1;
    }
    assert!(tree.contains(path), "Root not found: {}", path);
    assert!(!tree.contains(""), "Found an empty path");
    assert_eq!(ctr, EXP_DIRS + EXP_FILES, "All paths not accounted for");
    for &p in ["foo", "bar/foo", "/foo/baz", ".", "..", "../"].iter() {
        assert!(!tree.contains(p), "Found a nonexistent path: {p}");
    }
}

#[test]
fn test_tree_traversals() {
    let (path, tree, root_depth) = create_test_tree(true);
    let (nodes, dirs, files, depth) = counts(&tree);
    check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);

    let exp: HashSet<String> = path_generator(path);
    let p_iter: HashSet<String> = tree.iter_paths().collect();
    let p_walk: HashSet<String> = tree
        .nodes()
        .filter_map(|n: Arc<Node>| tree.fs_path(&n))
        .map(|p: PathBuf| p.to_string_lossy().to_string())
        .collect();
    let mut p_trav: HashSet<String> = HashSet::new();
    tree.traverse(|n: &Arc<Node>| {
        p_trav.insert(n.path(&tree.strings).to_string_lossy().to_string());
    });

    assert!(exp.is_subset(&p_iter), "iter() paths mismatch: {:?}", exp.difference(&p_iter));
    assert!(exp.is_subset(&p_walk), "walk() paths mismatch: {:?}", exp.difference(&p_walk));
    assert!(exp.is_subset(&p_trav), "traverse() paths mismatch: {:?}", exp.difference(&p_trav));
}

#[test]
fn test_tree_subcounts() {
    let (path, tree, root_depth) = create_test_tree(true);
    let (nodes, dirs, files, depth) = counts(&tree);
    check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);

    let mut l1_dirs: HashSet<String> = HashSet::new();
    let mut l2_dirs: HashSet<String> = HashSet::new();
    for l1_idx in 0..TEST_NUM[0] {
        l1_dirs.insert(format!("{}/level_1_{l1_idx}", path));
        for l2_idx in 0..TEST_NUM[1] {
            l2_dirs.insert(format!("{}/level_1_{l1_idx}/level_2_{l2_idx}", path));
        }
    }

    let (exp_l1_num, exp_l2_num) = (TEST_NUM[0], TEST_NUM[0] * TEST_NUM[1]);
    assert_eq!(l1_dirs.len(), exp_l1_num as usize, "L1 dirs num mismatch (test error)");
    assert_eq!(l2_dirs.len(), exp_l2_num as usize, "L2 dirs num mismatch (test error)");

    for p in l1_dirs.iter() {
        let node: Arc<Node> = tree.get_node(p).expect("get_node() should return a node");
        let (l1_n, l1_d, l1_f) = validate_counts_below_node(&tree, node);

        let dirs_exp: u64 = TEST_NUM[1] + 1; // +1 for the dir itself
        let files_exp: u64 = TEST_NUM[1] * TEST_NUM[2] + 1; // +1 for the extra test file in L1
        let nodes_exp: u64 = dirs_exp + files_exp;

        assert_eq!(l1_d, dirs_exp, "L1 dir count != expected");
        assert_eq!(l1_f, files_exp, "L1 file count != expected");
        assert_eq!(l1_n, nodes_exp, "L1 node count != expected");
    }

    for p in l2_dirs.iter() {
        let node: Arc<Node> = tree.get_node(p).expect("get_node() should return a node");
        let (l2_n, l2_d, l2_f) = validate_counts_below_node(&tree, node);

        assert_eq!(l2_d, 1, "L2 dir count != expected");
        assert_eq!(l2_f, TEST_NUM[2], "L2 file count != expected");
        assert_eq!(l2_n, TEST_NUM[2] + 1, "L2 node count != expected");
    }
}

#[test]
fn test_tree_removals() {
    let (path, tree, root_depth) = create_test_tree(true);
    let (mut nodes, mut dirs, mut files, depth) = counts(&tree);
    check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);

    let l1_idx: u64 = TEST_NUM[0] - 1;
    let file: String = format!("{path}/level_1_{0}/file-{0}_0.bin", l1_idx);
    let l2_p: String = format!("{path}/level_1_{}/level_2_{}", l1_idx - 1, TEST_NUM[1] - 1);
    let l1_p: String = format!("{path}/level_1_{}", l1_idx - 2);
    for &p in [&file, &l2_p, &l1_p].iter() {
        assert!(tree.contains(p), "Node/path not found (test error): {p}");
    }

    let (l2_n, l2_d, l2_f) =
        validate_counts_below_node(&tree, tree.get_node(&l2_p).expect("L2 node not found"));
    let (l1_n, l1_d, l1_f) =
        validate_counts_below_node(&tree, tree.get_node(&l1_p).expect("L1 node not found"));

    match tree.remove(&file) {
        Ok(_) => {
            tree_validate_counts(&tree);
            nodes -= 1;
            files -= 1;
            let (n_now, d_now, f_now, _) = counts(&tree);
            assert!(!tree.contains(&file), "File found after removal: {file}");
            assert_eq!(n_now, nodes, "Node count mismatch [file]");
            assert_eq!(d_now, dirs, "Dir count not equal [file]");
            assert_eq!(f_now, files, "File count mismatch [file]");
        }
        Err(e) => panic!("Error removing file: {e}"),
    }

    match tree.remove(&l2_p) {
        Ok(_) => {
            tree_validate_counts(&tree);
            nodes -= l2_n as u32;
            dirs -= l2_d as u32;
            files -= l2_f as u32;
            let (n_now, d_now, f_now, _) = counts(&tree);
            assert!(!tree.contains(&l2_p), "L2 dir found after removal: {l2_p}");
            assert_eq!(n_now, nodes, "Node count mismatch [L2]");
            assert_eq!(d_now, dirs, "Dir count mismatch [L2]");
            assert_eq!(f_now, files, "File count mismatch [L2]");
        }
        Err(e) => panic!("Error removing L2 dir {l2_p}: {e}"),
    }

    match tree.remove(&l1_p) {
        Ok(_) => {
            tree_validate_counts(&tree);
            nodes -= l1_n as u32;
            dirs -= l1_d as u32;
            files -= l1_f as u32;
            let (n_now, d_now, f_now, _) = counts(&tree);
            assert!(!tree.contains(&l1_p), "L1 dir found after removal: {l1_p}");
            assert_eq!(n_now, nodes, "Node count mismatch [L1]");
            assert_eq!(d_now, dirs, "Dir count mismatch [L1]");
            assert_eq!(f_now, files, "File count mismatch [L1]");
        }
        Err(e) => panic!("Error removing L1 dir {l1_p}: {e}"),
    }
}

#[test]
fn test_node_hash_no_recursion() {
    setup_tests();
    let tree: DirTree = DirTree::new(FileMode::default(), Filters::default());
    // Hashing a data-less node (Root) used to recurse infinitely.
    let mut hasher: DefaultHasher = DefaultHasher::new();
    tree.root().hash(&mut hasher);
    let _ = hasher.finish();
}

#[test]
fn test_hardlink_names() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    let spec: TreeSpec = TreeSpec::new().root_files(1).with(Special::Hardlink, 2);
    spec.create(temp.path()).unwrap();

    let tree: DirTree = walked_tree(dir, FileMode::NODE, false);
    /*
    Hardlinked files share an inode, so an equality-based child lookup
    cannot tell the siblings apart - name resolution must be by identity.
    */
    for name in ["file-0.bin", "hardlink-0", "hardlink-1"] {
        let p: String = format!("{dir}/{name}");
        let node: Arc<Node> = tree.get_node(&p).expect("hardlinked node should exist");
        assert_eq!(tree.node_name(&node), name, "hardlink resolved to wrong sibling");
    }
}

#[test]
fn test_name_mode_removal() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir(temp.path().join("sub")).unwrap();
    std::fs::write(temp.path().join("sub/afile.bin"), b"x").unwrap();

    let tree: DirTree = walked_tree(dir, FileMode::NAME, false);
    assert_eq!(tree.conf().files(), 1, "name-only file not counted");

    let p: String = format!("{dir}/sub/afile.bin");
    match tree.remove(&p) {
        Ok(Some((0, 0, 1))) => {}
        other => panic!("Name-only entry removal failed: {other:?}"),
    }
    assert_eq!(tree.conf().files(), 0, "file count not decremented");
    assert!(matches!(tree.remove(&p), Ok(None)), "second removal should be a no-op");
    tree_validate_counts(&tree);
}

#[test]
fn test_get_node_does_not_intern() {
    let (path, tree, _) = create_test_tree(true);
    let before: usize = tree.strings().len();
    assert!(!tree.contains(&format!("{path}/nonexistent_component_xyz")));
    assert_eq!(tree.strings().len(), before, "lookup must not grow the string store");
}

#[test]
fn test_concurrent_same_path_insert() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir_all(temp.path().join("a/b/c")).unwrap();

    let tree: DirTree = DirTree::new(FileMode::NODE, Filters::default()).from_path(dir);
    let target: PathBuf = temp.path().join("a/b/c");
    let (n0, d0) = (tree.conf().nodes(), tree.conf().dirs());

    // racing inserters of the same path must create each node exactly once
    rayon::scope(|s| {
        for _ in 0..8 {
            let (t, p) = (&tree, &target);
            s.spawn(move |_| t.insert(p, NodeType::Directory, None));
        }
    });
    assert_eq!(tree.conf().nodes(), n0 + 3, "duplicate node creation in racing inserts");
    assert_eq!(tree.conf().dirs(), d0 + 3, "duplicate dir creation in racing inserts");
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_update_diff() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir(temp.path().join("sub")).unwrap();
    std::fs::write(temp.path().join("sub/a.bin"), b"x").unwrap();
    std::fs::create_dir(temp.path().join("gone")).unwrap();
    std::fs::write(temp.path().join("gone/g.bin"), b"g").unwrap();

    let tree: DirTree = walked_tree(dir, FileMode::NODE, false);
    tree_validate_counts(&tree);

    // a no-op pass must diff everything and change nothing
    let stats: UpdateStats = tree.update(dir, Some(true)).expect("update failed");
    assert!(!stats.changed(), "no-op update changed something: {stats}");
    assert!(stats.scanned_dirs >= 3, "root, sub and gone should be diffed: {stats}");

    // mutate the filesystem behind the tree's back
    std::fs::write(temp.path().join("sub/new.bin"), b"n").unwrap();
    std::fs::create_dir(temp.path().join("newdir")).unwrap();
    std::fs::write(temp.path().join("newdir/inner.bin"), b"i").unwrap();
    std::fs::remove_dir_all(temp.path().join("gone")).unwrap();
    // replace a.bin via rename-over: guarantees a different inode
    std::fs::write(temp.path().join("sub/tmp.bin"), b"r").unwrap();
    std::fs::rename(temp.path().join("sub/tmp.bin"), temp.path().join("sub/a.bin")).unwrap();

    let stats: UpdateStats = tree.update(dir, Some(true)).expect("update failed");
    assert_eq!(stats.added_dirs, 1, "newdir should be added: {stats}");
    assert_eq!(stats.added_files, 2, "new.bin + replacement a.bin: {stats}");
    assert_eq!(stats.removed_dirs, 1, "gone should be removed: {stats}");
    assert_eq!(stats.removed_files, 2, "g.bin + old a.bin: {stats}");
    assert_eq!(stats.replaced, 1, "a.bin should count as replaced: {stats}");
    assert_eq!(stats.errors, 0, "no errors expected: {stats}");

    assert!(tree.contains(&format!("{dir}/sub/new.bin")), "new.bin missing");
    assert!(tree.contains(&format!("{dir}/newdir/inner.bin")), "inner.bin missing");
    assert!(tree.contains(&format!("{dir}/sub/a.bin")), "replaced a.bin missing");
    assert!(!tree.contains(&format!("{dir}/gone")), "gone still present");
    tree_validate_counts(&tree);

    // and a second pass is a no-op again
    let stats: UpdateStats = tree.update(dir, Some(true)).expect("update failed");
    assert!(!stats.changed(), "second update changed something: {stats}");
}

/**
Whether a walk records a planned entry, as the tree's policy stands:
directories and regular files, which a hardlink is too, but no symlinks
(not followed either) and no FIFOs or sockets.
*/
fn recorded(e: &PlannedEntry) -> bool {
    matches!(e.kind, EntryKind::Dir | EntryKind::File | EntryKind::Special(Special::Hardlink))
}

#[test]
fn test_tree_special_entries() {
    // every special kind, in the root and two levels down
    let with_all = |spec: TreeSpec| Special::ALL.into_iter().fold(spec, |s, k| s.with(k, 1));
    let spec: TreeSpec = with_all(with_all(TreeSpec::new().root_files(1)).level(2, 1).level(2, 2));
    let counts = spec.counts();
    for (mode, sync) in [(FileMode::NODE, false), (FileMode::NODE, true), (FileMode::NAME, false)] {
        let temp: TempDir = TempDir::new().unwrap();
        let dir: &str = temp.path().to_str().unwrap();
        spec.create(temp.path()).unwrap();
        let tree: DirTree = DirTree::new(mode, Filters::default())
            .from_path(dir)
            .with_recursive(true)
            .with_sync(sync);
        tree.walk().unwrap();

        let ctx: String = format!("{mode:?}, sync={sync}");
        assert_eq!(tree.conf().errors(), 0, "{ctx}: errors");
        let files: u64 = counts.files + counts.special(Special::Hardlink);
        assert_eq!(tree.conf().files() as u64, files, "{ctx}: files");
        if mode.is_node() {
            // name-only files have no node to look up
            for e in spec.plan(temp.path()) {
                let p = e.path.to_string_lossy();
                assert_eq!(tree.contains(&p), recorded(&e), "{ctx}: {p}");
            }
        }
        tree_validate_counts(&tree);

        // a diff-rescan sees the same entries as the walk: nothing to add or remove
        let stats: UpdateStats = tree.update(dir, Some(true)).unwrap();
        let changed: u32 = stats.added_dirs + stats.added_files + stats.removed_dirs
            + stats.removed_files + stats.replaced;
        assert_eq!(changed, 0, "{ctx}: {stats}");
        // the root and every directory diffed, none skipped by the pre-check
        assert_eq!(stats.scanned_dirs as u64, counts.dirs + 1, "{ctx}: {stats}");
        assert_eq!(tree.conf().errors(), 0, "{ctx}: update errors");
    }
}

#[test]
fn test_tree_deep_walk() {
    // deeper than MAX_RECURSE_DEPTH, so descents run as spawned tasks too
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    let depth: usize = 3 * dirtree::MAX_RECURSE_DEPTH + 1;
    /*
    A chain of single directories with one file at the bottom, and in
    the first one a symlink to "..": a symlinked directory is recorded
    as neither dir nor file, and not followed into the loop.
    */
    let spec: TreeSpec = (2..depth)
        .fold(TreeSpec::new().level(1, 0).with(Special::SymlinkDir, 1), |s, _| s.level(1, 0))
        .level(1, 1);
    spec.create(temp.path()).unwrap();
    let plan: Vec<PlannedEntry> = spec.plan(temp.path()).collect();
    let path_of = |kind: EntryKind| plan.iter().find(|e| e.kind == kind).unwrap().path.clone();
    let leaf: PathBuf = path_of(EntryKind::File);
    let link: PathBuf = path_of(EntryKind::Special(Special::SymlinkDir));

    let tree: DirTree = walked_tree(dir, FileMode::NODE, false);
    assert_eq!(tree.conf().errors(), 0, "deep walk reported errors");
    assert!(tree.contains(&leaf.to_string_lossy()), "deepest file missing");
    assert!(!tree.contains(&link.to_string_lossy()), "symlink should not be recorded");
    let root_depth: u64 = (dir.split(PATH_SEP).count() - 1) as u64;
    assert_eq!(tree.conf().dirs() as u64, root_depth + depth as u64, "dirs miscounted");
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_resident_handles_released() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    for d in ["a", "a/x", "b"] {
        std::fs::create_dir(temp.path().join(d)).unwrap();
    }

    let tree: DirTree = walked_tree(dir, FileMode::NODE, true);
    assert_eq!(tree.handles_len(), 4, "walk root, a, a/x and b should be pinned");

    // re-populating a known subtree must not pool a second set of handles
    tree.populate_par(&PathBuf::from(format!("{dir}/a")), Some(true));
    assert_eq!(tree.handles_len(), 4, "re-populate leaked handles");

    // removing a subtree closes the handles of all of its directories
    tree.remove(&format!("{dir}/a")).expect("remove failed");
    assert_eq!(tree.handles_len(), 2, "removed subtree kept its handles open");
    assert!(tree.handle(dir).is_some(), "walk root handle should still be pooled");
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_update_rejects_unscanned() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir(temp.path().join("sub")).unwrap();

    let tree: DirTree = walked_tree(dir, FileMode::NODE, false);
    let nodes: u32 = tree.conf().nodes();

    // the trie root and the intermediate nodes above the walk root
    let parent: String = temp.path().parent().unwrap().to_string_lossy().into_owned();
    for p in [PATH_SEP, parent.as_str()] {
        let res = tree.update(p, Some(true));
        assert!(res.is_err(), "update({p}) should be rejected: {res:?}");
    }
    assert_eq!(tree.conf().nodes(), nodes, "rejected updates must not touch the tree");

    // the walk root and its subdirs remain updatable
    tree.update(dir, Some(true)).expect("update of the tree root failed");
    tree.update(&format!("{dir}/sub"), Some(true)).expect("update of sub failed");
}

#[test]
fn test_tree_update_diff_name_mode() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir(temp.path().join("sub")).unwrap();
    std::fs::write(temp.path().join("sub/a.bin"), b"x").unwrap();

    let tree: DirTree = walked_tree(dir, FileMode::NAME, false);
    assert_eq!(tree.conf().files(), 1, "name-only file not counted");

    std::fs::write(temp.path().join("sub/b.bin"), b"y").unwrap();
    std::fs::remove_file(temp.path().join("sub/a.bin")).unwrap();

    let stats: UpdateStats = tree.update(dir, Some(true)).expect("update failed");
    assert_eq!(stats.added_files, 1, "b.bin should be added: {stats}");
    assert_eq!(stats.removed_files, 1, "a.bin should be removed: {stats}");
    assert_eq!(tree.conf().files(), 1, "file count should be steady");
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_update_mtime_precheck() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir(temp.path().join("sub")).unwrap();
    std::fs::write(temp.path().join("sub/a.bin"), b"x").unwrap();
    std::fs::create_dir(temp.path().join("sub2")).unwrap();
    std::fs::write(temp.path().join("sub2/b.bin"), b"y").unwrap();

    let tree: DirTree = walked_tree(dir, FileMode::NODE, false);

    /*
    Freshly built: fs timestamps and node scan times are within the
    slack window, so the pre-check must fall through to full diffs.
    The pass refreshes the baselines - but only a baseline that is
    clearly NEWER than the fs timestamps allows skipping, hence the
    sleep before the refreshing pass.
    */
    std::thread::sleep(Duration::from_millis(3500));
    let s1: UpdateStats = tree.update(dir, Some(true)).expect("update failed");
    assert_eq!(s1.skipped_dirs, 0, "first pass must diff everything: {s1}");
    assert!(!s1.changed(), "first pass changed something: {s1}");

    // now the refreshed baselines dominate: one stat per dir, no diffs
    let s2: UpdateStats = tree.update(dir, Some(true)).expect("update failed");
    assert_eq!(s2.scanned_dirs, 0, "second pass should diff nothing: {s2}");
    assert_eq!(s2.skipped_dirs, 3, "root, sub and sub2 should be skipped: {s2}");
    assert!(!s2.changed(), "second pass changed something: {s2}");

    // a new entry bumps its dir's mtime and forces a real diff there only
    std::fs::write(temp.path().join("sub/new.bin"), b"n").unwrap();
    let s3: UpdateStats = tree.update(dir, Some(true)).expect("update failed");
    assert_eq!(s3.added_files, 1, "new.bin should be found: {s3}");
    assert_eq!(s3.scanned_dirs, 1, "only sub should be diffed: {s3}");
    assert_eq!(s3.skipped_dirs, 2, "root and sub2 should be skipped: {s3}");
    assert!(tree.contains(&format!("{dir}/sub/new.bin")), "new.bin missing");
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_watcher_renames() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir(temp.path().join("sub")).unwrap();
    std::fs::write(temp.path().join("sub/a.bin"), b"x").unwrap();
    std::fs::create_dir_all(temp.path().join("dir1/inner")).unwrap();
    std::fs::write(temp.path().join("dir1/inner/d.bin"), b"d").unwrap();
    let outside: TempDir = TempDir::new().unwrap();

    let tree: Arc<DirTree> =
        Arc::new(walked_tree(dir, FileMode::NODE, false));
    let watcher: Arc<TreeWatcher> =
        TreeWatcher::start(tree.clone()).expect("watcher should start");

    // same-dir file rename
    std::fs::rename(temp.path().join("sub/a.bin"), temp.path().join("sub/renamed.bin")).unwrap();
    wait_for(|| tree.contains(&format!("{dir}/sub/renamed.bin")), "renamed file should appear");
    wait_for(|| !tree.contains(&format!("{dir}/sub/a.bin")), "old file name should vanish");

    // same-dir directory rename: subtree and node identity must survive
    let d1: Arc<Node> = tree.get_node(&format!("{dir}/dir1")).expect("dir1 missing");
    std::fs::rename(temp.path().join("dir1"), temp.path().join("dir2")).unwrap();
    wait_for(
        || tree.contains(&format!("{dir}/dir2/inner/d.bin")),
        "renamed dir's contents should be reachable under the new name",
    );
    assert!(!tree.contains(&format!("{dir}/dir1")), "old dir name should vanish");
    let d2: Arc<Node> = tree.get_node(&format!("{dir}/dir2")).expect("dir2 missing");
    assert!(Arc::ptr_eq(&d1, &d2), "in-place rename should preserve node identity");

    // the renamed subtree's watches must still be live
    std::fs::write(temp.path().join("dir2/inner/e.bin"), b"e").unwrap();
    wait_for(
        || tree.contains(&format!("{dir}/dir2/inner/e.bin")),
        "events under the renamed dir should still be tracked",
    );

    // cross-directory file move
    std::fs::rename(temp.path().join("sub/renamed.bin"), temp.path().join("dir2/moved.bin"))
        .unwrap();
    wait_for(|| tree.contains(&format!("{dir}/dir2/moved.bin")), "moved file should appear");
    wait_for(|| !tree.contains(&format!("{dir}/sub/renamed.bin")), "moved file source vanish");

    // cross-directory dir move (re-created via rescan)
    std::fs::create_dir(temp.path().join("sub/mvdir")).unwrap();
    std::fs::write(temp.path().join("sub/mvdir/m.bin"), b"m").unwrap();
    wait_for(|| tree.contains(&format!("{dir}/sub/mvdir/m.bin")), "mvdir contents scanned");
    std::fs::rename(temp.path().join("sub/mvdir"), temp.path().join("dir2/mvdir")).unwrap();
    wait_for(|| tree.contains(&format!("{dir}/dir2/mvdir/m.bin")), "moved dir contents appear");
    wait_for(|| !tree.contains(&format!("{dir}/sub/mvdir")), "moved dir source vanish");

    // move out of the tree: detached immediately, dropped on expiry
    std::fs::rename(temp.path().join("dir2/moved.bin"), outside.path().join("moved.bin"))
        .unwrap();
    wait_for(|| !tree.contains(&format!("{dir}/dir2/moved.bin")), "moved-out file vanish");

    // move into the tree from outside: an uncorrelated MOVED_TO = create
    std::fs::rename(outside.path().join("moved.bin"), temp.path().join("sub/back.bin"))
        .unwrap();
    wait_for(|| tree.contains(&format!("{dir}/sub/back.bin")), "moved-in file should appear");

    watcher.stop();
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_no_root() {
    // no from_path(): nothing to walk or build from
    let tree: DirTree = DirTree::new(FileMode::NODE, Filters::default());
    let res = tree.from();
    assert!(matches!(res, Err(TreeError::NoRoot)), "{res:?}");
    let res = tree.walk();
    assert!(matches!(res, Err(TreeError::NoRoot)), "{res:?}");
    let res = DirTree::new(FileMode::NODE, Filters::default()).build();
    assert!(matches!(res, Err(TreeError::NoRoot)), "{res:?}");
}

#[test]
fn test_tree_visitor_forces_parallel() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    for d in ["keep", "skip"] {
        std::fs::create_dir(temp.path().join(d)).unwrap();
        std::fs::write(temp.path().join(d).join("f.bin"), b"x").unwrap();
    }

    // the sync walker would ignore the visitor and record skip/ as well
    let tree: DirTree = DirTree::new(FileMode::NODE, Filters::default())
        .from_path(dir)
        .with_recursive(true)
        .with_sync(true);
    let visitor = NamePruneVisitor::new(tree.strings(), &["skip"]);
    let tree: DirTree = tree.with_visitor(Arc::new(visitor));
    tree.walk().unwrap();
    assert!(tree.contains(&format!("{dir}/keep/f.bin")));
    assert!(!tree.contains(&format!("{dir}/skip")), "the visitor did not run");
    tree_validate_counts(&tree);
}

/// Counts what a [TreeObserver] is told, for [test_tree_observer].
#[derive(Debug, Default)]
struct CountingObserver {
    dirs: AtomicU64,
    files: AtomicU64,
    bytes: AtomicU64,
}

impl TreeObserver for CountingObserver {
    fn dirs_added(&self, n: u64) {
        self.dirs.fetch_add(n, Relaxed);
    }

    fn files_added(&self, n: u64, bytes: u64) {
        self.files.fetch_add(n, Relaxed);
        self.bytes.fetch_add(bytes, Relaxed);
    }
}

#[test]
fn test_tree_observer() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir_all(temp.path().join("a/b")).unwrap();
    std::fs::write(temp.path().join("top.bin"), b"12345").unwrap();
    std::fs::write(temp.path().join("a/one.bin"), b"123").unwrap();
    std::fs::write(temp.path().join("a/b/two.bin"), b"12").unwrap();

    // both walkers report the same: the root, a, a/b and three files
    for sync in [false, true] {
        let obs: Arc<CountingObserver> = Arc::new(CountingObserver::default());
        let tree: DirTree = DirTree::new(FileMode::NODE | FileMode::SIZE, Filters::default())
            .from_path(dir)
            .with_recursive(true)
            .with_sync(sync)
            .with_observer(obs.clone());
        tree.walk().unwrap();
        assert_eq!(obs.dirs.load(Relaxed), 3, "dirs, sync={sync}");
        assert_eq!(obs.files.load(Relaxed), 3, "files, sync={sync}");
        assert_eq!(obs.bytes.load(Relaxed), 10, "bytes, sync={sync}");
    }
}

#[test]
fn test_tree_rescan_via_worker() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::write(temp.path().join("a.bin"), b"x").unwrap();

    let tree: Arc<DirTree> = DirTree::new(FileMode::NODE, Filters::default())
        .from_path(dir)
        .build()
        .unwrap();
    tree.scan(dir, Some(true));
    let p_a: String = format!("{dir}/a.bin");
    wait_for(|| tree.contains(&p_a), "initial scan should find a.bin");

    // mutate and let the queued Update op reconcile the tree
    std::fs::write(temp.path().join("b.bin"), b"y").unwrap();
    std::fs::remove_file(temp.path().join("a.bin")).unwrap();
    tree.rescan(dir);
    wait_for(|| tree.contains(&format!("{dir}/b.bin")), "rescan should add b.bin");
    wait_for(|| !tree.contains(&p_a), "rescan should remove a.bin");

    tree.quit_worker(true);
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_watcher_prepared_before_walk() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    let spec: TreeSpec = TreeSpec::new().root_files(1).level(2, 2).level(2, 1);
    spec.create(temp.path()).unwrap();

    let tree: Arc<DirTree> = Arc::new(
        DirTree::new(FileMode::NODE, Filters::default())
            .from_path(dir)
            .with_recursive(true),
    );
    let watcher: Arc<TreeWatcher> = TreeWatcher::prepare(tree.clone()).expect("prepare failed");
    tree.walk().unwrap();
    // every directory was watched as the walk reached it
    assert_eq!(watcher.watches_len() as u64, spec.counts().dirs + 1, "watches after the walk");

    // changed after the walk, before the event loop: queued by the kernel until run()
    let sub: PathBuf = temp.path().join("level_1_0/level_2_1");
    std::fs::write(sub.join("late.bin"), b"x").unwrap();
    std::fs::create_dir(sub.join("late_dir")).unwrap();
    std::fs::write(sub.join("late_dir/inner.bin"), b"y").unwrap();
    std::fs::remove_file(temp.path().join("file-0.bin")).unwrap();
    watcher.run().expect("run failed");
    assert!(watcher.run().is_err(), "a second run() must fail");

    for p in ["late.bin", "late_dir", "late_dir/inner.bin"] {
        let p: String = sub.join(p).to_string_lossy().into_owned();
        wait_for(|| tree.contains(&p), &format!("{p} should appear"));
    }
    let gone: String = format!("{dir}/file-0.bin");
    wait_for(|| !tree.contains(&gone), "removed file should vanish");
    watcher.stop();
    assert_eq!(tree.conf().errors(), 0, "watcher reported errors");
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_watcher_events_during_rename() {
    /*
    Events for a subtree between its MOVED_FROM and MOVED_TO cannot be
    applied while it is detached. The real race needs events from other
    threads inside one rename(), so the events are injected here, into a
    watcher whose event loop never runs.
    */
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir_all(temp.path().join("a/sub")).unwrap();
    std::fs::write(temp.path().join("a/sub/old.bin"), b"x").unwrap();
    let tree: Arc<DirTree> = Arc::new(walked_tree(dir, FileMode::NODE, false));
    let watcher: Arc<TreeWatcher> = TreeWatcher::prepare(tree.clone()).unwrap();
    let wd_root: i32 = watcher.wd_of(temp.path()).unwrap();
    let wd_sub: i32 = watcher.wd_of(&temp.path().join("a/sub")).unwrap();

    // rename a -> b, with a file created in b/sub between the two halves
    std::fs::rename(temp.path().join("a"), temp.path().join("b")).unwrap();
    watcher.inject(wd_root, libc::IN_MOVED_FROM | libc::IN_ISDIR, 7, "a");
    std::fs::write(temp.path().join("b/sub/new.bin"), b"y").unwrap();
    watcher.inject(wd_sub, libc::IN_CREATE, 0, "new.bin");
    watcher.inject(wd_root, libc::IN_MOVED_TO | libc::IN_ISDIR, 7, "b");

    assert!(tree.contains(&format!("{dir}/b/sub/old.bin")), "renamed subtree lost its file");
    assert!(tree.contains(&format!("{dir}/b/sub/new.bin")), "file created mid-rename lost");
    assert!(!tree.contains(&format!("{dir}/a")), "old name still in the tree");
    watcher.stop();
    assert_eq!(tree.conf().errors(), 0, "watcher reported errors");
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_watcher_move_across_dirs() {
    // level_1_0/level_2_0 (a subtree two levels deep) moves under level_1_1
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    let spec: TreeSpec = TreeSpec::new().level(2, 0).level(1, 1).level(2, 2).level(2, 1);
    spec.create(temp.path()).unwrap();
    let tree: Arc<DirTree> = Arc::new(walked_tree(dir, FileMode::NODE, true));
    let (files, dirs, handles) = (tree.conf().files(), tree.conf().dirs(), tree.handles_len());
    let watcher: Arc<TreeWatcher> = TreeWatcher::start(tree.clone()).expect("watcher should start");

    let from: PathBuf = temp.path().join("level_1_0/level_2_0");
    let to: PathBuf = temp.path().join("level_1_1/moved");
    std::fs::rename(&from, &to).unwrap();
    let deep: String = to.join("level_3_1/level_4_1/file-0_0_1_1_0.bin").to_string_lossy().into();
    wait_for(|| tree.contains(&deep), "moved subtree should appear in its new place");
    assert!(!tree.contains(&from.to_string_lossy()), "old place still in the tree");
    assert_eq!(tree.conf().files(), files, "files after the move");
    assert_eq!(tree.conf().dirs(), dirs, "dirs after the move");
    assert_eq!(tree.handles_len(), handles, "pooled handles after the move");
    tree_validate_counts(&tree);

    // the moved directories are still watched, now as their new nodes
    let late: PathBuf = to.join("level_3_1/late.bin");
    std::fs::write(&late, b"x").unwrap();
    let late: String = late.to_string_lossy().into();
    wait_for(|| tree.contains(&late), "file in the moved subtree should appear");
    watcher.stop();
    assert_eq!(tree.conf().errors(), 0, "watcher reported errors");
}

#[test]
fn test_tree_watcher_move_rebuilds_from_memory() {
    /*
    A move to another directory is rebuilt from the tree, not rescanned:
    a file added without its event being processed stays unseen. The
    events are injected into a watcher whose event loop never runs.
    */
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir_all(temp.path().join("src/a/sub")).unwrap();
    std::fs::create_dir(temp.path().join("dst")).unwrap();
    std::fs::write(temp.path().join("src/a/sub/known.bin"), b"x").unwrap();
    let tree: Arc<DirTree> = Arc::new(walked_tree(dir, FileMode::NODE, false));
    let watcher: Arc<TreeWatcher> = TreeWatcher::prepare(tree.clone()).unwrap();
    let wd_src: i32 = watcher.wd_of(&temp.path().join("src")).unwrap();
    let wd_dst: i32 = watcher.wd_of(&temp.path().join("dst")).unwrap();
    let files: u32 = tree.conf().files();

    std::fs::write(temp.path().join("src/a/sub/unseen.bin"), b"y").unwrap();
    std::fs::rename(temp.path().join("src/a"), temp.path().join("dst/b")).unwrap();
    watcher.inject(wd_src, libc::IN_MOVED_FROM | libc::IN_ISDIR, 9, "a");
    watcher.inject(wd_dst, libc::IN_MOVED_TO | libc::IN_ISDIR, 9, "b");

    assert!(tree.contains(&format!("{dir}/dst/b/sub/known.bin")), "moved file missing");
    assert!(!tree.contains(&format!("{dir}/dst/b/sub/unseen.bin")), "the move rescanned");
    assert!(!tree.contains(&format!("{dir}/src/a")), "old place still in the tree");
    assert_eq!(tree.conf().files(), files, "files after the move");
    watcher.stop();
    assert_eq!(tree.conf().errors(), 0, "watcher reported errors");
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_watcher_new_subtrees() {
    /*
    New directories with subdirectories, and files written into those
    right away: each subdirectory must be watched before it is listed,
    or a file written between its listing and its watch is lost for good.
    */
    const ROUNDS: usize = 100;
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    let tree: Arc<DirTree> = Arc::new(walked_tree(dir, FileMode::NODE, false));
    let _watcher: Arc<TreeWatcher> =
        TreeWatcher::start(tree.clone()).expect("watcher should start");

    let mut files: Vec<PathBuf> = Vec::new();
    for i in 0..ROUNDS {
        let deepest: PathBuf = temp.path().join(format!("r{i}/a/b/c"));
        std::fs::create_dir_all(&deepest).unwrap();
        for sub in ["", "a", "a/b", "a/b/c"] {
            let file: PathBuf = temp.path().join(format!("r{i}")).join(sub).join("f.bin");
            std::fs::write(&file, b"x").unwrap();
            files.push(file);
        }
    }
    for file in &files {
        let p: String = file.to_string_lossy().into_owned();
        wait_for(|| tree.contains(&p), &format!("{p} should appear"));
    }
    assert_eq!(tree.conf().errors(), 0, "watcher reported errors");
    tree_validate_counts(&tree);
}

#[test]
fn test_tree_watcher() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir(temp.path().join("sub")).unwrap();
    std::fs::write(temp.path().join("sub/a.bin"), b"x").unwrap();

    let tree: Arc<DirTree> =
        Arc::new(walked_tree(dir, FileMode::NODE, false));
    let watcher: Arc<TreeWatcher> =
        TreeWatcher::start(tree.clone()).expect("watcher should start");
    assert!(watcher.watches_len() >= 2, "root + sub should be watched");
    assert_eq!(watcher.failed_watches(), 0, "no watch failures expected");

    // file creation in an existing (watched) directory
    std::fs::write(temp.path().join("sub/b.bin"), b"y").unwrap();
    let p_b: String = format!("{dir}/sub/b.bin");
    wait_for(|| tree.contains(&p_b), "created file should appear in the tree");

    /*
    Nested directory creation: the "newdir" create event triggers a
    watch + recursive scan which must pick up "nested" as well. The
    short settle time lets the watches get established before the file
    below is created (see the watch-then-scan note in watch.rs).
    */
    std::fs::create_dir_all(temp.path().join("newdir/nested")).unwrap();
    let p_nested: String = format!("{dir}/newdir/nested");
    wait_for(|| tree.contains(&p_nested), "new nested dir should appear in the tree");
    std::fs::write(temp.path().join("newdir/nested/c.bin"), b"z").unwrap();
    let p_c: String = format!("{dir}/newdir/nested/c.bin");
    wait_for(|| tree.contains(&p_c), "file in new nested dir should appear");

    // removals: single file, then a whole subtree
    std::fs::remove_file(temp.path().join("sub/b.bin")).unwrap();
    wait_for(|| !tree.contains(&p_b), "removed file should disappear");
    std::fs::remove_dir_all(temp.path().join("newdir")).unwrap();
    wait_for(|| !tree.contains(&format!("{dir}/newdir")), "removed subtree should disappear");

    watcher.stop();
    tree_validate_counts(&tree);
}

/// Poll `cond` for up to ~2 seconds before failing the test with `msg`.
#[test]
fn test_tree_watcher_special_files() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();

    let tree: Arc<DirTree> = Arc::new(walked_tree(dir, FileMode::NODE, false));
    let _watcher: Arc<TreeWatcher> =
        TreeWatcher::start(tree.clone()).expect("watcher should start");
    let files: u32 = tree.conf().files();

    // symlinks (dangling, to "..", to themselves), a FIFO and a socket: none is a regular file
    let spec: TreeSpec = [
        Special::SymlinkDir,
        Special::SymlinkDangling,
        Special::SymlinkSelf,
        Special::Fifo,
        Special::Socket,
    ]
    .into_iter()
    .fold(TreeSpec::new(), |s, kind| s.with(kind, 1));
    spec.create(temp.path()).unwrap();
    // ...a regular file created last proves the events before it were handled
    std::fs::write(temp.path().join("plain.bin"), b"p").unwrap();
    wait_for(|| tree.contains(&format!("{dir}/plain.bin")), "regular file should appear");

    for e in spec.plan(temp.path()) {
        assert!(!tree.contains(&e.path.to_string_lossy()), "{:?} recorded", e.kind);
    }
    assert_eq!(tree.conf().files(), files + 1, "only plain.bin should be counted");
    assert_eq!(tree.conf().errors(), 0, "a special entry caused an error");
    tree_validate_counts(&tree);
}

fn wait_for<F: Fn() -> bool>(cond: F, msg: &str) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("Timeout waiting for: {msg}");
}

/* --------------------------------- */

/// A tree of `dir` in `filemode`, walked recursively (parallel walker).
fn walked_tree(dir: &str, filemode: FileMode, resident: bool) -> DirTree {
    let tree: DirTree = DirTree::new(filemode, Filters::default())
        .from_path(dir)
        .with_recursive(true)
        .with_resident(resident);
    tree.walk().unwrap();
    tree
}

/// Create a test DirTree from path and perform some basic validations.
fn create_test_tree(recursive: bool) -> (&'static str, DirTree, u8) {
    setup_tests();
    let path = unsafe { TESTDIR.as_ref().unwrap().path().to_str().unwrap() };
    let tree: DirTree = DirTree::new(FileMode::NODE, Filters::default())
        .from_path(path)
        .with_recursive(recursive);
    tree.walk().unwrap();
    assert_eq!(tree.root.node_t, NodeType::Root);
    assert_eq!(*tree.from().unwrap(), PathBuf::from(path));
    assert_eq!(tree.state(), TreeState::Ready);
    assert!(tree.is_ready(), "Tree is not ready");
    let root_depth: u8 = (path.split(PATH_SEP).count() - 1) as u8;
    tree_validate_counts(&tree);
    (path, tree, root_depth)
}

/// Return the node, dir and file counts from a DirTree.
#[rustfmt::skip]
fn counts(tree: &DirTree) -> (u32, u32, u32, u8) {
    (
        tree.conf.nodes(),
        tree.conf.dirs(),
        tree.conf.files(),
        tree.conf.depth(),
    )
}

/// Check that node, dir, and file counts match the expected values.
fn check_nodes_dirs_files(nodes: u32, root_depth: u8, dirs: u32, files: u32, depth: u8) {
    assert_eq!(nodes, EXP_NODES + root_depth as u32, "nodes mismatch");
    assert_eq!(dirs, EXP_DIRS + root_depth as u32, "dirs mismatch");
    assert_eq!(files, EXP_FILES, "files mismatch");
    assert_eq!(depth, root_depth + 3, "depth mismatch");
}

/// Validate and return the counts of nodes, dirs, and files below a given node.
fn validate_counts_below_node(tree: &DirTree, node: Arc<Node>) -> (u64, u64, u64) {
    let (nodes_c, dirs_c, files_c) = tree.count_from(node.clone());
    let (nodes_i, dirs_i, files_i) = tree.iter_count_from(node.clone());

    assert_eq!(nodes_c, dirs_c + files_c, "count_from() node count != dirs+files");
    assert_eq!(nodes_i, dirs_i + files_i, "iter_count_from() node count != dirs+files");

    assert_eq!(nodes_c, nodes_i, "count_from() != iter_count_from() [nodes]");
    assert_eq!(dirs_c, dirs_i, "count_from() != iter_count_from() [dirs]");
    assert_eq!(files_c, files_i, "count_from() != iter_count_from() [files]");

    (nodes_c as u64, dirs_c as u64, files_c as u64)
}

/**
The shared test tree: one file in the root, `TEST_NUM[0]` top-level
directories with one file each, `TEST_NUM[1]` subdirectories in each,
holding `TEST_NUM[2]` files each. miniutils' default names.
*/
fn test_spec() -> TreeSpec {
    TreeSpec::new()
        .root_files(1)
        .level(TEST_NUM[0], 1)
        .level(TEST_NUM[1], TEST_NUM[2])
}

/// All expected paths of the test tree under `path`.
fn path_generator(path: &str) -> HashSet<String> {
    test_spec()
        .plan(Path::new(path))
        .map(|e: PlannedEntry| e.path.to_string_lossy().into_owned())
        .collect()
}

/**
Optimally the test directory should be created only once and then
reused for all tests. The unsafe `setup_tests()` should ensure that
this fn is called only once.
*/
fn create_test_dirs_for_tree_test() -> TempDir {
    let temp_dir: TempDir = TempDir::new().unwrap();
    let counts = test_spec().create(temp_dir.path()).unwrap();
    assert_eq!((counts.dirs, counts.files), (EXP_DIRS as u64, EXP_FILES as u64));
    temp_dir
}
