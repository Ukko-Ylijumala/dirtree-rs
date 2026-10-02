// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

//! Small helpers shared by the tree module: path splitting, weak refs, atomics.

#![allow(dead_code)]

use std::sync::{
    atomic::{AtomicU32, Ordering::Relaxed},
    Arc, Weak,
};

/// Path component separator of the paths the tree stores.
pub(crate) const PATH_SEP: &str = "/";

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
        a.fetch_sub(n.abs() as u32, Relaxed);
    }
}
