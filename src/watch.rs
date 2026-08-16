// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
Inotify-based tree following for resident mode (Linux-only).

[`TreeWatcher`] pins one inotify watch per directory node of a [`DirTree`]
and applies filesystem events to the tree as they happen: created entries
are inserted (new directories are watched first, then scanned recursively),
deleted or moved-away entries are removed along with their subtrees. The
watch map keys kernel watch descriptors to `Weak<Node>`, so a removed
subtree self-heals: an event for a dead node drops the stale watch on
sight instead of requiring bookkeeping at removal time.

v1 caveats (deliberate, recorded for the future crate split):
- The watcher thread is the only continuous mutator of a resident tree
  (the worker thread idles). Node-level insertion is atomic, but no
  cross-operation ordering with worker ops is guaranteed.
- [`Filters`](crate::filters::Filters) are honored for new entries. A
  configured [`Visitor`](super::Visitor) runs for new-directory subtree
  scans (they go through the normal parallel walker) but is not
  consulted for single-file events.
- Renames are correlated via move cookies: a rename within one directory
  re-attaches the detached subtree in place (node identity, contents and
  kernel watches survive - no rescan), a cross-directory file move is
  rebuilt from its known inode without a stat, and only cross-directory
  *directory* moves pay a re-scan (the trie has no re-parenting). A
  `MOVED_FROM` with no matching `MOVED_TO` within [`PENDING_MOVE_TTL`]
  is a move out of the tree and drops the subtree. Events arriving for
  a subtree during its FROM->TO limbo window are skipped.
- A kernel queue overflow (`IN_Q_OVERFLOW`) marks the tree
  [`TreeState::Inconsistent`], repairs it with a full
  [diff-rescan](DirTree::update) (which fixes both lost creations and
  lost deletions), re-establishes watches and restores
  [`TreeState::Ready`]; only a failed resync leaves the tree flagged.
- Entries created between the initial tree build and watcher start are
  not seen (the usual scan-to-watch gap).
*/

use super::dirtree::DirTree;
use super::event::{TreeEvent, TreeOp, TreeState};
use super::node::{MaybeNode, Node, NodeType};
use super::traverse::traverse_from;
use crate::ScanState;

use dashmap::DashMap;
use parking_lot::Mutex;
use tracing::{debug, error, trace, warn};

