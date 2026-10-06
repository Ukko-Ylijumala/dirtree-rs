// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
Built-in [`Visitor`] implementations.

- [`NamePruneVisitor`] - drop a fixed set of directory names by interned
  `u32` membership. The cheapest universal "skip `.git`/`node_modules`/etc."
  building block.
- [`MarkerVisitor`] - recognize directories by the presence of marker
  entries: names in their dirent list, or nested paths below them
  (`wp-includes/version.php`), all of which must be present. Each
  marker carries a [`ScopeTag`], optional `new_scope`, optional
  non-descent (claiming the subtree), and an optional [`MarkerTarget`]
  for the "the *parent* is what we recognized" pattern.
- [`MaxDepthVisitor`] - global or per-scope depth cap.
- [`CompositeVisitor`] - fan out to N visitors and combine their verdicts.
*/

use super::osname::decode_name;
use super::visitor::*;
use dirhandle::{EntryExt, nix::dir::Type};
use stringstore::UniqueStrStore;
use std::{borrow::Cow, sync::Arc};

/* ---------------------------------------- */
/*  NamePruneVisitor                        */
/* ---------------------------------------- */

/**
Prune a fixed set of directory names. All comparisons are `u32`-vs-`u32`
against the pre-interned name set; no string allocations on the hot path.

File pruning is not done here - files are typically scanned for content,
not pruned by name. Use [`Filters`](super::Filters) for regex-based file
filtering if needed.
*/
#[derive(Debug, Clone)]
pub struct NamePruneVisitor {
    names: Vec<u32>,
}

impl NamePruneVisitor {
    /// Pre-interns each name in `dir_names` and stores the resulting indices.
    /// Names not yet in the store are inserted.
    pub fn new(strings: &UniqueStrStore, dir_names: &[&str]) -> Self {
        let mut names: Vec<u32> = dir_names.iter().map(|s| strings.insert(*s)).collect();
        names.sort_unstable();
        names.dedup();
        Self { names }
    }
}

impl Visitor for NamePruneVisitor {
    #[inline]
    fn prune_child(&self, _p: &WalkContext<'_>, child: u32, is_dir: bool) -> bool {
        /*
        Linear scan is fastest for small N (≤ ~16). For larger sets, a
        sorted binary search could win, but the breakpoint is high enough
        that the simple loop is the right default.
        */
        is_dir && self.names.contains(&child)
    }
}

/* ---------------------------------------- */
/*  MarkerVisitor                           */
/* ---------------------------------------- */

/// What kind of entry counts as a marker hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkerKind {
    File,
    Dir,
    Either,
}

impl MarkerKind {
    /// Whether an entry of type `t` (`None`: no such entry) is a hit.
    #[inline]
    fn matches(self, t: Option<Type>) -> bool {
        match (self, t) {
            (_, None) => false,
            (Self::File, Some(t)) => t == Type::File,
            (Self::Dir, Some(t)) => t == Type::Directory,
            (Self::Either, Some(_)) => true,
        }
    }
}

/**
Which directory a marker recognizes, relative to where its entries are.

- [`MarkerTarget::Self_`]: the directory holding the marker entries is
  the one tagged.
- [`MarkerTarget::Parent`]: the directory one level *above* them is
  tagged. This is the WordPress pattern: `version.php` sits in
  `wp-includes`, but the WP root is `wp-includes`' parent.

A Parent marker is checked at the parent's own visit, by looking one
level down (one `fstatat` per entry checked), before the parent's
subdirectories are walked. So the tag lands on the parent, and
`descend(false)` claims the parent's whole subtree. With
[`Marker::when_parent_is`] naming the subdirectory, one subdirectory
is checked; without it, every subdirectory of every directory walked
is, which costs one `fstatat` per subdirectory.

`Marker::file_named("wp-includes/version.php")` with the default
target is the same recognition, spelled as a nested path.
*/
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkerTarget {
    Self_,
    Parent,
}

/**
One entry a [`Marker`] requires: a name in the directory, or a relative
path below it such as `wp-includes/version.php`. A nested path costs one
`fstatat`, and only when its first component is listed as a directory.
Symlinks are not followed in the last component (a symlink is neither
a file nor a directory here), but are in the ones before it.
*/
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MarkerEntry {
    /// Raw bytes, compared with the listed names as they are (no UTF-8 conversion).
    pub path: Vec<u8>,
    pub kind: MarkerKind,
}

