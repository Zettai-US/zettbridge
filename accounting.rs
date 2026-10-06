// SPDX-License-Identifier: GPL-2.0-only
pub(crate) const MAX_LEASES: usize = 128;
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Lease {
    pub handle: u64,
    pub owner: u64,
    pub start: u64,
    pub bytes: u64,
    pub requested: u64,
    pub state: u32,
    pub mapped: bool,
    pub revoking: bool,
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Counters {
    pub capacity: u64,
    pub allocated: u64,
    pub releasing: u64,
    pub quarantined: u64,
    pub requested: u64,
}
impl Counters {
    pub(crate) fn available(&self) -> Option<u64> {
        if self.requested > self.allocated {
            return None;
        }
        self.capacity
            .checked_sub(self.allocated)?
            .checked_sub(self.releasing)?
            .checked_sub(self.quarantined)
    }
}
pub(crate) struct Ledger {
    pub leases: [Lease; MAX_LEASES],
    pub capacity: u64,
    pub next_handle: u64,
}
impl Ledger {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self {
            leases: [Lease::default(); MAX_LEASES],
            capacity: 0,
            next_handle: 1,
        }
    }
    pub(crate) fn counters(&self) -> Option<Counters> {
        let mut c = Counters {
            capacity: self.capacity,
            ..Counters::default()
        };
        for l in &self.leases {
            match l.state {
                1 => {
                    c.allocated = c.allocated.checked_add(l.bytes)?;
                    c.requested = c.requested.checked_add(l.requested)?;
                }
                2 => c.releasing = c.releasing.checked_add(l.bytes)?,
                3 => c.quarantined = c.quarantined.checked_add(l.bytes)?,
                _ => {}
            }
        }
        c.available()?;
        Some(c)
    }
    pub(crate) fn alloc(&mut self, owner: u64, requested: u64, align: u64) -> Result<Lease, i32> {
        if requested == 0 || align < 4096 || !align.is_power_of_two() {
            return Err(-22);
        }
        let bytes = requested.checked_add(4095).ok_or(-75)? & !4095;
        // mmap offsets dedicate 32 bits to each lease. Larger leases could
        // alias the next handle when a VMA is split or partially unmapped.
        if bytes > crate::capacity::MAX_LEASE_BYTES {
            return Err(-7);
        }
        let slot = self.leases.iter().position(|l| l.state == 0).ok_or(-28)?;
        let mut start = 0u64;
        loop {
            start = start.checked_add(align - 1).ok_or(-75)? & !(align - 1);
            let end = start.checked_add(bytes).ok_or(-75)?;
            if end > self.capacity {
                return Err(-28);
            }
            if let Some(l) = self
                .leases
                .iter()
                .find(|l| l.state != 0 && start < l.start + l.bytes && l.start < end)
            {
                start = l.start + l.bytes;
            } else {
                break;
            }
        }
        if self.next_handle >= (1u64 << 30) {
            return Err(-75);
        }
        let l = Lease {
            handle: self.next_handle,
            owner,
            start,
            bytes,
            requested,
            state: 1,
            mapped: false,
            revoking: false,
        };
        self.next_handle += 1;
        self.leases[slot] = l;
        Ok(l)
    }
    pub(crate) fn get_mut(&mut self, owner: u64, handle: u64) -> Result<&mut Lease, i32> {
        self.leases
            .iter_mut()
            .find(|l| l.state != 0 && l.handle == handle && l.owner == owner)
            .ok_or(-2)
    }
    pub(crate) fn release(&mut self, owner: u64, handle: u64) -> Result<(), i32> {
        let l = self.get_mut(owner, handle)?;
        if l.state == 1 {
            l.state = 2;
        }
        if l.state != 2 {
            return Err(-16);
        }
        Ok(())
    }
    /// Snapshot the releasing set while the State mutex still excludes faults.
    pub(crate) fn begin_revoke(&mut self) {
        for l in &mut self.leases {
            l.revoking = l.state == 2;
        }
    }
    /// Only this batch has a VMA zap + hardware barrier completion proof.
    pub(crate) fn finish_revoke(&mut self) -> impl Iterator<Item = Lease> + '_ {
        self.leases.iter_mut().filter_map(|l| {
            if l.state == 2 && l.revoking {
                Some(core::mem::take(l))
            } else {
                None
            }
        })
    }
}