use std::{
    ffi::{CString, OsStr},
    io,
    mem::size_of,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed},
    sync::{Arc, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// One `read()` worth of inotify events. The kernel refuses reads that
/// cannot fit at least one maximal event, so this must be comfortably
/// larger than `size_of::<inotify_event>() + NAME_MAX + 1`.
const EVENT_BUF_LEN: usize = 64 * 1024;
/// How long a single `poll()` blocks before re-checking the quit flag.
const POLL_TIMEOUT_MS: i32 = 500;
/// Size of the fixed header preceding each event's name bytes.
const EVENT_HDR: usize = size_of::<libc::inotify_event>();
/// Directory watch mask: entry creation/removal/moves, self-removal.
/// `IN_ONLYDIR` guards against a path being swapped for a non-directory
/// between resolution and watch; `IN_EXCL_UNLINK` mutes noise from
/// already-unlinked-but-open entries.
const DIR_MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR
    | libc::IN_EXCL_UNLINK;
/**
How long a detached `IN_MOVED_FROM` subtree waits for its matching
`IN_MOVED_TO` cookie before being dropped as moved-out-of-tree. The
kernel queues the two events back to back, so this only needs to cover
a read-batch boundary.
*/
const PENDING_MOVE_TTL: Duration = Duration::from_secs(2);

/**
A child detached by `IN_MOVED_FROM`, kept alive until the matching
`IN_MOVED_TO` cookie re-attaches it (rename), re-creates it elsewhere
(cross-directory move), or the TTL expires (moved out of the tree).
Tree counters are adjusted at detach time; `counts` restores them on
re-attach.
*/
struct PendingMove {
    /// The detached child; `None` for a Name-mode (node-less) entry.
    node: MaybeNode,
    /// `(nodes, dirs, files)` counts captured at detach time.
    counts: (u32, u32, u32),
    /// The parent the child was detached from.
    parent: Weak<Node>,
    /// When the `IN_MOVED_FROM` was seen (for expiry).
    seen: Instant,
    is_dir: bool,
}

/**
Follows filesystem changes under a [`DirTree`]'s root and applies them
to the tree. Construct with [`TreeWatcher::start`]; the event loop runs
on its own named thread until [`TreeWatcher::stop`] is called (or the
process exits - the thread holds its own `Arc<TreeWatcher>`, so merely
dropping the caller's handle does not stop it).
*/
pub struct TreeWatcher {
    tree: Arc<DirTree>,
    state: ScanState,
    ino_fd: OwnedFd,
    /// watch descriptor -> the watched directory's node
    watches: DashMap<i32, Weak<Node>>,
    /// move cookie -> detached subtree awaiting rename correlation
    pending: DashMap<u32, PendingMove>,
    /// filesystem events applied to the tree so far
    events_seen: AtomicU64,
    /// watches that could not be established (e.g. max_user_watches)
    failed: AtomicU32,
    quit: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl TreeWatcher {
    /**
    Create an inotify instance, watch every directory currently in the
    tree (from the tree's root path downward), and start the event loop
    thread. Watch registration failures (e.g. `fs.inotify.max_user_watches`
    exhaustion) do not fail the start; they are counted in
    [`TreeWatcher::failed_watches`] and recorded in the tree's event log.
    */
    pub fn start(tree: Arc<DirTree>, state: ScanState) -> io::Result<Arc<Self>> {
        let fd: RawFd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let from: PathBuf = tree.from().clone();
        let from_node: Arc<Node> = tree
            .get_node(from.to_string_lossy().as_ref())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Tree root node not found")
            })?;

        let watcher: Arc<Self> = Arc::new(Self {
            tree,
            state,
            ino_fd: unsafe { OwnedFd::from_raw_fd(fd) },
            watches: DashMap::new(),
            pending: DashMap::new(),
            events_seen: AtomicU64::new(0),
            failed: AtomicU32::new(0),
            quit: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        watcher.watch_subtree(&from_node);
        debug!(target: "WATCH", "watching {} directories under {}",
            watcher.watches.len(), from.display());

        let w: Arc<Self> = watcher.clone();
        let thread: JoinHandle<()> = thread::Builder::new()
            .stack_size(1024 * 1024) // the event loop can run subtree scans
            .name("tree_watch".into())
            .spawn(move || w.event_loop())?;
        *watcher.thread.lock() = Some(thread);
        Ok(watcher)
    }

    /// Signal the event loop to exit and wait for the thread to finish.
    pub fn stop(&self) {
        self.quit.store(true, Relaxed);
        if let Some(t) = self.thread.lock().take() {
            t.join().ok();
        }
    }

    /// Number of currently established directory watches.
    pub fn watches_len(&self) -> usize {
        self.watches.len()
    }

    /// Number of filesystem events applied to the tree so far.
    pub fn events_seen(&self) -> u64 {
        self.events_seen.load(Relaxed)
    }

    /// Number of directories that could not be watched.
    pub fn failed_watches(&self) -> u32 {
        self.failed.load(Relaxed)
    }

    /* --------------------------------- */

    /// Add a watch for a single directory node.
    fn add_watch(&self, node: &Arc<Node>) {
        if !node.is_traversable() {
            return;
        }
        let path: PathBuf = node.path(self.tree.strings());
        let cpath: CString = match CString::new(path.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return, // interior NUL cannot happen for real paths
        };
        let wd: i32 =
            unsafe { libc::inotify_add_watch(self.ino_fd.as_raw_fd(), cpath.as_ptr(), DIR_MASK) };
        if wd < 0 {
            let e: io::Error = io::Error::last_os_error();
            if self.failed.fetch_add(1, Relaxed) == 0 {
                // report the first failure loudly; the rest just count
                warn!("inotify watch failed for {}: {e} (see fs.inotify.max_user_watches)",
                    path.display());
                self.tree.add_error(
                    TreeEvent::new(&format!("inotify watch failed: {e}"))
                        .path(path.to_string_lossy().as_ref()),
                );
            }
            return;
        }
        trace!(target: "WATCH_ADD", "wd {wd} -> {}", path.display());
        self.watches.insert(wd, Arc::downgrade(node));
    }

    /// Watch a directory node and every directory below it.
    fn watch_subtree(&self, node: &Arc<Node>) {
        traverse_from(node, &mut |n: &Arc<Node>| self.add_watch(n));
    }

    /// Drop watches whose nodes have been removed from the tree.
    fn sweep_dead_watches(&self) {
        let fd: RawFd = self.ino_fd.as_raw_fd();
        self.watches.retain(|wd: &i32, w: &mut Weak<Node>| {
            if w.strong_count() == 0 {
                trace!(target: "WATCH_SWEEP", "dropping dead wd {wd}");
                unsafe { libc::inotify_rm_watch(fd, *wd) };
                false
            } else {
                true
            }
        });
    }

    /* --------------------------------- */

    /// Blocking event loop: poll the inotify fd, read event batches and
    /// apply them, until [`TreeWatcher::stop`] raises the quit flag.
    fn event_loop(&self) {
        let fd: RawFd = self.ino_fd.as_raw_fd();
        let mut buf: Vec<u8> = vec![0u8; EVENT_BUF_LEN];
        loop {
            if self.quit.load(Relaxed) {
                break;
            }
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let n: i32 = unsafe { libc::poll(&mut pfd, 1, POLL_TIMEOUT_MS) };
            if n < 0 {
                let e: io::Error = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                error!("inotify poll failed: {e}");
                break;
            }
            if n == 0 {
                // timeout: re-check the quit flag and expire stale moves
                self.expire_pending();
                continue;
            }
            let len: isize =
                unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if len < 0 {
                let e: io::Error = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => continue,
                    _ => {
                        error!("inotify read failed: {e}");
                        break;
                    }
                }
            }
            self.handle_buffer(&buf[..len as usize]);
            self.expire_pending();
        }
        debug!(target: "WATCH", "event loop exiting");
    }

    /// Drop pending moves whose `IN_MOVED_TO` never arrived: the subtree
    /// was moved out of the watched tree and is gone as far as the tree
    /// is concerned (counters were already adjusted at detach time).
    fn expire_pending(&self) {
        if self.pending.is_empty() {
            return;
        }
        let now: Instant = Instant::now();
        let mut sweep: bool = false;
        self.pending.retain(|cookie: &u32, p: &mut PendingMove| {
            if now.duration_since(p.seen) < PENDING_MOVE_TTL {
                return true;
            }
            debug!(target: "WATCH_MV_OUT", "cookie {cookie} expired (moved out of tree)");
            sweep |= p.is_dir;
            false // dropping the Arc cascades the subtree teardown
        });
        if sweep {
            self.sweep_dead_watches();
        }
    }

    /// Whether the node (or any of its ancestors) is a detached subtree
    /// root currently awaiting rename correlation.
    fn in_pending(&self, node: &Arc<Node>) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let roots: Vec<Arc<Node>> = self
            .pending
            .iter()
            .filter_map(|p| p.value().node.clone())
            .collect();
        let mut current: Arc<Node> = node.clone();
        loop {
            if roots.iter().any(|r: &Arc<Node>| Arc::ptr_eq(r, &current)) {
                return true;
            }
            match current.parent() {
                Some(p) => current = p,
                None => return false,
            }
        }
    }

    /// Split one `read()` buffer into its variable-length event records.
    fn handle_buffer(&self, buf: &[u8]) {
        let mut off: usize = 0;
        while off + EVENT_HDR <= buf.len() {
            /*
            Event headers are packed back to back with variable-length
            names between them, so a later header may be unaligned in
            the byte buffer - copy it out instead of referencing it.
            */
            let ev: libc::inotify_event =
                unsafe { ptr::read_unaligned(buf[off..].as_ptr() as *const libc::inotify_event) };
            let name_len: usize = ev.len as usize;
            if off + EVENT_HDR + name_len > buf.len() {
                error!("truncated inotify event record (kernel bug?)");
                break;
            }
            let name_bytes: &[u8] = &buf[off + EVENT_HDR..off + EVENT_HDR + name_len];
            // the name field is NUL-padded to its declared length
            let end: usize = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_len);
            self.handle_event(ev.wd, ev.mask, ev.cookie, OsStr::from_bytes(&name_bytes[..end]));
            off += EVENT_HDR + name_len;
        }
    }

    /// Apply a single inotify event to the tree.
    fn handle_event(&self, wd: i32, mask: u32, cookie: u32, name: &OsStr) {
        trace!(target: "WATCH_EVENT", "wd {wd} mask {mask:#x} cookie {cookie} name {name:?}");

        if mask & libc::IN_Q_OVERFLOW != 0 {
            /*
            The kernel dropped events. A diff-rescan repairs both
            directions (it inserts what appeared AND removes what
            vanished), so mark the tree inconsistent, resync, and
            restore the Ready state on success. Watches for new
            directories are re-established by re-walking the tree -
            re-adding an existing watch is idempotent.
            */
            let ev: TreeEvent = TreeEvent::new("inotify queue overflow, resyncing tree");
            error!("{ev:?}");
            self.tree.add_error(ev.clone());
            self.tree.set_state(TreeState::Inconsistent(ev));

            let from: PathBuf = self.tree.from().clone();
            let from_str = from.to_string_lossy();
            match self.tree.update(from_str.as_ref(), &self.state, Some(true)) {
                Ok(stats) => {
                    self.sweep_dead_watches();
                    if let Some(root) = self.tree.get_node(from_str.as_ref()) {
                        self.watch_subtree(&root);
                    }
                    let msg: String = format!("Tree resynced after overflow: {stats}");
                    warn!("{msg}");
                    self.tree.add_event(TreeEvent::new(&msg));
                    self.tree.set_state(TreeState::Ready);
                }
                Err(e) => {
                    // stays Inconsistent - the consumer must intervene
                    self.tree.add_error(TreeEvent::error(
                        &format!("Overflow resync failed: {e}"),
                        &TreeOp::Update(from.clone()),
                    ));
                }
            }
            return;
        }
        if mask & libc::IN_IGNORED != 0 {
            // the kernel removed this watch (dir deleted or unmounted)
            self.watches.remove(&wd);
            return;
        }

        let node: Arc<Node> = match self.watches.get(&wd).and_then(|w| w.value().upgrade()) {
            Some(n) => n,
            None => {
                // the node is gone from the tree: retire the stale watch
                unsafe { libc::inotify_rm_watch(self.ino_fd.as_raw_fd(), wd) };
                self.watches.remove(&wd);
                return;
            }
        };
        let dir_path: PathBuf = node.path(self.tree.strings());
        /*
        Guard against nodes that were detached from the tree but are kept
        alive by stray strong refs (e.g. a TreeEvent in the event log):
        the path must still resolve to this very node.
        */
        let attached: bool = self
            .tree
            .get_node(dir_path.to_string_lossy().as_ref())
            .is_some_and(|n: Arc<Node>| Arc::ptr_eq(&n, &node));
        if !attached {
            if self.in_pending(&node) {
                /*
                Part of an in-flight rename: skip the event (changes made
                inside a subtree during its FROM->TO limbo are lost, a
                documented v1 gap) but keep the watch - it stays valid
                once the subtree is re-attached.
                */
                return;
            }
            unsafe { libc::inotify_rm_watch(self.ino_fd.as_raw_fd(), wd) };
            self.watches.remove(&wd);
            return;
        }

        self.events_seen.fetch_add(1, Relaxed);

        if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
            if dir_path == *self.tree.from() {
                let ev: TreeEvent = TreeEvent::new("Tree root was removed or moved")
                    .path(dir_path.to_string_lossy().as_ref());
                error!("{ev:?}");
                self.tree.add_error(ev.clone());
                self.tree.set_state(TreeState::Error(ev));
            }
            // subtree cleanup is driven by the parent's IN_DELETE /
            // IN_MOVED_FROM event; IN_IGNORED retires the watch itself
            return;
        }

        let is_dir: bool = mask & libc::IN_ISDIR != 0;

        // moves first: FROM detaches into the pending map, TO correlates
        // by cookie (rename / cross-dir move); an uncorrelated TO falls
        // through to the plain create path below
        if mask & libc::IN_MOVED_FROM != 0 {
            self.on_moved_from(&node, name, cookie, is_dir);
            return;
        }
        if mask & libc::IN_MOVED_TO != 0 && self.on_moved_to(&node, &dir_path, name, cookie, is_dir)
        {
            return;
        }

        let full: PathBuf = dir_path.join(name);

        if mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
            if !self.tree.conf.filters().passes(name, is_dir) {
                return;
            }
            if is_dir {
                debug!(target: "WATCH_MKDIR", "{}", full.display());
                self.tree.insert(&full, NodeType::Directory, None);
                self.state.num_d.inc1();
                /*
                Watch-then-scan: watching the new directory before reading
                it closes the race where entries created right after the
                mkdir would be missed by both the scan and the watch.
                Grandchild directories created before their own watch
                existed are still found by the recursive scan below, and
                produce their own create events afterwards.
                */
                if let Some(new_node) =
                    self.tree.get_node(full.to_string_lossy().as_ref())
                {
                    self.add_watch(&new_node);
                    self.tree.populate_par(&full, &self.state, Some(true));
                    self.watch_subtree(&new_node);
                }
            } else {
                debug!(target: "WATCH_CREATE", "{}", full.display());
                let mode = self.tree.filemode();
                if mode.is_name() {
                    self.tree.insert(&full, NodeType::Name, None);
                } else if mode.is_node() {
                    self.tree.insert(&full, NodeType::File, None);
                }
                self.state.num_f.inc1();
            }
        } else if mask & libc::IN_DELETE != 0 {
            debug!(target: "WATCH_RM", "{}", full.display());
            let op: TreeOp = TreeOp::Remove(full.to_string_lossy().to_string());
            match self.tree.remove(full.to_string_lossy().as_ref()) {
                Ok(Some((nodes, dirs, files))) => {
                    let msg: String =
                        format!("Removed: {nodes} nodes, {dirs} dirs, {files} files");
                    self.tree.add_event(
                        TreeEvent::new(&msg)
                            .path(full.to_string_lossy().as_ref())
                            .op(&op),
                    );
                }
                Ok(None) => {
                    // e.g. an entry our filters never let into the tree
                    debug!(target: "WATCH_RM", "not in tree: {}", full.display());
                }
                Err(e) => {
                    warn!("Removal failed for {}: {e}", full.display());
                }
            }
            // deleted watched subdirectories retire their own watches via
            // the IN_IGNORED events the kernel sends for each of them
        }
    }

    /**
    Handle `IN_MOVED_FROM`: detach the named child from `parent` (with
    counters adjusted) and stash it under the move cookie so a matching
    `IN_MOVED_TO` can re-attach or re-create it. If no match arrives,
    [`TreeWatcher::expire_pending`] drops the stash - the entry was
    moved out of the watched tree.
    */
    fn on_moved_from(&self, parent: &Arc<Node>, name: &OsStr, cookie: u32, is_dir: bool) {
        // lookup only: a name we never interned cannot be in the tree
        let Some(idx) = self.tree.strings.idx(name.to_string_lossy().as_ref()) else {
            return;
        };
        let slot: Option<MaybeNode> = parent
            .as_dir()
            .and_then(|d| d.read().get(&idx).cloned());
        let Some(child_opt) = slot else {
            return; // not in the tree (filtered out or never scanned)
        };

        let counts: (u32, u32, u32) = match &child_opt {
            Some(child) => self.tree.remove_child_node(parent, idx, child.clone()),
            None => {
                // a Name-mode (node-less) file entry
                if parent
                    .as_dir()
                    .is_some_and(|d| d.remove_name_child(&idx))
                {
                    self.tree.conf.files_mod(-1);
                    (0, 0, 1)
                } else {
                    (0, 0, 0)
                }
            }
        };
        debug!(target: "WATCH_MV_FROM", "{name:?} detached (cookie {cookie})");
        self.pending.insert(
            cookie,
            PendingMove {
                node: child_opt,
                counts,
                parent: Arc::downgrade(parent),
                seen: Instant::now(),
                is_dir,
            },
        );
    }

    /**
    Handle `IN_MOVED_TO` for a cookie with a pending `IN_MOVED_FROM`.
    A rename within one directory re-attaches the detached subtree in
    place (node identity, contents and kernel watches all survive); a
    cross-directory move re-creates the entry at the destination (files
    from their known inode without a stat, directories via a re-scan,
    since the trie has no re-parenting). Returns `false` when the
    cookie is unknown - the caller then treats the event as a plain
    create (a move into the tree from outside).
    */
    fn on_moved_to(
        &self,
        parent: &Arc<Node>,
        dir_path: &PathBuf,
        name: &OsStr,
        cookie: u32,
        is_dir: bool,
    ) -> bool {
        let Some((_, pending)) = self.pending.remove(&cookie) else {
            return false;
        };
        // the destination name may be excluded even though the source was tracked
        if !self.tree.conf.filters().passes(name, is_dir) {
            if pending.is_dir {
                self.sweep_dead_watches();
            }
            return true; // handled: the entry ceases to exist for the tree
        }

        let full: PathBuf = dir_path.join(name);
        match pending.node {
            Some(child) => {
                let idx: u32 = self.tree.strings.insert(name.to_string_lossy().as_ref());
                let same_parent: bool = pending
                    .parent
                    .upgrade()
                    .is_some_and(|p: Arc<Node>| Arc::ptr_eq(&p, parent));
                if same_parent
                    && self
                        .tree
                        .attach_child_node(parent, idx, child.clone(), pending.counts)
                {
                    // in-place rename: subtree and watches survive intact
                    debug!(target: "WATCH_MV", "renamed to {} (cookie {cookie})", full.display());
                    self.tree.add_event(
                        TreeEvent::new("Renamed").path(full.to_string_lossy().as_ref()),
                    );
                    return true;
                }

                debug!(target: "WATCH_MV", "moved to {} (cookie {cookie})", full.display());
                if child.node_t.is_dir() {
                    drop(child); // release the old subtree before re-scanning
                    self.tree.insert(&full, NodeType::Directory, None);
                    self.state.num_d.inc1();
                    if let Some(new_node) =
                        self.tree.get_node(full.to_string_lossy().as_ref())
                    {
                        self.add_watch(&new_node);
                        self.tree.populate_par(&full, &self.state, Some(true));
                        self.watch_subtree(&new_node);
                    }
                    self.sweep_dead_watches();
                } else {
                    // a moved file is rebuilt from its known inode: no stat
                    let depth: u8 = full
                        .components()
                        .count()
                        .saturating_sub(1)
                        .min(u8::MAX as usize) as u8;
                    let ino: u64 = child.inode().unwrap_or(0);
                    drop(child);
                    let (_, created) =
                        self.tree.insert_child(parent, idx, NodeType::File, ino, depth);
                    if created {
                        self.state.num_f.inc1();
                    }
                }
                true
            }
            None => {
                // Name-mode entry: nothing to re-link, record the new name
                let idx: u32 = self.tree.strings.insert(name.to_string_lossy().as_ref());
                if parent
                    .as_dir()
                    .is_some_and(|d| d.add_name_child(idx))
                {
                    self.tree.conf.files_mod(1);
                    self.state.num_f.inc1();
                }
                true
            }
        }
    }
}
