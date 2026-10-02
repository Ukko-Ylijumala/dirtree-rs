// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::node::NodeRef;
use miniutils::{ToDebug, ToDisplay};
use std::{
    fmt::{self, Debug, Display, Formatter},
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
    /// The tree is being serialized. TODO.
    Serialize,
    /// The tree is being deserialized. TODO.
    Deserialize,
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

/// A [DirTree](super::DirTree) event. Could be an error, warning, or just a notice.
#[derive(Default, Clone, Hash, PartialEq)]
#[cfg_attr(feature = "size_of", derive(SizeOf))]
pub struct TreeEvent {
    pub info: EventInfo,
    pub oper: Option<TreeOp>,
    pub path: Option<String>,
    pub node: Option<NodeRef>,
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

    /// Create an error event.
    pub(super) fn error(msg: &str, op: &TreeOp) -> Self {
        Self {
            info: EventInfo::err_from(&format!("ERROR: {msg}")),
            oper: Some(op.to_owned()),
            ..Default::default()
        }
    }
}

impl Debug for TreeEvent {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let mut msg: String = format!("{} UTC: {}", self.when.to_display(), self.info);
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
