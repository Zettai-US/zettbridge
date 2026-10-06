// SPDX-License-Identifier: GPL-2.0-only
//! Compile the production consumer/recovery policy with synchronous fake MMIO.
//! The harness can place events exactly inside the unlocked VMA-zap interval.
#![allow(dead_code)]
#[path = "../accounting.rs"]
mod accounting;
#[path = "../capacity.rs"]
mod capacity;
#[path = "../consumer.rs"]
mod consumer;
#[path = "../recovery.rs"]
mod recovery;
#[path = "../uapi.rs"]
mod uapi;

use accounting::Ledger;
use std::{cell::RefCell, collections::BTreeMap};
use uapi::Request;

#[derive(Default)]
struct Device(RefCell<BTreeMap<u32, u64>>);
impl Device {
    fn read(&self, offset: u32) -> u32 {
        self.read64(offset) as u32
    }
    fn read64(&self, offset: u32) -> u64 {
        *self.0.borrow().get(&offset).unwrap_or(&0)
    }
    fn write(&self, offset: u32, value: u32) {
        self.write64(offset, value.into());
    }
    fn write64(&self, offset: u32, value: u64) {
        self.0.borrow_mut().insert(offset, value);
    }
    fn session(&self) -> [u64; 2] {
        [1, 2]
    }
    fn now(&self) -> u64 {
        0 // suppress periodic reporting; these tests exercise VMA reclamation
    }
}
struct Backing;
impl Backing {
    fn present(&self) -> bool {
        false
    }
}
struct Transaction {
    owner: u64,
    nr: u32,
    input: Request,
    output: Request,
    error: i32,
}
struct State {
    dev: Device,
    state: u32,
    frozen: bool,
    provider: bool,
    revoke_pending: bool,
    backing: Backing,
    pool_size: u64,
    epoch: u64,
    tier: u32,
    exported: u64,
    session: [u64; 2],
    seq: u64,
    ledger: Ledger,
    txns: Vec<Transaction>,
    txn_count: usize,
    report_seq: [u64; 2],
    usage_valid: [bool; 2],
    last_report_time: u64,
    commands: Vec<u32>,
    fail_op: Option<u32>,
}
impl State {
    fn new() -> Self {
        let mut ledger = Ledger::new();
        ledger.capacity = 8192;
        Self {
            dev: Device::default(),
            state: 3,
            frozen: false,
            provider: false,
            revoke_pending: false,
            backing: Backing,
            pool_size: 8192,
            epoch: 1,
            tier: 0,
            exported: 8192,
            session: [1, 2],
            seq: 0,
            ledger,
            txns: Vec::new(),
            txn_count: 0,
            report_seq: [0; 2],
            usage_valid: [true; 2],
            last_report_time: 0,
            commands: Vec::new(),
            fail_op: None,
        }
    }
    fn command(&mut self, _: u64, op: u32) -> Result<(), i32> {
        self.commands.push(op);
        if self.fail_op == Some(op) {
            Err(-5)
        } else {
            Ok(())
        }
    }
    fn inventory(&mut self) -> Result<(u64, u64, u32), i32> {
        unreachable!("tests start with an active attached export")
    }
    fn pin(&mut self) {}
    fn track_release(&mut self, owner: u64, handle: u64) {
        self.ledger.release(owner, handle).unwrap();
        let mut r: Request = unsafe { std::mem::zeroed() };
        r.handle = handle;
        let input = r;
        r.status = 1;
        self.txns.push(Transaction {
            owner,
            nr: 8,
            input,
            output: r,
            error: 0,
        });
        self.txn_count += 1;
    }
}

#[test]
fn late_release_does_not_complete_transaction_or_reuse_pfn() {
    let mut s = State::new();
    let a = s.ledger.alloc(1, 4096, 4096).unwrap();
    let b = s.ledger.alloc(2, 4096, 4096).unwrap();
    s.track_release(1, a.handle);
    assert_eq!(s.consumer_tick(None), 1);
    s.ledger.begin_revoke();
    // VMA B was zapped, then faulted in again while it was still ACTIVE.
    s.ledger.get_mut(2, b.handle).unwrap().mapped = true;
    s.track_release(2, b.handle);
    s.consumer_tick(Some(false));
    assert_eq!(s.commands, [7]);
    assert_eq!(s.txns[0].output.status, 0);
    assert_eq!(s.txns[1].output.status, 1);
    assert_eq!(s.ledger.counters().unwrap().releasing, 4096);
    assert_eq!(s.ledger.alloc(3, 4096, 4096).unwrap().start, a.start);
    assert!(s.ledger.alloc(4, 4096, 4096).is_err());
    assert_eq!(s.consumer_tick(None), 1);
    s.ledger.begin_revoke();
    s.consumer_tick(Some(false));
    assert_eq!(s.txns[1].output.status, 0);
    assert_eq!(s.ledger.alloc(4, 4096, 4096).unwrap().start, b.start);
}

#[test]
fn global_revoke_arriving_during_local_zap_requires_another_zap() {
    let mut s = State::new();
    let a = s.ledger.alloc(1, 4096, 4096).unwrap();
    s.ledger.alloc(2, 4096, 4096).unwrap();
    s.track_release(1, a.handle);
    assert_eq!(s.consumer_tick(None), 1);
    s.ledger.begin_revoke();
    s.dev.write(0x320, 1); // arrives after the zap of still-active lease B
    assert_eq!(s.consumer_tick(Some(false)), 1);
    assert!(
        s.commands.is_empty(),
        "DETACH would prematurely free provider backing"
    );
    assert_eq!(s.state, 4);
    assert!(s.frozen);
    assert_eq!(s.ledger.counters().unwrap().releasing, 8192);
    s.ledger.begin_revoke();
    assert_eq!(s.consumer_tick(Some(true)), 0);
    assert_eq!(s.commands, [0x101]);
    assert_eq!(s.state, 5);
    assert_eq!(s.ledger.counters().unwrap().available(), Some(8192));
}

#[test]
fn failed_barrier_quarantines_the_whole_pool_without_completing_release() {
    let mut s = State::new();
    let a = s.ledger.alloc(1, 4096, 4096).unwrap();
    s.ledger.alloc(2, 4096, 4096).unwrap();
    s.track_release(1, a.handle);
    s.ledger.begin_revoke();
    s.fail_op = Some(7);
    s.consumer_tick(Some(false));
    assert_eq!(s.state, 6);
    assert!(s.frozen && s.revoke_pending);
    assert_eq!(s.ledger.counters().unwrap().quarantined, 8192);
    assert_eq!(s.txns[0].output.status, 1);
    let seq = s.seq;
    s.revoke_pending = false; // a subsequent full zap completed
    s.fault();
    assert!(
        !s.revoke_pending,
        "repeated faults must not start endless revocations"
    );
    assert_eq!(s.seq, seq);
}
