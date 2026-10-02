//! 给人看的文本渲染。
//!
//! 放在库里而不是各个 CLI 里：一份报告只有一种正确的读法，多份渲染实现迟早会分叉——
//! 上游加了字段，某一端的输出漏掉，而且没人会发现。
//!
//! 硬件报告和运行时状态**分开渲染**：这两份数据的生命周期不同，一起打印会让人以为
//! "可用内存"和"CPU 型号"是同一类东西。

use crate::report::{AcceleratorKind, AcceleratorMemory, HardwareReport, RuntimeStatus};
use crate::state::RuntimeState;
use std::fmt::Write;

/// 把硬件报告渲染成多行文本（带末尾换行）。
pub fn human(report: &HardwareReport) -> String {
    let mut out = String::new();

    let cpu_model = report
        .cpu
        .model
        .clone()
        .unwrap_or_else(|| "未知 CPU".into());
    // 架构放在标题行：它是"这个模型能不能跑"的第一道闸
    let _ = writeln!(out, "CPU      {cpu_model}  [{}]", report.cpu.arch);

    let mut core_line = format!("         {} 逻辑核", report.cpu.logical_cores);
    if let Some(physical) = report.cpu.physical_cores {
        let _ = write!(core_line, " / {physical} 物理核");
    }
    if let Some(effective) = report.cpu.effective_cores() {
        // 混合架构下这个数才反映真实算力，和核数不是一回事
        let _ = write!(core_line, "  （等效算力 {effective:.1} 核）");
    }
    let _ = writeln!(out, "{core_line}");

    for tier in &report.cpu.core_tiers {
        let mut line = format!("         · {:>2} 核", tier.count());
        if let Some(freq) = tier.max_freq_mhz {
            let _ = write!(line, " @ {:.2} GHz", freq as f64 / 1000.0);
        }
        if let Some(capacity) = tier.capacity {
            let _ = write!(line, "  capacity {capacity:>4}");
        }
        let _ = write!(line, "  cpu{}", format_cpu_ids(&tier.cpus));
        let _ = writeln!(out, "{line}");
    }

    if !report.cpu.simd.is_empty() {
        let _ = writeln!(out, "         指令集: {}", report.cpu.simd.join(", "));
    }

    match report.memory.total_bytes {
        Some(bytes) => {
            let _ = writeln!(out, "内存     {}", human_bytes(bytes));
        }
        None => {
            let _ = writeln!(out, "内存     未知");
        }
    }

    if report.accelerators.is_empty() {
        let _ = writeln!(out, "加速器   未检测到 GPU / NPU，只能跑 CPU 推理");
    } else {
        for (index, accel) in report.accelerators.iter().enumerate() {
            let label = if index == 0 { "加速器   " } else { "         " };
            let mut line = format!("{label}{:<3}  {}", kind_label(accel.kind), accel.name);
            // PCI id 不靠名字承载：名字表可能缺失或过时，id 永远可查
            if let Some(pci_id) = &accel.pci_id {
                let _ = write!(line, "  ({})", pci_id.compact());
            }
            let _ = writeln!(out, "{line}");

            // 第二行：设备事实（是什么、绑了哪个驱动、内存什么语义、上限多少）
            let mut facts = Vec::new();
            if let Some(path) = &accel.device_path {
                facts.push(format!("设备 {}", path.display()));
            }
            match (&accel.driver, &accel.driver_version) {
                (Some(driver), Some(version)) => facts.push(format!("驱动 {driver} {version}")),
                (Some(driver), None) => facts.push(format!("驱动 {driver}")),
                (None, _) => facts.push("未绑定驱动".into()),
            }
            facts.push(match &accel.memory {
                AcceleratorMemory::Dedicated { bytes } => format!("显存 {}", human_bytes(*bytes)),
                AcceleratorMemory::SharedWithSystem => "内存 共享系统内存".into(),
                AcceleratorMemory::Unknown { reason } => format!("内存 未知（{reason}）"),
            });
            if let Some(freq) = accel.max_freq_mhz {
                facts.push(format!("频率上限 {freq} MHz"));
            }
            let _ = writeln!(out, "             {}", facts.join("   "));

            // 第三行：设备在 ≠ 能用
            let _ = writeln!(
                out,
                "             运行时 {}",
                match &accel.runtime {
                    RuntimeStatus::Ready { stack, .. } => format!("就绪 · {stack}"),
                    RuntimeStatus::Incomplete { stack, missing } => {
                        format!("缺件 · {stack}：缺 {}", missing.join("、"))
                    }
                    RuntimeStatus::Unknown { reason } => format!("未知 · {reason}"),
                }
            );

            if !accel.notes.is_empty() {
                let _ = writeln!(out, "             ! {}", accel.notes.join("; "));
            }
        }
    }

    for warning in &report.warnings {
        let _ = writeln!(out, "警告     {warning}");
    }

    out
}

