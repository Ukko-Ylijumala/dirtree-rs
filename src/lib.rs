// Copyright (c) 2024 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use crate::{metadata, HashMap, Instant, Metadata, MetadataExt, PathBuf, VecDeque};
use std::{
    cmp::Ordering,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind},
    ptr,
};

const PATH_SEP: char = '/';

#[derive(Debug, Clone, Eq)]
struct Data {
    path: PathBuf,
    inode: u64,
    mode: u32,
    scanned: u64,
    when: Instant,
}

// Path and inode are enough to uniquely identify a file or directory.
impl Hash for Data {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.inode.hash(state);
    }
}

impl PartialEq for Data {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.inode == other.inode
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

// ######################################################################### //

trait DirectoryEntry {
    fn data(&self) -> &Data;
    fn data_mut(&mut self) -> &mut Data;

    fn stat(&self) -> Option<Metadata> {
        match metadata(self.data().path.as_path()) {
            Ok(meta) => return Some(meta),
            Err(_) => return None,
        };
    }

    fn rescan(&mut self) -> Result<(), ()> {
        let meta: Metadata = match self.stat() {
            Some(m) => m,
            // failed to get metadata, likely deleted in the meantime
            None => return Err(()),
        };
        if self.data().inode != meta.ino() {
            // inode changed, file/dir was replaced and we're out of sync
            // this case must be handled by the caller
            return Err(());
        }
        if self.data().mode != meta.mode() {
            self.data_mut().mode = meta.mode();
        }
        self.data_mut().scanned += 1;
        self.data_mut().when = Instant::now();
        Ok(())
    }

    fn parent(&self) -> PathBuf {
        self.data().path.parent().unwrap().to_path_buf()
    }

    fn name(&self) -> String {
        self.data()
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }
}

// ######################################################################### //

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Directory(Data);

impl Directory {
    pub fn new(path: PathBuf, meta: Option<Metadata>) -> Result<Self, Error> {
        let meta: Option<Metadata> = meta.or_else(|| metadata(&path).ok());
        match meta {
            Some(m) => Ok(Self(Data {
                path,
                inode: m.ino(),
                mode: m.mode(),
                scanned: 1,
                when: Instant::now(),
            })),
            None => Err(Error::new(ErrorKind::NotFound, "Failed to get metadata")),
        }
    }
}

impl DirectoryEntry for Directory {
    fn data(&self) -> &Data {
        &self.0
    }

    fn data_mut(&mut self) -> &mut Data {
        &mut self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct File(Data);

impl File {
    pub fn new(path: PathBuf, meta: Option<Metadata>) -> Result<Self, Error> {
        let meta: Option<Metadata> = meta.or_else(|| metadata(&path).ok());
        match meta {
            Some(m) => Ok(Self(Data {
                path,
                inode: m.ino(),
                mode: m.mode(),
                scanned: 1,
                when: Instant::now(),
            })),
            None => Err(Error::new(ErrorKind::NotFound, "Failed to get metadata")),
        }
    }
}

impl DirectoryEntry for File {
    fn data(&self) -> &Data {
        &self.0
    }

    fn data_mut(&mut self) -> &mut Data {
        &mut self.0
    }
}

// ######################################################################### //

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeItem {
    Root,
    Dir(Directory),
    File(File),
    #[default]
    Uninitialized,
    AsDir,  // placeholder for a directory
    AsFile, // placeholder for a file
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

    /// Returns `true` if the node item is [`Uninitialized`].
    ///
    /// [`Uninitialized`]: NodeItem::Uninitialized
    #[must_use]
    pub fn is_uninit(&self) -> bool {
        matches!(self, Self::Uninitialized)
    }

    pub fn as_dir(&self) -> Option<&Directory> {
        if let Self::Dir(v) = self {
            Some(v)
        } else {
            None
        }
    }

    pub fn as_file(&self) -> Option<&File> {
        if let Self::File(v) = self {
            Some(v)
        } else {
            None
        }
    }
}

impl From<Directory> for NodeItem {
    fn from(v: Directory) -> Self {
        Self::Dir(v)
    }
}

impl From<File> for NodeItem {
    fn from(v: File) -> Self {
        Self::File(v)
    }
}

// ######################################################################### //

/// Node in the trie structure for storing paths.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Node {
    item: NodeItem,
    children: HashMap<String, Node>,
}

impl Node {
    /// Returns a new node with the given item.
    pub fn new(item: NodeItem) -> Self {
        Self {
            item,
            ..Default::default()
        }
    }

