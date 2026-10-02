//! 给人看的文本渲染。
//!
//! 放在库里而不是各个 CLI 里：一份报告只有一种正确的读法，多份渲染实现迟早会分叉——
//! 上游加了字段，某一端的输出漏掉，而且没人会发现。

use crate::report::{AcceleratorKind, HardwareReport};
use std::fmt::Write;

/// 把报告渲染成多行文本（带末尾换行）。
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

    let mut mem = format!("内存     {}", human_bytes(report.memory.total_bytes));
    if let Some(available) = report.memory.available_bytes {
        let _ = write!(mem, "  （当前可用 {}）", human_bytes(available));
    }
    let _ = writeln!(out, "{mem}");

    if report.accelerators.is_empty() {
        let _ = writeln!(out, "加速器   未检测到 GPU / NPU，只能跑 CPU 推理");
    } else {
        for (index, accel) in report.accelerators.iter().enumerate() {
            let label = if index == 0 { "加速器   " } else { "         " };
            let mut line = format!(
                "{label}{:<3}  {}",
                kind_label(accel.kind),
                accel.name
            );
            if let Some(path) = &accel.device_path {
                let _ = write!(line, "  ({})", path.display());
            }
            if let Some(driver) = &accel.driver {
                let _ = write!(line, "  driver={driver}");
            }
            let _ = writeln!(out, "{line}");
            if !accel.notes.is_empty() {
                let _ = writeln!(out, "           ! {}", accel.notes.join("; "));
            }
        }
    }

    for warning in &report.warnings {
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
}
