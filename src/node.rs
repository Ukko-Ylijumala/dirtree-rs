// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

/*!
The building blocks of the trie. Directories are shared nodes
([`Arc<Directory>`]); everything else is a [`FileEntry`] stored by value
in its parent directory's children map, as a [`Child`]. A file has no
allocation and no parent pointer of its own: its parent is whichever
directory's map holds it, and its name is the key it is stored under.

[`NodeRef`] (owned) and [`NodeView`] (borrowed) refer to either kind of
entry from outside a children map.
*/

#![allow(dead_code)]

use super::hash::DirTreeXxh3Hasher;
use super::osname::decode_os;
use super::utils::PATH_SEP;
use super::visitor::{SCOPE_NONE, ScopeTag};

use dirhandle::{
    DirFd,
    nix::{dir::Type, sys::stat::SFlag},
};
use stringstore::UniqueStrStore;

use parking_lot::RwLock;
use tracing::error;

use std::{
    collections::HashMap,
    collections::hash_map::Entry as HmEntry,
    fs::FileType,
    hash::{Hash, Hasher},
    os::fd::RawFd,
    os::unix::fs::FileTypeExt,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering::Relaxed},
    sync::{Arc, Weak},
};

#[cfg(feature = "size_of")]
use {
    size_of::{Context, SizeOf},
    std::mem::size_of,
};

// Convenience aliases
pub(super) type Children = RwLock<DirTreeHashMap<u32, Child>>;
pub(super) type NodeIter<'a> = dyn Iterator<Item = NodeRef> + 'a;
pub(super) type DirTreeHashMap<K, V> = HashMap<K, V, DirTreeXxh3Hasher>;

/// [FileEntry::target] of an entry that is not a symlink, or whose target is unknown.
const NO_TARGET: u32 = u32::MAX;

/**
A ctime as a change stamp: nanoseconds since the UNIX epoch (0 for
pre-epoch times, which thus never match as a baseline). ctime alone is
enough - every entry list change moves a directory's mtime *and* ctime,
and setting timestamps (`touch -d`, `utimensat`) bumps ctime to "now".
Stamps come from the filesystem's clock and are only ever compared with
each other, never with the local clock (skew on NFS / SMB / FUSE).
*/
pub(super) fn ctime_stamp(secs: i64, nsecs: i64) -> u64 {
    match secs {
        s if s < 0 => 0,
        s => (s as u64).saturating_mul(1_000_000_000).saturating_add(nsecs as u64),
    }
}

/* ######################################################################### */

/**
A directory node of the trie, shared as `Arc<Directory>`: the tree holds
one per directory, and so may a watcher (by its watch descriptor) or a
caller holding a [NodeRef]. Everything mutable in it is atomic or behind
the children map's lock, so it is changed in place through `&Directory`.
*/
#[derive(Debug)]
pub struct Directory {
    /// The parent directory; [None] for the root.
    parent: Option<Weak<Directory>>,
    /// NOTE: intermediate directories created without a stat have inode 0.
    inode: u64,
    /**
    Change stamp of the last complete scan: the directory's ctime as seen
    just before that scan, in nanoseconds since the UNIX epoch by the
    filesystem's own clock (see [ctime_stamp]); 0 = no baseline. Atomic
    so a diff-rescan can refresh it through the shared `&Directory`.
    */
    stamp: AtomicU64,
    /// Interned name index. Atomic so a rename can re-label the
    /// directory in place through the shared `&Directory`.
    name: AtomicU32,
    /**
    The tag of the last [`Verdict::Tag`](super::Verdict::Tag) a visitor
    gave this directory; [`SCOPE_NONE`] if none did (or no visitor is
    set). Refreshed each time the walker visits the directory.
    */
    tag: AtomicU16,
    /**
    Whether a caller named this directory as a walk root (or as the
    tree's root path): it may be opened by its path, symlinks included,
    and the directories below it are opened from it, never by a path.
    See `DirTree::dir_at`. Sits in what was padding.
    */
    walk_root: AtomicBool,
    fd: DirFd,
    children: Children,
}

