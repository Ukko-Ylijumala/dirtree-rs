// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::dirtree::DirTree;
use super::event::{TreeEvent, TreeOp, TreeState};
use crate::ScanState;
use std::{hint, path::PathBuf, sync::Arc, thread, time::Duration};

/// Background worker thread for handling [TreeOperation]s.
pub(super) fn tree_worker(t: Arc<DirTree>, state: ScanState) {
    let mut spin_ctr: u8 = 0;
    loop {
        if t.is_quitting() {
            break;
        }

        if t.no_work() {
            // wait for work in a spin loop
            while t.no_work() {
                if spin_ctr < 10 {
                    spin_ctr += 1;
                    hint::spin_loop();
                } else {
                    thread::sleep(Duration::from_micros(10));
                    spin_ctr = 0;
                    break;
                }
            }
        }

        match t.get_work() {
            None => {
                thread::sleep(Duration::from_millis(50));
            }
            Some(op) => {
                match op {
                    TreeOp::Quit => {
                        t.set_state(TreeState::Quitting);
                        break;
                    }

                    TreeOp::Build(ref path) => {
                        let p: PathBuf = path.clone();
                        t.set_state(TreeState::Active(op));
                        t.populate(&p, &state, Some(true));
                    }

                    TreeOp::Scan(ref path, recursive) => {
                        let p: PathBuf = path.clone();
                        if t.is_ready() {
                            t.set_state(TreeState::Active(op));
                        }
                        t.populate(&p, &state, recursive);
                    }

                    TreeOp::Remove(ref path) => {
                        let p: String = path.clone();
                        t.set_state(TreeState::Active(op.clone()));
                        t.remove(&p).ok().and_then(|r| r).map(|x| {
                            let msg: String =
                                format!("Removed: {} nodes, {} dirs, {} files", x.0, x.1, x.2);
                            t.add_event(TreeEvent::new(&msg).path(&p).op(&op));
                        });
                    }

                    TreeOp::Update(ref _path) => {
                        //let p: PathBuf = path.clone();
                        t.set_state(TreeState::Active(op));
                        //tree.update(&p);
                    }

                    TreeOp::Insert => {}
                    TreeOp::Serialize => {}   // TODO
                    TreeOp::Deserialize => {} // TODO
                    _ => {}
                };
                if t.no_work() {
                    t.set_state(TreeState::Ready);
                }
            }
        }
    }
}
