// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::conf::TreeConf;
use super::event::{TreeEvent, TreeOp, TreeState};
use super::node::{Directory, Entry, FileEntry, MaybeNode, Node, NodeItem, NodeIter, NodeType};
use super::traverse::{traverse_from, traverse_from_par, walk_nodes};
use super::visitor::*;
use super::worker::tree_worker;
use crate::{PATH_SEP, ScanState, args::FileMode, filters::Filters, utils::path_parts};

use dirhandle::{CheckedOutHandle, DirFd, DirHandle, EntryExt, OpenHandles};
use stringstore::UniqueStrStore;
use timesince::SecondsSinceEpoch;

use crossbeam::{channel::Sender, queue::SegQueue};
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use tracing::{debug, error, instrument, trace, trace_span, warn};

use std::{
    collections::VecDeque,
    ffi::OsStr,
    fmt::{self, Display, Formatter},
    fs::DirEntry,
    io::{Error, ErrorKind},
    os::fd::{AsRawFd, RawFd},
    os::unix::ffi::OsStrExt,
    os::unix::fs::DirEntryExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering::Relaxed},
    },
    thread,
};

#[cfg(feature = "size_of")]
use {
    crate::utils::mod_atom_u32,
    size_of::{Context, SizeOf},
    std::mem::size_of,
    timesince::TimeSinceEpoch,
};

const MAX_RECURSE_DEPTH: usize = 16;

/**
Lightweight state that flows down the parallel walk: the active
scope, the current dir's interned name, and its parent's. Pure `Copy`
so spawning into rayon costs nothing extra.
*/
#[derive(Clone, Copy, Default)]
struct WalkState {
    scope: ScopeTag,
    name_idx: u32,
    parent_name_idx: Option<u32>,
}

/**
Trie structure for storing a directory tree.

You can use it f.ex. like this:
```
use statter::ScanState;
use statter::tree::DirTree;

let state: ScanState = ScanState::default();
state.start_updates(); // start the progress bars

let tree: DirTree = DirTree::new_from_path("/tmp", &state, false, false);
eprintln!("{tree}"); // print basic tree info (nodes, dirs, files etc)
*/
#[derive(Default, Debug)]
pub struct DirTree {
    pub(super) conf: Arc<TreeConf>,
    pub(super) root: Arc<Node>,
    pub(super) strings: UniqueStrStore,
    pub(super) worker: Mutex<Option<thread::JoinHandle<()>>>,
    handles: OpenHandles,
    state: RwLock<TreeState>,
    workq: RwLock<VecDeque<TreeOp>>,
    events: RwLock<Vec<TreeEvent>>,
}

impl DirTree {
    /// Returns an Arc reference to the tree's root [[Node]].
    #[inline]
    pub fn root(&self) -> Arc<Node> {
        self.root.clone()
    }

    /// Tree root path (in the filesystem) from which the tree is built.
    /// NOTE: internally stored paths are relative to this.
    pub fn from(&self) -> &PathBuf {
        self.conf.from()
    }

    /// Returns a reference to the tree's [[TreeConf]] struct.
    pub fn conf(&self) -> Arc<TreeConf> {
        self.conf.clone()
    }

    pub fn filemode(&self) -> &FileMode {
        &self.conf.filemode
    }

    /// The number of open directory handles.
    pub fn handles_len(&self) -> usize {
        self.handles.len()
    }

    /// The total size of open directory handles in bytes.
    #[cfg(feature = "size_of")]
    pub fn handles_size(&self) -> usize {
        self.handles.size_of().total_bytes()
    }

    /// Returns a reference to the tree's creation time.
    pub fn created(&self) -> &SecondsSinceEpoch {
        self.conf.ctime()
    }

    /// The total size of stored strings structure in bytes.
    #[cfg(feature = "size_of")]
    pub fn strings_size(&self) -> usize {
        self.strings.size_of().total_bytes()
    }

    pub fn strings(&self) -> &UniqueStrStore {
        &self.strings
    }

    pub fn get_string(&self, idx: u32) -> &str {
        self.strings.get(idx).ok().unwrap_or("")
    }

    pub fn node_name(&self, node: &Arc<Node>) -> String {
        node.name(&self.strings).unwrap_or("<unnamed>".to_owned())
    }

    /// Returns the current [[TreeState]].
    pub fn state(&self) -> TreeState {
        self.state.read().clone()
    }

    /**
    Set the tree to the given state.

    - records the end of the previous op if the tree was in an active state
    - records the beginning of the new op if it is an "active" op
    */
    #[inline]
    pub(super) fn set_state(&self, state: TreeState) {
        if state == TreeState::Quitting {
            self.quit_worker(false);
        } else if state == TreeState::Ready && self.has_work() {
            self.add_error(TreeEvent::new("Work queue not empty, cannot set state::Ready"));
            return;
        }

        match self.active_op() {
            Some(ref op) => self.add_event(TreeEvent::op_end(op)),
            _ => {}
        }
        match state {
            TreeState::Active(ref op) => self.add_event(TreeEvent::op_beg(op)),
            _ => {}
        }
        *self.state.write() = state;
    }

    /// Record a new event in the tree's event log.
    #[inline]
    pub(super) fn add_event(&self, event: TreeEvent) {
        self.events.write().push(event);
    }

    /// Add an error event to the event log and increase the error count.
    fn add_error(&self, event: TreeEvent) {
        error!("{event:?}");
        self.add_event(event);
        self.conf.errors_inc();
    }

