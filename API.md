# deviceinfo v2 接口契约

库版本 0.2.0，JSON `schema_version = 2`。这是有意的破坏性升级。
后续 Next、mesh 和其他消费方只使用本页列出的接口；旧入口已删除，不提供兼容别名。

| 入口 | `Snapshot.data` | 用途 |
|---|---|---|
| `inspect_system(root, arch)` | `SystemReport` | CPU、内存容量、系统及 SoC 身份，低频缓存 |
| `inspect_hardware(root, arch)` | `HardwareReport` | GPU/NPU 身份、内存语义和推理栈就绪度 |
| `inspect_environment(root)` | `EnvironmentReport` | 已安装工具、容器及配置 |
| `inspect_platform(root)` | `PlatformReport` | 分别观测 UEFI、DT、ACPI 和 DMI |
| `inspect_soc(root)` | `SocReport` | 芯片厂商、型号及属性来源 |
| `inspect_storage(root)` | `StorageReport` | 块设备、分区、存储层和挂载关系 |
| `inspect_device_links(root)` | `()` | 供采集工具冻结 sysfs 实例身份 |
| `observe_system(root, options)` | `SystemState` | 内存/swap、CPU ticks、负载、uptime、文件系统空间 |
| `observe_accelerators(root, options)` | `RuntimeState` | GPU/NPU 瞬时读数；累积计数器需显式启用 |
| `observe_thermal(root, options)` | `ThermalReport` | 温度、RPM、PWM、散热状态，按需只读 |
| `observe_storage_health(root, options)` | `StorageHealthReport` | MMC sysfs；NVMe ioctl 需显式启用 |
| `observe_mmc_health(root, path)` | `MmcHealthObservation` | 显式读取原生 MMC EXT_CSD；注入目录禁止打开设备 |
| `check_environment(environment, options, runner, read_stamp)` | `LiveReport` | 显式运行命令/网络检查，命令与时间回调必须属于同一目标 |

输入清单、数据类型、纯字节解码器和文本渲染函数仍可复用。底层裸 IO 探测函数不是公开 API。
读取载荷要使用 `.data`；保存、传输和缓存必须保留**完整** `Snapshot`。
不添加 `Deref` 或隐式提取兼容层，避免消费方丢掉来源信息后继续差分。

## 采样上下文

每个快照包含 `schema_version`、`context`、`devices`、`data`。

