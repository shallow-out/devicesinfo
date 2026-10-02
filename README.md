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

## 四个入口，别混

| | `probe()` → `HardwareReport` | `probe_environment()` → `EnvironmentReport` | `sample_state()` → `RuntimeState` | `live::probe()` → `LiveReport` |
|---|---|---|---|---|
| 描述 | 这台机器**是什么** | **装了什么、配了什么** | **此刻**怎样 | **跑一次才知道**的 |
| 内容 | 架构、型号、核数与性能分层、指令集、加速器、内存总量 | 包管理器、init、cgroup、容器运行时与 socket、已装推理框架、镜像源、**人为声明的标签** | 可用内存、swap、磁盘余量、加速器的瞬时指标 | 工具版本、**逐个目标的连通性** |
| 怎么拿到的 | 读文件 | 读文件 | 读文件 | **执行命令 / 连网络** |
| 变化频率 | 装上就不变 | 装了/配了才变 | 每一秒都在变 | 随时在变 |
| 能否缓存 / 进夹具 | 能 | 能 | 不能 | **不能，也不进夹具** |
| 能否跨机器比较 | 能 | 不能（可改） | 不能 | 分目标比（"那台能不能拉镜像"） |

混在一起会让硬件报告失去它最大的用处：拿两台机器的报告直接 `diff`。

**报告里的路径都是"被探测机器上的路径"**（`/usr/bin/podman`），不带探测根。
这样本机、`--ssh`、夹具三种来源给出同一串，既可比，也不会把临时目录泄漏到输出里。

## 实时探测（`live`）：唯一**会执行命令、会连网络**的入口

```bash
deviceinfo live                       # 版本号 + 连通性
deviceinfo live --no-network          # 只查版本号
deviceinfo live --ssh r1 --timeout 4
```

三个刻意的限制：

1. **opt-in，不进 `EnvironmentReport`。** 那些数一秒后就可能变，混进可缓存的报告里会把它污染掉。
2. **不进夹具。** 夹具冻结的是"这台机器是什么"；把网络状态冻进去，重采必然 diff，快照信号就废了。
3. **每个检查有超时，而且并发跑。** 串行的话 N 个目标 × 超时就是几十秒。超时是**真杀进程**，不只是给 `curl` 加 `--max-time`。

**连通性必须逐个目标看**，而且机器自己配的镜像源会自动进列表——它才是那台机器实际拉镜像的通道。实测那台 NAS：

```
连通性   通      https://docker.fnnas.com          HTTP 403  （来自 /etc/docker/daemon.json）
         不通     https://huggingface.co            curl: (28) Connection timed out after 6001 ms
         不通     https://registry-1.docker.io/v2/  curl: (28) Connection timed out after 6001 ms
         通      https://www.modelscope.cn          HTTP 302
```

只测 `docker.io` 会得出"这台不能部署"的错误结论；真相是**它只能走自己的镜像源**。

> **401 算通**：Docker Hub 的 v2 API 未鉴权就回 401，把它当失败会误判能拉镜像的机器。

## 标签：唯一**人为声明**的东西

前面所有字段都是**探测**出来的，只有标签不是。`always-on` 说的是"这台机器会被一直开着"，
而 sysfs 不知道用户会不会合盖、会不会拔电——**只能由人声明**。

```bash
# /etc/deviceinfo/tags.conf      本机管理员（优先）
# /usr/share/deviceinfo/tags.conf  出厂/发行版默认（硬件产品随附）
# /etc/deviceinfo/tags.d/*.conf    分片，按用途拆开写
Always_On  powersave
```

格式是一个极简行格式：每行若干标签，`#` 开头是注释。**规范化是刻意的**（转小写、
`_` 换成 `-`）：标签的用途是**匹配**，大小写不一致会让路由静默失配。字符集限
`[A-Za-z0-9._-]`，其他字符一律拒绝并**出声**——静默忽略一张拼错的标签，等于让任务
永远找不到这台机器，而这种故障极难查。

两个规范标签（其余名字自由取，按需扩展）：

| 标签 | 含义 |
|---|---|
| `always-on` | 一直开着。任务路由第一步筛的就是它 |
| `powersave` | 省电优先，适合轻任务 |

### 路由怎么用（策略不在本模块里）

**路由策略属于调度器，探测库不做决定**——但把三份报告合起来就能筛：

| 任务 | 条件 | 依据 |
|---|---|---|
| 内核编译 | `always-on` 且 逻辑核/等效算力够 | 标签 + `HardwareReport.cpu` |
| 定时提醒 | `always-on` 且 `powersave` | 只用标签 |
| 大模型推理 | `has_usable_npu()` 或 `has_usable_gpu()` 且有容器或已装推理框架 | `HardwareReport` + `EnvironmentReport` |
| 拉镜像跑容器 | `has_container_runtime()` 且镜像源/出网可达 | `EnvironmentReport` |

