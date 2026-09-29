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

Policy matches the watcher: [`Filters`](crate::filters::Filters) are
honored (a filtered-out entry is treated as absent from disk, so tree
entries admitted by earlier, laxer filters get removed), the
[`Visitor`](super::Visitor) protocol is not consulted for the diff
itself but runs for the full scans of newly appeared subtrees. In Name
filemode files carry no inode, so file replacement is undetectable
there; adds and removes still work.
*/

use super::dirtree::{DirTree, MAX_RECURSE_DEPTH};
use super::event::{TreeEvent, TreeOp};
use super::hash::DirTreeXxh3Hasher;
use super::node::{DirTreeHashMap, MaybeNode, Node, NodeType};
use crate::ScanState;

use dirhandle::{DirHandle, EntryExt};
use timesince::SecondsSinceEpoch;
use tracing::{debug, instrument, trace};

use std::{
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
Timestamps within this many seconds of the last-scan baseline fall
through to a full diff, guarding against filesystem timestamp
granularity and same-second races (mirrors dirhandle's slack).
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
    #[instrument(level = "debug", skip(self, state))]
    pub fn update(
        &self,
        path: &str,
        state: &ScanState,
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
        // canonical path from the trie (normalizes e.g. trailing slashes)
        let full: PathBuf = node.path(&self.strings);
        let recursive: bool = recursive.unwrap_or(self.conf.recursive());
        let ctr: UpdateCtr = UpdateCtr::default();
        rayon::scope(|s| self.update_inner(&full, node, state, recursive, &ctr, s, 0));
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
        state: &'env ScanState,
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
        Fast path: a directory's own mtime changes exactly when its entry
        list changes (create / remove / rename), so if both mtime and
        ctime are clearly older than the node's last scan time, the
        entry list cannot have changed - skip the listing and diff on a
        single stat and only descend into subdirectories (their own
        timestamps decide for themselves). Backdating mtime bumps ctime,
        so it cannot fake "unchanged"; only nodes whose baseline was
        refreshed by an earlier full diff qualify.
        */
        if let Some(when) = node.scanned_at()
            && when > 0
            && let Ok(meta) = metadata(path)
        {
            let newest: u64 = meta.mtime().max(meta.ctime()).max(0) as u64;
            if newest + MTIME_SLACK_SECS < when {
                trace!(target: "UPDATE_SKIP", "{} unchanged since {when}", path.display());
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
                                &child_p, child, state, recursive, ctr, rs, frames + 1,
                            );
                        } else {
                            rs.spawn(move |s| {
                                self.update_inner(&child_p, child, state, recursive, ctr, s, 0)
                            });
                        }
                    }
                }
                return;
            }
        }

        // capture the pass time BEFORE listing, so a change racing the
        // diff makes the next pre-check fall through to a full diff again
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
        // children of this directory sit one path component deeper
        let child_depth: u8 = path.components().count().min(u8::MAX as usize) as u8;

        /*
        Disk view: interned name -> dirent. Filtered entries are treated
        as absent, and entry types the tree does not model (symlinks,
        sockets, ...) are ignored like everywhere else. An entry whose
        type cannot be determined at all (DT_UNKNOWN and a failed
        fstatat) is neither: it exists, so whatever the tree has under
        that name is kept as-is.
        */
        let mut iter = handle.iter();
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
        let mut disk: DirTreeHashMap<u32, &EntryExt> =
            DirTreeHashMap::with_capacity_and_hasher(entries.len(), DirTreeXxh3Hasher);
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
            disk.insert(self.strings.insert(name_os.to_string_lossy().as_ref()), e);
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
                            &node, name_idx, disk_ino, child_depth, &child_p, state, recursive,
                            ctr,
                        );
                    } else {
                        self.update_add_file(&node, name_idx, disk_ino, child_depth, state, ctr);
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
                            &node, name_idx, disk_ino, child_depth, &child_p, state, recursive,
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
                                &node, name_idx, disk_ino, child_depth, &child_p, state,
                                recursive, ctr,
                            );
                        } else {
                            self.update_add_file(
                                &node, name_idx, disk_ino, child_depth, state, ctr,
                            );
                        }
                    } else if is_dir && recursive {
                        // unchanged directory: recurse the diff into it
                        let child_p: PathBuf = path.join(name_os);
                        let child: Arc<Node> = existing.clone();
                        if frames < MAX_RECURSE_DEPTH {
                            self.update_inner(
                                &child_p, child, state, recursive, ctr, rs, frames + 1,
                            );
                        } else {
                            rs.spawn(move |s| {
                                self.update_inner(&child_p, child, state, recursive, ctr, s, 0)
                            });
                        }
                    }
                }
            }
        }

        // a completed full diff establishes a fresh pre-check baseline
        node.set_scanned(pass_time);
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
        state: &ScanState,
        recursive: bool,
        ctr: &UpdateCtr,
    ) {
        trace!(target: "UPDATE_ADD", "dir {}", child_p.display());
        let (child, created) =
            self.insert_child(parent, name_idx, NodeType::Directory, inode, depth);
        if created {
            ctr.added_dirs.fetch_add(1, Relaxed);
            state.num_d.inc1();
        }
        if recursive && child.is_some() {
            // a whole new subtree: full scan instead of a diff
            self.populate_par(child_p, state, Some(true));
        }
    }

    /// Insert a file that appeared on disk, honoring the tree's filemode.
    fn update_add_file(
        &self,
        parent: &Arc<Node>,
        name_idx: u32,
        inode: u64,
        depth: u8,
        state: &ScanState,
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
                state.num_f.inc1();
            }
        } else if mode.is_node() {
            let (_, created) = self.insert_child(parent, name_idx, NodeType::File, inode, depth);
            if created {
                ctr.added_files.fetch_add(1, Relaxed);
                state.num_f.inc1();
            }
        }
    }
}
