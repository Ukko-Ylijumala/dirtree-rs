// Copyright (c) 2024 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use super::{
    make_weak_ref, metadata, path_parts, path_parts_vec, Arc, AtomicU32, AtomicU8, Deref, DerefMut,
    DirEntry, DirTreeHashMap, DirTreeXxh3Hasher, HashMap, Instant, Metadata, MetadataExt,
    PathBuf, ReadDir, Relaxed, RwLock, ScanState, SecondsSinceEpoch, VecDeque, Weak, HashSet,
};
use rayon::prelude::*;
use std::{
    cmp::Ordering,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind},
};

const PATH_SEP: char = '/';
const META_FAIL: &str = "Failed to get metadata";

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
/// Used to consolidate common code between `Directory` and `File` structs
/// (which themselves are just type placeholders for `Entry` struct).
trait DirectoryEntry {
    fn data(&self) -> &Data;
    fn data_mut(&mut self) -> &mut Data;

    fn stat(&self, path: &PathBuf) -> Option<Metadata> {
        match metadata(path) {
            Ok(meta) => return Some(meta),
            Err(_) => return None,
        };
    }

    fn rescan(&mut self, path: &PathBuf) -> Result<Metadata, Error> {
        let meta: Metadata = match self.stat(path) {
            Some(m) => m,
            // failed to get metadata, likely deleted in the meantime
            None => return Err(Error::new(ErrorKind::NotFound, META_FAIL)),
        };
        if self.data().inode != meta.ino() {
            // inode changed, file/dir was replaced and we're out of sync
            // this case must be handled by the caller
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

/// A generic struct wrapping the `Data` struct, with an extra type parameter `T`.
/// The `Entry` struct's `new()` method is responsible for creating `Directory`
/// and `File` instances with the given path and metadata.
///
/// Two empty structs `Directory` and `File` are also defined, which are used
/// as type parameters for `Entry`. These structs implement the `Default` trait,
/// which is needed for creating `Entry` instances without additional params.
///
/// This approach allows us to share the implementation of `DirectoryEntry`
/// trait between `Directory` and `File` without too much code duplication.
///
/// You can use the struct like this:
/// ```rust
/// use statter::tree::{Directory, Entry, File};
/// use std::fs::{metadata, Metadata};
/// use std::path::PathBuf;
///
/// let root: PathBuf = PathBuf::from("/etc");
/// let pwfile: PathBuf = root.join("passwd");
/// let pwmeta: Metadata = metadata(&pwfile).ok().expect("Metadata should be returned");
///
/// let d = Entry::<Directory>::new(&root.join("systemd"), None).unwrap();
/// let f = Entry::<File>::new(&pwfile, Some(pwmeta)).unwrap();
#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entry<T>(Data, T);

impl<T: Default> Entry<T> {
    pub fn new(path: &PathBuf, meta: Option<Metadata>) -> Result<Self, Error> {
        match meta.or_else(|| metadata(path).ok()) {
            Some(m) => Ok(Self(
                Data {
                    inode: m.ino(),
                    when: SecondsSinceEpoch::new(),
                },
                Default::default(), // provides the type parameter T
            )),
            None => Err(Error::new(ErrorKind::NotFound, META_FAIL)),
        }
    }
}

/// Implement trait `DirectoryEntry` for `Entry` struct.
///
/// Basically, this allows us to consolidate common code under trait
/// `DirectoryEntry` since then we can reference the inner `Data` struct there.
impl<T> DirectoryEntry for Entry<T> {
    fn data(&self) -> &Data {
        &self.0
    }

    fn data_mut(&mut self) -> &mut Data {
        &mut self.0
    }
}

/* 
/// An empty struct, used as a type parameter T for `Entry`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Directory;

impl Default for Directory {
    fn default() -> Self {
        Directory
    }
}
*/

/// A NodeItem struct representing a Directory.
#[derive(Debug)]
pub struct Directory {
    name: String,
    children: RwLock<DirTreeHashMap<String, Arc<Node>>>,
    files: HashSet<String>,
}

impl Directory {
    pub fn new(name: &str) -> Self {
        Directory {
            name: name.into(),
            ..Default::default()
        }
    }

    #[inline]
    fn children(&self) -> &RwLock<DirTreeHashMap<String, Arc<Node>>> {
        &self.children
    }

    /// Whether we have a child with the given name.
    #[inline]
    pub fn has_child(&self, name: &str) -> bool {
        self.children().read().contains_key(name) || self.files.contains(name)
    }

    /// Add a child node to this item's children.
    #[inline]
    fn add_child(&mut self, name: String, node: Arc<Node>) {
        self.children.write().insert(name, node);
    }

    /// Add a file name entry to the `files` HashSet.
    #[inline]
    fn add_file(&mut self, name: String) {
        self.files.insert(name);
    }

    /// Get a child node by name.
    #[inline]
    pub fn get_child(&self, name: &str) -> Option<Arc<Node>> {
        self.children().read().get(name).cloned()
    }

    /// Remove a child node (or a file name entry) by name.
    fn remove_child(&mut self, name: &str) {
        self.children.write().remove(name);
        self.files.remove(name);
    }
}

impl Default for Directory {
    fn default() -> Self {
        Directory {
            name: String::new(),
            children: HashMap::with_hasher(DirTreeXxh3Hasher).into(),
            files: HashSet::new(),
        }
    }
}

impl Clone for Directory {
    fn clone(&self) -> Self {
        Directory {
            name: self.name.clone(),
            children: self.children.read().clone().into(),
            files: self.files.clone(),
        }
    }
}

impl Hash for Directory {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        let lock = self.children.read();
        let mut children: Vec<(&String, &Arc<Node>)> = Vec::from_iter(lock.iter());
        children.sort_by_key(|k| k.0);
        children.hash(state);
    }
}

impl PartialEq for Directory {
    fn eq(&self, other: &Self) -> bool {
        if self.name != other.name {
            // short circuit if the names don't match
            return false;
        }
        *self.children.read() == *other.children.read()
    }
}

impl Eq for Directory {}

/// An empty struct, used as a type parameter T for `Entry`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct File;

impl Default for File {
    fn default() -> Self {
        File
    }
}

/* ######################################################################### */

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeType {
    Root,
    Directory,
    File,
    #[default]
    Uninitialized,
}

impl NodeType {
    /// Returns `true` if the node type is [`Directory`].
    ///
    /// [`Directory`]: NodeType::Directory
    #[must_use]
    #[inline]
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Directory)
    }

