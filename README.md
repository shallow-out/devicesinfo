# devicesinfo

设备信息与能力探测。当前含 `deviceinfo` 模块：**CPU / 内存 / 加速器**，以及与之分开的
**运行时状态**采样。

目的只有一个：**在分配任务之前，准确回答这台设备能做什么、此刻还能不能做。**
上层（模型分发、任务调度、面板）靠这份数据决定"这个模型该不该装、该在哪跑"，
所以**报错比不报更糟**——一个乐观的数字会让人装上一个跑不动的模型，
一个悲观的数字会让本机永远拿不到该干的活。

## 三条原则

1. **优先 sysfs，`/proc/cpuinfo` 只当兜底。** cpuinfo 是给人看的文本，字段随架构变化
   （ARM64 没有 `physical id`/`core id`）；`sys/devices/system/cpu/*/topology/` 是内核主动
   提供的稳定接口，x86 和 ARM 都有。
2. **不猜。** 算不出来就留 `None`，并把原因写进 `warnings`。"不知道"必须显式可见，
   而不是伪装成一个具体的数字。
3. **不在数据层做筛选。** 指令集特征**原样上报内核给出的全部项**。早先按前缀挑
   "与推理相关"的，两个方向都被打脸：x86 的 `smep`（内核安全特性）撞上 `sme` 前缀混了
   进来；arm64 我以为叫 `dotprod` 的东西内核实际报 `asimddp`，于是列表里躺着一个永远
   匹配不到的死项。更根本的是**筛选在架构上就是错的**——消费方问的是开放式问题
   （"有没有 AMX"、"有没有 SVE2"、"有没有 FP8"），生产者一旦筛掉，信息就永久丢失，
   而且丢失是无声的。好看不好看交给显示层（`render`）。

## 硬件 vs 运行时状态

两个入口，别混：

| | `probe()` → `HardwareReport` | `sample_state()` → `RuntimeState` |
|---|---|---|
| 描述 | 这台机器**是什么** | **此刻**怎样 |
| 内容 | 架构、核数与性能分层、指令集、加速器与运行时就绪度、内存总量 | 可用内存、swap、磁盘余量、加速器的频率/常驻内存/累积忙碌时间 |
| 变化频率 | 装上就不变 | 每一秒都在变 |
| 能否缓存 / 跨机器 diff | 能 | 不能 |

混在一起会让硬件报告失去它最大的用处：拿两台机器的报告直接 `diff`。

## 用法

```bash
cargo run -p deviceinfo-cli                       # 硬件与能力（默认子命令）
cargo run -p deviceinfo-cli -- state --watch /var/cache
cargo run -p deviceinfo-cli -- state --counters    # 额外读累积计数器（见下）
cargo run -p deviceinfo-cli -- --json hardware     # 跨机器 diff 用
cargo run -p deviceinfo-cli -- hardware --root ./fixtures/lunar-lake-ultra7-258v
cargo run -p deviceinfo-cli -- capture --ssh o6n --arch aarch64 --out fixtures/新机器
cargo test
```

```rust
let report = deviceinfo::probe(std::path::Path::new("/"));
println!("{}", deviceinfo::render::human(&report));

let state = deviceinfo::sample_state(&[std::path::PathBuf::from("/var/cache")]);
```

所有探测都接受一个 `root` 前缀而不是写死 `/`，所以可以用**假文件树**做单元测试，
也可以探测容器内的可见设备，或事后对着真机采样夹具做回归。

例外是磁盘余量：它走 `statvfs`，查的是**真实挂载的文件系统**，
`sample_state_with(root, watch)` 里的 `watch` 不经过 `root`——对着假文件树问
"这块盘还剩多少"没有意义。

## 模块划分

| 模块 | 职责 |
|---|---|
| `report` | 硬件报告的对外结构，不含任何 IO |
| `cpu` | 架构、核数、性能分层、指令集 |
| `memory` | 内存**总量**（事实） |
| `accelerator` | GPU / NPU 的设备事实、内存语义、运行时就绪度 |
| `state` | 运行时状态：可用内存、swap、磁盘余量（瞬时值） |
| `runtime` | 用户态加速栈是否齐备（设备在 ≠ 能用） |
| `pci` | `8086:64a0` ↔ 人名（解析 `pci.ids`，不调 `lspci`） |
| `render` | 给人看的文本渲染（CLI 与 GUI 共用一份） |
| `sysfs` / `features` | 内部工具：按 root 前缀读文件、解析 cpuinfo 的特征行 |

