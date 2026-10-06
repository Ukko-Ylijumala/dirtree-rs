// Copyright (c) 2026 Mikko Tanner. All rights reserved.

/*!
Snapshots: a [`DirTree`] saved to a file, so that a later process starts
from it instead of walking again: load it, then [`DirTree::update`] the
root, which lists only the directories that changed since the save. The
format and its checks are described in `docs/snapshot.md`.
*/

use super::conf::NodeCounts;
use super::dirtree::{DirTree, NewChild};
use super::error::{TreeError, TreeResult};
use super::event::{FaultKind, TreeEvent, TreeOp, TreeState};
use super::filemode::FileMode;
use super::node::{Child, Directory, FileEntry, FileKind, NO_TARGET};
use super::osname::encode_os;
use super::visitor::SCOPE_NONE;

use custom_xxh3::CustomXxh3Hasher;
use timesince::SecondsSinceEpoch;

use std::{
    ffi::OsStr,
    fs::{File, metadata, rename},
    hash::Hasher,
    io::{self, BufReader, BufWriter, ErrorKind, Read, Write},
    os::unix::ffi::OsStrExt,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process,
    sync::Arc,
};

const MAGIC: &[u8; 8] = b"DIRTREE\0";
const END: &[u8; 8] = b"DTREEEND";
const VERSION: u16 = 1;
/// The first user string index: stringstore's built-in Latin-1 entries come before it.
const STRING_BASE: u32 = 256;
/// Directory record flag: a walk root named by the caller (see [Directory::is_walk_root]).
const DIR_WALK_ROOT: u8 = 1;
/// Longest string a reader accepts, against a length a corrupt file makes up.
const MAX_STRING: usize = 1 << 20;
/// Most strings a reader allocates room for ahead, for the same reason.
const MAX_PREALLOC: usize = 1 << 20;

/* ===== writing ===== */

/// A snapshot stream being written: every byte goes through the checksum.
struct SnapWriter<W: Write> {
    out: BufWriter<W>,
    sum: CustomXxh3Hasher,
}

impl<W: Write> SnapWriter<W> {
    fn new(w: W) -> Self {
        Self { out: BufWriter::new(w), sum: CustomXxh3Hasher::new_xxh3_defaults() }
    }

    fn bytes(&mut self, b: &[u8]) -> io::Result<()> {
        self.sum.write(b);
        self.out.write_all(b)
    }

    fn u8(&mut self, v: u8) -> io::Result<()> {
        self.bytes(&[v])
    }

    fn u16(&mut self, v: u16) -> io::Result<()> {
        self.bytes(&v.to_le_bytes())
    }

    fn u32(&mut self, v: u32) -> io::Result<()> {
        self.bytes(&v.to_le_bytes())
    }

    fn u64(&mut self, v: u64) -> io::Result<()> {
        self.bytes(&v.to_le_bytes())
    }

    /// A `u32` length, then the bytes.
    fn blob(&mut self, b: &[u8]) -> io::Result<()> {
        let len: u32 = u32::try_from(b.len()).map_err(|_| io::Error::from(ErrorKind::InvalidInput))?;
        self.u32(len)?;
        self.bytes(b)
    }

    /// The trailer's checksum and end marker (not checksummed), and flush.
    fn finish(mut self) -> io::Result<W> {
        let sum: u64 = self.sum.finish();
        self.out.write_all(&sum.to_le_bytes())?;
        self.out.write_all(END)?;
        self.out.into_inner().map_err(|e| e.into_error())
    }
}

