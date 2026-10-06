# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

- Build: `cargo build` (release: `cargo build --release`)
- With memory accounting: `cargo build --features size_of`
- Lint: `cargo clippy --all-targets`, and again with `--features size_of`, which gates a lot of code
- Docs: `cargo doc --no-deps` (keep it free of warnings)
- Tests: `cargo test` (`src/tests.rs`, against real temp directories; fixtures come from miniutils' `TreeSpec`, feature `testtree`). A few tests drive the background worker and the inotify watcher and take a few seconds.
- Don't run `cargo fmt` over existing files: formatting is hand-tuned.

## Crate shape

- Extracted from statter's `src/tree/` at 0.4.0 (`git filter-repo`). `src/lib.rs` was `src/tree/mod.rs` (and earlier `src/tree.rs`). The modules still refer to each other through `super::`, which at the crate root means the same as `crate::`.
- `publish = false`. Consumers use it as a git dependency. Dependencies `custom_xxh3`, `dirhandle`, `stringstore`, `timesince`, `miniutils` and a fork of `size-of` (a workaround for E0570 on Rust ≥ 1.89) are git-only, from `github.com/Ukko-Ylijumala/*`. Local checkouts live next to this one in `~/src/rust/`.
- Edition 2024, effective MSRV 1.93 (`custom_xxh3` and `stringstore` declare it).
- Linux-only.

## Model

- `DirTree` holds a `root: Arc<Directory>`, a `TreeConf` (atomic counters and configuration), the `UniqueStrStore`, the open-handle pool, a work queue with an optional background worker, and an event log.
- `Directory` is the only node type: parent `Weak`, inode, scan stamp, interned name, visitor tag, fd, and a children map `u32 -> Child`.
- `Child::File(FileEntry)` is 16 bytes (inode, symlink target index, `FileKind`), so a map slot is 24 bytes. `test_node_sizes` pins these sizes.
- `NodeRef` (owned) is handed out by lookups and iterators. `NodeView` (borrowed) is passed to traversal callbacks while the visited directory's read lock is held: a callback must not modify that directory.
- Names: every name is interned through `osname::encode_name`, so non-UTF-8 bytes become U+F780..U+F7FF escapes. Every `&str` path in the API is that encoded form, and paths are decoded on their way to the kernel. Never intern through `to_string_lossy()` or stringstore's `store_path` (which also strips control characters and drops `...`).
- Errors: build every error event with `TreeEvent::error(kind, msg)` (plus `.path()`, `.io()` / `.errno()`, `.op()`) and record it with `add_error()`. That counts it in `TreeConf::errors` and tells the observer (`TreeObserver::fault`). Use `add_fault()` for faults that repeat en masse. Don't record an error any other way, or a consumer that accounts for coverage misses it.
- Visitors run in the parallel walker only. A configured visitor forces the parallel walker. Markers are evaluated at the directory they tag, before its subdirectories are walked, so `descend(false)` claims the whole subtree.

## Conventions

- Atomics use `Ordering::Relaxed` throughout: they count, they don't order. Don't upgrade them without a reason.
- `parking_lot` `Mutex`/`RwLock` over `std::sync`.
- Hot paths keep their `#[inline]` and `#[instrument(skip_all)]` attributes. `tracing` calls use explicit `target = "..."` strings, so keep the targets when moving log statements.
- Benchmark walker changes with `perf stat -e instructions:u` (wall clock is noisy), using statter's `--testdirs` trees or `/usr`, with `MIMALLOC_ARENA_EAGER_COMMIT=0`.

## Design docs

Read the relevant one before a non-trivial change:

- `docs/implementation.md`: the visitor protocol, stored tags, walk integration.
- `docs/snapshot.md`: the snapshot (save/load) format and its checks (`src/snapshot.rs`, `TreeOp::Save`/`Load`).
- `docs/consumers.md`: what the WordPress scanner and a web malware scanner need, the gaps (#1–#10) and what each batch closed.
