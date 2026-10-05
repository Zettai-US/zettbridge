// SPDX-License-Identifier: GPL-2.0-only
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub size: u32,
    pub flags: u32,
    pub id: u64,
    pub epoch: u64,
    pub handle: u64,
    pub bytes: u64,
    pub alignment: u64,
    pub tier: u32,
    pub node: i32,
    pub status: u32,
    pub reserved: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Stats {
    pub size: u32,
    pub flags: u32,
    pub state: u32,
    pub tier: u32,
    pub epoch: u64,
    pub seq: u64,
    pub now: u64,
    pub session: [u64; 2],
    pub report_seq: u64,
    pub age: u64,
    pub valid: u64,
    pub counters: [u64; 13],
    pub reserved: [u8; 72],
}
impl Stats {
    pub(crate) fn empty(tier: u32) -> Self {
        Self {
            size: 256,
            flags: 0,
            state: 0,
            tier,
            epoch: 0,
            seq: 0,
            now: 0,
            session: [0; 2],
            report_seq: 0,
            age: 0,
            valid: 0,
            counters: [0; 13],
            reserved: [0; 72],
        }
    }
}
const _: () = assert!(core::mem::size_of::<Request>() == 64);
const _: () = assert!(core::mem::size_of::<Stats>() == 256);
const _: () = assert!(core::mem::offset_of!(Stats, counters) == 0x50);
const _: () = assert!(core::mem::offset_of!(Stats, reserved) == 0xb8);

// SAFETY: Both repr(C) records contain only integer/byte arrays, admit every
// bit pattern, and have no padding (sizes and field offsets asserted above).
#[cfg(not(test))]
unsafe impl kernel::transmute::FromBytes for Request {}
#[cfg(not(test))]
unsafe impl kernel::transmute::AsBytes for Request {}
#[cfg(not(test))]
unsafe impl kernel::transmute::FromBytes for Stats {}
#[cfg(not(test))]
unsafe impl kernel::transmute::AsBytes for Stats {}
#[cfg(not(test))]
impl Stats {
    pub(crate) fn validate_input(&self) -> kernel::prelude::Result {
        use kernel::transmute::AsBytes;
        if self.size != 256
            || self.tier > 1
            || self
                .as_bytes()
                .iter()
                .enumerate()
                .any(|(i, b)| !(i < 4 || (12..16).contains(&i)) && *b != 0)
        {
            Err(kernel::error::code::EINVAL)
        } else {
            Ok(())
        }
    }
}
