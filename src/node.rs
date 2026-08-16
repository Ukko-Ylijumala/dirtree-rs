// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use super::hash::DirTreeXxh3Hasher;
use crate::{PATH_SEP, utils::make_weak_ref};

use dirhandle::DirFd;
use stringstore::UniqueStrStore;
use timesince::SecondsSinceEpoch;

use parking_lot::RwLock;
use rayon::prelude::*;
use tracing::{error, instrument, trace_span};

use std::{
    cmp::Ordering,
    collections::HashMap,
    collections::hash_map::Entry as HmEntry,
    fs::{Metadata, metadata},
    hash::{Hash, Hasher},
    io::{Error, ErrorKind},
    ops::{Deref, DerefMut},
    os::fd::RawFd,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    ptr,
    sync::{Arc, OnceLock, Weak},
};

#[cfg(feature = "size_of")]
use {
    size_of::{Context, SizeOf, TotalSize},
    std::mem::size_of,
};

static META_FAIL: &str = "Failed to get metadata";

// Convenience aliases
pub(super) type MaybeNode = Option<Arc<Node>>;
pub(super) type Children = RwLock<DirTreeHashMap<u32, MaybeNode>>;
pub(super) type NodeIter<'a> = dyn Iterator<Item = Arc<Node>> + 'a;
pub(super) type DirTreeHashMap<K, V> = HashMap<K, V, DirTreeXxh3Hasher>;

#[derive(Default, Debug, Clone, Eq)]
struct Data {
    inode: u64,
    /// last scan time (seconds since UNIX epoch)
    when: SecondsSinceEpoch,
}

impl Data {
    /// The inode of the file or directory.
    pub fn inode(&self) -> u64 {
        self.inode
    }
}

// An inode should be enough to uniquely identify a file or directory.
impl Hash for Data {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inode.hash(state);
    }
}

impl PartialEq for Data {
    fn eq(&self, other: &Self) -> bool {
        self.inode == other.inode
    }
}

impl Ord for Data {
    fn cmp(&self, other: &Self) -> Ordering {
        self.inode.cmp(&other.inode)
    }
}

impl PartialOrd for Data {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/* ######################################################################### */

/**
A common trait for Directory and File entries.

Used to consolidate common code between [Directory] and [FileEntry] structs
(which themselves are just type placeholders for [Entry] struct).
*/
trait DirectoryEntry {
    fn data(&self) -> &Data;
    fn data_mut(&mut self) -> &mut Data;

    fn stat(&self, path: &PathBuf) -> Option<Metadata> {
        match metadata(path) {
            Ok(meta) => return Some(meta),
            Err(_) => return None,
        };
    }

    #[instrument(level = "debug", skip(self))]
    fn rescan(&mut self, path: &PathBuf) -> Result<Metadata, Error> {
        let meta: Metadata = match self.stat(path) {
            Some(m) => m,
            // failed to get metadata, likely deleted in the meantime
            None => {
                let mut msg: String = String::from(META_FAIL);
                msg.push_str(format!(": {}", path.display()).as_str());
                error!(msg);
                return Err(Error::new(ErrorKind::NotFound, META_FAIL));
            }
        };
        if self.data().inode != meta.ino() {
            // inode changed, file/dir was replaced and we're out of sync
            // this case must be handled by the caller
            error!("Inode changed: {} ({} -> {})", path.display(), self.data().inode, meta.ino());
            return Err(Error::new(ErrorKind::AlreadyExists, "Inode changed"));
        }
        self.data_mut().when = SecondsSinceEpoch::new();
        Ok(meta)
    }
}

impl AsRef<Data> for dyn DirectoryEntry {
    fn as_ref(&self) -> &Data {
        self.data()
    }
}

/**
A generic struct wrapping the [Data] struct, with an extra type parameter `T`.
The [Entry] struct's `new()` method is responsible for creating [Directory]
and [File] instances with the given path and metadata.

Two structs [Directory] and [FileEntry] are also defined, which are used
as type parameters for [Entry]. These structs implement the `Default` trait,
which is needed for creating [Entry] instances without additional params.

This approach allows us to share the implementation of [DirectoryEntry]
trait between [Directory] and [FileEntry] without too much code duplication.

You can use the struct like this:
```rust
use statter::tree::{Directory, Entry, FileEntry};
use std::fs::{metadata, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

let root: PathBuf = PathBuf::from("/etc");
let pwfile: PathBuf = root.join("passwd");
let pwmeta: Metadata = metadata(&pwfile).ok().expect("Metadata should be returned");

let d = Entry::<Directory>::new(&root.join("systemd"), None).unwrap();
let f = Entry::<FileEntry>::new(&pwfile, Some(pwmeta.ino())).unwrap();
*/
#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entry<T>(Data, T);

impl<T: Default> Entry<T> {
    #[instrument(level = "trace")]
    pub fn new(path: &PathBuf, inode: Option<u64>) -> Result<Self, Error> {
        let inode: u64 = match inode {
            Some(i) => i,
            None => match metadata(path) {
                Ok(meta) => meta.ino(),
                Err(_) => {
                    let mut msg: String = String::from(META_FAIL);
                    msg.push_str(format!(": {}", path.display()).as_str());
                    error!(msg);
                    return Err(Error::new(ErrorKind::NotFound, META_FAIL));
                }
            },
        };
        Ok(Self(
            Data {
                inode,
                when: SecondsSinceEpoch::new(),
            },
            Default::default(), // provides the type parameter T
        ))
    }
}

/**
Implement trait [DirectoryEntry] for [Entry] struct.

Basically, this allows us to consolidate common code under trait
[DirectoryEntry] since then we can reference the inner [Data] struct there.
*/
impl<T> DirectoryEntry for Entry<T> {
    fn data(&self) -> &Data {
        &self.0
    }