impl Directory {
    /// The root directory of a tree (no parent).
    pub(super) fn new_root(name_idx: u32) -> Self {
        Self::with_parent(None, name_idx, 0)
    }

    /// A directory named `name_idx` under `parent`.
    pub(super) fn new(parent: &Arc<Directory>, name_idx: u32, inode: u64) -> Self {
        Self::with_parent(Some(Arc::downgrade(parent)), name_idx, inode)
    }

    fn with_parent(parent: Option<Weak<Directory>>, name_idx: u32, inode: u64) -> Self {
        Self {
            parent,
            inode,
            stamp: AtomicU64::new(0),
            name: AtomicU32::new(name_idx),
            tag: AtomicU16::new(SCOPE_NONE),
            walk_root: AtomicBool::new(false),
            fd: DirFd::default(),
            children: HashMap::with_hasher(DirTreeXxh3Hasher).into(),
        }
    }

    /// Whether this is the root of its tree.
    #[inline]
    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }

    /// The parent directory; [None] for the root, or for a directory
    /// whose parent has been removed from the tree.
    #[inline]
    pub fn parent(&self) -> Option<Arc<Directory>> {
        self.parent.as_ref()?.upgrade()
    }

    #[inline]
    pub fn inode(&self) -> u64 {
        self.inode
    }

    /// Change stamp of the last complete scan (see `ctime_stamp`; 0 = none).
    pub fn scan_stamp(&self) -> u64 {
        self.stamp.load(Relaxed)
    }

    /// Record the change stamp of a complete scan of this directory.
    pub(super) fn set_scan_stamp(&self, stamp: u64) {
        self.stamp.store(stamp, Relaxed);
    }

    /// The directory's interned name index.
    #[inline]
    pub(super) fn name_idx(&self) -> u32 {
        self.name.load(Relaxed)
    }

    /// The directory's name, encoded (see [encode_name](super::encode_name)).
    pub fn name<'a>(&self, store: &'a UniqueStrStore) -> &'a str {
        unsafe { store.borrow_str(self.name_idx()) }
    }

    /// Re-label the directory with a new interned name (rename support).
    pub(super) fn name_set(&self, name_idx: u32) {
        self.name.store(name_idx, Relaxed);
    }

    /// The visitor's tag for this directory, if it gave one (see [DirTree::tagged](super::DirTree::tagged)).
    #[inline]
    pub fn tag(&self) -> Option<ScopeTag> {
        match self.tag.load(Relaxed) {
            SCOPE_NONE => None,
            tag => Some(tag),
        }
    }

    /// Record the visitor's tag for this directory; [`SCOPE_NONE`] clears it.
    #[inline]
    pub(super) fn set_tag(&self, tag: ScopeTag) {
        self.tag.store(tag, Relaxed);
    }

    /// Whether a caller named this directory as a walk root (see the field).
    #[inline]
    pub fn is_walk_root(&self) -> bool {
        self.walk_root.load(Relaxed)
    }

    /// Mark this directory as a walk root named by a caller.
    pub(super) fn set_walk_root(&self) {
        self.walk_root.store(true, Relaxed);
    }

    /// Returns the [[DirFd]] for this [[Directory]].
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

    /// The children map: interned name -> [Child].
    #[inline]
    pub fn children(&self) -> &Children {
        &self.children
    }

    /// Whether we have a child with the given name.
    #[inline]
    pub fn has_child(&self, name_idx: &u32) -> bool {
        self.children.read().contains_key(name_idx)
    }

    /// Get a child by name (a clone: an `Arc` for a directory, a copy for a file).
    #[inline]
    pub fn get_child(&self, name_idx: &u32) -> Option<Child> {
        self.children.read().get(name_idx).cloned()
    }

    /// Get a child directory by name.
    #[inline]
    pub fn get_dir(&self, name_idx: &u32) -> Option<Arc<Directory>> {
        match self.children.read().get(name_idx)? {
            Child::Dir(dir) => Some(dir.clone()),
            Child::File(_) => None,
        }
    }

    /// Add a child, replacing any previous occupant of the name.
    #[inline]
    pub(super) fn add_child(&self, name_idx: u32, child: Child) {
        self.children.write().insert(name_idx, child);
    }

    /**
    Remove the child under `name_idx` only if it still is `child` itself
    (see [Child::same]), checked and removed under one write lock hold.
    Returns `true` if it was removed. A caller acting on an earlier look
    at the slot thus cannot remove an entry recreated there since.
    */
    pub(super) fn remove_child_exact(&self, name_idx: &u32, child: &Child) -> bool {
        let mut ch = self.children.write();
        match ch.get(name_idx) {
            Some(c) if c.same(child) => {
                ch.remove(name_idx);
                true
            }
            _ => false,
        }
    }

    /**
    Atomically get an existing child, or add a new one built by `make`.

    The check-then-insert happens under a single write lock hold, so two
    threads racing to create the same child cannot overwrite each other
    (which would silently drop the loser's descendants). Returns the child
    and whether it was created by this call.
    */
    pub(super) fn get_or_add_child_with<F>(&self, name_idx: u32, make: F) -> (Child, bool)
    where
        F: FnOnce() -> Child,
    {
        let mut ch = self.children.write();
        match ch.entry(name_idx) {
            HmEntry::Occupied(e) => (e.get().clone(), false),
            HmEntry::Vacant(v) => (v.insert(make()).clone(), true),
        }
    }

    /**
    Add a batch of children under a single write lock hold. A name that is
    already occupied keeps its occupant; the new counterpart is dropped.
    Returns how many children were added.
    */
    pub(super) fn add_children_new<I>(&self, children: I) -> u32
    where
        I: IntoIterator<Item = (u32, Child)>,
    {
        let mut ch = self.children.write();
        let mut added: u32 = 0;
        for (name_idx, child) in children {
            if let HmEntry::Vacant(v) = ch.entry(name_idx) {
                v.insert(child);
                added += 1;
            }
        }
        added
    }

    /**
    Make room for `total` children in all, so that a directory filled in
    parallel does not rehash its map (under the write lock, stalling the
    other inserters) as it grows.
    */
    pub(super) fn reserve_children(&self, total: usize) {
        let mut ch = self.children.write();
        let additional: usize = total.saturating_sub(ch.len());
        ch.reserve(additional);
    }

    /**
    The names of this directory and its ancestors up to (not including)
    the root, root-first. [None] if the directory has been detached from
    the tree (it, or an ancestor, removed while still referenced).
    */
    pub(super) fn construct_path(&self, store: &UniqueStrStore) -> Option<Vec<String>> {
        let mut parts: Vec<String> = Vec::new();
        if self.is_root() {
            return Some(parts);
        }
        parts.push(self.name(store).to_string());
        let mut current: Arc<Directory> = self.parent()?;
        while !current.is_root() {
            parts.push(current.name(store).to_string());
            current = current.parent()?;
        }
        parts.reverse();
        Some(parts)
    }

    /// Filesystem path of this directory; `/` for the root, or for a
    /// detached directory (logged), whose path cannot be resolved.
    pub fn path(&self, store: &UniqueStrStore) -> PathBuf {
        let mut path: PathBuf = PathBuf::from(PATH_SEP);
        match self.construct_path(store) {
            Some(parts) => path.extend(parts.iter().map(|p: &String| decode_os(p))),
            None => error!("Cannot construct path for a detached directory: {self:?}"),
        }
        path
    }

    /**
    The memory of this directory alone: the struct and its `Arc` header,
    plus its children map, split into the slots holding files and the
    rest. Returns `(directory bytes, file bytes)`.
    */
    #[cfg(feature = "size_of")]
    pub(super) fn size_immediate(&self) -> (usize, usize) {
        let ch = self.children.read();
        // a hashbrown slot is the entry plus one control byte
        let slot: usize = size_of::<(u32, Child)>() + 1;
        let files: usize = ch.values().filter(|c| !c.is_dir()).count();
        let own: usize = size_of::<Directory>() + 2 * size_of::<usize>();
        (own + slot * (ch.capacity() - files), slot * files)
    }
}

