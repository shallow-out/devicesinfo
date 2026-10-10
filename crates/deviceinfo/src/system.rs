//! Lightweight, read-only inputs for device dashboards and SBC control tools.
//!
//! Cache [`SystemReport`] at a low frequency; sample [`SystemState`] on demand.
//! These entry points never enumerate accelerators, scan runtime libraries, execute
//! commands, access the network or sleep. Use the existing hardware/environment
//! entry points separately when their more extensive observations are needed.

use crate::{CpuInfo, DiskUsage, MemoryInfo, MemoryState, environment::OperatingSystem};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Additional inputs for capture tools, beyond the existing CPU/OS input lists.
/// Keep captured roots usable by both lightweight entry points.
pub const INPUTS: [&str; 5] = [
    "proc/sys/kernel/osrelease",
    "proc/sys/kernel/hostname",
    "proc/stat",
    "proc/loadavg",
    "proc/uptime",
];

/// CPU topology, memory capacity and OS identity. Unknown values remain explicit.
/// Kernel/hostname changes require the consumer to invalidate its cached report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemReport {
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    #[serde(default)]
    pub soc: crate::SocReport,
    pub operating_system: Option<OperatingSystem>,
    pub kernel_release: Option<String>,
    pub hostname: Option<String>,
    pub warnings: Vec<String>,
}

/// Inspect the local Linux-visible system, without the accelerator/runtime scan.
pub fn probe_system(root: &Path) -> SystemReport {
    probe_system_with(root, std::env::consts::ARCH)
}

/// As [`probe_system`], with an explicit architecture for fixtures/captured roots.
pub fn probe_system_with(root: &Path, arch: &str) -> SystemReport {
    let mut warnings = Vec::new();
    let cpu = crate::cpu::probe(root, arch, &mut warnings);
    let memory = crate::memory::probe(root, &mut warnings);
    let operating_system = crate::os::probe(root, &mut warnings);
    if operating_system.is_none()
        && !root.join("etc/os-release").exists()
        && !root.join("usr/lib/os-release").exists()
    {
        warnings.push("OS identity unavailable: no readable os-release".into());
    }
    let kernel_release = read_text(root, "proc/sys/kernel/osrelease", &mut warnings);
    let hostname = read_text(root, "proc/sys/kernel/hostname", &mut warnings);
    let soc = crate::probe_soc(root);
    SystemReport {
        cpu,
        memory,
        soc,
        operating_system,
        kernel_release,
        hostname,
        warnings,
    }
}

/// Aggregate CPU ticks from `/proc/stat`, in USER_HZ units.
/// Guest ticks are already included in user/nice and must not be counted twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    /// Percentage of non-idle CPU time between two observations (0..=100).
    /// The first sample, zero elapsed ticks, resets or decreasing counters are
    /// unknown. This is CPU utilization, not load average divided by CPU count.
    /// Linux iowait is not reliable and can decrease; that interval is unknown too.
    pub fn usage_since(&self, previous: &Self) -> Option<f64> {
        let values = |c: &Self| {
            [
                c.user, c.nice, c.system, c.idle, c.iowait, c.irq, c.softirq, c.steal,
            ]
        };
        let current = values(self);
        let old = values(previous);
        let mut delta = [0; 8];
        for (index, (new, old)) in current.into_iter().zip(old).enumerate() {
            delta[index] = new.checked_sub(old)?;
        }
        let total = delta
            .iter()
            .try_fold(0u64, |sum, value| sum.checked_add(*value))?;
        if total == 0 {
            return None;
        }
        let idle = delta[3].checked_add(delta[4])?;
        Some(total.checked_sub(idle)? as f64 / total as f64 * 100.0)
    }
}

/// Disk paths are real host paths, even when a fixture root is injected.
/// This matches [`crate::SampleOptions::watch`]; `statvfs` never queries a
/// fixture's filesystem as if it were the captured machine's disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemSampleOptions {
    pub watch: Vec<PathBuf>,
}

/// A cheap observation of changing state. No implicit delay or cached counters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemState {
    pub memory: MemoryState,
    pub cpu: Option<CpuTimes>,
    pub load_average: Option<[f64; 3]>,
    pub uptime_seconds: Option<f64>,
    pub disks: Vec<DiskUsage>,
    pub warnings: Vec<String>,
}

