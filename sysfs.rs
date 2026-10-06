// SPDX-License-Identifier: GPL-2.0-only
use crate::{accounting::Counters, uapi::Stats, State};
impl State {
    pub(crate) fn stats(&self, tier: u32) -> Stats {
        let mut s = Stats::empty(tier);
        s.epoch = self.epoch;
        s.state = self.state;
        s.seq = self.seq;
        s.now = self.dev.now();
        let t = tier as usize;
        let c: Counters;
        let valid: bool;
        if self.provider {
            s.flags = 1;
            s.valid = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 12);
            if tier == self.tier {
                s.counters[1] = self.reserved;
                s.counters[3] = self.mapped;
                s.counters[4] = self.exported;
                if self.state == 6 {
                    s.counters[12] = self.reserved;
                }
            }
            c = self.remote[t];
            s.session = self.remote_session;
            s.report_seq = self.report_seq[t];
            s.age = if self.received[t] != 0 {
                s.now.saturating_sub(self.received[t])
            } else {
                0
            };
            valid = self.usage_valid[t] && s.age <= 2_000_000_000 && self.state == 3;
            if self.state == 6 {
                s.valid &= !(1 << 4);
            }
            if !valid && s.report_seq != 0 {
                s.flags |= 8;
            }
        } else {
            c = if tier == self.tier {
                self.ledger.counters().unwrap_or_default()
            } else {
                Counters::default()
            };
            s.session = self.session;
            s.report_seq = self.report_seq[t];
            valid = self.state == 3 || self.state == 4;
        }
        s.counters[5] = c.capacity;
        s.counters[6] = c.allocated;
        s.counters[7] = c.releasing;
        s.counters[8] = c.quarantined;
        s.counters[9] = c.available().unwrap_or(0);
        s.counters[10] = c.requested;
        if valid {
            s.flags |= 2;
            s.valid |= 0x7e0;
        }
        if self.state == 3 && !self.frozen && valid {
            s.flags |= 16;
        }
        s
    }
}

/// Format an atomic Rust snapshot into a bounded sysfs buffer.
pub(crate) fn render(
    s: &crate::uapi::Stats,
    ats: &[u64; 8],
    tier: i32,
    field: i32,
    out: &mut [u8],
) -> usize {
    use core::fmt::Write;
    struct Buffer<'a> {
        bytes: &'a mut [u8],
        len: usize,
    }
    impl Write for Buffer<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let end = self.len.checked_add(s.len()).ok_or(core::fmt::Error)?;
            let dst = self.bytes.get_mut(self.len..end).ok_or(core::fmt::Error)?;
            dst.copy_from_slice(s.as_bytes());
            self.len = end;
            Ok(())
        }
    }
    let mut b = Buffer { bytes: out, len: 0 };
    let value = if tier >= 0 {
        if !(0..13).contains(&field) || s.valid & (1u64 << field) == 0 {
            let _ = b.write_str("unknown\n");
            return b.len;
        }
        s.counters[field as usize]
    } else {
        match field {
            0 => {
                let names = [
                    "offline\n",
                    "prepared\n",
                    "prepared\n",
                    "active\n",
                    "quiescing\n",
                    "offline\n",
                    "faulted\n",
                ];
                let _ = b.write_str(names[s.state.min(6) as usize]);
                return b.len;
            }
            1 => s.epoch,
            2 => s.seq,
            3 => u64::from(s.flags & 16 != 0),
            4 => u64::from(s.flags & 2 != 0),
            6..=13 => ats[(field - 6) as usize],
            14 => u64::from(ats[0] & 2 != 0),
            _ if s.report_seq == 0 => {
                let _ = b.write_str("unknown\n");
                return b.len;
            }
            _ => s.age,
        }
    };
    let _ = writeln!(b, "{value}");
    b.len
}
