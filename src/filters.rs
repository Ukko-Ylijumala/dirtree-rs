// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::osname::encode_os;
use regex::Regex;
use std::ffi::OsStr;

/**
Regex-based name filters for files and directories.

All patterns are unanchored (substring match). Use `^...$` to anchor.
Exclude takes priority: if the exclude pattern matches, the entry is
skipped even when an include pattern also would match.
*/
#[derive(Clone, Default, Debug)]
pub struct Filters {
    pub include_files: Option<Regex>,
    pub exclude_files: Option<Regex>,
    pub include_dirs: Option<Regex>,
    pub exclude_dirs: Option<Regex>,
}

impl Filters {
    /// Build a `Filters` from optional pattern strings, compiling each regex.
    pub fn try_build(
        include_files: Option<String>,
        exclude_files: Option<String>,
        include_dirs: Option<String>,
        exclude_dirs: Option<String>,
    ) -> Result<Self, regex::Error> {
        Ok(Self {
            include_files: include_files.map(|s| Regex::new(&s)).transpose()?,
            exclude_files: exclude_files.map(|s| Regex::new(&s)).transpose()?,
            include_dirs: include_dirs.map(|s| Regex::new(&s)).transpose()?,
            exclude_dirs: exclude_dirs.map(|s| Regex::new(&s)).transpose()?,
        })
    }

    /// Returns `true` if the entry should be processed.
    pub fn passes(&self, name: &OsStr, is_dir: bool) -> bool {
        let (exclude, include): (&Option<Regex>, &Option<Regex>) = match is_dir {
            true => (&self.exclude_dirs, &self.include_dirs),
            false => (&self.exclude_files, &self.include_files),
        };
        // the usual case, no filters: the name is not even looked at
        if exclude.is_none() && include.is_none() {
            return true;
        }
        let name = encode_os(&name);
        if let Some(re) = exclude
            && re.is_match(&name)
        {
            return false;
        }
        if let Some(re) = include
            && !re.is_match(&name)
        {
            return false;
        }
        true
    }
}
