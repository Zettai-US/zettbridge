# 双 QEMU 验证记录（更新于 2026-10-06）

实际使用 `~/linux`：SHA `40368e0702bb2c56932e9d91e617a33d76f1c267`，`7.0.0-mk3+`，Rust 1.93.1 / Clang 21.1.8 / bindgen 0.72.1。内核本来有本地修改，已在 deployment.json 保留 dirty 清单；本任务没有修改目标内核。QEMU 来源 `~/CXLMemSim/lib/qemu`，版本 10.0.94。

原始日志、initramfs、模块二进制和运行时 PID/socket 保存在执行机器的 `artifacts/`，不随源码提交。本文记录已执行的结果；复现命令见 README，各次启动还会生成完整命令及二进制哈希的 `run.json`。

最新边界测试和竞态修复见 [edge-cases.md](edge-cases.md)：12 项 Rust 测试、13 项新 qtest、4 线程 128 轮真实 mmap 回收、用量过期/恢复、两端断连隔离、8 GiB 及 ATS-off 回归均已通过。常驻 8 GiB pair 保持运行，未在线替换其二进制。

## ATS 持续开启（2026-10-06）

在下述 ATS 关闭基线上新增真实 ATS 请求、256 项设备翻译缓存、VT-d queued device invalidation、权限检查、RETIRE 清缓存和 Rust 状态监测。未修改目标内核。复现及运行接口见 [ats.md](ats.md)。

| 检查 | 结果 / 证据 |
| --- | --- |
| Rust OOT / QEMU 构建 | 通过；`artifacts/ats-{driver,qemu}-build.log` |
| Rust 核心与工具 / 固定 UAPI | 6 项核心检查通过，工具 `-Werror` 构建通过，32/64 位 UAPI 编译断言通过 |
| QEMU ATS 定向测试 | 5 组通过：Root Port requester、非恒等地址翻译、cache hit、正确 SID 和范围的失效、remap/unmap、只读权限及权限降级、跨非连续物理页写入、Bus Master 关闭、ATS 中途关闭及正常 RETIRE；`artifacts/ats-qtest.log` |
| 原管理 ABI | 通过；`artifacts/ats-baseline-qtest.log` |
| 256 MiB ATS 开启 / 关闭 | 两个 Linux guest 均实际完成完整流程；`artifacts/ats-second/`、`artifacts/ats-off-regression/` |
| 8 GiB ATS 开启 | 全范围 CPU→ATS DMA→CPU 校验及 8 个 1 GiB lease 跨窗采样、账本/释放/用量/最终回收通过；`artifacts/ats-8g-test/` |
| 8 GiB 回收时 ATS 计数 | status=7、requests=6317712、hits=57266、invalidations=4、cached_entries=0、transitions=1、faults=0；保持 ATS 直到 unmap/free 完成 |
| 临时 ATS 测试缓冲区 | 两轮真实 Linux coherent DMA map/read/cache hit/free/invalidation，通过后打印 `ATS_MAP_UNMAP_PASS` |
| COMMIT ACK 丢失及 UNKNOWN | DONE 时恢复并完成全流程；UNKNOWN 时拒绝 RECLAIM 并保留 backing；`artifacts/ats-ack/`、`artifacts/ats-quarantine/` |
| ATS 自测 ACK 丢失及 UNKNOWN | DONE 恢复；UNKNOWN 时保留尚有缓存的临时 DMA buffer，未发生 unmap/invalidation，RECLAIM 被拒绝；`artifacts/ats-probe-ack/`、`artifacts/ats-probe-quarantine/` |
| PCI/FD/模块生命周期 | 两端 unbind、旧 FD ENODEV、close 后卸载及重新加载通过；`artifacts/ats-lifetime/` |
| QEMU patch 检查 | `checkpatch --no-tree --no-signoff`：0 errors、0 warnings；`artifacts/ats-checkpatch.log` |
| 持续实例 | `artifacts/live-8g-ats/`，provider RAM=12 GiB、consumer RAM=4 GiB、pool=8 GiB、heartbeat lease=128 MiB；仍留有 8064 MiB 可分配 |
| 61 秒持续采样 | 13 次查询，report_seq 275→549，ATS hits 8134→8182，每次均 status=7、enabled=1、transitions=1、faults=0、usage_valid=1 且 age≤2 秒；`artifacts/live-8g-ats/ats-monitor.json` |

ATS 版本的双 guest 正常及故障回归未出现 BUG / WARNING / Oops / kernel panic。qtest 中明确注入的无映射/权限错误是预期失败。现有旧的 ATS 关闭实例 `live-8g/` 在新版就绪后正常 release/drain/retire/reclaim 并退出；当前运行实例为 `live-8g-ats/`。