    /// Whether the tree's workqueue is empty.
    #[inline]
    pub(super) fn no_work(&self) -> bool {
        self.workq.read().is_empty()
    }

    /// Whether there's pending work items in the tree's workqueue.
    #[inline]
    fn has_work(&self) -> bool {
        !self.no_work()
    }

    /// Get the next work item from the front of the tree's work queue, if any.
    #[inline]
    pub(super) fn get_work(&self) -> Option<TreeOp> {
        self.workq.write().pop_front()
    }

    /// Add an operation to the tree's work queue.
    #[inline]
    fn queue_op(&self, op: TreeOp) {
        self.workq.write().push_back(op);
    }

    /// Add a priority operation to the front of the tree's work queue.
    #[inline]
    fn queue_op_prio(&self, op: TreeOp) {
        self.workq.write().push_front(op);
    }

    /// Whether the background worker thread has been started.
    pub fn is_worker_running(&self) -> bool {
        self.worker.lock().is_some()
    }

    /// Tell the background worker thread to quit. Optionally wait till
    /// the worker thread exits.
    pub(super) fn quit_worker(&self, block: bool) {
        self.queue_op_prio(TreeOp::Quit);
        if block {
            if let Some(worker) = self.worker.lock().take() {
                worker.join().expect("Failed to join DirTree worker thread");
            }
        }
    }

    /**
    Tell the background worker thread to scan (populate) the given path.

    NOTE: If `recursive` is `None`, the tree's default is used.

    NOTE: non-blocking, the actual scan is done in the background. The scan
    is finished when [TreeState::Ready]. This can also be checked with the
    `is_ready()` method.
    */
    pub fn scan(&self, path: &str, recursive: Option<bool>) {
        self.queue_op(TreeOp::Scan(PathBuf::from(path), recursive));
    }

    /// Returns `true` if the tree is uninitialized.
    #[inline]
    pub fn is_uninit(&self) -> bool {
        matches!(self.state(), TreeState::Uninitialized)
    }

    /// Returns `true` if the tree is ready and has no active operations queued.
    #[inline]
    pub fn is_ready(&self) -> bool {
        matches!(self.state(), TreeState::Ready) && self.no_work() && !self.is_quitting()
    }

    /// Returns `true` if the tree is in an active state.
    #[inline]
    pub fn is_active(&self) -> bool {
        matches!(self.state(), TreeState::Active(_)) || self.has_work()
    }

    /// Returns `true` if the tree is in a quitting state.
    #[inline]
    pub fn is_quitting(&self) -> bool {
        matches!(self.state(), TreeState::Quitting)
    }

    /// Returns `true` if the tree is in an error state.
    #[inline]
    pub fn is_error(&self) -> bool {
        matches!(self.state(), TreeState::Error(_) | TreeState::Inconsistent(_))
    }

    /// Returns the error state of the tree, if any.
    pub fn error_state(&self) -> Option<TreeEvent> {
        match self.state() {
            TreeState::Error(e) | TreeState::Inconsistent(e) => Some(e),
            _ => None,
        }
    }

    /// Returns the current active operation on the tree, if any.
    pub fn active_op(&self) -> Option<TreeOp> {
        match self.state() {
            TreeState::Active(op) => Some(op),
            _ => None,
        }
    }

    /// Creates a new empty directory tree (internally a Trie structure).
    pub fn new(filemode: FileMode, filters: Filters) -> Self {
        let store: UniqueStrStore = UniqueStrStore::new_with_capacity(1024);
        let name_idx: u32 = store.insert("ROOT");
        DirTree {
            root: Node::new(NodeItem::Root(Directory::new(name_idx)), None).into(),
            conf: TreeConf::new(filemode, filters).into(),
            strings: store,
            ..Default::default()
        }
    }

    /// Set the root (filesystem) path of the tree.
    pub fn from_path(self, path: &str) -> Self {
        self.conf.set_from(path);
        self.insert(self.from(), NodeType::Directory, None);
        self.set_state(TreeState::Empty);
        self
    }

    /**
    Attach a [`Visitor`] to this tree. The visitor's hooks
    ([`Visitor::visit_dir`], [`Visitor::prune_child`],
    [`Visitor::max_depth`]) are invoked from the parallel walker
    (`populate_par`). Sync-mode walks ignore the visitor and
    [`DirTree::build`] rejects sync mode when a visitor is set.
    */
    pub fn with_visitor(self, v: Arc<dyn Visitor>) -> Self {
        self.conf.set_visitor(Some(v));
        self
    }

    /**
    Attach a [`WalkEvent`] sink. Every [`Verdict::Tag`] returned by
    the visitor produces one event on this channel. If the receiver
    is dropped, sends become no-ops (the walker continues).
    */
    pub fn with_discovery_sink(self, tx: Sender<WalkEvent>) -> Self {
        self.conf.set_discovery_tx(Some(tx));
        self
    }

    /// Whether this tree has a visitor configured.
    pub fn has_visitor(&self) -> bool {
        self.conf.has_visitor()
    }

    /// Builder: set recursive scanning.
    pub fn with_recursive(self, val: bool) -> Self {
        self.conf.set_recursive(val);
        self
    }

    /// Builder: set resident-mode FD pinning.
    pub fn with_resident(self, val: bool) -> Self {
        self.conf.set_resident(val);
        self
    }

