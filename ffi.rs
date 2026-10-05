// SPDX-License-Identifier: GPL-2.0-only
//! Audited missing-API boundary. C helpers contain no device/file/backing state.
use crate::{
    backing::OwnedBacking,
    device::{Bridge, Core, Node, Session},
    State,
};
use core::{ffi::c_void, ptr};
use kernel::{
    bindings, device, driver,
    fs::File,
    io::Io,
    miscdevice::{MiscDeviceOptions, MiscDeviceRegistration},
    mm::virt::VmaNew,
    pci,
    prelude::*,
    sync::{aref::ARef, Arc},
    time::ClockSource,
    types::{ForeignOwnable, Opaque},
};

// Generated C types embed configuration-dependent empty lockdep structs. They
// cross this boundary only behind pointers, never by value.
#[allow(improper_ctypes)]
extern "C" {
    fn zb_k_platform_register() -> i32;
    fn zb_k_platform_unregister();
    fn zb_k_class_add(
        parent: *mut bindings::device,
        owner: *mut c_void,
        provider: bool,
    ) -> *mut bindings::device;
    fn zb_k_pci_hooks(d: *mut bindings::pci_driver);
    fn zb_k_dma_supported(d: *mut bindings::device) -> bool;
    fn zb_k_max_segment(d: *mut bindings::device);
    fn zb_k_barrier();
    pub(crate) fn zb_k_alloc_page(node: i32) -> *mut bindings::page;
    pub(crate) fn zb_k_page_node(p: *mut bindings::page) -> i32;
    pub(crate) fn zb_k_page_phys(p: *mut bindings::page) -> u64;
    pub(crate) fn zb_k_page_address(p: *mut bindings::page) -> *mut c_void;
    pub(crate) fn zb_k_sg_alloc(
        t: *mut bindings::sg_table,
        pages: *mut *mut bindings::page,
        n: u32,
    ) -> i32;
    pub(crate) fn zb_k_sg_next(s: *mut bindings::scatterlist) -> *mut bindings::scatterlist;
    pub(crate) fn zb_k_sg_bounce(s: *mut bindings::scatterlist) -> bool;
    pub(crate) fn zb_k_sg_unmap(d: *mut bindings::device, t: *mut bindings::sg_table);
    pub(crate) fn zb_k_sg_cpu(d: *mut bindings::device, t: *mut bindings::sg_table);
    pub(crate) fn zb_k_sg_device(d: *mut bindings::device, t: *mut bindings::sg_table);
    fn zb_k_vma_prepare(
        v: *mut bindings::vm_area_struct,
        ops: *const bindings::vm_operations_struct,
    );
    fn zb_k_claim(base: u64, size: u64) -> bool;
    fn zb_k_unclaim(base: u64, size: u64);
}
pub(crate) fn raw_device<C: device::DeviceContext>(d: &device::Device<C>) -> *mut bindings::device {
    // Device is repr(transparent); this is a borrowed pointer, never an ownership transfer.
    ptr::from_ref(d).cast_mut().cast()
}
fn raw_pci<C: device::DeviceContext>(d: &pci::Device<C>) -> *mut bindings::pci_dev {
    ptr::from_ref(d).cast_mut().cast()
}
#[derive(Clone, Copy)]
pub(crate) struct Device(*const Core);
impl Device {
    pub(crate) fn new(core: &Arc<Core>) -> Self {
        Self(Arc::as_ptr(core))
    }
    fn core(&self) -> &Core {
        // SAFETY: Device is only stored in Core.state. Every caller holds that
        // Core's mutex through a live Arc (PCI, FD, work, sysfs or quarantine).
        unsafe { &*self.0 }
    }
    pub(crate) fn read(self, off: u32) -> u32 {
        self.core()
            .bar
            .try_access_with(|bar| bar.try_read32(off as usize).unwrap_or(u32::MAX))
            .unwrap_or(u32::MAX)
    }
    pub(crate) fn write(self, off: u32, value: u32) {
        let _ = self
            .core()
            .bar
            .try_access_with(|bar| bar.try_write32(value, off as usize));
    }
    pub(crate) fn read64(self, off: u32) -> u64 {
        self.read(off) as u64 | ((self.read(off + 4) as u64) << 32)
    }
    pub(crate) fn write64(self, off: u32, value: u64) {
        self.write(off, value as u32);
        self.write(off + 4, (value >> 32) as u32);
    }
    pub(crate) fn barrier(self) {
        unsafe { zb_k_barrier() };
    }
    pub(crate) fn now(self) -> u64 {
        kernel::time::Monotonic::ktime_get() as u64
    }
    pub(crate) fn sleep(self) {
        kernel::time::delay::fsleep(kernel::time::Delta::from_micros(500));
    }
    pub(crate) fn pin(self) {
        // SAFETY: Driver is registered and its module is live for every caller.
        unsafe { bindings::__module_get(crate::THIS_MODULE.as_ptr()) };
    }
    pub(crate) fn unpin(self) {
        // SAFETY: Matched to State::pin. Open files have an independent fops
        // owner reference; worker is flushed before module teardown can finish.
        unsafe { bindings::module_put(crate::THIS_MODULE.as_ptr()) };
    }
    pub(crate) fn session(self) -> [u64; 2] {
        let mut id = [0u64; 2];
        // SAFETY: Writable array of exactly sixteen bytes.
        unsafe { bindings::get_random_bytes(id.as_mut_ptr().cast(), 16) };
        id
    }
    pub(crate) fn reserve(
        self,
        size: u64,
        tier: u32,
        node: i32,
    ) -> core::result::Result<OwnedBacking, i32> {
        OwnedBacking::reserve(self.core().pdev.as_ref().as_ref(), size, tier, node)
            .map_err(|e| e.to_errno())
    }
}
// SAFETY: State's fields have no thread affinity; all access and raw-pointer
// lifetime operations are serialized by Core.state. See docs/unsafe-audit.md.
unsafe impl Send for State {}
pub(crate) fn new_state(provider: bool) -> Result<KVBox<State>> {
    let mut state = KVBox::<State>::new_uninit(GFP_KERNEL)?;
    // SAFETY: State consists solely of integers, booleans, arrays and nullable
    // raw pointers. Zero is valid for every field. No stack-sized State exists.
    unsafe {
        ptr::write_bytes(state.as_mut_ptr(), 0, 1);
        let mut state = state.assume_init();
        state.provider = provider;
        state.online = true;
        state.ledger.next_handle = 1;
        Ok(state)
    }
}
impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: Last Core owner, no VMA/fault/work access remains. Published
        // resources never reach Drop, as unbind retains a QuarantineOwner Arc.
        if !self.mapping_inode.is_null() {
            unsafe { bindings::iput(self.mapping_inode) };
        }
    }
}
pub(crate) fn check_dma(d: &device::Device<device::Core>) -> Result {
    // SAFETY: Probe owns a live device before any allocations or mappings.
    unsafe {
        if !zb_k_dma_supported(raw_device(d)) {
            return Err(Error::from_errno(-95));
        }
        zb_k_max_segment(raw_device(d));
        Ok(())
    }
}
pub(crate) struct PciPower(pub ARef<pci::Device>);
impl Drop for PciPower {
    fn drop(&mut self) {
        // SAFETY: Reference keeps pci_dev alive; all worker accesses stopped and
        // BAR devres accesses are revoked on unbind. Backing remains quarantined.
        unsafe {
            bindings::pci_clear_master(raw_pci(&self.0));
            bindings::pci_disable_device(raw_pci(&self.0));
        }
    }
}
pub(crate) struct PciAdapter;
// SAFETY: Identical layout/data and callbacks to native pci::Adapter<Bridge>.
unsafe impl driver::DriverLayout for PciAdapter {
    type DriverType = bindings::pci_driver;
    type DriverData = Bridge;
    const DEVICE_DRIVER_OFFSET: usize = core::mem::offset_of!(bindings::pci_driver, driver);
}
// SAFETY: Registration/unregistration delegate to native Rust PCI Adapter.
unsafe impl driver::RegistrationOps for PciAdapter {
    unsafe fn register(
        reg: &Opaque<Self::DriverType>,
        name: &'static CStr,
        module: &'static ThisModule,
    ) -> Result {
        // SAFETY: Fresh pinned pci_driver; hooks are installed before publication.
        unsafe {
            zb_k_pci_hooks(reg.get());
            <pci::Adapter<Bridge> as driver::RegistrationOps>::register(reg, name, module)
        }
    }
    unsafe fn unregister(reg: &Opaque<Self::DriverType>) {
        // SAFETY: Called once for a successfully registered pinned driver.
        unsafe { <pci::Adapter<Bridge> as driver::RegistrationOps>::unregister(reg) };
    }
}

