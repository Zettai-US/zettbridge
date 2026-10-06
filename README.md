# Zettai CXL Bridge v0.2

按照 `Zettai_CXL_Bridge_Rust_Kernel_Spec_v0.2.docx` 实现的 Rust 内核 provider / consumer，以及配套的双 QEMU 功能模型。驱动通过 Kbuild 作为 `zettbridge.ko` 独立构建，不需要把驱动加入内核源码树。目标内核固定为 `~/linux`，实际 SHA、配置摘要、工具链及部署参数见 [docs/deployment.json](docs/deployment.json)。

Host A 的应用使用 lease FD 对标准 CXL region 执行普通 CPU load/store。QEMU 的标准 HDM decoder 把 HPA_A 转为 DPA_A，`zettbridge` extent engine 将请求送到 Host B 的 PCI requester，通过 **Host B 的 Intel IOMMU / ATS** 访问 Rust provider 拥有的页。ATS 开启时使用设备翻译缓存及 translated DMA；关闭基线使用 `pci_dma_read/write`。两台 VM 的 RAM 相互独立；没有共享 payload 文件，没有用户态内存所有权守护进程。

```
Host A guest                         Host B guest
app mmap(/dev/rpu_consumer0)          /dev/rpu_provider0
  Rust lease ledger                    Rust ExportManager
  exclusive DAX + UC mappings           owned pages + DMA API
          |                                      ^
standard CXL Type 3 / HDM               Port B PCI requester / vIOMMU
          |                                      |
          +-- DPA -> extent -> DMA_B ------------+
              QEMU-to-QEMU Unix SEQPACKET
```