    /**
    Walk this tree's configured root path, populating it.

    Uses [`DirTree::populate_par`] when sync-mode is disabled OR a visitor is
    configured; otherwise falls back to the synchronous [`DirTree::populate`].
    
    This is the recommended entry point for builder-style construction:

    ```ignore
    let tree = DirTree::new(filemode, filters)
        .from_path("/some/root")
        .with_recursive(true)
        .with_visitor(Arc::new(visitor));
    tree.walk(&state);
    ```
    */
    pub fn walk(&self, state: &ScanState) {
        let from: PathBuf = self.from().clone();
        /*
        Counter bookkeeping for the root dir: kept here to mirror
        `new_from_path`, since the counter is for display only and is
        not load-bearing for tree consistency.
        */
        state.num_d.inc1();
        self.set_state(TreeState::Active(TreeOp::Build(from.clone())));
        let use_par: bool = !state.sync || self.has_visitor();
        let recursive: bool = self.conf.recursive();
        if use_par {
            self.populate_par(&from, state);
        } else {
            self.populate(&from, state, Some(recursive));
        }
        self.set_state(TreeState::Ready);
    }

    /**
    Build a new [[DirTree]] with the given options and start the worker thread.

    NOTE: must be chained with `from_path()` to set the root path.
    */
    pub fn build(self, state: &ScanState) -> Arc<Self> {
        if self.conf.from.get().is_none() {
            panic!("Root path must be set before building the tree");
        }
        if state.sync && self.has_visitor() {
            /*
            Silent fallthrough would mean the visitor is configured but
            never invoked - almost certainly a caller bug. Force the
            configuration error early.
            */
            panic!("DirTree::build: a visitor was configured, but state.sync is true. \
                    The visitor protocol is parallel-walker only - set sync=false.");
        }
        self.conf.set_sync(state.sync);
        let tree: Arc<Self> = self.into();
        let tree_c: Arc<Self> = tree.clone();
        let state: ScanState = state.clone();
        let worker: thread::JoinHandle<()> = thread::Builder::new()
            .stack_size(256 * 1024) // 256 KiB
            .name("tree_worker".into())
            .spawn(|| tree_worker(tree_c, state))
            .expect("Failed to start DirTree worker thread");
        *tree.worker.lock() = Some(worker);
        tree
    }

    /// Creates a new [[DirTree]] with the given path as root.
    ///
    /// If `recursive` is true, also populates the tree by recursively walking
    /// the full directory structure (starting from from the given directory)
    /// and inserting each found path into the tree.
    #[instrument(name = "DirTree", skip_all)]
    pub fn new_from_path(path: &str, state: &ScanState, recursive: bool, resident: bool) -> Self {
        debug!(target: "path", "{path}");
        let tree: DirTree = Self::new(state.filemode, state.filters.clone()).from_path(path);
        tree.conf.set_recursive(recursive);
        tree.conf.set_resident(resident);
        tree.conf.set_sync(state.sync);
        /*
        Technically we've not yet scanned the root directory, but this place
        is the most logical one to do the increment to keep the counter in
        sync as adding more logic to `populate*()` methods would be counter-
        productive. Besides, this counter is only for display.
        */
        state.num_d.inc1();
        debug!(target: "TREE", "{tree:?}");
        if recursive {
            tree.set_state(TreeState::Active(TreeOp::Build(PathBuf::from(path))));
            match state.sync {
                true => tree.populate(tree.from(), state, Some(recursive)),
                false => tree.populate_par(tree.from(), state),
            }
        };
        tree.set_state(TreeState::Ready);
        tree
    }

    /// Populate a leaf [[Node]] in the trie with the contents of a directory.
    /// Uses the standard [std::fs::read_dir] method to get the directory entries.
    ///
    /// NOTE: single threaded, potentially slow with large directory trees.
    #[instrument(level = "debug", skip_all, fields(p = path.strip_prefix(self.from()).ok().unwrap().to_str()))]
    pub fn populate(&self, path: &PathBuf, state: &ScanState, recursive: Option<bool>) {
        trace!(target: "get_entries", "{}", path.display());
        match path.read_dir().ok() {
            Some(entries) => {
                entries.filter_map(Result::ok).for_each(|entry: DirEntry| {
                    let name = entry.file_name();
                    let path: PathBuf = entry.path();
                    match entry.file_type() {
                        Ok(entry_t) => {
                            trace!(target: "DirEntry", "{}", path.display());
                            if entry_t.is_dir() {
                                if !self.conf.filters().passes(&name, true) {
                                    return;
                                }
                                self.insert(&path, NodeType::Directory, Some(entry.ino()));
                                state.num_d.inc1();
                                if recursive.is_some_and(|r: bool| r) || self.conf.recursive() {
                                    if self.is_worker_running() {
                                        self.queue_op(TreeOp::Scan(path, recursive));
                                    } else {
                                        self.populate(&path, state, recursive);
                                    }
                                }
                            } else if entry_t.is_file() {
                                if !self.conf.filters().passes(&name, false) {
                                    return;
                                }
                                if self.filemode().is_with_size() {
                                    //FIXME: add error handling
                                    state.fsize.fetch_add(entry.metadata().ok().unwrap().len());
                                }
                                if self.filemode().is_name() {
                                    self.insert(&path, NodeType::Name, None);
                                } else if self.filemode().is_node() {
                                    self.insert(&path, NodeType::File, Some(entry.ino()));
                                }
                                state.num_f.inc1();
                            }
                        }
                        Err(e) => {
                            self.add_error(TreeEvent::error(
                                &e.to_string(),
                                &TreeOp::Scan(path.clone(), recursive),
                            ));
                            debug!("Error with {}: {e}", path.display());
                        }
                    }
                })
            }
            None => return,
        };
    }

