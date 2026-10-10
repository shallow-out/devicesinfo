//! 设备信息与能力探测：CPU / 内存 / 加速器。
//!
//! 目的只有一个：**在分配任务之前，准确回答这台设备能做什么**。
//! 上层（模型分发、任务调度、面板）靠这份报告决定"这个模型该不该装、该在哪跑"，
//! 所以**报错比不报更糟**——一个乐观的数字会让人装上一个跑不动的模型，
//! 一个悲观的数字会让本机永远拿不到该干的活。
//!
//! # 三条原则
//!
//! 1. **优先 sysfs，`/proc/cpuinfo` 只当兜底。** cpuinfo 是给人看的文本，字段随架构变化
//!    （ARM64 没有 `physical id`/`core id`）；`sys/devices/system/cpu/*/topology/` 是内核
//!    主动提供的稳定接口，x86 和 ARM 都有。
//! 2. **不猜。** 算不出来就留 `None`，并把原因写进 [`HardwareReport::warnings`]。
//!    "不知道"必须显式可见，而不是伪装成一个具体的数字。
//! 3. **不列白名单。** 指令集特征按族匹配——漏项是静默的，报告看起来正常，只是少了一行。
//!
//! # 模块划分
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`report`] | 对外的数据结构，不含任何 IO |
//! | [`cpu`] | 架构、核数、性能分层、指令集 |
//! | [`memory`] | 总量与可用量 |
//! | [`accelerator`] | GPU / NPU 的设备事实、内存语义、运行时就绪度 |
//! | [`render`] | 给人看的文本渲染（CLI 与 GUI 共用一份） |
//! | `pci` | 把 `8086:64a0` 翻成人名（解析 `pci.ids`，不调 `lspci`） |
//! | `runtime` | 用户态加速栈是否齐备（设备在 ≠ 能用） |
//! | `sysfs` / `features` | 内部工具：按 root 前缀读文件、指令集特征族匹配 |
//!
//! # 硬件 vs 运行时状态
//!
//! 两个入口，别混：
//!
//! - [`probe`] → [`HardwareReport`]：**这台机器是什么**。装上就不变，可以缓存、可以跨设备比较。
//! - [`sample_state`] → [`RuntimeState`]：**此刻怎样**。每一秒都在变，不能缓存、不能比较。
//!
//! 混在一起会让硬件报告失去它最大的用处：拿两台机器的报告直接 `diff`。
//!
//! # 可注入的 root
//!
//! 所有探测都接受一个 `root` 前缀而不是写死 `/`，因此可以用**假文件树**做单元测试，
//! 也可以用来探测容器内的可见设备，或者事后对着真实机器的采样夹具做回归。
//! 真实调用传 [`Path::new("/")`]。
//!
//! ```
//! let report = deviceinfo::probe(std::path::Path::new("/"));
//! println!("{}", deviceinfo::render::human(&report));
//! ```

mod accelerator;
mod cpu;
pub mod environment;
mod features;
pub mod live;
mod memory;
pub mod mmc_health;
mod os;
mod report;
mod runtime;
mod state;
mod sysfs;
pub mod soc;
pub mod storage;
pub mod storage_health;
pub mod system;
pub mod tags;

pub mod pci;
pub mod render;

