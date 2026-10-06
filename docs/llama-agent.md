# llama.cpp agent：推理期间 KV 直接驻留共享池

`tools/run_llama_agent.py` 启动两个独立 Linux/QEMU，运行真实的 Qwen2.5-0.5B-Instruct Q8_0 模型。Host A 的 llama.cpp 通过 Rust OOT consumer 的 lease mmap 保存全部持久 K/V tensor；Host B 的 Rust provider 提供 backing，并经持续开启的 ATS 执行数据访问和中继用量。

模型先执行本地内存参考路径，再执行共享 KV 路径。每条路径均完成三次模型生成、两次工具调用：读取订单 `agent-17`，调用乘法工具计算 `7 × 23`，提交 `161`。JSON grammar 在每轮均允许全部三种工具；工具顺序和参数由模型生成，测试检查其正确性。数量和单价仅在读取订单工具返回后加入对话。

## 直接驻留的含义

- 24 层、每层 K/V 两个 tensor，共 48 个 F16 tensor；256-token context 占用 3,145,728 字节，即 3 MiB。
- `kv_buffer.cpp` 在上下文创建时取得 `/dev/rpu_consumer0` 的 ALLOC lease，以 `handle << 32` 映射。ggml 的 KV 根 tensor 和所有 views 直接使用该地址，贯穿 prefill、逐 token decode、工具等待和后续轮次；上下文销毁时才 munmap/RELEASE，并等待事务完成。
- 每个推理图的 KV 输入地址和每轮结束时的根 tensor/buffer 所有权均检查是否仍处于 lease 范围。模型权重、Q、attention 输出累加器和其余计算临时缓冲使用 Host A 普通内存。
- `attention.cpp` 交换 CPU GQA 循环顺序，同一 KV head 的多个 query 共用当前行。每个工作线程临时读取一行 K 和一行 V，模型对应 **256 字节**；不会保存整层或整个会话的本地 KV 副本，也不会跨 attention 调用保留这些行。所有持久 KV 写回和下一步读取仍经过 CXL/ATS。该工作负载没有 state save/load、checkpoint 或恢复操作。
- QEMU UC MMIO 不适合 AVX 内存操作，构建关闭 AVX/F16C 和自动向量化，使用可模拟的标量访问。首次清零使用 Rust ALLOC 已完成的零初始化，后续 clear 仍实际执行。

这是一套固定版本的独立应用和 ggml buffer/CPU 适配器，使用链接器 wrapping 与 ggml 内部 ABI。llama.cpp 源码 checkout 保持干净，但适配器改变了 KV 分配和 CPU attention 的执行路径；不能直接将其视为任意版本 `llama-server` 的通用插件。上游标量 Flash Attention 作为独立参考，保持相同的浮点舍入次序。

## 构建和重跑

先按仓库 README 构建 `~/linux`、Rust OOT 模块、`zbctl` 和配套 QEMU。需要 Python 3.11+、CMake、Ninja、C/C++ 编译器、静态 libc、e2fsprogs；guest 内核需要内建 virtio-pci、virtio-blk、ext4。

```sh
cd ~/zettbridge
python3 tools/build_llama_agent.py

# 不启动 VM：先检查模型与本地 mmap KV 的 token 和 logits 指纹一致性
artifacts/llama-build/zb-llama-agent \
  artifacts/llama-models/qwen2.5-0.5b-instruct-q8_0.gguf \
  tools/llama_agent/order.json local

# 新建两台 VM；output 必须不存在
python3 tools/run_llama_agent.py \
  --model artifacts/llama-models/qwen2.5-0.5b-instruct-q8_0.gguf \
  --output /tmp/zettbridge-agent-new
```

构建默认使用 `/usr/bin/cmake`，可通过 `--cmake` 覆盖。bootstrap 固定并校验：

