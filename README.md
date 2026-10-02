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
| `accelerator` | GPU / NPU 的存在性与驱动绑定 |
| `render` | 给人看的文本渲染（CLI 与 GUI 共用一份） |
| `sysfs` / `features` | 内部工具：按 root 前缀读文件、指令集特征族匹配 |

## 语义边界（重要）

**只报设备事实，不报环境状态。** 两个已知的例子：

- `has_npu()` 只回答"设备在不在"，**不回答"能不能用"**。设备节点存在但用户态编译器
  （如 OpenVINO 的 NPU 插件）缺失时它同样返回 `true`。能力判定是另一层的事。
- `memory.available_bytes` 是**瞬时值**，不是硬件事实。缓存这份报告、或者拿它跨设备
  比较时会骗人。

## 已知未做

- GPU 显存：目前一律 `None`，而集显的正确语义是"共享系统内存"而不是"未知"。
- GPU 名字：只有 PCI device id（`0x64a0`），没有翻成人名（`Arc Graphics 130V/140V`）。
- swap：`MemTotal` 之外没有交换分区信息，而 swap 耗尽时 `MemAvailable` 会系统性高估。
- 磁盘：完全没有文件系统可用空间的探测。
- ARM64 真实机器采样夹具：目前 ARM 路径靠手写夹具覆盖。

## 许可

GPL-3.0-or-later