impl DirTree {
    /**
    Write a snapshot of the tree to `w` (see `docs/snapshot.md`). The
    tree stays in use meanwhile: each directory's children lock is held
    only while its own record and files are written, so the snapshot is
    consistent per directory. A directory changed after it was written
    has a newer ctime than its stored stamp, and the [DirTree::update]
    after loading diffs it.
    */
    pub fn save<W: Write>(&self, w: W) -> TreeResult<()> {
        let mut out: SnapWriter<W> = SnapWriter::new(w);
        // entries named by strings interned after this are left out, as changed after the save
        let n_strings: u32 = self.strings().len() as u32;
        let root_path: Option<&PathBuf> = self.conf.from();
        let (root_dev, root_ino): (u64, u64) = match root_path {
            Some(p) => metadata(p).map(|m| (m.dev(), m.ino()))?,
            None => (0, 0),
        };

        out.bytes(MAGIC)?;
        out.u16(VERSION)?;
        out.u16(0)?; // flags
        out.u8(u8::from(*self.filemode()))?;
        out.u32(STRING_BASE)?;
        out.u64(root_dev)?;
        out.u64(root_ino)?;
        out.u64(**self.conf.ctime())?;
        out.u64(*SecondsSinceEpoch::new())?;
        out.blob(root_path.map_or(&[][..], |p: &PathBuf| p.as_os_str().as_bytes()))?;

        out.u32(n_strings.saturating_sub(STRING_BASE))?;
        for idx in STRING_BASE..n_strings {
            out.blob(self.get_string(idx).as_bytes())?;
        }

        let (counts, depth): (NodeCounts, u8) = self.save_dirs(&mut out, n_strings)?;
        out.u32(counts.nodes)?;
        out.u32(counts.dirs)?;
        out.u32(counts.files)?;
        out.u32(counts.specials)?;
        out.u8(depth)?;
        out.finish()?;
        Ok(())
    }

    /**
    The tree section: depth first from the root, each directory's record
    and its files before its subdirectories. Returns what was written, to
    be checked against what a load builds.
    */
    fn save_dirs<W: Write>(&self, out: &mut SnapWriter<W>, n_strings: u32) -> io::Result<(NodeCounts, u8)> {
        let known = |idx: u32| idx < n_strings;
        let mut counts: NodeCounts = NodeCounts::default();
        let mut max_depth: u8 = 0;
        // (directory, its name, its depth); the root's name is its own
        let mut todo: Vec<(Arc<Directory>, u32, u8)> = vec![(self.root(), self.root().name_idx(), 0)];
        while let Some((dir, name, depth)) = todo.pop() {
            let child_depth: u8 = depth.saturating_add(1);
            let mut files: Vec<(u32, FileEntry)> = Vec::new();
            let mut subdirs: Vec<(Arc<Directory>, u32, u8)> = Vec::new();
            {
                let children = dir.children().read();
                for (idx, child) in children.iter() {
                    match child {
                        Child::Dir(d) if known(*idx) => subdirs.push((d.clone(), *idx, child_depth)),
                        Child::File(f) if known(*idx) && f.target().is_none_or(known) => files.push((*idx, *f)),
                        _ => {}
                    }
                }
                out.u32(name)?;
                out.u64(dir.inode())?;
                out.u64(dir.scan_stamp())?;
                out.u16(dir.tag().unwrap_or(SCOPE_NONE))?;
                out.u8(if dir.is_walk_root() { DIR_WALK_ROOT } else { 0 })?;
                out.u32(files.len() as u32)?;
                out.u32(subdirs.len() as u32)?;
                for (idx, file) in &files {
                    out.u32(*idx)?;
                    out.u64(file.inode())?;
                    out.u8(file.kind() as u8)?;
                    out.u32(file.target().unwrap_or(NO_TARGET))?;
                    counts.add_file(file);
                }
            }
            if !files.is_empty() || !subdirs.is_empty() {
                max_depth = max_depth.max(child_depth);
            }
            counts.dirs += subdirs.len() as u32;
            counts.nodes += subdirs.len() as u32;
            // reversed: popped in the order the record announced them
            todo.extend(subdirs.into_iter().rev());
        }
        Ok((counts, max_depth))
    }