    fn data_mut(&mut self) -> &mut Data {
        &mut self.0
    }
}

/// A [NodeItem] struct representing a Directory.
#[derive(Debug)]
pub struct Directory {
    name: u32,
    fd: DirFd,
    children: Children,
}

impl Directory {
    #[instrument(level = "trace")]
    pub fn new(name_idx: u32) -> Self {
        Directory {
            name: name_idx,
            ..Default::default()
        }
    }

    pub fn name<'a>(&self, store: &'a UniqueStrStore) -> &'a str {
        unsafe { store.borrow_str(self.name) }
    }

    fn name_set(&mut self, name_idx: u32) {
        self.name = name_idx;
    }

    /// Returns the [[DirFd]] for this [[Directory]] item.
    pub fn fd(&self) -> &DirFd {
        &self.fd
    }

    /**
    Set the file descriptor for this directory.

    Returns the file descriptor if it was set successfully.
    If the fd is already set, returns an error with the existing fd.
    */
    pub(super) fn fd_set(&self, fd: RawFd) -> Result<RawFd, RawFd> {
        self.fd.set(fd)
    }

    /**
    Clear the file descriptor for this directory.

    If the fd is set, we "store" the negative value of the fd.
    If the fd is already cleared (negative), we set it to 0.
    */
    fn fd_clear(&self) {
        self.fd.clear();
    }

    #[inline]
    fn children(&self) -> &Children {
        &self.children
    }

    /// Whether we have a child with the given name.
    #[inline]
    pub fn has_child(&self, name_idx: &u32) -> bool {
        self.read().contains_key(name_idx)
    }

    /// Add a child node to this item's children.
    #[instrument(level = "trace", skip(self))]
    #[inline]
    pub(super) fn add_child(&self, name_idx: u32, node: MaybeNode) {
        self.write().insert(name_idx, node);
    }

    /// Get a child node by name.
    #[inline]
    pub fn get_child(&self, name_idx: &u32) -> MaybeNode {
        self.read().get(name_idx).map(|v: &MaybeNode| v.clone())?
    }

    /// Remove a child node (or a file name entry) by name. The removal is
    /// cascading (all descendants of the child node are removed as well).
    #[instrument(level = "trace", skip(self))]
    fn remove_child(&self, name_idx: &u32) {
        self.write().remove(name_idx);
    }

    /**
    Atomically get an existing child, or add a new one built by `make`.

    The check-then-insert happens under a single write lock hold, so two
    threads racing to create the same child cannot overwrite each other
    (which would silently drop the loser's descendants). Returns the child
    and whether it was created by this call (`false` for a pre-existing
    entry, including name-only `None` entries, and when `make` declines
    by returning `None`).
    */
    pub(super) fn get_or_add_child_with<F>(&self, name_idx: u32, make: F) -> (MaybeNode, bool)
    where
        F: FnOnce() -> MaybeNode,
    {
        let mut ch = self.write();
        match ch.entry(name_idx) {
            HmEntry::Occupied(e) => (e.get().clone(), false),
            HmEntry::Vacant(v) => match make() {
                Some(node) => (v.insert(Some(node)).clone(), true),
                None => (None, false),
            },
        }
    }

    /// Atomically record a name-only child (no [[Node]] is created).
    /// Returns `true` if the name was newly added, `false` if any entry
    /// (name-only or full node) already occupied the slot.
    pub(super) fn add_name_child(&self, name_idx: u32) -> bool {
        match self.write().entry(name_idx) {
            HmEntry::Occupied(_) => false,
            HmEntry::Vacant(v) => {
                v.insert(None);
                true
            }
        }
    }

    /// Remove a name-only (`None`) child entry. Returns `true` if one was
    /// removed; full [[Node]] entries are left untouched.
    pub(super) fn remove_name_child(&self, name_idx: &u32) -> bool {
        let mut ch = self.write();
        match ch.get(name_idx) {
            Some(None) => {
                ch.remove(name_idx);
                true
            }
            _ => false,
        }
    }

    /// Add the immediate (non-recursive) memory size of the directory to [Context].
    #[cfg(feature = "size_of")]
    fn size_immediate(&self, context: &mut Context) {
        self.name.size_of_children(context);
        context.add(size_of::<DirFd>());

        let ch = self.children.read();
        ch.hasher().size_of_children(context);
        if ch.capacity() > 0 {
            let s: usize = size_of::<Option<Arc<Node>>>() + size_of::<u32>();
            let used: usize = s * ch.len();
            let total: usize = s * ch.capacity();
            context
                .add(used)
                .add_excess(total - used)
                .add_distinct_allocation();

            ch.iter().for_each(|(key, _)| {
                key.size_of_children(context);
            });
        }
    }
}

