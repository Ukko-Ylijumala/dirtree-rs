// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::conf::{NodeCounts, TreeConf};
use super::error::{TreeError, TreeResult};
use super::event::{TreeEvent, TreeOp, TreeState};
use super::node::{
    Directory, Entry, FileEntry, FileKind, MaybeNode, Node, NodeItem, NodeIter, NodeType,
};
use super::observer::TreeObserver;
use super::traverse::{traverse_from, traverse_from_par, walk_nodes};
use super::visitor::*;
use super::worker::tree_worker;
use super::{FileMode, Filters};
use super::utils::{PATH_SEP, panic_message, path_parts};

use dirhandle::{
    CheckedOutHandle, DirFd, DirHandle, EntryExt, OpenHandles,
    nix::{
        fcntl::{OFlag, open, readlinkat},
        sys::stat::Mode,
    },
};
use stringstore::{ARENA_CHUNK_SIZE, UniqueStrStore};
use timesince::SecondsSinceEpoch;

use crossbeam::{channel::Sender, queue::SegQueue};
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use tracing::{debug, error, instrument, trace, trace_span, warn};

use std::{
    borrow::Cow,
    collections::VecDeque,
    ffi::{CStr, OsStr},
    fmt::{self, Display, Formatter},
    fs::{DirEntry, read_link},
    io::{Error, ErrorKind},
    os::fd::{AsRawFd, BorrowedFd, RawFd},
    os::unix::ffi::OsStrExt,
    os::unix::fs::DirEntryExt,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    thread,
};

#[cfg(feature = "size_of")]
use {
    super::utils::mod_atom_u32,
    size_of::{Context, SizeOf},
    std::mem::size_of,
    std::sync::atomic::{AtomicU32, Ordering::Relaxed},
    timesince::TimeSinceEpoch,
};

pub(super) const MAX_RECURSE_DEPTH: usize = 16;
/// Upper bound for the in-memory event log; oldest events are dropped
/// first. Keeps long-lived (resident) trees from growing without bound.
const EVENT_LOG_CAP: usize = 4096;
/**
Minimum number of non-directory dirents one rayon task takes from a
directory's entry list. Per-file work is tiny (an intern + one map
insert), so batching is what keeps scheduling overhead from dominating
the walk. Subdirectories are never batched: each one is a whole subtree
of work.
*/
pub(super) const ENTRY_BATCH_MIN: usize = 64;

/// What [DirTree::insert_child] creates: a directory, or a file of some [FileKind].
pub(super) enum NewChild {
    Dir,
    Leaf(FileEntry),
}

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
    /// Whether this walk descends into subdirectories. Resolved once at
    /// walk start from the per-op flag (or the tree default) and carried
    /// down so a per-op override actually takes effect in the walker.
    recursive: bool,
    /// Absolute path depth of the walk root (path components below `/`),
    /// used to keep the tree's depth counter in absolute-path terms while
    /// the walker itself tracks depth relative to the walk root.
    base_depth: u8,
}