kernel::sync::global_lock! {
    // SAFETY: Initialized once in module init before either driver registers.
    unsafe(uninit) static ROLES: Mutex<[Option<Arc<Core>>; 2]> = [None, None];
}
pub(crate) fn init_registry() {
    unsafe { ROLES.init() };
}
pub(crate) struct Role {
    core: Arc<Core>,
    index: usize,
}
impl Role {
    pub(crate) fn new(core: &Arc<Core>, provider: bool) -> Result<Self> {
        let mut roles = ROLES.lock();
        let slot = &mut roles[provider as usize];
        if slot.is_some() {
            return Err(EBUSY);
        }
        *slot = Some(core.clone());
        Ok(Self {
            core: core.clone(),
            index: provider as usize,
        })
    }
}
impl Drop for Role {
    fn drop(&mut self) {
        // A quarantined role remains occupied. Rebinding must not create a new
        // State with epoch zero over hardware that may still know old extents.
        if self.core.state.lock().pinned {
            return;
        }
        let mut roles = ROLES.lock();
        if roles[self.index]
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, &self.core))
        {
            roles[self.index] = None;
        }
    }
}
pub(crate) struct Platform;
impl Platform {
    pub(crate) fn new() -> Result<Self> {
        kernel::error::to_result(unsafe { zb_k_platform_register() })?;
        Ok(Self)
    }
}
impl Drop for Platform {
    fn drop(&mut self) {
        unsafe { zb_k_platform_unregister() };
    }
}
pub(crate) struct ClassDevice(*mut bindings::device);
// SAFETY: device_unregister may run on any task. Class callbacks use Core's mutex.
unsafe impl Send for ClassDevice {}
impl ClassDevice {
    pub(crate) fn new(core: &Arc<Core>, provider: bool) -> Result<Self> {
        // SAFETY: PCI owner keeps Core alive until after class unregister, which
        // waits for all sysfs callbacks. No C-owned device context exists.
        let dev = unsafe {
            zb_k_class_add(
                raw_device(core.pdev.as_ref().as_ref()),
                Arc::as_ptr(core).cast_mut().cast(),
                provider,
            )
        };
        if (dev as isize) < 0 && (dev as isize) >= -4095 {
            return Err(Error::from_errno(dev as i32));
        }
        Ok(Self(dev))
    }
}
impl Drop for ClassDevice {
    fn drop(&mut self) {
        unsafe { bindings::device_unregister(self.0) };
    }
}

