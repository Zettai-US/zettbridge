# Frozen v0.2 ABI and QEMU platform extension

The source of truth is `include/zettbridge_hw.h` (copied byte-for-byte into QEMU's `include/hw/cxl/`) and `include/zettbridge_uapi.h`. IDs are emulator assignments, not an allocation for a physical board. Guest addresses and DMA addresses never enter the userspace ABI.

## UAPI

All fields are fixed-width Linux native-endian values with explicit padding. Commands use ioctl type `'Z'`:

| nr | Direction / size | Operation |
| --- | --- | --- |
| 0 | IOR / 64 | GET_CAPS: major=0, minor=2, role=1 provider / 0 consumer |
| 1 | IOWR / 256 | GET_STATS; input contains only struct_size=256 and tier=0/1 |
| 2–6 | IOWR / 64 | RESERVE, PREPARE, COMMIT, QUIESCE, RECLAIM |
| 7–8 | IOWR / 64 | ALLOC_LEASE, RELEASE_LEASE |
| 9 | IOWR / 64 | QUERY_TXN by the same open-file owner and request_id |

The 64-byte request starts with the spec's 32-byte request header, then bytes@32, alignment@40, tier@48, signed numa_node@52, status@56 and reserved@60. `flags` and `reserved` must be zero; input status is zero. Side-effect requests require nonzero request_id. Outputs are fully initialized.

An owner is an open file description, inherited by dup/fork and retained by its VMAs. Provider handles and leases cannot be operated on from a different owner. Repeating an identical command/input with the same request_id returns the cached result; mismatching retries fail. QUERY_TXN does not need the current epoch. Failed copyout does not undo committed state. IDs are not recycled while that owner survives; a finite transaction table returns ENOSPC before performing another side effect. RELEASE/QUIESCE status=1 means pending; QUERY_TXN becomes status=0 after trusted completion.

For RESERVE the profile requires bytes=256 MiB, exact NUMA node, tier=0 DRAM or 1 qualified CXL System RAM, alignment=0 and handle=0. PREPARE/COMMIT operate on the returned handle and old epoch; COMMIT returns the new epoch. ALLOC requires a nonzero length, a power-of-two alignment >=4096 and matching tier/epoch; bytes output is rounded to 4096. mmap offset is `handle << 32`; a 4-GiB offset domain per handle prevents VMA splitting from turning a page offset into a different handle. Offsets inside that domain are checked against the live lease. Handles never repeat in a consumer instance.

GET_STATS exactly follows the document's 256-byte layout. `source_flags` and `valid_mask` must both be interpreted. Provider physical reservation is independent of lease usage. On a fault, current available/exported semantics are invalid; historical mirrored usage can remain with last-known set. Unknown inventory/cache fields have clear valid bits. There is no hardware payload cache.

## Base hardware ABI

Management BAR0 is 16 KiB, separated from standard CXL Type 3 registers. `ID_VERSION=0x5a520002`, `CAPS=0x1f`, `MAX_EXTENTS=65536`, `MIN_EXTENT_ALIGN=4096`. Only aligned 32-bit accesses are accepted; 64-bit staging is written low then high. Extents are 64-byte LE records; inventory/query snapshots are 64 bytes; usage reports are 128 bytes. Reserved bytes must be zero. DMA descriptor allocation is coherent; payload allocation is mapped through the DMA API. The baseline has no DMA command-ring extension.

The ATS platform extension leaves CAPS and the fixed UAPI unchanged. Read-only 64-bit registers at 0x368..0x3a0 hold status, translation requests, cache hits, device invalidations, cached entries, full flushes, faults and ATS Control transitions. Status bits are configured/enabled/notifier-live. Provider-only TEST_ATS=0x105 reads eight bytes from the page-aligned TABLE_BASE and returns the value at 0x3a8; permitted only before publication or after retirement, never while attached. It has synchronous completion and uses the existing seq/ACK/query protocol. Failed test translations return DMA_FAULT without publishing an extent. See [ats.md](ats.md) for lifecycle and diagnostic semantics.

Base commands 1–4 and 6–8, COMMIT, USAGE_PUBLISH and the separate query doorbell follow the spec. The two functions have independent staging, seq/ACK and query banks. Port A cannot COMMIT, ENABLE, QUIESCE, DRAIN or RETIRE. Latest command results survive until a later command; duplicate doorbells cannot repeat a side effect. A query never overwrites the command ACK. The model completes sequential commands synchronously, so queries return DONE or UNKNOWN, not PENDING.

Usage is full-snapshot only. Slot 0x2000 is DRAM, 0x2080 CXL. Port A uses USAGE_SELECT to choose the slot to publish. Port B READ_USAGE snapshots the latest report into an immutable shadow for that tier. Old report sequence is rejected; equal sequence is idempotent without replacement. The provider validates session, epoch, tier, reserved bytes, exact capacity and counter identities again. A sequence gap invalidates the mirror until a subsequent consecutive full snapshot; unchanged reports never refresh their receive timestamp.

## Model-only platform integration

The specification intentionally leaves attach, cache policy and prepare reachability to K0. This profile freezes those mechanics under a separate `MODEL@0x2f0=0x5a424d01` identifier. Drivers refuse a device without this identifier. These registers/opcodes are not standard CXL, not new base CAPS bits, and not a claim about any FPGA IP.

| Register | Purpose |
| --- | --- |
| 0x300 / 0x308 | Port A kernel's random session low/high staging |
| 0x310 | Port A validated DAX capacity staging |
| 0x318 | RO attached indication |
| 0x320 | RO provider's consumer-revoke request |
| 0x330 / 0x338 | Port A zero-range DPA and length staging |
| 0x340 / 0x348 | RO committed DRAM/CXL tier capacity |
| 0x350 / 0x358 | Port A local RO linked Type 3 HDM base/size, single-way profile |

| CONTROL | Owner | Semantics |
| --- | --- | --- |
| 0x100 ATTACH | A | Validate committed epoch, capacity and a nonzero fresh session; only one session per export |
| 0x101 DETACH | A | Kernel's proof that managed mappings were revoked; does not itself retire backing |
| 0x102 TEST_DMA | B | Fetch/validate staging table while offline, read/invert/write **every payload byte** without committing |
| 0x103 ZERO_RANGE | A | Zero a checked DPA range through committed DMA extents before admitting a new lease owner |
| 0x104 REQUEST_REVOKE | B | Request consumer cleanup while ingress remains available for ordered completion |

All model lifecycle operations use the same seq/epoch/ACK/query protocol. Zero-range cannot create an extent or address outside the committed export. Every DMA operation is synchronous under Host B's QEMU BQL; a barrier/drain therefore follows completion of all previously admitted DMA. The Unix protocol is fixed LE 96-byte SEQPACKET records, bounded by a socket timeout. It carries DPA requests, not Host A pages or raw Host B pointers. Disconnect/reset/HDM remap with an established connection faults the instance. Migration is disabled.

For fault injection, `x-drop-ack-op=512` drops the completion notification for one COMMIT after recording its result; `x-query-unknown=on` makes that result unprovable through the query bank. Both are test properties.

## sysfs

Provider: `/sys/class/rpu/rpu0/`; consumer: `/sys/class/rpu/consumer0/`. Root files: state, epoch, stats_seq, allocatable, usage_valid, usage_age_ns. Both `dram/` and `cxl/` expose the spec's fields, plus requested_bytes, user_pinned_bytes and quarantined_backing_bytes. Unknown fields print `unknown\n`, never a fabricated zero. Show callbacks only format cached kernel state and the receiver's current monotonic age; they issue no MMIO or DMA. Use GET_STATS for an atomic multi-field snapshot.

## 可配置容量扩展

模型只读寄存器 `ZB_CAPACITY`（`0x0360`，split64 little endian）返回启动时配置的完整 aperture，即使 COMMIT 前 exported 仍为零。provider RESERVE、consumer DAX claim 和 CAPS.max_bytes 均使用这一值。它是 QEMU 平台扩展，不占用标准 CXL 寄存器。

驱动接受 256 MiB 对齐、256 MiB–64 GiB 的池，启动器要求二次幂；已实测 256 MiB / 8 GiB。每个 lease 最大 4 GiB，超过返回 E2BIG 且不修改账本；mmap 的 `handle << 32` 编码保持兼容。多个 lease 可覆盖大于 4 GiB 的池。SG 构建和 DMA 请求均限制每段 2 MiB，描述符仍最多 65536 个；碎片导致超限时安全失败。DMA backing 页仍由 Rust 内核驱动分配并逐页检查来源。
