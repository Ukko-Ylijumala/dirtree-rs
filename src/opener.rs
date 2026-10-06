// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
[`FileOpener`]: opening the files of a tree by their node (directory and
interned name), never by a path, with one directory open per run of
files from the same directory.
*/

use super::dirtree::DirTree;
use super::node::Directory;
use super::osname::decode_os;

use dirhandle::{open_regular_at, read_nofollow_at};

use std::{
    borrow::Cow,
    ffi::OsStr,
    fs::File,
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    sync::Arc,
};

/**
Opens files of a [DirTree] relative to their directory's fd. The fd of
the last directory used is kept, so a run of files from one directory
(candidates grouped by directory, or simply in walk order) costs one
directory open in all, and each file one `openat`. Directories are
opened as [DirTree::path_fd] describes: no symlink below a walk root is
ever followed, and depth past `PATH_MAX` does not matter.

Not shared: each thread that reads files holds an opener of its own.
*/
pub struct FileOpener<'t> {
    tree: &'t DirTree,
    /// The directory last opened, and its `O_PATH` fd.
    current: Option<(Arc<Directory>, OwnedFd)>,
}

impl<'t> FileOpener<'t> {
    pub fn new(tree: &'t DirTree) -> Self {
        Self { tree, current: None }
    }

    /**
    An `O_PATH` fd of `dir`, kept until a file of another directory is
    opened: for other `*at` calls (`fstatat`, `readlinkat`, ...) on its
    entries.
    */
    pub fn dir_fd(&mut self, dir: &Arc<Directory>) -> io::Result<BorrowedFd<'_>> {
        let current: (Arc<Directory>, OwnedFd) = match self.current.take() {
            Some(current) if Arc::ptr_eq(&current.0, dir) => current,
            old => {
                drop(old); // closed first: a fresh open never holds two
                (dir.clone(), self.tree.path_fd(dir)?)
            }
        };
        Ok(self.current.insert(current).1.as_fd())
    }

    /**
    Open the regular file `name` (interned) of `dir`, and stat what was
    opened: dirhandle's `open_regular_at()`. No symlink is followed, a
    FIFO does not block, and anything but a regular file (replaced since
    the walk) fails with `InvalidInput`.
    */
    pub fn open_regular(&mut self, dir: &Arc<Directory>, name: u32) -> io::Result<(File, libc::stat)> {
        let name: Cow<OsStr> = self.name(name);
        open_regular_at(self.dir_fd(dir)?, &*name)
    }

    /// Open the entry `name` (interned) of `dir` for reading, whatever it is: dirhandle's `read_nofollow_at()`.
    pub fn read_nofollow(&mut self, dir: &Arc<Directory>, name: u32) -> io::Result<File> {
        let name: Cow<OsStr> = self.name(name);
        read_nofollow_at(self.dir_fd(dir)?, &*name)
    }

    /// The name `idx` decoded to its on-disk bytes.
    fn name(&self, idx: u32) -> Cow<'t, OsStr> {
        decode_os(self.tree.get_string(idx))
    }
}

impl DirTree {
    /**
    Open the regular file `name` (interned) of `dir`, and stat what was
    opened: see [FileOpener::open_regular]. For many files, a
    [FileOpener] opens each directory once.
    */
    pub fn open_file(&self, dir: &Arc<Directory>, name: u32) -> io::Result<(File, libc::stat)> {
        FileOpener::new(self).open_regular(dir, name)
    }
}