    /// Returns `true` if the node type is [`File`].
    ///
    /// [`File`]: NodeType::File
    #[must_use]
    #[inline]
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File)
    }

    /// Returns `true` if the node type is [`Uninitialized`].
    ///
    /// [`Uninitialized`]: NodeType::Uninitialized
    #[must_use]
    #[inline]
    pub fn is_uninit(&self) -> bool {
        matches!(self, Self::Uninitialized)
    }

    /// Returns `true` if the node contains a `Data` struct.
    #[inline]
    pub fn has_data(&self) -> bool {
        matches!(self, Self::Directory | Self::File)
    }
}

/* ######################################################################### */

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeItem {
    Root,
    Dir(Entry<Directory>),
    File(Entry<File>),
    #[default]
    None,
}

impl NodeItem {
    /// Returns `true` if the node item is [`Dir`].
    ///
    /// [`Directory`]: NodeItem::Dir
    #[must_use]
    #[inline]
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir(_))
    }

    /// Returns `true` if the node item is [`File`].
    ///
    /// [`File`]: NodeItem::File
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

    /// Returns a reference to the inner [`Directory`] if the node item is [`Dir`].
    pub fn as_dir(&self) -> Option<&Entry<Directory>> {
        if let Self::Dir(v) = self {
            Some(v)
        } else {
            None
        }
    }

    /// Returns a reference to the inner [`File`] if the node item is [`File`].
    pub fn as_file(&self) -> Option<&Entry<File>> {
        if let Self::File(v) = self {
            Some(v)
        } else {
            None
        }
    }

    /// Returns a reference to item's [`Data`] if the node item is [`Dir`] or [`File`].
    fn data(&self) -> Option<&Data> {
        Some(match self {
            Self::Dir(d) => d.data(),
            Self::File(f) => f.data(),
            _ => return None,
        })
    }
}

// Implement `From` for converting `Directory` into `NodeItem`.
impl From<Entry<Directory>> for NodeItem {
    fn from(v: Entry<Directory>) -> Self {
        Self::Dir(v)
    }
}

// Implement `From` for converting `File` into `NodeItem`.
impl From<Entry<File>> for NodeItem {
    fn from(v: Entry<File>) -> Self {
        Self::File(v)
    }
}

