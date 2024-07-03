// Copyright (c) 2024 Mikko Tanner. All rights reserved.

// non_snake_case added due to `instrument` macro causing a false positive for `dtor`
#![allow(dead_code, non_snake_case)]

use super::{make_weak_ref, path_parts, path_parts_vec, ScanState};
use crate::args::FileMode;
use crate::dirhandle::{DirHandle, EntryExt}; // OpenHandles
use crate::hashing::{DirTreeHashMap, DirTreeXxh3Hasher};
use crate::timesince::{SecondsSinceEpoch, TimeSinceEpoch};
use crossbeam::queue::SegQueue;
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
    fs::{metadata, DirEntry, Metadata},
    hash::{Hash, Hasher},
    hint,
    io::{Error, ErrorKind},
    ops::{Deref, DerefMut},
    os::unix::fs::{DirEntryExt, MetadataExt},
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, AtomicU8, Ordering::Relaxed},
        Arc, OnceLock, Weak,
    },
    thread,
    time::{Duration, Instant},
};
use tracing::{debug, error, info, instrument, trace, trace_span, warn, Level};

const PATH_SEP: char = '/';
const META_FAIL: &str = "Failed to get metadata";

// Convenience aliases
type MaybeNode = Option<Arc<Node>>;
type Children = RwLock<DirTreeHashMap<String, MaybeNode>>;
type NodeVec = Vec<Arc<Node>>;

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

/// A common trait for Directory and File entries.
///
/// Used to consolidate common code between [Directory] and [FileEntry] structs
/// (which themselves are just type placeholders for [Entry] struct).
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

/// A generic struct wrapping the [Data] struct, with an extra type parameter `T`.
/// The [Entry] struct's `new()` method is responsible for creating [Directory]
/// and [File] instances with the given path and metadata.
///
/// Two structs [Directory] and [FileEntry] are also defined, which are used
/// as type parameters for [Entry]. These structs implement the `Default` trait,
/// which is needed for creating [Entry] instances without additional params.
///
/// This approach allows us to share the implementation of [DirectoryEntry]
/// trait between [Directory] and [FileEntry] without too much code duplication.
///
/// You can use the struct like this:
/// ```rust
/// use statter::tree::{Directory, Entry, FileEntry};
/// use std::fs::{metadata, Metadata};
/// use std::os::unix::fs::MetadataExt;
/// use std::path::PathBuf;
///
/// let root: PathBuf = PathBuf::from("/etc");
/// let pwfile: PathBuf = root.join("passwd");
/// let pwmeta: Metadata = metadata(&pwfile).ok().expect("Metadata should be returned");
///
/// let d = Entry::<Directory>::new(&root.join("systemd"), None).unwrap();
/// let f = Entry::<FileEntry>::new(&pwfile, Some(pwmeta.ino())).unwrap();
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

/// Implement trait [DirectoryEntry] for [Entry] struct.
///
/// Basically, this allows us to consolidate common code under trait
/// [DirectoryEntry] since then we can reference the inner [Data] struct there.
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
    name: String,
    handle: Arc<Mutex<Option<DirHandle>>>, // libc readdir may not be thread-safe
    children: Children,
}

impl Directory {
    #[instrument(level = "trace")]
    pub fn new(name: &str) -> Self {
        Directory {
            name: name.to_owned(),
            ..Default::default()
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn set_name(&mut self, name: String) {
        self.name = name;
    }

    /// Returns the [[DirHandle]] for this [[Directory]] item.
    pub fn handle(&self) -> &Arc<Mutex<Option<DirHandle>>> {
        &self.handle
    }

    fn handle_set(&self, handle: DirHandle) {
        *self.handle.lock() = Some(handle);
    }

    fn handle_close(&self) {
        *self.handle.lock() = None;
    }

    #[inline]
    fn children(&self) -> &Children {
        &self.children
    }

    /// Whether we have a child with the given name.
    #[inline]
    pub fn has_child(&self, name: &str) -> bool {
        self.read().contains_key(name)
    }

    /// Add a child node to this item's children.
    #[instrument(level = "trace", skip(self))]
    #[inline]
    fn add_child(&self, name: &str, node: MaybeNode) {
        self.write().insert(name.to_owned(), node);
    }

    /// Get a child node by name.
    #[inline]
    pub fn get_child(&self, name: &str) -> MaybeNode {
        self.read().get(name).map(|v: &MaybeNode| v.clone())?
    }

    /// Remove a child node (or a file name entry) by name.
    #[instrument(level = "trace", skip(self))]
    fn remove_child(&self, name: &str) {
        self.write().remove(name);
    }
}

impl Default for Directory {
    fn default() -> Self {
        Directory {
            name: "".to_string(),
            handle: Arc::new(None.into()),
            children: HashMap::with_hasher(DirTreeXxh3Hasher).into(),
        }
    }
}

impl Clone for Directory {
    /// NOTE: a clone of a [Directory] does **not** clone the [DirHandle].
    fn clone(&self) -> Self {
        Directory {
            name: self.name.clone(),
            handle: self.handle.clone(), // can't clone the handle but the ptr is ok
            children: self.children.read().clone().into(),
        }
    }
}

impl Hash for Directory {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name().hash(state);
        let mut children: Vec<(String, MaybeNode)> = self
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
        self.name().cmp(&other.name())
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
    /// Returns `true` if the node type is [[Directory]].
    ///
    /// [[Directory]]: NodeType::Directory
    #[must_use]
    #[inline]
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Directory)
    }