驱动仓库：[Zettai-US/zettbridge](https://github.com/Zettai-US/zettbridge)。配套 QEMU 位于 [Zettai-US/qemu-cxl-type2](https://github.com/Zettai-US/qemu-cxl-type2/tree/codex/zettbridge-rust-v0.2)，本次源码提交为 `6930204cef056c72c99566c2bd91a279dc50a076`。`CXLMemSim` 的配套分支会固定该子模块版本；首次拉取后执行 `git submodule update --init lib/qemu`。

## 构建与一键验证

```sh
cd ~/zettbridge
make KDIR=~/linux LLVM=1
make tools test
ninja -C ~/CXLMemSim/lib/qemu/build qemu-system-x86_64 -j 12
python3 tools/run_pair.py --output /tmp/zettbridge-normal
```

脚本构建一次性 initramfs，依次启动 Host B 和 Host A 两个 QEMU。默认每台分配 2 GiB RAM / 2 vCPU，使用 `~/linux/arch/x86/boot/bzImage`。测试模式只清理自己启动的进程，不修改磁盘镜像、不在宿主机加载模块。输出目录需要是新的目录；成功后打印 `PAIR PASS`，保留两端 serial log、initramfs 和准确启动命令 `run.json`。

扩大内存并保持两台 VM 运行：

```sh
python3 tools/run_pair.py --capacity 8G --provider-memory 12G \
  --consumer-memory 4G --keep-running --output /tmp/zettbridge-8g
python3 tools/guest_console.py /tmp/zettbridge-8g consumer --command 'zbctl consumer-stats'
python3 tools/guest_console.py /tmp/zettbridge-8g provider --command 'zbctl provider-stats'
# 交互串口，Ctrl-] 断开；不需要 SSH 或网络端口。
python3 tools/guest_console.py /tmp/zettbridge-8g consumer
# 先 RELEASE → QUIESCE / DRAIN / RETIRE / RECLAIM，再退出两个 QEMU。
python3 tools/stop_pair.py /tmp/zettbridge-8g
```

`--capacity` 同时设置 provider/consumer 管理设备、Type 3 容量、CFMWS 和 guest 创建的 CXL region。支持 256M 至 64G 的二次幂配置；目前实际验证 256 MiB 和 8 GiB，其他容量仍需实测。provider RAM 至少比池大 1 GiB，默认按池的 1.5 倍、最低 2 GiB 自动选择。大容量 RPC 超时为 30 秒。可用 `--timeout` 设置整体启动/测试期限（默认 600 秒）。

`--keep-running` 在启动后完成整个 aperture 的分窗采样读写，再持有一个 128 MiB lease，每 5 秒读写首尾并更新日志；其余容量可分配。**CXL managed pool 不加入普通 System RAM**，`free` 显示 guest 本地 RAM，池容量/用量应通过 `zbctl` 或 `/sys/class/rpu/` 查看。每个 lease 最大 4 GiB，较大的池可通过多个 lease 完整使用；这一限制保持现有 `handle << 32` mmap ABI 的偏移隔离。

持久模式打印 `PAIR RUNNING` 后启动脚本退出，QEMU 保持运行；`run.json` 和 `.pid` 文件记录 PID，`.console` / `.qmp` 为本地 Unix socket。`.log` 是 guest 串口，`.qemu.log` 是 QEMU 自身日志。没有 `--keep-running` 时仍执行完整回收测试并关闭 VM。

依赖：已构建且带 Rust metadata 的目标内核、C/Clang/Rust/bindgen 工具链、BusyBox static、`cxl` 工具、cpio、Python 3 和 KVM。`--accel tcg` 可用于没有 KVM 的环境，但性能与 KVM 测试结果不同。`--kernel`、`--qemu` 可以覆盖路径。

故障测试及无 guest 的硬件 ABI 回归：

```sh
python3 tools/run_pair.py --drop-commit-ack --output /tmp/zettbridge-ack
python3 tools/run_pair.py --drop-commit-ack --query-unknown --output /tmp/zettbridge-quarantine
python3 ~/CXLMemSim/lib/qemu/tests/qtest/zettbridge-test.py \
  ~/CXLMemSim/lib/qemu/build/qemu-system-x86_64
```

ACK 丢失但查询为 DONE 时继续正常事务；ACK 与查询都不能证明结果时，测试要求返回 `ETIMEDOUT`、state=faulted、保留 256 MiB backing 和 DMA mapping，并拒绝 RECLAIM。

## guest 使用顺序

1. 两端加载 `zettbridge.ko`；provider 要求 translated IOMMU、64-bit coherent DMA。启动脚本默认 ATS 持续开启，Rust 核对 Linux IOMMU 已启用 ATS；`--ats off` 可复现原基线。
2. Host B 保持同一个 `/dev/rpu_provider0` FD，执行 RESERVE → PREPARE → COMMIT。PREPARE 遍历全部 backing 做 CPU 写 pattern → DMA 读取/反相写回 → CPU 校验，再清零并交还 DMA ownership。
3. Host A 使用标准 `cxl create-region -d decoder0.0 -m mem0 -s 256M -t ram -w 1`。本测试不加载 `device_dax` 或 `dax_kmem`。
4. `echo dax0.0 > /sys/bus/dax/drivers/zettbridge/new_id` 将该 DAX device 独占交给 consumer。驱动核对它的 HPA 范围等于关联 Type 3 的已提交 HDM aperture，再建立随机 128-bit session。Host B 收到 attach 后 ENABLE。
5. 应用从 `/dev/rpu_consumer0` ALLOC_LEASE，然后以 **`object_handle << 32` 为文件偏移** mmap 返回的对齐容量。所有访问都限制在所属 lease 内；VMA 分裂后的页偏移仍在同一个 handle 域内。
6. RELEASE 返回 pending。worker 先阻止新 fault，撤销所有相关 VMA（包含 fork / dup 引用），再 LEASE_BARRIER；完成前区间不可重用。重新分配时通过设备清零旧内容。
7. Host B QUIESCE 请求消费者冻结并撤销全部访问；消费者 DETACH 正向完成后，Host B 执行 QUIESCE → DRAIN → RETIRE。只有 state=reclaimable 时 RECLAIM 才允许 unmap/free。

UAPI、寄存器和模型扩展见 [docs/abi.md](docs/abi.md)。`tools/zbctl.c` 是持有 FD 的完整 C 使用示例，包含权限、幂等、copyout 故障、并发快照和 fork/revoke 验证。

## Rust OOT 实现

PCI probe / unbind、设备上下文、ioctl / compat / release、mmap fault、工作队列、NUMA backing、SG DMA、coherent extent table、DAX 接管策略和 sysfs 统计均由 Rust 实现。使用目标内核的 `pci::Driver` / `pci::Adapter`、`MiscDevice`、`Arc`、`Mutex`、`DelayedWork`、`Completion`、`UserSlice`、`CoherentAllocation` 和 SG iterator。

`kernel_shim.c` 仅保留目标内核尚未导出的宏/inline 包装、私有 DAX bus 描述符、AER/PM hook 转接和 sysfs 属性声明。它没有设备上下文、file_operations、工作任务、页数组、分配循环或释放策略。Rust 文件操作注册适配器为原生 MiscDevice vtable 补上此内核 API 尚缺少的 `owner` / `parent` / `mode`，确保 PCI 解绑后仍打开的 FD 正确保活模块。无需修改 `~/linux`。

| 文件 | 职责 |
| --- | --- |
| `driver.rs`, `provider.rs` | 模块入口、状态与事务去重、发布与回收 |
| `device.rs` | 原生 Rust PCI、MiscDevice 文件操作、Arc/Mutex、延迟任务、初始化失败和解绑清理 |
| `backing.rs` | Rust 页所有权、严格 NUMA/来源检查、SG 映射、coherent table、显式退役后释放 |
| `ffi.rs` | 原始指针边界、Rust VMA fault / DAX 回调、注册适配、隔离角色保留 |
| `kernel_shim.c` | 无状态内核宏/inline helper、私有 DAX ABI 和声明 |
| `consumer.rs`, `accounting.rs` | lease 分配账本、session、完整用量报告 |
| `transport.rs`, `recovery.rs` | 有界 ACK/query、故障隔离、模块 pin |
| `sysfs.rs`, `uapi.rs`, `include/` | 一致统计、Rust sysfs 格式化、固定布局 ABI |
| `../CXLMemSim/lib/qemu/hw/cxl/zettbridge.c` | 两个管理 bank、表校验、DMA requester、中继及功能性 barrier |
| `../CXLMemSim/lib/qemu/hw/mem/cxl_type3.c` | 可选 `zettbridge=` 连接，标准 CXL discovery / HDM 路径 |

额外验证 Rust 设备/模块生命周期：

```sh
python3 tools/run_pair.py --lifetime --output /tmp/zettbridge-lifetime
```

两端分别验证：打开 FD 阻止卸载 → 保持 FD 时解绑 PCI → 旧 FD 返回 ENODEV 且仍阻止卸载 → 最后 close 后可卸载 → 重新加载并在任务排队时卸载。此测试不发布 backing、不接管 DAX；活动/隔离资源仍禁止卸载。

## 当前交付范围

ATS 持续开启扩展、翻译缓存/失效语义和 sysfs 诊断见 [docs/ats.md](docs/ats.md)。启动脚本默认 `--ats on`；用量仍每约 200 ms 上报，ATS 在分配、读写和最终回收期间保持开启。

ATS、复位、断连、用量过期以及并发 VMA 回收的边界修复、复现命令和结果见 [docs/edge-cases.md](docs/edge-cases.md)。`make test` 包含生产 consumer/recovery 策略的确定性竞态测试；`python3 tools/edge_pair.py --output /tmp/zettbridge-edge-new` 创建专用双 VM 完成真实 mmap 和故障隔离验证。

已验证的是 **x86-64、256 MiB / 8 GiB 静态 volatile aperture、单路 HDM、Host B DRAM、UC managed pool 的双 QEMU 功能实现**。Rust 与 QEMU 均实际编译，两个 guest 实际启动。验证记录见 [docs/validation.md](docs/validation.md)，原始日志在 `artifacts/`。

该模型不模拟 FPGA credit、链路延迟、dirty WB cache、真实 PCIe posted-write 队列或 RTL。QEMU 同步 DMA 完成是本 profile 的 drain 证据，不能据此宣布真实 FPGA 的 K0–K6 验收完成。应用映射固定 UC；不允许 devdax/普通 malloc 用量冒充 lease 用量。CXL System RAM tier 具有严格 NUMA/资源来源检查和编码路径，但没有在此次双 VM 测试中作为 Host B backing 验证；不能视作已通过 K2。

当前版本使用有界轮询和 unbound workqueue，未实现 MSI-X 管理中断。单角色每 guest 一个实例，最多 128 个同时占用的 lease，256 条未关闭 owner 的事务记录；不会通过驱逐未决记录破坏重试语义。未知命令返回 ENOTTY，超限失败且不改账本。设备移除、AER、reset、PM 或歧义完成一律 fail closed 并隔离，不做未经证明的自动恢复。消费者 DAX ownership / module pin 保持到 guest 关闭；不要期待活动或隔离实例可以 `rmmod`。这项限制防止重新绑定通用 DAX 驱动绕过旧映射隔离。

虚拟机使用 `-cpu host,hypervisor=off`：目标内核的 `cpu_cache_has_invalidate_memregion()` 对 CPUID hypervisor 标志返回 false，标准 CXL region commit 因而被拒绝。此参数只属于 QEMU 测试平台；本驱动的应用映射仍是 UC，未修改 `~/linux` 的缓存失效实现或配置。
