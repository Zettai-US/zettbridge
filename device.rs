// SPDX-License-Identifier: GPL-2.0-only
//! Native Rust PCI, file operations, work queue and device ownership.
use crate::{
    ffi,
    uapi::{Request, Stats},
    State,
};
use kernel::{
    bindings, device,
    devres::Devres,
    dma::{Device as _, DmaMask},
    driver,
    fs::File,
    ioctl::{_IOC_NR, _IOR, _IOWR},
    miscdevice::{MiscDevice, MiscDeviceOptions, MiscDeviceRegistration},
    mm::virt::VmaNew,
    pci,
    prelude::*,
    sync::{aref::ARef, Arc, Completion, Mutex},
    types::Opaque,
    uaccess::UserSlice,
    workqueue::{self, impl_has_delayed_work, new_delayed_work, DelayedWork, WorkItem},
};

#[pin_data]
pub(crate) struct Core {
    pub(crate) pdev: ARef<pci::Device>,
    #[pin]
    pub(crate) bar: Devres<pci::Bar<0x4000>>,
    #[pin]
    pub(crate) state: Mutex<KVBox<State>>,
    #[pin]
    work: DelayedWork<Core>,
    #[pin]
    done: Completion,
}
impl_has_delayed_work! { impl HasDelayedWork<Self> for Core { self.work } }
impl Core {
    fn start(this: &Arc<Self>) {
        let _ = workqueue::system_unbound()
            .enqueue_delayed(this.clone(), kernel::time::msecs_to_jiffies(100));
    }
    pub(crate) fn offline(&self) {
        {
            let mut s = self.state.lock();
            if s.online {
                s.fault();
                s.online = false;
            }
        }
        // The sole queued worker owns an Arc and acknowledges shutdown only
        // after its last State/MMIO access. No driver lock during VMA walking.
        self.done.wait_for_completion();
        // SAFETY: Completion means no future requeue. Flush also waits for the
        // final callback/Arc destructor to return before module code is freed.
        unsafe {
            let work = DelayedWork::raw_as_work(&self.work);
            bindings::flush_work(workqueue::Work::raw_get(work));
        }
        self.revoke();
    }
    pub(crate) fn revoke(&self) {
        let mapping = self.state.lock().mapping;
        ffi::revoke(mapping);
    }
}
impl WorkItem for Core {
    type Pointer = Arc<Self>;
    fn run(this: Arc<Self>) {
        let revoke = {
            let mut s = this.state.lock();
            if !s.online {
                this.done.complete_all();
                return;
            }
            s.tick(false) > 0
        };
        if revoke {
            this.revoke();
            let mut s = this.state.lock();
            if s.online {
                s.tick(true);
            }
        }
        // Always queue the next execution. Unbind may set online=false between
        // here and queueing; that execution then acknowledges the shutdown.
        Self::start(&this);
    }
}

