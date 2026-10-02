// Copyright (c) 2024-2026 Mikko Tanner. All rights reserved.

use super::dirtree::DirTree;
use super::event::{TreeEvent, TreeOp, TreeState};
use std::{hint, path::PathBuf, sync::Arc, thread, time::Duration};

/// Background worker thread for handling [TreeOperation]s.
pub(super) fn tree_worker(t: Arc<DirTree>) {
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
                        t.remove(&p).ok().and_then(|r| r).map(|x| {
                            let msg: String =
                                format!("Removed: {} nodes, {} dirs, {} files", x.0, x.1, x.2);
                            t.add_event(TreeEvent::new(&msg).path(&p).op(&op));
                        });
                    }

                    TreeOp::Update(ref path) => {
                        let p: PathBuf = path.clone();
                        t.set_state(TreeState::Active(op.clone()));
                        match t.update(p.to_string_lossy().as_ref(), None) {
                            Ok(stats) => {
                                let msg: String = format!("Updated: {stats}");
                                t.add_event(
                                    TreeEvent::new(&msg)
                                        .path(p.to_string_lossy().as_ref())
                                        .op(&op),
                                );
                            }
                            Err(e) => {
                                t.add_error(TreeEvent::error(&e.to_string(), &op));
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
            }
        }
    }
}