/* ######################################################################### */

/// Node in the trie structure for storing paths and items, respectively.
#[derive(Default, Debug)]
pub struct Node {
    pub node_t: NodeType,
    // TODO: either make this an enum with variants for each NodeType, or Option<..>
    item: RwLock<NodeItem>,
    parent: Weak<Node>,
    children: Option<RwLock<DirTreeHashMap<String, Arc<Node>>>>,
}

impl Node {
    /// Returns a new node with the given item.
    /// NOTE: children are initialized only for containers (directories and root).
    pub fn new(item: NodeItem, parent: Option<Arc<Node>>) -> Self {
        let node_t: NodeType = match item {
            NodeItem::Root => NodeType::Root,
            NodeItem::Dir(_) => NodeType::Directory,
            NodeItem::File(_) => NodeType::File,
            NodeItem::None => NodeType::Uninitialized,
        };
        Self {
            children: match node_t {
                NodeType::Root | NodeType::Directory => {
                    Some(RwLock::new(HashMap::with_hasher(DirTreeXxh3Hasher)))
                }
                NodeType::Uninitialized => None,
                NodeType::File => None,
            },
            node_t,
            item: RwLock::new(item),
            parent: parent.map_or_else(|| Weak::new(), |p| make_weak_ref(p)),
        }
    }

    /// Returns `true` if the node is traversable (`children` != `None`).
    pub fn is_traversable(&self) -> bool {
        matches!(self.node_t, NodeType::Directory | NodeType::Root)
    }

    /// The Node as an `Entry<Directory>`, if it contains a directory.
    fn as_dir(&self) -> Option<Entry<Directory>> {
        match self.item.read().as_dir() {
            Some(d) => Some(d.clone()),
            None => None,
        }
    }

    /// The Node as an `Entry<File>`, if it contains a file.
    fn as_file(&self) -> Option<Entry<File>> {
        match self.item.read().as_file() {
            Some(d) => Some(d.clone()),
            None => None,
        }
    }

    /// Returns a weak reference to this node.
    fn weakref(self) -> Weak<Self> {
        make_weak_ref(self)
    }

    /// Resolve the weak reference to this node's parent node.
    #[inline]
    fn parent(&self) -> Option<Arc<Node>> {
        match self.parent.upgrade() {
            None => return None,
            Some(parent) => {
                return Some(parent.clone());
            }
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

    /// Filesystem path of this node as a `PathBuf`.
    pub fn path(&self) -> PathBuf {
        let mut path: PathBuf = PathBuf::from("/");
        for part in self.construct_path() {
            path.push(part);
        }
        path
    }

    /// Get this node's name from parent node's `children` HashMap.
    /// Root node always returns "/".
    pub fn name(&self) -> Result<String, Error> {
        match self.parent() {
            Some(parent) => Ok(parent
                .get_child_byref(self)
                .expect("Parent's children HashMap should contain the child node's name")
                .0),
            None => {
                if self.node_t == NodeType::Root {
                    return Ok("/".to_string());
                }
                Err(Error::new(ErrorKind::NotFound, "Stale parent reference"))
            }
        }
    }

/* 
    fn children_itm(&self) -> &RwLock<DirTreeHashMap<String, Arc<Node>>> {
        (*self.read()).as_dir().unwrap().1.children()
    }

*/
    #[inline]
    fn children(&self) -> Option<&RwLock<DirTreeHashMap<String, Arc<Node>>>> {
        self.children.as_ref()
    }

    /// Whether we have a child with the given name.
    #[inline]
    pub fn has_child(&self, name: &str) -> bool {
        self.children()
            .map_or(false, |c| c.read().contains_key(name))
    }

    /// Add a child node to the current node's children.
    #[inline]
    fn add_child(&self, name: String, node: Arc<Node>) {
        self.children().unwrap().write().insert(name, node);
    }

    /// Get a child node by name.
    #[inline]
    pub fn get_child(&self, name: &str) -> Option<Arc<Node>> {
        self.children().and_then(|c| c.read().get(name).cloned())
    }

    /// Remove a child node by name.
    fn remove_child(&self, name: &str) {
        self.children().unwrap().write().remove(name);
    }

    /// Get the name of a child node and its `Arc<Node>` ptr from a reference
    /// to the child node itself. The main use case is for a child node to find
    /// its own name and reference in the parent node's `children` HashMap.
    #[inline]
    fn get_child_byref(&self, child: &Node) -> Option<(String, Arc<Node>)> {
        match self
            .children()
            .unwrap()
            .read()
            .par_iter()
            .find_any(|item: &(&String, &Arc<Node>)| **item.1 == *child)
        {
            Some((name, child)) => Some((name.clone(), child.clone())),
            None => None,
        }
    }
}

impl Clone for Node {
    /// Clones the node and its children.
    fn clone(&self) -> Self {
        Self {
            node_t: self.node_t.clone(),
            item: RwLock::new(self.item.read().clone()),
            parent: self.parent.clone(),
            children: match self.children {
                Some(ref children) => Some(RwLock::new(children.read().clone())),
                None => None,
            },
        }
    }
}

// Implement Deref for Node to allow access to NodeItem methods.
impl Deref for Node {
    type Target = RwLock<NodeItem>;

    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

// Implement mutable Deref for Node to allow changing NodeItem.
impl DerefMut for Node {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.item
    }
}

// For a File or Directory, the hash is based on the inode.
impl Hash for Node {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if matches!(self.node_t, NodeType::Directory | NodeType::File) {
            self.read().data().unwrap().hash(state);
        } else {
            self.read().hash(state);
        }
        self.node_t.hash(state);
    }
}

// Implement <Node> == <Node> comparisons
impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        // *self.read() -> NodeItem (due to impl Deref)
        &*self.read() == &*other.read()
    }
}