#[pin_data]
pub(crate) struct Bridge {
    // Deregister the misc and sysfs callbacks before dropping Core references.
    #[pin]
    _node: Node,
    _class: ffi::ClassDevice,
    _role: ffi::Role,
    _active: Active,
    core: Arc<Core>,
    _power: ffi::PciPower,
}
kernel::pci_device_table!(
    PCI_TABLE,
    MODULE_PCI_TABLE,
    bool,
    [
        (pci::DeviceId::from_id(pci::Vendor::REDHAT, 0x10f0), true),
        (pci::DeviceId::from_id(pci::Vendor::REDHAT, 0x10f1), false),
    ]
);
impl pci::Driver for Bridge {
    type IdInfo = bool;
    const ID_TABLE: pci::IdTable<bool> = &PCI_TABLE;
    fn probe(pdev: &pci::Device<device::Core>, provider: &bool) -> impl PinInit<Self, Error> {
        let provider = *provider;
        pin_init::pin_init_scope(move || {
            pdev.enable_device_mem()?;
            let power = ffi::PciPower(pdev.into());
            if pdev.resource_len(0)? != 0x4000 {
                return Err(EINVAL);
            }
            if provider {
                // SAFETY: Probe precedes publication and all DMA allocations.
                unsafe {
                    pdev.dma_set_mask_and_coherent(DmaMask::new::<64>())?;
                }
                ffi::check_dma(pdev.as_ref())?;
                pdev.set_master();
            }
            let state = ffi::new_state(provider)?;
            let core = Arc::pin_init(
                try_pin_init!(Core {
                    bar <- pdev.iomap_region_sized::<0x4000>(0, c"zettbridge"),
                    pdev: pdev.into(),
                    state <- kernel::new_mutex!(state),
                    work <- new_delayed_work!("zettbridge"),
                    done <- Completion::new(),
                }),
                GFP_KERNEL,
            )?;
            {
                let mut s = core.state.lock();
                s.dev = ffi::Device::new(&core);
                if s.dev.read(0) != 0x5a520002
                    || s.dev.read(0x2f0) != 0x5a424d01
                    || s.dev.read(8) & 31 != 31
                {
                    return Err(ENODEV);
                }
                // Read-only platform capacity is distinct from currently
                // exported bytes, which remain zero before COMMIT.
                s.capacity = s.dev.read64(0x360);
                if !crate::capacity::valid_pool(s.capacity) {
                    return Err(EINVAL);
                }
                s.ats_required = provider && s.dev.read(0x368) & 1 != 0;
                if s.ats_required {
                    ffi::check_ats(pdev)?;
                    if s.dev.read(0x368) != 7 {
                        return Err(ENODEV);
                    }
                }
            }
            let active = Active::new(&core);
            let class = ffi::ClassDevice::new(&core, provider)?;
            let role = ffi::Role::new(&core, provider)?;
            let ats_mode = if core.state.lock().dev.read(0x368) & 2 != 0 {
                "enabled"
            } else {
                "disabled"
            };
            let node_core = core.clone();
            Ok(try_pin_init!(Self {
                _class: class,
                _role: role,
                _active: active,
                core,
                _power: power,
                _node <- Node::new(node_core, provider),
                _: {
                    dev_info!(pdev, "Zettai RPU v0.2 native Rust {}, provider ATS {}\n",
                        if provider { "provider" } else { "consumer" },
                        ats_mode);
                },
            }))
        })
    }
    fn unbind(_pdev: &pci::Device<device::Core>, this: Pin<&Self>) {
        this.core.offline();
    }
}

// Own the running worker before publishing callbacks. This also handles any
// probe failure after sysfs/DAX registration or a concurrent successful open.
struct Active(Arc<Core>);
impl Active {
    fn new(core: &Arc<Core>) -> Self {
        Core::start(core);
        Self(core.clone())
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0.offline();
        if self.0.state.lock().pinned {
            // Explicit QuarantineOwner survives devres and all registrations.
            let _ = Arc::into_raw(self.0.clone());
            pr_warn!("zettbridge: removed resources retained in quarantine\n");
        }
    }
}

