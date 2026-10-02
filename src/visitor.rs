// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
Visitor protocol for [`DirTree`].

See `docs/implementation.md` for the design rationale. Brief summary:

- [`Visitor`] is a per-directory hook + per-child prune + per-scope depth
  cap. It is invoked from the parallel walker (`populate_par_inner`),
  never from the synchronous `populate` path.
- [`Verdict`] is the visitor's answer for a directory: continue, skip
  children, or tag (with optional new scope, optional non-descent).
- [`WalkContext`] / [`DirContext`] carry pre-interned `u32` name indices
  so prune callbacks can answer with a handful of integer compares and
  zero allocations.
- [`WalkEvent`] is the streaming output: every `Verdict::Tag` produces
  one event on the optional `Sender<WalkEvent>` configured on the tree.

The trait coexists with [`Filters`](super::Filters): both run
per child, prune wins over filter, both veto.

[`DirTree`]: super::DirTree
*/

use dirhandle::EntryExt;
use stringstore::UniqueStrStore;

use std::{path::{Path, PathBuf}, sync::Arc};

/**
User-defined identifier for a recognized subtree kind.

`0` is reserved as [`SCOPE_NONE`]. All other values are caller-defined;
the framework treats tags opaquely.
*/
pub type ScopeTag = u16;

/// "No active scope" - the default scope at the root of a walk.
pub const SCOPE_NONE: ScopeTag = 0;

/**
Per-directory information passed to a [`Visitor`].

Constructed once per directory entered. Field access is `O(1)`; all
pre-interned ids are looked up by the walker in advance.
*/
#[derive(Clone, Copy)]
pub struct WalkContext<'a> {
    /// Absolute filesystem path of the directory currently being walked.
    pub path: &'a Path,
    /// Interned name of this directory.
    pub name_idx: u32,
    /// Interned name of the parent directory, if any. `None` only at the walk root.
    pub parent_name_idx: Option<u32>,
    /// Distance from the walk root (root is at depth `0`).
    pub depth: usize,
    /// Active scope flowing from a recognized ancestor. [`SCOPE_NONE`] when
    /// no ancestor has set one.
    pub scope: ScopeTag,
    /// Reference to the tree's string interner. Visitors that need to
    /// resolve a `u32` back to a `&str` (rare) can use this.
    pub strings: &'a UniqueStrStore,
}

/**
Per-call context for [`Visitor::visit_dir`].

Bundles the [`WalkContext`] with the freshly-read dirent list of the
directory. The slice is borrowed from a `Vec<EntryExt>` collected by
the walker; do not retain references to it past the `visit_dir` call.
*/
pub struct DirContext<'a> {
    pub walk: &'a WalkContext<'a>,
    pub entries: &'a [EntryExt<'a>],
}

/// The visitor's answer for the directory currently being walked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Recurse into children normally, under the current scope.
    #[default]
    Continue,
    /// Record this dir but do not recurse into its children. The visitor
    /// is declining to descend; no [`WalkEvent`] is emitted for this case.
    SkipChildren,
    /**
    Tag this directory with the given `tag` and emit a [`WalkEvent`].
    Descendants walk under `new_scope` (pass [`SCOPE_NONE`] or the
    current scope to leave it unchanged). If `descend == false` the
    children of this directory are not walked - the visitor claims
    the subtree.
    */
    Tag {
        tag: ScopeTag,
        new_scope: ScopeTag,
        descend: bool,
    },
}

impl Verdict {
    /// Convenience: `Tag { tag, new_scope: SCOPE_NONE, descend: true }`.
    #[inline]
    pub fn tag(tag: ScopeTag) -> Self {
        Self::Tag { tag, new_scope: SCOPE_NONE, descend: true }
    }

    /// Set `new_scope` on a `Tag` verdict. No-op for other variants.
    #[inline]
    pub fn enter_scope(self, scope: ScopeTag) -> Self {
        match self {
            Self::Tag { tag, descend, .. } => Self::Tag { tag, new_scope: scope, descend },
            other => other,
        }
    }

    /// Set `descend = false` on a `Tag` verdict. No-op for other variants.
    #[inline]
    pub fn claim(self) -> Self {
        match self {
            Self::Tag { tag, new_scope, .. } => {
                Self::Tag { tag, new_scope, descend: false }
            }
            other => other,
        }
    }

    /**
    "Most restrictive" combine. Used by [`crate::tree::CompositeVisitor`].

    Precedence: `SkipChildren` > `Tag{descend: false}` > `Tag{descend: true}` > `Continue`.

    When two `Tag` verdicts of equal precedence collide, `self`'s tag wins;
    the caller is expected to emit any extra events for the loser separately.
    */
    pub fn combine(self, other: Self) -> Self {
        use Verdict::*;
        match (self, other) {
            (SkipChildren, _) | (_, SkipChildren) => SkipChildren,
            (Tag { descend: false, .. }, _) => self,
            (_, Tag { descend: false, .. }) => other,
            (Tag { .. }, _) => self,
            (_, Tag { .. }) => other,
            _ => Continue,
        }
    }
}

/**
A single discovery event emitted whenever a [`Visitor`] returns
[`Verdict::Tag`]. Streamed on the optional `Sender<WalkEvent>`
configured on the tree.
*/
#[derive(Clone, Debug)]
pub struct WalkEvent {
    /// Absolute filesystem path of the tagged directory.
    pub path: PathBuf,
    /// User-defined kind for the match.
    pub tag: ScopeTag,
    /// Active scope at the moment of the match (the scope of the dir's
    /// parent, before any `new_scope` from this verdict takes effect).
    pub scope: ScopeTag,
    /// Distance from the walk root.
    pub depth: usize,
    /// `true` when the verdict had `descend: false`.
    pub claimed: bool,
}

/**
Per-directory hook + per-child prune + per-scope depth cap.

All methods have safe defaults so impls only override what they need.
Implementations must be `Send + Sync + Debug` - `Send + Sync` because
`populate_par_inner` invokes them from rayon workers, `Debug` because
[`TreeConf`](super::TreeConf) embeds them and is itself `Debug`.
*/
pub trait Visitor: Send + Sync + std::fmt::Debug {
    /// Called once per directory after its dirents have been read,
    /// before any recursion into children. Default: [`Verdict::Continue`].
    fn visit_dir(&self, _ctx: DirContext<'_>) -> Verdict {
        Verdict::Continue
    }

    /**
    Called for each child entry before insertion or recursion. Return
    `true` to drop the entry (it will not be recorded in the tree, nor
    recursed into). The child's name is given as a pre-interned `u32`.
    Default: keep everything.
    */
    fn prune_child(
        &self,
        _parent: &WalkContext<'_>,
        _child_name_idx: u32,
        _is_dir: bool,
    ) -> bool {
        false
    }

    /**
    Per-scope max recursion depth. `0` means unbounded. Called once
    per directory, before its `visit_dir`. Default: unbounded.
    */
    fn max_depth(&self, _scope: ScopeTag) -> usize {
        0
    }
}

// Blanket Visitor for Arc<V> so existing visitors can be wrapped without reimplementing the trait.
impl<V: Visitor + ?Sized> Visitor for Arc<V> {
    #[inline]
    fn visit_dir(&self, ctx: DirContext<'_>) -> Verdict {
        (**self).visit_dir(ctx)
    }
    #[inline]
    fn prune_child(&self, p: &WalkContext<'_>, c: u32, is_dir: bool) -> bool {
        (**self).prune_child(p, c, is_dir)
    }
    #[inline]
    fn max_depth(&self, scope: ScopeTag) -> usize {
        (**self).max_depth(scope)
    }
}