// Implement <Node> == <NodeItem> comparisons
impl PartialEq<NodeItem> for Node {
    fn eq(&self, other: &NodeItem) -> bool {
        &*self.read() == &*other
    }
}

// Implement <NodeItem> == <Node> comparisons
impl PartialEq<Node> for NodeItem {
    fn eq(&self, other: &Node) -> bool {
        &*self == &*other.read()
    }
}

/* ######################################################################### */

/// Atomic counters for tracking the number of nodes, directories, and files.
/// Using a separate counter struct allows us to not have to lock the entire
/// tree f.ex. when inserting or removing nodes.
#[derive(Default, Debug)]
pub struct Counts {
    /// Does not include the root node.
    pub nodes: AtomicU32,
    /// Maximum depth of the tree. Root is at depth 0.
    pub depth: AtomicU8,
    pub dirs: AtomicU32,
    pub files: AtomicU32,
}

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
/// let tree: DirTree = DirTree::new_from_path("/home", &state, true, false);
/// tree.print_info(); // print basic tree info (nodes, dirs, files etc)
#[derive(Default, Debug)]
pub struct DirTree {
    from: Arc<PathBuf>,
    counts: Arc<Counts>,
    created: SecondsSinceEpoch,
    pub root: Arc<Node>,
    dirsonly: bool,
    debug: bool,
}

impl DirTree {
    /// Tree root path (in the filesystem) from which the tree is built.
    /// NOTE: internally stored paths are relative to this.
    pub fn from(&self) -> &PathBuf {
        &self.from
    }

    /// Set the root (filesystem) path of the tree.
    fn set_from(&mut self, from: PathBuf) {
        self.from = from.into();
        self.insert(&self.from, NodeType::Directory, None);
    }

    /// Returns a reference to the tree's `counts` struct.
    pub fn counts(&self) -> Arc<Counts> {
        self.counts.clone()
    }

    /// Returns a reference to the tree's creation time.
    pub fn created(&self) -> &SecondsSinceEpoch {
        &self.created
    }

    /// Creates a new empty directory tree (internally a Trie structure).
    pub fn new(debug: bool, dirsonly: bool) -> Self {
        DirTree {
            root: Node::new(NodeItem::Root, None).into(),
            debug,
            dirsonly,
            ..Default::default()
        }
    }

    /// Creates a new tree (trie) with the given path as root.
    ///
    /// If `recursive` is true, also populates the tree by recursively walking
    /// the full directory structure (starting from from the given directory)
    /// and inserting each found path into the tree.
    ///
    /// If `dirsonly` is true, only directory items are fully initialized during creation.
    pub fn new_from_path(path: &str, state: &ScanState, recursive: bool, dirsonly: bool) -> Self {
        let mut tree: DirTree = Self::new(state.debug, dirsonly);
        tree.set_from(PathBuf::from(path));
        // Technically we've not yet scanned the root directory, but this place
        // is the most logical one to do the increment to keep the counter in
        // sync as adding more logic to `populate*()` methods would be counter-
        // productive. Besides, this counter is only for display for now.
        state.num_d.inc1();

        if state.debug {
            eprintln!("<TREE> : {:?}", tree)
        };

        if recursive {
            match state.parallel && !state.sync {
                true => tree.populate_par(&tree.from, true, state),
                false => tree.populate(&tree.from, true, state),
            }
        };
        tree
    }

