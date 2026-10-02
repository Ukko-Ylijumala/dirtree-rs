// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

//! [`FileMode`]: how a [`DirTree`](super::DirTree) walk handles the files it finds.

use std::{
    fmt::{self, Debug, Display, Formatter},
    ops::{BitAnd, BitOr, BitXor, Deref, DerefMut},
    str::FromStr,
};

/// Bitmap of options for file handling during directory tree scanning.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileMode(u8);

impl FileMode {
    pub const UNSET: FileMode = FileMode(0b0);
    /// Create a full Trie node for each file.
    pub const NODE: FileMode = FileMode(0b1);
    /// Only store the names of files in Trie parent nodes.
    pub const NAME: FileMode = FileMode(0b10);
    /// Just stat each file found.
    pub const STAT: FileMode = FileMode(0b100);
    /// Ignore files found during directory tree scanning.
    pub const IGNORE: FileMode = FileMode(0b1000);
    // encode the "--size" flag
    pub const SIZE: FileMode = FileMode(0b10000000);

    /// Whether the "node" option is set.
    pub fn is_node(&self) -> bool {
        self.0 & Self::NODE.0 != 0
    }

    /// Whether the "name" option is set.
    pub fn is_name(&self) -> bool {
        self.0 & Self::NAME.0 != 0
    }

    /// Whether the "stat" option is set.
    pub fn is_stat(&self) -> bool {
        self.0 & Self::STAT.0 != 0
    }

    /// Whether files should be fully ignored.
    pub fn is_ignore(&self) -> bool {
        self.0 & Self::IGNORE.0 != 0
    }

    /// Whether the "size" option is set.
    pub fn is_with_size(&self) -> bool {
        self.0 & Self::SIZE.0 != 0
    }
}

impl Debug for FileMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "FileMode({}: {self})", self.0)
    }
}

// `to_string()` comes from this impl (via ToString)
impl Display for FileMode {
    #[rustfmt::skip]
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if *self == Self::UNSET {
            return f.write_str("Unset");
        }

        let mut parts: Vec<&str> = Vec::new();
        if self.is_node() { parts.push("Node"); }
        if self.is_name() { parts.push("Name"); }
        if self.is_stat() { parts.push("Stat"); }
        if self.is_ignore() { parts.push("Ignore"); }
        if self.is_with_size() { parts.push("Size"); }
        f.write_str(&parts.join("|"))
    }
}

impl Deref for FileMode {
    type Target = u8;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for FileMode {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl BitAnd for FileMode {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        FileMode(*self & *rhs)
    }
}

impl BitOr for FileMode {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        FileMode(*self | *rhs)
    }
}

impl BitXor for FileMode {
    type Output = Self;

    fn bitxor(self, rhs: Self) -> Self::Output {
        FileMode(*self ^ *rhs)
    }
}

impl From<FileMode> for u8 {
    fn from(mode: FileMode) -> Self {
        *mode
    }
}

impl From<u8> for FileMode {
    fn from(bits: u8) -> Self {
        FileMode(bits)
    }
}

impl Default for FileMode {
    fn default() -> Self {
        FileMode(0) // unset
    }
}

// Parse one mode by name, case-insensitively (SIZE is a separate flag).
impl FromStr for FileMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "node" => Ok(FileMode::NODE),
            "name" => Ok(FileMode::NAME),
            "stat" => Ok(FileMode::STAT),
            "ignore" => Ok(FileMode::IGNORE),
            _ => Err(format!("Invalid file mode: {}", s)),
        }
    }
}
