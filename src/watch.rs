// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
Inotify-based tree following for resident mode (Linux-only).

[`TreeWatcher`] pins one inotify watch per directory node of a [`DirTree`]
and applies filesystem events to the tree as they happen: created entries
are inserted (new directories are watched first, then scanned recursively),
deleted or moved-away entries are removed along with their subtrees. The
watch map keys kernel watch descriptors to `Weak<Directory>`, so a removed
subtree self-heals: an event for a dead node drops the stale watch on
sight instead of requiring bookkeeping at removal time.

v1 caveats (deliberate, recorded for the future crate split):
- The watcher thread is the only continuous mutator of a resident tree
  (the worker thread idles). Node-level insertion is atomic, but no
  cross-operation ordering with worker ops is guaranteed.
- [`Filters`](super::Filters) are honored for new entries. A
  configured [`Visitor`](super::Visitor) runs for new-directory subtree
  scans (they go through the normal parallel walker) but is not
  consulted for single-file events.
- Renames are correlated via move cookies: a rename within one directory
  re-attaches the detached subtree in place (node identity, contents and
  kernel watches survive - no rescan). A file is re-attached the same way
  in any directory (it has no parent pointer); a directory moved to
  another one is rebuilt there from memory (the trie cannot re-parent a
  directory node), with its inodes, stamps and handles
  ([`DirTree::graft_subtree`]), without disk reads. With a visitor, a moved directory is re-scanned instead, as
  the visitor's verdicts depend on its ancestors. A
  `MOVED_FROM` with no matching `MOVED_TO` within [`PENDING_MOVE_TTL`]
  is a move out of the tree and drops the subtree. Events arriving for
  a subtree between its `MOVED_FROM` and `MOVED_TO` mark the move dirty,
  and the subtree is diffed once it is back in the tree.
- A kernel queue overflow (`IN_Q_OVERFLOW`) marks the tree
  [`TreeState::Inconsistent`], repairs it with a full
  [diff-rescan](DirTree::update) (which fixes both lost creations and
  lost deletions), re-establishes watches and restores
  [`TreeState::Ready`]; only a failed resync leaves the tree flagged.
- A watcher [prepared](TreeWatcher::prepare) before the tree's walk sees
  every change; one started on a walked tree misses the changes made
  between the walk and its start.
*/

use super::dirtree::DirTree;
use super::conf::ListHook;
use super::error::TreeError;
use super::event::{FaultKind, TreeEvent, TreeOp, TreeState};
use super::conf::NodeCounts;
use super::dirtree::NewChild;
use super::node::{Child, Directory, FileEntry, FileKind, NodeView};
use super::osname::encode_os;
use super::traverse::traverse_from;

use dashmap::DashMap;
use parking_lot::Mutex;
use tracing::{debug, error, trace, warn};