以下保留原 ATS 关闭版本的历史验证记录；其中的“基线”与路径不代表当前 ATS 运行模式。

## 8 GiB 扩容与持续运行（2026-10-05）

当前模块已增加可配置 aperture，并重新用同一个 `~/linux` 内核验证。此前下表中的四组 `rust-final-*` 是重构基线记录；当前扩容版本的实测证据如下：

| 检查 | 结果 / 证据 |
| --- | --- |
| 扩容版驱动 / QEMU 构建 | 通过；`artifacts/memory-capacity-{driver,qemu}-build.log` |
| Rust 核心检查 | 6 项通过；新增 8 GiB 账本、两段 4 GiB lease、超限拒绝不改账本；`artifacts/memory-capacity-core-tests.log` |
| 模型 ABI 回归 | 通过；增加只读容量寄存器检查；`artifacts/memory-capacity-qtest.log` |
| 8 GiB 全部 backing | CPU 写入 → DMA 读取/反相写回 → CPU 全字节校验通过；`artifacts/memory8g-test/provider.log` |
| 8 GiB aperture | 8 个 1 GiB lease 同时映射，在每个 MiB 边界及每窗末尾写入地址相关 pattern，最后统一回读以检测跨窗别名；验证范围包含 DPA=8589934591；`artifacts/memory8g-test/consumer.log` |
| 8 GiB 原有功能回归 | 并发账本、copyout 重试、fork revoke、SG 边界、VMA split、清零复用、usage 中继、最终 retire/reclaim 全部通过；`PAIR PASS` |
| 持久模式控制 | 256 MiB 临时实例成功后台运行、串口查询、正常 RELEASE → QUIESCE/DRAIN/RETIRE/RECLAIM → QMP quit；`artifacts/live-control-test/` 和 `artifacts/live-control-stop.log` |
| 正式持续实例 | `artifacts/live-8g/`：provider RAM=12 GiB，consumer RAM=4 GiB，CXL managed pool=8 GiB，保留 128 MiB heartbeat lease；`run.json` 记录 PID、命令和二进制 SHA-256；当前状态快照为 `provider-status.txt` / `consumer-status.txt` |

本次大容量测试没有出现 BUG / WARNING / Oops / kernel panic。QEMU 同步的大范围 DMA 导致启动期间出现 clocksource long-readout 提示，测试未将该耗时解释为物理 CXL 性能。池容量不计入 consumer 的普通 System RAM；当前额外可分配 8064 MiB。

## 已执行

| 检查 | 结果 / 证据 |
| --- | --- |
| 原生 Rust OOT Kbuild / MODPOST / .ko | 通过，无模块编译警告；`artifacts/rust-driver-build.log`；C 仅保留无状态 helper |
| 实际 QEMU x86_64 设备构建 | 通过，`artifacts/qemu-build.log` |
| Rust 核心测试 | 5 项通过：黄金 extent、溢出、账本恒等式、隔离、RELEASING/QUARANTINED 不复用、碎片与对齐；`artifacts/rust-core-tests.log` |
| C 64/32 位 UAPI 编译断言 | `cc -m64` / `cc -m32` 均通过；验证 struct 大小、关键 offset 和 ioctl number 相同（仅编译兼容性，未声称执行 32 位应用） |
| 双 qtest QEMU 管理 ABI | 通过；无洞/权限/保留位/溢出校验、两端权限、独立 ACK/query、稳定 inventory/usage shadow、重复/旧报告、attach/barrier/drain/retire；`artifacts/qtest.log` |
| 两个 Linux guest 正常链路 | 通过；`artifacts/rust-final-normal/{provider,consumer}.log` |
| COMMIT ACK 丢失 / query DONE | 通过；完整双 guest 流程仍完成，`artifacts/rust-final-ack/` |
| COMMIT ACK 丢失 / query UNKNOWN | 通过；256 MiB reserved / DMA mapped / quarantined backing 保留，RECLAIM 返回 EBUSY，allocatable=0，exported valid bit 清除；`artifacts/rust-final-quarantine/` |
| Rust FD / PCI / module 生命周期 | 两端通过；打开 FD 阻止卸载、带 FD 解绑后 ENODEV、最后 close 后卸载、重新加载及排队任务卸载；`artifacts/rust-final-lifetime/` |
| QEMU checkpatch | 0 errors；保留 1 个 GPL-2.0-only 许可偏好提示。与驱动/ABI 同用 GPL v2-only；`artifacts/qemu-checkpatch.log` |