impl MarkerEntry {
    /// Whether this entry is present in the directory of `ctx`, or below
    /// its subdirectory `sub`, if given (the caller checked `sub` is one).
    fn present(&self, ctx: &DirContext<'_>, sub: Option<&[u8]>) -> bool {
        if let Some(sub) = sub {
            let rel: Vec<u8> = [sub, b"/", &self.path].concat();
            return self.kind.matches(ctx.type_at(&rel));
        }
        match self.path.iter().position(|&b| b == b'/') {
            None => self
                .kind
                .matches(ctx.entry(&self.path).and_then(|e: &EntryExt| e.file_type())),
            // the first component decides from the listing whether the stat is worth it
            Some(i) => {
                ctx.entry(&self.path[..i]).is_some_and(|e: &EntryExt| e.is_dir())
                    && self.kind.matches(ctx.type_at(&self.path))
            }
        }
    }
}

/**
One marker rule: the entries that must all be present, and what to do
on a match. Use the builder methods to construct and chain into
[`MarkerVisitor::marker`].
*/
#[derive(Clone, Debug)]
pub struct Marker {
    /// All required, in order (cheap ones first saves stats).
    pub entries: Vec<MarkerEntry>,
    pub tag: ScopeTag,
    pub new_scope: ScopeTag,
    pub descend: bool,
    pub target: MarkerTarget,
    /**
    Optional gating on the interned name of the directory holding the
    marker entries: for [`MarkerTarget::Self_`] the directory being
    visited, for [`MarkerTarget::Parent`] the subdirectory looked into.
    Useful for the WP pattern, `version.php` only in a dir named
    `wp-includes`.
    */
    pub when_parent_is: Option<u32>,
}

impl Marker {
    /// A marker with one required entry of `kind` at `path`.
    pub fn new(path: &str, kind: MarkerKind) -> Self {
        Self {
            entries: vec![MarkerEntry { path: path.as_bytes().to_vec(), kind }],
            tag: 0,
            new_scope: SCOPE_NONE,
            descend: true,
            target: MarkerTarget::Self_,
            when_parent_is: None,
        }
    }

    /// A file-named marker. Builder pattern; chain `.tag()`, `.descend()` etc.
    /// `path` is a name or a relative path (see [`MarkerEntry`]).
    pub fn file_named(path: &str) -> Self {
        Self::new(path, MarkerKind::File)
    }

    /// A directory-named marker.
    pub fn dir_named(path: &str) -> Self {
        Self::new(path, MarkerKind::Dir)
    }

    /// Also require an entry of `kind` at `path`.
    pub fn and(mut self, path: &str, kind: MarkerKind) -> Self {
        self.entries.push(MarkerEntry { path: path.as_bytes().to_vec(), kind });
        self
    }

    /// Also require a file at `path`.
    pub fn and_file(self, path: &str) -> Self {
        self.and(path, MarkerKind::File)
    }

    /// Also require a directory at `path`.
    pub fn and_dir(self, path: &str) -> Self {
        self.and(path, MarkerKind::Dir)
    }

    pub fn tag(mut self, tag: ScopeTag) -> Self {
        self.tag = tag;
        self
    }

    pub fn enter_scope(mut self, scope: ScopeTag) -> Self {
        self.new_scope = scope;
        self
    }

    pub fn descend(mut self, descend: bool) -> Self {
        self.descend = descend;
        self
    }

    pub fn target(mut self, target: MarkerTarget) -> Self {
        self.target = target;
        self
    }

    /// Gate this marker on the name of the directory holding its entries.
    pub fn when_parent_is(mut self, parent_name_idx: u32) -> Self {
        self.when_parent_is = Some(parent_name_idx);
        self
    }

    /// Whether all the entries are present in the directory of `ctx` (or below `sub`).
    fn present(&self, ctx: &DirContext<'_>, sub: Option<&[u8]>) -> bool {
        self.entries.iter().all(|e: &MarkerEntry| e.present(ctx, sub))
    }

