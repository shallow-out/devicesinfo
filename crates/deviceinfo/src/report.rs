//! 对外数据结构。这一层**不做任何 IO**——凡是能从文件系统读出来的东西，
//! 都写成"接收 root 路径的纯函数"放在各自的探测模块里，方便测试。
//!
//! 字段的取舍遵循一条线：**只放硬件事实，不放运行时状态**。
//! 频率上限是事实，当前频率不是；内存总量是事实，可用内存不是（那个虽然暂时留在这里，
//! 但已经标好了语义）。混在一起会让这份报告既不能缓存，也不能跨设备比较。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AcceleratorKind {
    Cpu,
    Gpu,
    Npu,
    /// 只能做显示输出的 DRM 设备。
    ///
    /// 判据是**结构性的**：DRM 的 render 节点就是给渲染/计算用的（`DRIVER_RENDER`），
    /// 纯显示控制器拿不到。实测两台 ARM 机器上都有（`linlondp` ×3、`rockchip-drm`）——
    /// 以前它们被报成 GPU，于是 [`HardwareReport::has_gpu`] 在一台只有显示控制器的
    /// 机器上也会返回 `true`。
    ///
    /// 它们仍然留在 [`HardwareReport::accelerators`] 里："这台机器有几个 DRM 设备、
    /// 分别是什么"本身是值得知道的事实。只是**不参与"能不能跑模型"的判断**。
    Display,
}

/// PCI 标识。
///
/// 名字表（`pci.ids`）可能缺失、过时，或者干脆查不到——**原始 id 永远可查**，
/// 所以两者分开留字段，而不是把 id 拼进名字字符串里。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PciId {
    /// 形如 `0x8086`。
    pub vendor: String,
    /// 形如 `0x643e`。
    pub device: String,
}

impl PciId {
    /// `8086:643e`——搜索和贴到 issue 里用这个写法。
    pub fn compact(&self) -> String {
        format!(
            "{}:{}",
            self.vendor.trim_start_matches("0x").trim_start_matches("0X"),
            self.device.trim_start_matches("0x").trim_start_matches("0X")
        )
    }
}

/// 加速器可用的内存语义。
///
/// 这里曾经是个 `Option<u64>`，但 `None` 同时表示了两件完全不同的事：
/// **"和系统内存共享，没有独立上限"**（集显、NPU）和**"读不到"**（NVIDIA 专有驱动
/// 不通过 sysfs 暴露显存）。上层拿到 `None` 只能猜，而两个方向猜错都是错的：
/// 保守一点会禁掉能跑的模型，乐观一点会让人装一个装不下的模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum AcceleratorMemory {
    /// 独立显存，有确定容量。
    Dedicated { bytes: u64 },
    /// 与系统内存共享，没有独立上限（集显、NPU）。
    SharedWithSystem,
    /// 读不到，附上原因。
    Unknown { reason: String },
}

impl AcceleratorMemory {
    /// 独立显存的字节数；共享或未知时返回 `None`（**不是 0**）。
    pub fn dedicated_bytes(&self) -> Option<u64> {
        match self {
            Self::Dedicated { bytes } => Some(*bytes),
            _ => None,
        }
    }
}

