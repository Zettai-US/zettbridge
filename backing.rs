// SPDX-License-Identifier: GPL-2.0-only
//! Rust-owned pages, SG mapping and coherent extent table. Publication transfers
//! ownership to this explicit-release handle; Drop is never retirement proof.
use crate::ffi;
use core::{ptr, slice};
use kernel::{
    bindings, device, dma::CoherentAllocation, prelude::*, scatterlist::SGTable, sync::aref::ARef,
};

pub(crate) struct OwnedBacking(*mut Backing);
// SAFETY: The only access is through the enclosing State's sleepable mutex.
unsafe impl Send for OwnedBacking {}
#[repr(transparent)]
struct Page(*mut bindings::page);
impl Drop for Page {
    fn drop(&mut self) {
        // SAFETY: Each Page owns exactly one order-zero allocation.
        unsafe { bindings::__free_pages(self.0, 0) };
    }
}
struct Backing {
    dev: ARef<device::Device>,
    pages: KVVec<Page>,
    sg: bindings::sg_table,
    mapped: bool,
    segments: KVVec<(u64, u64)>,
    table: Option<CoherentAllocation<u8>>,
}
impl Drop for Backing {
    fn drop(&mut self) {
        // Only allocation failure or explicit release may reach this destructor.
        self.table.take();
        // SAFETY: The device reference and pages outlive the mapping; the State
        // lock excludes access and hardware retirement has already been proved.
        unsafe {
            if self.mapped {
                ffi::zb_k_sg_unmap(ffi::raw_device(&self.dev), &mut self.sg);
            }
            if !self.sg.sgl.is_null() {
                bindings::sg_free_table(&mut self.sg);
            }
        }
    }
}
impl OwnedBacking {
    pub(crate) fn reserve(dev: &device::Device, size: u64, tier: u32, node: i32) -> Result<Self> {
        if !crate::capacity::valid_pool(size) || tier > 1 {
            return Err(EINVAL);
        }
        let mut b = KBox::new(
            Backing {
                dev: dev.into(),
                pages: KVVec::with_capacity((size / 4096) as usize, GFP_KERNEL)?,
                sg: bindings::sg_table::default(),
                mapped: false,
                segments: KVVec::new(),
                table: None,
            },
            GFP_KERNEL,
        )?;
        for _ in 0..size / 4096 {
            // SAFETY: Helper validates node bounds before allocating one page.
            let p = unsafe { ffi::zb_k_alloc_page(node) };
            if p.is_null() {
                return Err(ENOMEM);
            }
            let page = Page(p);
            // SAFETY: Page is owned, live and order zero. Physical address is used
            // solely for provenance, never as a DMA address.
            let source = unsafe {
                bindings::region_intersects(
                    ffi::zb_k_page_phys(p),
                    4096,
                    bindings::IORESOURCE_MEM as _,
                    bindings::IORES_DESC_CXL as _,
                )
            };
            if unsafe { ffi::zb_k_page_node(p) } != node
                || source == bindings::REGION_MIXED as i32
                || (source == bindings::REGION_INTERSECTS as i32) != (tier == 1)
            {
                return Err(EINVAL);
            }
            b.pages.push(page, GFP_KERNEL)?;
        }
        // SAFETY: Page is transparent over a page pointer, all entries are owned
        // and live for at least the SG table lifetime. The table is initially empty.
        kernel::error::to_result(unsafe {
            ffi::zb_k_sg_alloc(&mut b.sg, b.pages.as_mut_ptr().cast(), b.pages.len() as _)
        })?;
        Ok(Self(KBox::into_raw(b)))
    }
    pub(crate) fn present(&self) -> bool {
        !self.0.is_null()
    }
    pub(crate) fn free(&mut self) {
        if self.present() {
            // SAFETY: Sole ownership was transferred from KBox at reserve. Rust
            // lifecycle callers permit release only before publication or after
            // successful drain+retire; taking the pointer prevents double free.
            drop(unsafe { KBox::from_raw(core::mem::replace(&mut self.0, ptr::null_mut())) });
        }
    }
    fn get(&self) -> &Backing {
        assert!(self.present());
        // SAFETY: Only the unique OwnedBacking can release this allocation.
        unsafe { &*self.0 }
    }
    fn get_mut(&mut self) -> &mut Backing {
        assert!(self.present());
        // SAFETY: Exclusive handle access under State mutex.
        unsafe { &mut *self.0 }
    }
    pub(crate) fn map(&mut self) -> core::result::Result<u32, i32> {
        self.map_inner().map_err(|e| e.to_errno())
    }
    fn map_inner(&mut self) -> Result<u32> {
        let b = self.get_mut();
        if b.mapped {
            return Err(EBUSY);
        }
        for (i, page) in b.pages.iter().enumerate() {
            // SAFETY: x86-64 baseline pages are directly addressable, owned by
            // this driver and not yet made accessible to the device.
            let bytes = unsafe {
                slice::from_raw_parts_mut(ffi::zb_k_page_address(page.0).cast::<u8>(), 4096)
            };
            for (j, byte) in bytes.iter_mut().enumerate() {
                *byte = (i ^ j ^ 0x5a) as u8;
            }
        }
        // SAFETY: SG owns the described pages, mask is set once in probe, and
        // unbind cannot proceed while this operation holds State's mutex.
        kernel::error::to_result(unsafe {
            bindings::dma_map_sgtable(
                ffi::raw_device(&b.dev),
                &mut b.sg,
                bindings::dma_data_direction_DMA_BIDIRECTIONAL,
                0,
            )
        })?;
        b.mapped = true;
        let mut sg = b.sg.sgl;
        for _ in 0..b.sg.orig_nents {
            // SAFETY: Iterate original SG entries (bounce flags aren't limited
            // to DMA output entries). orig_nents is set by sg_alloc_table.
            unsafe {
                if ffi::zb_k_sg_bounce(sg) {
                    return Err(Error::from_errno(-95));
                }
                sg = ffi::zb_k_sg_next(sg);
            }
        }
        if b.sg.nents == 0 || b.sg.nents > 65536 {
            return Err(E2BIG);
        }
        // Snapshot mapped segments once. Rewalking the SG chain for every
        // extent is quadratic when the pool grows to several GiB.
        b.segments.reserve(b.sg.nents as usize, GFP_KERNEL)?;
        // SAFETY: This mapped SG table is exclusively owned under State mutex.
        for segment in unsafe { SGTable::from_raw(&mut b.sg) }.iter() {
            b.segments.push(
                (segment.dma_address(), segment.dma_len() as u64),
                GFP_KERNEL,
            )?;
        }
        // SAFETY: Caller holds the online State mutex. The PCI unbind callback
        // must acquire the same mutex before revoking resources, so device stays
        // bound throughout this allocation; no bound reference escapes.
        let bound = unsafe { &*(ffi::raw_device(&b.dev).cast::<device::Device<device::Bound>>()) };
        b.table = Some(CoherentAllocation::alloc_coherent(
            bound,
            b.sg.nents as usize * 64,
            GFP_KERNEL,
        )?);
        Ok(b.sg.nents)
    }
    pub(crate) fn segment(&self, index: u32) -> core::result::Result<(u64, u64), i32> {
        self.get().segments.get(index as usize).copied().ok_or(-22)
    }
    pub(crate) fn descriptor(&mut self, index: u32, bytes: &[u8; 64]) {
        let table = self.get_mut().table.as_mut().expect("mapped table");
        // SAFETY: Prepare writes descriptors before TEST_DMA/COMMIT doorbells.
        // CoherentAllocation::write checks the range; hardware isn't reading it.
        unsafe {
            table
                .write(bytes, index as usize * 64)
                .expect("extent index")
        };
    }
    pub(crate) fn table(&self) -> u64 {
        self.get()
            .table
            .as_ref()
            .expect("mapped table")
            .dma_handle()
    }
    pub(crate) fn verify(&mut self) -> core::result::Result<(), i32> {
        let b = self.get_mut();
        // SAFETY: A positive TEST_DMA ACK precedes this CPU ownership transfer.
        unsafe { ffi::zb_k_sg_cpu(ffi::raw_device(&b.dev), &mut b.sg) };
        let mut valid = true;
        for (i, page) in b.pages.iter().enumerate() {
            // SAFETY: Owned pages, synchronised for CPU, device test has stopped.
            let bytes = unsafe {
                slice::from_raw_parts_mut(ffi::zb_k_page_address(page.0).cast::<u8>(), 4096)
            };
            for (j, byte) in bytes.iter_mut().enumerate() {
                valid &= *byte == (i ^ j ^ 0xa5) as u8;
                *byte = 0;
            }
        }
        // SAFETY: CPU access has ended before returning ownership to the device.
        unsafe { ffi::zb_k_sg_device(ffi::raw_device(&b.dev), &mut b.sg) };
        if valid {
            Ok(())
        } else {
            Err(-5)
        }
    }
}
