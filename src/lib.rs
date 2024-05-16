// Copyright (c) 2024 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use crate::{
    make_weak_ref, metadata, path_parts, path_parts_vec, Arc, Deref, DerefMut, HashMap, Instant,
    Metadata, MetadataExt, PathBuf, RwLock, VecDeque, Weak,
};
use std::{
    cmp::Ordering,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind},
    sync::atomic::{AtomicU32, AtomicU8, Ordering::Relaxed},
};

const PATH_SEP: char = '/';
const META_FAIL: &str = "Failed to get metadata";

#[derive(Debug, Clone, Eq)]
struct Data {
    root: Arc<PathBuf>,
    relpath: String,
    inode: u64,
    mode: u32,
    scanned: u64,
    when: Instant,
}

impl Data {
    /// Full path of the file or directory, as `root.join(relpath)`
    fn path(&self) -> PathBuf {
        (*self.root).clone().join(&self.relpath)
    }
}

// Path and inode are enough to uniquely identify a file or directory.
impl Hash for Data {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path().hash(state);
        self.inode.hash(state);
    }
}

impl PartialEq for Data {
    fn eq(&self, other: &Self) -> bool {
        self.path() == other.path() && self.inode == other.inode
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

/// `std::time::Instant` does not implement Default, so we cannot use
/// #[derive(Default)] for Data and must implement it ourselves.
impl Default for Data {
    fn default() -> Self {
        Self {
            root: PathBuf::new().into(),
            relpath: String::new(),
            inode: 0,
            mode: 0,
            scanned: 0,
            when: Instant::now(),
        }
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

    fn stat(&self) -> Option<Metadata> {
        match metadata(self.data().path().as_path()) {
            Ok(meta) => return Some(meta),
            Err(_) => return None,
        };
    }

    fn rescan(&mut self) -> Result<Metadata, Error> {
        let meta: Metadata = match self.stat() {
            Some(m) => m,
            // failed to get metadata, likely deleted in the meantime
            None => return Err(Error::new(ErrorKind::NotFound, META_FAIL)),
        };
        if self.data().inode != meta.ino() {
            // inode changed, file/dir was replaced and we're out of sync
            // this case must be handled by the caller
            return Err(Error::new(ErrorKind::AlreadyExists, "Inode changed"));
        }
        if self.data().mode != meta.mode() {
            self.data_mut().mode = meta.mode();
        }
        self.data_mut().scanned += 1;
        self.data_mut().when = Instant::now();
        Ok(meta)
    }

    fn parent(&self) -> PathBuf {
        self.data().path().parent().unwrap().to_path_buf()
    }

    fn name(&self) -> String {
        self.data()
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
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
/// let d = Entry::<Directory>::new(PathBuf::from("/dir/path"), None).unwrap();
/// let f = Entry::<File>::new(PathBuf::from("/file/path"), None).unwrap();
#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entry<T>(Data, T);

impl<T: Default> Entry<T> {
    fn new(root: Arc<PathBuf>, relpath: String, meta: Option<Metadata>) -> Result<Self, Error> {
        let path: PathBuf = root.join(&relpath);
        let meta: Option<Metadata> = meta.or_else(|| metadata(&path).ok());
        match meta {
            Some(m) => Ok(Self(
                Data {
                    root,
                    relpath,
                    inode: m.ino(),
                    mode: m.mode(),
                    scanned: 1,
                    when: Instant::now(),
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

/// An empty struct, used as a type parameter T for `Entry`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Directory;

impl Default for Directory {
    fn default() -> Self {
        Directory
    }
}

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
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Directory)
    }

    /// Returns `true` if the node type is [`File`].
    ///
    /// [`File`]: NodeType::File
    #[must_use]
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File)
    }

    /// Returns `true` if the node type is [`Uninitialized`].
    ///
    /// [`Uninitialized`]: NodeType::Uninitialized
    #[must_use]
    pub fn is_uninit(&self) -> bool {
        matches!(self, Self::Uninitialized)
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
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir(_))
    }

    /// Returns `true` if the node item is [`File`].
    ///
    /// [`File`]: NodeItem::File
    #[must_use]
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// Returns `true` if the node item is [`None`].
    ///
    /// [`None`]: NodeItem::None
    #[must_use]
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

    /// Returns the item's path as Option<String> if the node item is [`Dir`] or [`File`].
    fn path_str(&self) -> Option<String> {
        Some(match self {
            Self::Dir(d) => d.data().path().to_str().map(|s| s.to_owned())?,
            Self::File(f) => f.data().path().to_str().map(|s| s.to_owned())?,
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
    node_t: NodeType,
    item: RwLock<NodeItem>,
    parent: Weak<Node>,
    depth: AtomicU8,
    children: Option<RwLock<HashMap<String, Arc<Node>>>>,
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
        let depth: AtomicU8 = match parent {
            Some(ref p) => (p.depth.load(Relaxed) + 1).into(),
            None => AtomicU8::new(0),
        };
        Self {
            children: match node_t {
                NodeType::Root | NodeType::Directory => Some(RwLock::new(HashMap::new())),
                NodeType::Uninitialized => None,
                NodeType::File => None,
            },
            node_t,
            item: RwLock::new(item),
            parent: parent.map_or_else(|| Weak::new(), |p| make_weak_ref(p)),
            depth,
        }
    }

    /// Returns `true` if the node is traversable (`children` != `None`).
    pub fn is_traversable(&self) -> bool {
        matches!(self.node_t, NodeType::Directory | NodeType::Root)
    }

    fn as_dir(&self) -> Option<Entry<Directory>> {
        match self.item.read().as_dir() {
            Some(d) => Some(d.clone()),
            None => None,
        }
    }

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

    /// Wrap the node in an Arc for sharing between threads.
    #[inline]
    fn arc(self) -> Arc<Node> {
        self.into()
    }

    /// Whether we have a child with the given name.
    #[inline]
    fn has_child(&self, name: &str) -> bool {
        self.children
            .as_ref()
            .map_or(false, |c| c.read().contains_key(name))
    }

    /// Add a child node to the current node's children.
    #[inline]
    fn add_child(&self, name: String, node: Arc<Node>) {
        self.children.as_ref().unwrap().write().insert(name, node);
    }

    /// Get a child node by name.
    #[inline]
    fn get_child(&self, name: &str) -> Option<Arc<Node>> {
        self.children
            .as_ref()
            .and_then(|c| c.read().get(name).cloned())
    }

    /// Remove a child node by name.
    fn remove_child(&self, name: &str) {
        self.children.as_ref().unwrap().write().remove(name);
    }
}

impl Clone for Node {
    /// Clones the node and its children.
    fn clone(&self) -> Self {
        Self {
            node_t: self.node_t.clone(),
            item: RwLock::new(self.item.read().clone()),
            parent: self.parent.clone(),
            depth: AtomicU8::new(self.depth.load(Relaxed)),
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

// For a File or Directory, the hash is based on the path and inode.
impl Hash for Node {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if matches!(self.node_t, NodeType::Directory | NodeType::File) {
            self.read().data().unwrap().hash(state);
        } else {
            self.read().hash(state);
        }
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
    nodes: AtomicU32,
    /// Maximum depth of the tree. Root is at depth 0.
    depth: AtomicU8,
    pub dirs: AtomicU32,
    pub files: AtomicU32,
}

/// Trie structure for storing a directory tree.
#[derive(Default, Debug)]
pub struct DirTree {
    from: Arc<PathBuf>,
    pub counts: Arc<Counts>,
    root: Arc<Node>,
    debug: bool,
}

impl DirTree {
    /// Creates a new empty directory tree (internally a Trie structure).
    pub fn new(debug: bool) -> Self {
        DirTree {
            root: Arc::new(Node::new(NodeItem::Root, None)),
            debug,
            ..Default::default()
        }
    }

    /// Creates a new tree (trie) with the given path as root.
    ///
    /// If `recursive` is true, also populates the tree by recursively walking
    /// the full directory structure (starting from from the given directory)
    /// and inserting each found path into the tree.
    pub fn new_from_path(path: &str, recursive: bool, debug: bool) -> Arc<Self> {
        let mut tree: DirTree = Self::new(debug);
        let p: PathBuf = PathBuf::from(path);
        tree.from = p.clone().into();

        if debug {
            eprintln!("<TREE> : {:?}", tree)
        };

        tree.insert(&p, NodeType::Directory, None);
        if recursive {
            tree.populate(&p, true);
        };
        tree.into()
    }

    /// Populate a leaf node in the trie with the contents of a directory.
    /// NOTE: single threaded, potentially slow with large directory trees.
    pub fn populate(&self, path: &PathBuf, recursive: bool) {
        if self.debug {
            eprintln!(" -> DIR: {}", path.to_string_lossy())
        };
        let entries = match path.read_dir() {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.filter_map(Result::ok) {
            let path: PathBuf = entry.path();
            let meta: Metadata = match entry.metadata() {
                Ok(metadata) => {
                    if self.debug {
                        eprintln!("  entry: {} ", path.to_string_lossy())
                    };
                    metadata
                }
                Err(_) => {
                    if self.debug {
                        eprintln!("Error reading metadata: {}", path.to_string_lossy());
                    }
                    continue;
                }
            };
            if meta.is_dir() {
                self.insert(&path, NodeType::Directory, Some(meta));
                if recursive {
                    self.populate(&path, recursive);
                }
            } else if meta.is_file() {
                self.insert(&path, NodeType::File, Some(meta));
            }
        }
    }

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
                    new.children = Some(RwLock::new(HashMap::new()));
                } else {
                    new.node_t = node_t.clone();
                    if node_t == NodeType::Directory {
                        new.children = Some(RwLock::new(HashMap::new()));
                    }
                }
                if self.debug {
                    eprintln!("  + new: {part} ::: {:?}", &new);
                }
                current.add_child(part.clone(), new.arc());
            }
            current = current.get_child(&part).unwrap();
        }

        if !matches!(*current.item.read(), NodeItem::None) {
            // for now we don't overwrite existing nodes, but
            // this may change in the future to allow for updates
            return;
        }

        let relpath: String = path
            .strip_prefix(self.from.as_ref())
            .unwrap()
            .to_string_lossy()
            .to_string();
        match node_t {
            NodeType::Directory => {
                *current.item.write() = NodeItem::Dir(
                    Entry::<Directory>::new(self.from.clone(), relpath, meta).unwrap(),
                );
                self.counts.dirs.fetch_add(1, Relaxed);
            }
            NodeType::File => {
                *current.item.write() =
                    NodeItem::File(Entry::<File>::new(self.from.clone(), relpath, meta).unwrap());
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
    /// WARNING: implementation is WIP and may yet contain bugs.
    pub fn remove(&self, path: &str) {
        match self.get_node(path) {
            Some(node) => match node.parent.upgrade() {
                Some(parent) => {
                    eprintln!("*** Removing ***\n{:?}", node);
                    eprintln!("\n*** Parent before ***\n{:?}", (*parent));
                    parent.remove_child(PathBuf::from(path).file_name().unwrap().to_str().unwrap());
                    eprintln!("\n*** Parent after ***\n{:?}", (*parent));
                    self.counts.nodes.fetch_sub(1, Relaxed);
                    if node.item.read().is_dir() {
                        self.counts.dirs.fetch_sub(1, Relaxed);
                    } else if node.item.read().is_file() {
                        self.counts.files.fetch_sub(1, Relaxed);
                    }
                }
                None => {
                    if self.debug {
                        eprintln!("*** ERROR: stale parent reference for: {:?}", node)
                    };
                    return;
                }
            },
            None => {
                if self.debug {
                    eprintln!("*** WARN: node not found: {path}")
                };
                return;
            }
        }
    }

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

    /// Walk the tree recursively from a Node and return a Vec of child Nodes.
    fn walk(&self, node: Arc<Node>) -> Vec<Arc<Node>> {
        let mut nodes: Vec<Arc<Node>> = Vec::new();
        if node.is_traversable() {
            for (name, child) in node.children.as_ref().unwrap().read().iter() {
                if self.debug {
                    eprintln!("*** Walking Node: {name}");
                }
                nodes.push(child.clone());
                if child.is_traversable() {
                    nodes.extend(self.walk(child.clone()));
                }
            }
        }
        nodes
    }

    /// Iterates the full tree and returns a Vec of all Nodes. WARNING: this can be
    /// slow and memory intensive for large trees. Prefer using `DirTree::iter()`.
    pub fn nodes(&self) -> Vec<Arc<Node>> {
        self.walk(self.root.clone())
    }

    /// Returns a Vec of all `Directory` nodes in the tree.
    pub fn dirs(&self) -> Vec<Arc<Node>> {
        self.nodes()
            .iter()
            .filter_map(|n| match n.node_t {
                NodeType::Directory => Some(n.clone()),
                _ => None,
            })
            .collect()
    }

    /// Returns a Vec of all `File` nodes in the tree.
    pub fn files(&self) -> Vec<Arc<Node>> {
        self.nodes()
            .iter()
            .filter_map(|n| match n.node_t {
                NodeType::File => Some(n.clone()),
                _ => None,
            })
            .collect()
    }

    /// Creates an iterator to walk through all Nodes in the tree.
    pub fn iter(&self) -> DirTreeIterator {
        DirTreeIterator(VecDeque::from(vec![self.root.clone()]))
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
    pub fn iter_paths(&self) -> impl Iterator<Item = String> {
        self.iter()
            .filter_map(|node: Arc<Node>| node.item.read().path_str().map(|s| s.to_owned()))
    }

    /// Traverses recursively from a Node and applies function `f` to each child Node.
    fn traverse_node<F>(&self, node: Arc<Node>, f: &mut F)
    where
        F: FnMut(Arc<Node>),
    {
        f(node.clone());
        if node.is_traversable() {
            for child in node.children.as_ref().unwrap().read().values() {
                if child.is_traversable() {
                    // traverse directories first (depth-first search)
                    self.traverse_node(child.clone(), f);
                } else {
                    f(child.clone());
                }
            }
        }
    }

    /// Traverses the tree and applies function `f` to each Node.
    pub fn traverse<F>(&self, mut f: F)
    where
        F: FnMut(Arc<Node>),
    {
        self.traverse_node(self.root.clone(), &mut f);
    }

    /// Print the full contents of the tree recursively. This is a debugging function.
    pub fn print(&self) {
        eprintln!("\n{:?}\n", self);
        self.traverse(|node: Arc<Node>| {
            if matches!(node.node_t, NodeType::Directory | NodeType::File) {
                match node.item.read().data() {
                    Some(data) => println!("{}", data.path().display()),
                    None => {}
                }
            }
        });
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
                .children
                .as_ref()
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
