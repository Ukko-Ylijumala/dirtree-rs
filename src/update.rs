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
use super::event::{FaultKind, TreeEvent, TreeOp};
use super::hash::DirTreeXxh3Hasher;
use super::conf::NodeCounts;
use super::dirtree::NewChild;
use super::node::{Child, DirTreeHashMap, Directory, FileEntry, FileKind, NodeRef, ctime_stamp};
use super::osname::{decode_os, encode_os};

use dirhandle::{DirHandle, EntryExt};
use timesince::SecondsSinceEpoch;
use tracing::{debug, instrument, trace};

use std::{
    borrow::Cow,
    ffi::OsStr,
    fmt::{self, Display, Formatter},
    fs::metadata,
    io::{Error, ErrorKind},
    os::fd::{AsRawFd, BorrowedFd},
    os::unix::ffi::OsStrExt,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
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
    /// Special files added (see [FileKind]).
    pub added_specials: u32,
    /// Directory nodes removed (including subtree contents).
    pub removed_dirs: u32,
    /// File nodes / name entries removed (including subtree contents).
    pub removed_files: u32,
    /// Special file nodes removed (including subtree contents).
    pub removed_specials: u32,
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
            || self.added_specials != 0
            || self.removed_dirs != 0
            || self.removed_files != 0
            || self.removed_specials != 0
            || self.replaced != 0
    }
}