    /// Returns `true` if the node is traversable.
    pub fn is_traversable(&self) -> bool {
        matches!(
            self.item,
            NodeItem::Dir(_) | NodeItem::Uninitialized | NodeItem::Root
        )
    }
}

// ######################################################################### //

/// Trie structure for storing a directory tree.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct DirTree {
    from: PathBuf,
    nodes: u64,
    debug: bool,
    root: Node,
    pub dirs: u32,
    pub files: u64,
}

impl DirTree {
    /// Creates a new empty directory tree (trie).
    pub fn new() -> Self {
        let mut tree: DirTree = Self::default();
        tree.root.item = NodeItem::Root;
        tree
    }

    /// Creates a new tree with the given path as root.
    pub fn new_from_path(path: &str, debug: bool) -> Self {
        let mut tree: DirTree = DirTree {
            from: PathBuf::from(path),
            root: Node::new(NodeItem::Root),
            debug,
            ..Default::default()
        };
        tree.insert(&tree.from.clone(), NodeItem::AsDir, None);
        if debug {
            eprintln!("Created tree: {:?}", tree)
        };
        tree
    }

    /// Populates a new tree recursively, starting from the given directory. This
    /// function will walk the full directory tree and insert each path into the trie.
    pub fn new_from_path_recursive(path: &str, debug: bool) -> Self {
        let mut tree: DirTree = Self::new_from_path(&path, debug);
        tree.populate(&tree.from.clone(), true);
        tree
    }

    /// Populate a leaf node in the trie with the contents of a directory.
    /// NOTE: single threaded, potentially slow with large directory trees.
    pub fn populate(&mut self, path: &PathBuf, recursive: bool) {
        if self.debug {
            eprintln!("* Populating: {}", path.to_string_lossy())
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
                        eprintln!("**     Found: {} ", path.to_string_lossy())
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
                self.insert(&path, NodeItem::AsDir, Some(meta));
                if recursive {
                    self.populate(&path, recursive);
                }
            } else if meta.is_file() {
                self.insert(&path, NodeItem::AsFile, Some(meta));
            }
        }
    }

    /// Inserts a path into the trie. The path must be an absolute filesystem path.
    /// The path is split on forward slash ("/") and the first empty string discarded.
    pub fn insert(&mut self, path: &PathBuf, node_t: NodeItem, meta: Option<Metadata>) {
        let mut current: &mut Node = &mut self.root;
        for part in path_parts(&path.to_string_lossy()) {
            current = current.children.entry(part.to_owned()).or_default();
        }
        if matches!(current.item, NodeItem::Dir(_) | NodeItem::File(_)) {
            // for now we don't overwrite existing nodes, but
            // this may change in the future to allow for updates
            return;
        }
        current.item = match node_t {
            NodeItem::AsDir => {
                let d: NodeItem = NodeItem::Dir(Directory::new(path.clone(), meta).unwrap());
                self.dirs += 1;
                d
            }
            NodeItem::AsFile => {
                let f: NodeItem = NodeItem::File(File::new(path.clone(), meta).unwrap());
                self.files += 1;
                f
            }
            _ => return,
        };
        self.nodes += 1;
        if self.debug {
            eprintln!("*** Inserted: {:?}", current)
        };
    }

    /// Remove a Node (or a leaf) from the trie. Expects an absolute path.
    ///
    /// NOTE: uses unsafe code to work with raw pointers.
    ///
    /// WARNING: implementation is WIP and may yet contain bugs.
    pub fn remove(&mut self, path: &str) {
        let mut current: *mut Node = &mut self.root;
        let mut parent: *mut Node = ptr::null_mut();
        let mut name: &str = "";
        unsafe {
            for part in path_parts(path) {
                parent = current;
                match (*current).children.get_mut(part) {
                    Some(node) => {
                        current = node as *mut Node;
                        name = part;
                    }
                    None => return, // node not found
                }
            }
            if !current.is_null() && !parent.is_null() {
                eprintln!("*** Removing ***\n{:?}", (*current));
                eprintln!("\n*** Parent before ***\n{:?}", (*parent));
                (*current).item = NodeItem::Uninitialized;
                (*parent).children.remove(name);
                eprintln!("\n*** Parent after ***\n{:?}", (*parent));
                self.nodes -= 1;
            } else if current.is_null() {
                eprintln!("*** ERROR: 'current' ptr is null, expected &Node");
            } else if parent.is_null() {
                eprintln!("*** ERROR: 'parent' ptr is null, expected &Node");
            }
        }
    }

