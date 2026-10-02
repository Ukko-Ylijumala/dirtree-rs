// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
Diff-rescan ("update") for [`DirTree`]: re-list a directory and apply
the differences to the tree - insert entries that appeared, remove
entries that vanished, replace entries whose inode changed, and
optionally recurse into unchanged subdirectories to diff them too.

This is the repair primitive behind [`TreeOp::Update`] and the inotify
queue-overflow recovery in [`TreeWatcher`](super::TreeWatcher). Unlike
a blind re-scan (which can only ever add nodes), the diff detects and
removes stale tree entries as well.

Policy matches the watcher: [`Filters`](super::Filters) are
honored (a filtered-out entry is treated as absent from disk, so tree
entries admitted by earlier, laxer filters get removed), the
[`Visitor`](super::Visitor) protocol is not consulted for the diff
itself but runs for the full scans of newly appeared subtrees. In Name
filemode files carry no inode, so file replacement is undetectable
there; adds and removes still work.
*/

use super::dirtree::{DirTree, ENTRY_BATCH_MIN, MAX_RECURSE_DEPTH};
use super::event::{TreeEvent, TreeOp};
use super::hash::DirTreeXxh3Hasher;
use super::node::{DirTreeHashMap, MaybeNode, Node, NodeType, ctime_stamp};

use dirhandle::{DirHandle, EntryExt};
use timesince::SecondsSinceEpoch;
use tracing::{debug, instrument, trace};

use std::{
    borrow::Cow,
    ffi::OsStr,
    fmt::{self, Display, Formatter},
    fs::metadata,
    io::{Error, ErrorKind},
    os::unix::ffi::OsStrExt,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::Arc,
    sync::atomic::{AtomicU32, Ordering::Relaxed},
};

/**
A directory's ctime must be this many seconds older than the start of a
full diff for that diff to establish a pre-check baseline. A change
landing within the stamp's own timestamp granule would not move it, so
a fresher stamp cannot prove "unchanged" later (mirrors dirhandle's
slack).
*/
const MTIME_SLACK_SECS: u64 = 2;

/// Counters describing what a [`DirTree::update`] pass changed.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateStats {
    /// Directories whose entry lists were diffed.
    pub scanned_dirs: u32,
    /// Directories skipped by the mtime/ctime pre-check (a single stat
    /// showed the entry list cannot have changed since the last scan).
    pub skipped_dirs: u32,
    pub added_dirs: u32,
    pub added_files: u32,
    /// Directory nodes removed (including subtree contents).
    pub removed_dirs: u32,
    /// File nodes / name entries removed (including subtree contents).
    pub removed_files: u32,
    /// Entries whose inode or type changed (counted once each; their
    /// old subtree contents are included in the removed counters).
    pub replaced: u32,
    pub errors: u32,
}

impl UpdateStats {
    /// Whether the pass changed the tree at all.
    pub fn changed(&self) -> bool {
        self.added_dirs != 0
            || self.added_files != 0
            || self.removed_dirs != 0
            || self.removed_files != 0
            || self.replaced != 0
    }
}

impl Display for UpdateStats {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "+{}d/+{}f, -{}d/-{}f, ~{} replaced ({} dirs diffed, {} skipped, {} errors)",
            self.added_dirs,
            self.added_files,
            self.removed_dirs,
            self.removed_files,
            self.replaced,
            self.scanned_dirs,
            self.skipped_dirs,
            self.errors
        )
    }
}

/// Internal atomic accumulator shared across the parallel diff.
#[derive(Default)]
struct UpdateCtr {
    scanned_dirs: AtomicU32,
    skipped_dirs: AtomicU32,
    added_dirs: AtomicU32,
    added_files: AtomicU32,
    removed_dirs: AtomicU32,
    removed_files: AtomicU32,
    replaced: AtomicU32,
    errors: AtomicU32,
}

impl UpdateCtr {
    fn snapshot(&self) -> UpdateStats {
        UpdateStats {
            scanned_dirs: self.scanned_dirs.load(Relaxed),
            skipped_dirs: self.skipped_dirs.load(Relaxed),
            added_dirs: self.added_dirs.load(Relaxed),
            added_files: self.added_files.load(Relaxed),
            removed_dirs: self.removed_dirs.load(Relaxed),
            removed_files: self.removed_files.load(Relaxed),
            replaced: self.replaced.load(Relaxed),
            errors: self.errors.load(Relaxed),
        }
    }
}

