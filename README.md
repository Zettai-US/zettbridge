# Zettai CXL Bridge v0.2

English | [简体中文](README.zh-CN.md)

This repository implements the Rust kernel provider and consumer described in
`Zettai_CXL_Bridge_Rust_Kernel_Spec_v0.2.docx`, together with a functional
two-QEMU model. Kbuild builds the out-of-tree `zettbridge.ko`; the driver does
not need to be added to the kernel source tree. The default target kernel is
`~/linux`. Its SHA, configuration digest, toolchain and deployment parameters
are recorded in [docs/deployment.json](docs/deployment.json).

Host A applications use a lease FD to perform ordinary CPU loads and stores
through a standard CXL region. QEMU's HDM decoder translates HPA_A to DPA_A;
the `zettbridge` extent engine forwards each request to the Host B PCI
requester, which accesses Rust provider-owned pages through **Host B's Intel
IOMMU and ATS**. With ATS enabled, the requester uses a device translation
cache and translated DMA. The ATS-off baseline uses `pci_dma_read/write`.
The two VMs have independent RAM: there is no shared payload file and no
userspace daemon owns exported memory.

```text
Host A guest                         Host B guest
app mmap(/dev/rpu_consumer0)          /dev/rpu_provider0
  Rust lease ledger                    Rust ExportManager
  exclusive DAX + UC mappings          owned pages + DMA API
          |                                      ^
standard CXL Type 3 / HDM               Port B PCI requester / vIOMMU
          |                                      |
          +-- DPA -> extent -> DMA_B ------------+
              QEMU-to-QEMU Unix SEQPACKET
```