正常 guest 流程的实际覆盖：

* Intel vIOMMU translated domain、strict invalidation，设备无 ATS；256 MiB DRAM 全范围 CPU→DMA→CPU pattern 校验。管理缓冲不计入 payload。
* 标准 CXL 枚举、标准 cxl create-region、真实 DAX device 独占绑定；HPA aperture 与关联 Type 3 HDM 范围匹配。
* 8193-byte 请求占用 12288 bytes。真实 CPU mmap 逐字节写入/读回，之后检查 3 MiB lease 的多个页及 2 MiB DMA segment 边界。
* 四线程各 8 轮 allocate / snapshot / release；所有快照保持 `allocated+releasing+quarantined+available=capacity` 和 `requested<=allocated`。
* 向只读用户结构 copyout 故意失败；重复相同 request_id 只返回原 lease，QUERY_TXN 可恢复 handle。未知 flags、跨 FD owner 的 handle 被拒绝。
* fork 继承已有映射，父进程 RELEASE；barrier 后子进程访问旧地址收到 SIGBUS。释放后的区间再次分配时旧内容已清零。mprotect 拆分 VMA 后 offset/handle 边界正确。
* Host B 得到有效完整 usage snapshot，最后 consumer revoke / DETACH → QUIESCE / DRAIN / RETIRE → RECLAIM，reserved / mapped / exported 归零。

最终四组测试均使用同一个 Rust OOT `.ko`，各目录 `run.json` 记录模块、QEMU 和内核镜像的 SHA-256。日志未出现 BUG / WARNING / Oops / panic。模块签名未强制校验，测试内核预期记录 unsigned / out-of-tree taint。

成功日志标记包括 `DMA_FULL_RANGE_PASS`、`CONCURRENT_LEDGER_PASS`、`CXL_DATA_PASS`、`SG_BOUNDARY_VMA_SPLIT_PASS`、`REVOKE_FORK_ZERO_PASS`、两端 `ZB_GUEST_PASS`。故障测试为 `QUARANTINE_PASS`；生命周期测试为 `FD_UNBIND_UNLOAD_PASS` 和 `RELOAD_WORKER_UNLOAD_PASS`。

## 验收边界

| 条件 | 本次覆盖 | 尚不能据此声明通过的部分 |
| --- | --- | --- |
| K0 | 固定现有内核/工具链，实际 probe/sysfs/MMIO/DMA/DAX 骨架 | 实体板卡、FPGA IP manifest 及 Rust 接口的长期兼容 |
| K1 | ABI/账本核心测试、qtest、guest 并发和事务重试 | 全面的目标 KUnit、运行时 32 位 compat、所有分配故障点 |
| K2 | IOMMU 开 / ATS 关、DRAM 全范围 DMA、跨段和 byte writes | Host B 的 CXL System RAM backing、非 coherent 平台、物理 DMA 限制 |
| K3 | 独占 DAX、lease mmap/revoke、fork、VMA split、用量中继 | 长时间高负载、多个池/多个 consumer、WB 映射 |
| K4 | ACK/query/copyout 故障、软件 revoke、功能模型 drain | 实际 late dirty-cache writeback、posted PCIe writes、FPGA credit/ordering |
| K5 | fail-closed、不可证明结果的 quarantine、空实例带 FD 解绑/卸载 | 全套实体 AER/link/PM/reset、设备热移除 race、KASAN/KCSAN/lockdep 长测 |
| K6 | 两 VM 枚举、容量、普通 load/store、错误路径和完整回收 | 物理链路吞吐、P99/尾延迟、目标 IP 的硬件服务期限 |

虚拟机的每个 cache-line 操作都经过 QEMU IPC，不能将本测试耗时作为 CXL 硬件吞吐/尾延迟。正常测试已显式停止自己的两台 QEMU；如需交互式持续运行，可以取对应 `run.json` 的命令并替换 initramfs 的测试 init。

初始 bring-up 曾发现：KVM IRQ remapping 需要 `kernel-irqchip=split`；该内核拒绝在 CPUID hypervisor 标志下提交 CXL region；CXL 创建的 DAX device 默认是 kmem 类型，需要明确绑定本驱动的 `new_id`。最终脚本已处理这些条件。首次带 hypervisor 标志的启动还出现过一次 CPU1 instruction-fetch/栈异常，未将其根因归于驱动或内核；最终 profile 的正常、ACK 恢复和 quarantine 测试中未重现。测试结论仅针对 manifest 固定的最终 profile。