- `context.started` / `finished` 是来源机器上整个读取窗口的起止观测，不宣称多文件读取原子化。
- `unix_time_ns` 用于展示/日志，`boot_time_ns` 用于差分。两者在 JSON 中是**十进制字符串或 null**，防止 JavaScript 丢失纳秒精度。Rust 中是 `Option<u64>`。
- Linux 原生读取使用 `CLOCK_BOOTTIME`；SSH 使用目标 `/proc/uptime`，报告其 10 ms 精度。`boot_clock_resolution_ns` 明确时钟分辨率。
- 起止都记录 `boot_id`、time namespace 和 mount namespace。缺失、格式不合法、采样中重启、namespace 改变或单调时间回退时 `consistent = false`。
- `origin` 是 `live`、`captured` 或 `unattributed`。任意注入目录不会借用采集机时钟、启动标识或 sysfs inode。
- UTC 校时不参与差分。时钟依据见 [CLOCK_BOOTTIME](https://www.man7.org/linux/man-pages/man2/clock_gettime.2.html) 和 [time namespaces](https://www.man7.org/linux/man-pages/man7/time_namespaces.7.html)。

不同 OS 的启动标识适配尚未实现；非 Linux 原生根可以展示已读到的信息，但不能伪造 Linux 启动标识来启用差分。

## 设备关联与差分

`devices` 保留每条通道/设备的 `source`、解析后的内核设备 `physical_path`、
`kernel_instance`、`driver` 和 `device_number`。路径都属于来源机器，不含夹具前缀。

- 温度、风扇和 PWM 通道以原报告的 `source` 查询；这些通道可以关联同一个父设备，但不据此推断风扇/PWM 接线。
- 加速器库存和状态通过必填 `source` 关联，以 `/sys/class/accel/<node>` 或 `/sys/class/drm/<node>` 查询；存储以 `/sys/class/block/<name>` 查询。
- PCI vendor:product 只是型号标识，不是设备实例。禁止按列表下标、`kind` 或相同型号拼接两次采样。
- `kernel_instance` 使用来源内核的物理设备和 class 对象身份，作用域是同一次启动与 namespace。热插拔、节点重用、驱动/设备号变化会使关联失效。无法证实时保留 null。
- 原生文件系统空间附来源路径、major:minor 和可解析的块设备路径；不猜文件系统累积计数器实例，不能把空间余量当累积计数器差分。

消费方调用 `Snapshot<SystemState>::cpu_usage_since(previous)` 或
`Snapshot<RuntimeState>::accelerator_busy_since(previous, node)`。
后续 ARM/网络计数器可使用 `counter_since(previous, source, current, old)`，传入值必须来自对应设备的**同一个指标**。

方法先验证 schema、完整上下文、同一启动/namespace、非重叠窗口与时钟精度，再验证设备实例和计数器。
`DifferenceError` 区分 `different_boot`、`different_namespace`、`non_increasing_time`、
`invalid_context`、`unknown_device`、`device_changed`、`counter_reset`、`missing_counter`、`no_counter_progress`、`schema_mismatch`。
任何错误都展示未知；不得转为 0%。`CounterDifference.elapsed_ns` 使用窗口中点估计，
`delta` 保留计数器原单位，`per_second()` 转为每秒。较长采样窗口会增加速率估计的不确定性。

## SSH 与夹具

`check_environment` 在执行全部检查前后各调用一次 `read_stamp`，获取目标当时的
`SampleStamp`；回调签名为 `Fn() -> io::Result<SampleStamp>`。
回调必须重新读取目标时间、启动身份与 namespace，不能返回镜像的冻结窗口。
它的上下文独立于用于选择检查项的环境库存。SSH CLI 在两侧重新查询目标机，绕过文件缓存。
回调失败时保留检查结果，失败边界全部为 null，`origin = unattributed`、`consistent = false`，
并附结构化诊断；启动身份缺失、重启或 namespace 改变也不能声明一致。
本地 `live --root <镜像>` 无法获取来源的即时时钟，按未知处理，不使用镜像或采集机时间。

新采集会冻结 `deviceinfo-context.json` (`CaptureMetadata`) 和来源 `boot_id`。
SSH 的时钟、namespace、canonical sysfs 路径和 inode 全部在目标机读取，并在采集前后复核。
本地镜像读取复用这些原始标识，不将暂存目录 inode 当成目标设备身份。

- 完整 SSH 镜像保留原计数器；同一镜像反复回放的时间窗口相同，因此不能生成速率。
- `capture` 会清理部分瞬时读数，故 `counters_preserved = false`，禁止用于差分。
- 四台实机夹具保留原始输入，期望载荷仅增加经文件树验证的 `source` 关联键，其余历史事实不变；缺少来源上下文时，新入口报告 `unattributed` 与结构化诊断。它们不是兼容接口，也没有被补造启动时间。
- 注入根的 `watch` 不执行采集机 `statvfs`，以免将 Mac 的磁盘空间归到远端设备。返回空 disks 与 `context.diagnostics` 的 `unsupported`。

`context.diagnostics` 采用统一 `Diagnostic`；thermal/platform 载荷也采用同一结构。
其他载荷目前仍有供人阅读的 `warnings`，NVMe 保留控制器状态错误。消费方不应解析这些文字来判断启动、设备关联或差分状态。

正式接入固定经过验证的完整提交，并提交调用方 Cargo.lock。新的公开接口和 CLI JSON 都要求显式迁移，不接受旧版本报告冒充 v2 快照。