use std::{
    ffi::{CString, OsStr},
    fs::symlink_metadata,
    io,
    mem::size_of,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::ffi::OsStrExt,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
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
    /// The detached child.
    node: Child,
    /// Counts captured at detach time.
    counts: NodeCounts,
    /// The parent the child was detached from.
    parent: Weak<Directory>,
    /// When the `IN_MOVED_FROM` was seen (for expiry).
    seen: Instant,
    is_dir: bool,
    /// Events arrived for the detached subtree: diff it once it is back in the tree.
    dirty: bool,
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
    /// the tree's root path, checked to be set by `start()`
    root: PathBuf,
    ino_fd: OwnedFd,
    /// watch descriptor -> the watched directory's node
    watches: DashMap<i32, Weak<Directory>>,
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
    [`TreeWatcher::prepare`] and [`TreeWatcher::run`] in one, for a tree
    that is already walked. Changes made between its walk and this call
    are not seen; prepare the watcher before the walk to see them.
    */
    pub fn start(tree: Arc<DirTree>) -> io::Result<Arc<Self>> {
        let watcher: Arc<Self> = Self::prepare(tree)?;
        watcher.run()?;
        Ok(watcher)
    }

    /**
    Create an inotify instance and watch every directory currently in the
    tree (from the tree's root path downward), without processing events
    yet: the kernel queues them until [`TreeWatcher::run`].

    From here on every directory the tree scans is watched right before
    it is read. Prepared before the tree's walk, the watcher so misses
    nothing: an entry the walk does not see produces an event, and an
    event for one it did see changes nothing. Should the queue overflow
    during a long walk, `run()` resyncs the tree.

    Watch registration failures (e.g. `fs.inotify.max_user_watches`
    exhaustion) do not fail it; they are counted in
    [`TreeWatcher::failed_watches`] and recorded in the tree's event log.
    Progress goes to the tree's [`TreeObserver`](super::TreeObserver).
    */
    pub fn prepare(tree: Arc<DirTree>) -> io::Result<Arc<Self>> {
        let fd: RawFd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let from: PathBuf = tree
            .from()
            .map_err(|e: TreeError| io::Error::new(io::ErrorKind::InvalidInput, e))?
            .clone();
        let from_node: Arc<Directory> = tree
            .get_dir(encode_os(&from).as_ref())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Tree root node not found")
            })?;

        let watcher: Arc<Self> = Arc::new(Self {
            tree,
            root: from.clone(),
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

        // every scan from here on watches each directory before reading it
        let weak: Weak<Self> = Arc::downgrade(&watcher);
        watcher.tree.conf.set_list_hook(Some(ListHook(Arc::new(move |node: &Arc<Directory>| {
            if let Some(w) = weak.upgrade() {
                w.add_watch(node);
            }
        }))));
        Ok(watcher)
    }

    /**
    Start the event loop thread of a [prepared](TreeWatcher::prepare)
    watcher; it first applies the events queued since then. Fails if it
    is running already.
    */
    pub fn run(self: &Arc<Self>) -> io::Result<()> {
        let mut slot = self.thread.lock();
        if slot.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the watcher is running already",
            ));
        }
        let w: Arc<Self> = self.clone();
        let spawned = thread::Builder::new()
            .stack_size(1024 * 1024) // the event loop can run subtree scans
            .name("tree_watch".into())
            .spawn(move || w.event_loop());
        match spawned {
            Ok(thread) => {
                *slot = Some(thread);
                Ok(())
            }
            Err(e) => {
                self.tree.conf.set_list_hook(None);
                Err(e)
            }
        }
    }

    /// Signal the event loop to exit, wait for the thread to finish, and stop watching new directories.
    pub fn stop(&self) {
        self.quit.store(true, Relaxed);
        if let Some(t) = self.thread.lock().take() {
            t.join().ok();
        }
        self.tree.conf.set_list_hook(None);
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
    fn add_watch(&self, node: &Arc<Directory>) {
        let path: PathBuf = node.path(self.tree.strings());
        let cpath: CString = match CString::new(path.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return, // interior NUL cannot happen for real paths
        };
        let wd: i32 =
            unsafe { libc::inotify_add_watch(self.ino_fd.as_raw_fd(), cpath.as_ptr(), DIR_MASK) };
        if wd < 0 {
            let e: io::Error = io::Error::last_os_error();
            let ev: TreeEvent = TreeEvent::error(FaultKind::Watch, &format!("inotify watch failed: {e}"))
                .path(encode_os(&path).as_ref())
                .io(&e);
            if self.failed.fetch_add(1, Relaxed) == 0 {
                // log the first failure loudly; the rest are only counted and reported
                warn!("inotify watch failed for {}: {e} (see fs.inotify.max_user_watches)",
                    path.display());
                self.tree.add_error(ev);
            } else {
                self.tree.add_fault(&ev);
            }
            return;
        }
        trace!(target: "WATCH_ADD", "wd {wd} -> {}", path.display());
        self.watches.insert(wd, Arc::downgrade(node));
    }

    /// Watch a directory node and every directory below it.
    fn watch_subtree(&self, node: &Arc<Directory>) {
        traverse_from(node, &mut |n: NodeView<'_>| {
            if let NodeView::Dir(dir) = n {
                self.add_watch(dir);
            }
        });
    }

    /// Drop watches whose nodes have been removed from the tree.
    fn sweep_dead_watches(&self) {
        let fd: RawFd = self.ino_fd.as_raw_fd();
        self.watches.retain(|wd: &i32, w: &mut Weak<Directory>| {
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
            if let Child::Dir(ref node) = p.node {
                self.tree.release_handles(node);
            }
            sweep |= p.is_dir;
            false // dropping the Arc cascades the subtree teardown
        });
        if sweep {
            self.sweep_dead_watches();
        }
    }

    /// The cookie of the pending move whose detached subtree holds the
    /// node (as its root or below it), if any.
    fn pending_cookie(&self, node: &Arc<Directory>) -> Option<u32> {
        if self.pending.is_empty() {
            return None;
        }
        let roots: Vec<(u32, Arc<Directory>)> = self
            .pending
            .iter()
            .filter_map(|p| p.value().node.as_dir().map(|d: &Arc<Directory>| (*p.key(), d.clone())))
            .collect();
        let mut current: Arc<Directory> = node.clone();
        loop {
            if let Some((cookie, _)) = roots.iter().find(|(_, r)| Arc::ptr_eq(r, &current)) {
                return Some(*cookie);
            }
            current = current.parent()?;
        }
    }

    /**
    Diff the subtree at `path` against the disk, for a moved subtree that
    had events while detached. New directories found are watched as the
    diff scans them; watches of removed ones are dropped.
    */
    fn resync_subtree(&self, path: &Path) {
        let path_str = encode_os(&path);
        match self.tree.update(path_str.as_ref(), Some(true)) {
            Ok(stats) => {
                debug!(target: "WATCH_MV", "{} resynced: {stats}", path.display());
                self.sweep_dead_watches();
            }
            Err(e) => self.tree.add_error(
                TreeEvent::error(FaultKind::WatchLost, &format!("Moved subtree resync failed: {e}"))
                    .path(path_str.as_ref())
                    .op(&TreeOp::Update(path.to_path_buf())),
            ),
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
            let ev: TreeEvent = TreeEvent::error(FaultKind::WatchLost, "inotify queue overflow, resyncing tree")
                .path(encode_os(&self.root).as_ref());
            error!("{ev:?}");
            self.tree.add_error(ev.clone());
            self.tree.set_state(TreeState::Inconsistent(ev));

            let from_str = encode_os(&self.root);
            match self.tree.update(from_str.as_ref(), Some(true)) {
                Ok(stats) => {
                    self.sweep_dead_watches();
                    if let Some(root) = self.tree.get_dir(from_str.as_ref()) {
                        self.watch_subtree(&root);
                    }
                    let msg: String = format!("Tree resynced after overflow: {stats}");
                    warn!("{msg}");
                    self.tree.add_event(TreeEvent::new(&msg));
                    self.tree.set_state(TreeState::Ready);
                }
                Err(e) => {
                    // stays Inconsistent - the consumer must intervene
                    self.tree.add_error(
                        TreeEvent::error(FaultKind::WatchLost, &format!("Overflow resync failed: {e}"))
                            .path(from_str.as_ref())
                            .op(&TreeOp::Update(self.root.clone())),
                    );
                }
            }
            return;
        }
        if mask & libc::IN_IGNORED != 0 {
            // the kernel removed this watch (dir deleted or unmounted)
            self.watches.remove(&wd);
            return;
        }

        let node: Arc<Directory> = match self.watches.get(&wd).and_then(|w| w.value().upgrade()) {
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
            .get_dir(encode_os(&dir_path).as_ref())
            .is_some_and(|n: Arc<Directory>| Arc::ptr_eq(&n, &node));
        if !attached {
            if let Some(cookie) = self.pending_cookie(&node) {
                /*
                Part of an in-flight move: the subtree is detached, so the
                event cannot be applied now. Mark the move dirty instead -
                the subtree is diffed once it is back in the tree - and keep
                the watch, which stays valid across the move.
                */
                if let Some(mut pending) = self.pending.get_mut(&cookie) {
                    pending.dirty = true;
                }
                return;
            }
            unsafe { libc::inotify_rm_watch(self.ino_fd.as_raw_fd(), wd) };
            self.watches.remove(&wd);
            return;
        }

        self.events_seen.fetch_add(1, Relaxed);

        if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
            if dir_path == self.root {
                let ev: TreeEvent = TreeEvent::error(FaultKind::WatchLost, "Tree root was removed or moved")
                    .path(encode_os(&dir_path).as_ref());
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
                self.tree.insert_dir(&full, None);
                self.tree.conf.observer().dirs_added(1);
                /*
                Watch-then-list, per directory: the scan runs the watcher's
                ListHook on each directory it reaches, the new one included,
                before reading it. An entry created before a directory's
                watch is in its listing, one created after it produces an
                event. (Watching the subtree only after the whole scan lost
                entries created in a subdirectory between its listing and
                its watch.)
                */
                self.tree.populate_par(&full, Some(true));
            } else {
                /*
                IN_CREATE fires for every entry type. Like the walker and the
                diff, record each with its FileKind, special files included.
                lstat, so a symlink is not followed - that also yields the
                inode, and the insert needs no stat of its own.
                */
                let (ino, kind): (u64, FileKind) = match symlink_metadata(&full) {
                    Ok(meta) => match FileKind::from_std(meta.file_type()) {
                        Some(kind) => (meta.ino(), kind),
                        None => return,
                    },
                    Err(_) => return, // already gone again
                };
                debug!(target: "WATCH_CREATE", "{}", full.display());
                let idx: u32 = self.tree.strings.insert(encode_os(&name).as_ref());
                let depth: u8 = full
                    .components()
                    .count()
                    .saturating_sub(1)
                    .min(u8::MAX as usize) as u8;
                let counted: bool = if self.tree.filemode().is_node() {
                    let target: Option<u32> = match kind {
                        FileKind::Symlink => self.tree.link_target(&full),
                        _ => None,
                    };
                    let file: FileEntry = FileEntry::new(ino, kind, target);
                    self.tree.insert_child(&node, idx, NewChild::File(file), depth).1
                } else {
                    true // counted, but not stored
                };
                match counted && kind.is_special() {
                    true => self.tree.conf.observer().specials_added(1),
                    false if counted => self.tree.conf.observer().files_added(1, 0),
                    false => {}
                }
            }
        } else if mask & libc::IN_DELETE != 0 {
            debug!(target: "WATCH_RM", "{}", full.display());
            let op: TreeOp = TreeOp::Remove(encode_os(&full).to_string());
            match self.tree.remove(encode_os(&full).as_ref()) {
                Ok(Some(counts)) => {
                    let msg: String = format!("Removed: {counts}");
                    self.tree.add_event(
                        TreeEvent::new(&msg)
                            .path(encode_os(&full).as_ref())
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
    fn on_moved_from(&self, parent: &Arc<Directory>, name: &OsStr, cookie: u32, is_dir: bool) {
        // lookup only: a name we never interned cannot be in the tree
        let Some(idx) = self.tree.strings.idx(encode_os(&name).as_ref()) else {
            return;
        };
        let Some(child) = parent.get_child(&idx) else {
            return; // not in the tree (filtered out or never scanned)
        };

        // detach only: an in-place rename re-attaches it, handles and all
        let counts: NodeCounts = self.tree.detach_child_node(parent, idx, &child);
        if counts.is_empty() {
            /*
            Nothing was detached: the slot changed under us (a concurrent
            remove or re-create). Parking the stale child would re-attach
            it at the destination; without a pending entry the matching
            IN_MOVED_TO takes the plain create path instead.
            */
            return;
        }
        debug!(target: "WATCH_MV_FROM", "{name:?} detached (cookie {cookie})");
        self.pending.insert(
            cookie,
            PendingMove {
                node: child,
                counts,
                parent: Arc::downgrade(parent),
                seen: Instant::now(),
                is_dir,
                dirty: false,
            },
        );
    }

    /**
    Handle `IN_MOVED_TO` for a cookie with a pending `IN_MOVED_FROM`.
    A rename within one directory re-attaches the detached subtree in
    place (node identity, contents and kernel watches all survive), and
    so does a file's move to any directory. A directory moved to another
    directory is rebuilt there from memory, or re-scanned with a visitor
    (the trie has no re-parenting). Returns `false` when the cookie is
    unknown - the caller then treats the event as a plain create (a move
    into the tree from outside).
    */
    fn on_moved_to(
        &self,
        parent: &Arc<Directory>,
        dir_path: &Path,
        name: &OsStr,
        cookie: u32,
        is_dir: bool,
    ) -> bool {
        let Some((_, pending)) = self.pending.remove(&cookie) else {
            return false;
        };
        // the destination name may be excluded even though the source was tracked
        if !self.tree.conf.filters().passes(name, is_dir) {
            if let Child::Dir(ref node) = pending.node {
                self.tree.release_handles(node);
            }
            let was_dir: bool = pending.is_dir;
            // drop the subtree first, or the sweep still sees its watches alive
            drop(pending);
            if was_dir {
                self.sweep_dead_watches();
            }
            return true; // handled: the entry ceases to exist for the tree
        }

        let full: PathBuf = dir_path.join(name);
        let dirty: bool = pending.dirty;
        let child: Child = pending.node;
        let idx: u32 = self.tree.strings.insert(encode_os(&name).as_ref());
        let same_parent: bool = pending
            .parent
            .upgrade()
            .is_some_and(|p: Arc<Directory>| Arc::ptr_eq(&p, parent));
        // a file has no parent pointer: it is re-attached wherever it went
        if (same_parent || !child.is_dir())
            && self
                .tree
                .attach_child_node(parent, idx, child.clone(), pending.counts)
        {
            // in-place rename: subtree and watches survive intact
            debug!(target: "WATCH_MV", "renamed to {} (cookie {cookie})", full.display());
            self.tree.add_event(
                TreeEvent::new("Renamed").path(encode_os(&full).as_ref()),
            );
            if dirty {
                self.resync_subtree(&full);
            }
            return true;
        }
        // (attaching a file cannot fail)
        let Child::Dir(child) = child else {
            return true;
        };

        debug!(target: "WATCH_MV", "moved to {} (cookie {cookie})", full.display());
        let depth: u8 = full
            .components()
            .count()
            .saturating_sub(1)
            .min(u8::MAX as usize) as u8;
        if self.tree.has_visitor() {
            /*
            A visitor's verdicts depend on a directory's ancestors
            (scopes, depth caps, path-aware prunes), so a subtree in
            a new place is walked again for the visitor to see it.
            */
            self.tree.release_handles(&child);
            drop(child); // release the old subtree before re-scanning
            self.tree.insert_dir(&full, None);
            self.tree.conf.observer().dirs_added(1);
            // watched directory by directory as the scan reaches them, see WATCH_MKDIR
            self.tree.populate_par(&full, Some(true));
            self.sweep_dead_watches();
        } else {
            /*
            Rebuilt from memory under the new parent, with no disk
            reads. The kernel watches follow the inodes, so
            re-adding them hands back the same descriptors, now
            mapped to the new nodes; the handles moved over too, so
            the old subtree is just dropped.
            */
            match self.tree.graft_subtree(parent, idx, &child, depth) {
                Some(new_root) => self.watch_subtree(&new_root),
                None => self.tree.add_error(
                    TreeEvent::error(FaultKind::Tree, "Moved directory has no place to go to")
                        .path(encode_os(&full).as_ref()),
                ),
            }
            drop(child);
            self.sweep_dead_watches();
            if dirty {
                self.resync_subtree(&full);
            }
        }
        true
    }
}

/* ===== test hooks ===== */

#[cfg(test)]
impl TreeWatcher {
    /// Apply one synthetic event, as the event loop would.
    pub(super) fn inject(&self, wd: i32, mask: u32, cookie: u32, name: &str) {
        self.handle_event(wd, mask, cookie, OsStr::new(name));
    }

    /// The watch descriptor of the directory at `path`, if it is watched.
    pub(super) fn wd_of(&self, path: &Path) -> Option<i32> {
        self.watches
            .iter()
            .find(|e| e.value().upgrade().is_some_and(|n| n.path(self.tree.strings()) == path))
            .map(|e| *e.key())
    }
}
