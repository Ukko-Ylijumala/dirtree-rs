// Copyright (c) 2024 Mikko Tanner. All rights reserved.

#![allow(dead_code)]

use crate::{metadata, Metadata, PathBuf};
use std::collections::HashMap;

const PATH_SEP: char = '/';

trait DirectoryEntry {
    fn path(&self) -> &PathBuf;
    fn inode(&self) -> u64;

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
pub struct Directory {
    path: PathBuf,
    inode: u64,
}

impl Directory {
    pub fn new(path: PathBuf, inode: u64) -> Self {
        Self { path, inode }
    }
}

impl DirectoryEntry for Directory {
    fn path(&self) -> &PathBuf {
        &self.path
    }

    fn inode(&self) -> u64 {
        self.inode
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct File {
    path: PathBuf,
    inode: u64,
}

impl File {
    pub fn new(path: PathBuf, inode: u64) -> Self {
        Self { path, inode }
    }
}

impl DirectoryEntry for File {
    fn path(&self) -> &PathBuf {
        &self.path
    }

    fn inode(&self) -> u64 {
        self.inode
    }
}

// ######################################################################### //

// Enum to distinguish between file types
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EntryType {
    Directory,
    File,
}

// Unified struct for both directories and files
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileSystemEntry {
    pub path: PathBuf,
    inode: u64,
    etype: EntryType,
}

impl FileSystemEntry {
    pub fn new(path: PathBuf, inode: u64, etype: EntryType) -> Self {
        Self { path, inode, etype }
    }

    pub fn inode(&self) -> u64 {
        self.inode
    }

    pub fn stat(&self) -> Metadata {
        metadata(self.path.as_path()).unwrap()
    }

    pub fn parent(&self) -> PathBuf {
        self.path.parent().unwrap().to_path_buf()
    }

    pub fn name(&self) -> String {
        self.path.file_name().unwrap().to_string_lossy().to_string()
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.etype, EntryType::Directory)
    }

    pub fn is_file(&self) -> bool {
        matches!(self.etype, EntryType::File)
    }
}

// ######################################################################### //

/// Node in the trie structure for storing paths.
#[derive(Default, Debug)]
struct Node {
    children: HashMap<String, Node>,
    is_leaf: bool,
}

/// Trie structure for storing a directory tree.
#[derive(Default, Debug)]
struct DirTree {
    root: Node,
}

impl DirTree {
    /// Inserts a path into the trie.
    pub fn insert(&mut self, path: &str) {
        let mut current = &mut self.root;
        for part in path.split(PATH_SEP) {
            current = current.children.entry(part.to_owned()).or_default();
        }
        current.is_leaf = true;
    }

    /// Checks if a given path exists in the trie.
    pub fn contains(&self, path: &str) -> bool {
        let mut current = &self.root;
        for part in path.split(PATH_SEP) {
            match current.children.get(part) {
                Some(node) => current = node,
                None => return false,
            }
        }
        current.is_leaf
    }
}
