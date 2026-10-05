// SPDX-License-Identifier: GPL-2.0-only
use crate::ffi::Device;
pub(crate) struct Transport {
    pub seq: u32,
    query: u32,
    pub uncertain: bool,
}
impl Transport {
    pub(crate) fn command(&mut self, dev: Device, epoch: u64, op: u32) -> Result<(), i32> {
        if self.uncertain {
            return Err(-110);
        }
        self.seq = self.seq.checked_add(1).ok_or(-75)?;
        dev.write64(0x50, epoch);
        dev.write(0x58, self.seq);
        dev.barrier();
        match op {
            0x200 => dev.write(0x40, 1),
            0x201 => dev.write(0x90, 1),
            _ => dev.write(0x18, op),
        }
        let deadline = dev.now().saturating_add(2_000_000_000);
        loop {
            let seq = dev.read(0x60);
            if seq == self.seq && dev.read64(0xc8) == epoch {
                let status = dev.read(0x68);
                if dev.read(0x60) == seq {
                    return status_result(status);
                }
            }
            if seq == u32::MAX || dev.now() >= deadline {
                break;
            }
            dev.sleep();
        }
        // A separate query bank never overwrites the uncertain command ACK.
        self.query = self.query.checked_add(1).ok_or(-75)?;
        dev.write(0xa0, self.seq);
        dev.write64(0xa8, epoch);
        dev.write(0xb0, self.query);
        dev.barrier();
        dev.write(0xb8, 1);
        for _ in 0..3 {
            if dev.read(0xc0) == self.query
                && dev.read(0x1100) == self.query
                && dev.read(0x1104) == self.seq
                && dev.read64(0x1108) == epoch
                && dev.read(0x1110) == 2
            {
                return status_result(dev.read(0x1114));
            }
            dev.sleep();
        }
        self.uncertain = true;
        Err(-110)
    }
}
fn status_result(status: u32) -> Result<(), i32> {
    match status {
        0 => Ok(()),
        1 | 5 => Err(-16),
        2 | 3 => Err(-22),
        4 => Err(-116),
        6 => Err(-5),
        7 => Err(-110),
        8 => Err(-1),
        _ => Err(-5),
    }
}