```python
# 例：内核编译交给"一直开着且性能不差"的设备
[host for host in fleet
 if host.env.always_on() and host.hw.cpu.effective_cores >= 6]

# 例：定时提醒交给"一直开着且省电"的设备
[host for host in fleet
 if host.env.always_on() and host.env.powersave()]
```

## 用法

```bash
cargo run -p deviceinfo-cli                       # 硬件与能力（默认子命令）
cargo run -p deviceinfo-cli -- environment --ssh r1   # 软件环境（含 --ssh）
cargo run -p deviceinfo-cli -- live --ssh r1          # 实时：版本号 + 连通性
cargo run -p deviceinfo-cli -- state --watch /var/cache
cargo run -p deviceinfo-cli -- state --counters    # 额外读累积计数器（见下）
cargo run -p deviceinfo-cli -- --json hardware     # 跨机器 diff 用
cargo run -p deviceinfo-cli -- hardware --ssh o6n          # 隔着 ssh 探测另一台机器
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
| `cpu` | 架构、处理器/整机型号、核数、性能分层、指令集 |
| `memory` | 内存**总量**（事实） |
| `accelerator` | GPU / NPU 的设备事实、内存语义、运行时就绪度 |
| `state` | 运行时状态：可用内存、swap、磁盘余量（瞬时值） |
| `runtime` | 用户态加速栈是否齐备（设备在 ≠ 能用） |
| `pci` | `8086:64a0` ↔ 人名（解析 `pci.ids`，不调 `lspci`） |
| `render` | 给人看的文本渲染（CLI 与 GUI 共用一份） |
| `sysfs` / `features` | 内部工具：按 root 前缀读文件、解析 cpuinfo 的特征行 |

## 真机夹具（`fixtures/`）

`fixtures/` 下每一棵树都是一台**真实机器**的采样，配一份 `expected.json` 快照。
目前四台：

| 夹具 | 机器 | 补上了哪种形状 |
|---|---|---|
| `lunar-lake-ultra7-258v` | Intel Lunar Lake | x86 hybrid 4P+4E、NPU 走 `/dev/accel` + PCI class `0x1200`、xe 驱动 |
| `r1-venus-i9-12900h` | Intel i9-12900H（Minisforum Venus） | **`cpu_capacity` 退化**（全 1024）→ 只能靠 `highest_perf`；**SMT**（20 线程 / 14 物理核）；**i915** 的频率路径 |
| `radxa-orion-o6n` | CIX CD8180 | ARM **5 簇**、**ACPI 无设备树**（走 DMI）、panthor + 3 个显示控制器 |
| `radxa-rock-5b-plus` | Rockchip RK3588S | ARM 4+4、**RKNPU 走 DRM**、有设备树、内核没绑 Mali |
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
变了的时候产生 diff。改完探测逻辑或采集清单，**必须重采夹具**：夹具只冻结"它里面有的东西"，忘了重采不会
让测试变红，只会让覆盖静默缩水。一个便宜的核对办法是拿真机对着夹具比（**除根前缀外
应当逐字节相同**）：

```bash
diff <(deviceinfo --json hardware) \
     <(deviceinfo --json hardware --root fixtures/<本机那份> | sed 's|fixtures/<本机那份>||')