impl Default for Directory {
    fn default() -> Self {
        Directory {
            name: 0,
            fd: DirFd::default(),
            children: HashMap::with_hasher(DirTreeXxh3Hasher).into(),
        }
    }
}

impl Clone for Directory {
    /// NOTE: a clone of a [Directory] has the same [RawFd], but it may become stale.
    fn clone(&self) -> Self {
        Directory {
            name: self.name.clone(),
            fd: self.fd.clone(),
            children: self.children.read().clone().into(),
        }
    }
}

impl Hash for Directory {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        let mut children: Vec<(u32, MaybeNode)> = self
            .children
            .read()
            .iter()
            .map(|(name, child)| (name.to_owned(), child.clone()))
            .collect();
        children.sort_by_key(|(name, _)| name.clone());
        children.hash(state);
    }
}

impl PartialEq for Directory {
    fn eq(&self, other: &Self) -> bool {
        if self.name != other.name {
            // short circuit if the names don't match
            return false;
        }
        self.children.read().len() == other.children.read().len()
            && self.children.read().par_iter().all(|(name, child)| {
                other
                    .children
                    .read()
                    .get(name)
                    .map_or(false, |ov: &MaybeNode| *child == *ov)
            })
    }
}

impl Eq for Directory {}

impl Ord for Directory {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name.cmp(&other.name)
    }
}

impl PartialOrd for Directory {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Implement Deref for Directory to allow read access through RwLock to children.
impl Deref for Directory {
    type Target = Children;

    fn deref(&self) -> &Self::Target {
        &self.children
    }
}

// Implement mutable Deref for Directory to allow write access through RwLock to children.
impl DerefMut for Directory {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.children
    }
}

/// An empty struct, used as a type parameter T for [Entry].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileEntry;

impl Default for FileEntry {
    fn default() -> Self {
        FileEntry
    }
}

/* ######################################################################### */

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeType {
    Root,
    Directory,
    File,
    /// Signifies an entry with a name, but for which no Node should be created.
    Name,
    #[default]
    Uninitialized,
}

impl NodeType {
    /**
    Returns `true` if the node type is [[Directory]].

    [[Directory]]: NodeType::Directory
    */
    #[must_use]
    #[inline]
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Directory)
    }

    /**
    Returns `true` if the node type is [[File]].

    [[File]]: NodeType::File
    */
    #[must_use]
    #[inline]
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File)
    }

    /**
    Returns `true` if the node type is [[Uninitialized]].

    [[Uninitialized]]: NodeType::Uninitialized
    */
    #[must_use]
    #[inline]
    pub fn is_uninit(&self) -> bool {
        matches!(self, Self::Uninitialized)
    }

    /// Returns `true` if the node contains a [Data] struct.
    #[inline]
    pub fn has_data(&self) -> bool {
        matches!(self, Self::Directory | Self::File)
    }
}