    /// Returns `true` if the node type is [[File]].
    ///
    /// [[File]]: NodeType::File
    #[must_use]
    #[inline]
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File)
    }

    /// Returns `true` if the node type is [[Uninitialized]].
    ///
    /// [[Uninitialized]]: NodeType::Uninitialized
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
    /// Returns `true` if the node item is [[Directory]].
    ///
    /// [[Directory]]: NodeItem::Dir
    #[must_use]
    #[inline]
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir(_))
    }

    /// Returns `true` if the node item is [[FileEntry]].
    ///
    /// [[FileEntry]]: NodeItem::File
    #[must_use]
    #[inline]
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// Returns `true` if the node item is [`None`].
    ///
    /// [`None`]: NodeItem::None
    #[must_use]
    #[inline]
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Set the name of the inner [[Directory]] if the node item is [NodeItem::Dir].
    fn set_dir_name(&mut self, name: &str) {
        if let Self::Dir(v) = self {
            v.1.set_name(name.to_owned());
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
#[derive(Debug)]
pub struct Node {
    pub node_t: NodeType,
    item: Arc<OnceLock<NodeItem>>,
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
                NodeType::Uninitialized => OnceLock::new().into(),
                _ => Arc::new(item.into()),
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
    fn parent(&self) -> MaybeNode {
        match self.parent.upgrade() {
            Some(parent) => parent.clone().into(),
            None => None,
        }
    }

    /// Construct this node's full path by walking the tree upwards to Root.
    fn construct_path(&self) -> Vec<String> {
        if self.node_t == NodeType::Root {
            return vec!["/".to_string()];
        }
        let mut path: Vec<String> = Vec::new();
        let (_, mut current) = self
            .parent()
            .expect("Node should have a parent")
            .get_child_byref(self)
            .expect("Node's parent should return a name and an Arc reference");

        loop {
            match current.parent() {
                Some(parent) => {
                    path.insert(
                        0,
                        parent
                            .get_child_byref(&*current)
                            .expect("Node should have a name")
                            .0,
                    );
                    current = parent;
                }
                None => break,
            }
        }
        path
    }

    /// Filesystem path of this node as a [PathBuf].
    pub fn path(&self) -> PathBuf {
        let mut path: PathBuf = PathBuf::from("/");
        for part in self.construct_path() {
            path.push(part);
        }
        path
    }

    /// For directories, the name is retrieved from the [[Directory]] struct.
    ///
    /// For files, the name is retrieved from parent node's `children` HashMap.
    ///
    /// Root node always returns `/`.
    pub fn name(&self) -> Result<String, Error> {
        if self.node_t == NodeType::Directory {
            return Ok(self.as_dir().unwrap().name().to_string());
        }
        match self.parent() {
            Some(parent) => Ok(parent
                .get_child_byref(self)
                .expect("Parent's children HashMap should contain the child node's name")
                .0),
            None => {
                if self.node_t == NodeType::Root {
                    return Ok("/".to_string());
                }
                let msg: &str = "Stale parent reference";
                error!(node = ?self, msg);
                Err(Error::new(ErrorKind::NotFound, msg))
            }
        }
    }

    /// Returns the [[DirHandle]] for this node if it's a directory.
    pub fn handle(&self) -> Option<&Arc<Mutex<Option<DirHandle>>>> {
        self.as_dir().map(|x| x.handle())
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
    pub fn has_child(&self, name: &str) -> bool {
        self.as_dir()
            .map_or(false, |dir: &Directory| dir.has_child(name))
    }

    /// Add a child [[Node]] to the current node's children.
    #[inline]
    fn add_child(&self, name: &str, node: Arc<Node>) {
        self.as_dir()
            .map(|dir: &Directory| dir.add_child(name, Some(node)));
    }

    /// Get a child [[Node]] by name.
    #[inline]
    pub fn get_child(&self, name: &str) -> MaybeNode {
        self.as_dir()
            .map_or(None, |dir: &Directory| dir.get_child(name))
    }

    /// Remove a child [[Node]] by name.
    fn remove_child(&self, name: &str) {
        self.as_dir().map(|dir: &Directory| dir.remove_child(name));
    }

    /// Get the name of a child [[Node]] and its `Arc<Node>` ptr from a reference
    /// to the child node itself. The main use case is for a child node to find
    /// its own name and reference in the parent node's `children` HashMap.
    #[inline]
    fn get_child_byref(&self, child: &Node) -> Option<(String, Arc<Node>)> {
        trace_span!("get_child_byref", ?child).in_scope(|| {
            match self
                .children()
                .unwrap()
                .read()
                .par_iter()
                .find_any(|entry| entry.1 == child)
            {
                Some((name, child)) => Some((name.to_owned(), child.clone()?)),
                None => None,
            }
        })
    }
}

impl Default for Node {
    fn default() -> Self {
        Node {
            node_t: NodeType::Uninitialized,
            item: OnceLock::new().into(),
            parent: Weak::new(),
        }
    }
}

impl Clone for Node {
    /// Clones the [[Node]] and its children.
    fn clone(&self) -> Self {
        Self {
            node_t: self.node_t.clone(),
            item: self.item.clone(),
            parent: self.parent.clone(),
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
impl Hash for Node {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if self.node_t.has_data() {
            self.data().unwrap().hash(state);
        } else {
            self.hash(state);
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

/// The current operation being performed on the [[DirTree]].
#[derive(Default, Debug, Clone, Eq, PartialEq, Hash)]
pub enum TreeOperation {
    #[default]
    None,
    /// The tree is being built.
    Build,
    /// Scan a directory and process it into the tree.
    Scan(PathBuf),
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

/// The current state of the [[DirTree]].
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
    Active(TreeOperation),
    /// The tree is in an inconsistent state.
    Inconsistent(TreeEvent),
    /// The tree is in an error state.
    Error(TreeEvent),
    /// The tree is being torn down.
    Quitting,
}

/// A [DirTree] event. Could be an error, warning, or just a notice.
#[derive(Default, Debug, Clone, Hash, PartialEq)]
pub struct TreeEvent {
    pub msg: String,
    pub oper: Option<TreeOperation>,
    pub path: Option<String>,
    pub node: MaybeNode,
    pub when: TimeSinceEpoch,
}

impl TreeEvent {
    /// Create a new tree event with the given message. Details can be provided
    /// by chaining with the `path()`, `node()`, and `oper()` methods.
    fn new(msg: &str) -> Self {
        Self {
            msg: msg.to_string(),
            ..Default::default()
        }
    }

    /// Specify a path for the event.
    fn path(mut self, path: &str) -> Self {
        self.path = Some(path.to_owned());
        self
    }

    /// Specify a [Node] for the event.
    fn node(mut self, node: &Arc<Node>) -> Self {
        self.node = Some(node.to_owned());
        self
    }

    /// Specify a [TreeOperation] for the event.
    fn oper(mut self, oper: TreeOperation) -> Self {
        self.oper = Some(oper);
        self
    }

    /// Mark the start of an operation.
    fn op_beg(op: &TreeOperation) -> Self {
        Self {
            msg: "begin".to_string(),
            oper: Some(op.to_owned()),
            ..Default::default()
        }
    }

    /// Mark the end of an operation.
    fn op_end(op: TreeOperation) -> Self {
        Self {
            msg: "end".to_string(),
            oper: Some(op),
            ..Default::default()
        }
    }
}

/// Atomic counters for tracking the number of nodes, directories, and files.
/// Using a separate counter struct allows us to not have to lock the entire
/// tree f.ex. when inserting or removing nodes.
#[derive(Default, Debug)]
pub struct Counts {
    /// Does not include the root node.
    pub nodes: AtomicU32,
    pub dirs: AtomicU32,
    pub files: AtomicU32,
    /// Maximum depth of the tree. Root is at depth 0.
    pub depth: AtomicU8,
    /// Number of open directory handles.
    pub handles: AtomicU32,
    pub errors: AtomicU32,
}

/// Background worker thread for handling [TreeOperation]s.
fn tree_worker(t: Arc<DirTree>, state: ScanState) {
    let mut spin_ctr: u8 = 0;
    loop {
        if t.is_quitting() {
            break;
        }

        if t.workq.is_empty() {
            // wait for work in a spin loop
            while t.workq.is_empty() {
                if spin_ctr < 10 {
                    spin_ctr += 1;
                    hint::spin_loop();
                } else {
                    thread::sleep(Duration::from_micros(10));
                    spin_ctr = 0;
                    break;
                }
            }
        }

        match t.workq.pop() {
            None => {
                thread::sleep(Duration::from_millis(50));
            }
            Some(op) => {
                match op {
                    TreeOperation::Quit => {
                        t.set_state(TreeState::Quitting);
                        break;
                    }
                    TreeOperation::Scan(ref path) => {
                        let p: PathBuf = path.clone();
                        t.add_event(TreeEvent::op_beg(&op));
                        t.set_state(TreeState::Active(op));
                        t.populate(&p, true, &state);
                    }
                    TreeOperation::Remove(ref path) => {
                        let p: String = path.clone();
                        t.add_event(TreeEvent::op_beg(&op));
                        t.set_state(TreeState::Active(op.clone()));
                        t.remove(&p).err().map(|e| {
                            t.counts.errors.fetch_add(1, Relaxed);
                            t.add_event(TreeEvent::new(&e.to_string()).path(&p).oper(op));
                        });
                    }
                    TreeOperation::Update(ref _path) => {
                        //let p: PathBuf = path.clone();
                        t.add_event(TreeEvent::op_beg(&op));
                        t.set_state(TreeState::Active(op));
                        //tree.update(&p);
                    }
                    TreeOperation::Build => {}
                    TreeOperation::Insert => {}
                    TreeOperation::Serialize => {}   // TODO
                    TreeOperation::Deserialize => {} // TODO
                    _ => {}
                };
                t.set_state(TreeState::Ready);
            }
        }
    }
}

/* ######################################################################### */

/// Trie structure for storing a directory tree.
///
/// You can use it f.ex. like this:
/// ```ignore
/// use statter::args::Config;
/// use statter::tree::DirTree;
/// use statter::ScanState;
///
/// let conf: Config = Config::parse(); // parse command line args
/// let state: ScanState = ScanState::new(&conf);
/// state.start_updates(); // start the progress bars
///
/// let tree: DirTree = DirTree::new_from_path("/home", &state, true);
/// tree.print_info(); // print basic tree info (nodes, dirs, files etc)
#[derive(Default, Debug)]
pub struct DirTree {
    from: OnceLock<PathBuf>,
    counts: Arc<Counts>,
    created: SecondsSinceEpoch,
    filemode: FileMode,
    state: Arc<RwLock<TreeState>>,
    root: Arc<Node>,
    worker: OnceLock<thread::JoinHandle<()>>,
    workq: Arc<SegQueue<TreeOperation>>,
    events: Arc<RwLock<Vec<TreeEvent>>>,
}

impl DirTree {
    /// Tree root path (in the filesystem) from which the tree is built.
    /// NOTE: internally stored paths are relative to this.
    pub fn from(&self) -> &PathBuf {
        &self.from.get().expect("Tree must be initialized")
    }

    /// Returns a reference to the tree's [[Counts]] struct.
    pub fn counts(&self) -> Arc<Counts> {
        self.counts.clone()
    }

    /// Returns a reference to the tree's creation time.
    pub fn created(&self) -> &SecondsSinceEpoch {
        &self.created
    }

    /// Returns the current [[TreeState]].
    pub fn state(&self) -> TreeState {
        self.state.read().clone()
    }

    /// Set the tree to the given state. Also records the end of the previous
    /// operation if the tree was in an active state.
    #[inline]
    fn set_state(&self, state: TreeState) {
        match self.active_op() {
            Some(op) => self.add_event(TreeEvent::op_end(op)),
            _ => {}
        }
        *self.state.write() = state;
    }

    /// Record a new event in the tree's event log.
    #[inline]
    fn add_event(&self, event: TreeEvent) {
        self.events.write().push(event);
    }

    /// Add an operation to the tree's work queue.
    fn add_op(&self, op: TreeOperation) {
        self.workq.push(op);
    }

    /// Tell the background worker thread to quit.
    fn quit_worker(&self) {
        self.add_op(TreeOperation::Quit);
    }

    /// Tell the background worker thread to scan (populate) the given path.
    ///
    /// NOTE: non-blocking, the actual scan is done in the background.
    pub fn scan(&self, path: &str) {
        self.add_op(TreeOperation::Scan(PathBuf::from(path)));
    }

    /// Returns `true` if the tree is uninitialized.
    #[inline]
    pub fn is_uninit(&self) -> bool {
        matches!(self.state(), TreeState::Uninitialized)
    }

    /// Returns `true` if the tree is ready (has no active operation).
    #[inline]
    pub fn is_ready(&self) -> bool {
        matches!(self.state(), TreeState::Ready) && !self.is_quitting()
    }

    /// Returns `true` if the tree is in an active state.
    #[inline]
    pub fn is_active(&self) -> bool {
        matches!(self.state(), TreeState::Active(_))
    }

    /// Returns `true` if the tree is in a quitting state.
    #[inline]
    pub fn is_quitting(&self) -> bool {
        matches!(self.state(), TreeState::Quitting)
    }

    /// Returns `true` if the tree is in an error state.
    #[inline]
    pub fn is_error(&self) -> bool {
        matches!(self.state(), TreeState::Error(_) | TreeState::Inconsistent(_))
    }

    /// Returns the error state of the tree, if any.
    pub fn error_state(&self) -> Option<TreeEvent> {
        match self.state() {
            TreeState::Error(e) | TreeState::Inconsistent(e) => Some(e),
            _ => None,
        }
    }

    /// Returns the current active operation on the tree, if any.
    pub fn active_op(&self) -> Option<TreeOperation> {
        match self.state() {
            TreeState::Active(op) => Some(op),
            _ => None,
        }
    }

    /// Returns an Arc reference to the tree's root [[Node]].
    #[inline]
    pub fn root(&self) -> Arc<Node> {
        self.root.clone()
    }

    /// Creates a new empty directory tree (internally a Trie structure).
    pub fn new(filemode: FileMode) -> Self {
        DirTree {
            root: Node::new(NodeItem::Root(Directory::new("ROOT")), None).into(),
            filemode,
            ..Default::default()
        }
    }

    /// Set the root (filesystem) path of the tree.
    pub fn from_path(self, path: &str) -> Self {
        self.from.set(PathBuf::from(path)).ok();
        self.insert(self.from(), NodeType::Directory, None);
        self.set_state(TreeState::Empty);
        self
    }

    /// Build a new [[DirTree]] with the given options and start the worker thread.
    ///
    /// NOTE: must be chained with `from_path()` to set the root path.
    pub fn build(self, state: &ScanState) -> Arc<DirTree> {
        if self.from.get().is_none() {
            panic!("Root path must be set before building the tree");
        }
        let tree: Arc<DirTree> = self.into();
        let tree_c: Arc<DirTree> = tree.clone();
        let state: ScanState = state.clone();
        let worker: thread::JoinHandle<()> = thread::Builder::new()
            .name("tree_worker".into())
            .spawn(|| tree_worker(tree_c, state))
            .expect("Failed to start DirTree worker thread");
        tree.worker.set(worker).ok();
        tree
    }

    /// Creates a new [[DirTree]] with the given path as root.
    ///
    /// If `recursive` is true, also populates the tree by recursively walking
    /// the full directory structure (starting from from the given directory)
    /// and inserting each found path into the tree.
    #[instrument(name = "DirTree", skip_all)]
    pub fn new_from_path(path: &str, state: &ScanState, recursive: bool) -> Self {
        debug!(target: "path", "{path}");
        let tree: DirTree = Self::new(state.filemode).from_path(path);
        /*
        Technically we've not yet scanned the root directory, but this place
        is the most logical one to do the increment to keep the counter in
        sync as adding more logic to `populate*()` methods would be counter-
        productive. Besides, this counter is only for display for now.
        */
        state.num_d.inc1();
        debug!(target: "TREE", "{tree:?}");
        if recursive {
            tree.set_state(TreeState::Active(TreeOperation::Build));
            tree.add_event(TreeEvent::op_beg(&TreeOperation::Build).path(path));
            match state.sync {
                false => tree.populate_par(tree.from(), true, state),
                true => tree.populate(tree.from(), true, state),
            }
        };
        tree.set_state(TreeState::Ready);
        tree
    }

    /// Populate a leaf [[Node]] in the trie with the contents of a directory.
    /// Uses the standard [std::fs::read_dir] method to get the directory entries.
    ///
    /// NOTE: single threaded, potentially slow with large directory trees.
    #[instrument(level = "debug", skip_all, fields(p = path.strip_prefix(self.from()).ok().unwrap().to_str()))]
    pub fn populate(&self, path: &PathBuf, recursive: bool, state: &ScanState) {
        trace!(target: "get_entries", "{}", path.display());
        match path.read_dir().ok() {
            Some(entries) => {
                entries.filter_map(Result::ok).for_each(|entry: DirEntry| {
                    let path: PathBuf = entry.path();
                    match entry.file_type() {
                        Ok(entry_t) => {
                            trace!(target: "DirEntry", "{}", path.display());
                            if entry_t.is_dir() {
                                self.insert(&path, NodeType::Directory, Some(entry.ino()));
                                state.num_d.inc1();
                                if recursive {
                                    //rayon::spawn(move || self.populate(&path, recursive, state));
                                    self.populate(&path, recursive, state);
                                }
                            } else if entry_t.is_file() {
                                if self.filemode.is_with_size() {
                                    //FIXME: add error handling
                                    state.fsize.fetch_add(entry.metadata().ok().unwrap().len());
                                }
                                if self.filemode.is_name() {
                                    self.insert(&path, NodeType::Name, None);
                                } else if self.filemode.is_node() {
                                    self.insert(&path, NodeType::File, Some(entry.ino()));
                                }
                                state.num_f.inc1();
                            }
                        }
                        Err(e) => {
                            self.counts.errors.fetch_add(1, Relaxed);
                            debug!("Error with {}: {e}", path.display());
                        }
                    }
                })
            }
            None => return,
        };
    }

    /// Parallel version of [DirTree::populate] using [rayon::iter]
    /// to process each dir entry in parallel.
    ///
    /// Uses [[DirHandle]] to read the directory entries, and its [DirHandle::iter]
    /// method, which tries to return the directory entries first using a small
    /// buffer to look ahead in the directory stream.
    #[instrument(level = "debug", skip_all, fields(p = path.strip_prefix(self.from()).ok().unwrap().to_str()))]
    pub fn populate_par(&self, path: &PathBuf, recursive: bool, state: &ScanState) {
        trace!(target: "DirHandle", "{:?}", path.display());
        match DirHandle::new(path) {
            Ok(mut handle) => {
                handle.iter().par_bridge().for_each(|entry: EntryExt| {
                    let entry_p: PathBuf = path.join(entry.name());
                    match entry.file_type() {
                        Some(_) => {
                            trace!(target: "ENTRY", "{:?} : {:?}", entry.name(), entry);
                            if entry.is_dir() {
                                self.insert(&entry_p, NodeType::Directory, Some(entry.ino()));
                                state.num_d.inc1();
                                if recursive {
                                    self.populate_par(&entry_p, recursive, state);
                                }
                            } else if entry.is_file() {
                                if self.filemode.is_with_size() {
                                    state.fsize.fetch_add(entry.len());
                                }
                                if self.filemode.is_name() {
                                    self.insert(&entry_p, NodeType::Name, None);
                                } else if self.filemode.is_node() {
                                    self.insert(&entry_p, NodeType::File, Some(entry.ino()));
                                }
                                state.num_f.inc1();
                            }
                        }
                        None => {
                            debug!(target: "WARN", "Unknown entry type: {}", entry_p.display());
                        }
                    }
                    drop(entry);
                });

                #[cfg(debug_assertions)]
                {
                    let cur = handle.state_current();
                    let old = handle.state();
                    debug!(target: "HANDLE_STATE", "equal: {}", old == &cur);
                    debug!(target: "HANDLE_STATE", "old: {old:?}");
                    debug!(target: "HANDLE_STATE", "cur: {cur:?}");
                } // END DEBUG -- TODO: remove

                self.add_handle(path, handle);
            }
            Err(e) => {
                self.counts.errors.fetch_add(1, Relaxed);
                debug!(target: "ERROR", "Cannot read directory: {}", e);
            }
        }
    }

    /// Add a [[DirHandle]] object to a directory node's [[Directory]] item.
    pub fn add_handle(&self, path: &PathBuf, handle: DirHandle) {
        self.get_node(path.to_string_lossy().as_ref()).map(|node| {
            node.as_dir().map(|dir| {
                dir.handle_set(handle);
                self.counts.handles.fetch_add(1, Relaxed);
            })
        });
    }

    /* --------------------------------- */

    /// Inserts a path into the trie. The path must be an absolute filesystem path.
    /// The path is split on forward slash (`/`) and the first empty string discarded.
    #[instrument(level = "debug", skip(self))]
    pub fn insert(&self, path: &PathBuf, node_t: NodeType, inode: Option<u64>) {
        let mut current: Arc<Node> = self.root();
        let p_unicode = path.to_string_lossy();
        let parts: Vec<&str> = path_parts_vec(&p_unicode);
        let len: usize = parts.len();
        let mut depth: usize = 0; // root node is at depth 0
        self.counts.depth.fetch_max(len as u8, Relaxed);
        // max depth can just as well be updated at this point

        for part in parts {
            depth += 1;
            if !current.has_child(part) {
                trace!(target: "CURRENT_NODE", "{:?}", &current.name().ok().unwrap());
                /*
                we don't have an item for this node yet, hence NodeItem::None
                also node_t must be set here since later the Node will be in
                an Arc and we can't change that field anymore
                */
                let mut new: Node = Node::new(NodeItem::None, Some(current.clone()));
                if depth < len {
                    trace!(target: "intermediate", "{part:?} --> depth: {depth}/{len}");
                    // must be a container (directory) so let's create the basic structure
                    new.node_t = NodeType::Directory;
                    let mut itm = NodeItem::Dir(Entry::<Directory>::default());
                    itm.set_dir_name(part);
                    new.item.set(itm).ok();
                    /*
                    we must increment the node counters here since we've
                    not reached the leaf node yet and we shouldn't do a
                    full initialization for an intermediate node
                    */
                    self.counts.nodes.fetch_add(1, Relaxed);
                    self.counts.dirs.fetch_add(1, Relaxed);
                } else {
                    new.node_t = node_t.clone();
                    if node_t == NodeType::Directory {
                        let mut itm = NodeItem::Dir(Entry::<Directory>::new(path, inode).unwrap());
                        itm.set_dir_name(part);
                        new.item.set(itm).ok();
                        self.counts.nodes.fetch_add(1, Relaxed);
                        self.counts.dirs.fetch_add(1, Relaxed);
                    } else if node_t == NodeType::Name {
                        /*
                        optimization: don't create file Nodes at all, just
                        record the fact that a file exists in the directory
                        NOTE: total node count is not incremented in this case
                        */
                        drop(new);
                        current.as_dir().map(|dir| dir.add_child(part, None));
                        self.counts.files.fetch_add(1, Relaxed);
                        debug!(target: "FILENAME_ADD", "{part:?} (store name only)");
                        return;
                    }
                }
                debug!(target: "CREATED_NODE", "{part:?} : {:?}", &new);
                current.add_child(part, new.into());
            }

            // we've reached the bottom of the path -> grab the leaf node
            current = match current.get_child(part) {
                Some(node) => node,
                None => {
                    if current.has_child(part) && node_t == NodeType::Name {
                        // this is a file name entry, so None is expected
                        return;
                    } else {
                        // should never happen
                        error!("Child {part:?} missing from HashMap: {current:?}");
                        return;
                    }
                }
            }
        }

        if !matches!(current.item.get(), None) {
            // for now we don't overwrite existing nodes, but
            // this may change in the future to allow for updates
            trace!(target: "SKIP_EX_NODE", "{:?} ({:?}) : {:?}", current.name().ok().unwrap(), current.node_t, current.path());
            return;
        }

        match node_t {
            NodeType::Directory => {
                let mut itm = NodeItem::Dir(Entry::<Directory>::new(path, inode).unwrap());
                itm.set_dir_name(&path.file_name().unwrap().to_string_lossy());
                current.item.set(itm).ok();
                self.counts.nodes.fetch_add(1, Relaxed);
                self.counts.dirs.fetch_add(1, Relaxed);
            }
            NodeType::File => {
                current
                    .item
                    .set(NodeItem::File(Entry::<FileEntry>::new(path, inode).unwrap()))
                    .ok();
                self.counts.nodes.fetch_add(1, Relaxed);
                self.counts.files.fetch_add(1, Relaxed);
            }
            _ => return,
        };
        debug!(target: "INSERT_CHILD", "{current:?}");
    }

    /// Remove a [[Node]] (or a leaf) from the trie. Expects an absolute path.
    ///
    /// Returns a tuple of `(nodes, dirs, files)` removed on success and [[None]]
    /// if the path was not found. The root node cannot be removed.
    ///
    /// WARNING: implementation is WIP and may yet contain bugs.
    #[instrument(level = "debug", skip(self))]
    pub fn remove(&self, path: &str) -> Result<Option<(u32, u32, u32)>, Error> {
        match self.get_node(path) {
            Some(node) => {
                if node.node_t == NodeType::Root {
                    let msg: &str = "Cannot remove root node";
                    error!(msg);
                    return Err(Error::new(ErrorKind::InvalidInput, msg));
                };

                match node.parent() {
                    Some(parent) => {
                        let (nodes, dirs, files) = self.count_from(node.clone());
                        let (name, c) = parent.get_child_byref(&node).unwrap();
                        debug!(target: "REMOVE_NODE", "{:?}", node.path().display());
                        assert_eq!(node, c, "Node should be the same as the one in parent");

                        parent.remove_child(&name);
                        self.counts.nodes.fetch_sub(nodes, Relaxed);
                        self.counts.dirs.fetch_sub(dirs, Relaxed);
                        self.counts.files.fetch_sub(files, Relaxed);
                        return Ok(Some((nodes, dirs, files)));
                    }

                    None => {
                        let msg: String = format!("Stale parent reference: {:?}", node.path());
                        error!(msg);
                        return Err(Error::new(ErrorKind::NotFound, msg));
                    }
                }
            }

            None => {
                warn!("Node not found: {path:?}");
                return Ok(None);
            }
        }
    }

    /* --------------------------------- */

    /// Get a [[Node]] from the trie. Expects an absolute path.
    pub fn get_node(&self, path: &str) -> MaybeNode {
        // short circuit if the path is not absolute or does not look like a path
        if !path.starts_with(PATH_SEP) || !path.contains(PATH_SEP) {
            return None;
        }
        let mut current: Arc<Node> = self.root();
        for part in path_parts(path) {
            match current.get_child(part) {
                Some(node) => current = node,
                None => return None,
            }
        }
        Some(current) // found the node
    }

    /// Checks if a given path exists in the trie. Expects an absolute path.
    pub fn contains(&self, path: &str) -> bool {
        self.get_node(path).is_some()
    }

    /// The filesystem path of a [[Node]], if it contains a file or directory.
    pub fn fs_path(&self, node: Arc<Node>) -> Option<PathBuf> {
        match node.node_t.has_data() {
            true => Some(node.path()),
            false => None,
        }
    }

    /* --------------------------------- */

    /// Walk the tree recursively from a [[Node]] and return a [Vec] of child nodes. The
    /// `dirs` and `files` flags control whether to include directory and/or file nodes.
    fn walk(&self, node: &Arc<Node>, dirs: bool, files: bool) -> Arc<Mutex<NodeVec>> {
        let result: Arc<Mutex<NodeVec>> = Mutex::new(Vec::new()).into();
        if node.is_traversable() && node.children().is_some() {
            node.children()
                .unwrap()
                .read()
                .values()
                .par_bridge()
                .for_each(|c: &MaybeNode| {
                    c.as_ref().map(|child: &Arc<Node>| {
                        let mut nodes_shard: NodeVec = Vec::new();
                        if dirs && child.node_t.is_dir() {
                            nodes_shard.push(child.clone());
                        } else if files && child.node_t.is_file() {
                            nodes_shard.push(child.clone());
                        }
                        if child.is_traversable() {
                            nodes_shard
                                .extend(self.walk(child, dirs, files).lock().iter().cloned());
                        }
                        result.lock().extend(nodes_shard);
                    });
                });
        }
        result
    }

    /// Walks the full tree and returns a [Vec] of all nodes. WARNING: this can be
    /// slow and memory intensive for large trees. Prefer using [DirTree::iter].
    pub fn nodes(&self) -> NodeVec {
        trace_span!("walk:nodes").in_scope(|| self.walk(&self.root(), true, true).lock().to_vec())
    }

    /// Returns a [Vec] of all [[Directory]] nodes in the tree.
    pub fn dirs(&self) -> NodeVec {
        trace_span!("walk:dirs").in_scope(|| self.walk(&self.root(), true, false).lock().to_vec())
    }

    /// Returns a [Vec] of all [[FileEntry]] nodes in the tree.
    pub fn files(&self) -> NodeVec {
        trace_span!("walk:files").in_scope(|| self.walk(&self.root(), false, true).lock().to_vec())
    }

    /* --------------------------------- */

    /// Creates an iterator to iterate through the tree starting from a [[Node]].
    /// The iterator is depth-first and includes the starting node.
    #[instrument(level = "trace", skip(self))]
    pub fn iter_from(&self, node: Arc<Node>) -> DirTreeIterator {
        DirTreeIterator(VecDeque::from(vec![node]))
    }

    /// Creates an iterator to walk through all [[Node]]s in the tree.
    pub fn iter(&self) -> DirTreeIterator {
        self.iter_from(self.root())
    }

    /// An iterator over all [[Directory]] items in the tree.
    pub fn iter_dirs(&self) -> impl Iterator<Item = Directory> {
        self.iter()
            .filter_map(|node: Arc<Node>| node.as_dir().cloned())
    }

    /// An iterator over all [[FileEntry]] items in the tree.
    pub fn iter_files(&self) -> impl Iterator<Item = FileEntry> {
        self.iter()
            .filter_map(|node: Arc<Node>| node.as_file().cloned())
    }

    /// An iterator over all Paths in the tree.
    pub fn iter_paths(&self) -> impl Iterator<Item = String> + '_ {
        self.iter()
            .filter_map(|node: Arc<Node>| self.fs_path(node))
            .map(|p| p.to_string_lossy().to_string())
    }

    /// Count the number of directory and file nodes by iterating from a [[Node]].
    /// Also counts the starting node. Returns a tuple of `(nodes, dirs, files)`.
    pub fn iter_count_from(&self, node: Arc<Node>) -> (u32, u32, u32) {
        if node.node_t.is_file() {
            return (1, 0, 1);
        }

        let mut nodes: u32 = 0;
        let mut dirs: u32 = 0;
        let mut files: u32 = 0;
        /*
        NOTE: trying to convert this iterating closure to a parallel
        one with Rayon's `par_bridge()` makes the counting almost 5x slower.
        This is much more than the slowdown observed with `count_from()`,
        and I have no good explanation for it at this point.
        */
        self.iter_from(node).for_each(|node: Arc<Node>| {
            nodes += 1;
            if node.node_t.is_dir() {
                dirs += 1;
            } else if node.node_t.is_file() {
                files += 1;
            }
        });
        (nodes, dirs, files)
    }

    /// Count the number of directory and file nodes by iterating the whole tree.
    /// Does not count the root [[Node]]. Returns a tuple of `(nodes, dirs, files)`.
    pub fn iter_count(&self) -> (u32, u32, u32) {
        let (mut nodes, dirs, files) = self.iter_count_from(self.root());
        nodes -= 1; // remove root node since we started from it
        (nodes, dirs, files)
    }

    /* --------------------------------- */

    /// Traverses recursively from a [[Node]] and applies function `f` to each
    /// child node, AND the starting node itself. Parallel version.
    ///
    /// In contrast to `traverse_from()`, this function requires that the
    /// fn `f` is `Send` and `Sync` since it will be sent to other threads.
    ///
    /// Basically, to make this work you must use Atomic types or other thread-safe
    /// primitives ([Mutex], [RwLock], [AtomicCell] etc) for any variables in `f`.
    /// IOW, no interior mutability or shared mutable state.
    ///
    /// Testing shows that this traversal is slower than the sequential version
    /// for `f` which do just a simple operation on each node. This makes sense
    /// since the overhead of moving stuff between threads can be significant.
    pub fn traverse_par<F>(&self, node: &Arc<Node>, f: &F)
    where
        F: Fn(&Arc<Node>) + Send + Sync,
    {
        trace!(target: "traverse_par", "{}", node.path().display());
        f(node);
        if node.is_traversable() && node.children().is_some() {
            node.children()
                .unwrap()
                .read()
                .values()
                .par_bridge()
                .for_each(|c: &MaybeNode| {
                    c.as_ref().map(|child: &Arc<Node>| {
                        if child.is_traversable() {
                            // traverse directories first (depth-first search)
                            self.traverse_par(child, f);
                        } else {
                            f(child);
                        }
                    });
                });
        }
    }

    /// Traverses recursively from a [[Node]] and applies function `f` to each
    /// child node, AND the starting node itself.
    pub fn traverse_from<F>(&self, node: &Arc<Node>, f: &mut F)
    where
        F: FnMut(&Arc<Node>),
    {
        trace!(target: "traverse_from", "{}", node.path().display());
        f(node);
        if node.is_traversable() && node.children().is_some() {
            node.children()
                .unwrap()
                .read()
                .values()
                .for_each(|c: &MaybeNode| {
                    c.as_ref().map(|child: &Arc<Node>| {
                        if child.is_traversable() {
                            // traverse directories first (depth-first search)
                            self.traverse_from(child, f);
                        } else {
                            f(child);
                        }
                    });
                });
        }
    }

    /// Traverses the tree from root and applies function `f` to each [[Node]].
    pub fn traverse<F>(&self, mut f: F)
    where
        F: FnMut(&Arc<Node>),
    {
        self.traverse_from(&self.root(), &mut f);
    }

    /// Count the number of directory and file nodes with `traverse()`. Also counts
    /// the starting [[Node]] (except root). Returns a tuple of `(nodes, dirs, files)`.
    pub fn count_from(&self, node: Arc<Node>) -> (u32, u32, u32) {
        if node.node_t.is_file() {
            return (1, 0, 1);
        }

        let mut nodes: u32 = 0;
        let mut files: u32 = 0;
        let mut dirs: u32 = 0;
        /*
        NOTE: trying to convert this iterating closure to a parallel
        one with `traverse_par()` makes the counting almost 50% slower.
        Likely the overhead from moving stuff between threads and having
        to use Atomic versions of counters is the main reason.
        */
        self.traverse_from(&node, &mut |n: &Arc<Node>| {
            nodes += 1;
            match n.node_t {
                NodeType::Directory => dirs += 1,
                NodeType::File => files += 1,
                _ => (),
            }
        });

        if node.node_t == NodeType::Root {
            nodes -= 1; // remove root node if we started from it
        };
        (nodes, dirs, files)
    }

    /* --------------------------------- */

    /// Print the tree's info (nodes, dirs, files, depth, ctime) to stderr.
    pub fn print_info(&self) {
        eprintln!(
            "Tree info : nodes {}, dirs {}, files {}, depth {}, ctime {} UTC",
            self.counts().nodes.load(Relaxed),
            self.counts().dirs.load(Relaxed),
            self.counts().files.load(Relaxed),
            self.counts().depth.load(Relaxed),
            self.created(),
        );
    }

    /// Print the full contents of the tree recursively. This is a debugging function.
    pub fn print_debug(&self) {
        eprintln!("\n{:?}\n", self);
        self.traverse(|node: &Arc<Node>| {
            if node.node_t.has_data() {
                if tracing::level_enabled!(Level::DEBUG) {
                    debug!("{:?}", node.construct_path());
                } else {
                    info!("{}", node.path().to_string_lossy());
                }
            }
        });
    }

    /// Validate the counts of nodes, dirs, and files in the tree.
    ///
    /// We take the counts from the tree's [[Counts]] struct as master data and
    /// firstly validate that the counts of directories and files add up to the
    /// total number of nodes. Then we compare those to the counts we get by
    /// traversing the tree with:
    /// - `count_from()` (`traverse_from()` -> count)
    /// - `iter_count()` (`iter()` -> count)
    /// - `dirs().len()` and `files().len()` (`walk()` -> count)
    ///
    /// We will also print the time it took to count the nodes using each method.
    ///
    /// This is a debugging function using asserts, hence it will panic if the
    /// counts do not match.
    pub fn validate_counts(&self) {
        let want_n: u32 = self.counts.nodes.load(Relaxed);
        let want_d: u32 = self.counts.dirs.load(Relaxed);
        let want_f: u32 = self.counts.files.load(Relaxed);
        let d_o: &str = "[dirsonly]";
        if self.filemode.is_name() {
            assert_eq!(want_n, want_d, "master node count != dirs {d_o}")
        } else {
            assert_eq!(want_n, want_d + want_f, "master node count != dirs+files")
        }

        /* ------------------------- */

        let start: Instant = Instant::now();
        let (nodes, dirs, files) = self.count_from(self.root());
        let n: &str = "count_from()";
        if self.filemode.is_name() {
            assert_eq!(nodes, dirs, "{n} node count != dirs {d_o}");
            assert_eq!(files, 0, "{n} files != 0 {d_o}");
        } else {
            assert_eq!(nodes, dirs + files, "{n} node count != dirs+files");
            assert_eq!(want_f, files, "{n} files do not match");
        }
        assert_eq!(want_n, nodes, "{n} node count != master count");
        assert_eq!(want_d, dirs, "{n} dirs do not match");
        eprintln!(" --> {n} = {:?}", start.elapsed());

        /* ------------------------- */

        let start: Instant = Instant::now();
        let (nodes, dirs, files) = self.iter_count();
        let n: &str = "iter_count()";
        if self.filemode.is_name() {
            assert_eq!(nodes, dirs, "{n} node count != dirs {d_o}");
            assert_eq!(files, 0, "{n} files != 0 {d_o}");
        } else {
            assert_eq!(nodes, dirs + files, "{n} node count != dirs+files");
            assert_eq!(want_f, files, "{n} files do not match");
        }
        assert_eq!(want_n, nodes, "{n} node count != master count");
        assert_eq!(want_d, dirs, "{n} dirs do not match");
        eprintln!(" --> {n} = {:?}", start.elapsed());

        /* ------------------------- */

        let n: &str = "walk()";
        let start: Instant = Instant::now();
        let dirs: u32 = self.dirs().len() as u32;
        eprintln!(" --> {n} dirs  = {:?}", start.elapsed());

        let start: Instant = Instant::now();
        let files: u32 = self.files().len() as u32;
        eprintln!(" --> {n} files = {:#?}", start.elapsed());

        assert_eq!(want_d, dirs, "{n} dirs do not match");
        if self.filemode.is_name() {
            assert_eq!(files, 0, "{n} files != 0 {d_o}");
        } else {
            assert_eq!(want_f, files, "{n} files do not match");
        }
    }
}

/* ######################################################################### */

/// Iterator for walking through a [[DirTree]].
pub struct DirTreeIterator(VecDeque<Arc<Node>>);

impl Iterator for DirTreeIterator {
    type Item = Arc<Node>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop_front().map(|node: Arc<Node>| {
            trace!(target: "DirTreeIterator", "{}", node.path().display());
            if node.children().is_none() {
                return node;
            }

            // Push all found children to the stack
            node.children()
                .unwrap()
                .read()
                .values()
                .for_each(|c: &MaybeNode| {
                    c.as_ref().map(|child: &Arc<Node>| {
                        if child.is_traversable() {
                            // push directories to the front of the queue...
                            self.0.push_front(child.clone());
                        } else {
                            // ...and files to the back
                            self.0.push_back(child.clone());
                        }
                    });
                });
            node
        })
    }
}

/* ######################################################################### */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{testdirs::create_test_dirs, Config, ScanState};
    use ctor::dtor;
    use nix::libc;
    use parking_lot::Mutex;
    use std::collections::HashSet;
    use tempfile::TempDir;

    const TEST_NUM: [u64; 3] = [9, 11, 7];
    const EXP_DIRS: u32 = (TEST_NUM[0] + TEST_NUM[0] * TEST_NUM[1]) as u32;
    const EXP_FILES: u32 = (TEST_NUM[0] * TEST_NUM[1] * TEST_NUM[2] + TEST_NUM[0] + 1) as u32;
    const EXP_NODES: u32 = EXP_DIRS + EXP_FILES;

    // statics for all tests
    static mut CONF: Option<Config> = None;
    static mut STATE: Option<ScanState> = None;
    static mut TESTDIR: Option<TempDir> = None;
    static INITIALIZED: Mutex<bool> = Mutex::new(false);

    /// Setup common test environment for all tests. Will initialize
    /// the needed statics only once (due to the Mutex).
    unsafe fn setup_tests() {
        let mut init = INITIALIZED.lock();
        if *init {
            // already initialized
            return;
        }
        CONF = Some(Config::default());
        STATE = Some(ScanState {
            filemode: FileMode::NODE,
            ..Default::default()
        });
        TESTDIR = Some(create_test_dirs_for_tree_test());
        *init = true;
    }

    #[dtor]
    unsafe fn teardown() {
        // println! or eprintln! in `dtor` will panic as Rust has already
        // shut down certain facilities. We can use libc::printf instead.
        libc::printf("*** DirTree tests done, tearing down ***\n\0".as_ptr() as *const i8);
        if let Some(_) = TESTDIR {
            libc::printf(" - Deleting temp directory...\n\0".as_ptr() as *const i8);
            let temp: TempDir = TESTDIR.take().unwrap();
            temp.close().unwrap();
        }
        libc::printf("*** Teardown finished ***\n\n\0".as_ptr() as *const i8);
    }

    /* --------------------------------- */

    #[test]
    fn test_create_empty_tree() {
        unsafe { setup_tests() }
        let tree: DirTree = DirTree::new(FileMode::default());
        let (nodes, dirs, files, depth) = counts(&tree);

        assert_eq!(tree.root.node_t, NodeType::Root);
        assert_eq!(tree.from, OnceLock::default());
        assert_eq!(tree.state(), TreeState::Uninitialized);
        assert!(tree.is_uninit(), "Tree is not uninitialized");
        tree.validate_counts();
        assert_eq!(nodes, 0, "nodes mismatch");
        assert_eq!(dirs, 0, "dirs mismatch");
        assert_eq!(files, 0, "files mismatch");
        assert_eq!(depth, 0, "depth mismatch");
        tree.from.set(PathBuf::from("/foo")).ok();
        assert_eq!(tree.from(), &PathBuf::from("/foo"));
    }

    #[test]
    fn test_tree_new_from_path() {
        unsafe { setup_tests() }
        let (path, _) = unsafe {
            (TESTDIR.as_ref().unwrap().path().to_str().unwrap(), STATE.as_ref().unwrap())
        };

        let tree: DirTree = DirTree::new(FileMode::NODE).from_path(path);
        let (nodes, dirs, files, depth) = counts(&tree);
        let root_depth: u8 = (path.split(PATH_SEP).count() - 1) as u8;

        tree.validate_counts();
        assert_eq!(tree.state(), TreeState::Empty);
        assert_eq!(nodes, root_depth.into(), "nodes mismatch");
        assert_eq!(dirs, root_depth.into(), "dirs mismatch");
        assert_eq!(files, 0, "files mismatch");
        assert_eq!(depth, root_depth, "depth mismatch");
    }

    #[test]
    fn test_tree_new_from_path_recursive() {
        let (_, tree, root_depth) = create_test_tree(true);
        let (nodes, dirs, files, depth) = counts(&tree);
        check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);
    }

    #[test]
    fn test_tree_build_thread() {
        unsafe { setup_tests() }
        let (path, state) = unsafe {
            (TESTDIR.as_ref().unwrap().path().to_str().unwrap(), STATE.as_ref().unwrap())
        };

        let tree: Arc<DirTree> = DirTree::new(FileMode::NODE).from_path(path).build(state);
        assert!(tree.worker.get().is_some(), "Worker not initialized");

        tree.scan(path); // non-blocking, so we must wait for it to finish
        while !tree.is_ready() {
            std::thread::sleep(Duration::from_millis(10));
        }

        tree.validate_counts();
        let (nodes, dirs, files, depth) = counts(&tree);
        let root_depth: u8 = (path.split(PATH_SEP).count() - 1) as u8;
        check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);
        tree.quit_worker();
    }

    #[test]
    fn test_tree_contains() {
        let (path, tree, root_depth) = create_test_tree(true);
        let (nodes, dirs, files, depth) = counts(&tree);
        check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);

        let mut ctr: u32 = 0;
        for p in path_generator(path) {
            assert!(tree.contains(&p), "Not found: {}", p);
            ctr += 1;
        }
        assert!(tree.contains(&path), "Root not found: {}", path);
        assert!(!tree.contains(""), "Found an empty path");
        assert_eq!(ctr, EXP_DIRS + EXP_FILES, "All paths not accounted for");
        for &p in ["foo", "bar/foo", "/foo/baz", ".", "..", "../"].iter() {
            assert!(!tree.contains(p), "Found a nonexistent path: {p}");
        }
    }

    #[test]
    fn test_tree_traversals() {
        let (path, tree, root_depth) = create_test_tree(true);
        let (nodes, dirs, files, depth) = counts(&tree);
        check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);

        let exp: HashSet<String> = path_generator(path);
        let p_iter: HashSet<String> = tree.iter_paths().collect();
        let p_walk: HashSet<String> = tree
            .nodes()
            .iter()
            .map(|n: &Arc<Node>| n.path().to_string_lossy().to_string())
            .collect();
        let mut p_trav: HashSet<String> = HashSet::new();
        tree.traverse(|n: &Arc<Node>| {
            p_trav.insert(n.path().to_string_lossy().to_string());
        });

        assert!(exp.is_subset(&p_iter), "iter() paths mismatch: {:?}", exp.difference(&p_iter));
        assert!(exp.is_subset(&p_walk), "walk() paths mismatch: {:?}", exp.difference(&p_walk));
        assert!(exp.is_subset(&p_trav), "traverse() paths mismatch: {:?}", exp.difference(&p_trav));
    }

    #[test]
    fn test_tree_subcounts() {
        let (path, tree, root_depth) = create_test_tree(true);
        let (nodes, dirs, files, depth) = counts(&tree);
        check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);

        let mut l1_dirs: HashSet<String> = HashSet::new();
        let mut l2_dirs: HashSet<String> = HashSet::new();
        for l1_idx in 0..TEST_NUM[0] {
            l1_dirs.insert(format!("{}/level_1_{l1_idx}", path));
            for l2_idx in 0..TEST_NUM[1] {
                l2_dirs.insert(format!("{}/level_1_{l1_idx}/level_2_{l2_idx}", path));
            }
        }

        let (exp_l1_num, exp_l2_num) = (TEST_NUM[0], TEST_NUM[0] * TEST_NUM[1]);
        assert_eq!(l1_dirs.len(), exp_l1_num as usize, "L1 dirs num mismatch (test error)");
        assert_eq!(l2_dirs.len(), exp_l2_num as usize, "L2 dirs num mismatch (test error)");

        for p in l1_dirs.iter() {
            let node: Arc<Node> = tree.get_node(p).expect("get_node() should return a node");
            let (l1_n, l1_d, l1_f) = validate_counts_below_node(&tree, node);

            let dirs_exp: u64 = TEST_NUM[1] + 1; // +1 for the dir itself
            let files_exp: u64 = TEST_NUM[1] * TEST_NUM[2] + 1; // +1 for the extra test file in L1
            let nodes_exp: u64 = dirs_exp + files_exp;

            assert_eq!(l1_d, dirs_exp, "L1 dir count != expected");
            assert_eq!(l1_f, files_exp, "L1 file count != expected");
            assert_eq!(l1_n, nodes_exp, "L1 node count != expected");
        }

        for p in l2_dirs.iter() {
            let node: Arc<Node> = tree.get_node(p).expect("get_node() should return a node");
            let (l2_n, l2_d, l2_f) = validate_counts_below_node(&tree, node);

            assert_eq!(l2_d, 1, "L2 dir count != expected");
            assert_eq!(l2_f, TEST_NUM[2], "L2 file count != expected");
            assert_eq!(l2_n, TEST_NUM[2] + 1, "L2 node count != expected");
        }
    }

    #[test]
    fn test_tree_removals() {
        let (path, tree, root_depth) = create_test_tree(true);
        let (mut nodes, mut dirs, mut files, depth) = counts(&tree);
        check_nodes_dirs_files(nodes, root_depth, dirs, files, depth);

        let l1_idx: u64 = TEST_NUM[0] - 1;
        let file: String = format!("{path}/level_1_{0}/file-{0}.bin", l1_idx);
        let l2_p: String = format!("{path}/level_1_{}/level_2_{}", l1_idx - 1, TEST_NUM[1] - 1);
        let l1_p: String = format!("{path}/level_1_{}", l1_idx - 2);
        for &p in [&file, &l2_p, &l1_p].iter() {
            assert!(tree.contains(p), "Node/path not found (test error): {p}");
        }

        let (l2_n, l2_d, l2_f) =
            validate_counts_below_node(&tree, tree.get_node(&l2_p).expect("L2 node not found"));
        let (l1_n, l1_d, l1_f) =
            validate_counts_below_node(&tree, tree.get_node(&l1_p).expect("L1 node not found"));

        match tree.remove(&file) {
            Ok(_) => {
                tree.validate_counts();
                nodes -= 1;
                files -= 1;
                assert!(!tree.contains(&file), "File found after removal: {file}");
                assert_eq!(tree.counts().nodes.load(Relaxed), nodes, "Node count mismatch [file]");
                assert_eq!(tree.counts().dirs.load(Relaxed), dirs, "Dir count not equal [file]");
                assert_eq!(tree.counts().files.load(Relaxed), files, "File count mismatch [file]");
            }
            Err(e) => panic!("Error removing file: {e}"),
        }

        match tree.remove(&l2_p) {
            Ok(_) => {
                tree.validate_counts();
                nodes -= l2_n as u32;
                dirs -= l2_d as u32;
                files -= l2_f as u32;
                assert!(!tree.contains(&l2_p), "L2 dir found after removal: {l2_p}");
                assert_eq!(tree.counts().nodes.load(Relaxed), nodes, "Node count mismatch [L2]");
                assert_eq!(tree.counts().dirs.load(Relaxed), dirs, "Dir count mismatch [L2]");
                assert_eq!(tree.counts().files.load(Relaxed), files, "File count mismatch [L2]");
            }
            Err(e) => panic!("Error removing L2 dir {l2_p}: {e}"),
        }

        match tree.remove(&l1_p) {
            Ok(_) => {
                tree.validate_counts();
                nodes -= l1_n as u32;
                dirs -= l1_d as u32;
                files -= l1_f as u32;
                assert!(!tree.contains(&l1_p), "L1 dir found after removal: {l1_p}");
                assert_eq!(tree.counts().nodes.load(Relaxed), nodes, "Node count mismatch [L1]");
                assert_eq!(tree.counts().dirs.load(Relaxed), dirs, "Dir count mismatch [L1]");
                assert_eq!(tree.counts().files.load(Relaxed), files, "File count mismatch [L1]");
            }
            Err(e) => panic!("Error removing L1 dir {l1_p}: {e}"),
        }
    }

    /* --------------------------------- */

    /// Create a test DirTree from path and perform some basic validations.
    fn create_test_tree(recursive: bool) -> (&'static str, DirTree, u8) {
        unsafe { setup_tests() }
        let (path, state) = unsafe {
            (TESTDIR.as_ref().unwrap().path().to_str().unwrap(), STATE.as_ref().unwrap())
        };
        let tree: DirTree = DirTree::new_from_path(path, state, recursive);
        assert_eq!(tree.root.node_t, NodeType::Root);
        assert_eq!(*tree.from(), PathBuf::from(path));
        assert_eq!(tree.state(), TreeState::Ready);
        assert!(tree.is_ready(), "Tree is not ready");
        let root_depth: u8 = (path.split(PATH_SEP).count() - 1) as u8;
        tree.validate_counts();
        (path, tree, root_depth)
    }

    /// Return the node, dir and file counts from a DirTree.
    fn counts(tree: &DirTree) -> (u32, u32, u32, u8) {
        (
            tree.counts().nodes.load(Relaxed),
            tree.counts().dirs.load(Relaxed),
            tree.counts().files.load(Relaxed),
            tree.counts().depth.load(Relaxed),
        )
    }

    /// Check that node, dir, and file counts match the expected values.
    fn check_nodes_dirs_files(nodes: u32, root_depth: u8, dirs: u32, files: u32, depth: u8) {
        assert_eq!(nodes, EXP_NODES + root_depth as u32, "nodes mismatch");
        assert_eq!(dirs, EXP_DIRS + root_depth as u32, "dirs mismatch");
        assert_eq!(files, EXP_FILES, "files mismatch");
        assert_eq!(depth, root_depth + 3, "depth mismatch");
    }

    /// Validate and return the counts of nodes, dirs, and files below a given node.
    fn validate_counts_below_node(tree: &DirTree, node: Arc<Node>) -> (u64, u64, u64) {
        let (nodes_c, dirs_c, files_c) = tree.count_from(node.clone());
        let (nodes_i, dirs_i, files_i) = tree.iter_count_from(node.clone());

        assert_eq!(nodes_c, dirs_c + files_c, "count_from() node count != dirs+files");
        assert_eq!(nodes_i, dirs_i + files_i, "iter_count_from() node count != dirs+files");

        assert_eq!(nodes_c, nodes_i, "count_from() != iter_count_from() [nodes]");
        assert_eq!(dirs_c, dirs_i, "count_from() != iter_count_from() [dirs]");
        assert_eq!(files_c, files_i, "count_from() != iter_count_from() [files]");

        (nodes_c as u64, dirs_c as u64, files_c as u64)
    }

    /// Generate all expected paths for the test directory structure.
    fn path_generator(path: &str) -> HashSet<String> {
        let mut paths: HashSet<String> = HashSet::new();
        paths.insert(format!("{}/test.bin", path));

        for l1_idx in 0..TEST_NUM[0] {
            paths.insert(format!("{}/level_1_{l1_idx}", path));
            for l2_idx in 0..TEST_NUM[1] {
                paths.insert(format!("{}/level_1_{l1_idx}/level_2_{l2_idx}", path));
                for l3_idx in 1..=TEST_NUM[2] {
                    paths.insert(format!(
                        "{0}/level_1_{1}/level_2_{2}/file-{1}_{2}_{3}.bin",
                        path, l1_idx, l2_idx, l3_idx
                    ));
                }
                paths.insert(format!("{path}/level_1_{0}/file-{0}.bin", l1_idx));
            }
        }
        paths
    }

    /// Optimally the test directory should be created only once and then
    /// reused for all tests. The unsafe `setup_tests()` should ensure that
    /// this fn is called only once.
    fn create_test_dirs_for_tree_test() -> TempDir {
        let temp_dir: TempDir = TempDir::new().unwrap();
        let path: &str = temp_dir.path().to_str().unwrap();
        create_test_dirs(path, Some(TEST_NUM.to_vec()), true, false, None).unwrap();
        temp_dir
    }
}