```

**快照变了不一定是 bug**：先看 diff，决定是"有意改了行为"
（重新 capture 并提交）还是"引入了回归"（修代码）。

**远端夹具没法在本地独立验证，本地夹具可以。** 差别在于 `expected.json` 是从哪儿算的：

| 来源 | expected.json 从哪算 | 夹具测试能发现什么 |
|---|---|---|
| `--root`（本地） | **真实机器** | 连"采集漏了一个文件"都能发现（那会让夹具的报告偏离真值） |
| `--ssh`（远端） | 镜像出来的暂存树 | 只能发现"探测逻辑变了"——采集本身的缺陷是**自洽的**，测试会一起接受 |

所以远端采集的可靠性只能靠采集自己把住：读失败报 `@@X` 并**硬失败**、缺 `proc/cpuinfo`
直接拒绝写、长度与内容用同一次读取（见下）。

采集清单（`capture_plan`）是探测逻辑的镜像，两边要一起改。软链要单独重建——
驱动名是从 `device/driver` 软链的末段读的，夹具里放空文件的话 `read_link` 会失败，
而且**不会报错**，只会静默地让夹具和真机不一样。

## 语义边界（重要）

- **`has_npu()` / `has_gpu()` 只回答"设备在不在"**，不回答"能不能用"。设备节点存在但
  用户态栈不全（比如插着 NPU 却没装编译器）时它们同样返回 `true`。要问"能不能真的用"，
  看 `accelerator.runtime`，或者用 `has_usable_npu()` / `has_usable_gpu()`。
- **相对性能的来源有优先级：`acpi_cppc/highest_perf` → `cpu_capacity` → 频率。**
  `cpu_capacity` 会在**整机全同**时退化：实测那台 i9-12900H（6 P + 8 E）上它是**全 1024**，
  只看它会把 P/E 抹平成一档"20 核 @5.00 GHz"。`highest_perf` 是它的**源头**
  （本机 56/55/37 ↔ 1024/1005/676 完全对应），归一化到 1024 后两边一致。
  它的绝对值是平台相关的（56 / 64 / …），**只有同机内的比率有意义**。
  两者都区分不了时才退到频率——频率不反映 P/E 的 IPC 差异。
- **等效算力按物理核折算，不按逻辑核。** SMT 的两个线程不等于两个核：实测那台
  i9-12900H 按逻辑核求和给 20.0，按物理核是 **10.7**（6 P + 8 E，E 核 ≈ 0.6 P）。
  每档各有多少物理核记在 `CoreTier::physical_cores`，渲染成"4 物理核（8 线程）"。
- **同一个驱动族的频率节点可能在两个不同层级。** xe 在 `<device>/tileN/gtN/freq0/`，
  i915 在 **`<card>/gt_max_freq_mhz`、`<card>/gt/gt0/rps_max_freq_mhz`**（card 目录下，
  不在 `<card>/device` 下）。写错层级不会报错，只会永远 `None`——前两台机器都没有 i915，
  所以这个洞是在 `r1` 上才暴露的。
- **`AcceleratorMemory` 是三态**，不是 `Option<u64>`：`Dedicated` 有确定容量，
  `SharedWithSystem` 是"和系统内存共享，没有独立上限"（集显、NPU），`Unknown` 是"读不到"
  并附原因。合成一个 `None` 会让上层只能猜，而两个方向猜错都是错的。
  另外**推断会明说**：有的 `SharedWithSystem` 是证据（设备没有 PCI 标识），有的是按厂商
  常见形态猜的，后者会附一条 `notes`。
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
- **显存语义里只有两件事是证据**：驱动暴露了 `mem_info_vram_total`（→ 独立显存），
  或者设备**没有 PCI 标识**（→ SoC 上集成的单元，必然共享系统内存）。
  剩下那种情况（PCI 设备 + 驱动没暴露显存总量）**从 sysfs 判不出来**：集显和独显在这
  长得一样。实测本机 iGPU 是 `0000:00:02.0`、只有一个 256 MB 的 BAR，而**非 ReBAR 的
  独显 BAR 也是 256 MB**，连 BAR 大小都不能用。所以那里给的 `SharedWithSystem` 是
  **按厂商常见形态的推断**，会附一条 `notes` 说明——ARM 服务器插一张 AMD 卡就会判错。
- **`has_gpu() == false` 的意思是"内核没暴露可计算的 GPU"，不是"硬件上没有 GPU"。**
  实测那台 RK3588S 的 Mali 在这台内核上根本没绑定（连 `panfrost`/`panthor` 都没编进去），
  只有一个 `rockchip-drm` 显示控制器，于是报告说没有 GPU。这是对的——这份报告回答的是
  "这台机器**现在**能拿什么跑模型"。同理 `has_npu()` 也不代表硬件上有没有。
- **"没看到 render 节点"有两种原因，不能混。** `Some(0)` 是确实看到了那个目录、
  里面没有（→ 显示设备）；`None` 是我们**没能看到目录**（老内核？）——那就什么都不改，
  继续当 GPU 并注明。把后者也当显示设备，会在老内核上把真 GPU 静默降级。
- **身份来源按「设备」选，不按架构选。** 有这个设备的 PCI 标识就用它（`pci.ids` 给人名、
  独立显存也看这个节点）；没有才退到设备树 `compatible`。判据是"**这个设备**有没有 PCI
  标识"，不是"这台机器是什么架构"——**ARM 服务器一样有 PCIe，一样能插独显/加速卡**
  （本机那颗 Intel NPU 就是 PCI 设备，class `0x120000`）。少了 `compatible`，
  没有 PCI 标识的加速器就只能叫 `GPU (card0)`，信息量为零。
- **处理器型号和整机型号是两个字段**（`cpu_model` / `machine_model`）：x86 上两者都有
  （`Intel(R) Core(TM) Ultra 7 258V` / `83LC`），而以前挤在一个字段里、含义还随架构变。
  整机型号按固件接口取：**设备树 `model` → DMI `product_name`**，实测两台 ARM 各走一条
  （Rockchip 有设备树、CIX 是 ACPI 机器只有 DMI）。设备树属性是 NUL 结尾、可能是 NUL
  分隔的多个值，`trim()` 去不掉。
- **`npu_memory_utilization` 的单位是字节**（驱动文档原话：*report in bytes a current NPU
  memory utilization*），即当前常驻的 NPU 内存总量。之前因为单位不明而不敢收，现在确定。
- **累积计数器默认不读**（`SampleOptions::counters`）。驱动文档：
  *"shouldn't be read too often as it may have an impact on job submission performance"*，
  推荐周期 1 秒。默认开启会让高频轮询的面板在无意中拖慢 NPU 作业提交，而这种损害
  在数据里看不出来。
- **`AcceleratorState` 用 `node`（`accel0` / `card1`）+ PCI 标识标识自己**，不用列表位置
  ——顺序不是契约。`node` 是必要的：平台设备没有 PCI 标识，否则三个显示控制器会给出
  三条一模一样的记录。
- **`npu_max_frequency_mhz` / `npu_current_frequency_mhz` 读的是新路径 `freq/*`**，
  驱动文档把前者标为 *Legacy attributes (backward compatibility)*，旧路径只当兜底。

## 已知未做

- **PCI 设备的"集显还是独显"从 sysfs 判不出来**：驱动没暴露 `mem_info_vram_total` 时，
  Intel/AMD 会被**推断**为共享系统内存并附 `notes`（那是概率，不是证据），其余厂商给
  `Unknown`。想找更硬的判据但没找到：BAR 大小不行（非 ReBAR 独显也是 256 MB），
  PCI class `0x030000`/`0x030200` 也不行。**没有独显可以验证**。
- **`sched_mode` 未收**：驱动文档已经说明它是 `HW` / `OS` 调度模式（属于**硬件事实**，
  不是瞬时值），但当前判断它对"能不能跑 / 跑多快"没有直接影响，所以没进报告。
- **DRM 上的 NPU 只能靠名字认**：`classify_drm_device` 先看驱动名/`compatible` 里有没有
  `npu`（再加一张只含 `rknpu` 的表），**再看有没有 render 节点**——有就是 GPU，
  有 `drm/` 目录却没有 render 节点就是显示设备。第一步没有结构性判据（DRM 层面 NPU 和
  GPU 长得一样），认不出来会当 GPU。
- **NPU 的其它频率档位未收**：`freq/hw_min_freq`（650）、`freq/hw_efficient_freq`（950）
  是驱动暴露的硬件事实，对能效调度有用，暂未收。
- **`freq/set_min_freq` / `set_max_freq` 是可写的**：驱动允许配置 NPU 频率上下限，
  本模块只读不写——写属于调度策略，不该由探测库做。
- **运行时判据只有 Intel 两套**（Intel NPU / Intel GPU）。所以 Rockchip 那台的 NPU 报
  "运行时 未知"、`o6n` 的 panthor 也报未知。这是诚实的——没有验证过的栈就不编判据。
- **加速器的身份来源有三种，按设备挑**：PCI 标识、设备树 `compatible`、以及什么都没有。
  `o6n` 那台**没有 `/sys/firmware/devicetree/base`**，它的 DRM 设备又是平台设备
  （没有 PCI 标识），所以名字只能退回 `GPU (card0)`。（整机型号走了 DMI 兜底，
  但**单个设备**的 DMI 信息拿不到。）
- **`warnings`** 会点名"本模块不认识的加速器"：内核给出的通用加速器入口只有
  `sys/class/accel`（→ `/dev/accel/accelN`）和 DRM，而有些卡两个都不用
  （Hailo-8 → `/dev/hailo0`、Coral → `/dev/apex_0`、FPGA → `/dev/xdma*`）。
  这类设备按 PCI `class`（内核自己的分类：`0x12xx` 处理加速器 / `0x0b40` 协处理器）
  扫出来并**点名警告**，而不是静默消失。
  刻意**只警告、不进 `accelerators`**：我们只知道它是加速器，不知道它属于哪一类、
  能不能拿来跑模型——宁可说"我认不出它"，也不要给它编一个类别。
  实测校准：`0x11xx`（Signal processing controller）**不算**——本机那两个是
  Intel DTT 功耗控制和 Crash Log Telemetry，`lspci` 也把它们和 "Processing
  accelerators [1200]" 分开标。
  **这套逻辑没有真机验证过**：手上没有任何一块这类卡，只有单元测试 + 本机那颗
  已被 `/dev/accel` 覆盖的 Intel NPU（用它验证去重是对的）。
- **arm64 的处理器型号常常拿不到**：`cpu_model` 只有内核报 `model name` 时才有值
  （Rockchip 那台就没有）。设备树里的 `cpus/cpu@N/compatible`（形如 `arm,cortex-a76`）
  是**每个簇的核类型**，不是整颗 CPU 的型号——一台 A76+A55 的机器填哪个都是误导，
  所以刻意没收。

- **版本号靠 `--version`，而它不被所有工具支持。** 实测 `llama-cli` 会成功但什么都不输出，
  `llama-bench` 直接报参数错误。报告里会把这类如实写成"查不到（命令成功但没有输出）"，
  而不是假装没有版本——但**要拿到 llama.cpp 的真实版本得换别的问法**（比如解析 `--help`），
  那属于按工具定制，暂不做。
- **连通性只回答"这个 URL 此刻有没有响应"**：不验证证书链之外的东西，也不代表能完成一次
  真实的镜像拉取（鉴权、manifest、平台匹配都还没发生）。
- **容器镜像源只解析到"配置文件里写了什么"**：`podman` 的 `registries.conf` 解析
  **没有真机验证过**（三台 Arch 上那份是上游模板、整份都被注释掉），只有合成样本的单元测试。
  Docker 那条有真样本（那台 NAS）。
- **正在监听的端口**没进环境报告：那是**状态**（服务起了才变），该进 `state`，而 `state`
  目前不支持 `--ssh`（磁盘余量查的是本机挂载的文件系统）。
- **`socket` 存在与否不等于守护进程健康**：只能说明"有人起过它"。
- **`libc` 依赖**：只为了 `statvfs`（标准库至今没有 `std::fs::statfs`/`statvfs`）。
  全部 `unsafe` 只出现在 `state::filesystem_usage` 一处。

## 拿到实机后怎么补

上面那几条的共同点是**必须有对应硬件才算验证过**——没有硬件就写，写出来的就是没验证过的
枚举逻辑。拿到机器时按下面的配方走，基本都是几分钟的事。

### NVIDIA / AMD 的运行时判据

```bash
ssh <host> 'ls /usr/lib/libcuda.so* /usr/lib/libnvidia-ml.so* /usr/lib/libze_* /usr/lib/libamd* 2>/dev/null;
            command -v nvidia-smi rocminfo'
deviceinfo hardware --ssh <host>     # 现在会报 "运行时 未知 · 还没有 NVIDIA 的用户态运行时判据"
```

加一条 `spec_for` 的分支即可。**关键是先确认"缺了哪个包就真的跑不起来"**——判据的力量来自
"缺了就一定能拦住"，不是来自"装齐了就有"。加完要**故意少装一个包**验证它变 `Incomplete`。

### PCI 加速器点名逻辑（Hailo-8 / Coral / FPGA）

```bash
lspci -nn | grep -iE '\[1200\]|\[0b40\]'   # 确认内核给它的 class
deviceinfo hardware | tail -3                   # 应当有一条警告点名它
```

如果它同时也走 `/dev/accel`（有些卡会），要确认**去重生效**——同一块卡只能被点名一次。

### Intel 独显的显存语义

```bash
ls /sys/class/drm/card*/device/mem_info_vram_total   # xe 在独显上到底暴不暴露显存
```

- **暴露了** → 会得到 `Dedicated{bytes}`，那么"Intel → 推断为共享"这条分支只在集显上生效，
  可以接受（但仍建议把推断说明保留）。
- **没暴露** → 会得到 `SharedWithSystem` **加一条"这是推断"的说明**。那条说明正好证明了它
  存在的必要：此时 Intel（Intel/AMD）分支应当改成 `Unknown`，或者继续找更硬的判据
  （BAR 大小和 PCI class 都试过，都不行，见上）。

### `0x1200` / `0x0b40` 之外的加速器类别

见到具体设备再收。`0x11xx` 已经实测确认**不是**（本机那两个是 Intel DTT 功耗控制和
Crash Log Telemetry，`lspci` 也把它们分开标），别提前扩。

### arm64 的处理器型号

设备树 `cpus/cpu@N/compatible` 是**每个簇的核类型**（`arm,cortex-a76`），不是整颗 CPU 的型号
——要补的话得先想清楚 `cpu_model` 怎么表达"一颗 CPU 有三种核"，否则填哪个都是误导。

## 许可

GPL-3.0-or-later
