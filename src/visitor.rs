// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
Visitor protocol for [`DirTree`].

See `docs/implementation.md` for the design rationale. Brief summary:

- [`Visitor`] is a per-directory hook + per-child prune + per-file hook +
  per-scope depth cap. It is invoked from the parallel walker
  (`populate_par_inner`), never from the synchronous `populate` path.
- [`Verdict`] is the visitor's answer for a directory: continue, skip
  children, or tag (with optional new scope, optional non-descent).
  [`FileVerdict`] is its answer for a file: keep, store anyway, or drop.
- [`WalkContext`] / [`DirContext`] carry pre-interned `u32` name indices
  so prune callbacks can answer with a handful of integer compares and
  zero allocations.
- [`WalkEvent`] is the streaming output: every `Verdict::Tag` produces
  one event on the optional `Sender<WalkEvent>` configured on the tree.
  The tag is also stored on the directory (see
  [`DirTree::tagged`](super::DirTree::tagged)) for querying after the walk.

The trait coexists with [`Filters`](super::Filters): both run
per child, prune wins over filter, both veto.

[`DirTree`]: super::DirTree
*/

use super::node::{FileKind, mode_type};

use dirhandle::{
    EntryExt,
    nix::{
        dir::Type,
        fcntl::AtFlags,
        sys::stat::fstatat,
    },
};
use stringstore::UniqueStrStore;

use std::{
    os::fd::BorrowedFd,
    path::{Path, PathBuf},
    sync::Arc,
};

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
    /**
    The directory itself, open: for `*at()` calls relative to it, such
    as dirhandle's `open_regular_at()` or [DirContext::type_at]. Open
    entries through it, never through `path`: a path may be longer than
    `PATH_MAX`, and any of its components may have been swapped for a
    symlink since the walk passed it.
    */
    pub dirfd: BorrowedFd<'a>,
}

/**
Per-call context for [`Visitor::visit_dir`].

Bundles the [`WalkContext`] with the freshly-read dirent list of the
directory (whose fd is [`WalkContext::dirfd`]). The slice is borrowed
from a `Vec<EntryExt>` collected by the walker; do not retain references
to it past the `visit_dir` call.
*/
#[derive(Clone, Copy)]
pub struct DirContext<'a> {
    pub walk: &'a WalkContext<'a>,
    pub entries: &'a [EntryExt<'a>],
}

/**
Per-call context for [`Visitor::visit_file`]: one non-directory entry,
in the [`WalkContext`] of its directory (`walk.dirfd` is that directory,
open), with the directory's whole listing for sibling lookups.

The entry carries its type from `readdir` and a lazy, cached stat:
`entry.stat()` (or `len()`, `mode()`, `mtime()`, ...) is one `fstatat` on
the directory's fd, the first time only. `entry.open_regular()` opens it
for reading - no symlink followed, no blocking on a FIFO, regular files
only - and fills that cache from the opened file. Do not retain the
references past the `visit_file` call.
*/
#[derive(Clone, Copy)]
pub struct FileContext<'a> {
    pub walk: &'a WalkContext<'a>,
    pub entry: &'a EntryExt<'a>,
    /// Interned name of the entry.
    pub name_idx: u32,
    /// The entry's kind (a regular file, a symlink or another special file).
    pub kind: FileKind,
    /// Every entry of the directory, subdirectories included.
    pub listing: &'a [EntryExt<'a>],
}

impl DirContext<'_> {
    /// The entry of this directory named `name` (raw bytes), if listed.
    pub fn entry(&self, name: &[u8]) -> Option<&EntryExt<'_>> {
        listed(self.entries, name)
    }

    /**
    The type of the entry at `rel`, a path relative to this directory
    such as `wp-includes/version.php`: one `fstatat` on the directory's
    fd, not following a symlink in the last component. [None] if there
    is no such entry (or it cannot be reached). For an entry of this
    directory itself, [DirContext::entry] answers without a syscall.
    */
    pub fn type_at(&self, rel: &[u8]) -> Option<Type> {
        let st = fstatat(self.walk.dirfd, rel, AtFlags::AT_SYMLINK_NOFOLLOW).ok()?;
        mode_type(st.st_mode)
    }
}

impl FileContext<'_> {
    /// The entry of the same directory named `name` (raw bytes), if listed: no syscall.
    pub fn sibling(&self, name: &[u8]) -> Option<&EntryExt<'_>> {
        listed(self.listing, name)
    }
}

/// The entry of `entries` named `name` (raw bytes), if any.
fn listed<'e>(entries: &'e [EntryExt<'e>], name: &[u8]) -> Option<&'e EntryExt<'e>> {
    entries.iter().find(|e: &&EntryExt| e.name_as_bytes() == name)
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
    "Most restrictive" combine. Used by [`super::CompositeVisitor`].

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
The visitor's answer for a non-directory entry ([`Visitor::visit_file`]).
*/
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FileVerdict {
    /// Count the entry, and store it if the tree's [`FileMode`](super::FileMode) stores files.
    #[default]
    Keep,
    /**
    Count the entry and store it whatever the file mode: e.g. a
    directory skeleton (a mode that stores no files) that keeps the
    files a scanner selected, and nothing else.
    */
    Store,
    /// Drop the entry, as [`Visitor::prune_child`] would: neither counted nor stored.
    Drop,
}

impl FileVerdict {
    /**
    Combine two answers. Used by [`super::CompositeVisitor`].

    Precedence: `Drop` > `Store` > `Keep`.
    */
    pub fn combine(self, other: Self) -> Self {
        use FileVerdict::*;
        match (self, other) {
            (Drop, _) | (_, Drop) => Drop,
            (Store, _) | (_, Store) => Store,
            _ => Keep,
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
    Called for each non-directory entry (a regular file, a symlink or
    another special file) that `prune_child` and the name filters let
    through, before it is counted and stored - whatever the tree's file
    mode, also when files are not stored at all. Not called for the
    entries of a directory whose `visit_dir` declined its children.

    The place for per-file work during the walk: select a candidate by
    name or [`FileContext::kind`], look at its lazy stat or its
    siblings, open it relative to its directory, and hand it on (e.g.
    through a channel the visitor owns), so the work overlaps the walk.
    Calls come from many walker threads at once. Default:
    [`FileVerdict::Keep`].
    */
    fn visit_file(&self, _ctx: &FileContext<'_>) -> FileVerdict {
        FileVerdict::Keep
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
    fn visit_file(&self, ctx: &FileContext<'_>) -> FileVerdict {
        (**self).visit_file(ctx)
    }
    #[inline]
    fn max_depth(&self, scope: ScopeTag) -> usize {
        (**self).max_depth(scope)
    }
}
