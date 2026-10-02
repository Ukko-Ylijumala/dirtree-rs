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
    /// No root path was set: chain `from_path()` before `build()`.
    NoRoot,
    /**
    A visitor was configured, but the scan state asks for sync mode. The
    visitor protocol is parallel-walker only; the sync walker would never
    invoke it, so this is almost certainly a caller bug.
    */
    VisitorInSyncMode,
    /// The background worker thread could not be started.
    WorkerSpawn(io::Error),
}

impl Display for TreeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRoot => f.write_str("root path must be set before building the tree"),
            Self::VisitorInSyncMode => f.write_str(
                "a visitor was configured, but sync mode is on: \
                 the visitor protocol is parallel-walker only",
            ),
            Self::WorkerSpawn(e) => write!(f, "failed to start the tree worker thread: {e}"),
        }
    }
}

impl Error for TreeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WorkerSpawn(e) => Some(e),
            _ => None,
        }
    }
}