/* ######################################################################### */

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeItem {
    Root(Directory),
    Dir(Entry<Directory>),
    File(Entry<FileEntry>),
    #[default]
    None,
}

impl NodeItem {
    /**
    Returns `true` if the node item is [[Directory]].

    [[Directory]]: NodeItem::Dir
    */
    #[must_use]
    #[inline]
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir(_))
    }

    /**
    Returns `true` if the node item is [[FileEntry]].

    [[FileEntry]]: NodeItem::File
    */
    #[must_use]
    #[inline]
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /**
    Returns `true` if the node item is [`None`].

    [`None`]: NodeItem::None
    */
    #[must_use]
    #[inline]
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Set the name of the inner [[Directory]] if the node item is [NodeItem::Dir].
    pub(super) fn set_dir_name(&mut self, name_idx: u32) {
        if let Self::Dir(v) = self {
            v.1.name_set(name_idx);
        }
    }

    /// Clear the file descriptor of the inner [[Directory]]
    /// if the node item is [NodeItem::Dir].
    fn clear_fd(&mut self) {
        if let Self::Dir(v) = self {
            v.1.fd_clear();
        }
    }

    /// Returns a ref to the inner [[Directory]] if the node item is
    /// [NodeItem::Dir] or [NodeItem::Root].
    pub fn as_dir(&self) -> Option<&Directory> {
        if self.is_file() {
            // short circuit since files are expected to outnumber
            // directories by a large margin and we can optimize for that
            return None;
        }

        // Root and Dir store the [Directory] struct differently
        // so we must handle them separately
        if let Self::Dir(v) = self {
            Some(&v.1)
        } else if let Self::Root(v) = self {
            Some(&v)
        } else {
            None
        }
    }

    /// Returns a reference to the inner [[FileEntry]] if the node item is [NodeItem::File].
    pub fn as_file(&self) -> Option<&FileEntry> {
        if let Self::File(v) = self {
            Some(&v.1)
        } else {
            None
        }
    }

    /// Returns a reference to item's [[Data]] if the node item is
    /// [NodeItem::Dir] or [NodeItem::File].
    fn data(&self) -> Option<&Data> {
        Some(match self {
            Self::Dir(d) => d.data(),
            Self::File(f) => f.data(),
            _ => return None,
        })
    }
}

// Implement `From` for converting [Directory] into `NodeItem`.
impl From<Entry<Directory>> for NodeItem {
    fn from(v: Entry<Directory>) -> Self {
        Self::Dir(v)
    }
}

// Implement `From` for converting [File] into `NodeItem`.
impl From<Entry<FileEntry>> for NodeItem {
    fn from(v: Entry<FileEntry>) -> Self {
        Self::File(v)
    }
}

/* ######################################################################### */

/// Node in the trie structure for storing paths and items, respectively.
#[derive(Debug, Clone)]
pub struct Node {
    pub node_t: NodeType,
    pub(super) item: OnceLock<NodeItem>,
    parent: Weak<Node>,
}

impl Node {
    /// Returns a new node with the given item.
    /// NOTE: children are initialized only for containers (directories and root).
    #[instrument(level = "debug")]
    pub fn new(item: NodeItem, parent: MaybeNode) -> Self {
        let node_t: NodeType = match item {
            NodeItem::Root(_) => NodeType::Root,
            NodeItem::Dir(_) => NodeType::Directory,
            NodeItem::File(_) => NodeType::File,
            NodeItem::None => NodeType::Uninitialized,
        };
        Self {
            item: match node_t {
                NodeType::Uninitialized => OnceLock::new(),
                _ => item.into(),
            },
            node_t,
            parent: parent.map_or_else(|| Weak::new(), |p: Arc<Node>| make_weak_ref(p)),
        }
    }

    #[inline]
    pub fn item(&self) -> &NodeItem {
        self.item
            .get()
            .expect("Node should have an item (but not initialized yet)")
    }

    /// Returns `true` if the node is traversable (`children` != `None`).
    #[inline]
    pub fn is_traversable(&self) -> bool {
        matches!(self.node_t, NodeType::Directory | NodeType::Root)
    }