    /**
    [DirTree::save] to the file `path`, through a temporary file next to
    it that is synced and then renamed over `path`: a crash never leaves
    a torn snapshot behind.
    */
    pub fn save_to(&self, path: &Path) -> TreeResult<()> {
        let name: &OsStr = path.file_name().ok_or_else(|| io::Error::from(ErrorKind::InvalidInput))?;
        let mut tmp_name: Vec<u8> = b".".to_vec();
        tmp_name.extend_from_slice(name.as_bytes());
        tmp_name.extend_from_slice(format!(".tmp{}", process::id()).as_bytes());
        let tmp: PathBuf = path.with_file_name(OsStr::from_bytes(&tmp_name));
        let written: TreeResult<()> = File::create(&tmp)
            .map_err(TreeError::from)
            .and_then(|f: File| {
                self.save(&f)?;
                Ok(f.sync_all()?)
            })
            .and_then(|_| Ok(rename(&tmp, path)?));
        if written.is_err() {
            std::fs::remove_file(&tmp).ok();
        }
        written
    }

    /**
    Tell the background worker to [save](DirTree::save_to) the tree to
    `path`. Fails with [TreeError::WorkerNotRunning] when no worker would
    run it; a failed save is recorded as a tree error.
    */
    pub fn save_bg(&self, path: &Path) -> TreeResult<()> {
        self.queue_bg_op(TreeOp::Save(path.to_path_buf()))
    }
}

/* ===== reading ===== */

/// A snapshot stream being read: every byte goes through the checksum.
struct SnapReader<R: Read> {
    inp: BufReader<R>,
    sum: CustomXxh3Hasher,
}

impl<R: Read> SnapReader<R> {
    fn new(r: R) -> Self {
        Self { inp: BufReader::new(r), sum: CustomXxh3Hasher::new_xxh3_defaults() }
    }

    fn array<const N: usize>(&mut self) -> TreeResult<[u8; N]> {
        let mut b: [u8; N] = [0; N];
        read_exact(&mut self.inp, &mut b)?;
        self.sum.write(&b);
        Ok(b)
    }

    fn u8(&mut self) -> TreeResult<u8> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> TreeResult<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> TreeResult<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> TreeResult<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// A `u32` length, then the bytes.
    fn blob(&mut self) -> TreeResult<Vec<u8>> {
        let len: usize = self.u32()? as usize;
        if len > MAX_STRING {
            return Err(bad(format!("string of {len} bytes")));
        }
        let mut b: Vec<u8> = vec![0; len];
        read_exact(&mut self.inp, &mut b)?;
        self.sum.write(&b);
        Ok(b)
    }

    /// The checksum and the end marker, then the end of the stream.
    fn finish(mut self) -> TreeResult<()> {
        let sum: u64 = self.sum.finish();
        let mut stored: [u8; 8] = [0; 8];
        read_exact(&mut self.inp, &mut stored)?;
        if u64::from_le_bytes(stored) != sum {
            return Err(bad("checksum mismatch".into()));
        }
        let mut end: [u8; 8] = [0; 8];
        read_exact(&mut self.inp, &mut end)?;
        if &end != END || self.inp.read(&mut [0u8; 1])? != 0 {
            return Err(bad("no end marker where the file should end".into()));
        }
        Ok(())
    }
}

/// [Read::read_exact], with a short file reported as what it is.
fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> TreeResult<()> {
    r.read_exact(buf).map_err(|e: io::Error| match e.kind() {
        ErrorKind::UnexpectedEof => bad("truncated".into()),
        _ => e.into(),
    })
}

fn bad(reason: String) -> TreeError {
    TreeError::BadSnapshot(reason)
}

/// Old string indices (from the file) to new ones (in the tree's store).
struct Remap(Vec<u32>);

impl Remap {
    fn get(&self, idx: u32) -> TreeResult<u32> {
        match idx.checked_sub(STRING_BASE) {
            None => Ok(idx), // a built-in entry, the same in every store
            Some(i) => self.0.get(i as usize).copied().ok_or_else(|| bad(format!("string index {idx}"))),
        }
    }
}

/// A directory record, as read.
struct DirRecord {
    name: u32,
    inode: u64,
    stamp: u64,
    tag: u16,
    flags: u8,
    n_files: u32,
    n_dirs: u32,
}

