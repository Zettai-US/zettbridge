// SPDX-License-Identifier: GPL-2.0-only
pub(crate) const MAX_EXTENTS: u64 = 65536;
pub(crate) fn encode(dpa: u64, length: u64, dma: u64, epoch: u64, tier: u16) -> Option<[u8; 64]> {
    if length == 0 || epoch == 0 || tier > 1 || (dpa | length | dma) & 4095 != 0 {
        return None;
    }
    dpa.checked_add(length)?;
    dma.checked_add(length)?;
    let mut b = [0; 64];
    b[0..8].copy_from_slice(&dpa.to_le_bytes());
    b[8..16].copy_from_slice(&length.to_le_bytes());
    b[16..24].copy_from_slice(&dma.to_le_bytes());
    b[24..32].copy_from_slice(&epoch.to_le_bytes());
    b[32..36].copy_from_slice(&1u32.to_le_bytes());
    b[36..40].copy_from_slice(&7u32.to_le_bytes());
    b[40..42].copy_from_slice(&tier.to_le_bytes());
    Some(b)
}