pub use report::{
    Accelerator, AcceleratorKind, AcceleratorMemory, CoreTier, CpuFeatureGroup, CpuInfo, HardwareReport, MemoryInfo,
    PciId, RuntimeStatus,
};
pub use cpu::{PER_CORE_INPUTS as CPU_PER_CORE_INPUTS, SHARED_INPUTS as CPU_SHARED_INPUTS};
pub use environment::EnvironmentReport;
pub use live::{LiveOptions, LiveReport};
pub use runtime::{LIBRARY_DIRS, OPENCL_VENDOR_DIR, library_inputs as runtime_library_inputs};
pub use state::{AcceleratorState, DiskUsage, MemoryState, RuntimeState, SampleOptions};
pub use sysfs::resolve_path_in_root;
pub use mmc_health::{MmcExtCsdHealth, decode_mmc_ext_csd, read_mmc_health};
pub use soc::{SocDevice, SocReport, probe_soc};
pub use storage::{BlockDevice, StorageInterface, StorageMount, StorageReport, probe_storage};
pub use storage_health::{
    MmcHealth, NvmeHealth, NvmeHealthError, NvmeSmartLog, StorageHealthOptions,
    StorageHealthReport, StorageHealthState, StorageHealthError, StorageHealthErrorKind, decode_nvme_smart_log, read_nvme_health,
    sample_storage_health, sample_storage_health_with,
};
pub use system::{
    CpuTimes, SystemReport, SystemSampleOptions, SystemState, probe_system, probe_system_with,
    sample_system_state, sample_system_state_with,
};

use std::collections::BTreeSet;
use std::path::Path;

/// 探测真实系统。`root` 传 `/`。
pub fn probe(root: &Path) -> HardwareReport {
    probe_with(root, std::env::consts::ARCH)
}

/// 与 [`probe`] 相同，但架构可注入。
///
/// 架构用**编译期常量**而不是读文件：本模块总是探测自己所在的机器，二进制架构就是主机架构，
/// 而 `/proc/cpuinfo` 里根本没有 arch 字段（x86 只给 `vendor_id`，arm64 什么都不给）。
/// 参数化只是为了测试夹具能伪造一台 aarch64 机器。
pub fn probe_with(root: &Path, arch: &str) -> HardwareReport {
    let mut warnings = Vec::new();
    let cpu = cpu::probe(root, arch, &mut warnings);
    let memory = memory::probe(root, &mut warnings);
    // 库索引建一次，所有加速器共用——每台机器通常有好几个加速器，
    // 各自重新遍历一遍 /usr/lib 是没必要的浪费
    let libraries = runtime::LibraryIndex::build(root);
    // 已报过的加速器占了哪些 PCI 槽位——扫 PCI 总线时要用它去重
    // （Intel 的 NPU 本身就是一个 class 0x1200 的 PCI 设备）
    let mut known_pci_slots = BTreeSet::new();
    let mut accelerators =
        accelerator::probe_npus(root, &libraries, &mut known_pci_slots, &mut warnings);
    accelerators.extend(accelerator::probe_gpus(
        root,
        &libraries,
        &mut known_pci_slots,
        &mut warnings,
    ));
    accelerator::warn_unmodelled_pci_accelerators(root, &known_pci_slots, &mut warnings);
    HardwareReport {
        cpu,
        memory,
        accelerators,
        warnings,
    }
}

/// 探测软件环境：**这台机器装了什么、配了什么**。
///
/// 与 [`probe`] 的区别是变化频率：硬件装了就不变，环境**装了/配了才变**。
/// 所以它可以缓存，但它不是机器本身的固有属性——不要拿它跨机器比较"谁更强"。
pub fn probe_environment(root: &Path) -> EnvironmentReport {
    let mut warnings = Vec::new();
    let mut report = environment::probe(root, &mut warnings);
    report.warnings = warnings;
    report
}

/// 采样一次运行时状态。
///
/// 注意 [`SampleOptions::counters`] **默认关**：NPU 的累积忙碌时间不宜频繁读取，
/// 驱动文档建议间隔不低于 1 秒。理由见那个字段的文档。
pub fn sample_state(options: &SampleOptions) -> RuntimeState {
    state::sample(Path::new("/"), options)
}

/// 与 [`sample_state`] 相同，但 `/proc` 与 `/sys` 部分可注入（测试用）。
///
/// `options.watch` 里的路径**不经过 `root`**：`statvfs` 查的是真实挂载的文件系统，
/// 对着假文件树问"这块盘还剩多少"没有意义。
pub fn sample_state_with(root: &Path, options: &SampleOptions) -> RuntimeState {
    state::sample(root, options)
}
