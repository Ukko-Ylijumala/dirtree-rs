// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
[`TreeObserver`]: progress reporting out of a [`DirTree`](super::DirTree),
without the tree knowing who listens (a progress bar, a log, nothing).
*/

use std::fmt::Debug;

/// Used while no observer is set, so callers need no `Option` check.
pub(super) static NOOP_OBSERVER: NoopObserver = NoopObserver;

/**
Receives progress from a [`DirTree`](super::DirTree): walks, updates and
the watcher report the nodes they attach. The parallel walker reports
once per file batch, so a call through `dyn` costs nothing measurable.

`Send + Sync` because rayon workers call it, `Debug` because
[`TreeConf`](super::TreeConf) holds it and is itself `Debug`.
*/
pub trait TreeObserver: Send + Sync + Debug {
    /// `n` directories were attached to the tree.
    fn dirs_added(&self, _n: u64) {}

    /// `n` files were attached; `bytes` is their total size (0 unless the
    /// filemode includes `SIZE`).
    fn files_added(&self, _n: u64, _bytes: u64) {}
}

/// The default observer: no progress reporting.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopObserver;

impl TreeObserver for NoopObserver {}