The driver is in [Zettai-US/zettbridge](https://github.com/Zettai-US/zettbridge).
The matching QEMU branch is
[Zettai-US/qemu-cxl-type2](https://github.com/Zettai-US/qemu-cxl-type2/tree/codex/zettbridge-rust-v0.2)
at commit `c16508c908fc5761ea158d8386db738978c36390`. The matching
`CXLMemSim` branch pins that QEMU submodule commit; after cloning it, run
`git submodule update --init lib/qemu`.

## Build and end-to-end check

```sh
cd ~/zettbridge
make KDIR=~/linux LLVM=1
make tools test
ninja -C ~/CXLMemSim/lib/qemu/build qemu-system-x86_64 -j 12
python3 tools/run_pair.py --output /tmp/zettbridge-normal
```

The launcher builds a temporary initramfs and starts Host B and Host A QEMU
instances in that order. Each VM defaults to 2 GiB RAM, 2 vCPUs and
`~/linux/arch/x86/boot/bzImage`. To use different kernels, specify
`--provider-kernel`, `--consumer-kernel`, and matching prebuilt
`--provider-module` and `--consumer-module` files. The launcher checks module
vermagic before boot and packages separate initramfs images. Test mode cleans
up only its own processes; it does not modify disk images or load a module on
the host. Use a fresh output directory. Success prints `PAIR PASS` and retains
both serial logs, initramfs images and exact commands in `run.json`. The
`kernels.provider` and `kernels.consumer` entries record each source commit,
release and binary checksum.

`--transport vfio-user-cxl` starts the separate
`tools/vfio_user_cxl_backend.py` process. Consumer QEMU sends BAR operations
as vfio-user region messages and CXL.mem operations as negotiated private
commands. The backend relays requests to Provider QEMU, which still owns the
PCI requester, IOMMU and ATS model. This is a Zettbridge-specific vfio-user
extension, not a generic vfio-user CXL Type 3 device. `run.json` records the
backend command, PID and transport.

```sh
python3 tools/run_pair.py --transport vfio-user-cxl \
  --output /tmp/zettbridge-vfio-user
```

## Larger persistent pool

```sh
python3 tools/run_pair.py --capacity 8G --provider-memory 12G \
  --consumer-memory 4G --keep-running --output /tmp/zettbridge-8g
python3 tools/guest_console.py /tmp/zettbridge-8g consumer --command 'zbctl consumer-stats'
python3 tools/guest_console.py /tmp/zettbridge-8g provider --command 'zbctl provider-stats'
# Interactive serial console; Ctrl-] disconnects. No SSH or network port is required.
python3 tools/guest_console.py /tmp/zettbridge-8g consumer
# RELEASE, QUIESCE, DRAIN, RETIRE and RECLAIM precede QEMU shutdown.
python3 tools/stop_pair.py /tmp/zettbridge-8g
```

`--capacity` sets the provider and consumer management devices, Type 3
capacity, CFMWS and guest CXL region together. It accepts powers of two from
256M to 64G. The validated sizes are 256 MiB and 8 GiB; other capacities
still need testing. Provider RAM must exceed the pool by at least 1 GiB. By
default it is chosen as at least 1.5 times the pool and at least 2 GiB.
Large-capacity RPC timeout is 30 seconds. `--timeout` sets the overall
startup/test deadline, 600 seconds by default.

`--keep-running` samples and writes across windows spanning the full aperture,
then holds a 128 MiB lease and touches both ends every five seconds. The rest
of the pool remains allocatable. **The managed CXL pool is not ordinary System
RAM**: `free` shows guest-local RAM; use `zbctl` or `/sys/class/rpu/` for pool
capacity and usage. Each lease is limited to 4 GiB. Larger pools use multiple
leases to preserve the `handle << 32` mmap-offset isolation ABI.

In persistent mode, the launcher exits after printing `PAIR RUNNING`; QEMU
continues running. `run.json` and `.pid` files record PIDs. `.console` and
`.qmp` are local Unix sockets; `.log` contains guest serial output and
`.qemu.log` contains QEMU output. Without `--keep-running`, the launcher
still checks full reclamation and shuts down both VMs.

Dependencies: a built target kernel with Rust metadata; C, Clang, Rust and
bindgen toolchains; static BusyBox; the `cxl` utility; cpio; Python 3; and
KVM. `--accel tcg` works without KVM but has different performance.
`--kernel` and `--qemu` override the default paths.

## Fault and hardware-ABI checks

```sh
python3 tools/run_pair.py --drop-commit-ack --output /tmp/zettbridge-ack
python3 tools/run_pair.py --drop-commit-ack --query-unknown --output /tmp/zettbridge-quarantine
python3 ~/CXLMemSim/lib/qemu/tests/qtest/zettbridge-test.py \
  ~/CXLMemSim/lib/qemu/build/qemu-system-x86_64
```

When an ACK is lost but a query returns DONE, the transaction continues.
When neither ACK nor query proves completion, the test requires `ETIMEDOUT`,
`state=faulted`, retention of the 256 MiB backing and DMA mapping, and
rejection of RECLAIM.

## Guest sequence

1. Load `zettbridge.ko` in both guests. The provider requires a translated
   IOMMU and 64-bit coherent DMA. The launcher keeps ATS enabled by default;
   Rust checks that Linux IOMMU enabled it. `--ats off` runs the baseline.
2. Host B keeps one `/dev/rpu_provider0` FD open for RESERVE, PREPARE and
   COMMIT. PREPARE writes a pattern through the CPU across all backing,
   reads and inverts it by DMA, checks it through the CPU, then zeroes it and
   returns DMA ownership.
3. Host A creates a standard region with
   `cxl create-region -d decoder0.0 -m mem0 -s 256M -t ram -w 1`. This test
   does not load `device_dax` or `dax_kmem`.
4. `echo dax0.0 > /sys/bus/dax/drivers/zettbridge/new_id` gives the DAX
   device exclusively to the consumer. The driver checks that its HPA range
   equals the linked Type 3 committed HDM aperture, then creates a random
   128-bit session. Host B enables the export after ATTACH.
5. The application allocates a lease from `/dev/rpu_consumer0` and mmaps the
   aligned capacity at file offset **`object_handle << 32`**. Every access
   stays within that lease; a split VMA stays in the same handle domain.
6. RELEASE returns pending. A worker blocks new faults, revokes related VMAs
   (including fork and dup references), then issues LEASE_BARRIER. The range
   cannot be reused before completion. Reallocation zeroes old contents
   through the device.
7. Host B requests the consumer to freeze and revoke all access. After
   consumer DETACH completes, Host B performs QUIESCE, DRAIN and RETIRE.
   RECLAIM may unmap/free only in the reclaimable state.

The UAPI, registers and model extensions are in [docs/abi.md](docs/abi.md).
`tools/zbctl.c` is a complete C example that holds the FD and checks
permissions, idempotency, copyout faults, concurrent snapshots and fork/revoke.

## Rust out-of-tree implementation

Rust implements PCI probe/unbind, device context, ioctl/compat/release, mmap
faults, workqueues, NUMA backing, SG DMA, coherent extent tables, DAX
ownership and sysfs statistics. It uses the target kernel's `pci::Driver`,
`pci::Adapter`, `MiscDevice`, `Arc`, `Mutex`, `DelayedWork`, `Completion`,
`UserSlice`, `CoherentAllocation` and SG iterator.

`kernel_shim.c` contains only wrappers for unavailable kernel macros/inlines,
private DAX bus descriptors, AER/PM hook forwarding and sysfs attribute
declarations. It owns no device context, file operations, work tasks, page
arrays, allocation loops or release policy. The Rust file-operation
registration adapter adds `owner`, `parent` and `mode` to this kernel's native
MiscDevice vtable so open FDs pin the module after PCI unbind. No change to
`~/linux` is needed.

| File | Responsibility |
| --- | --- |
| `driver.rs`, `provider.rs` | Module entry, state, transaction deduplication, publication and reclaim |
| `device.rs` | Native Rust PCI and MiscDevice file operations, Arc/Mutex, delayed work, failure and unbind cleanup |
| `backing.rs` | Rust page ownership, strict NUMA/source checks, SG maps, coherent table and release after explicit retirement |
| `ffi.rs` | Raw-pointer boundaries, Rust VMA fault and DAX callbacks, registration adapter, quarantined-role retention |
| `kernel_shim.c` | Stateless kernel helpers and private DAX ABI declarations |
| `consumer.rs`, `accounting.rs` | Lease ledger, session and complete usage reports |
| `transport.rs`, `recovery.rs` | Bounded ACK/query, fault quarantine and module pin |
| `sysfs.rs`, `uapi.rs`, `include/` | Consistent statistics, Rust sysfs formatting and fixed-layout ABI |
| `../CXLMemSim/lib/qemu/hw/cxl/zettbridge.c` | Two management banks, table checks, DMA requester, relay and functional barrier |
| `../CXLMemSim/lib/qemu/hw/mem/cxl_type3.c` | Optional `zettbridge=` link and standard CXL discovery/HDM path |

The additional Rust device/module lifetime check is:

```sh
python3 tools/run_pair.py --lifetime --output /tmp/zettbridge-lifetime
```

In each guest it verifies: an open FD blocks unload; PCI unbind while the FD
remains open; the old FD returns ENODEV and still blocks unload; closing it
allows unload; reloading and unloading with queued work succeeds. This test
publishes no backing and claims no DAX device. Active or quarantined resources
still block unload.

## Delivered scope and limits

Continuously enabled ATS, translation-cache/invalidation semantics and sysfs
diagnostics are in [docs/ats.md](docs/ats.md). The launcher defaults to
`--ats on`; usage is reported about every 200 ms while ATS stays enabled
through allocation, data access and final reclamation.

The ATS, reset, disconnect, usage-expiry and concurrent VMA-reclaim edge
fixes, reproduction commands and results are in
[docs/edge-cases.md](docs/edge-cases.md). `make test` includes deterministic
races for the production consumer/recovery policy.
`python3 tools/edge_pair.py --output /tmp/zettbridge-edge-new` creates a
dedicated two-VM pair for real mmap and fault-quarantine checks.

The multi-round llama.cpp tool-calling workload and direct KV residency in
Host B's shared pool are described in
[docs/llama-agent.md](docs/llama-agent.md). `tools/build_llama_agent.py` pins
source/model and runs attention reference tests. `tools/run_llama_agent.py`
checks lease addresses for all KV tensors, token/logits fingerprints,
Host B usage and continuous ATS.

Validated scope: **x86-64, a 256 MiB or 8 GiB static volatile aperture,
single-way HDM, Host B DRAM and a UC managed pool in two QEMU guests**. Rust
and QEMU were built, and both guests booted. See
[docs/validation.md](docs/validation.md); raw logs are under `artifacts/`.

The model does not simulate FPGA credits, link latency, dirty write-back cache,
real PCIe posted-write queues or RTL. Synchronous QEMU DMA completion is
drain evidence only for this functional profile; it is not a physical FPGA
K0–K6 acceptance result. Application mappings remain UC. Device-DAX or
ordinary malloc usage must not be counted as lease usage. The CXL System RAM
tier has strict NUMA/resource checks and a coded path, but Host B CXL backing
has not passed this two-VM test and cannot be treated as validated K2.

This version uses bounded polling and an unbound workqueue, without MSI-X
management interrupts. Each guest has one instance of one role, at most 128
simultaneously occupied leases and 256 records for unclosed-owner
transactions. Unresolved records are never evicted at the expense of retry
semantics. Unknown commands return ENOTTY; exceeding limits fails without
changing the ledger. Device removal, AER, reset, PM and ambiguous completion
fail closed and quarantine resources rather than attempting unproven
automatic recovery. Consumer DAX ownership and module pin last until guest
shutdown; an active or quarantined instance cannot be expected to `rmmod`.
This prevents a generic DAX driver from rebinding around old-map isolation.

The VMs use `-cpu host,hypervisor=off` because the target kernel's
`cpu_cache_has_invalidate_memregion()` returns false when the CPUID hypervisor
bit is set, causing standard CXL region commit to fail. This is only a QEMU
test-platform setting. Application mappings stay UC; the target kernel's
cache-invalidation implementation and configuration were not changed.