// Native MiscDeviceOptions lacks owner/parent/mode in this target kernel. This
// Rust registration adapter retains the native MiscDevice vtable, copying it
// into immutable module storage to supply owner before misc_register.
#[pin_data(PinnedDrop)]
pub(crate) struct Node {
    pub(crate) core: Arc<Core>,
    #[pin]
    pub(crate) misc: Opaque<bindings::miscdevice>,
}
// SAFETY: Registration/deregistration can run on any task; the mutex protects Core.
unsafe impl Send for Node {}
impl Node {
    fn new(core: Arc<Core>, provider: bool) -> impl PinInit<Self, Error> {
        let opts = MiscDeviceOptions {
            name: if provider {
                c"rpu_provider0"
            } else {
                c"rpu_consumer0"
            },
        };
        try_pin_init!(Self {
            core,
            misc <- Opaque::try_ffi_init(move |p| ffi::misc_register(p, opts, &core.pdev)),
        })
    }
}
#[pinned_drop]
impl PinnedDrop for Node {
    fn drop(self: Pin<&mut Self>) {
        ffi::misc_unregister(self.misc.get());
    }
}
pub(crate) struct Session {
    pub(crate) core: Arc<Core>,
    pub(crate) owner: u64,
}
#[vtable]
impl MiscDevice for Session {
    type Ptr = KBox<Self>;
    fn open(file: &File, misc: &MiscDeviceRegistration<Self>) -> Result<Self::Ptr> {
        ffi::admin()?;
        let core = ffi::misc_core(misc);
        let mut s = core.state.lock();
        if !s.online {
            return Err(ENODEV);
        }
        let owner = s.owner_seq.checked_add(1).ok_or(EOVERFLOW)?;
        let session = KBox::new(
            Session {
                core: core.clone(),
                owner,
            },
            GFP_KERNEL,
        )?;
        ffi::share_mapping(&mut s, file)?;
        s.owner_seq = owner;
        Ok(session)
    }
    fn release(session: Self::Ptr, _file: &File) {
        session.core.state.lock().close(session.owner);
    }
    fn ioctl(session: &Self, _file: &File, cmd: u32, arg: usize) -> Result<isize> {
        ffi::admin()?;
        let nr = _IOC_NR(cmd);
        let caps = _IOR::<[u64; 8]>('Z' as u32, 0);
        let stats = _IOWR::<Stats>('Z' as u32, 1);
        let request = _IOWR::<Request>('Z' as u32, nr);
        let user = UserPtr::from_addr(arg);
        if cmd == caps {
            let value = {
                let s = session.core.state.lock();
                if !s.online {
                    return Err(ENODEV);
                }
                [
                    64,
                    2 | ((s.provider as u64) << 32),
                    31,
                    4096,
                    65536,
                    s.capacity,
                    0,
                    0,
                ]
            };
            UserSlice::new(user, 64).writer().write(&value)?;
        } else if cmd == stats {
            let input = UserSlice::new(user, 256).reader().read::<Stats>()?;
            input.validate_input()?;
            let value = {
                let s = session.core.state.lock();
                if !s.online {
                    return Err(ENODEV);
                }
                s.stats(input.tier)
            };
            UserSlice::new(user, 256).writer().write(&value)?;
        } else if cmd == request && (2..=9).contains(&nr) {
            let mut value = UserSlice::new(user, 64).reader().read::<Request>()?;
            {
                let mut s = session.core.state.lock();
                if !s.online {
                    return Err(ENODEV);
                }
                s.request(session.owner, nr, &mut value)
                    .map_err(Error::from_errno)?;
            }
            // No rollback on copyout failure: retry/query uses the transaction
            // already recorded while holding the Rust mutex.
            UserSlice::new(user, 64).writer().write(&value)?;
        } else {
            return Err(ENOTTY);
        }
        Ok(0)
    }
    fn mmap(session: &Self, _file: &File, vma: &VmaNew) -> Result {
        if vma.flags() & bindings::VM_SHARED == 0 || vma.flags() & bindings::VM_EXEC != 0 {
            return Err(EINVAL);
        }
        let mut s = session.core.state.lock();
        if s.provider || !s.online || !s.dax_live {
            return Err(ENODEV);
        }
        s.map(
            session.owner,
            ffi::vma_pgoff(vma),
            (vma.end() - vma.start()) as u64,
        )
        .map_err(Error::from_errno)?;
        ffi::prepare_vma(vma);
        Ok(())
    }
}

pub(crate) struct Module {
    // Unregister PCI (and stop every worker) before private DAX/class glue.
    _pci: Pin<KBox<driver::Registration<ffi::PciAdapter>>>,
    _platform: ffi::Platform,
}
impl kernel::Module for Module {
    fn init(module: &'static ThisModule) -> Result<Self> {
        ffi::init_registry();
        let platform = ffi::Platform::new()?;
        let pci = KBox::try_pin_init(driver::Registration::new(c"zettbridge", module), GFP_KERNEL)?;
        Ok(Self {
            _pci: pci,
            _platform: platform,
        })
    }
}
