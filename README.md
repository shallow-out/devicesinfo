# devicesinfo

设备信息与能力探测。当前含 `deviceinfo` 模块：**CPU / 内存 / 加速器**。

目的只有一个：**在分配任务之前，准确回答这台设备能做什么。**
上层（模型分发、任务调度、面板）靠这份报告决定"这个模型该不该装、该在哪跑"，
所以**报错比不报更糟**——一个乐观的数字会让人装上一个跑不动的模型，
一个悲观的数字会让本机永远拿不到该干的活。

## 三条原则

1. **优先 sysfs，`/proc/cpuinfo` 只当兜底。** cpuinfo 是给人看的文本，字段随架构变化
   （ARM64 没有 `physical id`/`core id`）；`sys/devices/system/cpu/*/topology/` 是内核主动
   提供的稳定接口，x86 和 ARM 都有。
2. **不猜。** 算不出来就留 `None`，并把原因写进 `warnings`。"不知道"必须显式可见，
   而不是伪装成一个具体的数字。
3. **不列白名单。** 指令集特征按族匹配——漏项是静默的，报告看起来正常，只是少了一行。

## 用法

```bash
cargo run -p deviceinfo-cli            # 给人看
cargo run -p deviceinfo-cli -- --json  # 跨机器 diff 用
cargo run -p deviceinfo-cli -- --root ./fixtures/arm64 --arch aarch64
cargo test
```

```rust
let report = deviceinfo::probe(std::path::Path::new("/"));
println!("{}", deviceinfo::render::human(&report));
```

所有探测都接受一个 `root` 前缀而不是写死 `/`，所以可以用**假文件树**做单元测试，
也可以探测容器内的可见设备，或事后对着真实机器的采样夹具做回归。

## 模块划分

| 模块 | 职责 |
|---|---|
| `report` | 对外的数据结构，不含任何 IO |
| `cpu` | 架构、核数、性能分层、指令集 |
| `memory` | 总量与可用量 |
| `accelerator` | GPU / NPU 的设备事实、内存语义、运行时就绪度 |
| `render` | 给人看的文本渲染（CLI 与 GUI 共用一份） |
| `pci` | 把 `8086:64a0` 翻成人名（解析 `pci.ids`，不调 `lspci`） |
| `runtime` | 用户态加速栈是否齐备（设备在 ≠ 能用） |
| `sysfs` / `features` | 内部工具：按 root 前缀读文件、指令集特征族匹配 |

## 语义边界（重要）

**只报设备事实，不报环境状态。** 三个容易混的地方：

- **`has_npu()` / `has_gpu()` 只回答"设备在不在"**，不回答"能不能用"。设备节点存在但
  用户态栈不全（比如插着 NPU 却没装编译器）时它们同样返回 `true`。要问"能不能真的用"，
  看 `accelerator.runtime`，或者用 `has_usable_npu()` / `has_usable_gpu()`。
- **`AcceleratorMemory` 是三态**，不是 `Option<u64>`：`Dedicated` 有确定容量，
  `SharedWithSystem` 是"和系统内存共享，没有独立上限"（集显、NPU），`Unknown` 是"读不到"
  并附原因。合成一个 `None` 会让上层只能猜，而两个方向猜错都是错的。
- **`memory.available_bytes` 是瞬时值**，不是硬件事实。缓存或跨设备比较时会骗人。

同样地，加速器只收**上限**类的频率事实（`max_freq_mhz`），不收 `cur_freq` / `act_freq` /
`busy_time` / `memory_utilization`。

`warnings` 也不含 `RuntimeStatus::Incomplete`：那是**确定的观测**（确定缺件），不是不确定。
混在一张列表里会让"未知"失去信号。

## 已知未做

- **Intel 独显的显存**：`gpu_memory` 在驱动没暴露 `mem_info_vram_total` 且厂商是 Intel/AMD
  时判为 `SharedWithSystem`。这在 Linux 上绝大多数机器是对的（那两个厂商基本都是集显，
  而独显会暴露该节点），但**没有 Intel 独显可以验证**。
- **swap**：`MemTotal` 之外没有交换分区信息。swap 耗尽时 `MemAvailable` 会系统性高估——
  实测某台机器 swap 4 GiB 已用满，而 `MemAvailable` 还报 9 GiB。
- **磁盘**：完全没有文件系统可用空间的探测。
- **NPU 的运行时状态**：`npu_busy_time_us` / `npu_memory_utilization` / `npu_current_frequency_mhz`
  读得到但故意不收（瞬时值，见上面「语义边界」）。它们应该进一个单独的运行时状态端点。
- **`sched_mode`**：语义不清楚（HW/SW 调度），对选模型没有影响，暂且不收。
- **其它厂商的运行时判据**：目前只有 Intel NPU 和 Intel GPU 两套。NVIDIA / AMD 会返回
  `RuntimeStatus::Unknown` 而不是猜一个「就绪」。
- **ARM64 真实机器采样夹具**：ARM 路径靠手写夹具覆盖。

## 许可

GPL-3.0-or-later