    /// Whether this marker recognizes the directory of `ctx`.
    fn matches(&self, ctx: &DirContext<'_>) -> bool {
        match (self.target, self.when_parent_is) {
            (MarkerTarget::Self_, Some(gate)) if ctx.walk.name_idx != gate => false,
            (MarkerTarget::Self_, _) => self.present(ctx, None),
            (MarkerTarget::Parent, Some(gate)) => {
                let Ok(name) = ctx.walk.strings.get(gate) else {
                    return false;
                };
                let name: Cow<[u8]> = decode_name(name);
                ctx.entry(&name).is_some_and(|e: &EntryExt| e.is_dir())
                    && self.present(ctx, Some(&name))
            }
            (MarkerTarget::Parent, None) => ctx
                .entries
                .iter()
                .filter(|e: &&EntryExt| e.is_dir())
                .any(|e: &EntryExt| self.present(ctx, Some(e.file_name().to_bytes()))),
        }
    }
}

/// A bag of markers. The first matching marker wins for a given directory.
#[derive(Default, Debug, Clone)]
pub struct MarkerVisitor {
    markers: Vec<Marker>,
}

impl MarkerVisitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn marker(mut self, m: Marker) -> Self {
        self.markers.push(m);
        self
    }
}

impl Visitor for MarkerVisitor {
    fn visit_dir(&self, ctx: DirContext<'_>) -> Verdict {
        match self.markers.iter().find(|m: &&Marker| m.matches(&ctx)) {
            Some(m) => Verdict::Tag {
                tag: m.tag,
                new_scope: m.new_scope,
                descend: m.descend,
            },
            None => Verdict::Continue,
        }
    }
}

/* ---------------------------------------- */
/*  MaxDepthVisitor                         */
/* ---------------------------------------- */

/// Enforce a global or per-scope depth cap.
#[derive(Default, Debug, Clone)]
pub struct MaxDepthVisitor {
    /// Cap applied when no scope-specific cap is set (and `scope == SCOPE_NONE`).
    pub global: usize,
    /// (scope, max_depth) pairs. Linear scan; expected to be tiny.
    pub per_scope: Vec<(ScopeTag, usize)>,
}

impl MaxDepthVisitor {
    pub fn new(global: usize) -> Self {
        Self { global, per_scope: Vec::new() }
    }

    pub fn for_scope(mut self, scope: ScopeTag, depth: usize) -> Self {
        self.per_scope.push((scope, depth));
        self
    }
}

impl Visitor for MaxDepthVisitor {
    fn max_depth(&self, scope: ScopeTag) -> usize {
        for &(s, d) in &self.per_scope {
            if s == scope {
                return d;
            }
        }
        self.global
    }
}

/* ---------------------------------------- */
/*  CompositeVisitor                        */
/* ---------------------------------------- */

/**
Fan out to N visitors. Verdicts are combined via [`Verdict::combine`]
(most restrictive wins). Prunes are OR - any inner pruning prunes the
child. Depth caps are min - the tightest cap wins.
*/
#[derive(Default, Debug)]
pub struct CompositeVisitor {
    inner: Vec<Arc<dyn Visitor>>,
}

impl CompositeVisitor {
    pub fn new() -> Self {
        Self::default()
    }

    // a builder method, not addition
    #[allow(clippy::should_implement_trait)]
    pub fn add<V: Visitor + 'static>(mut self, v: V) -> Self {
        self.inner.push(Arc::new(v));
        self
    }

    pub fn add_arc(mut self, v: Arc<dyn Visitor>) -> Self {
        self.inner.push(v);
        self
    }
}

impl Visitor for CompositeVisitor {
    fn visit_dir(&self, ctx: DirContext<'_>) -> Verdict {
        let mut acc = Verdict::Continue;
        for v in &self.inner {
            // Each inner visitor sees a fresh DirContext copy.
            acc = acc.combine(v.visit_dir(ctx));
        }
        acc
    }

    fn prune_child(
        &self,
        parent: &WalkContext<'_>,
        child_name_idx: u32,
        is_dir: bool,
    ) -> bool {
        self.inner
            .iter()
            .any(|v| v.prune_child(parent, child_name_idx, is_dir))
    }

    fn visit_file(&self, ctx: &FileContext<'_>) -> FileVerdict {
        // every inner visitor sees the entry, even after one has dropped it
        let mut acc = FileVerdict::Keep;
        for v in &self.inner {
            acc = acc.combine(v.visit_file(ctx));
        }
        acc
    }

    fn max_depth(&self, scope: ScopeTag) -> usize {
        // Min of non-zero caps; 0 means unbounded.
        let mut min_cap: usize = 0;
        for v in &self.inner {
            let d = v.max_depth(scope);
            if d == 0 {
                continue;
            }
            min_cap = if min_cap == 0 { d } else { min_cap.min(d) };
        }
        min_cap
    }
}
