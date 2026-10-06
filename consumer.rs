// SPDX-License-Identifier: GPL-2.0-only
use crate::{
    accounting::{Counters, Lease},
    uapi::Request,
    State,
};
impl State {
    pub(crate) fn consumer_request(
        &mut self,
        owner: u64,
        nr: u32,
        r: &mut Request,
    ) -> Result<(), i32> {
        if r.epoch != self.epoch {
            return Err(-116);
        }
        if self.state != 3 || self.frozen {
            return Err(-16);
        }
        if r.tier != self.tier {
            return Err(-22);
        }
        match nr {
            7 => {
                if r.handle != 0 {
                    return Err(-22);
                }
                let l = self.ledger.alloc(owner, r.bytes, r.alignment)?;
                // No user mapping exists yet. Clear through the authorized
                // extent engine before the new owner can see previous data.
                self.dev.write64(0x330, l.start);
                self.dev.write64(0x338, l.bytes);
                if let Err(e) = self.command(self.epoch, 0x103) {
                    self.ledger.get_mut(owner, l.handle)?.state = 3;
                    return Err(e);
                }
                r.handle = l.handle;
                r.bytes = l.bytes;
                Ok(())
            }
            8 => {
                self.ledger.release(owner, r.handle)?;
                r.status = 1;
                Ok(())
            }
            _ => Err(-25),
        }
    }
    pub(crate) fn consumer_tick(&mut self, revoked: Option<bool>) -> i32 {
        if self.pool_size == 0 {
            return 0;
        }
        if self.state == 0 {
            let Ok((epoch, capacity, state)) = self.inventory() else {
                return 0;
            };
            if state != 2 || capacity != self.pool_size {
                return 0;
            }
            let dram = self.dev.read64(0x340);
            let cxl = self.dev.read64(0x348);
            self.tier = if dram == capacity && cxl == 0 {
                0
            } else if cxl == capacity && dram == 0 {
                1
            } else {
                self.fault();
                return 0;
            };
            self.epoch = epoch;
            self.exported = capacity;
            self.session = self.dev.session();
            self.dev.write64(0x300, self.session[0]);
            self.dev.write64(0x308, self.session[1]);
            self.dev.write64(0x310, capacity);
            if self.command(epoch, 0x100).is_err() {
                return 0;
            }
            self.state = 2;
            self.ledger.capacity = capacity;
        }
        if self.state == 2 {
            if self.dev.read(0x10) & 2 == 0 {
                return 0;
            }
            self.state = 3;
            self.seq += 1;
        }
        if self.state != 3 && self.state != 4 {
            return 0;
        }
        if self.dev.read(0x320) != 0 {
            self.frozen = true;
            self.state = 4;
            for l in &mut self.ledger.leases {
                if l.state == 1 {
                    l.state = 2;
                }
            }
        }
        let pending = self.ledger.leases.iter().any(|l| l.state == 2);
        if (pending || self.state == 4) && revoked.is_none() {
            return 1;
        }
        if revoked.is_some() {
            if self.state == 4 {
                // A global revoke discovered AFTER a local zap needs a new
                // pass: active leases could have faulted during that zap.
                if revoked != Some(true) {
                    return 1;
                }
                // The Rust worker has zapped every VMA with synchronous TLB invalidation.
                // All mappings are UC: there are no dirty WB cache lines.
                if self.command(self.epoch, 0x101).is_err() {
                    self.fault();
                    return 0;
                }
                self.state = 5;
                self.ledger.leases.fill(Lease::default());
                self.seq += 1;
                // DAX resource remains owned until device removal, preventing
                // a /dev/dax or kmem rebind from bypassing this mapping policy.
                return 0;
            }
            if self.command(self.epoch, 7).is_err() {
                self.fault();
                return 0;
            }
            for l in self.ledger.finish_revoke() {
                for t in &mut self.txns[..self.txn_count] {
                    if t.nr == 8 && t.owner == l.owner && t.input.handle == l.handle && t.error == 0
                    {
                        t.output.status = 0;
                    }
                }
            }
            self.seq += 1;
        }
        if self.state == 3 && self.dev.now().saturating_sub(self.last_report_time) >= 200_000_000 {
            for tier in 0..2 {
                let c = if tier == self.tier as usize {
                    self.ledger.counters().unwrap_or_default()
                } else {
                    Counters::default()
                };
                self.report_seq[tier] = match self.report_seq[tier].checked_add(1) {
                    Some(v) => v,
                    None => {
                        self.fault();
                        return 0;
                    }
                };
                let mut b = [0u8; 128];
                for (offset, value) in [
                    (0, self.epoch),
                    (8, self.session[0]),
                    (16, self.session[1]),
                    (24, self.report_seq[tier]),
                    (40, c.capacity),
                    (48, c.allocated),
                    (56, c.releasing),
                    (64, c.quarantined),
                    (72, c.requested),
                ] {
                    b[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
                }
                b[32..36].copy_from_slice(&(tier as u32).to_le_bytes());
                b[36..40].copy_from_slice(&3u32.to_le_bytes());
                self.dev.write(0x98, tier as u32);
                for i in (0..128).step_by(4) {
                    self.dev.write(
                        0x2000 + tier as u32 * 128 + i as u32,
                        u32::from_le_bytes(b[i..i + 4].try_into().unwrap()),
                    );
                }
                if self.command(self.epoch, 0x201).is_err() {
                    self.fault();
                    return 0;
                }
            }
            self.last_report_time = self.dev.now();
        }
        0
    }
}
