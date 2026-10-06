// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::node::NodeRef;
use super::osname::decode_path;
use miniutils::{ToDebug, ToDisplay};
use std::{
    fmt::{self, Debug, Display, Formatter},
    io,
    path::PathBuf,
};
use timesince::TimeSinceEpoch;

#[cfg(feature = "size_of")]
use size_of::SizeOf;

/// The current operation being performed on the [[DirTree](super::DirTree)].
#[derive(Default, Debug, Clone, Eq, PartialEq, Hash)]
#[cfg_attr(feature = "size_of", derive(SizeOf))]
pub enum TreeOp {
    #[default]
    None,
    /// The tree is being built. Implicitly recursive.
    Build(PathBuf),
    /// Scan a directory and process it into the tree, optionally recursively.
    Scan(PathBuf, Option<bool>),
    /// A node (or leaf) is being inserted into the tree.
    Insert,
    /// A node (or leaf) is being removed from the tree.
    Remove(String),
    /// The tree is being updated.
    Update(PathBuf),
    /// The tree is being saved to a snapshot file (see [DirTree::save_to](super::DirTree::save_to)).
    Save(PathBuf),
    /// The tree is being loaded from a snapshot file (see [DirTree::load_from](super::DirTree::load_from)).
    Load(PathBuf),
    /// Signals the background worker thread that it should quit.
    Quit,
}

/// The current state of the [[DirTree](super::DirTree)].
#[derive(Default, Debug, Clone, Hash, PartialEq)]
pub enum TreeState {
    /// Initial state, no nodes.
    #[default]
    Uninitialized,
    /// Initialized but empty.
    Empty,
    /// The tree is ready for use.
    Ready,
    /// The tree is being actively used.
    Active(TreeOp),
    /// The tree is in an inconsistent state.
    Inconsistent(TreeEvent),
    /// The tree is in an error state.
    Error(TreeEvent),
    /// The tree is being torn down.
    Quitting,
}

#[derive(Default, Debug, Clone, Hash, PartialEq)]
#[cfg_attr(feature = "size_of", derive(SizeOf))]
pub enum EventInfo {
    #[default]
    None,
    Begin,
    End,
    Err(String),
    Msg(String),
}

impl EventInfo {
    fn msg_from(s: &str) -> Self {
        Self::Msg(s.to_string())
    }

    fn err_from(s: &str) -> Self {
        Self::Err(s.to_string())
    }
}

impl Display for EventInfo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::None => "None".to_string(),
                Self::Begin => "Begin".to_string(),
                Self::End => "End".to_string(),
                Self::Err(s) => s.clone(),
                Self::Msg(s) => s.clone(),
            }
        )
    }
}

/**
What a [TreeFault] is about. The first five are holes in what the tree
holds of the filesystem; a consumer that reports its coverage counts
them. An `errno` of `ENOENT` on one of them means the entry vanished
while being handled (churn) rather than that it could not be seen.
*/
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "size_of", derive(SizeOf))]
#[non_exhaustive]
pub enum FaultKind {
    /// A directory could not be opened to be listed: its entries are missing.
    OpenDir,
    /// Listing a directory failed part-way: some of its entries are missing.
    ReadDir,
    /// An entry (or a path given to the tree) could not be stat'ed: it was skipped.
    Stat,
    /// A symlink's target could not be read: the entry is kept without one.
    ReadLink,
    /// The watcher could not watch a directory: changes in it go unseen.
    Watch,
    /**
    The watcher lost track: its event queue overflowed, its root went
    away, or a resync failed. Changes may have been missed.
    */
    WatchLost,
    /// The tree's own structure got in the way (a path not in the tree, a slot taken by another kind of entry...).
    Tree,
    /// The background worker thread panicked.
    Worker,
}

/**
A fault, as a [TreeObserver](super::TreeObserver) is told of it: what,
where, and the OS error if a syscall failed. Each fault also counts in
[TreeConf::errors](super::TreeConf::errors), and the event log holds
the latest ones as [TreeEvent]s.
*/
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TreeFault {
    pub kind: FaultKind,
    /// The filesystem path concerned (exact bytes); [None] when not about one.
    pub path: Option<PathBuf>,
    /// The OS error number, when a syscall failed.
    pub errno: Option<i32>,
}