    /// Populate a leaf node in the trie with the contents of a directory.
    /// NOTE: single threaded, potentially slow with large directory trees.
    #[inline]
    pub fn populate(&self, path: &PathBuf, recursive: bool, state: &ScanState) {
        let entries: ReadDir = match self.get_entries(path) {
            Some(value) => value,
            None => return,
        };
        for entry in entries.filter_map(Result::ok) {
            self.process_entry(entry, state, recursive);
        }
    }

    /// Parallel version of `populate()` using Rayon's `par_bridge()`.
    #[inline]
    pub fn populate_par(&self, path: &PathBuf, recursive: bool, state: &ScanState) {
        let entries: ReadDir = match self.get_entries(path) {
            Some(value) => value,
            None => return,
        };
        entries
            .filter_map(Result::ok)
            .par_bridge()
            .for_each(|entry: DirEntry| {
                self.process_entry(entry, state, recursive);
            });
    }

    /// Get the entries in a directory as a `ReadDir` iterator.
    #[inline]
    fn get_entries(&self, path: &PathBuf) -> Option<ReadDir> {
        if self.debug {
            eprintln!(" -> DIR: {}", path.to_string_lossy())
        };
        let entries: ReadDir = match path.read_dir() {
            Ok(entries) => entries,
            Err(_) => return None,
        };
        Some(entries)
    }

    /// Process a directory entry and insert it into the trie.
    #[inline]
    fn process_entry(&self, entry: DirEntry, state: &ScanState, recursive: bool) {
        let path: PathBuf = entry.path();
        if let Ok(meta) = entry.metadata() {
            if self.debug {
                eprintln!("  entry: {} ", path.to_string_lossy())
            };

            if meta.is_dir() {
                self.insert(&path, NodeType::Directory, Some(meta));
                state.num_d.inc1();
                if recursive {
                    match state.parallel && !state.sync {
                        true => self.populate_par(&path, recursive, state),
                        false => self.populate(&path, recursive, state),
                    }
                }
            } else if meta.is_file() {
                if state.verbose {
                    state.fsize.fetch_add(meta.len());
                }
                self.insert(&path, NodeType::File, Some(meta));
                state.num_f.inc1();
            }
        } else {
            if self.debug {
                eprintln!("Error reading metadata: {}", path.to_string_lossy());
            }
        }
    }

    /* --------------------------------- */

    /// Inserts a path into the trie. The path must be an absolute filesystem path.
    /// The path is split on forward slash ("/") and the first empty string discarded.
    pub fn insert(&self, path: &PathBuf, node_t: NodeType, meta: Option<Metadata>) {
        let mut current: Arc<Node> = self.root.clone();
        let p_unicode = path.to_string_lossy();
        let parts: Vec<&str> = path_parts_vec(&p_unicode);
        let len: usize = parts.len();
        let mut depth: usize = 0; // root node is at depth 0

        for part in parts {
            depth += 1;
            let part: String = part.to_owned();
            if !current.has_child(&part) {
                if self.debug {
                    eprintln!("<NODE> : {:?}", &current);
                }
                // we don't have an item for this node yet, hence NodeItem::None
                // also node_t must be set here since later the Node will be in
                // an Arc and we can't change that field anymore
                let mut new: Node = Node::new(NodeItem::None, Some(current.clone()));
                if depth < len {
                    if self.debug {
                        eprintln!("\n<---- {part} ----> depth: {depth} len: {len}");
                    }
                    // must be a container (directory)
                    new.node_t = NodeType::Directory;
                    // Node.children = None in Node::new() for NodeItem::None
                    new.children = Some(RwLock::new(HashMap::with_hasher(DirTreeXxh3Hasher)));
                    // we must increment the node counters here since we've
                    // not reached the leaf node yet and we shouldn't do a
                    // full initialization for an intermediate node
                    self.counts.nodes.fetch_add(1, Relaxed);
                    self.counts.dirs.fetch_add(1, Relaxed);
                } else {
                    new.node_t = node_t.clone();
                    if node_t == NodeType::Directory {
                        new.children = Some(RwLock::new(HashMap::with_hasher(DirTreeXxh3Hasher)));
                    }
                }
                if self.debug {
                    eprintln!("  + new: {part} ::: {:?}", &new);
                }
                current.add_child(part.clone(), new.into());
            }
            current = current.get_child(&part).unwrap();
        }

        if !matches!(*current.item.read(), NodeItem::None) {
            // for now we don't overwrite existing nodes, but
            // this may change in the future to allow for updates
            return;
        }

        match node_t {
            NodeType::Directory => {
                *current.item.write() = NodeItem::Dir(Entry::<Directory>::new(path, meta).unwrap());
                self.counts.dirs.fetch_add(1, Relaxed);
            }
            NodeType::File => {
                if !self.dirsonly {
                    // optimization: don't create file Entry to conserve memory
                    // the node itself exists though, just not fully initialized
                    *current.item.write() = NodeItem::File(Entry::<File>::new(path, meta).unwrap());
                }
                self.counts.files.fetch_add(1, Relaxed);
            }
            _ => return,
        };

        self.counts.nodes.fetch_add(1, Relaxed);
        self.counts.depth.fetch_max(len as u8, Relaxed);
        if self.debug {
            eprintln!(" ++ INS: {:?}", current)
        };
    }