/* ######################################################################### */

/// The entry type a `st_mode` names; [None] for one no file can have.
pub(super) fn mode_type(mode: u32) -> Option<Type> {
    Some(match SFlag::from_bits_truncate(mode) & SFlag::S_IFMT {
        SFlag::S_IFDIR => Type::Directory,
        SFlag::S_IFREG => Type::File,
        SFlag::S_IFLNK => Type::Symlink,
        SFlag::S_IFIFO => Type::Fifo,
        SFlag::S_IFSOCK => Type::Socket,
        SFlag::S_IFCHR => Type::CharacterDevice,
        SFlag::S_IFBLK => Type::BlockDevice,
        _ => return None,
    })
}

/**
What a non-directory entry is, from its directory entry type (`d_type`,
so knowing it costs no syscall). A regular file is [FileKind::File];
everything else is a special file, counted apart from the files (see
[TreeConf::specials](super::TreeConf::specials)). Symlinks are recorded,
never followed.
*/
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FileKind {
    #[default]
    File,
    Symlink,
    Fifo,
    Socket,
    CharDevice,
    BlockDevice,
}

impl FileKind {
    /// The kind of a directory entry type; [None] for a directory.
    pub fn from_type(t: Type) -> Option<Self> {
        Some(match t {
            Type::File => Self::File,
            Type::Symlink => Self::Symlink,
            Type::Fifo => Self::Fifo,
            Type::Socket => Self::Socket,
            Type::CharacterDevice => Self::CharDevice,
            Type::BlockDevice => Self::BlockDevice,
            Type::Directory => return None,
        })
    }