impl DirTree {
    /**
    Fill this tree from a snapshot (see `docs/snapshot.md`). The tree must
    be uninitialized or empty (only `from_path()` called), else
    [TreeError::NotEmpty]. Its filemode must be the snapshot's, and if it
    has a root path, it must be the snapshot's. The root must still be the
    same directory on the same device. A file that fails any check, or
    its checksum, leaves the tree empty, never partial.

    Then [DirTree::update] the root: only directories changed since the
    save are listed again.
    */
    pub fn load<R: Read>(&self, r: R) -> TreeResult<()> {
        // (the worker marks the tree Active(Load) before it runs a queued load)
        if !matches!(
            self.state(),
            TreeState::Uninitialized | TreeState::Empty | TreeState::Active(TreeOp::Load(_))
        ) {
            return Err(TreeError::NotEmpty);
        }
        let before: TreeState = match self.state() {
            TreeState::Active(_) => TreeState::Empty,
            state => state,
        };
        self.clear_nodes(); // the root path's chain: the snapshot brings its own
        match self.load_inner(r) {
            Ok(counts) => {
                let obs = self.conf.observer();
                obs.dirs_added(counts.dirs as u64);
                obs.files_added(counts.files as u64, 0);
                obs.specials_added(counts.specials as u64);
                self.set_state(TreeState::Ready);
                Ok(())
            }
            Err(e) => {
                self.clear_nodes();
                self.insert_from();
                self.set_state(before);
                Err(e)
            }
        }
    }

    fn load_inner<R: Read>(&self, r: R) -> TreeResult<NodeCounts> {
        let mut inp: SnapReader<R> = SnapReader::new(r);
        if &inp.array::<8>()? != MAGIC {
            return Err(bad("not a dirtree snapshot".into()));
        }
        match inp.u16()? {
            VERSION => {}
            v => return Err(bad(format!("version {v}"))),
        }
        inp.u16()?; // flags
        let mode: FileMode = FileMode::from(inp.u8()?);
        if mode != *self.filemode() {
            return Err(bad(format!("filemode {mode}, the tree's is {}", self.filemode())));
        }
        match inp.u32()? {
            STRING_BASE => {}
            b => return Err(bad(format!("string base {b}"))),
        }
        let (root_dev, root_ino): (u64, u64) = (inp.u64()?, inp.u64()?);
        let (_created, _saved): (u64, u64) = (inp.u64()?, inp.u64()?);
        let root_path: PathBuf = PathBuf::from(OsStr::from_bytes(&inp.blob()?));
        if !root_path.as_os_str().is_empty() {
            if let Some(from) = self.conf.from()
                && *from != root_path
            {
                return Err(bad(format!("root {}, the tree's is {}", root_path.display(), from.display())));
            }
            let meta = metadata(&root_path)?;
            if (meta.dev(), meta.ino()) != (root_dev, root_ino) {
                return Err(bad(format!("{} is no longer the directory saved", root_path.display())));
            }
            self.conf.set_from(encode_os(&root_path).as_ref());
        }

        let count: usize = inp.u32()? as usize;
        let mut remap: Vec<u32> = Vec::with_capacity(count.min(MAX_PREALLOC));
        for _ in 0..count {
            let s: String = String::from_utf8(inp.blob()?).map_err(|_| bad("a string not UTF-8".into()))?;
            remap.push(self.strings().insert(s.as_str()));
        }
        let remap: Remap = Remap(remap);

        // the root's record: its own name and inode stay
        let rec: DirRecord = read_dir_record(&mut inp)?;
        let root: Arc<Directory> = self.root();
        apply_dir_record(&root, &rec);
        self.load_files(&mut inp, &remap, &root, rec.n_files, 1)?;
        // (directory, subdirectory records still to come, its depth)
        let mut stack: Vec<(Arc<Directory>, u32, u8)> = vec![(root, rec.n_dirs, 0)];
        while let Some(top) = stack.last_mut() {
            if top.1 == 0 {
                stack.pop();
                continue;
            }
            top.1 -= 1;
            let (parent, depth): (Arc<Directory>, u8) = (top.0.clone(), top.2.saturating_add(1));
            let rec: DirRecord = read_dir_record(&mut inp)?;
            let name: u32 = remap.get(rec.name)?;
            let dir: Arc<Directory> = match self.insert_child(&parent, name, NewChild::Dir(rec.inode), depth) {
                (Child::Dir(d), true) => d,
                _ => return Err(bad("a name twice in one directory".into())),
            };
            apply_dir_record(&dir, &rec);
            self.load_files(&mut inp, &remap, &dir, rec.n_files, depth.saturating_add(1))?;
            stack.push((dir, rec.n_dirs, depth));
        }

        let saved: NodeCounts = NodeCounts {
            nodes: inp.u32()?,
            dirs: inp.u32()?,
            files: inp.u32()?,
            specials: inp.u32()?,
        };
        let depth: u8 = inp.u8()?;
        inp.finish()?;
        let loaded: NodeCounts = self.conf.counts();
        if loaded != saved || depth != self.conf.depth() {
            return Err(bad(format!("loaded {loaded:?} at depth {}, saved {saved:?} at {depth}", self.conf.depth())));
        }
        Ok(loaded)
    }