    /// Resolve the weak reference to this node's parent node.
    #[inline]
    pub(super) fn parent(&self) -> MaybeNode {
        match self.parent.upgrade() {
            Some(parent) => parent.clone().into(),
            None => None,
        }
    }

    /**
    Construct this node's full path by walking the tree upwards to Root.

    A detached node (removed from the tree, or with an ancestor removed
    mid-walk) cannot be reconstructed; an empty Vec is returned in that
    case instead of panicking.
    */
    pub(super) fn construct_path(&self, store: &UniqueStrStore) -> Vec<String> {
        if self.node_t == NodeType::Root {
            return vec![PATH_SEP.to_string()];
        }
        let mut path: Vec<String> = Vec::new();
        let mut current: Arc<Node> = match self.parent().and_then(|p| p.get_child_byref(self)) {
            Some((_, me)) => me,
            None => {
                error!("Cannot construct path for a detached node: {self:?}");
                return path;
            }
        };

        loop {
            match current.parent() {
                Some(parent) => {
                    let name_idx: u32 = match parent.get_child_byref(&current) {
                        Some((name, _)) => name,
                        // an ancestor was detached while we were walking up
                        None => {
                            error!("Detached ancestor while constructing path: {current:?}");
                            path.clear();
                            return path;
                        }
                    };
                    path.insert(0, unsafe { store.borrow_str(name_idx) }.to_string());
                    current = parent;
                }
                None => break,
            }
        }
        path
    }

    /// Filesystem path of this node as a [PathBuf].
    pub fn path(&self, store: &UniqueStrStore) -> PathBuf {
        let mut path: PathBuf = PathBuf::from(PATH_SEP.to_string());
        for part in self.construct_path(store) {
            path.push(part);
        }
        path
    }

    /**
    For directories, the name is retrieved from the [[Directory]] struct.

    For files, the name is retrieved from parent node's `children` HashMap.

    Root node always returns `/`.
    */
    pub fn name(&self, store: &UniqueStrStore) -> Result<String, Error> {
        if self.node_t == NodeType::Directory {
            return Ok(self.as_dir().unwrap().name(store).to_string());
        }
        match self.parent() {
            Some(parent) => {
                let name_idx: u32 = parent
                    .get_child_byref(self)
                    .expect("Parent's children HashMap should contain the child node's name")
                    .0;
                Ok(unsafe { store.borrow_str(name_idx) }.to_string())
            }

            None => {
                if self.node_t == NodeType::Root {
                    return Ok(PATH_SEP.to_string());
                }
                let msg: &str = "Stale parent reference";
                error!(node = ?self, msg);
                Err(Error::new(ErrorKind::NotFound, msg))
            }
        }
    }

    /// Returns the [[DirFd]] for this node if it's a directory.
    pub fn dirfd(&self) -> Option<&DirFd> {
        self.as_dir().map(|dir: &Directory| dir.fd())
    }

    #[inline]
    pub fn children(&self) -> Option<&Children> {
        match self.item.get() {
            Some(item) => item.as_dir()?.children().into(),
            // catch uninitialized nodes
            None => None,
        }
    }

    /// Whether we have a child with the given name.
    #[inline]
    pub fn has_child(&self, name_idx: &u32) -> bool {
        self.as_dir()
            .map_or(false, |dir: &Directory| dir.has_child(name_idx))
    }

    /// Add a child [[Node]] to the current node's children.
    #[inline]
    pub(super) fn add_child(&self, name_idx: u32, node: Arc<Node>) {
        self.as_dir()
            .map(|dir: &Directory| dir.add_child(name_idx, Some(node)));
    }

    /// Get a child [[Node]] by name.
    #[inline]
    pub fn get_child(&self, name_idx: &u32) -> MaybeNode {
        self.as_dir()
            .map_or(None, |dir: &Directory| dir.get_child(name_idx))
    }

    /**
    Remove a child [[Node]] by name.

    NOTE: due to the way the tree is structured, as soon as we drop a child,
    all its descendants are also dropped in a cascading manner. This happens
    because each child is stored in an `Arc` and most likely only the parent
    has a reference to it. When the last reference to a node is dropped,
    the node is dropped as well due to refcounting.
    */
    pub(super) fn remove_child(&self, name_idx: &u32) {
        self.as_dir()
            .map(|dir: &Directory| dir.remove_child(name_idx));
    }

