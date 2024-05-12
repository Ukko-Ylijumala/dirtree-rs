// Copyright (c) 2024 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use crate::{metadata, Instant, Metadata, MetadataExt, PathBuf};
use std::{
    cmp::Ordering,
    collections::HashMap,
    hash::{Hash, Hasher},
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

trait DirectoryEntry {
    fn path(&self) -> &PathBuf;
    fn inode(&self) -> u64;
    fn scanned(&self) -> u64;
    fn when(&self) -> Instant;

    fn stat(&self) -> Metadata {
        metadata(self.path().as_path()).unwrap()
    }

    fn parent(&self) -> PathBuf {
        self.path().parent().unwrap().to_path_buf()
    }

    fn name(&self) -> String {
        self.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Directory(Data);

impl Directory {
    pub fn new(path: PathBuf, meta: Option<Metadata>) -> Self {
        let meta: Metadata = match meta {
            Some(m) => m,
            None => metadata(path.as_path()).unwrap(),
        };
        Self(Data {
            path,
            inode: meta.ino(),
            mode: meta.mode(),
            scanned: 1,
            when: Instant::now(),
        })
    }
}

impl DirectoryEntry for Directory {
    fn path(&self) -> &PathBuf {
        &self.0.path
    }

    fn inode(&self) -> u64 {
        self.0.inode
    }

    fn scanned(&self) -> u64 {
        self.0.scanned
    }

    fn when(&self) -> Instant {
        self.0.when
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct File(Data);

impl File {
    pub fn new(path: PathBuf, meta: Option<Metadata>) -> Self {
        let meta: Metadata = match meta {
            Some(m) => m,
            None => metadata(path.as_path()).unwrap(),
        };
        Self(Data {
            path,
            inode: meta.ino(),
            mode: meta.mode(),
            scanned: 1,
            when: Instant::now(),
        })
    }
}

impl DirectoryEntry for File {
    fn path(&self) -> &PathBuf {
        &self.0.path
    }

    fn inode(&self) -> u64 {
        self.0.inode
    }

    fn scanned(&self) -> u64 {
        self.0.scanned
    }

    fn when(&self) -> Instant {
        self.0.when
    }
}

// ######################################################################### //

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeType {
    RootNode,
    #[default]
    Directory,
    File,
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
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeItem {
    Dir(Directory),
    File(File),
    Root,
    #[default]
    Uninitialized,
}

impl NodeItem {
    /// Returns `true` if the node item is [`Uninitialized`].
    ///
    /// [`Uninitialized`]: NodeItem::Uninitialized
    #[must_use]
    pub fn is_uninit(&self) -> bool {
        matches!(self, Self::Uninitialized)
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

/// Node in the trie structure for storing paths.
#[derive(Default, Debug, Clone, PartialEq)]
struct Node {
    node_t: NodeType,
    item: NodeItem,
    children: HashMap<String, Node>,
}

/// Trie structure for storing a directory tree.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct DirTree {
    from: PathBuf,
    nodes: u64,
    debug: bool,
    root: Node,
}

impl DirTree {
    /// Creates a new empty directory tree (trie).
    pub fn new() -> Self {
        let mut tree: DirTree = Self::default();
        tree.root.node_t = NodeType::RootNode;
        tree.root.item = NodeItem::Root;
        tree
    }

    /// Creates a new tree with the given path as root.
    pub fn new_from_path(path: &str, debug: bool) -> Self {
        let mut tree: DirTree = DirTree {
            from: PathBuf::from(path),
            root: Node {
                node_t: NodeType::RootNode,
                item: NodeItem::Root,
                ..Default::default()
            },
            debug,
            ..Default::default()
        };
        tree.insert(&tree.from.clone(), NodeType::Directory, None);
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
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path: PathBuf = entry.path();
            if self.debug {
                eprintln!("**     Found: {} ", path.to_string_lossy())
            };
            let meta = entry.metadata().ok();
            if path.is_dir() {
                self.insert(&path, NodeType::Directory, meta);
                if recursive {
                    self.populate(&path, recursive);
                }
            } else if path.is_file() {
                self.insert(&path, NodeType::File, meta);
            }
        }
    }

    /// Inserts a path into the trie. The path must be an absolute filesystem path.
    /// The path is split on forward slash ("/") and the first empty string discarded.
    pub fn insert(&mut self, path: &PathBuf, node_t: NodeType, meta: Option<Metadata>) {
        let mut current: &mut Node = &mut self.root;
        for part in path.to_string_lossy().split(PATH_SEP).skip(1) {
            current = current.children.entry(part.to_owned()).or_default();
        }
        current.node_t = node_t;
        match current.node_t {
            NodeType::Directory => {
                current.item = NodeItem::Dir(Directory::new(path.clone(), meta));
            }
            NodeType::File => {
                current.item = NodeItem::File(File::new(path.clone(), meta));
            }
            _ => {}
        }
        self.nodes += 1;
        if self.debug {
            eprintln!("*** Inserted: {:?}", current)
        };
    }

    /// Checks if a given path exists in the trie. Expects an absolute path.
    pub fn contains(&self, path: &str) -> bool {
        let mut current: &Node = &self.root;
        for part in path.trim_end_matches(PATH_SEP).split(PATH_SEP).skip(1) {
            match current.children.get(part) {
                Some(node) => current = node,
                None => return false,
            }
        }
        true // found the path
    }

    /// Walk the tree recursively from a node and print each path. This is a debugging function.
    pub(self) fn walk(&self, node: &Node, path: &str) {
        if matches!(node.node_t, NodeType::Directory | NodeType::File) {
            // if node.end_of_leaf {
            println!("{}", path);
        }
        for (part, child) in &node.children {
            let mut new_path: String = path.to_owned();
            new_path.push_str(part);
            if child.node_t.is_dir() {
                new_path.push(PATH_SEP);
            }
            self.walk(child, &new_path);
        }
    }

    /// Print the contents of the tree. The tree is walked recursively starting from the root node.
    /// This is a debugging function.
    pub fn print(&self) {
        eprintln!("{:?}", self);
        self.walk(&self.root, &PATH_SEP.to_string());
    }
}

impl Iterator for DirTree {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        todo!("DirTree iterator will be implemented later.")
    }
}