    /// Get a node from the trie. Expects an absolute path.
    fn get_node(&self, path: &str) -> Option<&Node> {
        // short circuit if the path is not absolute or does not look like a path
        if !path.contains(PATH_SEP) || !path.starts_with(PATH_SEP) {
            return None;
        }
        let mut current: &Node = &self.root;
        for part in path_parts(path) {
            match current.children.get(part) {
                Some(node) => current = node,
                None => return None,
            }
        }
        Some(&current) // found the node
    }

    /// Checks if a given path exists in the trie. Expects an absolute path.
    pub fn contains(&self, path: &str) -> bool {
        self.get_node(path).is_some()
    }

    /// Returns the item at the given path. Expects an absolute path.
    pub fn item(&self, path: &str) -> Option<&NodeItem> {
        let node: &Node = match self.get_node(path) {
            Some(n) => n,
            None => return None,
        };
        Some(&node.item)
    }

    /// Walk the tree recursively from a Node and return a Vec of child Nodes.
    fn walk<'a>(&self, node: &'a Node) -> Vec<&'a Node> {
        let mut nodes: Vec<&'a Node> = Vec::new();
        for (name, child) in node.children.iter() {
            if self.debug {
                eprintln!("*** Walking Node: {name}");
            }
            nodes.push(child);
            if child.is_traversable() {
                nodes.extend(self.walk(child));
            }
        }
        nodes
    }

    /// Iterates the full tree and returns a Vec of all Nodes. WARNING: this can be
    /// slow and memory intensive for large trees. Prefer using `DirTree::iter()`.
    pub fn nodes(&self) -> Vec<&Node> {
        self.walk(&self.root)
    }

    /// Returns a Vec of all `Directory` items in the tree.
    pub fn dirs(&self) -> Vec<&Directory> {
        self.nodes()
            .iter()
            .filter_map(|n| {
                if n.item.is_dir() {
                    n.item.as_dir()
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns a Vec of all `File` items in the tree.
    pub fn files(&self) -> Vec<&File> {
        self.nodes()
            .iter()
            .filter_map(|n| {
                if n.item.is_file() {
                    n.item.as_file()
                } else {
                    None
                }
            })
            .collect()
    }

    /// Creates an iterator to walk through all Nodes in the tree.
    pub fn iter(&self) -> DirTreeIterator {
        DirTreeIterator(VecDeque::from(vec![&self.root]))
    }

    /// An iterator over all `Directory` items in the tree.
    pub fn iter_dirs(&self) -> impl Iterator<Item = &Directory> {
        self.iter().filter_map(|node| {
            if node.item.is_dir() {
                node.item.as_dir()
            } else {
                None
            }
        })
    }

    /// An iterator over all `File` items in the tree.
    pub fn iter_files(&self) -> impl Iterator<Item = &File> {
        self.iter().filter_map(|node| {
            if node.item.is_file() {
                node.item.as_file()
            } else {
                None
            }
        })
    }

    /// Traverses recursively from a Node and applies function `f` to each child Node.
    fn traverse_node<F>(&self, node: &Node, f: &mut F)
    where
        F: FnMut(&Node),
    {
        f(node);
        for child in node.children.values() {
            if child.is_traversable() {
                // traverse directories first (depth-first search)
                self.traverse_node(child, f);
            } else {
                f(child);
            }
        }
    }

    /// Traverses the tree and applies function `f` to each Node.
    pub fn traverse<F>(&self, mut f: F)
    where
        F: FnMut(&Node),
    {
        self.traverse_node(&self.root, &mut f);
    }

    /// Print the full contents of the tree recursively. This is a debugging function.
    pub fn print(&self) {
        eprintln!("{:?}\n", self);
        self.traverse(|node| {
            if node.item.is_file() {
                println!("{}", node.item.as_file().unwrap().data().path.display());
            } else if node.item.is_dir() {
                println!("{}", node.item.as_dir().unwrap().data().path.display());
            }
        });
    }
}

// ######################################################################### //

/// Iterator for walking through a DirTree.
pub struct DirTreeIterator<'a>(VecDeque<&'a Node>);

impl<'a> Iterator for DirTreeIterator<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop_front().map(|node| {
            // Push all children that are traversable to the stack
            for child in node
                .children
                .values()
                .collect::<Vec<&Node>>()
                .into_iter()
                .rev()
            {
                if child.is_traversable() {
                    // push directories to the front of the queue...
                    self.0.push_front(child);
                } else {
                    // ...and files to the back
                    self.0.push_back(child);
                }
            }
            node
        })
    }
}

// ######################################################################### //

/// Split a path into parts and skip the first empty string.
#[inline]
fn path_parts(path: &str) -> std::iter::Skip<std::str::Split<char>> {
    path.trim_end_matches(PATH_SEP).split(PATH_SEP).skip(1)
}