impl DirTree {
    /**
    Diff-rescan a directory that is already in the tree, applying any
    differences found on disk (see the module docs for the semantics).
    With `recursive` (or the tree default when [None]) the diff also
    descends into subdirectories that exist on both sides; newly
    appeared subdirectories are always scanned in full.

    Blocking; runs the per-directory diffs in parallel on the rayon
    pool. For the queued (non-blocking) form, see [DirTree::rescan].
    */
    #[instrument(level = "debug", skip(self))]
    pub fn update(
        &self,
        path: &str,
        recursive: Option<bool>,
    ) -> Result<UpdateStats, Error> {
        let node: Arc<Node> = self
            .get_node(path)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("Not in tree: {path}")))?;
        if !node.is_traversable() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Not a directory: {path}"),
            ));
        }
        /*
        The trie root and the intermediate nodes above a walk root (inode
        0, created without a stat) were never listed: their children are
        only the path to the walk root. Diffing one would treat all of
        its other entries on disk as new and fully scan them (all of `/`
        for the trie root).
        */
        if matches!(node.inode(), None | Some(0)) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Not a scanned directory (above the tree root?): {path}"),
            ));
        }
        // canonical path from the trie (normalizes e.g. trailing slashes)
        let full: PathBuf = node.path(&self.strings);
        let recursive: bool = recursive.unwrap_or(self.conf.recursive());
        let ctr: UpdateCtr = UpdateCtr::default();
        rayon::scope(|s| self.update_inner(&full, node, recursive, &ctr, s, 0));
        let stats: UpdateStats = ctr.snapshot();
        debug!(target: "UPDATE", "{}: {stats}", full.display());
        Ok(stats)
    }

    /// Diff one directory and recurse. `frames` bounds direct recursion
    /// before offloading to the rayon scope, like the parallel walker.
    #[allow(clippy::too_many_arguments)]
    fn update_inner<'env>(
        &'env self,
        path: &PathBuf,
        node: Arc<Node>,
        recursive: bool,
        ctr: &'env UpdateCtr,
        rs: &rayon::Scope<'env>,
        frames: usize,
    ) {
        if self.conf.is_cancelled() {
            return;
        }
        let op: TreeOp = TreeOp::Update(path.clone());

        /*
        Fast path: a directory's ctime moves whenever its entry list
        changes (create / remove / rename), so if it still equals the
        stamp recorded before the last full diff, the entry list cannot
        have changed - skip the listing and diff on a single stat and
        only descend into subdirectories (their own stamps decide for
        themselves). Stamps are compared with stamps only, never with
        the local clock, so clock skew against an NFS / SMB / FUSE
        server cannot hide a change. Only nodes whose baseline was set
        by an earlier, settled full diff qualify (see [MTIME_SLACK_SECS]).
        */
        if let Some(stamp) = node.scan_stamp()
            && stamp != 0
            && let Ok(meta) = metadata(path)
            && ctime_stamp(meta.ctime(), meta.ctime_nsec()) == stamp
        {
            trace!(target: "UPDATE_SKIP", "{} unchanged since {stamp}", path.display());
            ctr.skipped_dirs.fetch_add(1, Relaxed);
            if recursive && let Some(ch) = node.children() {
                let subdirs: Vec<(u32, Arc<Node>)> = ch
                    .read()
                    .iter()
                    .filter_map(|(k, v)| match v {
                        Some(n) if n.node_t.is_dir() => Some((*k, n.clone())),
                        _ => None,
                    })
                    .collect();
                for (name_idx, child) in subdirs {
                    let child_p: PathBuf = path.join(self.get_string(name_idx));
                    if frames < MAX_RECURSE_DEPTH {
                        self.update_inner(
                            &child_p, child, recursive, ctr, rs, frames + 1,
                        );
                    } else {
                        rs.spawn(move |s| {
                            self.update_inner(&child_p, child, recursive, ctr, s, 0)
                        });
                    }
                }
            }
            return;
        }

        // the pass start, for the settle check of the new baseline below
        let pass_time: u64 = *SecondsSinceEpoch::new();

        let mut handle: DirHandle = match DirHandle::new(path) {
            Ok(h) => h,
            Err(e) => {
                /*
                The directory vanished between its parent's diff and
                this open - detach it here since the parent pass has
                already moved on.
                */
                if e.kind() == ErrorKind::NotFound
                    && let Some(parent) = node.parent()
                    && let Some((name_idx, _)) = parent.get_child_byref(&node)
                {
                    let (_, dirs, files) =
                        self.remove_child_node(&parent, name_idx, node.clone());
                    ctr.removed_dirs.fetch_add(dirs, Relaxed);
                    ctr.removed_files.fetch_add(files, Relaxed);
                    return;
                }
                ctr.errors.fetch_add(1, Relaxed);
                self.add_error(TreeEvent::error(&e.to_string(), &op));
                return;
            }
        };
        ctr.scanned_dirs.fetch_add(1, Relaxed);
        /*
        Stamp the directory BEFORE listing it (fstat on the open handle,
        so it is the very directory being listed): a change racing the
        diff moves ctime past the stamp, and the next pre-check falls
        through to a full diff again.
        */
        let stamp: u64 = handle
            .stat()
            .map(|st: libc::stat| ctime_stamp(st.st_ctime, st.st_ctime_nsec))
            .unwrap_or(0);
        // children of this directory sit one path component deeper
        let child_depth: u8 = path.components().count().min(u8::MAX as usize) as u8;

        /*
        Disk view: interned name -> dirent. Filtered entries are treated
        as absent, and entry types the tree does not model (symlinks,
        sockets, ...) are ignored like everywhere else. An entry whose
        type cannot be determined at all (DT_UNKNOWN and a failed
        fstatat) is neither: it exists, so whatever the tree has under
        that name is kept as-is.

        The diff keeps its own stamp (above), so the handle's own state
        tracking would be wasted work: iterate untracked.
        */
        let mut iter = handle.iter_untracked();
        let entries: Vec<EntryExt> = iter.by_ref().collect();
        if let Some(e) = iter.error() {
            /*
            A readdir error cut the listing short. Diffing against it
            would remove every entry not read yet, and a refreshed
            baseline would then let the pre-check skip this directory
            for good. Leave the tree (and the baseline) as they are.
            */
            ctr.errors.fetch_add(1, Relaxed);
            self.add_error(TreeEvent::error(
                &format!("readdir failed, listing incomplete: {e}"),
                &op,
            ));
            return;
        }
        drop(iter);
        /*
        The passing names are interned after the loop, ENTRY_BATCH_MIN at
        a time: one insert_many() takes stringstore's writer mutex once per
        batch instead of once per new name (as the walker does). Batches
        stay moderate, as every reader and writer waiting on the mutex
        waits out the whole batch.
        */
        let mut names: Vec<Cow<str>> = Vec::with_capacity(entries.len());
        let mut passing: Vec<&EntryExt> = Vec::with_capacity(entries.len());
        let mut unknown: Vec<u32> = Vec::new();
        for e in &entries {
            let name_os: &OsStr = OsStr::from_bytes(e.name_as_bytes());
            if e.file_type().is_none() {
                // lookup only: a name the store lacks is not in the tree either
                if let Some(idx) = self.strings.idx(name_os.to_string_lossy().as_ref()) {
                    unknown.push(idx);
                }
                continue;
            }
            let is_dir: bool = e.is_dir();
            if !is_dir && !e.is_file() {
                continue;
            }
            if !self.conf.filters().passes(name_os, is_dir) {
                continue;
            }
            names.push(name_os.to_string_lossy());
            passing.push(e);
        }
        let mut disk: DirTreeHashMap<u32, &EntryExt> =
            DirTreeHashMap::with_capacity_and_hasher(passing.len(), DirTreeXxh3Hasher);
        for (batch, batch_entries) in names
            .chunks(ENTRY_BATCH_MIN)
            .zip(passing.chunks(ENTRY_BATCH_MIN))
        {
            let indices: Vec<u32> = self.strings.insert_many(batch);
            disk.extend(indices.into_iter().zip(batch_entries.iter().copied()));
        }

        // tree view snapshot (Arc clones only, one read lock hold)
        let tree_view: DirTreeHashMap<u32, MaybeNode> = match node.children() {
            Some(ch) => ch.read().clone(),
            None => return,
        };

        // pass 1: remove what is no longer on disk
        for (name_idx, child) in &tree_view {
            if disk.contains_key(name_idx) || unknown.contains(name_idx) {
                continue;
            }
            trace!(target: "UPDATE_RM", "{:?} in {}", name_idx, path.display());
            match child {
                Some(c) => {
                    let (_, dirs, files) = self.remove_child_node(&node, *name_idx, c.clone());
                    ctr.removed_dirs.fetch_add(dirs, Relaxed);
                    ctr.removed_files.fetch_add(files, Relaxed);
                }
                None => {
                    // a Name-mode (node-less) file entry
                    if node
                        .as_dir()
                        .is_some_and(|d| d.remove_name_child(name_idx))
                    {
                        self.conf.files_mod(-1);
                        ctr.removed_files.fetch_add(1, Relaxed);
                    }
                }
            }
        }

        // pass 2: add what is new, replace what changed, recurse into the rest
        for (name_idx, entry) in disk {
            let is_dir: bool = entry.is_dir();
            let disk_ino: u64 = entry.ino();
            let name_os: &OsStr = OsStr::from_bytes(entry.name_as_bytes());

            match tree_view.get(&name_idx) {
                // new on disk
                None => {
                    if is_dir {
                        let child_p: PathBuf = path.join(name_os);
                        self.update_add_dir(
                            &node, name_idx, disk_ino, child_depth, &child_p, recursive,
                            ctr,
                        );
                    } else {
                        self.update_add_file(&node, name_idx, disk_ino, child_depth, ctr);
                    }
                }

                // a name-only entry occupies the slot
                Some(None) => {
                    if is_dir {
                        // type change: a directory replaced a former file
                        if node
                            .as_dir()
                            .is_some_and(|d| d.remove_name_child(&name_idx))
                        {
                            self.conf.files_mod(-1);
                            ctr.removed_files.fetch_add(1, Relaxed);
                            ctr.replaced.fetch_add(1, Relaxed);
                        }
                        let child_p: PathBuf = path.join(name_os);
                        self.update_add_dir(
                            &node, name_idx, disk_ino, child_depth, &child_p, recursive,
                            ctr,
                        );
                    }
                    // a plain file behind a name entry has no inode to compare
                }

                Some(Some(existing)) => {
                    let type_match: bool = existing.node_t.is_dir() == is_dir;
                    let tree_ino: u64 = existing.inode().unwrap_or(0);
                    /*
                    Inode 0 marks an intermediate node created without a
                    stat - treat it as "unknown" rather than "changed".
                    */
                    let replaced: bool =
                        !type_match || (tree_ino != 0 && disk_ino != 0 && tree_ino != disk_ino);
                    if replaced {
                        trace!(target: "UPDATE_REPLACE", "{:?} in {}", name_os, path.display());
                        let (_, dirs, files) =
                            self.remove_child_node(&node, name_idx, existing.clone());
                        ctr.removed_dirs.fetch_add(dirs, Relaxed);
                        ctr.removed_files.fetch_add(files, Relaxed);
                        ctr.replaced.fetch_add(1, Relaxed);
                        if is_dir {
                            let child_p: PathBuf = path.join(name_os);
                            self.update_add_dir(
                                &node, name_idx, disk_ino, child_depth, &child_p, recursive, ctr,
                            );
                        } else {
                            self.update_add_file(
                                &node, name_idx, disk_ino, child_depth, ctr,
                            );
                        }
                    } else if is_dir && recursive {
                        // unchanged directory: recurse the diff into it
                        let child_p: PathBuf = path.join(name_os);
                        let child: Arc<Node> = existing.clone();
                        if frames < MAX_RECURSE_DEPTH {
                            self.update_inner(
                                &child_p, child, recursive, ctr, rs, frames + 1,
                            );
                        } else {
                            rs.spawn(move |s| {
                                self.update_inner(&child_p, child, recursive, ctr, s, 0)
                            });
                        }
                    }
                }
            }
        }

        /*
        A completed full diff establishes a fresh pre-check baseline -
        unless the stamp is too recent to be trusted (see
        [MTIME_SLACK_SECS]), which clears the baseline instead.
        */
        let settled: bool = stamp / 1_000_000_000 + MTIME_SLACK_SECS < pass_time;
        node.set_scan_stamp(if settled { stamp } else { 0 });
    }

    /// Insert a directory that appeared on disk and, when `recursive`,
    /// scan its contents in full (visitor- and resident-aware).
    #[allow(clippy::too_many_arguments)]
    fn update_add_dir(
        &self,
        parent: &Arc<Node>,
        name_idx: u32,
        inode: u64,
        depth: u8,
        child_p: &PathBuf,
        recursive: bool,
        ctr: &UpdateCtr,
    ) {
        trace!(target: "UPDATE_ADD", "dir {}", child_p.display());
        let (child, created) =
            self.insert_child(parent, name_idx, NodeType::Directory, inode, depth);
        if created {
            ctr.added_dirs.fetch_add(1, Relaxed);
            self.conf.observer().dirs_added(1);
        }
        if recursive && child.is_some() {
            // a whole new subtree: full scan instead of a diff
            self.populate_par(child_p, Some(true));
        }
    }

    /// Insert a file that appeared on disk, honoring the tree's filemode.
    fn update_add_file(
        &self,
        parent: &Arc<Node>,
        name_idx: u32,
        inode: u64,
        depth: u8,
        ctr: &UpdateCtr,
    ) {
        let mode = self.filemode();
        if mode.is_name() {
            if parent
                .as_dir()
                .is_some_and(|d| d.add_name_child(name_idx))
            {
                self.conf.files_mod(1);
                self.conf.depth_compare(depth);
                ctr.added_files.fetch_add(1, Relaxed);
                self.conf.observer().files_added(1, 0);
            }
        } else if mode.is_node() {
            let (_, created) = self.insert_child(parent, name_idx, NodeType::File, inode, depth);
            if created {
                ctr.added_files.fetch_add(1, Relaxed);
                self.conf.observer().files_added(1, 0);
            }
        }
    }
}