/// 把运行时状态渲染成多行文本（带末尾换行）。
pub fn human_state(state: &RuntimeState) -> String {
    let mut out = String::new();

    let mut line = match state.memory.total_bytes {
        Some(total) => format!("内存     {} 总量", human_bytes(total)),
        None => "内存     总量未知".to_string(),
    };
    match state.memory.available_bytes {
        Some(available) => {
            let _ = write!(line, "   可用 {}", human_bytes(available));
        }
        None => line.push_str("   可用未知"),
    }
    let _ = writeln!(out, "{line}");

    match (state.memory.swap_total_bytes, state.memory.swap_free_bytes) {
        (Some(0), _) => {
            let _ = writeln!(out, "交换     无");
        }
        (Some(total), free) => {
            let free = free.map_or_else(|| "未知".into(), human_bytes);
            let marker = if state.memory.swap_exhausted() {
                "   ! 已基本用满"
            } else {
                ""
            };
            let _ = writeln!(out, "交换     {} 总量   空闲 {free}{marker}", human_bytes(total));
        }
        _ => {
            let _ = writeln!(out, "交换     未知");
        }
    }

    if state.memory.swap_exhausted() {
        // 这个提醒是状态报告存在的主要理由之一：swap 满时"可用内存"会系统性高估，
        // 而这在原始数字上看不出来
        let _ = writeln!(
            out,
            "         ! swap 用满时，系统的\"可用内存\"会高估还能装下多少，判断时要留余量"
        );
    }

    for disk in &state.disks {
        let _ = writeln!(
            out,
            "磁盘     {}   可写 {} / {}",
            disk.path.display(),
            human_bytes(disk.available_bytes),
            human_bytes(disk.total_bytes)
        );
    }
    if state.disks.is_empty() {
        let _ = writeln!(out, "磁盘     （未指定要查的路径）");
    }

    for (index, accel) in state.accelerators.iter().enumerate() {
        let label = if index == 0 { "加速器   " } else { "         " };
        let mut line = format!("{label}{:<3} ", kind_label(accel.kind));
        if let Some(pci_id) = &accel.pci_id {
            let _ = write!(line, "{}   ", pci_id.compact());
        }
        line.push_str(&match accel.current_freq_mhz {
            // 0 的含义是**设备空闲**，不是没读到——驱动文档：freq/current_freq
            // 只在设备活跃时有效。印成 "0 MHz" 会让人以为读数坏了。
            Some(0) => "空闲".to_string(),
            Some(freq) => format!("{freq} MHz"),
            None => "频率未知".to_string(),
        });
        if let Some(bytes) = accel.resident_memory_bytes {
            let _ = write!(line, "   常驻内存 {}", human_bytes(bytes));
        }
        if let Some(micros) = accel.busy_time_us {
            let _ = write!(line, "   累积忙碌 {}", human_duration_us(micros));
        }
        let _ = writeln!(out, "{line}");
    }

    for warning in &state.warnings {
        let _ = writeln!(out, "警告     {warning}");
    }

    out
}

fn kind_label(kind: AcceleratorKind) -> &'static str {
    match kind {
        AcceleratorKind::Cpu => "CPU",
        AcceleratorKind::Gpu => "GPU",
        AcceleratorKind::Npu => "NPU",
    }
}

