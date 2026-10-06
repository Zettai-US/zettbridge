# ATS 与回收边界验证（2026-10-06）

本轮修复覆盖 Rust OOT 回收竞态、ATS 不支持配置、复位、连接中断及诊断有效性。目标内核仍为 `~/linux`；测试均使用新建的独立 QEMU 实例，未重启常驻的 `artifacts/live-8g-ats`。

## 修复与回归证据

| 边界 | 原行为 | 修复后的行为 |
| --- | --- | --- |
| 正在撤销 VMA 时，另一个 ACTIVE lease 重新 fault 后 RELEASE | worker 也回收新加入 RELEASING 的 lease，旧 PFN 可能仍存在 | 加锁时标记本批 lease；只在本批 zap + barrier 后完成它们，晚到者等待下一批 |
| 局部 zap 期间收到全局 REQUEST_REVOKE | 第二次 tick 直接 DETACH，没有证明后来冻结的全部 VMA 已撤销 | 记录 zap 前是否已冻结；新收到全局请求时再执行一轮全局 zap 才 DETACH |
| 故障持续存在 | 每次检测重置 revoke_pending 和统计序号，可能不断 zap | `fault()` 幂等；同一故障不重启已完成的撤销 |
| `device-iotlb=off` 或不支持的 snoop-control 下启用 ATS | VT-d notifier 使用 `error_fatal`，guest 写配置可终止 QEMU | PCI notifier 回调传递错误；设备进入 FAULT，QEMU 继续运行；失败不标记 notifier live |
| 发布前复位 | 旧命令序号留存，新驱动从 1 开始的命令被忽略 | 未发布且未故障时清空寄存器 bank 和状态；发布后的复位仍永久 FAULT |
| COMMIT 后、ATTACH 前 peer 消失 | provider 未进入 FAULT | 已有 export epoch 时断连也 FAULT 并清空 ATC；保留 ATS 失效能力及 backing 隔离 |
| provider 消失后 consumer 读取 ATS | MMIO 全 1 被解释为 enabled=1 | ATS 字段显示 `unknown` |

对上一提交的 consumer/recovery 策略运行新确定性测试，三项均失败；测试仅将新 `Option<bool>` 参数适配到旧 bool，未改变旧策略。修复后均通过。QEMU 的不支持配置、发布前复位、ATTACH 前断连也先复现失败再修复。日志为 `artifacts/edge-revoke-before.log`、`artifacts/edge-before.log`。

## 已执行

| 测试 | 结果 | 本地证据 |
| --- | --- | --- |
| Rust 核心 | 9/9，包括晚到释放、隔离不可复用、lease 表满和 handle 耗尽 | `artifacts/edge-rust-tests.log` |
| 生产 consumer/recovery 策略的模拟 MMIO 测试 | 3/3；准确插入 zap 与完成之间的 RELEASE / 全局 revoke；barrier 失败不完成事务 | 同上 |
| QEMU 新增边界场景 | 13/13；无 IOMMU、device-IOTLB/snoop/STU、ATC 索引碰撞、MMIO 翻译目标、发布前后复位/断连、错误 usage、地址溢出、序号回绕、超长报文 | `artifacts/edge-qtest.log` |
| 原 ATS / 管理 ABI 回归 | ATS 5/5、管理 ABI 通过 | `artifacts/edge-ats-regression.log`、`edge-abi-regression.log` |
| 真实 mmap 并发回收 | 4 线程 × 32 轮；保留旧 VMA、完成 RELEASE、重新 ALLOC，再由 fork 子进程验证旧 VMA 必须 SIGBUS；无残留 lease | `artifacts/edge-pairs/recovery/mapped-stress.log` |
| 用量过期与恢复 | 暂停 consumer 3 秒；provider 保持 ACTIVE/ATS=7，usage_valid=0、allocated=unknown、last-known 标志置位；恢复后新报告有效 | `artifacts/edge-pairs/recovery/edge-results.json` |
| consumer 丢失 | provider FAULT，256 MiB backing 隔离，ATS status=7、transitions=1 | `artifacts/edge-pairs/consumer-loss/edge-results.json` |
| provider 丢失 | consumer FAULT，128 MiB lease 隔离，ATS 诊断 unknown | `artifacts/edge-pairs/provider-loss/edge-results.json` |
| 8 GiB 正常回归 | 全范围 DMA、跨 4 GiB aperture、VMA split、fork 撤销、最终回收通过；回收后 ATC=0、ATS=7、transitions=1 | `artifacts/edge-8g-regression/` |
| ATS 关闭基线 | 双 Linux guest 正常链路通过 | `artifacts/edge-ats-off/` |
| 构建与检查 | Rust Kbuild、静态 zbctl、QEMU x86_64 构建通过；QEMU checkpatch 0 errors，2 项提示（保留已有 GPL-2.0-only 许可、无 maintainer 变更）；guest 日志无 BUG/WARNING/Oops/panic | `artifacts/edge-*-build.log`、`edge-qemu-checkpatch.log` |
| 原常驻 8 GiB pair | 两端仍 running；allocated=128 MiB、available=8064 MiB，ATS=7、transitions=1、faults=0、usage_valid=1 | `artifacts/edge-existing-live.json` |

每个 Linux 测试目录的 `run.json` 记录实际使用的 QEMU、模块、内核 SHA-256 和启动命令。故障测试结束时退出它们创建的 VM；丢弃测试 VM 不能当作真实硬件完成安全回收的证明。常驻 pair 仍使用更新前二进制，本轮没有在线替换它的驱动或模型。

## 重跑

```sh
cd ~/zettbridge
make KDIR=~/linux LLVM=1 -j8
make tools test
ninja -C ~/CXLMemSim/lib/qemu/build qemu-system-x86_64 -j8
python3 ~/CXLMemSim/lib/qemu/tests/qtest/zettbridge-edge-test.py \
    ~/CXLMemSim/lib/qemu/build/qemu-system-x86_64
python3 tools/edge_pair.py --output /tmp/zettbridge-edge-new
python3 tools/run_pair.py --capacity 8G --provider-memory 12G \
    --consumer-memory 4G --output /tmp/zettbridge-edge-8g-new
python3 tools/run_pair.py --ats off --output /tmp/zettbridge-edge-off-new
```

输出目录必须不存在。`edge_pair.py --case recovery|consumer-loss|provider-loss` 可选择单项；脚本仅操作本次创建的目录和 QMP sockets。确定性策略测试验证关键交错顺序，真实 mmap 测试补充实际 PFN/TLB 验证；128 轮压力测试不等于覆盖所有调度。实体 FPGA ATS 时序、异步 DMA、posted writes、AER/热拔插完整矩阵及 KASAN/KCSAN 长测仍未验证。
