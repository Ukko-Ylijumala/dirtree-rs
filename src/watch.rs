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
- Renames arrive as `IN_MOVED_FROM` + `IN_MOVED_TO` and are handled as
  remove + re-scan; move cookies are not correlated yet.
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
use super::node::{Node, NodeType};
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
                continue; // timeout: re-check the quit flag
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
        }
        debug!(target: "WATCH", "event loop exiting");
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
            self.handle_event(ev.wd, ev.mask, OsStr::from_bytes(&name_bytes[..end]));
            off += EVENT_HDR + name_len;
        }
    }

    /// Apply a single inotify event to the tree.
    fn handle_event(&self, wd: i32, mask: u32, name: &OsStr) {
        trace!(target: "WATCH_EVENT", "wd {wd} mask {mask:#x} name {name:?}");

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
        } else if mask & (libc::IN_DELETE | libc::IN_MOVED_FROM) != 0 {
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
            if is_dir {
                // moved-away directories keep their kernel watches alive;
                // deleted ones get IN_IGNORED, but sweeping covers both
                self.sweep_dead_watches();
            }
        }
    }
}
