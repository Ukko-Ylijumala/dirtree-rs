// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use custom_xxh3::QuickXxh3Hasher;
use std::hash::BuildHasher;

#[cfg(feature = "size_of")]
use size_of::{Context, SizeOf};

/**
[BuildHasher] for the trie's [`HashMap`](std::collections::HashMap)s. The
keys are interned `u32` name indices, so a hash is one short input:
[QuickXxh3Hasher] hashes it from registers (~1 ns), where a streaming
`Xxh3` costs ~15 ns to set up per map operation. The hashes are the same
as before (custom_xxh3's default secret, seed 0).
*/
#[derive(Default, Debug, Clone, Copy)]
pub struct DirTreeXxh3Hasher;

impl BuildHasher for DirTreeXxh3Hasher {
    type Hasher = QuickXxh3Hasher;

    #[inline(always)]
    fn build_hasher(&self) -> Self::Hasher {
        QuickXxh3Hasher::new()
    }
}

// a hasher is built on the stack per map operation; the builder owns nothing
#[cfg(feature = "size_of")]
impl SizeOf for DirTreeXxh3Hasher {
    fn size_of_children(&self, _context: &mut Context) {}
}