    /// The kind of a [std::fs::FileType] (not followed); [None] for a directory.
    pub fn from_std(t: FileType) -> Option<Self> {
        Some(match t {
            t if t.is_file() => Self::File,
            t if t.is_symlink() => Self::Symlink,
            t if t.is_fifo() => Self::Fifo,
            t if t.is_socket() => Self::Socket,
            t if t.is_char_device() => Self::CharDevice,
            t if t.is_block_device() => Self::BlockDevice,
            _ => return None,
        })
    }

    /// Whether this is anything but a regular file.
    #[inline]
    pub fn is_special(self) -> bool {
        self != Self::File
    }
}

/**
A non-directory entry, stored by value in its parent's children map: its
inode, its [FileKind], and for a symlink the interned index of its target
(read with `readlink` when the symlink is recorded; symlinks never change
in place, so a new target always comes with a new inode). 16 bytes, with
room to spare.
*/
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileEntry {
    inode: u64,
    target: u32,
    kind: FileKind,
}

impl FileEntry {
    /// An entry of `kind`; `target` is the interned target of a symlink.
    pub fn new(inode: u64, kind: FileKind, target: Option<u32>) -> Self {
        Self {
            inode,
            target: target.unwrap_or(NO_TARGET),
            kind,
        }
    }

    #[inline]
    pub fn inode(&self) -> u64 {
        self.inode
    }

    #[inline]
    pub fn kind(&self) -> FileKind {
        self.kind
    }

    /// The interned target of a symlink, if it could be read.
    pub fn target(&self) -> Option<u32> {
        (self.target != NO_TARGET).then_some(self.target)
    }
}

/* ######################################################################### */