## 真机夹具（`fixtures/`）

`fixtures/` 下每一棵树都是一台**真实机器**的采样，配一份 `expected.json` 快照。
目前三台：Intel Lunar Lake（x86 混合架构，有 NPU）、Rockchip RK3588S（ARM，RKNPU 走 DRM）、
CIX CD8180（ARM，ACPI 启动无设备树，5 个 CPU 簇，panthor GPU + 3 个显示控制器）。
`cargo test` 会拿着夹具重跑一遍探测，结果和快照不一致就失败。

这解决的是一个具体问题：**手写的假 features 列表抓不到真实内核里的意外。**
`smep`（Supervisor Mode Execution Prevention，内核安全特性，不是指令集）曾经因为
`sme` 前缀被当成加速指令集列入报告；arm64 那边的 `dotprod` 则是个永远匹配不到的死项。
两个都是手写列表造成的，两个都是夹具（或查内核源码）才发现的。真机采样里那 146 个
x86 flag / 108 个 arm64 feature 全都躺着，下次再引入同类偏差会立刻红。

加一台机器：

```bash
# 本机
cargo run -p deviceinfo-cli -- capture --out fixtures/<名字>
# 另一台机器**隔着 ssh** 采（推荐：目标机不需要任何工具链）
cargo run -p deviceinfo-cli -- capture --ssh <主机名> --arch <架构> --out fixtures/<名字>
git add fixtures/<名字>
```

**为什么采集要能隔着 ssh 做**：夹具必须在目标机上采，而目标机往往装不了 Rust 工具链
（路由器、嵌入式盒子），本机也未必能交叉编译到它的架构（本机 Rust 是 pacman 装的，
只有 host target，没有 rustup）。远端采集的做法是先把清单里的东西镜像成本地临时树，
之后**一切都按本地处理**——本地路径一字未改。

采集期间踩过的两个坑，都是"不报错但结果全错"那类：

- **每条远端命令必须先 `cd /`。** 远端 shell 的 cwd 是 HOME，而清单里的路径都是从根
  起算的相对路径；少了这一步整份清单会全报"不存在"，采集"成功"但内容全是空的。
- **库目录要只取非目录。** `LIBRARY_DIRS` 里既有 `usr/lib` 又有 `usr/lib/aarch64-linux-gnu`，
  而 `usr/lib` 的列表里就含 `aarch64-linux-gnu` 这个**目录名**——当成库文件写成普通文件后，
  下一步往它里面写就 `EEXIST`。

另外**不存在的路径必须跳过**，不能写成空文件：那会让探测把缺的 `vendor`/`device`
读成空字符串，报告里就冒出 `GPU (card0, id )` 和 `(:)` 这种垃圾。

性能：单次 ssh 往返在同一局域网实测 **770ms**（握手 + 认证 + 远端起 shell），
而一次采集有二十来次往返。两步优化之后 `o6n` 那台从 **18s → 2.2s**：

1. **连接复用**（`ControlMaster`）：18s → 4.1s
2. **把列目录 / 读软链 / 读内容各自合并成一次往返**：4.1s → 2.2s

还剩的固定开销是"每个阶段一次往返"（四个阶段）加上第一次握手，已经接近下限了。

**采集期间踩过的第三个坑**：喂给远端 `while read` 的清单**末尾必须有换行**。
`read` 遇到 EOF 且没有换行时返回非零，`while` 的循环体就不执行——**最后一行被静默丢掉**。
现象是"每台机器最后一个 DRM 设备的 `drm/` 目录列表凭空消失"，只是夹具里少一个条目。

采集是幂等的（`/proc` 里的瞬时字段会被剔掉；只被状态采样读的瞬时值文件——
`npu_busy_time_us`、`*_cur_freq`、`npu_memory_utilization`——读数归一化成 `0`，
它们**存在**这件事才是被测的对象），所以重新采集只会在探测逻辑**真的**
变了的时候产生 diff。**快照变了不一定是 bug**：先看 diff，决定是"有意改了行为"
（重新 capture 并提交）还是"引入了回归"（修代码）。

