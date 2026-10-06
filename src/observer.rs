// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
[`TreeObserver`]: progress reporting out of a [`DirTree`](super::DirTree),
without the tree knowing who listens (a progress bar, a log, nothing).
*/

use super::event::TreeFault;
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

    /// `n` regular files were attached; `bytes` is their total size (0
    /// unless the filemode includes `SIZE`).
    fn files_added(&self, _n: u64, _bytes: u64) {}

    /// `n` special files (symlinks, FIFOs, sockets, devices) were attached.
    fn specials_added(&self, _n: u64) {}

    /**
    Something could not be seen or done (see [FaultKind](super::FaultKind)).
    Called once per fault, from whichever thread hit it, including the
    repeated ones the log does not keep.

    This is the one complete record of the holes in a walk: a consumer
    that reports its coverage must take it from here. The tree's event
    log keeps only its latest few thousand events, and the faults of a
    walk with hooks of its own ([`WalkHooks`](super::WalkHooks)) reach
    that walk's observer, not the tree's.
    */
    fn fault(&self, _fault: &TreeFault) {}
}

/// The default observer: no progress reporting.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopObserver;

impl TreeObserver for NoopObserver {}