/// An entry of a directory's children map.
#[derive(Debug, Clone)]
pub enum Child {
    Dir(Arc<Directory>),
    File(FileEntry),
}

impl Child {
    #[inline]
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir(_))
    }

    #[inline]
    pub fn as_dir(&self) -> Option<&Arc<Directory>> {
        match self {
            Self::Dir(dir) => Some(dir),
            Self::File(_) => None,
        }
    }

    #[inline]
    pub fn as_file(&self) -> Option<&FileEntry> {
        match self {
            Self::Dir(_) => None,
            Self::File(file) => Some(file),
        }
    }

    /// NOTE: intermediate directories created without a stat have inode 0.
    #[inline]
    pub fn inode(&self) -> u64 {
        match self {
            Self::Dir(dir) => dir.inode(),
            Self::File(file) => file.inode(),
        }
    }

    /// The [FileKind] of a file; [None] for a directory.
    #[inline]
    pub fn file_kind(&self) -> Option<FileKind> {
        self.as_file().map(|f: &FileEntry| f.kind())
    }

    /**
    Whether `self` is the very entry `other` is: the same directory node
    (by identity), or an equal file entry. Guards "remove it if it is
    still there" against a slot emptied or re-filled since it was read.
    */
    pub(super) fn same(&self, other: &Child) -> bool {
        match (self, other) {
            (Self::Dir(a), Self::Dir(b)) => Arc::ptr_eq(a, b),
            (Self::File(a), Self::File(b)) => a == b,
            _ => false,
        }
    }
}

/* ######################################################################### */

/**
An owned reference to an entry of the tree, from a lookup or an
iterator. A directory is its shared node; a file is a snapshot of its
entry, with its parent directory and its interned name. Holds no lock.
*/
#[derive(Debug, Clone)]
pub enum NodeRef {
    Dir(Arc<Directory>),
    File {
        parent: Arc<Directory>,
        name: u32,
        file: FileEntry,
    },
}

impl NodeRef {
    #[inline]
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir(_))
    }

    #[inline]
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File { .. })
    }

    #[inline]
    pub fn as_dir(&self) -> Option<&Arc<Directory>> {
        match self {
            Self::Dir(dir) => Some(dir),
            Self::File { .. } => None,
        }
    }

    #[inline]
    pub fn as_file(&self) -> Option<&FileEntry> {
        match self {
            Self::Dir(_) => None,
            Self::File { file, .. } => Some(file),
        }
    }

    /// NOTE: intermediate directories created without a stat have inode 0.
    #[inline]
    pub fn inode(&self) -> u64 {
        match self {
            Self::Dir(dir) => dir.inode(),
            Self::File { file, .. } => file.inode(),
        }
    }

    /// The [FileKind] of a file; [None] for a directory.
    #[inline]
    pub fn file_kind(&self) -> Option<FileKind> {
        self.as_file().map(|f: &FileEntry| f.kind())
    }

    /// The interned name of the entry.
    #[inline]
    pub fn name_idx(&self) -> u32 {
        match self {
            Self::Dir(dir) => dir.name_idx(),
            Self::File { name, .. } => *name,
        }
    }

    /// The parent directory; [None] for the root (or a detached directory).
    pub fn parent(&self) -> Option<Arc<Directory>> {
        match self {
            Self::Dir(dir) => dir.parent(),
            Self::File { parent, .. } => Some(parent.clone()),
        }
    }

    /// Filesystem path of the entry (see [Directory::path]).
    pub fn path(&self, store: &UniqueStrStore) -> PathBuf {
        self.view().path(store)
    }

    /// The entry as it is stored in its parent's map.
    pub(super) fn to_child(&self) -> Child {
        match self {
            Self::Dir(dir) => Child::Dir(dir.clone()),
            Self::File { file, .. } => Child::File(*file),
        }
    }

    /// The borrowed form of this reference.
    pub fn view(&self) -> NodeView<'_> {
        match self {
            Self::Dir(dir) => NodeView::Dir(dir),
            Self::File { parent, name, file } => NodeView::File { parent, name: *name, file },
        }
    }
}