    /// Remove a Node (or a leaf) from the trie. Expects an absolute path.
    ///
    /// Returns a tuple of `(nodes, dirs, files)` removed on success and `None`
    /// if the path was not found. The root node cannot be removed.
    ///
    /// WARNING: implementation is WIP and may yet contain bugs.
    pub fn remove(&self, path: &str) -> Result<Option<(u32, u32, u32)>, Error> {
        match self.get_node(path) {
            Some(node) => {
                if node.node_t == NodeType::Root {
                    return Err(Error::new(ErrorKind::InvalidInput, "Cannot remove root node"));
                };

                match node.parent() {
                    Some(parent) => {
                        let (nodes, dirs, files) = self.count_from(node.clone());
                        if self.debug {
                            eprintln!("*** Removing: {:?}", node.path());
                        }
                        let (name, c) = parent.get_child_byref(&node).unwrap();
                        assert_eq!(node, c, "Node should be the same as the one in parent");
                        parent.remove_child(&name);
                        self.counts.nodes.fetch_sub(nodes, Relaxed);
                        self.counts.dirs.fetch_sub(dirs, Relaxed);
                        self.counts.files.fetch_sub(files, Relaxed);
                        return Ok(Some((nodes, dirs, files)));
                    }

                    None => {
                        let msg: String = format!("Stale parent reference: {:?}", node.path());
                        if self.debug {
                            eprintln!("ERROR: {msg}");
                        };
                        return Err(Error::new(ErrorKind::NotFound, msg));
                    }
                }
            }

            None => {
                if self.debug {
                    eprintln!("WARN: node not found: {path}")
                };
                return Ok(None);
            }
        }
    }

    /* --------------------------------- */

