# ATS 持续开启（2026-10-06）

双 QEMU 启动脚本默认 `--ats on`。Host B provider 的 PCIe function 广告 ATS，Intel vIOMMU 使用 `device-iotlb=on` 并生成 ATSR。`~/linux` 的 Intel IOMMU 驱动启用 ATS、维护 requester 的设备 TLB 失效记录；Rust OOT 驱动核对内核 ATS 状态、PCI ATS Control 和模型 notifier 状态后才允许分配。驱动不自行调用 `pci_enable_ats()`，以免绕过 IOMMU 的失效管理。目标内核无需补丁。

```sh
make KDIR=~/linux LLVM=1
make tools
ninja -C ~/CXLMemSim/lib/qemu/build qemu-system-x86_64 -j8
python3 tools/run_pair.py --ats on --capacity 8G \
  --provider-memory 12G --consumer-memory 4G --keep-running \
  --output /tmp/zettbridge-8g-ats
python3 tools/guest_console.py /tmp/zettbridge-8g-ats provider \
  --command 'zbctl provider-stats; cat /sys/class/rpu/rpu0/ats_enabled /sys/class/rpu/rpu0/ats_hits /sys/class/rpu/rpu0/usage_valid'
python3 tools/stop_pair.py /tmp/zettbridge-8g-ats
```

`--ats off` 保留 v0.2 的 ATS 关闭基线。QEMU 单独启动时，`zettbridge,provider=on,ats=on` 和 `intel-iommu,device-iotlb=on,caching-mode=on,aw-bits=48` 必须配套；设备属性 `ats` 默认关闭，避免改变已有命令的行为。Host A 的管理 function 不广告 ATS；它通过硬件中继读取 Host B 的 ATS 诊断数据。

## 翻译与生命周期

* 描述符、PREPARE 全范围 DMA、清零和应用读写全部经过同一 ATS 路径。256 项直接映射 ATC 以 4 KiB IOVA 页为键；miss 调用 QEMU `pci_ats_request_translation()`，仅缓存成功的 requester-domain 翻译及读写权限。IOMMU 返回的大页翻译按 4 KiB 缓存。
* 命中后对 IOMMU 返回的目标地址空间执行 translated DMA，避免二次翻译。跨页请求拆分；每页分别检查权限及 Bus Master Enable；translated DMA 只允许访问内存。ATS 配置开启后，关闭 ATS 不会自动降级为普通 DMA。
* 注册 `IOMMU_NOTIFIER_DEVIOTLB_UNMAP`，VT-d 的 queued invalidation 按 requester 和地址范围删除 ATC 条目。IOMMU 自身的 IOTLB invalidation 不被冒充为设备失效完成。设备通知同步返回之后，VT-d 才完成该失效描述符。
* BQL 串行化 ATS 请求、同步 DMA 和失效回调。ACTIVE 期间禁用 ATS 或修改 STU 会使 export fault；重新打开 ATS 不会自动恢复该 export。reset、故障、ATS 禁用都会清空缓存。
* RETIRE 先删除 extent 并清空 ATC，再返回 ACK。ATS 保持开启，Rust 随后通过 DMA API unmap/free，Linux 继续发送设备失效。正常启用到回收的 `ats_transitions` 为 1。
* PREPARE 增加两轮独立 coherent buffer 自测：CPU 写 pattern → ATS miss/read → hit/read → `dma_free_coherent` → 确认设备失效且 ATC 为空，全程保持 ATS。若该测试 ACK 丢失且查询不能确认完成，临时 buffer 移交 Rust backing 持有并一起隔离，禁止释放或 RECLAIM。

此 QEMU 分支的 PCI ATS/PRI helper 曾把 IOMMU 根总线与设备总线的输出参数传反。已修正所有对应调用；ATS 测试把 requester 挂在 Root Port 下，验证 `01:00.0` 的映射及失效路由。

## 持续上报与诊断

Host A 在 ACTIVE 状态下约每 200 ms 发布完整 lease 用量，经自定义 `USAGE_PUBLISH → READ_USAGE` 中继到 Host B。有效期为 2 秒；ATS 和用量报告独立运行。统计口径仍是导出池的 lease 用量，包含 allocated/releasing/quarantined/available，不包含 Host B 其他进程的内存。

两端 `/sys/class/rpu/{rpu0,consumer0}/` 新增以下只读字段，均报告 Host B 的设备数据：

| 字段 | 含义 |
| --- | --- |
| `ats_enabled` | PCI ATS Enable 是否开启 |
| `ats_status` | bit 0 配置开启、bit 1 PCI 开启、bit 2 失效 notifier 注册；正常为 7 |
| `ats_requests` / `ats_hits` | 翻译请求数 / ATC 命中数 |
| `ats_invalidations` | 收到的设备失效通知数，不等于删除条目数 |
| `ats_cached_entries` | 当前有效 ATC 条目，最大 256 |
| `ats_flushes` | 完整缓存清除次数，包括初始化与 RETIRE |
| `ats_faults` | ATS 请求、权限或 translated DMA 失败次数 |
| `ats_transitions` | ATS Control 改变次数；正常部署为 1 |

Rust 每约 100 ms 缓存这些诊断数据；sysfs show 不做 MMIO。这些诊断字段是逐项采样，不构成跨 Host 原子快照；原有 256-byte GET_STATS ABI 不变。

peer 不可达、MMIO 返回全 1 时，ATS 诊断显示 `unknown`，包括 `ats_enabled`。不支持的 IOMMU ATS notifier 配置只使设备进入 FAULT，不终止 QEMU。复位和断连、报告过期以及并发回收的验证见 [edge-cases.md](edge-cases.md)。

## 验证

```sh
python3 ~/CXLMemSim/lib/qemu/tests/qtest/zettbridge-ats-test.py \
  ~/CXLMemSim/lib/qemu/build/qemu-system-x86_64
python3 tools/run_pair.py --ats on --capacity 8G --output /tmp/zb-ats-test
python3 tools/run_pair.py --ats off --output /tmp/zb-noats-test
python3 tools/run_pair.py --drop-ats-test-ack --output /tmp/zb-ats-ack
python3 tools/run_pair.py --drop-ats-test-ack --query-unknown --output /tmp/zb-ats-unknown
```

qtest 使用真实 VT-d 页表及 queued invalidation，覆盖非恒等映射、cache hit、错误 requester/不相交范围、4 KiB/8 KiB 失效、重映射与解除映射、读写权限降级、非连续物理页间跨页请求、Bus Master 关闭、活动时关闭 ATS，以及保持 ATS 的 RETIRE。双 Linux guest 的验证记录见 [validation.md](validation.md)。

实现是同步功能模型；尚未验证物理 FPGA 的 ATS TLP 时序、并发请求、失效队列深度及 posted-write drain。没有启用 PASID、PRI、SVA、缺页或 payload data cache。ATS 与这些功能的关系可参见 [Linux 官方说明](https://www.kernel.org/doc/html/latest/arch/x86/sva.html)；QEMU 的平台配置见 [官方手册](https://www.qemu.org/docs/master/system/qemu-manpage.html)。