    /**
    Get the name of a child [[Node]] and its `Arc<Node>` ptr from a reference
    to the child node itself. The main use case is for a child node to find
    its own name and reference in the parent node's `children` HashMap.

    The lookup is by pointer identity, not structural equality: equality
    would deep-compare directory subtrees and cannot distinguish hardlinked
    files (their [Data] compares equal via the shared inode).
    */
    #[inline]
    pub(super) fn get_child_byref(&self, child: &Node) -> Option<(u32, Arc<Node>)> {
        trace_span!("get_child_byref", ?child).in_scope(|| {
            self.children()?
                .read()
                .iter()
                .find_map(|(name, c)| match c {
                    Some(n) if ptr::eq(Arc::as_ptr(n), child) => Some((*name, n.clone())),
                    _ => None,
                })
        })
    }

    /// Get the immediate (non-recursive) memory size of this node.
    #[cfg(feature = "size_of")]
    pub(super) fn size_immediate(&self) -> TotalSize {
        let mut context: Context = Context::new();
        context.add(size_of::<NodeType>());
        context.add(size_of::<Weak<Node>>());
        context.add(size_of::<OnceLock<NodeItem>>());
        if self.node_t.has_data() {
            context.add(size_of::<Data>());
        }
        if self.node_t == NodeType::Directory {
            self.as_dir().unwrap().size_immediate(&mut context);
        }
        context.total_size()
    }
}

impl Default for Node {
    fn default() -> Self {
        Node {
            node_t: NodeType::Uninitialized,
            item: OnceLock::new(),
            parent: Weak::new(),
        }
    }
}

// Implement Deref for Node to allow access to NodeItem methods.
impl Deref for Node {
    type Target = NodeItem;

    fn deref(&self) -> &Self::Target {
        self.item()
    }
}

// For a [FileEntry] or [Directory], the hash is based on the inode.
// Data-less nodes (Root, Uninitialized) hash their item instead; NOTE:
// `self.hash(state)` here would recurse into this same impl infinitely.
impl Hash for Node {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if self.node_t.has_data() {
            self.data().unwrap().hash(state);
        } else {
            self.item.get().hash(state);
        }
        self.node_t.hash(state);
    }
}

// Implement <Node> == <Node> comparisons
impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        // we could use plain `self` here due to impl Deref, but let's be explicit
        &self.item == &other.item
    }
}

// Implement <Node> == Option<Arc<Node>> comparisons
impl PartialEq<MaybeNode> for Node {
    fn eq(&self, other: &Option<Arc<Self>>) -> bool {
        match other {
            Some(other) => &self.item == &other.item,
            None => false,
        }
    }
}

// Implement Option<Arc<Node>> == <Node> comparisons
impl PartialEq<Node> for MaybeNode {
    fn eq(&self, other: &Node) -> bool {
        match self {
            Some(node) => &node.item == &other.item,
            None => false,
        }
    }
}

// Implement <Node> == <NodeItem> comparisons
impl PartialEq<NodeItem> for Node {
    fn eq(&self, other: &NodeItem) -> bool {
        &*self.item() == &*other
    }
}

// Implement <NodeItem> == <Node> comparisons
impl PartialEq<Node> for NodeItem {
    fn eq(&self, other: &Node) -> bool {
        &*self == &*other.item()
    }
}

/* ######################################################################### */

#[cfg(feature = "size_of")]
impl SizeOf for Directory {
    fn size_of_children(&self, context: &mut Context) {
        self.name.size_of_children(context);
        self.children.read().size_of_children(context);
    }
}

#[cfg(feature = "size_of")]
impl SizeOf for Entry<Directory> {
    fn size_of_children(&self, context: &mut Context) {
        self.1.size_of_children(context);
    }
}

#[cfg(feature = "size_of")]
impl SizeOf for NodeItem {
    fn size_of_children(&self, context: &mut Context) {
        match self {
            NodeItem::Root(r) => r.size_of_children(context),
            NodeItem::Dir(d) => d.size_of_children(context),
            _ => {}
        }
    }
}

#[cfg(feature = "size_of")]
impl SizeOf for Node {
    fn size_of_children(&self, context: &mut Context) {
        if let Some(item) = self.item.get() {
            item.size_of_children(context);
        };
    }
}