    /**
    Parallel version of [DirTree::populate] using [rayon::iter]
    to process each directory entry in parallel.

    Uses [[DirHandle]] to read the directory entries, and its [DirHandle::iter]
    method which tries to return inner directories first using a small
    buffer to look ahead in the directory stream.
    */
    pub fn populate_par(&self, path: &PathBuf, state: &ScanState) {
        let walk = self.initial_walk_state(path);
        rayon::scope(|s| self.populate_par_inner(path, state, walk, s, 0));
    }

    /**
    Build the [`WalkState`] for the very first call into the walker.
    The interned name is the basename of `path`; if `path` has no
    basename (e.g. `/`) we fall back to the empty-string index `0`.
    */
    fn initial_walk_state(&self, path: &PathBuf) -> WalkState {
        let name_idx = path
            .file_name()
            .map(|n| self.strings.insert(n.to_string_lossy().as_ref()))
            .unwrap_or(0);
        WalkState { scope: SCOPE_NONE, name_idx, parent_name_idx: None }
    }

    /// Emit a [`WalkEvent`] on the discovery channel, if one is configured.
    /// No-op if no consumer is attached or the receiver has been dropped.
    fn emit_walk_event(
        &self,
        path: &PathBuf,
        tag: ScopeTag,
        scope: ScopeTag,
        depth: usize,
        claimed: bool,
    ) {
        if let Some(tx) = self.conf.discovery_tx() {
            let _ = tx.send(WalkEvent {
                path: path.clone(),
                tag,
                scope,
                depth,
                claimed,
            });
        }
    }