impl Display for UpdateStats {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "+{}d/+{}f/+{}s, -{}d/-{}f/-{}s, ~{} replaced ({} dirs diffed, {} skipped, {} errors)",
            self.added_dirs,
            self.added_files,
            self.added_specials,
            self.removed_dirs,
            self.removed_files,
            self.removed_specials,
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
    added_specials: AtomicU32,
    removed_dirs: AtomicU32,
    removed_files: AtomicU32,
    removed_specials: AtomicU32,
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
            added_specials: self.added_specials.load(Relaxed),
            removed_dirs: self.removed_dirs.load(Relaxed),
            removed_files: self.removed_files.load(Relaxed),
            removed_specials: self.removed_specials.load(Relaxed),
            replaced: self.replaced.load(Relaxed),
            errors: self.errors.load(Relaxed),
        }
    }

    /// Count what a removal took off the tree.
    fn removed(&self, c: NodeCounts) {
        self.removed_dirs.fetch_add(c.dirs, Relaxed);
        self.removed_files.fetch_add(c.files, Relaxed);
        self.removed_specials.fetch_add(c.specials, Relaxed);
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
        let node: Arc<Directory> = match self.get_node(path) {
            Some(NodeRef::Dir(dir)) => dir,
            Some(NodeRef::File { .. }) => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Not a directory: {path}"),
                ));
            }
            None => return Err(Error::new(ErrorKind::NotFound, format!("Not in tree: {path}"))),
        };
        /*
        The trie root and the intermediate nodes above a walk root (inode
        0, created without a stat) were never listed: their children are
        only the path to the walk root. Diffing one would treat all of
        its other entries on disk as new and fully scan them (all of `/`
        for the trie root).
        */
        if node.inode() == 0 {
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
        node: Arc<Directory>,
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
        let stamp: u64 = node.scan_stamp();
        if stamp != 0
            && let Ok(meta) = metadata(path)
            && ctime_stamp(meta.ctime(), meta.ctime_nsec()) == stamp
        {
            trace!(target: "UPDATE_SKIP", "{} unchanged since {stamp}", path.display());
            ctr.skipped_dirs.fetch_add(1, Relaxed);
            if recursive {
                let subdirs: Vec<(u32, Arc<Directory>)> = node
                    .children()
                    .read()
                    .iter()
                    .filter_map(|(k, v)| v.as_dir().map(|d: &Arc<Directory>| (*k, d.clone())))
                    .collect();
                for (name_idx, child) in subdirs {
                    let child_p: PathBuf = path.join(decode_os(self.get_string(name_idx)));
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
                {
                    let child: Child = Child::Dir(node.clone());
                    ctr.removed(self.remove_child_node(&parent, node.name_idx(), &child));
                    return;
                }
                ctr.errors.fetch_add(1, Relaxed);
                self.add_error(
                    TreeEvent::error(FaultKind::OpenDir, &e.to_string())
                        .path(&encode_os(path))
                        .io(&e)
                        .op(&op),
                );
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
        as absent. An entry whose type cannot be determined at all
        (DT_UNKNOWN and a failed fstatat) is neither: it exists, so
        whatever the tree has under that name is kept as-is.

        The diff keeps its own stamp (above), so the handle's own state
        tracking would be wasted work: iterate untracked.
        */
        // for readlinkat() on new symlinks; the handle outlives the entries
        let dirfd: BorrowedFd<'_> = unsafe { BorrowedFd::borrow_raw(handle.as_raw_fd()) };
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
            self.add_error(
                TreeEvent::error(FaultKind::ReadDir, &format!("readdir failed, listing incomplete: {e}"))
                    .path(&encode_os(path))
                    .errno(e as i32)
                    .op(&op),
            );
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
                if let Some(idx) = self.strings.idx(encode_os(name_os).as_ref()) {
                    unknown.push(idx);
                }
                continue;
            }
            let is_dir: bool = e.is_dir();
            if !is_dir && e.file_type().and_then(FileKind::from_type).is_none() {
                continue;
            }
            if !self.conf.filters().passes(name_os, is_dir) {
                continue;
            }
            names.push(encode_os(name_os));
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

        // tree view snapshot (file copies and directory Arc clones, one read lock hold)
        let tree_view: DirTreeHashMap<u32, Child> = node.children().read().clone();

        // pass 1: remove what is no longer on disk
        for (name_idx, child) in &tree_view {
            if disk.contains_key(name_idx) || unknown.contains(name_idx) {
                continue;
            }
            trace!(target: "UPDATE_RM", "{:?} in {}", name_idx, path.display());
            ctr.removed(self.remove_child_node(&node, *name_idx, child));
        }

        // pass 2: add what is new, replace what changed, recurse into the rest
        for (name_idx, entry) in disk {
            let is_dir: bool = entry.is_dir();
            let disk_ino: u64 = entry.ino();
            let name_os: &OsStr = OsStr::from_bytes(entry.name_as_bytes());
            // what a new or replaced non-directory is recorded as
            let leaf = || -> FileEntry {
                let kind: FileKind = entry.file_type().and_then(FileKind::from_type).unwrap_or_default();
                let target: Option<u32> = match kind {
                    FileKind::Symlink => self.link_target_at(dirfd, path, entry.file_name()),
                    _ => None,
                };
                FileEntry::new(disk_ino, kind, target)
            };

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
                        self.update_add_file(&node, name_idx, leaf(), child_depth, ctr);
                    }
                }

                Some(existing) => {
                    /*
                    A directory has no FileKind on either side. A kind change
                    under the same inode is a freed inode number reused at
                    once (a file deleted and a FIFO created, say): replaced.
                    */
                    let type_match: bool = existing.is_dir() == is_dir
                        && existing.file_kind() == entry.file_type().and_then(FileKind::from_type);
                    let tree_ino: u64 = existing.inode();
                    /*
                    Inode 0 marks an intermediate node created without a
                    stat - treat it as "unknown" rather than "changed".
                    */
                    let replaced: bool =
                        !type_match || (tree_ino != 0 && disk_ino != 0 && tree_ino != disk_ino);
                    if replaced {
                        trace!(target: "UPDATE_REPLACE", "{:?} in {}", name_os, path.display());
                        ctr.removed(self.remove_child_node(&node, name_idx, existing));
                        ctr.replaced.fetch_add(1, Relaxed);
                        if is_dir {
                            let child_p: PathBuf = path.join(name_os);
                            self.update_add_dir(
                                &node, name_idx, disk_ino, child_depth, &child_p, recursive, ctr,
                            );
                        } else {
                            self.update_add_file(&node, name_idx, leaf(), child_depth, ctr);
                        }
                    } else if let Child::Dir(child) = existing
                        && recursive
                    {
                        // unchanged directory: recurse the diff into it
                        let child_p: PathBuf = path.join(name_os);
                        let child: Arc<Directory> = child.clone();
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
        parent: &Arc<Directory>,
        name_idx: u32,
        inode: u64,
        depth: u8,
        child_p: &Path,
        recursive: bool,
        ctr: &UpdateCtr,
    ) {
        trace!(target: "UPDATE_ADD", "dir {}", child_p.display());
        let (child, created) = self.insert_child(parent, name_idx, NewChild::Dir(inode), depth);
        if created {
            ctr.added_dirs.fetch_add(1, Relaxed);
            self.conf.observer().dirs_added(1);
        }
        if recursive && child.is_dir() {
            // a whole new subtree: full scan instead of a diff
            self.populate_par(child_p, Some(true));
        }
    }

    /// Insert a file that appeared on disk, honoring the tree's filemode.
    fn update_add_file(
        &self,
        parent: &Arc<Directory>,
        name_idx: u32,
        file: FileEntry,
        depth: u8,
        ctr: &UpdateCtr,
    ) {
        if self.filemode().is_node() {
            let (_, created) =
                self.insert_child(parent, name_idx, NewChild::File(file), depth);
            if created && file.kind().is_special() {
                ctr.added_specials.fetch_add(1, Relaxed);
                self.conf.observer().specials_added(1);
            } else if created {
                ctr.added_files.fetch_add(1, Relaxed);
                self.conf.observer().files_added(1, 0);
            }
        }
    }
}