采集清单（`capture_plan`）是探测逻辑的镜像，两边要一起改。软链要单独重建——
驱动名是从 `device/driver` 软链的末段读的，夹具里放空文件的话 `read_link` 会失败，
而且**不会报错**，只会静默地让夹具和真机不一样。

## 语义边界（重要）

- **`has_npu()` / `has_gpu()` 只回答"设备在不在"**，不回答"能不能用"。设备节点存在但
  用户态栈不全（比如插着 NPU 却没装编译器）时它们同样返回 `true`。要问"能不能真的用"，
  看 `accelerator.runtime`，或者用 `has_usable_npu()` / `has_usable_gpu()`。
- **`AcceleratorMemory` 是三态**，不是 `Option<u64>`：`Dedicated` 有确定容量，
  `SharedWithSystem` 是"和系统内存共享，没有独立上限"（集显、NPU），`Unknown` 是"读不到"
  并附原因。合成一个 `None` 会让上层只能猜，而两个方向猜错都是错的。
- **`RuntimeStatus::Unknown` 不是失败**，是"认不出这个厂商该找什么"。NVIDIA / AMD 目前
  都走这条路——编一个"就绪"会让用户装上一个跑不起来的模型。
- **`warnings` 不含 `Incomplete`**：那是**确定的观测**（确定缺件），不是不确定。
  混在一张列表里会让"未知"失去信号。
- **加速器只收"上限"类的频率事实**（`max_freq_mhz`），不收 `cur_freq` / `act_freq` /
  `busy_time` / `memory_utilization`——那些是瞬时值，该进 `state`。
- **`DiskUsage` 同时给 `free_bytes` 和 `available_bytes`**：前者含 root 保留块，
  后者是当前用户实际可写的量。判断"装不装得下"要用后者（ext4 默认给 root 留 5%）。
- **`swap_exhausted()` 是个信号，不是数字**：swap 用满时 `MemAvailable` 会系统性高估，
  实测某台机器 swap 4 GiB 已用满而 `MemAvailable` 仍报 9 GiB。
- **频率 `Some(0)` 的含义是"设备空闲"**，不是"读不到"。ivpu 驱动文档：`freq/current_freq`
  "Valid only when the device is active; returns 0 when idle"。渲染成 `空闲` 而不是
  `0 MHz`——后者会让人以为读数坏了。读不到才是 `None`。
- **一个 DRM `cardN` 未必是 GPU。** Rockchip 的 RKNPU 走 DRM 暴露（这个内核不用
  `/dev/accel`），实测它出现在 `/sys/class/drm/card0`。一律当 GPU 的后果很重：
  一台**确实有 NPU** 的机器上 `has_npu()` 返回 `false`，上层再也不会考虑 NPU 卸载，
  而且没有任何报错。判据来自驱动名 / 设备树 `compatible`。
- **`renderD*` 的归属由 `<device 目录>/drm/` 给出**，不按下标猜。实测那台 ARM 机器上
  `renderD128` 属于 card0，而"第 N 个 render 配第 N 个 card"把它给了 card1。
  也刻意不解 `sys/class/drm/renderD*` 的软链：软链存不进夹具。
- **有没有 render 节点决定它是 GPU 还是只会显示。** DRM 的 render 节点就是给渲染/计算
  用的（`DRIVER_RENDER`），纯显示控制器拿不到——这是**结构性判据，不需要任何驱动名
  白名单**。实测 `linlondp`（3 个）和 `rockchip-drm` 都因此被正确归为
  [`AcceleratorKind::Display`]，于是只有显示控制器的机器上 `has_gpu()` 是 `false`。
  它们仍在 `accelerators` 里，因为"有几个 DRM 设备、分别是什么"是值得知道的事实，
  只是不参与"能不能跑模型"的判断。`runtime` 对它们是 `NotApplicable`——
  那是"知道不用找"，和 `Unknown`（"不知道该找什么"）分开。
- **"没看到 render 节点"有两种原因，不能混。** `Some(0)` 是确实看到了那个目录、
  里面没有（→ 显示设备）；`None` 是我们**没能看到目录**（老内核？）——那就什么都不改，
  继续当 GPU 并注明。把后者也当显示设备，会在老内核上把真 GPU 静默降级。
- **`compatible` 是非 PCI 平台上设备的权威标识。** ARM／嵌入式上既没有 PCI id，
  也没有 `cardN` 以外的名字——少了它，加速器只能叫 `GPU (card0, id 未知)`，信息量为零。