/// A [DirTree](super::DirTree) event. Could be an error, warning, or just a notice.
#[derive(Default, Clone, Hash, PartialEq)]
#[cfg_attr(feature = "size_of", derive(SizeOf))]
pub struct TreeEvent {
    pub info: EventInfo,
    pub oper: Option<TreeOp>,
    /// Encoded (see [encode_name](super::encode_name)).
    pub path: Option<String>,
    pub node: Option<NodeRef>,
    /// Set on errors: what the fault is about.
    pub fault: Option<FaultKind>,
    /// Set on errors from a failed syscall.
    pub errno: Option<i32>,
    #[cfg_attr(feature = "size_of", size_of(skip))]
    pub when: TimeSinceEpoch,
}

impl TreeEvent {
    /// Create a new tree event with the given message. Details can be provided
    /// by chaining with the `path()`, `node()`, and `op()` methods.
    pub(super) fn new(msg: &str) -> Self {
        Self {
            info: EventInfo::msg_from(msg),
            ..Default::default()
        }
    }

    /// Specify a path for the event.
    pub(super) fn path(mut self, path: &str) -> Self {
        self.path = Some(path.to_owned());
        self
    }

    /// Specify the tree entry the event is about.
    pub(super) fn node(mut self, node: NodeRef) -> Self {
        self.node = Some(node);
        self
    }

    /// Specify a [TreeOperation] for the event.
    pub(super) fn op(mut self, oper: &TreeOp) -> Self {
        self.oper = Some(oper.to_owned());
        self
    }

    /// Mark the start of an operation.
    pub(super) fn op_beg(op: &TreeOp) -> Self {
        Self {
            info: EventInfo::Begin,
            oper: Some(op.to_owned()),
            ..Default::default()
        }
    }

    /// Mark the end of an operation.
    pub(super) fn op_end(op: &TreeOp) -> Self {
        Self {
            info: EventInfo::End,
            oper: Some(op.to_owned()),
            ..Default::default()
        }
    }

    /// Create an error event of `kind`; chain `path()`, `io()` and `op()` for the details.
    pub(super) fn error(kind: FaultKind, msg: &str) -> Self {
        Self {
            info: EventInfo::err_from(&format!("ERROR: {msg}")),
            fault: Some(kind),
            ..Default::default()
        }
    }

    /// Specify the OS error the event comes from, if it has one.
    pub(super) fn io(mut self, e: &io::Error) -> Self {
        self.errno = e.raw_os_error();
        self
    }

    /// Specify the OS error number the event comes from.
    pub(super) fn errno(mut self, errno: i32) -> Self {
        self.errno = Some(errno);
        self
    }

    /// The [TreeFault] of an error event ([FaultKind::Tree] if it has no kind).
    pub fn to_fault(&self) -> TreeFault {
        TreeFault {
            kind: self.fault.unwrap_or(FaultKind::Tree),
            path: self.path.as_deref().map(decode_path),
            errno: self.errno,
        }
    }
}

impl Debug for TreeEvent {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let mut msg: String = format!("{} UTC: {}", self.when.to_display(), self.info);
        if let Some(kind) = &self.fault {
            msg.push_str(&format!(", fault: {kind:?}"));
        }
        if let Some(errno) = &self.errno {
            msg.push_str(&format!(", errno: {errno}"));
        }
        if let Some(op) = &self.oper {
            msg.push_str(&format!(", oper: {}", op.to_debug()));
        }
        if let Some(path) = &self.path {
            msg.push_str(&format!(", path: {}", path));
        }
        if let Some(node) = &self.node {
            msg.push_str(&format!(", node: {:?}", node));
        }
        write!(f, "TreeEvent {{ {msg} }}")
    }
}

impl Display for TreeEvent {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let mut msg: String = format!("{} UTC: {}", self.when, self.info);
        if let Some(op) = &self.oper {
            msg.push_str(&format!(", op: {}", op.to_debug()));
        }
        if let Some(path) = &self.path {
            msg.push_str(&format!(" ({})", path));
        }
        if let Some(node) = &self.node {
            msg.push_str(&format!(" [{:?}]", node));
        }
        write!(f, "{msg}")
    }
}
