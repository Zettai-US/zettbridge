// SPDX-License-Identifier: GPL-2.0-only
// Kbuild supplies #![no_std] for every Rust module.
//! Zettai RPU v0.2. All policy and resource lifetime decisions are Rust-owned.
use kernel::prelude::*;
mod accounting;
mod backing;
mod capacity;
mod consumer;
mod device;
mod extent;
mod ffi;
mod provider;
mod recovery;
mod sysfs;
mod transport;
mod uapi;
use accounting::{Counters, Ledger};
use backing::OwnedBacking as Backing;
use ffi::Device;
use transport::Transport;
use uapi::Request;
module! { type: device::Module, name: "zettbridge", authors: ["Zettai"],
description: "Zettai RPU v0.2 native Rust provider and consumer", license: "GPL", }
#[derive(Clone, Copy)]
struct Transaction {
    owner: u64,
    nr: u32,
    error: i32,
    input: Request,
    output: Request,
}
struct State {
    dev: Device,
    capacity: u64,
    provider: bool,
    ats_required: bool,
    ats: [u64; 8],
    online: bool,
    owner_seq: u64,
    mapping: *mut kernel::bindings::address_space,
    mapping_inode: *mut kernel::bindings::inode,
    dax_base: u64,
    dax_size: u64,
    dax_live: bool,
    pinned: bool,
    frozen: bool,
    revoke_pending: bool,
    state: u32,
    tier: u32,
    epoch: u64,
    seq: u64,
    owner: u64,
    backing: Backing,
    reserved: u64,
    mapped: u64,
    exported: u64,
    transport: Transport,
    ledger: Ledger,
    pool_size: u64,
    session: [u64; 2],
    report_seq: [u64; 2],
    remote: [Counters; 2],
    received: [u64; 2],
    usage_valid: [bool; 2],
    remote_session: [u64; 2],
    last_report_time: u64,
    txns: [Transaction; 256],
    txn_count: usize,
}
impl State {
    fn pin(&mut self) {
        if !self.pinned {
            self.dev.pin();
            self.pinned = true;
        }
    }
    fn unpin(&mut self) {
        if self.pinned {
            self.pinned = false;
            self.dev.unpin();
        }
    }
    fn command(&mut self, epoch: u64, op: u32) -> core::result::Result<(), i32> {
        let r = self.transport.command(self.dev, epoch, op);
        if self.transport.uncertain || r == Err(-5) {
            self.fault();
        }
        r
    }
    fn inventory(&mut self) -> core::result::Result<(u64, u64, u32), i32> {
        self.command(self.epoch, 6)?;
        Ok((
            self.dev.read64(0x1000),
            self.dev.read64(0x1008),
            self.dev.read(0x1010),
        ))
    }
    fn request(&mut self, owner: u64, nr: u32, r: &mut Request) -> core::result::Result<(), i32> {
        if !(2..=9).contains(&nr) {
            return Err(-25);
        }
        if r.size != 64 || r.flags != 0 || r.reserved != 0 || r.status != 0 || r.id == 0 {
            return Err(-22);
        }
        if let Some(t) = self.txns[..self.txn_count]
            .iter()
            .find(|t| t.owner == owner && t.input.id == r.id)
        {
            if nr != 9 && (t.nr != nr || t.input != *r) {
                return Err(-22);
            }
            *r = t.output;
            return if t.error == 0 { Ok(()) } else { Err(t.error) };
        }
        if nr == 9 {
            return Err(-2);
        }
        if self.txn_count == self.txns.len() {
            return Err(-28);
        }
        let input = *r;
        let result = if self.provider {
            self.provider_request(owner, nr, r)
        } else {
            self.consumer_request(owner, nr, r)
        };
        self.txns[self.txn_count] = Transaction {
            owner,
            nr,
            error: result.err().unwrap_or(0),
            input,
            output: *r,
        };
        self.txn_count += 1;
        self.seq = self.seq.saturating_add(1);
        result
    }
    // Some(frozen) records whether ALL new faults were blocked before the zap.
    fn tick(&mut self, revoked: Option<bool>) -> i32 {
        // Both functions expose Host B's ATS counters; sysfs reads this cached
        // snapshot without issuing MMIO. Once selected, ATS is mandatory for
        // the provider's complete mapping lifetime, including reclamation.
        for i in 0..self.ats.len() {
            self.ats[i] = self.dev.read64(0x368 + i as u32 * 8);
        }
        if self.ats_required && self.ats[0] != 7 {
            self.fault();
        }
        if self.state == 6 {
            if revoked == Some(true) {
                self.revoke_pending = false;
            }
            return i32::from(self.revoke_pending);
        }
        if self.dev.read(0x10) == u32::MAX || self.dev.read(0x10) & 4 != 0 {
            self.fault();
            return 1;
        }
        if self.provider {
            self.provider_tick();
            0
        } else {
            self.consumer_tick(revoked)
        }
    }
}
impl State {
    fn close(&mut self, owner: u64) {
        for l in &mut self.ledger.leases {
            if l.owner == owner && l.state == 1 {
                l.state = 2;
            }
        }
        self.seq = self.seq.saturating_add(1);
        let mut kept = 0;
        for i in 0..self.txn_count {
            if self.txns[i].owner != owner {
                self.txns[kept] = self.txns[i];
                kept += 1;
            }
        }
        self.txn_count = kept;
        if self.provider && self.owner == owner && self.state < 2 && !self.transport.uncertain {
            self.backing.free();
            self.reserved = 0;
            self.mapped = 0;
            self.state = 0;
            self.unpin();
        }
    }
    fn map(&mut self, owner: u64, encoded: u64, length: u64) -> core::result::Result<u64, i32> {
        let offset = (encoded & ((1 << 20) - 1)) << 12;
        let handle = encoded >> 20;
        if self.state != 3 || self.frozen {
            return Err(-19);
        }
        match self.ledger.get_mut(owner, handle) {
            Ok(l) if l.state == 1 && offset < l.bytes && length <= l.bytes - offset => {
                l.mapped = true;
                Ok(l.start + offset)
            }
            _ => Err(-22),
        }
    }
    fn attach_dax(&mut self, capacity: u64) -> core::result::Result<(), i32> {
        if self.provider || capacity != self.capacity || self.pool_size != 0 {
            return Err(-22);
        }
        self.pool_size = capacity;
        self.pin();
        Ok(())
    }
}