    /// Get a node from the trie. Expects an absolute path.
    pub fn get_node(&self, path: &str) -> Option<Arc<Node>> {
        // short circuit if the path is not absolute or does not look like a path
        if !path.contains(PATH_SEP) || !path.starts_with(PATH_SEP) {
            return None;
        }
        let mut current: Arc<Node> = self.root.clone();
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

    /// Returns a CLONE of the item at the given path. Expects an absolute path.
    fn item(&self, path: &str) -> Option<NodeItem> {
        let node: Arc<Node> = match self.get_node(path) {
            Some(n) => n,
            None => return None,
        };
        let item: NodeItem = node.item.read().clone();
        Some(item)
    }

    /// The filesystem path of a Node, if it contains a file or directory.
    pub fn fs_path(&self, node: Arc<Node>) -> Option<PathBuf> {
        match node.node_t.has_data() {
            true => Some(node.path()),
            false => None,
        }
    }

    /* --------------------------------- */

    /// Walk the tree recursively from a Node and return a Vec of child Nodes. The
    /// `dirs` and `files` flags control whether to include directory and/or file Nodes.
    fn walk(&self, node: Arc<Node>, dirs: bool, files: bool) -> Arc<RwLock<Vec<Arc<Node>>>> {
        let all_nodes: Arc<RwLock<Vec<Arc<Node>>>> = RwLock::new(Vec::new()).into();
        if node.is_traversable() {
            node.children()
                .unwrap()
                .read()
                .values()
                .par_bridge()
                .for_each(|child| {
                    let mut tmp_nodes = Vec::new();
                    if dirs && child.node_t.is_dir() {
                        tmp_nodes.push(child.clone());
                    } else if files && child.node_t.is_file() {
                        tmp_nodes.push(child.clone());
                    }
                    if child.is_traversable() {
                        tmp_nodes
                            .extend(self.walk(child.clone(), dirs, files).read().iter().cloned());
                    }
                    all_nodes.write().extend(tmp_nodes);
                });
        }
        all_nodes
    }

    /// Walks the full tree and returns a Vec of all Nodes. WARNING: this can be
    /// slow and memory intensive for large trees. Prefer using `DirTree::iter()`.
    pub fn nodes(&self) -> Vec<Arc<Node>> {
        self.walk(self.root.clone(), true, true).read().to_vec()
    }

    /// Returns a Vec of all `Directory` nodes in the tree.
    pub fn dirs(&self) -> Vec<Arc<Node>> {
        self.walk(self.root.clone(), true, false).read().to_vec()
    }

    /// Returns a Vec of all `File` nodes in the tree.
    pub fn files(&self) -> Vec<Arc<Node>> {
        self.walk(self.root.clone(), false, true).read().to_vec()
    }

    /* --------------------------------- */

    /// Creates an iterator to iterate through the tree starting from a Node.
    /// The iterator is depth-first and includes the starting Node.
    pub fn iter_from(&self, node: Arc<Node>) -> DirTreeIterator {
        DirTreeIterator(VecDeque::from(vec![node]))
    }

    /// Creates an iterator to walk through all Nodes in the tree.
    pub fn iter(&self) -> DirTreeIterator {
        self.iter_from(self.root.clone())
    }

    /// An iterator over all `Directory` items in the tree.
    pub fn iter_dirs(&self) -> impl Iterator<Item = Entry<Directory>> {
        self.iter().filter_map(|node: Arc<Node>| node.as_dir())
    }

    /// An iterator over all `File` items in the tree.
    pub fn iter_files(&self) -> impl Iterator<Item = Entry<File>> {
        self.iter().filter_map(|node: Arc<Node>| node.as_file())
    }

    /// An iterator over all Paths in the tree.
    pub fn iter_paths(&self) -> impl Iterator<Item = String> + '_ {
        self.iter()
            .filter_map(|node: Arc<Node>| self.fs_path(node))
            .map(|p| p.to_string_lossy().to_string())
    }