    /// The `n` file records of `dir`, at depth `depth`.
    fn load_files<R: Read>(
        &self,
        inp: &mut SnapReader<R>,
        remap: &Remap,
        dir: &Arc<Directory>,
        n: u32,
        depth: u8,
    ) -> TreeResult<()> {
        for _ in 0..n {
            let name: u32 = remap.get(inp.u32()?)?;
            let inode: u64 = inp.u64()?;
            let kind: FileKind = FileKind::from_repr(inp.u8()?).ok_or_else(|| bad("unknown file kind".into()))?;
            let target: Option<u32> = match inp.u32()? {
                NO_TARGET => None,
                t => Some(remap.get(t)?),
            };
            let file: FileEntry = FileEntry::new(inode, kind, target);
            if !self.insert_child(dir, name, NewChild::File(file), depth).1 {
                return Err(bad("a name twice in one directory".into()));
            }
        }
        Ok(())
    }

    /// [DirTree::load] from the file `path`.
    pub fn load_from(&self, path: &Path) -> TreeResult<()> {
        self.load(File::open(path)?)
    }

    /**
    Tell the background worker to [load](DirTree::load_from) a snapshot
    from `path`, then [rescan](DirTree::rescan) the root to catch up.
    Fails with [TreeError::WorkerNotRunning] when no worker would run it;
    a failed load is recorded as a tree error, and leaves the tree empty.
    */
    pub fn load_bg(&self, path: &Path) -> TreeResult<()> {
        self.queue_bg_op(TreeOp::Load(path.to_path_buf()))
    }

    /// Run a queued [TreeOp::Save] or [TreeOp::Load] on the worker; a failure is a tree error.
    pub(super) fn snapshot_op(&self, op: &TreeOp) {
        let result: TreeResult<()> = match op {
            TreeOp::Save(path) => self.save_to(path),
            TreeOp::Load(path) => self.load_from(path),
            _ => return,
        };
        let msg: String = match result {
            Ok(()) => format!("{op:?}: done"),
            Err(e) => {
                return self.add_error(
                    TreeEvent::error(FaultKind::Tree, &format!("{op:?} failed: {e}")).op(op),
                );
            }
        };
        self.add_event(TreeEvent::new(&msg).op(op));
    }
}

fn read_dir_record<R: Read>(inp: &mut SnapReader<R>) -> TreeResult<DirRecord> {
    Ok(DirRecord {
        name: inp.u32()?,
        inode: inp.u64()?,
        stamp: inp.u64()?,
        tag: inp.u16()?,
        flags: inp.u8()?,
        n_files: inp.u32()?,
        n_dirs: inp.u32()?,
    })
}

/// What a directory record restores beyond the node itself.
fn apply_dir_record(dir: &Directory, rec: &DirRecord) {
    dir.set_scan_stamp(rec.stamp);
    dir.set_tag(rec.tag);
    if rec.flags & DIR_WALK_ROOT != 0 {
        dir.set_walk_root();
    }
}
