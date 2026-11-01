// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

//! Small helpers shared by the tree module: path splitting, weak refs, atomics.

#![allow(dead_code)]

use std::{
    any::Any,
    env,
    os::fd::{AsRawFd, BorrowedFd},
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering::Relaxed},
        Arc, Weak,
    },
};

/// Path component separator of the paths the tree stores.
pub(crate) const PATH_SEP: &str = "/";
/// Where a process's open fds can be named by a path (see [proc_fd_path]).
const PROC_SELF_FD: &str = "/proc/self/fd";

/// The `/proc/self/fd` link of `fd`: a path that names what `fd` holds open, for calls that take only a path.
pub(crate) fn proc_fd_path(fd: BorrowedFd<'_>) -> String {
    format!("{PROC_SELF_FD}/{}", fd.as_raw_fd())
}

/**
`path` made absolute and normalized: a relative one is taken relative
to the current directory, as the kernel would, once, here, and `.`
components and repeated or trailing separators are dropped. The trie is
keyed by absolute paths in that form, so a relative walk root would be
filed under `/`-plus-itself and opened relative to wherever the process
is by then, and a `.` in the path would match no node. A `..` is left
as it is (a lookup takes it lexically). Left relative if the current
directory cannot be had.
*/
pub(crate) fn abs_path(path: PathBuf) -> PathBuf {
    let path: PathBuf = match path.is_relative() {
        true => env::current_dir().map(|cwd: PathBuf| cwd.join(&path)).unwrap_or(path),
        false => path,
    };
    path.components().collect()
}

// Define a trait to handle conversion to a Weak reference
pub(crate) trait ToWeak<T> {
    fn to_weak(self) -> Weak<T>;
}

// Implement the trait for owned values
impl<T> ToWeak<T> for T {
    #[inline]
    fn to_weak(self) -> Weak<T> {
        Arc::downgrade(&Arc::new(self))
    }
}

// Implement the trait for Arc<T>
impl<T> ToWeak<T> for Arc<T> {
    #[inline]
    fn to_weak(self) -> Weak<T> {
        Arc::downgrade(&self)
    }
}

/// Create a weak reference. The item can be an owned value or an Arc.
///
/// For owned values, an Arc will be created and then downgraded.
/// For Arc values, the Arc reference will be directly downgraded.
#[inline]
pub(crate) fn make_weak_ref<T, U>(item: U) -> Weak<T>
where
    U: ToWeak<T>,
{
    item.to_weak()
}

/* ######################################################################### */

/// Split a path into parts and skip the first empty string.
#[inline]
pub fn path_parts(path: &str) -> std::iter::Skip<std::str::Split<'_, &str>> {
    path.trim_end_matches(PATH_SEP).split(PATH_SEP).skip(1)
}

/// Split a path into a Vec of parts (skips the first empty string).
#[inline]
pub fn path_parts_vec(path: &str) -> Vec<&str> {
    path_parts(path).collect()
}

/* ######################################################################### */

/// Increment or decrement an [AtomicU32] value in Relaxed mode.
#[inline]
pub fn mod_atom_u32(a: &AtomicU32, n: i32) {
    if n > 0 {
        a.fetch_add(n as u32, Relaxed);
    } else if n < 0 {
        a.fetch_sub(n.unsigned_abs(), Relaxed);
    }
}

/// The message of a thread's panic, from the payload `JoinHandle::join` returns.
pub(crate) fn panic_message(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(msg) => *msg,
        Err(payload) => match payload.downcast_ref::<&str>() {
            Some(msg) => msg.to_string(),
            None => "<non-string panic payload>".to_owned(),
        },
    }
}
