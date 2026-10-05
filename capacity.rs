// SPDX-License-Identifier: GPL-2.0-only
//! Bounds for the static CXL aperture and the existing mmap handle encoding.
pub(crate) const MIN_POOL_BYTES: u64 = 256 << 20;
pub(crate) const MAX_POOL_BYTES: u64 = 64 << 30;
pub(crate) const MAX_LEASE_BYTES: u64 = 1 << 32;

pub(crate) fn valid_pool(bytes: u64) -> bool {
    (MIN_POOL_BYTES..=MAX_POOL_BYTES).contains(&bytes) && bytes % MIN_POOL_BYTES == 0
}