| 依赖 | 固定版本 |
| --- | --- |
| [llama.cpp](https://github.com/ggml-org/llama.cpp) | `ad6c66839af3c5646fba8c6c2e2087a1e4e38948` |
| [Qwen2.5-0.5B-Instruct-GGUF](https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF) | `9217f5db79a29953eb74d5343926648285ec7e67` |
| GGUF SHA-256 | `ca59ca7f13d0e15a8cfa77bd17e65d24f6844b554a7b6c12e07a5f89ff76844e` |

模型下载约 676 MB。脚本将模型、静态应用和订单复制进本次新建的只读 ext4 workload disk，仅挂给 consumer。它不承载共享 KV。默认 provider RAM 2 GiB、consumer RAM 4 GiB、共享池 256 MiB；原有 heartbeat 占用 128 MiB，推理期间另增 3 MiB。该脚本只管理自己创建的两台 VM；成功后完成 lease release、provider retire/reclaim，再退出 QEMU。

## 自动验收与证据

`result.json` 的 `status=passed` 和终端的 `LLAMA AGENT E2E PASS` 表示以下条件同时通过：

1. 两条路径各完成三轮模型生成、两次实际工具调用，返回正确答案，token 序列逐个一致。每一步采样前还对完整 FP32 logits 数组计算 64-bit FNV-1a 指纹并比较，不只比较最终答案。
2. `KV_MAPPED remote=true`，三次 `KV_DIRECT_VERIFIED` 中图引用和 attention 行读取计数持续增长，最后 `KV_RELEASED completed=true`。没有 checkpoint 事件。
3. Host B 的有效用量报告观察到准确的额外 KV 大小；ATS status=7、transitions=1、faults=0，翻译请求和命中计数显著增长。报告序号继续前进，用量保持有效。
4. 推理后 consumer 占用回到 128 MiB heartbeat 基线，releasing/quarantined 均为零，然后正常回收整个导出。

输出目录保留 `agent-console.log`、`provider-samples.json`、`result.json`、两端串口日志、`pair/run.json` 和本次只读磁盘。manifest 包含模型、应用、内核、模块、QEMU 的 SHA-256，及适配器源码 hash。观察进度无需连接被测试占用的 consumer 串口，可直接读取 `pair/consumer.log` 和 `provider-samples.json`。

`zb-attention-test` 对 29 个小型图逐字节比较上游参考输出，覆盖单 token/prefill、无 mask/因果 mask/完全 mask、多 mask head、1/2/3 线程、GQA、非整齐行长和 ALiBi 回退。bootstrap 会运行它，也可用：

```sh
/usr/bin/ctest --test-dir artifacts/llama-build --output-on-failure
```

## 已执行结果（2026-10-06）

完整运行位于 `artifacts/llama-agent-accepted/`，`result.json` 为 `passed`，`pair/run.json` 为 `stopped`。应用总耗时 **318.58 秒**，包含模型加载、本地参考、共享推理和工具等待。

| 验证 | 实测结果 |
| --- | --- |
| 三轮共享推理 | `read_order(agent-17)` → `multiply(7,23)` → `final(161)`；38 个生成 token 与每步完整 logits 指纹均一致 |
| KV 驻留 | 同一 handle=3，48 tensors / 3,145,728 bytes；三轮图引用数 2688 / 5568 / 7872，attention 行读取数 64848 / 166368 / 266304 |
| Host B 镜像用量 | 128 MiB → 131 MiB → 128 MiB；额外 3 MiB 与 KV 分配完全一致 |
| 持续用量 / ATS | 287 次有效采样；report 3 → 1411，最大 usage age 310.20 ms；ATS=7、transitions=1、faults=0 |
| 数据通路 | 采样区间 ATS translations 增加 18,289，hits 增加 9,575,825 |
| 释放 / 最终回收 | KV_RELEASED completed=true，releasing/quarantined=0；随后整个池 reserved/mapped/exported/allocated=0，ATC entries=0，ATS 仍为 7，retired=1 |
| attention 数值边界 | Release / Debug 各 29/29，输出逐字节一致；`artifacts/llama-attention-test.log`、`llama-debug-test.log` |
| 第二份订单，本地适配器 | `agent-29` 的 12 × 19 = 228；三轮 token/logits 指纹一致；`artifacts/llama-alternate-local.log` |
| 采集函数回归 | 128 轮并发 mmap、usage 过期/恢复、最终回收通过；`artifacts/llama-collector-regression/recovery/` |

两端串口没有 BUG/WARNING/Oops/panic，两个本次 QEMU 进程均已退出。还修复了串口并发输出造成的两种验收问题：启动标记被带时间戳的 printk 拆开，以及 heartbeat 插入 `printf key` / `cat value` 之间。启动检测保留原始日志做错误检查，重建用户态就绪标记；指标采集先读取值再一次打印整行，GET_STATS 使用独立前缀，数值字段严格校验。

当前测试用于证明真实共享访存、推理正确性和 lease/ATS 生命周期。UC 页上的每次访问会触发模拟链路，运行需要数分钟及以上；这不代表物理 CXL 的吞吐或延迟。尚未验证并发多会话、大上下文、GPU KV、任意模型架构或生产服务器调度。
