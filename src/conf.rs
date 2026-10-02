// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::observer::{NOOP_OBSERVER, TreeObserver};
use super::visitor::{Visitor, WalkEvent};
use super::{FileMode, Filters};
use crate::utils::mod_atom_u32;
use crossbeam::channel::Sender;
use parking_lot::RwLock;
use std::{
    path::PathBuf,
    sync::Arc,
    sync::OnceLock,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::Relaxed},
};
use timesince::SecondsSinceEpoch;

/**
Atomic counters for tracking the number of nodes, directories, and files.
Using a separate counter struct allows us to not have to lock the entire
tree f.ex. when inserting or removing nodes. Also stores tree configuration.
*/
#[derive(Default, Debug)]
pub struct TreeConf {
    pub(super) from: OnceLock<PathBuf>,
    pub(super) filemode: FileMode,
    pub(super) filters: Filters,
    /**
    Optional [`Visitor`] used by the parallel walker to recognize
    subtrees, prune children, and bound depth. When set, the walker
    follows the visitor-aware path; otherwise the original fast path.
    */
    pub(super) visitor: RwLock<Option<Arc<dyn Visitor>>>,
    /// Optional channel for streaming [`WalkEvent`]s as the walk runs.
    pub(super) discovery_tx: RwLock<Option<Sender<WalkEvent>>>,
    /// Progress reporting; [`NoopObserver`](super::NoopObserver) until set.
    observer: OnceLock<Arc<dyn TreeObserver>>,
    /// Cooperative cancellation flag. Checked at the top of every `populate_par_inner`.
    pub(super) cancelled: AtomicBool,
    ctime: SecondsSinceEpoch,
    /// Does not include the root node.
    nodes: AtomicU32,
    dirs: AtomicU32,
    files: AtomicU32,
    /// Maximum depth of the tree. Root is at depth 0.
    depth: AtomicU8,
    errors: AtomicU32,
    recursive: AtomicBool,
    resident: AtomicBool,
    sync: AtomicBool,
}

impl TreeConf {
    pub(super) fn new(filemode: FileMode, filters: Filters) -> Self {
        Self {
            filemode,
            filters,
            ..Default::default()
        }
    }

    pub(super) fn filters(&self) -> &Filters {
        &self.filters
    }

    pub(super) fn from(&self) -> &PathBuf {
        self.from.get().expect("Tree must be initialized")
    }
    pub(super) fn set_from(&self, path: &str) {
        self.from.set(PathBuf::from(path)).ok();
    }

    pub fn ctime(&self) -> &SecondsSinceEpoch {
        &self.ctime
    }

    pub fn nodes(&self) -> u32 {
        self.nodes.load(Relaxed)
    }
    pub fn dirs(&self) -> u32 {
        self.dirs.load(Relaxed)
    }
    pub fn files(&self) -> u32 {
        self.files.load(Relaxed)
    }
    pub fn depth(&self) -> u8 {
        self.depth.load(Relaxed)
    }
    pub fn errors(&self) -> u32 {
        self.errors.load(Relaxed)
    }

    pub(super) fn recursive(&self) -> bool {
        self.recursive.load(Relaxed)
    }
    pub(super) fn resident(&self) -> bool {
        self.resident.load(Relaxed)
    }

    pub(super) fn sync(&self) -> bool {
        self.sync.load(Relaxed)
    }

    pub(super) fn set_recursive(&self, val: bool) {
        self.recursive.store(val, Relaxed);
    }
    pub(super) fn set_resident(&self, val: bool) {
        self.resident.store(val, Relaxed);
    }
    pub(super) fn set_sync(&self, val: bool) {
        self.sync.store(val, Relaxed);
    }

    /// A snapshot of the configured visitor, if any.
    pub(super) fn visitor(&self) -> Option<Arc<dyn Visitor>> {
        self.visitor.read().clone()
    }

    pub(super) fn has_visitor(&self) -> bool {
        self.visitor.read().is_some()
    }

    pub(super) fn set_visitor(&self, v: Option<Arc<dyn Visitor>>) {
        *self.visitor.write() = v;
    }

    pub(super) fn discovery_tx(&self) -> Option<Sender<WalkEvent>> {
        self.discovery_tx.read().clone()
    }

    pub(super) fn set_discovery_tx(&self, tx: Option<Sender<WalkEvent>>) {
        *self.discovery_tx.write() = tx;
    }

    /// The progress observer, or a no-op one if none was set.
    #[inline]
    pub(super) fn observer(&self) -> &dyn TreeObserver {
        match self.observer.get() {
            Some(o) => o.as_ref(),
            None => &NOOP_OBSERVER,
        }
    }

    /// Set the progress observer. Only the first one set takes effect.
    pub(super) fn set_observer(&self, o: Arc<dyn TreeObserver>) {
        self.observer.set(o).ok();
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Relaxed)
    }

    /// Raise the cancellation flag. Subsequent calls into the parallel walker return early.
    pub fn cancel(&self) {
        self.cancelled.store(true, Relaxed);
    }

    /// Increment or decrement the node counter.
    #[inline]
    pub(super) fn nodes_mod(&self, n: i32) {
        mod_atom_u32(&self.nodes, n);
    }
    /// Increment or decrement the dirs counter.
    #[inline]
    pub(super) fn dirs_mod(&self, n: i32) {
        mod_atom_u32(&self.dirs, n);
    }
    /// Increment or decrement the files counter.
    #[inline]
    pub(super) fn files_mod(&self, n: i32) {
        mod_atom_u32(&self.files, n);
    }

    /// Increment the error counter by 1.
    pub(super) fn errors_inc(&self) {
        mod_atom_u32(&self.errors, 1);
    }

    /// Compare the current depth with the given depth and set the maximum.
    pub(super) fn depth_compare(&self, d: u8) {
        self.depth.fetch_max(d, Relaxed);
    }
}