    #[
        instrument(level = "debug", name = "p_par_inner", skip_all,
        fields(p = path.strip_prefix(self.from()).ok().unwrap().to_str(), d = depth))
    ]
    fn populate_par_inner<'env>(
        &'env self,
        path: &PathBuf,
        state: &'env ScanState,
        walk: WalkState,
        rs: &rayon::Scope<'env>,
        depth: usize,
    ) {
        // Cooperative cancellation - checked at the top of every directory
        // entry so an in-flight walk can wind down promptly once raised.
        if self.conf.is_cancelled() {
            return;
        }

        let visitor: Option<Arc<dyn Visitor>> = self.conf.visitor();

        // Per-scope visitor depth cap (separate from MAX_RECURSE_DEPTH,
        // which is about stack/spawn thresholds, not tree depth).
        if let Some(ref v) = visitor {
            let cap = v.max_depth(walk.scope);
            if cap > 0 && depth >= cap {
                return;
            }
        }

        let op: TreeOp = TreeOp::Scan(path.into(), Some(self.conf.recursive()));
        let mut handle = match DirHandle::new(path) {
            Ok(h) => h,
            Err(e) => {
                self.add_error(TreeEvent::error(&e.to_string(), &op));
                debug!(target: "ERROR", "Cannot read directory: {}", e);
                return;
            }
        };
        trace!(target: "iter_dir", "{:?} ::: {handle:?}", path.display());

        // Visitor branch: collect dirents so visit_dir sees the full list, then act on the verdict.
        let mut scope_for_children: ScopeTag = walk.scope;
        let mut skip_children: bool = false;
        let collected: Option<Vec<EntryExt>> = if let Some(ref v) = visitor {
            let entries: Vec<EntryExt> = handle.iter().collect();
            let walk_ctx = WalkContext {
                path: path.as_path(),
                name_idx: walk.name_idx,
                parent_name_idx: walk.parent_name_idx,
                depth,
                scope: walk.scope,
                strings: &self.strings,
            };
            let verdict = v.visit_dir(DirContext {
                walk: &walk_ctx,
                entries: &entries,
            });
            match verdict {
                Verdict::Continue => {}
                Verdict::SkipChildren => {
                    skip_children = true;
                }
                Verdict::Tag { tag, new_scope, descend } => {
                    self.emit_walk_event(path, tag, walk.scope, depth, !descend);
                    scope_for_children = new_scope;
                    if !descend {
                        skip_children = true;
                    }
                }
            }
            Some(entries)
        } else {
            None
        };

        if !skip_children {
            /*
            The closure is identical for both iteration paths; we factor
            it out so we only write the entry-handling logic once. It
            captures `&self`, `state`, `walk`, `scope_for_children`,
            `depth`, `rs`, and `&visitor` by reference - all valid for
            the duration of the iteration.
            */
            let visitor_ref = visitor.as_ref();
            let process = |entry: EntryExt| {
                self.process_par_entry(
                    path,
                    state,
                    &walk,
                    scope_for_children,
                    depth,
                    rs,
                    visitor_ref,
                    &op,
                    entry,
                );
            };
            match collected {
                Some(v) => v.into_par_iter().for_each(process),
                None => handle.iter().par_bridge().for_each(process),
            }
        }

        #[cfg(debug_assertions)]
        {
            let cur = handle.state_current();
            let old = handle.state();
            debug!(target: "HANDLE_STATE", "equal: {}", old == &cur);
            debug!(target: "HANDLE_STATE", "old: {old:?}");
            debug!(target: "HANDLE_STATE", "cur: {cur:?}");
        } // END DEBUG -- TODO: remove

        // shall we keep the directory handle (file descriptor) open?
        if self.conf.resident() {
            self.add_fd(path, handle.as_raw_fd());
            self.handles.insert(handle);
        } else {
            drop(handle); // unnecessary, but explicit
        }
    }

    /**
    Per-dirent processing for the parallel walker. Shared between the
    fast path (`par_bridge` over `DirHandle::iter`) and the visitor
    path (`into_par_iter` over a collected `Vec<EntryExt>`).
    */
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn process_par_entry<'env>(
        &'env self,
        parent_path: &PathBuf,
        state: &'env ScanState,
        parent_walk: &WalkState,
        scope_for_children: ScopeTag,
        depth: usize,
        rs: &rayon::Scope<'env>,
        visitor: Option<&Arc<dyn Visitor>>,
        op: &TreeOp,
        entry: EntryExt,
    ) {
        match entry.file_type() {
            Some(_) => {
                /*
                file_name() via Deref<nix::dir::Entry> returns &CStr, a
                zero-copy borrow from the dirent. from_bytes() wraps it
                as &OsStr without any allocation, so filtered entries
                never pay for the String allocation from entry.name().
                */
                let name_os = OsStr::from_bytes(entry.file_name().to_bytes());
                trace!(target: "ENTRY", "{:?} : {:?}", name_os, entry);

                if entry.is_dir() {
                    /*
                    Path-aware prune via the visitor (when set). We
                    intern the child name once here so the visitor
                    can compare u32-vs-u32 without allocations.
                    */
                    let child_idx: u32 = if visitor.is_some() {
                        self.strings.insert(name_os.to_string_lossy().as_ref())
                    } else {
                        0
                    };
                    if let Some(v) = visitor {
                        let parent_ctx = WalkContext {
                            path: parent_path.as_path(),
                            name_idx: parent_walk.name_idx,
                            parent_name_idx: parent_walk.parent_name_idx,
                            depth,
                            scope: parent_walk.scope,
                            strings: &self.strings,
                        };
                        if v.prune_child(&parent_ctx, child_idx, true) {
                            return;
                        }
                    }
                    if !self.conf.filters().passes(name_os, true) {
                        return;
                    }
                    let entry_p: PathBuf = parent_path.join(entry.name());
                    self.insert(&entry_p, NodeType::Directory, Some(entry.ino()));
                    state.num_d.inc1();
                    if self.conf.recursive() {
                        let next = WalkState {
                            scope: scope_for_children,
                            name_idx: child_idx,
                            parent_name_idx: Some(parent_walk.name_idx),
                        };
                        // spawning is slower than direct recursion, so only
                        // spawn after MAX_RECURSE_DEPTH to bound stack use.
                        if depth < MAX_RECURSE_DEPTH {
                            self.populate_par_inner(&entry_p, state, next, rs, depth + 1);
                        } else {
                            rs.spawn(move |s| {
                                self.populate_par_inner(&entry_p, state, next, s, 0)
                            });
                        }
                    }
                } else if entry.is_file() {
                    /*
                    Same prune+filter shape for files. We only intern
                    the child name when a visitor is set; insertion via
                    `insert()` does its own interning later if needed.
                    */
                    if let Some(v) = visitor {
                        let child_idx = self.strings.insert(name_os.to_string_lossy().as_ref());
                        let parent_ctx = WalkContext {
                            path: parent_path.as_path(),
                            name_idx: parent_walk.name_idx,
                            parent_name_idx: parent_walk.parent_name_idx,
                            depth,
                            scope: parent_walk.scope,
                            strings: &self.strings,
                        };
                        if v.prune_child(&parent_ctx, child_idx, false) {
                            return;
                        }
                    }
                    if !self.conf.filters().passes(name_os, false) {
                        return;
                    }
                    let entry_p: PathBuf = parent_path.join(entry.name());
                    if self.filemode().is_with_size() {
                        state.fsize.fetch_add(entry.len());
                    }
                    if self.filemode().is_name() {
                        self.insert(&entry_p, NodeType::Name, None);
                    } else if self.filemode().is_node() {
                        self.insert(&entry_p, NodeType::File, Some(entry.ino()));
                    }
                    state.num_f.inc1();
                }
            }
            None => {
                let entry_p: PathBuf = parent_path.join(entry.name());
                self.add_event(
                    TreeEvent::new("Unknown entry type")
                        .path(&entry_p.to_string_lossy().clone())
                        .op(op),
                );
                debug!(target: "WARN", "Unknown entry type: {}", entry_p.display());
            }
        }
    }

    /// Add a [[RawFd]] to a directory node's [[Directory]] item.
    fn add_fd(&self, path: &PathBuf, fd: RawFd) {
        self.get_node(path.to_string_lossy().as_ref()).map(|node| {
            node.as_dir().map(|dir| {
                dir.fd_set(fd).ok();
            })
        });
    }

    /* --------------------------------- */

    /// Inserts a path into the trie. The path must be an absolute filesystem path.
    /// The path is split on forward slash (`/`) to obtain its parts.
    #[instrument(level = "debug", skip(self))]
    pub fn insert(&self, path: &PathBuf, node_t: NodeType, inode: Option<u64>) {
        let mut current: Arc<Node> = self.root();
        let parts = &self.strings.store_path(path)[1..];
        let len: usize = parts.len();
        let mut depth: usize = 0; // root node is at depth 0
        self.conf.depth_compare(len as u8);
        // max depth can just as well be updated at this point

        for part in parts.iter().map(|i: &u32| *i) {
            depth += 1;
            if !current.has_child(&part) {
                trace!(target: "CURRENT_NODE", "{:?}", self.node_name(&current));
                /*
                we don't have an item for this node yet, hence NodeItem::None
                also node_t must be set here since later the Node will be in
                an Arc and we can't change that field anymore
                */
                let mut new: Node = Node::new(NodeItem::None, Some(current.clone()));
                if depth < len {
                    trace!(target: "intermediate", "{part:?} --> depth: {depth}/{len}");
                    // must be a container (directory) so let's create the basic structure
                    new.node_t = NodeType::Directory;
                    let mut itm = NodeItem::Dir(Entry::<Directory>::default());
                    itm.set_dir_name(part);
                    new.item.set(itm).ok();
                    /*
                    we must increment the node counters here since we've
                    not reached the leaf node yet and we shouldn't do a
                    full initialization for an intermediate node
                    */
                    self.conf.nodes_mod(1);
                    self.conf.dirs_mod(1);
                } else {
                    new.node_t = node_t.clone();
                    if node_t == NodeType::Directory {
                        let mut itm = NodeItem::Dir(Entry::<Directory>::new(path, inode).unwrap());
                        itm.set_dir_name(part);
                        new.item.set(itm).ok();
                        self.conf.nodes_mod(1);
                        self.conf.dirs_mod(1);
                    } else if node_t == NodeType::Name {
                        /*
                        optimization: don't create file Nodes at all, just
                        record the fact that a file exists in the directory
                        NOTE: total node count is not incremented in this case
                        */
                        drop(new);
                        current
                            .as_dir()
                            .map(|dir: &Directory| dir.add_child(part, None));
                        self.conf.files_mod(1);
                        debug!(target: "FILENAME_ADD", "{part:?} (store name only)");
                        return;
                    }
                }
                debug!(target: "CREATED_NODE", "{part:?} : {:?}", &new);
                current.add_child(part, new.into());
            }

            // we've reached the bottom of the path -> grab the leaf node
            current = match current.get_child(&part) {
                Some(node) => node,
                None => {
                    if current.has_child(&part) && node_t == NodeType::Name {
                        // this is a file name entry, so None is expected
                        return;
                    } else {
                        // should never happen
                        self.add_error(TreeEvent::error(
                            &format!("Child {part:?} missing from HashMap: {current:?}"),
                            &TreeOp::Insert,
                        ));
                        return;
                    }
                }
            }
        }

        if !matches!(current.item.get(), None) {
            // for now we don't overwrite existing nodes, but
            // this may change in the future to allow for updates
            trace!(target: "SKIP_EX_NODE", "{:?} ({:?}) : {:?}",
                self.node_name(&current), current.node_t, current.path(&self.strings)
            );
            return;
        }

        match node_t {
            NodeType::Directory => {
                let mut itm = NodeItem::Dir(Entry::<Directory>::new(path, inode).unwrap());
                itm.set_dir_name(parts[len - 1]);
                current.item.set(itm).ok();
                self.conf.dirs_mod(1);
            }
            NodeType::File => {
                current
                    .item
                    .set(NodeItem::File(Entry::<FileEntry>::new(path, inode).unwrap()))
                    .ok();
                self.conf.files_mod(1);
            }
            _ => return,
        };
        self.conf.nodes_mod(1);
        debug!(target: "INSERT_CHILD", "{current:?}");
    }

    /// Remove a [[Node]] (or a leaf) from the trie. Expects an absolute path.
    ///
    /// Returns a tuple of `(nodes, dirs, files)` removed on success and [[None]]
    /// if the path was not found. The root node cannot be removed.
    ///
    /// WARNING: implementation is WIP and may yet contain bugs.
    #[instrument(level = "debug", skip(self))]
    pub fn remove(&self, path: &str) -> Result<Option<(u32, u32, u32)>, Error> {
        let op: TreeOp = TreeOp::Remove(path.into());
        match self.get_node(path) {
            Some(node) => {
                if node.node_t == NodeType::Root {
                    let msg: &str = "Cannot remove root node";
                    self.add_error(TreeEvent::error(msg, &op));
                    return Err(Error::new(ErrorKind::InvalidInput, msg));
                };

                let p: PathBuf = node.path(&self.strings);
                match node.parent() {
                    Some(parent) => {
                        let (nodes, dirs, files) = self.count_from(node.clone());
                        let (name, c) = parent.get_child_byref(&node).unwrap();
                        debug!(target: "REMOVE_NODE", "{:?}", p.display());
                        assert_eq!(node, c, "Node should be the same as the one in parent");

                        /*
                        We're potentially removing a branch instead of a leaf,
                        but since the tree consists of nested Arc<Node> refs,
                        as soon as we drop a node, its descendant nodes should
                        also be dropped in a cascading manner since they are no
                        longer referenced anywhere else. Ahh, the beauty of
                        automatic reference counting.
                        */
                        parent.remove_child(&name);
                        self.conf.nodes_mod(-(nodes as i32));
                        self.conf.dirs_mod(-(dirs as i32));
                        self.conf.files_mod(-(files as i32));
                        return Ok(Some((nodes, dirs, files)));
                    }

                    None => {
                        let msg: String = format!("Stale parent reference: {:?}", p);
                        self.add_error(TreeEvent::error(&msg, &op).node(&node));
                        return Err(Error::new(ErrorKind::NotFound, msg));
                    }
                }
            }

            None => {
                warn!("Node not found: {path:?}");
                return Ok(None);
            }
        }
    }

    /* --------------------------------- */

    /// Get a [[Node]] from the trie. Expects an absolute path.
    pub fn get_node(&self, path: &str) -> MaybeNode {
        // short circuit if the path is not absolute or does not look like a path
        if !path.starts_with(PATH_SEP) || !path.contains(PATH_SEP) {
            return None;
        }
        let mut current: Arc<Node> = self.root();
        for part in path_parts(path) {
            match current.get_child(&self.strings.insert(part)) {
                Some(node) => current = node,
                None => return None,
            }
        }
        Some(current) // found the node
    }

    /// Checks if a given path exists in the trie. Expects an absolute path.
    pub fn contains(&self, path: &str) -> bool {
        self.get_node(path).is_some()
    }

    /// The filesystem path of a [[Node]], if it contains a file or directory.
    pub fn fs_path(&self, node: &Arc<Node>) -> Option<PathBuf> {
        match node.node_t.has_data() {
            true => Some(node.path(&self.strings)),
            false => None,
        }
    }

    /* --------------------------------- */

    /// If we have a directory handle for a path, return its file descriptor.
    pub fn dirfd(&self, path: &str) -> Option<DirFd> {
        self.get_node(path).and_then(|node: Arc<Node>| {
            node.as_dir()
                .and_then(|dir: &Directory| Some(dir.fd().clone()))
        })
    }

    /**
    If we have a directory handle for a path, return its [CheckedOutHandle].
    If we don't have an open handle, but we have a [Node] for such directory,
    we try opening a handle and returning it.
    */
    pub fn handle(&self, path: &str) -> Option<CheckedOutHandle<'_>> {
        let node: Arc<Node> = self.get_node(path)?;
        node.as_dir().and_then(|dir: &Directory| {
            let fd: RawFd = dir.fd().fd();
            if fd > 0 {
                self.handles.get(fd)
            } else {
                if let Ok(handle) = self.handles.open(&node.path(&self.strings)) {
                    dir.fd_set(handle.as_raw_fd()).ok();
                    Some(handle)
                } else {
                    None
                }
            }
        })
    }

    /// Remove a handle for a directory path and clear the [DirFd] in the [Directory].
    pub fn handle_close(&self, path: &str) {
        if let Some(node) = self.get_node(path) {
            if node.is_dir() {
                let fd: RawFd = node.dirfd().unwrap().fd();
                if fd > 0 {
                    self.handles.close(fd);
                    node.dirfd().unwrap().clear();
                }
            }
        }
    }

    /* --------------------------------- */

    /**
    Walks the full tree and returns an Iterator of all nodes. WARNING: this
    can be memory intensive for large trees. Prefer using [DirTree::iter],
    which should be more efficient due to not building a full list beforehand.
    */
    pub fn nodes<'a>(&'a self) -> Box<NodeIter<'a>> {
        let q: SegQueue<Arc<Node>> = SegQueue::new();
        trace_span!("walk:nodes").in_scope(|| walk_nodes(&self.root(), &q, true, true));
        Box::new(q.into_iter())
    }

    /// Returns an Iterator of all [[Directory]] nodes in the tree.
    pub fn dirs<'a>(&'a self) -> Box<NodeIter<'a>> {
        let q: SegQueue<Arc<Node>> = SegQueue::new();
        trace_span!("walk:dirs").in_scope(|| walk_nodes(&self.root(), &q, true, false));
        Box::new(q.into_iter())
    }

    /// Returns an iterator of all [[FileEntry]] nodes in the tree.
    pub fn files<'a>(&'a self) -> Box<NodeIter<'a>> {
        let q: SegQueue<Arc<Node>> = SegQueue::new();
        trace_span!("walk:files").in_scope(|| walk_nodes(&self.root(), &q, false, true));
        Box::new(q.into_iter())
    }

    /* --------------------------------- */

    /// Creates an iterator to iterate through the tree starting from a [[Node]].
    /// The iterator is depth-first and includes the starting node.
    #[instrument(level = "trace", skip(self))]
    pub fn iter_from(&self, node: Arc<Node>) -> DirTreeIterator<'_> {
        DirTreeIterator(VecDeque::from(vec![node]), &self.strings)
    }

    /// Creates an iterator to walk through all [[Node]]s in the tree.
    pub fn iter(&self) -> DirTreeIterator<'_> {
        self.iter_from(self.root())
    }

    /// An iterator over all [[Directory]] items in the tree.
    pub fn iter_dirs<'a>(&'a self) -> impl Iterator<Item = Directory> + 'a {
        self.iter()
            .filter_map(|node: Arc<Node>| node.as_dir().cloned())
    }

    /// An iterator over all [[FileEntry]] items in the tree.
    pub fn iter_files<'a>(&'a self) -> impl Iterator<Item = FileEntry> + 'a {
        self.iter()
            .filter_map(|node: Arc<Node>| node.as_file().cloned())
    }

    /// An iterator over all Paths in the tree.
    pub fn iter_paths(&self) -> impl Iterator<Item = String> + '_ {
        self.iter()
            .filter_map(|node: Arc<Node>| self.fs_path(&node))
            .map(|p: PathBuf| p.to_string_lossy().to_string())
    }

    /// Count the number of directory and file nodes by iterating from a [[Node]].
    /// Also counts the starting node. Returns a tuple of `(nodes, dirs, files)`.
    pub fn iter_count_from(&self, node: Arc<Node>) -> (u32, u32, u32) {
        if node.node_t.is_file() {
            return (1, 0, 1);
        }

        let mut nodes: u32 = 0;
        let mut dirs: u32 = 0;
        let mut files: u32 = 0;
        /*
        NOTE: trying to convert this iterating closure to a parallel
        one with Rayon's `par_bridge()` makes the counting almost 5x slower.
        This is much more than the slowdown observed with `count_from()`,
        and I have no good explanation for it at this point.
        */
        self.iter_from(node).for_each(|node: Arc<Node>| {
            nodes += 1;
            if node.node_t.is_dir() {
                dirs += 1;
            } else if node.node_t.is_file() {
                files += 1;
            }
        });
        (nodes, dirs, files)
    }

    /// Count the number of directory and file nodes by iterating the whole tree.
    /// Does not count the root [[Node]]. Returns a tuple of `(nodes, dirs, files)`.
    pub fn iter_count(&self) -> (u32, u32, u32) {
        let (mut nodes, dirs, files) = self.iter_count_from(self.root());
        nodes -= 1; // remove root node since we started from it
        (nodes, dirs, files)
    }

    /* --------------------------------- */

    /// Traverses the tree from root and applies function `f` to each [[Node]].
    pub fn traverse<F>(&self, mut f: F)
    where
        F: FnMut(&Arc<Node>),
    {
        traverse_from(&self.root(), &mut f);
    }

    /// Traverses the tree from root in parallel and applies function `f`
    /// to each [[Node]].
    pub fn traverse_par<F>(&self, f: F)
    where
        F: Fn(&Arc<Node>) + Send + Sync,
    {
        traverse_from_par(&self.root(), &f);
    }

    /// Count the number of directory and file nodes with `traverse()`. Also counts
    /// the starting [[Node]] (except root). Returns a tuple of `(nodes, dirs, files)`.
    pub fn count_from(&self, node: Arc<Node>) -> (u32, u32, u32) {
        if node.node_t.is_file() {
            return (1, 0, 1);
        }

        let mut nodes: u32 = 0;
        let mut files: u32 = 0;
        let mut dirs: u32 = 0;
        /*
        NOTE: trying to convert this iterating closure to a parallel
        one with `traverse_from_par()` makes the counting almost 50% slower.
        Likely the overhead from moving stuff between threads and having
        to use Atomic versions of counters is the main reason.
        */
        traverse_from(&node, &mut |n: &Arc<Node>| {
            nodes += 1;
            match n.node_t {
                NodeType::Directory => dirs += 1,
                NodeType::File => files += 1,
                _ => (),
            }
        });

        if node.node_t == NodeType::Root {
            nodes -= 1; // remove root node if we started from it
        };
        (nodes, dirs, files)
    }

    /// Calculates and returns the memory usage of directory and file nodes.
    #[cfg(feature = "size_of")]
    pub fn nodes_memuse(&self) -> (u64, u64) {
        let dn_sz: AtomicU32 = AtomicU32::new(0);
        let fn_sz: AtomicU32 = AtomicU32::new(0);

        traverse_from_par(&self.root, &|n: &Arc<Node>| match n.node_t {
            NodeType::Directory => mod_atom_u32(&dn_sz, n.size_immediate().total_bytes() as i32),
            NodeType::File => mod_atom_u32(&fn_sz, n.size_of().total_bytes() as i32),
            _ => (),
        });

        (dn_sz.load(Relaxed) as u64, fn_sz.load(Relaxed) as u64)
    }
}

