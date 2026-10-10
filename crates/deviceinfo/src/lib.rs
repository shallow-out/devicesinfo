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
//! # 统一公开接口
//!
//! `inspect_*` 用于低频身份/能力探测，`observe_*` 用于按需遥测。
//! 所有入口返回 [`Snapshot`]，保留采样窗口、`boot_id`、namespace 和设备实例。
//! CPU 差分使用 [`Snapshot::cpu_usage_since`]，设备计数使用
//! [`Snapshot::counter_since`]；缺少来源或跨重启时返回 [`DifferenceError`]。
//! v0.2 不导出旧的裸探测/采样入口，也不提供兼容别名。
//!
//! 旧入口无法调用：
//!
//! ```compile_fail
//! deviceinfo::probe(std::path::Path::new("/"));
//! ```
//! ```compile_fail
//! deviceinfo::probe_with(std::path::Path::new("/"), "aarch64");
//! ```
//! ```compile_fail
//! deviceinfo::sample_state(&deviceinfo::SampleOptions::default());
//! ```
//! ```compile_fail
//! deviceinfo::probe_environment(std::path::Path::new("/"));
//! ```
//! ```compile_fail
//! deviceinfo::system::sample_system_state_with(std::path::Path::new("/"), &deviceinfo::SystemSampleOptions::default());
//! ```
//! ```compile_fail
//! deviceinfo::thermal::sample_thermal(std::path::Path::new("/"));
//! ```
//! ```compile_fail
//! deviceinfo::read_mmc_health(std::path::Path::new("/dev/mmcblk0"));
//! ```
//! ```compile_fail
//! fn unsafe_difference(now: deviceinfo::CpuTimes, previous: deviceinfo::CpuTimes) {
//!     now.usage_since(&previous);
//! }
//! ```
//!
//! # 可注入的 root
//!
//! 所有探测都接受一个 `root` 前缀而不是写死 `/`，因此可以用**假文件树**做单元测试，
//! 也可以用来探测容器内的可见设备，或者事后对着真实机器的采样夹具做回归。
//! 真实调用传 `Path::new("/")`。
//!
//! ```
//! let report = deviceinfo::inspect_hardware(std::path::Path::new("/"), std::env::consts::ARCH);
//! println!("{}", deviceinfo::render::human(&report.data));
//! ```

mod api;
mod snapshot;
pub use api::*;
pub use snapshot::{
    BOOT_ID_INPUT, CONTEXT_FILE, CaptureMetadata, CounterDifference, DeviceAssociation,
    DifferenceError, ObservationOrigin, SCHEMA_VERSION, SampleContext, SampleStamp, Snapshot,
};

mod accelerator;
mod cpu;
pub mod diagnostics;
pub mod environment;
mod features;
pub mod live;
mod memory;
pub mod mmc_health;
mod os;
pub mod platform;
mod report;
mod runtime;
pub mod soc;
mod state;
pub mod storage;
pub mod storage_health;
mod sysfs;
pub mod system;
pub mod tags;
pub mod thermal;

pub mod pci;
pub mod render;

pub use cpu::{PER_CORE_INPUTS as CPU_PER_CORE_INPUTS, SHARED_INPUTS as CPU_SHARED_INPUTS};
pub use diagnostics::{Diagnostic, DiagnosticCode, DiagnosticOperation};
pub use environment::EnvironmentReport;
pub use live::{LiveOptions, LiveReport};
pub use mmc_health::{MmcExtCsdHealth, decode_mmc_ext_csd};
pub use platform::{DmiIdentity, Exposure, FirmwareObservation, IdentityValue, PlatformReport};
pub use report::{
    Accelerator, AcceleratorKind, AcceleratorMemory, CoreTier, CpuFeatureGroup, CpuInfo,
    HardwareReport, MemoryInfo, PciId, RuntimeStatus,
};
pub use runtime::{LIBRARY_DIRS, OPENCL_VENDOR_DIR, library_inputs as runtime_library_inputs};
pub use soc::{SocDevice, SocReport};
pub use state::{AcceleratorState, DiskUsage, MemoryState, RuntimeState, SampleOptions};
pub use storage::{BlockDevice, StorageInterface, StorageMount, StorageReport};
pub use storage_health::{
    MmcHealth, NvmeHealth, NvmeHealthError, NvmeSmartLog, StorageHealthError,
    StorageHealthErrorKind, StorageHealthOptions, StorageHealthReport, StorageHealthState,
    decode_nvme_smart_log,
};
pub use sysfs::resolve_path_in_root;
pub use system::{CpuTimes, SystemReport, SystemSampleOptions, SystemState};
pub use thermal::{
    CoolingDevice, FanSensor, HysteresisKind, PwmChannel, TemperatureOrigin, TemperatureSensor,
    TemperatureThreshold, TemperatureUnit, ThermalOptions, ThermalReport,
};

use std::collections::BTreeSet;
use std::path::Path;

/// Internal hardware parser; public callers retain the snapshot envelope.
///
/// 架构用**编译期常量**而不是读文件：本模块总是探测自己所在的机器，二进制架构就是主机架构，
/// 而 `/proc/cpuinfo` 里根本没有 arch 字段（x86 只给 `vendor_id`，arm64 什么都不给）。
/// 参数化只是为了测试夹具能伪造一台 aarch64 机器。
pub(crate) fn probe_with(root: &Path, arch: &str) -> HardwareReport {
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
/// 与 [`inspect_hardware`] 的区别是变化频率：硬件装了就不变，环境**装了/配了才变**。
/// 所以它可以缓存，但它不是机器本身的固有属性——不要拿它跨机器比较"谁更强"。
pub(crate) fn probe_environment(root: &Path) -> EnvironmentReport {
    let mut warnings = Vec::new();
    let mut report = environment::probe(root, &mut warnings);
    report.warnings = warnings;
    report
}