/// 微秒 → 人可读时长。累积忙碌时间动辄上亿微秒，直接印数字没人看得懂。
fn human_duration_us(micros: u64) -> String {
    let seconds = micros / 1_000_000;
    let (hours, minutes, secs) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m{secs}s")
    } else {
        format!("{secs}s")
    }
}

/// 人类可读的字节数。二进制单位（KiB 而不是 KB）——内存和权重都是按 1024 算的。
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 把 `[0, 1, 4, 5, 6, 7]` 压成 `0-1,4-7`。核一多，逐个列出来就没法看了。
pub fn format_cpu_ids(cpus: &[usize]) -> String {
    let mut parts = Vec::new();
    let mut index = 0;
    while index < cpus.len() {
        let start = cpus[index];
        let mut end = start;
        while index + 1 < cpus.len() && cpus[index + 1] == end + 1 {
            index += 1;
            end = cpus[index];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        index += 1;
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CoreTier, CpuInfo, MemoryInfo, PciId};

    #[test]
    fn cpu_ids_are_compressed_into_ranges() {
        assert_eq!(format_cpu_ids(&[]), "");
        assert_eq!(format_cpu_ids(&[0]), "0");
        assert_eq!(format_cpu_ids(&[0, 1]), "0-1");
        assert_eq!(format_cpu_ids(&[0, 1, 4, 5, 6, 7]), "0-1,4-7");
        // 不连续也不丢项
        assert_eq!(format_cpu_ids(&[0, 2, 4]), "0,2,4");
        assert_eq!(format_cpu_ids(&[0, 1, 2, 5, 9, 10]), "0-2,5,9-10");
    }

    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(33_129_861_120), "30.9 GiB");
    }

    #[test]
    fn unknown_accelerator_states_are_visible_in_the_output() {
        let report = HardwareReport {
            cpu: CpuInfo {
                arch: "x86_64".into(),
                model: Some("Intel(R) Core(TM) Ultra 7 258V".into()),
                logical_cores: 8,
                physical_cores: Some(8),
                core_tiers: vec![CoreTier {
                    cpus: (0..8).collect(),
                    max_freq_mhz: Some(4800),
                    capacity: Some(1024),
                }],
                simd: vec!["avx_vnni".into()],
            },
            memory: MemoryInfo {
                total_bytes: Some(33_129_861_120),
            },
            accelerators: vec![
                crate::Accelerator {
                    kind: AcceleratorKind::Gpu,
                    name: "NVIDIA GA102 [GeForce RTX 3090]".into(),
                    device_path: Some("/dev/dri/renderD128".into()),
                    driver: Some("nvidia".into()),
                    driver_version: None,
                    vendor: Some("NVIDIA".into()),
                    pci_id: Some(PciId {
                        vendor: "0x10de".into(),
                        device: "0x2204".into(),
                    }),
                    memory: AcceleratorMemory::Unknown {
                        reason: "NVIDIA 驱动未通过 sysfs 暴露显存上限".into(),
                    },
                    max_freq_mhz: None,
                    runtime: RuntimeStatus::Unknown {
                        reason: "还没有 NVIDIA 的用户态运行时判据".into(),
                    },
                    notes: Vec::new(),
                },
                crate::Accelerator {
                    kind: AcceleratorKind::Npu,
                    name: "Intel Core Ultra 200V Series Processors NPU".into(),
                    device_path: Some("/dev/accel/accel0".into()),
                    driver: Some("intel_vpu".into()),
                    driver_version: Some("1.0.0".into()),
                    vendor: Some("Intel".into()),
                    pci_id: None,
                    memory: AcceleratorMemory::SharedWithSystem,
                    max_freq_mhz: Some(1900),
                    runtime: RuntimeStatus::Incomplete {
                        stack: "OpenVINO NPU".into(),
                        missing: vec!["libopenvino_intel_npu_compiler_loader.so".into()],
                    },
                    notes: Vec::new(),
                },
            ],
            warnings: Vec::new(),
        };

        let text = human(&report);
        // 架构和等效算力在
        assert!(text.contains("[x86_64]"), "{text}");
        assert!(text.contains("等效算力 8.0 核"), "{text}");
        // 显存"未知"要带原因，不能只说未知
        assert!(
            text.contains("内存 未知（NVIDIA 驱动未通过 sysfs 暴露显存上限）"),
            "{text}"
        );
        // 运行时缺件要列出可以照抄去装的东西
        assert!(
            text.contains("运行时 缺件 · OpenVINO NPU：缺 libopenvino_intel_npu_compiler_loader.so"),
            "{text}"
        );
        assert!(text.contains("(10de:2204)"), "{text}");
        assert!(text.contains("共享系统内存"), "{text}");
        assert!(text.contains("频率上限 1900 MHz"), "{text}");
        // 硬件报告里不该再出现"可用内存"——那是运行时状态
        assert!(!text.contains("可用"), "硬件报告不该报瞬时值: {text}");
    }

    #[test]
    fn exhausted_swap_is_called_out() {
        let state = RuntimeState {
            memory: crate::MemoryState {
                total_bytes: Some(33_129_861_120),
                available_bytes: Some(9_945_219_072),
                swap_total_bytes: Some(4_294_963_200),
                swap_free_bytes: Some(1_552_384),
            },
            disks: Vec::new(),
            accelerators: Vec::new(),
            warnings: Vec::new(),
        };
        let text = human_state(&state);
        assert!(text.contains("已基本用满"), "{text}");
        assert!(text.contains("会高估"), "必须给出为什么重要: {text}");
        assert!(text.contains("未指定要查的路径"), "{text}");

        // 没有 swap 的机器不该出现任何"用满"的提示
        let no_swap = RuntimeState {
            memory: crate::MemoryState {
                swap_total_bytes: Some(0),
                swap_free_bytes: Some(0),
                ..state.memory
            },
            disks: Vec::new(),
            accelerators: Vec::new(),
            warnings: Vec::new(),
        };
        let text = human_state(&no_swap);
        assert!(text.contains("交换     无"), "{text}");
        assert!(!text.contains("用满"), "{text}");
    }

    #[test]
    fn a_zero_frequency_renders_as_idle_not_as_a_broken_reading() {
        let state = RuntimeState {
            memory: crate::MemoryState {
                total_bytes: None,
                available_bytes: None,
                swap_total_bytes: None,
                swap_free_bytes: None,
            },
            disks: Vec::new(),
            accelerators: vec![
                crate::AcceleratorState {
                    kind: AcceleratorKind::Npu,
                    pci_id: Some(PciId {
                        vendor: "0x8086".into(),
                        device: "0x643e".into(),
                    }),
                    // 驱动在设备空闲时报 0，而不是不报
                    current_freq_mhz: Some(0),
                    resident_memory_bytes: Some(68_714_496),
                    busy_time_us: Some(88_694_865),
                },
                crate::AcceleratorState {
                    kind: AcceleratorKind::Gpu,
                    pci_id: None,
                    current_freq_mhz: Some(967),
                    resident_memory_bytes: None,
                    busy_time_us: None,
                },
            ],
            warnings: Vec::new(),
        };

        let text = human_state(&state);
        assert!(text.contains("空闲"), "0 频率应当显示为空闲: {text}");
        assert!(!text.contains("0 MHz"), "不能把空闲印成一个读数: {text}");
        assert!(text.contains("967 MHz"), "{text}");
        assert!(text.contains("常驻内存 65.5 MiB"), "{text}");
        assert!(text.contains("累积忙碌 1m28s"), "{text}");
    }

    #[test]
    fn durations_are_human_readable() {
        assert_eq!(human_duration_us(0), "0s");
        assert_eq!(human_duration_us(88_694_865), "1m28s");
        assert_eq!(human_duration_us(3_600_000_000), "1h0m");
        assert_eq!(human_duration_us(90_000_000_000), "25h0m");
    }
}
