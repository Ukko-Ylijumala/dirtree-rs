// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
Built-in [`Visitor`] implementations.

- [`NamePruneVisitor`] - drop a fixed set of directory names by interned
  `u32` membership. The cheapest universal "skip `.git`/`node_modules`/etc."
  building block.
- [`MarkerVisitor`] - recognize directories by the presence of a marker
  file (or directory) in their dirent list. Each marker carries a
  [`ScopeTag`], optional `new_scope`, optional non-descent, and an
  optional [`MarkerTarget`] for the "the *parent* is what we recognized"
  pattern (e.g. WordPress `wp-includes/version.php`).
- [`MaxDepthVisitor`] - global or per-scope depth cap.
- [`CompositeVisitor`] - fan out to N visitors and combine their verdicts.
*/

use super::visitor::*;
use stringstore::UniqueStrStore;
use std::sync::Arc;

/* ---------------------------------------- */
/*  NamePruneVisitor                        */
/* ---------------------------------------- */

/**
Prune a fixed set of directory names. All comparisons are `u32`-vs-`u32`
against the pre-interned name set; no string allocations on the hot path.

File pruning is not done here - files are typically scanned for content,
not pruned by name. Use [`crate::filters::Filters`] for regex-based file
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
        is_dir && self.names.iter().any(|&n| n == child)
    }
}

/* ---------------------------------------- */
/*  MarkerVisitor                           */
/* ---------------------------------------- */

/// What kind of dirent counts as a marker hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkerKind {
    File,
    Dir,
    Either,
}

/**
Where the [`WalkEvent`](super::WalkEvent) attaches when a marker matches.

- [`MarkerTarget::Self_`]: the directory containing the marker is what
  gets tagged.
- [`MarkerTarget::Parent`]: the *parent* of the directory containing the
  marker is what gets tagged. This is the WordPress pattern: detect
  `version.php` inside `wp-includes`, but the "WP root" is `wp-includes`'s
  parent.
*/
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkerTarget {
    Self_,
    Parent,
}

/// One marker rule. Use the builder methods to construct and chain into [`MarkerVisitor::marker`].
#[derive(Clone, Debug)]
pub struct Marker {
    /**
    Raw bytes of the marker entry's basename (no trailing nul).
    Compared against `EntryExt::file_name().to_bytes()` directly so that
    no UTF-8 conversion is required.
    */
    pub name: Vec<u8>,
    pub kind: MarkerKind,
    pub tag: ScopeTag,
    pub new_scope: ScopeTag,
    pub descend: bool,
    pub target: MarkerTarget,
    /**
    Optional gating: only fire when the parent dir's interned name
    equals this index. Useful for the WP pattern where you want to
    detect `version.php` only when in a dir named `wp-includes`.
    */
    pub when_parent_is: Option<u32>,
}

impl Marker {
    /// A file-named marker. Builder pattern; chain `.tag()`, `.descend()` etc.
    pub fn file_named(name: &str) -> Self {
        Self {
            name: name.as_bytes().to_vec(),
            kind: MarkerKind::File,
            tag: 0,
            new_scope: SCOPE_NONE,
            descend: true,
            target: MarkerTarget::Self_,
            when_parent_is: None,
        }
    }

    /// A directory-named marker.
    pub fn dir_named(name: &str) -> Self {
        Self {
            name: name.as_bytes().to_vec(),
            kind: MarkerKind::Dir,
            tag: 0,
            new_scope: SCOPE_NONE,
            descend: true,
            target: MarkerTarget::Self_,
            when_parent_is: None,
        }
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

    /// Gate this marker on the parent directory's interned name.
    pub fn when_parent_is(mut self, parent_name_idx: u32) -> Self {
        self.when_parent_is = Some(parent_name_idx);
        self
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
        for m in &self.markers {
            if let Some(p) = m.when_parent_is {
                /*
                Gate: only consider this marker when the *current dir's
                own name* (which is the parent of the entries we're
                scanning) matches. So we test ctx.walk.name_idx, not the
                grandparent. The naming "when_parent_is" reflects the
                marker's perspective - the marker is a child entry, and
                its parent is `ctx.walk`.
                */
                if ctx.walk.name_idx != p {
                    continue;
                }
            }
            let hit = ctx.entries.iter().any(|e| {
                if e.file_name().to_bytes() != m.name.as_slice() {
                    return false;
                }
                match m.kind {
                    MarkerKind::File => e.is_file(),
                    MarkerKind::Dir => e.is_dir(),
                    MarkerKind::Either => true,
                }
            });
            if !hit {
                continue;
            }
            /*
            Build the verdict. MarkerTarget::Parent is handled by the
            walker (it adjusts where the WalkEvent's path attaches);
            here we just signal the tag. To make the walker's job
            straightforward we encode "target=Parent" by setting
            descend=false (the matching dir is owned by the marker
            logic) and the walker emits the parent's path.

            For simplicity in v1: visit_dir cannot directly retarget
            the path. We emit the matching dir's path; if the caller
            needs the parent, they can `Path::parent()` on the receiver
            side. This keeps the walker's emit logic uniform.

            A future revision can extend Verdict with an explicit
            PathTarget if needed.
            */
            let _ = m.target; // currently informational; consumer-side concern
            return Verdict::Tag {
                tag: m.tag,
                new_scope: m.new_scope,
                descend: m.descend,
            };
        }
        Verdict::Continue
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
            let dctx = DirContext { walk: ctx.walk, entries: ctx.entries };
            acc = acc.combine(v.visit_dir(dctx));
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