    /// Count the number of directory and file Nodes by iterating from a Node.
    /// Also counts the starting Node. Returns a tuple of `(nodes, dirs, files)`.
    pub fn iter_count_from(&self, node: Arc<Node>) -> (u32, u32, u32) {
        if node.node_t.is_file() {
            return (1, 0, 1);
        }

        let mut nodes: u32 = 0;
        let mut dirs: u32 = 0;
        let mut files: u32 = 0;

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

    /// Count the number of directory and file Nodes by iterating the whole tree.
    /// Does not count the root Node. Returns a tuple of `(nodes, dirs, files)`.
    pub fn iter_count(&self) -> (u32, u32, u32) {
        let (mut nodes, dirs, files) = self.iter_count_from(self.root.clone());
        nodes -= 1; // remove root node since we started from it
        (nodes, dirs, files)
    }

    /* --------------------------------- */

    /// Traverses recursively from a Node and applies function `f` to each
    /// child Node, AND the starting Node itself.
    pub fn traverse_from<F>(&self, node: Arc<Node>, f: &mut F)
    where
        F: FnMut(Arc<Node>),
    {
        f(node.clone());
        if node.is_traversable() {
            for child in node.children().unwrap().read().values() {
                if child.is_traversable() {
                    // traverse directories first (depth-first search)
                    self.traverse_from(child.clone(), f);
                } else {
                    f(child.clone());
                }
            }
        }
    }

    /// Traverses the tree from root and applies function `f` to each Node.
    pub fn traverse<F>(&self, mut f: F)
    where
        F: FnMut(Arc<Node>),
    {
        self.traverse_from(self.root.clone(), &mut f);
    }

    /// Count the number of directory and file Nodes with `traverse()`. Also counts
    /// the starting Node (except root). Returns a tuple of `(nodes, dirs, files)`.
    pub fn count_from(&self, node: Arc<Node>) -> (u32, u32, u32) {
        if node.node_t.is_file() {
            return (1, 0, 1);
        }

        let mut nodes: u32 = 0;
        let mut files: u32 = 0;
        let mut dirs: u32 = 0;

        self.traverse_from(node.clone(), &mut |n: Arc<Node>| {
            nodes += 1;
            if n.node_t.is_dir() {
                dirs += 1;
            } else if n.node_t.is_file() {
                files += 1;
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
        eprintln!("\n{:#?}\n", self);
        self.traverse(|node: Arc<Node>| {
            if node.node_t.has_data() {
                if self.debug {
                    println!("{:?}", &node.construct_path());
                } else {
                    println!("{}", &node.path().to_string_lossy());
                }
            }
        });
    }

    /// Validate the counts of nodes, dirs, and files in the tree.
    ///
    /// We take the counts from the tree's `Counts` struct as master data and
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
        assert_eq!(want_n, want_d + want_f, "master node count != dirs+files");

        let start: Instant = Instant::now();
        let (nodes, dirs, files) = self.count_from(self.root.clone());
        assert_eq!(nodes, dirs + files, "count_from() node count != dirs+files");
        assert_eq!(want_n, nodes, "count_from() node count != master count");
        assert_eq!(want_d, dirs, "count_from() dirs do not match");
        assert_eq!(want_f, files, "count_from() files do not match");
        eprintln!(" --> count_from()  = {:?}", start.elapsed());

        let start: Instant = Instant::now();
        let (nodes, dirs, files) = self.iter_count();
        assert_eq!(nodes, dirs + files, "iter_count() node count != dirs+files");
        assert_eq!(want_n, nodes, "iter_count() node count != master count");
        assert_eq!(want_d, dirs, "iter_count() dirs do not match");
        assert_eq!(want_f, files, "iter_count() files do not match");
        eprintln!(" --> iter_count()  = {:?}", start.elapsed());

        let start: Instant = Instant::now();
        let dirs: u32 = self.dirs().len() as u32;
        eprintln!(" --> walk: dirs()  = {:?}", start.elapsed());
        let start: Instant = Instant::now();
        let files: u32 = self.files().len() as u32;
        eprintln!(" --> walk: files() = {:#?}", start.elapsed());
        assert_eq!(want_d, dirs, "walk() dirs do not match");
        assert_eq!(want_f, files, "walk() files do not match");
    }
}

/* ######################################################################### */

/// Iterator for walking through a DirTree.
pub struct DirTreeIterator(VecDeque<Arc<Node>>);

impl Iterator for DirTreeIterator {
    type Item = Arc<Node>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop_front().map(|node| {
            if node.children.is_none() {
                return node;
            }
            // Push all found children to the stack
            for child in node
                .children()
                .unwrap()
                .read()
                .values()
                .collect::<Vec<&Arc<Node>>>()
                .into_iter()
                .rev()
            {
                if child.is_traversable() {
                    // push directories to the front of the queue...
                    self.0.push_front(child.clone());
                } else {
                    // ...and files to the back
                    self.0.push_back(child.clone());
                }
            }
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
        STATE = Some(ScanState::default());
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
        let tree: DirTree = DirTree::new(false, false);
        let (nodes, dirs, files, depth) = counts(&tree);

        assert_eq!(tree.root.node_t, NodeType::Root);
        assert_eq!(*tree.from(), PathBuf::default());
        tree.validate_counts();
        assert_eq!(nodes, 0, "nodes mismatch");
        assert_eq!(dirs, 0, "dirs mismatch");
        assert_eq!(files, 0, "files mismatch");
        assert_eq!(depth, 0, "depth mismatch");
    }

    #[test]
    fn test_tree_new_from_path() {
        let (_, tree, root_depth) = create_test_tree(false);
        let (nodes, dirs, files, depth) = counts(&tree);

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
        tree.traverse(|n: Arc<Node>| {
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
        let tree: DirTree = DirTree::new_from_path(path, state, recursive, false);
        assert_eq!(tree.root.node_t, NodeType::Root);
        assert_eq!(*tree.from(), PathBuf::from(path));
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