/**
Trie structure for storing a directory tree.

You can use it f.ex. like this:
```
use statter::{FileMode, Filters};
use statter::tree::DirTree;

let tree: DirTree = DirTree::new(FileMode::NODE, Filters::default())
    .from_path("/tmp")
    .with_recursive(false);
tree.walk().unwrap();
eprintln!("{tree}"); // print basic tree info (nodes, dirs, files etc)
```
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
    events: RwLock<VecDeque<TreeEvent>>,
}

impl DirTree {
    /// Returns an Arc reference to the tree's root [[Node]].
    #[inline]
    pub fn root(&self) -> Arc<Node> {
        self.root.clone()
    }

    /**
    Tree root path (in the filesystem) from which the tree is built.
    Fails with [TreeError::NoRoot] until set with `from_path()`.

    NOTE: internally stored paths are relative to this.
    */
    pub fn from(&self) -> TreeResult<&PathBuf> {
        self.conf.from().ok_or(TreeError::NoRoot)
    }

    /// `path` relative to the tree root, for log and trace output; as is without a root.
    fn rel_path<'p>(&self, path: &'p Path) -> &'p Path {
        self.conf
            .from()
            .and_then(|from: &PathBuf| path.strip_prefix(from).ok())
            .unwrap_or(path)
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
            // non-blocking, so it cannot fail
            let _ = self.quit_worker(false);
        } else if state == TreeState::Ready && self.has_work() {
            self.add_error(TreeEvent::new("Work queue not empty, cannot set state::Ready"));
            return;
        }

        if let Some(ref op) = self.active_op() {
            self.add_event(TreeEvent::op_end(op));
        }
        if let TreeState::Active(ref op) = state {
            self.add_event(TreeEvent::op_beg(op));
        }
        *self.state.write() = state;
    }

    /// Record a new event in the tree's event log. The log is bounded to
    /// [EVENT_LOG_CAP] entries; the oldest event is dropped when full.
    #[inline]
    pub(super) fn add_event(&self, event: TreeEvent) {
        let mut events = self.events.write();
        if events.len() >= EVENT_LOG_CAP {
            events.pop_front();
        }
        events.push_back(event);
    }

    /// Add an error event to the event log and increase the error count.
    pub(super) fn add_error(&self, event: TreeEvent) {
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

    /**
    Stop the background worker thread started by [DirTree::build] and
    wait for it to exit. The operation in progress, if any, finishes;
    operations still queued are dropped. The tree then returns to the
    state it was in before the stop ([TreeState::Ready] if the stop
    caught an operation in progress). A no-op without a worker.

    Dropping the tree stops the worker too; this is for stopping it
    early, and for seeing a panic. The tree stays usable afterwards
    through the blocking API ([DirTree::walk], [DirTree::update] etc.),
    but [DirTree::scan] and [DirTree::rescan] need a running worker.

    A worker that panicked fails with [TreeError::WorkerPanicked]; the
    panic is also logged as a tree error, and if it struck during an
    operation the tree is left [TreeState::Inconsistent].
    */
    pub fn stop_worker(&self) -> TreeResult<()> {
        if !self.is_worker_running() {
            return Ok(());
        }
        let before: TreeState = self.state();
        let res: TreeResult<()> = self.quit_worker(true);
        /*
        Nothing runs the queue any more. It still holds the extra Quit
        that set_state(Quitting) queues behind the one the worker took
        (or ours, if the worker died first), and that would block the
        Ready state below.
        */
        self.workq.write().clear();
        match res {
            // the worker left Quitting; an op it was running has finished
            Ok(()) => self.set_state(match before {
                TreeState::Active(_) => TreeState::Ready,
                state => state,
            }),
            Err(ref e) => {
                let ev: TreeEvent = TreeEvent::new(&e.to_string());
                self.add_error(ev.clone());
                // the state the worker died in; Active = an op half-applied
                if self.active_op().is_some() {
                    self.set_state(TreeState::Inconsistent(ev));
                }
            }
        }
        res
    }

    /**
    Tell the background worker thread to quit. Optionally wait till
    the worker thread exits; a worker that panicked is reported as
    [TreeError::WorkerPanicked]. Never fails without `block`.
    */
    pub(super) fn quit_worker(&self, block: bool) -> TreeResult<()> {
        self.queue_op_prio(TreeOp::Quit);
        if block && let Some(worker) = self.worker.lock().take() {
            worker
                .join()
                .map_err(|payload| TreeError::WorkerPanicked(panic_message(payload)))?;
        }
        Ok(())
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

    /**
    Tell the background worker thread to diff-rescan (update) the given
    path: entries that appeared on disk are inserted, entries that
    vanished are removed, entries whose inode changed are replaced.

    NOTE: non-blocking; see [DirTree::update] for the direct form.
    */
    pub fn rescan(&self, path: &str) {
        self.queue_op(TreeOp::Update(PathBuf::from(path)));
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
        let store: UniqueStrStore = UniqueStrStore::new_with_capacity(1024, ARENA_CHUNK_SIZE);
        let name_idx: u32 = store.insert("ROOT");
        DirTree {
            root: Node::new(NodeItem::Root(Directory::new(name_idx)), None).into(),
            conf: TreeConf::new(filemode, filters).into(),
            strings: store,
            // spelled out: a Drop type cannot take the ..Default::default() form
            worker: Mutex::default(),
            handles: OpenHandles::default(),
            state: RwLock::default(),
            workq: RwLock::default(),
            events: RwLock::default(),
        }
    }

    /// Set the root (filesystem) path of the tree.
    pub fn from_path(self, path: &str) -> Self {
        self.conf.set_from(path);
        // the root actually stored: a second from_path() keeps the first
        if let Some(from) = self.conf.from() {
            self.insert(from, NodeType::Directory, None);
        }
        self.set_state(TreeState::Empty);
        self
    }

    /**
    Attach a [`Visitor`] to this tree. The visitor's hooks
    ([`Visitor::visit_dir`], [`Visitor::prune_child`],
    [`Visitor::max_depth`]) are invoked from the parallel walker
    (`populate_par`), so a visitor forces the parallel walker whatever
    the sync setting.
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

    /// Builder: walk with the synchronous walker (ignored when a visitor is set).
    pub fn with_sync(self, val: bool) -> Self {
        self.conf.set_sync(val);
        self
    }

    /// Builder: report progress to `observer`. Only the first one set takes effect.
    pub fn with_observer(self, observer: Arc<dyn TreeObserver>) -> Self {
        self.conf.set_observer(observer);
        self
    }

    /**
    Walk this tree's configured root path, populating it (one level only
    unless recursive, see `with_recursive()`). Blocking; progress goes to
    the observer set with `with_observer()`. Fails with
    [TreeError::NoRoot] if no root path was set with `from_path()`.

    The walker is picked as in [DirTree::populate_auto].

    This is the recommended entry point for builder-style construction:

    ```ignore
    let tree = DirTree::new(filemode, filters)
        .from_path("/some/root")
        .with_recursive(true)
        .with_observer(Arc::new(progress))
        .with_visitor(Arc::new(visitor));
    tree.walk()?;
    ```
    */
    #[instrument(name = "DirTree", skip_all)]
    pub fn walk(&self) -> TreeResult<()> {
        let from: PathBuf = self.from()?.clone();
        debug!(target: "path", "{}", from.display());
        // the root dir itself, which the walk lists but does not attach
        self.conf.observer().dirs_added(1);
        self.set_state(TreeState::Active(TreeOp::Build(from.clone())));
        self.populate_auto(&from, Some(self.conf.recursive()));
        self.set_state(TreeState::Ready);
        Ok(())
    }

    /**
    Build a new [[DirTree]] with the given options and start the worker thread.

    NOTE: must be chained with `from_path()` to set the root path.

    Fails with [TreeError::NoRoot] without a root path, and with
    [TreeError::WorkerSpawn] if the worker thread cannot be started.
    */
    pub fn build(self) -> TreeResult<Arc<Self>> {
        self.from()?;
        let tree: Arc<Self> = self.into();
        let tree_w: Weak<Self> = Arc::downgrade(&tree);
        let worker: thread::JoinHandle<()> = thread::Builder::new()
            .stack_size(256 * 1024) // 256 KiB
            .name("tree_worker".into())
            .spawn(|| tree_worker(tree_w))
            .map_err(TreeError::WorkerSpawn)?;
        *tree.worker.lock() = Some(worker);
        Ok(tree)
    }

    /**
    Populate `path` with the walker the tree is configured for: the
    parallel [DirTree::populate_par] unless sync mode is on, and always
    when a visitor is set (the visitor protocol is parallel-walker only).
    Otherwise the synchronous [DirTree::populate].

    NOTE: If `recursive` is [None], the tree's default is used.
    */
    pub fn populate_auto(&self, path: &Path, recursive: Option<bool>) {
        match !self.conf.sync() || self.has_visitor() {
            true => self.populate_par(path, recursive),
            false => self.populate(path, recursive),
        }
    }

    /// Populate a leaf [[Node]] in the trie with the contents of a directory.
    /// Uses the standard [std::fs::read_dir] method to get the directory entries.
    ///
    /// NOTE: If `recursive` is [None], the tree's default is used.
    ///
    /// NOTE: single threaded, potentially slow with large directory trees.
    #[instrument(level = "debug", skip_all, fields(p = self.rel_path(path).to_str()))]
    pub fn populate(&self, path: &Path, recursive: Option<bool>) {
        trace!(target: "get_entries", "{}", path.display());
        match path.read_dir() {
            Ok(entries) => {
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
                                self.conf.observer().dirs_added(1);
                                // an explicit per-op flag overrides the tree default
                                if recursive.unwrap_or(self.conf.recursive()) {
                                    if self.is_worker_running() {
                                        self.queue_op(TreeOp::Scan(path, recursive));
                                    } else {
                                        self.populate(&path, recursive);
                                    }
                                }
                            } else if let Some(kind) = FileKind::from_std(entry_t) {
                                // a name-only entry could not tell a special file apart
                                if kind.is_special() && self.filemode().is_name() {
                                    return;
                                }
                                if !self.conf.filters().passes(&name, false) {
                                    return;
                                }
                                let mut size: u64 = 0;
                                if !kind.is_special() && self.filemode().is_with_size() {
                                    match entry.metadata() {
                                        Ok(meta) => {
                                            size = meta.len();
                                        }
                                        // likely deleted between readdir and stat
                                        Err(e) => {
                                            self.add_error(TreeEvent::error(
                                                &e.to_string(),
                                                &TreeOp::Scan(path.to_path_buf(), recursive),
                                            ));
                                            return;
                                        }
                                    }
                                }
                                if self.filemode().is_name() {
                                    self.insert(&path, NodeType::Name, None);
                                } else if self.filemode().is_node() {
                                    let target: Option<u32> = match kind {
                                        FileKind::Symlink => self.link_target(&path),
                                        _ => None,
                                    };
                                    let file: FileEntry = FileEntry::new(kind, target);
                                    self.insert_with(&path, NodeType::File, Some(entry.ino()), file);
                                }
                                match kind.is_special() {
                                    true => self.conf.observer().specials_added(1),
                                    false => self.conf.observer().files_added(1, size),
                                }
                            }
                        }
                        Err(e) => {
                            self.add_error(TreeEvent::error(
                                &e.to_string(),
                                &TreeOp::Scan(path.to_path_buf(), recursive),
                            ));
                            debug!("Error with {}: {e}", path.display());
                        }
                    }
                })
            }
            Err(e) => {
                self.add_error(TreeEvent::error(
                    &e.to_string(),
                    &TreeOp::Scan(path.to_path_buf(), recursive),
                ));
                debug!(target: "ERROR", "Cannot read directory {}: {e}", path.display());
            }
        };
    }

    /**
    Parallel version of [DirTree::populate] using [rayon::iter]
    to process each directory entry in parallel.

    Uses [[DirHandle]] to read the directory entries, and its [DirHandle::iter]
    method which tries to return inner directories first using a small
    buffer to look ahead in the directory stream.
    */
    /// NOTE: If `recursive` is [None], the tree's default is used.
    pub fn populate_par(&self, path: &Path, recursive: Option<bool>) {
        // the walker passes owned paths down; one conversion per walk root
        let path: &PathBuf = &path.to_path_buf();
        /*
        Resolve the walk root's node up front - children are attached
        directly to their parent's node during the walk (one intern and
        one lock per entry), instead of a full root-to-leaf trie walk
        per entry through [DirTree::insert].
        */
        let path_str = path.to_string_lossy();
        let node: Arc<Node> = match self.get_node(path_str.as_ref()) {
            Some(n) => n,
            None => {
                // an unknown walk root (e.g. a worker Scan op) is created first
                self.insert(path, NodeType::Directory, None);
                match self.get_node(path_str.as_ref()) {
                    Some(n) => n,
                    None => {
                        self.add_error(TreeEvent::error(
                            &format!("Cannot resolve walk root: {}", path.display()),
                            &TreeOp::Scan(path.clone(), recursive),
                        ));
                        return;
                    }
                }
            }
        };
        if !node.is_traversable() {
            self.add_error(TreeEvent::error(
                &format!("Walk root is not a directory: {}", path.display()),
                &TreeOp::Scan(path.clone(), recursive),
            ));
            return;
        }
        let walk = self.initial_walk_state(path, recursive);
        // the walk root is opened by path; a symlinked root is followed
        let handle: Result<DirHandle, Error> = DirHandle::new(path);
        rayon::scope(|s| self.populate_par_inner(path, handle, walk, node, s, 0, 0));
    }

    /**
    Build the [`WalkState`] for the very first call into the walker.
    The interned name is the basename of `path`; if `path` has no
    basename (e.g. `/`) we fall back to interning the empty string.
    */
    fn initial_walk_state(&self, path: &Path, recursive: Option<bool>) -> WalkState {
        let name_idx = path
            .file_name()
            .map(|n| self.strings.insert(n.to_string_lossy().as_ref()))
            .unwrap_or_else(|| self.strings.insert(""));
        // number of path components below the filesystem root
        let base_depth: u8 = path
            .components()
            .count()
            .saturating_sub(1)
            .min(u8::MAX as usize) as u8;
        WalkState {
            scope: SCOPE_NONE,
            name_idx,
            parent_name_idx: None,
            recursive: recursive.unwrap_or(self.conf.recursive()),
            base_depth,
        }
    }

    /// Emit a [`WalkEvent`] on the discovery channel, if one is configured.
    /// No-op if no consumer is attached or the receiver has been dropped.
    fn emit_walk_event(
        &self,
        path: &Path,
        tag: ScopeTag,
        scope: ScopeTag,
        depth: usize,
        claimed: bool,
    ) {
        if let Some(tx) = self.conf.discovery_tx() {
            let _ = tx.send(WalkEvent {
                path: path.to_path_buf(),
                tag,
                scope,
                depth,
                claimed,
            });
        }
    }

    /**
    The recursive workhorse of the parallel walker.

    `handle` is this directory, as opened by the caller (see
    [open_dir_nofollow] for why descents never follow symlinks). `node`
    is this directory's own node in the trie; children are attached
    directly under it. `depth` is the semantic distance from
    the walk root (drives visitor depth caps and events); `frames` is
    the direct-recursion count since the last rayon spawn (drives the
    stack-bounding spawn decision) - the two must not be conflated, or
    a spawn would reset the visitor's view of tree depth.
    */
    #[
        instrument(level = "debug", name = "p_par_inner", skip_all,
        fields(p = self.rel_path(path).to_str(), d = depth))
    ]
    #[allow(clippy::too_many_arguments)]
    fn populate_par_inner<'env>(
        &'env self,
        path: &PathBuf,
        handle: Result<DirHandle, Error>,
        walk: WalkState,
        node: Arc<Node>,
        rs: &rayon::Scope<'env>,
        depth: usize,
        frames: usize,
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

        let op: TreeOp = TreeOp::Scan(path.into(), Some(walk.recursive));
        let mut handle: DirHandle = match handle {
            Ok(h) => h,
            Err(e) => {
                self.add_error(TreeEvent::error(&e.to_string(), &op));
                debug!(target: "ERROR", "Cannot read directory: {}", e);
                return;
            }
        };
        trace!(target: "iter_dir", "{:?} ::: {handle:?}", path.display());

        /*
        Collect the dirents first (visit_dir needs the full list anyway),
        then process them with indexed parallel iterators. par_bridge()
        over the streaming iterator was measured to be a bad fit here:
        with O(1) child insertion the per-entry work is so small that
        par_bridge's per-item synchronization dominated the walk (futex
        storm, 4x the context switches). An indexed iterator splits the
        work log(n) times instead.

        Since dirhandle 0.4.2 each `EntryExt` borrows its `DirHandle`, so
        the Vec must be consumed before the handle is moved below.

        Since dirhandle 0.6.0 a `readdir` error ends the iteration early
        instead of posing as a clean end of stream. The entries read so
        far are real and still get inserted, but the listing is partial,
        so the error must be recorded rather than silently accepted.

        iter_untracked() (dirhandle 0.6.1): the walker never reads the
        handle's DirectoryState, and partitions the entries itself, so
        skip the state pass (a directory fstat and a digest per entry)
        and the dir-first lookahead.
        */
        // a watcher watches this directory before it is read (see ListHook)
        self.conf.before_listing(&node);
        /*
        For readlinkat() on the symlinks listed here. Taken before the
        listing borrows the handle mutably; the handle is only moved once
        every entry has been processed, so the fd stays open meanwhile.
        */
        let dirfd: BorrowedFd<'_> = unsafe { BorrowedFd::borrow_raw(handle.as_raw_fd()) };
        let mut iter = handle.iter_untracked();
        let entries: Vec<EntryExt> = iter.by_ref().collect();
        if let Some(e) = iter.error() {
            self.add_error(TreeEvent::error(
                &format!("readdir failed, listing incomplete: {e}"),
                &op,
            ));
        }
        drop(iter);
        let mut scope_for_children: ScopeTag = walk.scope;
        let mut skip_children: bool = false;
        if let Some(ref v) = visitor {
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
        }
        if !skip_children {
            /*
            Subdirectories and files are scheduled differently. A single
            with_min_len(ENTRY_BATCH_MIN) over all entries never split a
            directory of fewer than 2 * ENTRY_BATCH_MIN entries, so a tree
            of small directories - most real trees - was walked almost
            serially: its subdirectories all recursed on one thread (at
            16 workers on /usr, ~3 CPUs busy and the rest spinning in
            sched_yield). Each subdirectory now is a task of its own, and
            only files are batched - one children-map lock and one round
            of counter updates per batch (see `process_par_files`).
            */
            let visitor_ref = visitor.as_ref();
            let mut entries = entries;
            let n_dirs: usize = partition_dirs_first(&mut entries);
            let (dirs, files) = entries.split_at(n_dirs);
            if let Some(dir) = node.as_dir() {
                dir.reserve_children(entries.len());
            }
            let each_dir = |entry: &EntryExt| {
                self.process_par_dir(
                    path,
                    &walk,
                    &node,
                    scope_for_children,
                    depth,
                    frames,
                    rs,
                    visitor_ref,
                    &op,
                    entry,
                );
            };
            let each_files = |chunk: &[EntryExt]| {
                self.process_par_files(path, &walk, &node, depth, visitor_ref, &op, dirfd, chunk);
            };
            rayon::join(
                || dirs.par_iter().for_each(each_dir),
                || files.par_chunks(ENTRY_BATCH_MIN).for_each(each_files),
            );
        }

        // shall we keep the directory handle (file descriptor) open?
        if self.conf.resident() {
            /*
            We already hold this directory's node - no path lookup needed.
            Pool the handle only if the node takes its fd: a re-populated
            node keeps the handle it already has, and pooling a second one
            would leak it (no node would ever point at it for closing).
            */
            let fd: RawFd = handle.as_raw_fd();
            if node
                .as_dir()
                .is_some_and(|dir: &Directory| dir.fd_set(fd).is_ok())
            {
                self.handles.insert(handle);
            }
        } else {
            drop(handle); // unnecessary, but explicit
        }
    }

    /**
    Per-subdirectory processing for the parallel walker, run from an
    indexed parallel iterator over the directory entries of the parent's
    collected `Vec<EntryExt>` (with or without a visitor): prune, filter,
    attach the child node under `parent_node` via [DirTree::insert_child],
    and recurse into it.
    */
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn process_par_dir<'env>(
        &'env self,
        parent_path: &Path,
        parent_walk: &WalkState,
        parent_node: &Arc<Node>,
        scope_for_children: ScopeTag,
        depth: usize,
        frames: usize,
        rs: &rayon::Scope<'env>,
        visitor: Option<&Arc<dyn Visitor>>,
        op: &TreeOp,
        entry: &EntryExt<'_>,
    ) {
        /*
        name_as_bytes() borrows the name stored in the entry itself.
        from_bytes() wraps it as &OsStr without any allocation, so
        filtered entries never pay for the String allocation from
        entry.name().
        */
        let name_os = OsStr::from_bytes(entry.name_as_bytes());
        trace!(target: "ENTRY", "{:?} : {:?}", name_os, entry);
        // children of this directory live one path component deeper
        let depth_abs: u8 =
            (parent_walk.base_depth as usize + depth + 1).min(u8::MAX as usize) as u8;

        /*
        Path-aware prune via the visitor (when set). The name is interned
        up front for the visitor (u32-vs-u32 compares); otherwise only
        after the cheaper filter check has passed.
        */
        let mut child_idx: Option<u32> = None;
        if let Some(v) = visitor {
            let idx: u32 = self.strings.insert(name_os.to_string_lossy().as_ref());
            child_idx = Some(idx);
            let parent_ctx = WalkContext {
                path: parent_path,
                name_idx: parent_walk.name_idx,
                parent_name_idx: parent_walk.parent_name_idx,
                depth,
                scope: parent_walk.scope,
                strings: &self.strings,
            };
            if v.prune_child(&parent_ctx, idx, true) {
                return;
            }
        }
        if !self.conf.filters().passes(name_os, true) {
            return;
        }
        let child_idx: u32 =
            child_idx.unwrap_or_else(|| self.strings.insert(name_os.to_string_lossy().as_ref()));
        let (child, _) =
            self.insert_child(parent_node, child_idx, NewChild::Dir, entry.ino(), depth_abs);
        self.conf.observer().dirs_added(1);
        let Some(child_node) = child else {
            // a name-only entry (or similar) blocks this slot
            self.add_error(TreeEvent::error(
                &format!("Cannot attach directory node: {:?}", name_os),
                op,
            ));
            return;
        };
        if !parent_walk.recursive {
            return;
        }

        let entry_p: PathBuf = parent_path.join(name_os);
        let next = WalkState {
            scope: scope_for_children,
            name_idx: child_idx,
            parent_name_idx: Some(parent_walk.name_idx),
            recursive: true,
            base_depth: parent_walk.base_depth,
        };
        // spawning is slower than direct recursion, so only spawn after
        // MAX_RECURSE_DEPTH frames to bound stack use; semantic depth
        // keeps increasing across spawns.
        if frames < MAX_RECURSE_DEPTH {
            // relative to our open handle, never following a symlink
            self.populate_par_inner(
                &entry_p,
                entry.open_dir(),
                next,
                child_node,
                rs,
                depth + 1,
                frames + 1,
            );
        } else {
            /*
            The parent's handle may be gone by the time the task runs
            (and opening ahead would hold one fd per queued task), so
            the task opens by path.
            */
            rs.spawn(move |s| {
                self.populate_par_inner(
                    &entry_p,
                    open_dir_nofollow(&entry_p),
                    next,
                    child_node,
                    s,
                    depth + 1,
                    0,
                )
            });
        }
    }

    /**
    One batch of non-directory entries for the parallel walker. Pruning,
    filtering and node construction run lock-free; the survivors are then
    attached to `parent_node` under a single children-map write lock, and
    the shared counters are bumped once per batch instead of once per
    entry - with per-entry updates, the global `Counter` mutexes and the
    parent's map lock were contended by every worker at once.

    Special files are recorded with their [FileKind] (Node filemode only:
    a name-only entry could not tell them apart) and counted apart from
    regular files; a symlink's target is read through `dirfd`, the
    directory being listed. Entries of undeterminable type are reported
    as events. No paths are constructed: the interned name and the inode
    from the dirent are all an insertion requires.
    */
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn process_par_files(
        &self,
        parent_path: &Path,
        parent_walk: &WalkState,
        parent_node: &Arc<Node>,
        depth: usize,
        visitor: Option<&Arc<dyn Visitor>>,
        op: &TreeOp,
        dirfd: BorrowedFd<'_>,
        batch: &[EntryExt<'_>],
    ) {
        let Some(dir) = parent_node.as_dir() else {
            return;
        };
        let name_only: bool = self.filemode().is_name();
        let store: bool = name_only || self.filemode().is_node();
        let with_size: bool = self.filemode().is_with_size();
        let mut children: Vec<(u32, MaybeNode)> = Vec::with_capacity(batch.len());
        /*
        Names still to intern, with their inodes and kinds: interned
        together after the loop. Every new name takes stringstore's writer
        mutex, and with a mutex round per name the handoffs between workers
        dominated the walk of a tree of unique file names (it got slower
        past 4 workers).
        */
        let mut names: Vec<Cow<str>> = Vec::with_capacity(batch.len());
        let mut leaves: Vec<(u64, FileEntry)> = Vec::with_capacity(batch.len());
        // specials go in apart from the files, to be counted apart
        let mut specials: Vec<(u32, MaybeNode)> = Vec::new();
        let mut seen: u64 = 0;
        let mut seen_specials: u64 = 0;
        let mut size: u64 = 0;
        // name-only files get no node, just a (name -> None) entry
        let file_node = |inode: u64, file: FileEntry| -> MaybeNode {
            match name_only {
                true => None,
                false => Some(
                    Node::new(
                        NodeItem::File(Entry::<FileEntry>::leaf(inode, file)),
                        Some(parent_node.clone()),
                    )
                    .into(),
                ),
            }
        };

        for entry in batch {
            let Some(entry_t) = entry.file_type() else {
                let entry_p: PathBuf = parent_path.join(OsStr::from_bytes(entry.name_as_bytes()));
                self.add_event(
                    TreeEvent::new("Unknown entry type")
                        .path(&entry_p.to_string_lossy())
                        .op(op),
                );
                debug!(target: "WARN", "Unknown entry type: {}", entry_p.display());
                continue;
            };
            let Some(kind) = FileKind::from_type(entry_t) else {
                continue;
            };
            if kind.is_special() && name_only {
                continue;
            }
            let name_os: &OsStr = OsStr::from_bytes(entry.name_as_bytes());
            trace!(target: "ENTRY", "{:?} : {:?}", name_os, entry);

            // same prune + filter shape as for directories
            let mut child_idx: Option<u32> = None;
            if let Some(v) = visitor {
                let idx: u32 = self.strings.insert(name_os.to_string_lossy().as_ref());
                child_idx = Some(idx);
                let parent_ctx = WalkContext {
                    path: parent_path,
                    name_idx: parent_walk.name_idx,
                    parent_name_idx: parent_walk.parent_name_idx,
                    depth,
                    scope: parent_walk.scope,
                    strings: &self.strings,
                };
                if v.prune_child(&parent_ctx, idx, false) {
                    continue;
                }
            }
            if !self.conf.filters().passes(name_os, false) {
                continue;
            }
            if kind.is_special() {
                seen_specials += 1;
            } else {
                seen += 1;
                if with_size {
                    size += entry.len();
                }
            }
            if !store {
                continue;
            }
            let target: Option<u32> = match kind {
                FileKind::Symlink => self.link_target_at(dirfd, entry.file_name()),
                _ => None,
            };
            let file: FileEntry = FileEntry::new(kind, target);
            match child_idx {
                // already interned for the visitor
                Some(idx) if kind.is_special() => specials.push((idx, file_node(entry.ino(), file))),
                Some(idx) => children.push((idx, file_node(entry.ino(), file))),
                None => {
                    names.push(name_os.to_string_lossy());
                    leaves.push((entry.ino(), file));
                }
            }
        }
        if !names.is_empty() {
            let indices: Vec<u32> = self.strings.insert_many(&names);
            for (idx, (inode, file)) in indices.into_iter().zip(leaves) {
                match file.kind().is_special() {
                    true => specials.push((idx, file_node(inode, file))),
                    false => children.push((idx, file_node(inode, file))),
                }
            }
        }

        let added: u32 = match children.is_empty() {
            true => 0,
            false => dir.add_children_new(children),
        };
        let added_specials: u32 = match specials.is_empty() {
            true => 0,
            false => dir.add_children_new(specials),
        };
        if added + added_specials > 0 {
            // name-only entries are files, but not nodes
            if !name_only {
                self.conf.nodes_mod((added + added_specials) as i32);
            }
            self.conf.files_mod(added as i32);
            self.conf.specials_mod(added_specials as i32);
            // files live one path component below their directory
            let depth_abs: u8 =
                (parent_walk.base_depth as usize + depth + 1).min(u8::MAX as usize) as u8;
            self.conf.depth_compare(depth_abs);
        }
        if seen > 0 {
            self.conf.observer().files_added(seen, size);
        }
        if seen_specials > 0 {
            self.conf.observer().specials_added(seen_specials);
        }
    }

    /**
    O(1) child insertion under an already-resolved parent [[Node]]: one
    children-map lock instead of a full root-to-leaf trie walk like
    [DirTree::insert]. The inode must already be known (it comes from
    the dirent), so construction never stats and cannot fail.

    Returns the child (created or pre-existing) and whether this call
    created it; `(None, false)` when the slot is occupied by a name-only
    entry or `parent` is not a directory.
    */
    #[inline]
    pub(super) fn insert_child(
        &self,
        parent: &Arc<Node>,
        name_idx: u32,
        child: NewChild,
        inode: u64,
        depth_abs: u8,
    ) -> (MaybeNode, bool) {
        let dir: &Directory = match parent.as_dir() {
            Some(d) => d,
            None => return (None, false),
        };
        let kind: Option<FileKind> = match &child {
            NewChild::Dir => None,
            NewChild::Leaf(file) => Some(file.kind()),
        };
        let (node, created) = dir.get_or_add_child_with(name_idx, || {
            let itm: NodeItem = match child {
                NewChild::Leaf(file) => NodeItem::File(Entry::<FileEntry>::leaf(inode, file)),
                NewChild::Dir => {
                    let itm = NodeItem::Dir(Entry::<Directory>::with_inode(inode));
                    itm.set_dir_name(name_idx);
                    itm
                }
            };
            Some(Node::new(itm, Some(parent.clone())).into())
        });
        if created {
            self.conf.nodes_mod(1);
            match kind {
                Some(kind) => self.conf.leaf_mod(kind, 1),
                None => self.conf.dirs_mod(1),
            }
            self.conf.depth_compare(depth_abs);
        }
        (node, created)
    }

    /**
    The interned target of the symlink `name` in the directory `dirfd`,
    or [None] if it cannot be read (e.g. removed since it was listed).
    */
    pub(super) fn link_target_at(&self, dirfd: BorrowedFd<'_>, name: &CStr) -> Option<u32> {
        match readlinkat(dirfd, name) {
            Ok(target) => Some(self.strings.insert(target.to_string_lossy())),
            Err(e) => {
                debug!(target: "WARN", "Cannot read symlink {name:?}: {e}");
                None
            }
        }
    }

    /// [DirTree::link_target_at] for a symlink given by its path.
    pub(super) fn link_target(&self, path: &Path) -> Option<u32> {
        match read_link(path) {
            Ok(target) => Some(self.strings.insert(target.to_string_lossy())),
            Err(e) => {
                debug!(target: "WARN", "Cannot read symlink {}: {e}", path.display());
                None
            }
        }
    }

    /// The target of a symlink node, as read when it was recorded.
    pub fn symlink_target(&self, node: &Node) -> Option<PathBuf> {
        let idx: u32 = node.as_file()?.target()?;
        Some(PathBuf::from(unsafe { self.strings.borrow_str(idx) }))
    }

    /// Add a [[RawFd]] to a directory node's [[Directory]] item.
    #[allow(unused)]
    fn add_fd(&self, path: &Path, fd: RawFd) {
        self.get_node(path.to_string_lossy().as_ref()).map(|node| {
            node.as_dir().map(|dir| {
                dir.fd_set(fd).ok();
            })
        });
    }

    /* --------------------------------- */

    /// Inserts a path into the trie. The path must be an absolute filesystem path.
    /// The path is split on forward slash (`/`) to obtain its parts.
    ///
    /// Concurrency-safe: per-level child creation is atomic (check + insert
    /// under one write lock), so parallel inserts of overlapping paths can
    /// neither overwrite each other's nodes nor double-count.
    #[instrument(level = "debug", skip(self))]
    pub fn insert(&self, path: &PathBuf, node_t: NodeType, inode: Option<u64>) {
        self.insert_with(path, node_t, inode, FileEntry::default());
    }

    /// [DirTree::insert], with a [NodeType::File] leaf recorded as `file`.
    pub(super) fn insert_with(
        &self,
        path: &PathBuf,
        node_t: NodeType,
        inode: Option<u64>,
        file: FileEntry,
    ) {
        let mut current: Arc<Node> = self.root();
        let parts = &self.strings.store_path(path)[1..];
        let len: usize = parts.len();
        let mut depth: usize = 0; // root node is at depth 0
        self.conf.depth_compare(len.min(u8::MAX as usize) as u8);
        // max depth can just as well be updated at this point

        for part in parts.iter().copied() {
            depth += 1;
            let is_leaf: bool = depth == len;
            trace!(target: "CURRENT_NODE", "{:?}", self.node_name(&current));

            let dir: &Directory = match current.as_dir() {
                Some(d) => d,
                None => {
                    // a non-container node occupies this path component
                    self.add_error(TreeEvent::error(
                        &format!("Not a directory at depth {depth}/{len}: {current:?}"),
                        &TreeOp::Insert,
                    ));
                    return;
                }
            };

            if is_leaf && node_t == NodeType::Name {
                /*
                optimization: don't create file Nodes at all, just
                record the fact that a file exists in the directory
                NOTE: total node count is not incremented in this case
                */
                if dir.add_name_child(part) {
                    self.conf.files_mod(1);
                    debug!(target: "FILENAME_ADD", "{part:?} (store name only)");
                }
                return;
            }

            /*
            Build the item for a would-be new node up front. Leaf items can
            require a stat (when no inode is given) and thus fail on a
            filesystem race; erroring out here keeps the creation closure
            below infallible. If the child turns out to already exist, the
            speculative item is simply dropped with it.
            */
            let itm: NodeItem = if is_leaf {
                match node_t {
                    NodeType::Directory => match Entry::<Directory>::new(path, inode) {
                        Ok(e) => {
                            let itm = NodeItem::Dir(e);
                            itm.set_dir_name(part);
                            itm
                        }
                        Err(e) => {
                            self.add_error(TreeEvent::error(&e.to_string(), &TreeOp::Insert));
                            return;
                        }
                    },
                    NodeType::File => match Entry::<FileEntry>::new(path, inode) {
                        Ok(e) => NodeItem::File(e.with_file(file)),
                        Err(e) => {
                            self.add_error(TreeEvent::error(&e.to_string(), &TreeOp::Insert));
                            return;
                        }
                    },
                    // Root / Uninitialized / Name are not insertable leaves
                    _ => return,
                }
            } else {
                trace!(target: "intermediate", "{part:?} --> depth: {depth}/{len}");
                // must be a container (directory) so let's create the basic structure
                let itm = NodeItem::Dir(Entry::<Directory>::default());
                itm.set_dir_name(part);
                itm
            };

            let kind: Option<FileKind> = itm.as_file().map(|f: &FileEntry| f.kind());
            let (child, created) =
                dir.get_or_add_child_with(part, || Some(Node::new(itm, Some(current.clone())).into()));
            if created {
                self.conf.nodes_mod(1);
                match kind {
                    Some(kind) => self.conf.leaf_mod(kind, 1),
                    None => self.conf.dirs_mod(1),
                }
            }

            current = match child {
                Some(node) => {
                    if created {
                        debug!(target: "CREATED_NODE", "{part:?} : {:?}", &node);
                    }
                    node
                }
                None => {
                    // a name-only entry occupies this slot; a full path
                    // cannot pass through (or overwrite) it
                    self.add_error(TreeEvent::error(
                        &format!("Name-only entry blocks {part:?} at depth {depth}/{len}"),
                        &TreeOp::Insert,
                    ));
                    return;
                }
            };

            if is_leaf && !created {
                // for now we don't overwrite existing nodes, but
                // this may change in the future to allow for updates
                trace!(target: "SKIP_EX_NODE", "{:?} ({:?}) : {:?}",
                    self.node_name(&current), current.node_t, current.path(&self.strings)
                );
                return;
            }
        }
        debug!(target: "INSERT_CHILD", "{current:?}");
    }

    /// Remove a [[Node]] (or a leaf) from the trie. Expects an absolute path.
    ///
    /// Returns a tuple of `(nodes, dirs, files)` removed on success and [[None]]
    /// if the path was not found. The root node cannot be removed.
    ///
    /// WARNING: implementation is WIP and may yet contain bugs.
    #[instrument(level = "debug", skip(self))]
    pub fn remove(&self, path: &str) -> Result<Option<NodeCounts>, Error> {
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
                        /*
                        get_child_byref is a pointer-identity lookup, so a
                        None here means the node was detached concurrently -
                        report instead of panicking.
                        */
                        let (name, _c) = match parent.get_child_byref(&node) {
                            Some(found) => found,
                            None => {
                                let msg: String = format!("Node detached during removal: {:?}", p);
                                self.add_error(TreeEvent::error(&msg, &op).node(&node));
                                return Err(Error::new(ErrorKind::NotFound, msg));
                            }
                        };
                        debug!(target: "REMOVE_NODE", "{:?}", p.display());
                        Ok(Some(self.remove_child_node(&parent, name, node.clone())))
                    }

                    None => {
                        let msg: String = format!("Stale parent reference: {:?}", p);
                        self.add_error(TreeEvent::error(&msg, &op).node(&node));
                        Err(Error::new(ErrorKind::NotFound, msg))
                    }
                }
            }

            None => {
                /*
                In Name filemode files exist only as name entries in their
                parent's children map (no Node), so get_node cannot find
                them - check for one before declaring the path missing.
                */
                if self.remove_name_entry(path) {
                    self.conf.counts_mod(NodeCounts::NAME_ENTRY, -1);
                    return Ok(Some(NodeCounts::NAME_ENTRY));
                }
                warn!("Node not found: {path:?}");
                Ok(None)
            }
        }
    }

    /**
    Detach an already-resolved child [[Node]] from its parent and adjust
    the tree counters. The subtree below the child is dropped in a
    cascading manner via refcounting. Returns the counts removed.

    Nothing is removed (and empty counts returned) if `name_idx` no longer
    holds `child` itself: the caller resolved the child earlier, and a
    concurrent remove (watcher vs. worker update, say) may have emptied
    the slot or a create re-filled it with a new node since. Removing
    by name alone would delete that newcomer and subtract the old
    subtree's counts a second time.

    The open directory handles (resident mode) of the removed subtree
    are closed. To detach a subtree that will be re-attached, use
    [DirTree::detach_child_node] instead.
    */
    pub(super) fn remove_child_node(
        &self,
        parent: &Node,
        name_idx: u32,
        child: Arc<Node>,
    ) -> NodeCounts {
        let counts: NodeCounts = self.detach_child_node(parent, name_idx, child.clone());
        if !counts.is_empty() {
            self.release_handles(&child);
        }
        counts
    }

    /**
    [DirTree::remove_child_node] without closing the subtree's directory
    handles: for a subtree that is kept around to be re-attached (rename
    support). Whoever finally drops such a subtree calls
    [DirTree::release_handles] on it.
    */
    pub(super) fn detach_child_node(
        &self,
        parent: &Node,
        name_idx: u32,
        child: Arc<Node>,
    ) -> NodeCounts {
        let counts: NodeCounts = self.count_from(child.clone());
        if !parent.remove_child_exact(&name_idx, &child) {
            return NodeCounts::default();
        }
        self.conf.counts_mod(counts, -1);
        counts
    }

    /**
    Close the pooled directory handles (resident mode) of every directory
    in the subtree at `node` and clear their [DirFd]s. Without this, the
    handles of removed directories stay open forever, pinning deleted
    directories and eventually running the process out of fds.
    */
    pub(super) fn release_handles(&self, node: &Arc<Node>) {
        if self.handles.is_empty() {
            return;
        }
        traverse_from(node, &mut |n: &Arc<Node>| {
            if let Some(dir) = n.as_dir()
                && dir.fd().is_open()
            {
                self.handles.close(dir.fd().fd());
                dir.fd().clear();
            }
        });
    }

    /**
    Re-create the detached directory subtree `old` under `parent` as
    `name_idx`, `depth_abs` deep: a directory moved to another parent.
    The trie cannot re-parent a node in place, so the nodes are new, but
    they are built from memory - inodes, interned names, scan stamps,
    Name-mode entries and pooled directory handles (resident mode) carry
    over - so nothing is read from disk, where a rescan would list every
    directory of the subtree again. Whoever holds a node of `old` holds a
    detached node afterwards.

    Any previous occupant of `name_idx` is removed first (a rename over
    an empty directory). `old` must be detached already
    ([DirTree::detach_child_node] took its nodes off the counters); the
    new nodes are counted as they are inserted. Not reported to the
    observer: nothing new appeared on disk. Returns the new subtree root,
    or [None] if `parent` is not a directory.
    */
    pub(super) fn graft_subtree(
        &self,
        parent: &Arc<Node>,
        name_idx: u32,
        old: &Arc<Node>,
        depth_abs: u8,
    ) -> MaybeNode {
        let occupant: Option<MaybeNode> = parent.as_dir()?.read().get(&name_idx).cloned();
        if let Some(Some(occupant)) = occupant {
            self.remove_child_node(parent, name_idx, occupant);
        }
        let inode: u64 = old.inode().unwrap_or(0);
        let (root, _) = self.insert_child(parent, name_idx, NewChild::Dir, inode, depth_abs);
        let root: Arc<Node> = root?;

        // (old directory, its new node, the depth of the new node)
        let mut work: Vec<(Arc<Node>, Arc<Node>, u8)> = vec![(old.clone(), root.clone(), depth_abs)];
        while let Some((from, to, depth)) = work.pop() {
            carry_dir_state(&from, &to);
            let Some(children) = from.children() else {
                continue;
            };
            let children: Vec<(u32, MaybeNode)> =
                children.read().iter().map(|(idx, c)| (*idx, c.clone())).collect();
            let child_depth: u8 = depth.saturating_add(1);
            for (idx, child) in children {
                match child {
                    Some(c) if c.is_traversable() => {
                        let ino: u64 = c.inode().unwrap_or(0);
                        let (new, _) =
                            self.insert_child(&to, idx, NewChild::Dir, ino, child_depth);
                        if let Some(new) = new {
                            work.push((c, new, child_depth));
                        }
                    }
                    Some(c) => {
                        let ino: u64 = c.inode().unwrap_or(0);
                        let file: FileEntry = c.as_file().copied().unwrap_or_default();
                        self.insert_child(&to, idx, NewChild::Leaf(file), ino, child_depth);
                    }
                    None => {
                        // a Name-mode (node-less) file entry
                        if to.as_dir().is_some_and(|d: &Directory| d.add_name_child(idx)) {
                            self.conf.files_mod(1);
                            self.conf.depth_compare(child_depth);
                        }
                    }
                }
            }
        }
        Some(root)
    }

    /**
    Re-attach a previously detached child under `parent` with the given
    name (rename support). The child's stored parent reference must
    still point at `parent` - the trie has no re-parenting, so
    cross-directory moves must re-create nodes instead of using this
    ([DirTree::graft_subtree] for a directory).

    Any existing occupant of the destination name is removed first
    (rename-over semantics), and a directory child is re-labeled with
    the new name. The tree counters are restored from `counts` as
    captured at detach time.

    Returns `false` (leaving the tree unchanged) if `parent` is not a
    directory or is not the child's actual parent.
    */
    pub(super) fn attach_child_node(
        &self,
        parent: &Arc<Node>,
        name_idx: u32,
        child: Arc<Node>,
        counts: NodeCounts,
    ) -> bool {
        let Some(dir) = parent.as_dir() else {
            return false;
        };
        match child.parent() {
            Some(p) if Arc::ptr_eq(&p, parent) => {}
            _ => return false,
        }

        // rename-over: drop any previous occupant of the destination name
        let occupant: Option<MaybeNode> = dir.read().get(&name_idx).cloned();
        match occupant {
            Some(Some(old)) => {
                self.remove_child_node(parent, name_idx, old);
            }
            Some(None) if dir.remove_name_child(&name_idx) => {
                self.conf.counts_mod(NodeCounts::NAME_ENTRY, -1);
            }
            _ => {}
        }

        if child.node_t.is_dir() {
            child.set_dir_name(name_idx);
        }
        dir.add_child(name_idx, Some(child));
        self.conf.counts_mod(counts, 1);
        true
    }

    /// Remove a name-only (Node-less) file entry from its parent directory.
    /// Returns `true` if such an entry existed and was removed.
    fn remove_name_entry(&self, path: &str) -> bool {
        let trimmed: &str = path.trim_end_matches(PATH_SEP);
        let Some((parent_p, name)) = trimmed.rsplit_once(PATH_SEP) else {
            return false;
        };
        // a first-level path like "/foo" splits into ("", "foo")
        let parent_p: &str = if parent_p.is_empty() { PATH_SEP } else { parent_p };
        let Some(name_idx) = self.strings.idx(name) else {
            return false;
        };
        self.get_node(parent_p)
            .and_then(|parent: Arc<Node>| {
                parent
                    .as_dir()
                    .map(|dir: &Directory| dir.remove_name_child(&name_idx))
            })
            .unwrap_or(false)
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
            /*
            Lookup only - a component not in the string store cannot be in
            the tree either. Interning here (via insert) would permanently
            grow the store with every queried nonexistent path.
            */
            let idx: u32 = self.strings.idx(part)?;
            current = current.get_child(&idx)?;
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
                .map(|dir: &Directory| dir.fd().clone())
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
            // NOTE: fd 0 is a valid open fd, hence is_open() and not `fd > 0`
            if dir.fd().is_open() {
                self.handles.get(dir.fd().fd())
            } else {
                let handle: CheckedOutHandle = self.handles.open(&node.path(&self.strings)).ok()?;
                match dir.fd_set(handle.as_raw_fd()) {
                    Ok(_) => Some(handle),
                    // lost a race to another opener: keep theirs, close ours
                    Err(existing) => {
                        handle.close();
                        self.handles.get(existing)
                    }
                }
            }
        })
    }

    /// Remove a handle for a directory path and clear the [DirFd] in the [Directory].
    pub fn handle_close(&self, path: &str) {
        if let Some(node) = self.get_node(path)
            && node.is_dir()
            && let Some(dirfd) = node.dirfd()
            && dirfd.is_open()
        {
            self.handles.close(dirfd.fd());
            dirfd.clear();
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
    pub fn iter_count_from(&self, node: Arc<Node>) -> NodeCounts {
        if node.node_t.is_file() {
            return NodeCounts::of_node(&node);
        }

        let mut counts: NodeCounts = NodeCounts::default();
        /*
        NOTE: trying to convert this iterating closure to a parallel
        one with Rayon's `par_bridge()` makes the counting almost 5x slower.
        This is much more than the slowdown observed with `count_from()`,
        and I have no good explanation for it at this point.
        */
        self.iter_from(node).for_each(|node: Arc<Node>| counts.add_node(&node));
        counts
    }

    /// Count the number of directory and file nodes by iterating the whole tree.
    /// Does not count the root [[Node]].
    pub fn iter_count(&self) -> NodeCounts {
        let mut counts: NodeCounts = self.iter_count_from(self.root());
        counts.nodes -= 1; // remove root node since we started from it
        counts
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
    /// the starting [[Node]] (except root).
    pub fn count_from(&self, node: Arc<Node>) -> NodeCounts {
        if node.node_t.is_file() {
            return NodeCounts::of_node(&node);
        }

        let mut counts: NodeCounts = NodeCounts::default();
        /*
        NOTE: trying to convert this iterating closure to a parallel
        one with `traverse_from_par()` makes the counting almost 50% slower.
        Likely the overhead from moving stuff between threads and having
        to use Atomic versions of counters is the main reason.
        */
        traverse_from(&node, &mut |n: &Arc<Node>| counts.add_node(n));

        if node.node_t == NodeType::Root {
            counts.nodes -= 1; // remove root node if we started from it
        };
        counts
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

/**
Stops the background worker thread, if any. The worker holds only a
`Weak` reference, so once this runs it can no longer reach the tree and
exits on its next round; this waits for that. If the worker itself let go
of the last reference (an operation finishing after every outside `Arc`
was dropped), this runs on the worker thread, which exits right after.
*/
impl Drop for DirTree {
    fn drop(&mut self) {
        let Some(worker) = self.worker.get_mut().take() else {
            return;
        };
        if worker.thread().id() == thread::current().id() {
            return;
        }
        if let Err(payload) = worker.join() {
            error!("tree worker thread panicked: {}", panic_message(payload));
        }
    }
}

impl Display for DirTree {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "DirTree: nodes {}, dirs {}, files {}, specials {}, depth {}, handles {}, ctime {} UTC",
            self.conf.nodes(),
            self.conf.dirs(),
            self.conf.files(),
            self.conf.specials(),
            self.conf.depth(),
            self.handles.len(),
            self.created()
        )
    }
}

/**
Move the directory entries to the front of `entries` (unstable, in
place) and return how many there are, so that subdirectories and other
entries can be scheduled differently. Entries of undeterminable type
count as non-directories, like everywhere else.
*/
fn partition_dirs_first(entries: &mut [EntryExt<'_>]) -> usize {
    let mut n_dirs: usize = 0;
    for i in 0..entries.len() {
        if entries[i].is_dir() {
            entries.swap(n_dirs, i);
            n_dirs += 1;
        }
    }
    n_dirs
}

/**
Open a directory by path for a walker descent, without following a
symlink in the final component - the counterpart of
[EntryExt::open_dir] for when the parent's handle is no longer at hand.
The walker only descends into entries readdir reported as directories;
should one be swapped for a symlink before the open, following it would
walk a foreign subtree into the tree (or loop, via `link -> .`). With
`O_NOFOLLOW` the open fails with `ELOOP` instead.
*/
fn open_dir_nofollow(path: &Path) -> Result<DirHandle, Error> {
    let flags: OFlag = OFlag::O_RDONLY
        | OFlag::O_DIRECTORY
        | OFlag::O_NOFOLLOW
        | OFlag::O_CLOEXEC
        | OFlag::O_NONBLOCK;
    DirHandle::from_fd(open(path, flags, Mode::empty())?)
}

/**
Carry the per-directory state that a [DirTree::graft_subtree] copy would
otherwise lose from the old directory node to its new one: the scan
stamp (so the diff pre-check still skips it) and the pooled handle's fd
(the handle stays pooled; only the old node, being dropped, stops
pointing at it).
*/
fn carry_dir_state(from: &Node, to: &Node) {
    if let Some(stamp) = from.scan_stamp() {
        to.set_scan_stamp(stamp);
    }
    if let (Some(old), Some(new)) = (from.as_dir(), to.as_dir())
        && old.fd().is_open()
    {
        new.fd_set(old.fd().fd()).ok();
    }
}

/* ######################################################################### */

/// Iterator for walking through a [[DirTree]].
pub struct DirTreeIterator<'a>(VecDeque<Arc<Node>>, &'a UniqueStrStore);

impl<'a> Iterator for DirTreeIterator<'a> {
    type Item = Arc<Node>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop_front().inspect(|node: &Arc<Node>| {
            trace!(target: "DirTreeIterator", "{}", node.path(self.1).display());
            let Some(children) = node.children() else {
                return;
            };

            // Push all found children to the stack
            children.read().values().flatten().for_each(|child: &Arc<Node>| {
                if child.is_traversable() {
                    // push directories to the front of the queue...
                    self.0.push_front(child.clone());
                } else {
                    // ...and files to the back
                    self.0.push_back(child.clone());
                }
            });
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