pub fn sample_system_state(options: &SystemSampleOptions) -> SystemState {
    sample_system_state_with(Path::new("/"), options)
}

/// Inject `/proc` for deterministic testing; watched disks still belong to the host.
pub fn sample_system_state_with(root: &Path, options: &SystemSampleOptions) -> SystemState {
    let mut warnings = Vec::new();
    let memory = crate::state::sample_memory(root, &mut warnings);
    let cpu = read_parsed(root, "proc/stat", parse_cpu_times, &mut warnings);
    let load_average = read_parsed(root, "proc/loadavg", parse_load, &mut warnings);
    let uptime_seconds = read_parsed(
        root,
        "proc/uptime",
        |text| finite_nonnegative(text.split_whitespace().next()?),
        &mut warnings,
    );
    let mut disks = Vec::new();
    for path in &options.watch {
        match crate::state::filesystem_usage(path) {
            Ok(usage) => disks.push(usage),
            Err(error) => warnings.push(format!(
                "Cannot sample filesystem {}: {error}",
                path.display()
            )),
        }
    }
    SystemState {
        memory,
        cpu,
        load_average,
        uptime_seconds,
        disks,
        warnings,
    }
}

fn read_text(root: &Path, relative: &str, warnings: &mut Vec<String>) -> Option<String> {
    match crate::resolve_path_in_root(root, relative).and_then(fs::read_to_string) {
        Ok(text) if !text.trim().is_empty() => Some(text.trim().into()),
        Ok(_) => {
            warnings.push(format!("Empty /{relative}"));
            None
        }
        Err(error) => {
            warnings.push(format!("Cannot read /{relative}: {error}"));
            None
        }
    }
}

fn read_parsed<T>(
    root: &Path,
    relative: &str,
    parse: impl FnOnce(&str) -> Option<T>,
    warnings: &mut Vec<String>,
) -> Option<T> {
    let text = read_text(root, relative, warnings)?;
    let value = parse(&text);
    if value.is_none() {
        warnings.push(format!("Invalid /{relative}"));
    }
    value
}

fn parse_cpu_times(text: &str) -> Option<CpuTimes> {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some("cpu"))?;
    let ticks = line
        .split_whitespace()
        .skip(1)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if ticks.len() < 4 {
        return None;
    }
    let at = |index| ticks.get(index).copied().unwrap_or(0);
    Some(CpuTimes {
        user: at(0),
        nice: at(1),
        system: at(2),
        idle: at(3),
        iowait: at(4),
        irq: at(5),
        softirq: at(6),
        steal: at(7),
    })
}

fn finite_nonnegative(text: &str) -> Option<f64> {
    let value: f64 = text.parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn parse_load(text: &str) -> Option<[f64; 3]> {
    let mut fields = text.split_whitespace();
    Some([
        finite_nonnegative(fields.next()?)?,
        finite_nonnegative(fields.next()?)?,
        finite_nonnegative(fields.next()?)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utilization_excludes_guest_time_and_requires_a_valid_interval() {
        let before = parse_cpu_times("cpu 100 20 30 400 50 2 3 4 80 10\ncpu0 1 2 3 4").unwrap();
        let after = parse_cpu_times("cpu 120 20 40 430 60 2 3 4 100 10").unwrap();
        assert!((after.usage_since(&before).unwrap() - 100.0 * 30.0 / 70.0).abs() < 1e-10);
        assert_eq!(before.usage_since(&before), None);
        assert_eq!(before.usage_since(&after), None);
        let decreased_iowait = CpuTimes {
            user: after.user + 100,
            iowait: 0,
            ..after
        };
        assert_eq!(decreased_iowait.usage_since(&after), None);
        assert!(parse_cpu_times("cpu0 1 2 3 4").is_none());
        assert!(parse_cpu_times("cpu 1 2 broken 4").is_none());
    }

    #[test]
    fn invalid_metrics_do_not_become_zero_or_nonfinite_json() {
        for text in ["NaN 0 0", "inf 0 0", "-1 0 0", "1 2"] {
            assert_eq!(parse_load(text), None);
        }
        assert_eq!(finite_nonnegative("NaN"), None);
        assert_eq!(finite_nonnegative("-10"), None);
        assert_eq!(parse_load("1.0 2.5 0.0 3/100 42"), Some([1.0, 2.5, 0.0]));
    }
}
