// Copyright (c) 2026 Mikko Tanner. All rights reserved.

//! [`TreeError`]: errors from the [`DirTree`](super::DirTree) API.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
};

pub type TreeResult<T> = Result<T, TreeError>;

/// Errors from setting up and running a [`DirTree`](super::DirTree).
#[derive(Debug)]
pub enum TreeError {
    /// No root path was set: chain `from_path()` before `walk()` / `build()`.
    NoRoot,
    /// The background worker thread could not be started.
    WorkerSpawn(io::Error),
    /// The background worker thread panicked; holds the panic message.
    WorkerPanicked(String),
    /**
    A background operation was requested, but no worker thread runs it:
    the tree was not built with [`build()`](super::DirTree::build), or
    its worker was stopped, is quitting or has died.
    */
    WorkerNotRunning,
    /// Reading or writing a snapshot file failed.
    Io(io::Error),
    /// A snapshot file failed a check of its format, integrity or fit (see `docs/snapshot.md`).
    BadSnapshot(String),
    /// A snapshot loads only into a tree that is uninitialized or empty.
    NotEmpty,
}

impl Display for TreeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRoot => f.write_str("root path must be set before building the tree"),
            Self::WorkerSpawn(e) => write!(f, "failed to start the tree worker thread: {e}"),
            Self::WorkerPanicked(msg) => write!(f, "the tree worker thread panicked: {msg}"),
            Self::WorkerNotRunning => f.write_str("the tree worker thread is not running"),
            Self::Io(e) => write!(f, "snapshot I/O failed: {e}"),
            Self::BadSnapshot(reason) => write!(f, "bad snapshot: {reason}"),
            Self::NotEmpty => f.write_str("a snapshot loads only into an empty tree"),
        }
    }
}

impl From<io::Error> for TreeError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl Error for TreeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WorkerSpawn(e) | Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
