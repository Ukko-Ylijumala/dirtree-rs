// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use custom_xxh3::build_xxh3_with_custom_secret;
use std::hash::BuildHasher;
use xxhash_rust::xxh3::Xxh3;

#[cfg(feature = "size_of")]
use {
    size_of::{Context, SizeOf},
    std::mem::size_of,
};

#[derive(Default, Debug, Clone, Copy)]
pub struct DirTreeXxh3Hasher;

impl BuildHasher for DirTreeXxh3Hasher {
    type Hasher = Xxh3;

    fn build_hasher(&self) -> Self::Hasher {
        build_xxh3_with_custom_secret()
    }
}

#[cfg(feature = "size_of")]
impl SizeOf for DirTreeXxh3Hasher {
    fn size_of_children(&self, context: &mut Context) {
        context.add(size_of::<Xxh3>()).add_distinct_allocation();
    }
}