/// 用户态运行时是否就绪。
///
/// **"设备在"不等于"能用"。** 驱动认了硬件，不代表应用能拿它跑推理：
/// Intel NPU 还需要编译器（由独立的 `intel-npu-compiler` 包提供，不随驱动一起装），
/// Intel GPU 还需要 Level Zero 或 OpenCL 运行时。缺这一层时，模型会被选中、
/// 然后在使用时失败——这是最容易让用户踩坑的一类误报。
///
/// 判据是**库文件是否存在**，这是个启发式（本 crate 不链接任何推理栈，没法真的试跑一次）。
/// 所以认不出的组合一律返回 [`RuntimeStatus::Unknown`]，而不是猜一个"就绪"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum RuntimeStatus {
    /// 已知栈的组成部分齐了。`components` 是实际命中的文件名，便于核对。
    Ready {
        stack: String,
        components: Vec<String>,
    },
    /// 已知栈缺件。`missing` 是可以照抄去装的东西。
    Incomplete { stack: String, missing: Vec<String> },
    /// 这个设备根本不涉及推理运行时（例如只能显示输出的 DRM 设备）。
    ///
    /// 和 [`RuntimeStatus::Unknown`] 分开：那是"不知道该找什么"，这是**知道不用找**。
    /// 混成一个值就丢了"这个设备能不能用来算"与"能不能判断"的区别。
    NotApplicable { reason: String },
    /// 认不出这个组合该找什么——不猜。
    Unknown { reason: String },
}

impl RuntimeStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

/// 一个加速设备。
///
/// 名字起得保守是因为这个结构还要能**接住后端 probe 回来的设备**：推理后端自己探测到的
/// 设备信息会以同样的形状合并进报告，所以字段要能表达"不完整"（见 `notes`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accelerator {
    /// Kernel class path used to join inventory, telemetry and snapshot.devices.
    /// PCI vendor:product identifies a model, never a particular device instance.
    pub source: PathBuf,
    pub kind: AcceleratorKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// 内核驱动版本。
    ///
    /// 读 `/sys/module/<驱动>/version`。**NPU 会报**（本机 `intel_vpu` → `1.0.0`），
    /// 而 `xe`/`i915`/`amdgpu` 都没有这个文件，此时是 `None`——那表示"驱动没暴露"，
    /// 不表示"没版本"。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// PCI 标识。GPU 一定有；NPU 也挂在 PCI 上，所以一般也有。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_id: Option<PciId>,
    /// 设备树 `compatible` 字符串（如 `rockchip,rk3588-rknpu`）。
    ///
    /// **非 PCI 平台（ARM / 嵌入式）上这才是设备的权威标识**：那边既没有 PCI id，
    /// 也没有 `cardN` 以外的名字。少了它，一台 ARM 机器上的加速器就只能叫
    /// "GPU (card0, id 未知)"——信息量等于零。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatible: Option<String>,
    /// 可用内存的语义（独立 / 共享 / 未知）。
    pub memory: AcceleratorMemory,
    /// 频率上限（MHz）。**这是硬件事实，不是瞬时值**——
    /// `cur_freq` / `act_freq` 那一类不进报告。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_freq_mhz: Option<u64>,
    /// 硬件支持的最低频率（MHz），目前由 Intel NPU 的 freq/hw_min_freq 暴露。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_freq_mhz: Option<u64>,
    /// 硬件自报的能效频率（MHz）；不代表某个具体模型的最优频率。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efficient_freq_mhz: Option<u64>,
    /// 驱动自报的调度模式，如 Intel NPU 的 HW / OS，原样保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduling_mode: Option<String>,
    /// 用户态运行时是否就绪。
    pub runtime: RuntimeStatus,
    /// 观测到的、影响可用性的备注（缺驱动、设备节点不存在等）。
    ///
    /// 注意语义边界：**这里只记"设备本身"的问题**。"设备在但用户态栈不全"由
    /// [`Self::runtime`] 表达；"探测不到"由 [`HardwareReport::warnings`] 表达。
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuInfo {
    /// 目标架构（`x86_64` / `aarch64`）。这是"能不能跑"最硬的过滤条件：
    /// 一段带 AVX-512 kernel 的 GGUF 在 ARM 上根本跑不了。
    pub arch: String,
    /// **处理器**型号（cpuinfo 的 `model name`）。
    ///
    /// arm64 上内核未必报它——取决于厂商内核，实测 Rockchip 那台一行都没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    /// **整机**型号。来源按固件接口：设备树 `model`，或 DMI `product_name`。
    ///
    /// 和 [`Self::cpu_model`] 分开，因为它们是**两件不同的事实**：x86 上两者都有
    /// （`Intel(R) Core(TM) Ultra 7 258V` vs `83LC`），而以前挤在一个字段里、
    /// 含义还随架构变化——ARM 上那个字段装的其实是整机型号，上层根本没法统一处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_model: Option<String>,
    pub logical_cores: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_cores: Option<usize>,
    /// 按相对性能分组的核，强的在前。
    ///
    /// 混合架构（Intel 的 P+E、ARM 的 big.LITTLE）下**这一项才是机器的真实形状**：
    /// 只报一个总核数会高估算力——实测某台 Lunar Lake 的 8 核里有 4 个
    /// `cpu_capacity` 只有 676/1024，而另一台 i9-12900H 的 20 个逻辑核其实只有 14 个
    /// 物理核、还是 6 P + 8 E。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub core_tiers: Vec<CoreTier>,
    /// 内核报告的全部指令集特征的并集，**原样**（排序去重）。
    /// 并集中的特征可能只在部分核上出现；全核任务应查 common_features。
    ///
    /// 刻意不做筛选：消费方问的是开放式问题（"有没有 AMX"、"有没有 SVE2"、
    /// "有没有 FP8"），生产者一旦筛掉信息就永久丢了，而且丢失是无声的。
    /// 早先按前缀筛选的两个方向都错过：x86 的 `smep` 混进来（假阳性），
    /// arm64 的 `asimddp` 因为名字猜错而落空。具体理由见 `features` 模块。
    #[serde(default)]
    pub features: Vec<String>,
    /// 所有被探测逻辑核的公共特征。部分核缺记录时未知；Some([]) 表示交集为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub common_features: Option<Vec<String>>,
    /// 按内核报告的特征集合分组，不等同于按性能分组。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feature_groups: Vec<CpuFeatureGroup>,
}

