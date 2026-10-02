//! 对外数据结构。这一层**不做任何 IO**——凡是能从文件系统读出来的东西，
//! 都写成"接收 root 路径的纯函数"放在各自的探测模块里，方便测试。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AcceleratorKind {
    Cpu,
    Gpu,
    Npu,
}

/// 一个加速设备。
///
/// 名字起得保守是因为这个结构还要能**接住后端 probe 回来的设备**：推理后端自己探测到的
/// 设备信息会以同样的形状合并进报告，所以字段要能表达"不完整"（见 `notes`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accelerator {
    pub kind: AcceleratorKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// 观测到的、影响可用性的备注（缺驱动、设备节点不存在等）。
    ///
    /// 注意语义边界：**这里只记"设备本身"的问题**。"设备在但用户态栈不全"（比如插着 NPU
    /// 但没装编译器）属于能力判定，不是设备事实，不写在这里。
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuInfo {
    /// 目标架构（`x86_64` / `aarch64`）。这是"能不能跑"最硬的过滤条件：
    /// 一段带 AVX-512 kernel 的 GGUF 在 ARM 上根本跑不了。
    pub arch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub logical_cores: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_cores: Option<usize>,
    /// 按相对性能分组的核，强的在前。
    ///
    /// 混合架构（Intel 的 P+E、ARM 的 big.LITTLE）下**这一项才是机器的真实形状**：
    /// 只报一个总核数会高估算力——实测某台 Lunar Lake 的 8 核里有 4 个
    /// `cpu_capacity` 只有 676/1024。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub core_tiers: Vec<CoreTier>,
    /// 观测到的加速指令集，按族过滤（见 `features::is_relevant_feature`）。
    #[serde(default)]
    pub simd: Vec<String>,
}

impl CpuInfo {
    /// 等效核数：各核 `cpu_capacity` 之和 ÷ 1024。
    ///
    /// 同构机器上等于核数；混合架构上**小于**核数（实测 8 核的 Lunar Lake ≈ 6.6）。
    /// 估算吞吐时按容量折算，是不系统性高估的唯一做法。
    ///
    /// 部分核没有 `cpu_capacity` 时返回 `None`——宁可没有数字，也不要一个偏低的数字。
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
        let total: u64 = self
            .core_tiers
            .iter()
            .filter_map(|tier| tier.capacity.map(|capacity| capacity * tier.count() as u64))
            .sum();
        Some(total as f64 / 1024.0)
    }
}

/// 一组性能相同的核。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreTier {
    /// 属于这一组的逻辑核编号（`/sys/devices/system/cpu/cpuN` 的 N）。
    /// 存编号而不只是计数：调度要真把活钉在强核上，光知道"有两个"没用。
    pub cpus: Vec<usize>,
    /// 组内单核最大频率（MHz）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_freq_mhz: Option<u64>,
    /// sysfs `cpu_capacity`，最强核为 1024。跨架构唯一可靠的相对性能来源。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u64>,
}

impl CoreTier {
    pub fn count(&self) -> usize {
        self.cpus.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    /// `/proc/meminfo` 的 `MemAvailable`。**这是瞬时值**，不是硬件事实——
    /// 缓存/比较它的时候要记住这一点。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
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
    pub warnings: Vec<String>,
}

impl HardwareReport {
    /// 可用于加载模型的内存（保守取可用内存，取不到就退回总量）。
    pub fn usable_memory_bytes(&self) -> u64 {
        self.memory
            .available_bytes
            .unwrap_or(self.memory.total_bytes)
    }

    /// 是否存在 NPU 设备。
    ///
    /// **注意这只回答"设备在不在"，不回答"能不能用"**——设备节点存在但用户态编译器
    /// 缺失时这里同样返回 `true`。能力判定是另一层的事。
    pub fn has_npu(&self) -> bool {
        self.accelerators
            .iter()
            .any(|accel| accel.kind == AcceleratorKind::Npu)
    }

    /// 是否存在 GPU 设备。语义边界同 [`Self::has_npu`]。
    pub fn has_gpu(&self) -> bool {
        self.accelerators
            .iter()
            .any(|accel| accel.kind == AcceleratorKind::Gpu)
    }
}
