// SPDX-License-Identifier: GPL-2.0-only
use crate::{accounting::Counters, extent, uapi::Request, State};
impl State {
    pub(crate) fn provider_request(
        &mut self,
        owner: u64,
        nr: u32,
        r: &mut Request,
    ) -> Result<(), i32> {
        if r.epoch != self.epoch {
            return Err(-116);
        }
        if nr != 2 && (owner != self.owner || r.handle != 1) {
            return Err(-2);
        }
        match nr {
            2 => {
                if self.backing.present() || self.state != 0 {
                    return Err(-16);
                }
                if r.bytes != self.capacity || r.tier > 1 || r.alignment != 0 || r.handle != 0 {
                    return Err(-22);
                }
                self.backing = self.dev.reserve(r.bytes, r.tier, r.node)?;
                self.reserved = r.bytes;
                self.tier = r.tier;
                self.owner = owner;
                self.pin();
                r.handle = 1;
                Ok(())
            }
            3 => {
                if self.state != 0 || !self.backing.present() {
                    return Err(-16);
                }
                let result = self.prepare();
                if result.is_err() && !self.transport.uncertain && self.state != 6 {
                    self.backing.free();
                    self.reserved = 0;
                    self.mapped = 0;
                    self.unpin();
                }
                result
            }
            4 => {
                if self.state != 1 {
                    return Err(-16);
                }
                let epoch = self.epoch.checked_add(1).ok_or(-75)?;
                self.command(epoch, 0x200)?;
                self.epoch = epoch;
                self.exported = self.reserved;
                self.state = 2;
                r.epoch = epoch;
                Ok(())
            }
            5 => {
                if self.state != 2 && self.state != 3 && self.state != 4 {
                    return Err(-16);
                }
                self.command(self.epoch, 0x104)?;
                self.state = 4;
                r.status = 1;
                Ok(())
            }
            6 => {
                if self.state != 5 {
                    return Err(-16);
                }
                self.backing.free();
                self.reserved = 0;
                self.mapped = 0;
                self.exported = 0;
                self.state = 0;
                self.unpin();
                Ok(())
            }
            _ => Err(-25),
        }
    }
    fn prepare(&mut self) -> Result<(), i32> {
        let n = self.backing.map()?;
        self.mapped = self.reserved;
        if n == 0 || n as u64 > extent::MAX_EXTENTS {
            return Err(-22);
        }
        let epoch = self.epoch.checked_add(1).ok_or(-75)?;
        let mut dpa = 0;
        for i in 0..n {
            let (dma, len) = self.backing.segment(i)?;
            let b = extent::encode(dpa, len, dma, epoch, self.tier as u16).ok_or(-22)?;
            self.backing.descriptor(i, &b);
            dpa = dpa.checked_add(len).ok_or(-75)?;
        }
        if dpa != self.reserved {
            return Err(-22);
        }
        self.dev.write64(0x30, self.backing.table());
        self.dev.write64(0x38, n as u64);
        self.command(epoch, 0x102)?;
        self.backing.verify()?;
        self.state = 1;
        Ok(())
    }
    pub(crate) fn provider_tick(&mut self) {
        if self.state < 2 || self.state == 5 {
            return;
        }
        if self.state == 2 && self.dev.read(0x318) == 1 {
            if self.command(self.epoch, 1).is_err() {
                return;
            }
            self.state = 3;
            self.seq += 1;
        }
        if self.state == 4 {
            if self.dev.read(0x318) != 0 {
                return;
            }
            for op in [2, 3, 4] {
                if self.command(self.epoch, op).is_err() {
                    return;
                }
            }
            self.state = 5;
            for t in &mut self.txns[..self.txn_count] {
                if t.nr == 5 && t.error == 0 {
                    t.output.status = 0;
                }
            }
            self.exported = 0;
            self.usage_valid = [false; 2];
            self.seq += 1;
            return;
        }
        for tier in 0..2 {
            self.dev.write(0x98, tier as u32);
            if self.command(self.epoch, 8).is_err() {
                continue;
            }
            let off = 0x2000 + tier as u32 * 128;
            let mut b = [0u8; 128];
            for i in (0..128).step_by(4) {
                b[i..i + 4].copy_from_slice(&self.dev.read(off + i as u32).to_le_bytes());
            }
            let get = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
            let session = [get(8), get(16)];
            let report = get(24);
            let c = Counters {
                capacity: get(40),
                allocated: get(48),
                releasing: get(56),
                quarantined: get(64),
                requested: get(72),
            };
            if get(0) != self.epoch
                || session == [0; 2]
                || (self.remote_session != [0; 2] && session != self.remote_session)
                || u32::from_le_bytes(b[32..36].try_into().unwrap()) != tier as u32
                || u32::from_le_bytes(b[36..40].try_into().unwrap()) != 3
                || b[80..].iter().any(|v| *v != 0)
                || c.available().is_none()
                || c.capacity
                    != if tier == self.tier as usize {
                        self.exported
                    } else {
                        0
                    }
            {
                self.usage_valid[tier] = false;
                continue;
            }
            if report <= self.report_seq[tier] {
                continue;
            }
            // First full report establishes the mirror. A gap invalidates one
            // observation; the next consecutive FULL snapshot repairs it.
            self.usage_valid[tier] =
                self.report_seq[tier] == 0 || report == self.report_seq[tier] + 1;
            self.remote_session = session;
            self.report_seq[tier] = report;
            self.remote[tier] = c;
            self.received[tier] = self.dev.now();
            self.seq += 1;
        }
    }
}