/// 内核为这些逻辑核报告了相同的特征集合。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuFeatureGroup {
    pub cpus: Vec<usize>,
    /// 排序去重的原始特征；空列表仍是一条明确的观测。
    pub features: Vec<String>,
}

impl CpuInfo {
    /// 指定核集合的公共特征。任一核缺记录或归属重复时返回 None，不退回全机并集。
    /// 空的核集合也返回 None。该查询不改变任务亲和性。
    pub fn features_for_cpus(&self, cpus: &[usize]) -> Option<Vec<String>> {
        let cpus: std::collections::BTreeSet<_> = cpus.iter().copied().collect();
        if cpus.is_empty() {
            return None;
        }
        let mut common: Option<std::collections::BTreeSet<String>> = None;
        for cpu in cpus {
            let mut matches = self.feature_groups.iter()
                .filter(|group| group.cpus.contains(&cpu));
            let group = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            let features = group.features.iter().cloned().collect();
            common = Some(match common {
                None => features,
                Some(previous) => previous.intersection(&features).cloned().collect(),
            });
        }
        common.map(|features| features.into_iter().collect())
    }

    /// 等效核数：各**物理核** `cpu_capacity` 之和 ÷ 1024。
    ///
    /// 同构机器上等于物理核数；混合架构上**小于**核数（实测 8 核的 Lunar Lake ≈ 6.6）。
    ///
    /// ⚠️ **按物理核算，不按逻辑核。** SMT 的两个线程不等于两个核：实测那台
    /// i9-12900H（6 P + 8 E，20 线程）按逻辑核求和会给 20.0，而按物理核算约 10.7。
    /// 每档各有多少物理核记在 [`CoreTier::physical_cores`]。
    ///
    /// 部分核缺少相对性能或物理核拓扑时返回 `None`。
    pub fn effective_cores(&self) -> Option<f64> {
        let covered: usize = self
            .core_tiers
            .iter()
            .filter(|tier| tier.capacity.is_some())
            .map(CoreTier::count)
            .sum();
        if covered == 0 || covered != self.logical_cores {
            return None;
        }
        let mut total = 0_u64;
        for tier in &self.core_tiers {
            total += tier.capacity? * tier.physical_cores? as u64;
        }
        Some(total as f64 / 1024.0)
    }
}