- **arm64 的机器型号只能从设备树读**（`/sys/firmware/devicetree/base/model`）：
  `/proc/cpuinfo` 在 arm64 上**不报型号**，一行都没有。设备树属性是 NUL 结尾、
  可能是 NUL 分隔的多个值，`trim()` 去不掉。
- **`npu_memory_utilization` 的单位是字节**（驱动文档原话：*report in bytes a current NPU
  memory utilization*），即当前常驻的 NPU 内存总量。之前因为单位不明而不敢收，现在确定。
- **累积计数器默认不读**（`SampleOptions::counters`）。驱动文档：
  *"shouldn't be read too often as it may have an impact on job submission performance"*，
  推荐周期 1 秒。默认开启会让高频轮询的面板在无意中拖慢 NPU 作业提交，而这种损害
  在数据里看不出来。
- **`AcceleratorState` 用 PCI 标识当连接键**（不是列表下标——顺序不是契约）。
  没有 PCI 的加速器（ARM 上的 NPU 之类）只能靠 `kind` 对应。
- **`npu_max_frequency_mhz` / `npu_current_frequency_mhz` 读的是新路径 `freq/*`**，
  驱动文档把前者标为 *Legacy attributes (backward compatibility)*，旧路径只当兜底。

## 已知未做

- **Intel 独显的显存**：驱动没暴露 `mem_info_vram_total` 且厂商是 Intel/AMD 时判为
  `SharedWithSystem`。Linux 上这绝大多数机器是对的（那两个厂商基本都是集显，而独显会
  暴露该节点），但**没有 Intel 独显可以验证**。
- **`sched_mode` 未收**：驱动文档已经说明它是 `HW` / `OS` 调度模式（属于**硬件事实**，
  不是瞬时值），但当前判断它对"能不能跑 / 跑多快"没有直接影响，所以没进报告。
- **DRM 上的 NPU 靠名字识别**：`classify_drm_device` 用驱动名 / `compatible` 里是否含
  `npu`，再加一张极短的表（目前只有 `rknpu`）。DRM 层面 NPU 和 GPU 长得一样，
  没有结构性判据；认不出来就保守地当 GPU 并在 `notes` 里说明。
- **ARM 上加速器的运行时判据仍然缺失**：`runtime::spec_for` 只有 Intel 的两套，
  所以那台 Rockchip 机器上 NPU 报"运行时 未知"。这是诚实的——没有验证过的栈就不编判据。
- **NPU 的其它频率档位未收**：`freq/hw_min_freq`（650）、`freq/hw_efficient_freq`（950）
  是驱动暴露的硬件事实，对能效调度有用，暂未收。
- **`freq/set_min_freq` / `set_max_freq` 是**可写**的**：驱动允许配置 NPU 频率上下限，
  本模块只读不写——写属于调度策略，不该由探测库做。
- **其它厂商的运行时判据**：目前只有 Intel NPU 和 Intel GPU 两套。
- **`CpuInfo.model` 一个字段担了两种含义**：x86 上是**处理器**型号（cpuinfo 的
  `model name`），ARM 上通常是**整机**型号（设备树 `model` 或 DMI `product_name`）——
  因为平台只给得出后者。拆成 `cpu_model` + `machine_model` 更干净，但会动到字段。
- **加速器的身份来源在三种平台上各不相同**：PCI id（x86）、设备树 `compatible`
  （多数 ARM）、DMI（ACPI 启动的 ARM）。`o6n` 那台**整个 `/sys/firmware/devicetree`
  都不存在**，所以它的加速器没有 `compatible`，名字只能退回 `GPU (card0)`。
  加了 DMI 兜底之后**整机**型号能拿到，但**单个设备**的 DMI 信息是拿不到的——
  那台机器上设备身份就到此为止。
- **设备树/DMI 的型号兜底没有真机验证**：`machine_model` 的 DMI 分支目前没有一台
  "既没设备树、cpuinfo 又不报型号"的机器可以验证（`o6n` 的 cpuinfo 里有
  `model name`，所以走的是第一条路）。单元测试覆盖了，真机没验。
- **`libc` 依赖**：只为了 `statvfs`（标准库至今没有 `std::fs::statfs`/`statvfs`）。
  全部 `unsafe` 只出现在 `state::filesystem_usage` 一处。

## 许可

GPL-3.0-or-later
