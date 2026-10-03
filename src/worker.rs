// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::dirtree::DirTree;
use super::event::{FaultKind, TreeEvent, TreeOp, TreeState};
use super::osname::encode_os;
use std::{hint, path::PathBuf, sync::Weak, thread, time::Duration};

/// What one round of the worker loop found.
enum Round {
    Worked,
    Idle,
    Quit,
}

/**
Background worker thread for handling [TreeOp]s. It holds only a [Weak]
reference to its tree, upgraded for one round at a time, so the tree is
freed once the last outside `Arc` is gone; its `Drop` then stops this
thread, which finds the tree gone on its next round.
*/
pub(super) fn tree_worker(tree: Weak<DirTree>) {
    let mut spin_ctr: u8 = 0;
    loop {
        let Some(t) = tree.upgrade() else {
            break;
        };
        let round: Round = worker_round(&t, &mut spin_ctr);
        drop(t); // never sleep holding the tree
        match round {
            Round::Worked => {}
            Round::Idle => thread::sleep(Duration::from_millis(50)),
            Round::Quit => break,
        }
    }
}

/// One round of [tree_worker]: wait briefly for work, then run one op.
fn worker_round(t: &DirTree, spin_ctr: &mut u8) -> Round {
    if t.is_quitting() {
        return Round::Quit;
    }

    if t.no_work() {
        // wait for work in a spin loop
        while t.no_work() {
            if *spin_ctr < 10 {
                *spin_ctr += 1;
                hint::spin_loop();
            } else {
                thread::sleep(Duration::from_micros(10));
                *spin_ctr = 0;
                break;
            }
        }
    }

    match t.get_work() {
        None => Round::Idle,
        Some(op) => {
            match op {
                TreeOp::Quit => {
                    t.set_state(TreeState::Quitting);
                    return Round::Quit;
                }

                TreeOp::Build(ref path) => {
                    let p: PathBuf = path.clone();
                    t.set_state(TreeState::Active(op));
                    t.populate_auto(&p, Some(true));
                }

                TreeOp::Scan(ref path, recursive) => {
                    let p: PathBuf = path.clone();
                    if t.is_ready() {
                        t.set_state(TreeState::Active(op));
                    }
                    t.populate_auto(&p, recursive);
                }

                TreeOp::Remove(ref path) => {
                    let p: String = path.clone();
                    t.set_state(TreeState::Active(op.clone()));
                    if let Ok(Some(x)) = t.remove(&p) {
                        let msg: String = format!("Removed: {x}");
                        t.add_event(TreeEvent::new(&msg).path(&p).op(&op));
                    }
                }

                TreeOp::Update(ref path) => {
                    let p: PathBuf = path.clone();
                    t.set_state(TreeState::Active(op.clone()));
                    match t.update(encode_os(&p).as_ref(), None) {
                        Ok(stats) => {
                            let msg: String = format!("Updated: {stats}");
                            t.add_event(
                                TreeEvent::new(&msg)
                                    .path(encode_os(&p).as_ref())
                                    .op(&op),
                            );
                        }
                        Err(e) => {
                            t.add_error(
                                TreeEvent::error(FaultKind::Tree, &e.to_string())
                                    .path(encode_os(&p).as_ref())
                                    .op(&op),
                            );
                        }
                    }
                }

                TreeOp::Insert => {}
                TreeOp::Serialize => {}   // TODO
                TreeOp::Deserialize => {} // TODO
                _ => {}
            };
            if t.no_work() {
                t.set_state(TreeState::Ready);
            }
            Round::Worked
        }
    }
}