/// 一组性能相同的核。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreTier {
    /// 属于这一组的逻辑核编号（`/sys/devices/system/cpu/cpuN` 的 N）。
    /// 存编号而不只是计数：调度要真把活钉在强核上，光知道"有两个"没用。
    pub cpus: Vec<usize>,
    /// 这一组里有几个**物理核**（SMT 的线程已经算成一个）。
    ///
    /// 与 `cpus.len()` 的区别在 SMT 机器上才看得出来：实测那台 i9-12900H 上
    /// 有两档是「2 物理核 / 4 线程」和「4 物理核 / 8 线程」。
    /// 拓扑读不全且不能确认 SMT 已关闭时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_cores: Option<usize>,
    /// 组内单核最大频率（MHz）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_freq_mhz: Option<u64>,
    /// 相对性能，最强核为 1024。
    ///
    /// 优先由 `acpi_cppc/highest_perf` 归一化而来，退化到内核的 `cpu_capacity`
    /// ——理由见 `cpu::observe_cores` 的注释。
    ///
    /// **只有同机内的比率有意义**，不同机器的值不可比。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u64>,
}

impl CoreTier {
    /// 这一组的**逻辑核**数（线程数）。
    pub fn count(&self) -> usize {
        self.cpus.len()
    }
}

/// 内存的**硬件事实**：只有总量。
///
/// 可用内存和 swap 在 [`crate::state::MemoryState`]——那些是瞬时值。
/// 总量也用 `Option`：读不到就是 `None`，**不是 0**。报 0 会让上层算出
/// "一字节都装不下"，进而拒绝掉本来能跑的模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareReport {
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub accelerators: Vec<Accelerator>,
    /// 探测过程中的不确定之处。
    ///
    /// 这份报告的用途是"避免选错"，所以**"探测不到"必须显式暴露**，
    /// 而不是假装设备不存在、或者悄悄用一个默认值把洞填上。
    ///
    /// 注意这不含 [`RuntimeStatus::Incomplete`]：那是**确定的观测**（确定缺件），
    /// 不是不确定。两者混在一张列表里，会让"未知"这件事失去信号。
    pub warnings: Vec<String>,
}

impl HardwareReport {
    /// 是否存在 NPU 设备。
    ///
    /// **只回答"设备在不在"**。设备节点存在但用户态栈不全时它同样返回 `true`——
    /// 要问"能不能真的用"，看那个设备的 [`Accelerator::runtime`]。
    pub fn has_npu(&self) -> bool {
        self.accelerators
            .iter()
            .any(|accel| accel.kind == AcceleratorKind::Npu)
    }

    /// 是否存在 GPU 设备。语义边界同 [`Self::has_npu`]。
    ///
    /// **只有 [`AcceleratorKind::Gpu`] 算数。** 只能做显示输出的 DRM 设备是
    /// [`AcceleratorKind::Display`]，它们跑不了模型。
    pub fn has_gpu(&self) -> bool {
        self.accelerators
            .iter()
            .any(|accel| accel.kind == AcceleratorKind::Gpu)
    }

    /// 设备在**且**用户态运行时已就绪——这才是"能拿它跑模型"。
    pub fn has_usable_npu(&self) -> bool {
        self.accelerators
            .iter()
            .any(|accel| accel.kind == AcceleratorKind::Npu && accel.runtime.is_ready())
    }

    /// 同 [`Self::has_usable_npu`]，针对 GPU。
    pub fn has_usable_gpu(&self) -> bool {
        self.accelerators
            .iter()
            .any(|accel| accel.kind == AcceleratorKind::Gpu && accel.runtime.is_ready())
    }
}