pub(crate) fn admin() -> Result {
    // SAFETY: Checks the current caller in the initial user namespace.
    if unsafe { bindings::capable(bindings::CAP_SYS_ADMIN as _) } {
        Ok(())
    } else {
        Err(EPERM)
    }
}
// fops must outlive Node and every surviving FD after PCI unbind. Immutable
// module-static storage plus fops.owner supplies that lifetime.
struct FileOps(bindings::file_operations);
// SAFETY: Immutable static vtable, native callbacks synchronize through Core.
unsafe impl Sync for FileOps {}
static FILE_OPS: FileOps = FileOps({
    // SAFETY: The const native builder returns a static initialized vtable.
    let mut ops = unsafe {
        *MiscDeviceOptions {
            name: c"zettbridge",
        }
        .into_raw::<Session>()
        .fops
    };
    ops.owner = crate::THIS_MODULE.as_ptr();
    ops
});
pub(crate) fn misc_register(
    p: *mut bindings::miscdevice,
    options: MiscDeviceOptions,
    pdev: &pci::Device,
) -> Result {
    // SAFETY: Pinned Node storage, immutable module-static fops outlive every FD. Session uses native MiscDevice file callbacks and layout.
    unsafe {
        let mut raw = options.into_raw::<Session>();
        raw.fops = &FILE_OPS.0;
        raw.parent = raw_device(pdev.as_ref());
        raw.mode = 0o600;
        p.write(raw);
        kernel::error::to_result(bindings::misc_register(p))
    }
}
pub(crate) fn misc_unregister(p: *mut bindings::miscdevice) {
    unsafe { bindings::misc_deregister(p) };
}
pub(crate) fn misc_core(misc: &MiscDeviceRegistration<Session>) -> Arc<Core> {
    // SAFETY: Node installs the native vtable before registration. The native
    // registration wrapper is repr(transparent) over miscdevice; its callback
    // can borrow our identically laid-out misc field. misc_open prevents node
    // removal during open. Only clone the already initialized Core reference.
    let node = unsafe {
        kernel::container_of!(
            misc.as_raw().cast::<Opaque<bindings::miscdevice>>(),
            Node,
            misc
        )
    };
    unsafe { &*node }.core.clone()
}
pub(crate) fn share_mapping(s: &mut State, file: &File) -> Result {
    // SAFETY: Called only during open before file publication. The inode ref
    // retains the shared address_space until Core is dropped after every VMA.
    unsafe {
        if s.mapping.is_null() {
            s.mapping_inode = bindings::igrab((*file.as_ptr()).f_inode);
            if s.mapping_inode.is_null() {
                return Err(ENODEV);
            }
            s.mapping = (*file.as_ptr()).f_mapping;
        }
        (*file.as_ptr()).f_mapping = s.mapping;
    }
    Ok(())
}
pub(crate) fn revoke(mapping: *mut bindings::address_space) {
    // SAFETY: Retained by Core's inode reference. Called with a live Core Arc,
    // outside its mutex to preserve VMA -> State lock ordering.
    if !mapping.is_null() {
        unsafe { bindings::unmap_mapping_range(mapping, 0, 0, 1) };
    }
}
pub(crate) fn vma_pgoff(vma: &VmaNew) -> u64 {
    unsafe { (*vma.as_ptr()).vm_pgoff as u64 }
}
const VM_FAULT_SIGBUS: bindings::vm_fault_t = 2;
static VM_OPS: bindings::vm_operations_struct = bindings::vm_operations_struct {
    fault: Some(vma_fault),
    ..pin_init::zeroed()
};
pub(crate) fn prepare_vma(vma: &VmaNew) {
    // SAFETY: Exclusive VMA initialization, ops static for module lifetime, and
    // vm_file holds the module owner and Rust Session until every VMA closes.
    unsafe { zb_k_vma_prepare(vma.as_ptr(), &VM_OPS) };
}
unsafe extern "C" fn vma_fault(vmf: *mut bindings::vm_fault) -> bindings::vm_fault_t {
    // SAFETY: Kernel supplies live vm_fault and vm_file, created by our mmap.
    // vm_file retains Session. The State mutex serializes revocation and fault.
    unsafe {
        let vma = (*vmf).__bindgen_anon_1.vma;
        let file = (*vma).vm_file;
        let session = <KBox<Session> as ForeignOwnable>::borrow((*file).private_data);
        let mut s = session.core.state.lock();
        if !s.online || !s.dax_live {
            return VM_FAULT_SIGBUS;
        }
        let address = (*vmf).__bindgen_anon_1.address;
        let offset = (address - (*vma).__bindgen_anon_1.__bindgen_anon_1.vm_start) as u64;
        let length = (*vma).__bindgen_anon_1.__bindgen_anon_1.vm_end
            - (*vma).__bindgen_anon_1.__bindgen_anon_1.vm_start;
        match s.map(session.owner, (*vma).vm_pgoff as u64, length as u64) {
            Ok(dpa) if offset < s.dax_size && dpa < s.dax_size - offset => {
                bindings::vmf_insert_pfn_prot(
                    vma,
                    address,
                    ((s.dax_base + dpa + offset) >> 12) as usize,
                    (*vma).vm_page_prot,
                )
            }
            _ => VM_FAULT_SIGBUS,
        }
    }
}
#[no_mangle]
pub(crate) unsafe extern "C" fn zb_rs_offline(pdev: *mut bindings::pci_dev) {
    let core = ROLES
        .lock()
        .iter()
        .flatten()
        .find(|c| raw_pci(&c.pdev) == pdev)
        .cloned();
    if let Some(core) = core {
        core.offline();
    }
}
#[no_mangle]
pub(crate) unsafe extern "C" fn zb_rs_dax_probe(
    _dev: *mut bindings::device,
    base: u64,
    size: u64,
) -> *mut c_void {
    let attach = || -> Result<Arc<Core>> {
        let core = ROLES.lock()[0].clone().ok_or(ENODEV)?;
        {
            let mut s = core.state.lock();
            if !s.online || s.dax_live || s.pool_size != 0 {
                return Err(EBUSY);
            }
            if size != s.capacity || base != s.dev.read64(0x350) || size != s.dev.read64(0x358) {
                return Err(ENODEV);
            }
            // SAFETY: Exact range validated against linked CXL HDM decoder.
            if !unsafe { zb_k_claim(base, size) } {
                return Err(EBUSY);
            }
            if let Err(e) = s.attach_dax(size) {
                unsafe { zb_k_unclaim(base, size) };
                return Err(Error::from_errno(e));
            }
            s.dax_base = base;
            s.dax_size = size;
            s.dax_live = true;
        }
        pr_info!(
            "zettbridge: Rust exclusively owns {} DAX bytes at {:#x}\n",
            size,
            base
        );
        Ok(core)
    };
    match attach() {
        Ok(c) => Arc::into_raw(c).cast_mut().cast(),
        Err(e) => e.to_errno() as isize as *mut c_void,
    }
}
#[no_mangle]
pub(crate) unsafe extern "C" fn zb_rs_dax_remove(owner: *mut c_void) {
    // SAFETY: One Arc was transferred to DAX at successful probe; remove runs once.
    let core = unsafe { Arc::<Core>::from_raw(owner.cast()) };
    {
        let mut s = core.state.lock();
        s.fault();
        s.dax_live = false;
    }
    core.revoke();
    // Keep resource claim and module pin: removal is not a retirement proof.
    let _ = Arc::into_raw(core);
}
#[no_mangle]
pub(crate) unsafe extern "C" fn zb_rs_show(
    owner: *mut c_void,
    tier: i32,
    field: i32,
    buffer: *mut core::ffi::c_char,
) -> isize {
    // SAFETY: Class unregister waits for sysfs callbacks before Core is dropped.
    // Sysfs provides a PAGE_SIZE output buffer; render writes at most 64 bytes.
    let core = unsafe { &*owner.cast::<Core>() };
    let stats = core.state.lock().stats(tier.max(0) as u32);
    let bytes = unsafe { core::slice::from_raw_parts_mut(buffer.cast::<u8>(), 64) };
    crate::sysfs::render(&stats, tier, field, bytes) as isize
}
