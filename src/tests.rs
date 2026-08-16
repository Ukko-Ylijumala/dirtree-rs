#![cfg(test)]
// Silence warning "creating a mutable reference to mutable static is discouraged".
// This can be done due to the way we're using the mutable reference in the tests.
#![allow(static_mut_refs)]

use super::*;
use crate::{FileMode, Filters, PATH_SEP, ScanState, testdirs::create_test_dirs};
use ctor::dtor;
use libc;
use parking_lot::Mutex;
use std::{
    collections::HashSet,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tempfile::TempDir;

const TEST_NUM: [u64; 3] = [9, 11, 7];
const EXP_DIRS: u32 = (TEST_NUM[0] + TEST_NUM[0] * TEST_NUM[1]) as u32;
const EXP_FILES: u32 = (TEST_NUM[0] * TEST_NUM[1] * TEST_NUM[2] + TEST_NUM[0] + 1) as u32;
const EXP_NODES: u32 = EXP_DIRS + EXP_FILES;

// statics for all tests
static mut STATE: Option<ScanState> = None;
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
        STATE = Some(ScanState {
            filemode: FileMode::NODE,
            ..Default::default()
        });
        TESTDIR = Some(create_test_dirs_for_tree_test());
    }
    *init = true;
}

#[dtor]
fn teardown() {
    // println! or eprintln! in `dtor` will panic as Rust has already
    // shut down certain facilities. We can use libc::printf instead.
    unsafe {
        libc::printf("*** DirTree tests done, tearing down ***\n\0".as_ptr() as *const i8);
        if let Some(_) = TESTDIR {
            libc::printf(" - Deleting temp directory...\n\0".as_ptr() as *const i8);
            let temp: TempDir = TESTDIR.take().unwrap();
            temp.close().unwrap();
        }
        libc::printf("*** Teardown finished ***\n\n\0".as_ptr() as *const i8);
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
    assert_eq!(tree.from(), &PathBuf::from("/foo"));
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
    let (path, state) =
        unsafe { (TESTDIR.as_ref().unwrap().path().to_str().unwrap(), STATE.as_ref().unwrap()) };

    let tree: Arc<DirTree> = DirTree::new(FileMode::NODE, Filters::default())
        .from_path(path)
        .build(state);
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
    assert!(tree.contains(&path), "Root not found: {}", path);
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
    let file: String = format!("{path}/level_1_{0}/file-{0}.bin", l1_idx);
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
    std::fs::write(temp.path().join("orig.bin"), b"x").unwrap();
    std::fs::hard_link(temp.path().join("orig.bin"), temp.path().join("link.bin")).unwrap();

    let state = ScanState { filemode: FileMode::NODE, ..Default::default() };
    let tree: DirTree = DirTree::new_from_path(dir, &state, true, false);
    /*
    Hardlinked files share an inode, so an equality-based child lookup
    cannot tell the siblings apart - name resolution must be by identity.
    */
    for name in ["orig.bin", "link.bin"] {
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

    let state = ScanState { filemode: FileMode::NAME, ..Default::default() };
    let tree: DirTree = DirTree::new_from_path(dir, &state, true, false);
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

    let state = ScanState { filemode: FileMode::NODE, ..Default::default() };
    let tree: DirTree = DirTree::new_from_path(dir, &state, false, false);
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
fn test_tree_watcher() {
    let temp: TempDir = TempDir::new().unwrap();
    let dir: &str = temp.path().to_str().unwrap();
    std::fs::create_dir(temp.path().join("sub")).unwrap();
    std::fs::write(temp.path().join("sub/a.bin"), b"x").unwrap();

    let state = ScanState { filemode: FileMode::NODE, ..Default::default() };
    let tree: Arc<DirTree> =
        Arc::new(DirTree::new_from_path(dir, &state, true, false));
    let watcher: Arc<TreeWatcher> =
        TreeWatcher::start(tree.clone(), state.clone()).expect("watcher should start");
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

/// Create a test DirTree from path and perform some basic validations.
fn create_test_tree(recursive: bool) -> (&'static str, DirTree, u8) {
    setup_tests();
    let (path, state) =
        unsafe { (TESTDIR.as_ref().unwrap().path().to_str().unwrap(), STATE.as_ref().unwrap()) };
    let tree: DirTree = DirTree::new_from_path(path, state, recursive, false);
    assert_eq!(tree.root.node_t, NodeType::Root);
    assert_eq!(*tree.from(), PathBuf::from(path));
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

/// Generate all expected paths for the test directory structure.
fn path_generator(path: &str) -> HashSet<String> {
    let mut paths: HashSet<String> = HashSet::new();
    paths.insert(format!("{}/test.bin", path));

    for l1_idx in 0..TEST_NUM[0] {
        paths.insert(format!("{}/level_1_{l1_idx}", path));
        for l2_idx in 0..TEST_NUM[1] {
            paths.insert(format!("{}/level_1_{l1_idx}/level_2_{l2_idx}", path));
            for l3_idx in 1..=TEST_NUM[2] {
                paths.insert(format!(
                    "{0}/level_1_{1}/level_2_{2}/file-{1}_{2}_{3}.bin",
                    path, l1_idx, l2_idx, l3_idx
                ));
            }
            paths.insert(format!("{path}/level_1_{0}/file-{0}.bin", l1_idx));
        }
    }
    paths
}

/**
Optimally the test directory should be created only once and then
reused for all tests. The unsafe `setup_tests()` should ensure that
this fn is called only once.
*/
fn create_test_dirs_for_tree_test() -> TempDir {
    let temp_dir: TempDir = TempDir::new().unwrap();
    let path: &str = temp_dir.path().to_str().unwrap();
    create_test_dirs(path, Some(TEST_NUM.to_vec()), true, false, None).unwrap();
    temp_dir
}