// identity: the same directory node, or the same file entry under the same name and parent
impl PartialEq for NodeRef {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Dir(a), Self::Dir(b)) => Arc::ptr_eq(a, b),
            (
                Self::File { parent: pa, name: na, file: fa },
                Self::File { parent: pb, name: nb, file: fb },
            ) => Arc::ptr_eq(pa, pb) && na == nb && fa == fb,
            _ => false,
        }
    }
}

impl Eq for NodeRef {}

// consistent with PartialEq: hashes the identity, not the subtree
impl Hash for NodeRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Dir(dir) => Arc::as_ptr(dir).hash(state),
            Self::File { parent, name, file } => {
                Arc::as_ptr(parent).hash(state);
                name.hash(state);
                file.hash(state);
            }
        }
    }
}

/**
A borrowed reference to an entry of the tree, as a traversal visits it:
no reference counting and no copies. A traversal holds the read lock of
the directory whose children it is visiting while calling back, so the
callback must not modify that directory (it would deadlock).
*/
#[derive(Debug, Clone, Copy)]
pub enum NodeView<'a> {
    Dir(&'a Arc<Directory>),
    File {
        parent: &'a Arc<Directory>,
        name: u32,
        file: &'a FileEntry,
    },
}

impl NodeView<'_> {
    #[inline]
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir(_))
    }

    #[inline]
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File { .. })
    }

    #[inline]
    pub fn as_dir(&self) -> Option<&Arc<Directory>> {
        match self {
            Self::Dir(dir) => Some(dir),
            Self::File { .. } => None,
        }
    }

    #[inline]
    pub fn as_file(&self) -> Option<&FileEntry> {
        match self {
            Self::Dir(_) => None,
            Self::File { file, .. } => Some(file),
        }
    }

    /// The [FileKind] of a file; [None] for a directory.
    #[inline]
    pub fn file_kind(&self) -> Option<FileKind> {
        self.as_file().map(|f: &FileEntry| f.kind())
    }

    /// Filesystem path of the entry (see [Directory::path]).
    pub fn path(&self, store: &UniqueStrStore) -> PathBuf {
        match self {
            Self::Dir(dir) => dir.path(store),
            Self::File { parent, name, .. } => {
                parent.path(store).join(decode_os(unsafe { store.borrow_str(*name) }))
            }
        }
    }

    /// The owned form of this reference.
    pub fn to_ref(&self) -> NodeRef {
        match *self {
            Self::Dir(dir) => NodeRef::Dir(dir.clone()),
            Self::File { parent, name, file } => {
                NodeRef::File { parent: parent.clone(), name, file: *file }
            }
        }
    }
}

/* ######################################################################### */

// a parentless directory: what a defaulted tree starts from
impl Default for Directory {
    fn default() -> Self {
        Self::new_root(0)
    }
}

#[cfg(feature = "size_of")]
impl SizeOf for Directory {
    fn size_of_children(&self, context: &mut Context) {
        self.children.read().size_of_children(context);
    }
}

#[cfg(feature = "size_of")]
impl SizeOf for Child {
    fn size_of_children(&self, context: &mut Context) {
        if let Self::Dir(dir) = self {
            dir.size_of_children(context);
        }
    }
}

#[cfg(feature = "size_of")]
impl SizeOf for FileEntry {
    fn size_of_children(&self, _context: &mut Context) {}
}

#[cfg(feature = "size_of")]
impl SizeOf for NodeRef {
    fn size_of_children(&self, context: &mut Context) {
        match self {
            Self::Dir(dir) => dir.size_of_children(context),
            Self::File { parent, .. } => parent.size_of_children(context),
        }
    }
}