impl Display for DirTree {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "DirTree: nodes {}, dirs {}, files {}, depth {}, handles {}, ctime {} UTC",
            self.conf.nodes(),
            self.conf.dirs(),
            self.conf.files(),
            self.conf.depth(),
            self.handles.len(),
            self.created()
        )
    }
}

/* ######################################################################### */

/// Iterator for walking through a [[DirTree]].
pub struct DirTreeIterator<'a>(VecDeque<Arc<Node>>, &'a UniqueStrStore);

impl<'a> Iterator for DirTreeIterator<'a> {
    type Item = Arc<Node>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop_front().map(|node: Arc<Node>| {
            trace!(target: "DirTreeIterator", "{}", node.path(&self.1).display());
            if node.children().is_none() {
                return node;
            }

            // Push all found children to the stack
            node.children()
                .unwrap()
                .read()
                .values()
                .for_each(|c: &MaybeNode| {
                    c.as_ref().map(|child: &Arc<Node>| {
                        if child.is_traversable() {
                            // push directories to the front of the queue...
                            self.0.push_front(child.clone());
                        } else {
                            // ...and files to the back
                            self.0.push_back(child.clone());
                        }
                    });
                });
            node
        })
    }
}

/* ######################################################################### */

#[cfg(feature = "size_of")]
impl SizeOf for DirTree {
    fn size_of_children(&self, context: &mut Context) {
        context.add(size_of::<TreeConf>()).add_distinct_allocation();
        self.handles.size_of_children(context);
        self.strings.size_of_children(context);
        self.workq.read().size_of_children(context);
        context
            .add(size_of::<TreeState>())
            .add_distinct_allocation();
        context
            .add(size_of::<Option<thread::JoinHandle<()>>>())
            .add_distinct_allocation();
        {
            let e = self.events.read();
            e.size_of_children(context);
            context.add(size_of::<TimeSinceEpoch>() * e.len());
        }

        self.root.size_of_children(context);
    }
}
