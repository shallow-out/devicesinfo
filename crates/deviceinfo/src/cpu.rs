//! CPU 探测：架构、核数、性能分层、指令集。
//!
//! 这一块是整份报告里最容易出错的部分，因为"核数"这个看起来最简单的数字
//! 在混合架构上根本回答不了"这台机器有多快"。

use crate::features;
use crate::report::{CoreTier, CpuInfo};
use crate::sysfs::{field, read_dt_property, read_trimmed, read_u64};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub(crate) fn probe(root: &Path, arch: &str, warnings: &mut Vec<String>) -> CpuInfo {
    let cpuinfo_path = root.join("proc/cpuinfo");
    let cpuinfo = fs::read_to_string(&cpuinfo_path).ok();
    if cpuinfo.is_none() {
        warnings.push(format!(
            "读不到 {}，CPU 型号与指令集缺失",
            cpuinfo_path.display()
        ));
    }

    let cpu_ids = sysfs_cpu_ids(root);
    let logical_cores = if cpu_ids.is_empty() {
        cpuinfo.as_deref().map(count_processors).unwrap_or(0)
    } else {
        cpu_ids.len()
    };
    if cpu_ids.is_empty() && cpuinfo.is_some() {
        // 有 cpuinfo 没 sysfs：型号还能报，但核分组和物理核都拿不到，
        // 等效算力随之消失——这会让上层的筛选变粗，值得出声。
        warnings.push("没有 sysfs CPU 拓扑（sys/devices/system/cpu），核分组与物理核数不可用".into());
    }

    // x86 用 "model name"；arm64 未必有（见 CpuInfo::cpu_model 的文档）
    let cpu_model = cpuinfo.as_deref().and_then(|text| {
        ["model name", "Model", "Hardware", "Processor"]
            .iter()
            .find_map(|key| field(text, key))
            .map(str::to_string)
    });

    let simd = cpuinfo.as_deref().map(features::parse).unwrap_or_default();

    CpuInfo {
        arch: arch.to_string(),
        cpu_model,
        machine_model: machine_model(root),
        logical_cores,
        physical_cores: probe_physical_cores(root, &cpu_ids, cpuinfo.as_deref(), logical_cores),
        core_tiers: probe_core_tiers(root, &cpu_ids, logical_cores, warnings),
        features: simd,
    }
}

/// 整机型号。两级，对应两种固件接口：
///
/// - **设备树**（ARM/嵌入式的常见形态）：`/sys/firmware/devicetree/base/model`，
///   例如 `Radxa ROCK 5B+`。
/// - **DMI**（ACPI 平台，包括 ACPI 启动的 ARM 服务器）：`product_name`，
///   例如 `Radxa Orion O6N`。实测那台 CIX 的机器**整个 `/sys/firmware/devicetree`
///   都不存在**，只有 DMI。
///
/// 之所以需要这一级：`/proc/cpuinfo` 在 arm64 上**未必**有型号——Rockchip 那台一行
/// 都没有，而 CIX 那台厂商内核又报 `model name`。内核之间不一致，就得有兜底。
/// x86 上这个函数根本不会被调用（cpuinfo 总有 `model name`）。
fn machine_model(root: &Path) -> Option<String> {
    read_dt_property(&root.join("sys/firmware/devicetree/base/model"))
        .or_else(|| read_dt_property(&root.join("sys/class/dmi/id/product_name")))
}

/// cpuinfo 里 `processor` 行的条数。
fn count_processors(text: &str) -> usize {
    text.lines()
        .filter(|line| line.starts_with("processor"))
        .count()
}

/// sysfs 下的在线 CPU 编号（`cpuN` 目录）。
fn sysfs_cpu_ids(root: &Path) -> Vec<usize> {
    let dir = root.join("sys/devices/system/cpu");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut ids: Vec<usize> = entries
        .flatten()
        .filter_map(|entry| {
            // `cpufreq` / `cpuidle` 也带 cpu 前缀，靠能否 parse 成数字区分
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("cpu")?
                .parse::<usize>()
                .ok()
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// 解析 `0,3-5` 这类 CPU 列表。
fn parse_cpu_list(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in text.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((from, to)) => {
                if let (Ok(from), Ok(to)) = (
                    from.trim().parse::<usize>(),
                    to.trim().parse::<usize>(),
                ) {
                    out.extend(from..=to);
                }
            }
            None => {
                if let Ok(cpu) = part.parse::<usize>() {
                    out.push(cpu);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// 核分组键。
///
/// **只用一个维度**：拿 `(capacity, freq)` 当联合键，会把同一性能档、频率只差几十 MHz
/// 的核拆成一堆没意义的组。capacity 是内核给出的相对性能，优先；没有才退到频率。
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum TierKey {
    Capacity(u64),
    Freq(u64),
}

/// 按相对性能把核分组，强的在前。
fn probe_core_tiers(
    root: &Path,
    cpu_ids: &[usize],
    logical_cores: usize,
    warnings: &mut Vec<String>,
) -> Vec<CoreTier> {
    // sysfs 目录读不出来时按编号猜一遍路径：单文件缺失只会让那个核不进分组，不会出错
    let candidates: Vec<usize> = if cpu_ids.is_empty() {
        (0..logical_cores).collect()
    } else {
        cpu_ids.to_vec()
    };

    let mut groups: BTreeMap<TierKey, Vec<(usize, Option<u64>)>> = BTreeMap::new();
    let mut seen = 0_usize;
    let mut with_capacity = 0_usize;

    for cpu in candidates {
        let base = root.join(format!("sys/devices/system/cpu/cpu{cpu}"));
        let capacity = read_u64(&base.join("cpu_capacity"));
        // cpuinfo_max_freq 的单位是 kHz
        let freq_mhz = read_u64(&base.join("cpufreq/cpuinfo_max_freq")).map(|khz| khz / 1000);
        if capacity.is_none() && freq_mhz.is_none() {
            continue;
        }
        seen += 1;
        if capacity.is_some() {
            with_capacity += 1;
        }
        if let Some(key) = capacity.map(TierKey::Capacity).or(freq_mhz.map(TierKey::Freq)) {
            groups.entry(key).or_default().push((cpu, freq_mhz));
        }
    }

    if with_capacity > 0 && with_capacity < seen {
        // 部分核没有 capacity：等效核数要么算不出来、要么偏低，必须说出来
        warnings.push(format!(
            "只有 {with_capacity}/{seen} 个核暴露 cpu_capacity，等效核数不可信"
        ));
    }

    // BTreeMap 迭代是键升序，而我们要强的在前
    groups
        .into_iter()
        .rev()
        .map(|(key, members)| CoreTier {
            cpus: members.iter().map(|(cpu, _)| *cpu).collect(),
            max_freq_mhz: members.iter().filter_map(|(_, freq)| *freq).max(),
            capacity: match key {
                TierKey::Capacity(capacity) => Some(capacity),
                TierKey::Freq(_) => None,
            },
        })
        .collect()
}

/// 物理核数。三条路径，按可靠性排序；都拿不到就 `None`，不猜。
///
/// 1. `smt/active == 0` → 没有超线程，物理核 == 逻辑核。最快也最不会错，ARM 上同样成立。
/// 2. `topology/core_cpus_list` 去重 → 每个物理核一组。跨架构可用，不用解析文本。
/// 3. 兜底 `/proc/cpuinfo` 的 `physical id`/`core id`——**ARM64 上这两个字段通常不存在**，
///    所以只能当兜底。
fn probe_physical_cores(
    root: &Path,
    cpu_ids: &[usize],
    cpuinfo: Option<&str>,
    logical_cores: usize,
) -> Option<usize> {
    if logical_cores == 0 {
        return None;
    }

    if read_trimmed(&root.join("sys/devices/system/cpu/smt/active")).as_deref() == Some("0") {
        return Some(logical_cores);
    }

    let mut core_groups: Vec<Vec<usize>> = Vec::new();
    for cpu in cpu_ids {
        let Some(list) = read_trimmed(&root.join(format!(
            "sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list"
        ))) else {
            continue;
        };
        let group = parse_cpu_list(&list);
        if !group.is_empty() && !core_groups.contains(&group) {
            core_groups.push(group);
        }
    }
    // 多于逻辑核数说明这个来源不可信，宁可不用
    if !core_groups.is_empty() && core_groups.len() <= logical_cores {
        return Some(core_groups.len());
    }

    cpuinfo.and_then(physical_cores_from_cpuinfo)
}

/// 兜底来源：`/proc/cpuinfo` 里按 `(physical id, core id)` 去重。
///
/// 每个 `processor` 行开始一条记录，遇到下一条时把上一条落库，循环结束后再补最后一条。
fn physical_cores_from_cpuinfo(text: &str) -> Option<usize> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut pending: Option<(Option<String>, Option<String>)> = None;
    for line in text.lines() {
        if line.starts_with("processor") {
            if let Some((Some(package), Some(core))) = pending.take() {
                pairs.push((package, core));
            }
            pending = Some((None, None));
        } else if let Some((key, value)) = line.split_once(':') {
            if let Some(record) = pending.as_mut() {
                match key.trim() {
                    "physical id" => record.0 = Some(value.trim().to_string()),
                    "core id" => record.1 = Some(value.trim().to_string()),
                    _ => {}
                }
            }
        }
    }
    if let Some((Some(package), Some(core))) = pending {
        pairs.push((package, core));
    }
    pairs.sort();
    pairs.dedup();
    (!pairs.is_empty()).then_some(pairs.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 构造一棵假文件树的根（建目录由调用方负责）。
    fn fake_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("deviceinfo-cpu-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    /// 在假 root 下造 CPU 拓扑。每一项是 `(cpu, 最大频率 kHz, cpu_capacity)`。
    fn fake_cpus(root: &Path, cpus: &[(usize, Option<u64>, Option<u64>)]) {
        for (cpu, freq_khz, capacity) in cpus {
            let base = root.join(format!("sys/devices/system/cpu/cpu{cpu}"));
            fs::create_dir_all(base.join("topology")).unwrap();
            fs::write(base.join("topology/core_cpus_list"), format!("{cpu}\n")).unwrap();
            if let Some(freq) = freq_khz {
                fs::create_dir_all(base.join("cpufreq")).unwrap();
                fs::write(base.join("cpufreq/cpuinfo_max_freq"), format!("{freq}\n")).unwrap();
            }
            if let Some(capacity) = capacity {
                fs::write(base.join("cpu_capacity"), format!("{capacity}\n")).unwrap();
            }
        }
    }

    fn fake_meminfo(root: &Path) {
        fs::create_dir_all(root.join("proc")).unwrap();
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();
    }

    #[test]
    fn parse_cpu_list_handles_ranges_duplicates_and_junk() {
        assert_eq!(parse_cpu_list("0"), vec![0]);
        assert_eq!(parse_cpu_list("0,1,2,3"), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpu_list("0-3"), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpu_list("0-1,3,5-6"), vec![0, 1, 3, 5, 6]);
        // 乱序和重复都要收敛成规范形式（core_cpus_list 会被拿来当去重键）
        assert_eq!(parse_cpu_list("3,1,1,0-2"), vec![0, 1, 2, 3]);
        // 脏数据不能 panic
        assert_eq!(parse_cpu_list(""), Vec::<usize>::new());
        assert_eq!(parse_cpu_list("x"), Vec::<usize>::new());
        assert_eq!(parse_cpu_list("1-"), Vec::<usize>::new());
    }

    /// 一台真实 Lunar Lake（Core Ultra 7 258V）的形状：4P+4E，且 P 核内部还分两档。
    #[test]
    fn core_tiers_capture_hybrid_p_and_e_cores() {
        let root = fake_root("hybrid");
        fake_meminfo(&root);
        let mut cpuinfo = String::new();
        for cpu in 0..8 {
            cpuinfo.push_str(&format!(
                "processor\t: {cpu}\nmodel name\t: Intel(R) Core(TM) Ultra 7 258V\n\
                 physical id\t: 0\ncore id\t: {cpu}\n\
                 flags\t\t: fpu vme avx avx2 avx_vnni f16c fma bmi1 bmi2 tpr_shadow\n\n"
            ));
        }
        fs::write(root.join("proc/cpuinfo"), cpuinfo).unwrap();
        // 实测：smt/active = 0（Lunar Lake 没有超线程）
        fs::create_dir_all(root.join("sys/devices/system/cpu/smt")).unwrap();
        fs::write(root.join("sys/devices/system/cpu/smt/active"), "0\n").unwrap();

        let mut cpus = Vec::new();
        for cpu in 0..2 {
            cpus.push((cpu, Some(4_800_000), Some(1024)));
        }
        for cpu in 2..4 {
            cpus.push((cpu, Some(4_700_000), Some(1005)));
        }
        for cpu in 4..8 {
            cpus.push((cpu, Some(3_700_000), Some(676)));
        }
        fake_cpus(&root, &cpus);

        let mut warnings = Vec::new();
        let reported = probe(&root, "x86_64", &mut warnings);
        assert_eq!(reported.logical_cores, 8);
        assert_eq!(reported.physical_cores, Some(8));

        let tiers = &reported.core_tiers;
        assert_eq!(tiers.len(), 3, "{tiers:#?}");
        // 强的在前
        assert_eq!(tiers[0].cpus, vec![0, 1]);
        assert_eq!(tiers[0].max_freq_mhz, Some(4800));
        assert_eq!(tiers[0].capacity, Some(1024));
        assert_eq!(tiers[1].cpus, vec![2, 3]);
        assert_eq!(tiers[1].max_freq_mhz, Some(4700));
        assert_eq!(tiers[2].cpus, vec![4, 5, 6, 7]);
        assert_eq!(tiers[2].max_freq_mhz, Some(3700));
        assert_eq!(tiers[2].capacity, Some(676));

        // 关键数字：等效核数 6.60，而不是数出来的 8
        let effective = reported.effective_cores().expect("应当能算出等效核数");
        assert!(
            (effective - 6.60).abs() < 0.01,
            "等效核数应为 6.60，实际 {effective}"
        );

        // 原样上报：像 avx_vnni 这样“相关”的和 tpr_shadow 这样“无关”的都在里面
        assert!(reported.features.contains(&"avx_vnni".to_string()));
        assert!(reported.features.contains(&"tpr_shadow".to_string()));
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn smt_active_uses_core_groups_instead_of_the_shortcut() {
        let root = fake_root("smt");
        fake_meminfo(&root);
        fs::write(
            root.join("proc/cpuinfo"),
            "processor\t: 0\nmodel name\t: x\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("sys/devices/system/cpu/smt")).unwrap();
        fs::write(root.join("sys/devices/system/cpu/smt/active"), "1\n").unwrap();
        // 4 个逻辑核 / 2 个物理核：cpu0 与 cpu2 共用核 0，cpu1 与 cpu3 共用核 1
        for cpu in 0..4 {
            let base = root.join(format!("sys/devices/system/cpu/cpu{cpu}"));
            fs::create_dir_all(base.join("topology")).unwrap();
            let group = if cpu % 2 == 0 { "0,2" } else { "1,3" };
            fs::write(base.join("topology/core_cpus_list"), format!("{group}\n")).unwrap();
        }

        let mut warnings = Vec::new();
        let reported = probe(&root, "x86_64", &mut warnings);
        assert_eq!(reported.logical_cores, 4);
        assert_eq!(
            reported.physical_cores,
            Some(2),
            "开了 SMT 就不能把逻辑核数当物理核数"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// ARM64 的 `/proc/cpuinfo` 里**没有** `physical id`/`core id`；
    /// 靠 sysfs 才拿得到物理核。
    #[test]
    fn arm64_without_cpuinfo_topology_still_reports_physical_cores() {
        let root = fake_root("arm64");
        fake_meminfo(&root);
        let mut cpuinfo = String::from(
            "processor\t: 0\nBogoMIPS\t: 38.40\nCPU implementer\t: 0x41\nCPU part\t: 0xd0b\n\
             Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 cpuid asimddp i8mm sve2\n\n",
        );
        for cpu in 1..8 {
            cpuinfo.push_str(&format!("processor\t: {cpu}\nBogoMIPS\t: 38.40\n\n"));
        }
        cpuinfo.push_str("Hardware\t: Radxa Orion O6N\n");
        fs::write(root.join("proc/cpuinfo"), cpuinfo).unwrap();
        // 故意不写 smt/active —— 走 core_cpus_list 这条路
        fake_cpus(
            &root,
            &(0..8)
                .map(|cpu| (cpu, Some(2_400_000), Some(1024)))
                .collect::<Vec<_>>(),
        );

        let mut warnings = Vec::new();
        let reported = probe(&root, "aarch64", &mut warnings);
        assert_eq!(reported.arch, "aarch64");
        assert_eq!(reported.logical_cores, 8);
        assert_eq!(
            reported.physical_cores,
            Some(8),
            "ARM64 没有 physical id，必须靠 sysfs 拓扑"
        );
        // 这台 CIX 的厂商内核在 arm64 上也报 `model name` → 处理器型号有值
        assert_eq!(reported.cpu_model.as_deref(), Some("Radxa Orion O6N"));
        // 而它是 ACPI 机器，设备树整个不存在 → 整机型号只能靠 DMI
        assert_eq!(reported.machine_model, None);
        // 同构 ARM：等效核数就等于核数
        assert_eq!(reported.effective_cores(), Some(8.0));
        assert_eq!(reported.core_tiers.len(), 1);
        for feature in ["asimd", "asimddp", "i8mm", "sve2", "sha2", "crc32", "aes"] {
            assert!(
                reported.features.contains(&feature.to_string()),
                "漏了 {feature}: {:?}",
                reported.features
            );
        }
        // 不相关的也照样上报（完整名单），只是不会被显示层高亮
        assert!(reported.features.contains(&"fp".to_string()));
        assert!(reported.features.contains(&"evtstrm".to_string()));
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn partial_capacity_is_warned_about_and_yields_no_effective_cores() {
        let root = fake_root("partial");
        fake_meminfo(&root);
        fs::write(
            root.join("proc/cpuinfo"),
            "processor\t: 0\nmodel name\t: x\n",
        )
        .unwrap();
        // cpu0 有 capacity，cpu1 只有频率：等效核数算出来会偏低，必须出声而不是硬算
        fake_cpus(&root, &[(0, Some(1_000_000), Some(1024)), (1, Some(900_000), None)]);

        let mut warnings = Vec::new();
        let reported = probe(&root, "x86_64", &mut warnings);
        assert_eq!(reported.logical_cores, 2);
        assert_eq!(reported.effective_cores(), None);
        assert!(
            warnings.iter().any(|w| w.contains("cpu_capacity")),
            "{warnings:?}"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_sysfs_is_warned_about_but_cpuinfo_still_answers() {
        let root = fake_root("no-sysfs");
        fake_meminfo(&root);
        fs::write(
            root.join("proc/cpuinfo"),
            "processor\t: 0\nmodel name\t: Intel(R) Core(TM) Ultra 7 258V\n\
             physical id\t: 0\ncore id\t: 0\nflags\t\t: fpu vme avx2 avx512f f16c\n\n\
             processor\t: 1\nmodel name\t: Intel(R) Core(TM) Ultra 7 258V\n\
             physical id\t: 0\ncore id\t: 1\nflags\t\t: fpu vme avx2 avx512f f16c\n",
        )
        .unwrap();

        let mut warnings = Vec::new();
        let reported = probe(&root, "x86_64", &mut warnings);
        assert_eq!(reported.logical_cores, 2);
        // 没有 sysfs 时退回 cpuinfo 的 physical id / core id
        assert_eq!(reported.physical_cores, Some(2));
        assert_eq!(
            reported.cpu_model.as_deref(),
            Some("Intel(R) Core(TM) Ultra 7 258V")
        );
        assert!(reported.features.contains(&"avx512f".to_string()));
        assert!(reported.features.contains(&"f16c".to_string()));
        // 原样上报：无关的 flag 也在，该不该露出来是显示层的事
        assert!(reported.features.contains(&"vme".to_string()));
        // 核分组拿不到，等效核数随之消失
        assert!(reported.core_tiers.is_empty());
        assert_eq!(reported.effective_cores(), None);
        // 这件事必须说出来，否则等效算力是无声地没有的
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("sysfs"), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    /// arm64 的 `/proc/cpuinfo` **不报型号**，型号在设备树里。
    /// 少了这一步，每一台 ARM 机器都会显示"未知 CPU"。
    #[test]
    fn model_falls_back_to_the_device_tree() {
        let root = fake_root("dt-model");
        fake_meminfo(&root);
        // 真机形状：没有 model name / Hardware / Processor 行
        fs::write(
            root.join("proc/cpuinfo"),
            "processor\t: 0\nBogoMIPS\t: 48.00\nCPU implementer\t: 0x41\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("sys/devices/system/cpu/smt")).unwrap();
        fs::write(root.join("sys/devices/system/cpu/smt/active"), "0\n").unwrap();
        fs::create_dir_all(root.join("sys/firmware/devicetree/base")).unwrap();
        // 设备树属性是 NUL 结尾的，trim() 去不掉 NUL
        fs::write(
            root.join("sys/firmware/devicetree/base/model"),
            b"Radxa ROCK 5B+\0",
        )
        .unwrap();

        let mut warnings = Vec::new();
        let reported = probe(&root, "aarch64", &mut warnings);
        // 这台 Rockchip 的 cpuinfo 里**没有**型号 → 处理器型号为空，整机型号来自设备树
        assert_eq!(reported.cpu_model, None);
        assert_eq!(reported.machine_model.as_deref(), Some("Radxa ROCK 5B+"));
        // 顺带：空值不能冒充型号
        fs::write(root.join("sys/firmware/devicetree/base/model"), b"\0").unwrap();
        assert_eq!(
            probe(&root, "aarch64", &mut Vec::new()).machine_model,
            None
        );

        fs::remove_dir_all(&root).ok();
    }

    /// ACPI 平台没有设备树，整机型号在 DMI 里。实测那台 CIX 的机器**整个
    /// `/sys/firmware/devicetree` 都不存在**，只有 `/sys/class/dmi/id/product_name`。
    #[test]
    fn machine_model_falls_back_to_dmi() {
        let root = fake_root("dmi-model");
        fake_meminfo(&root);
        fs::write(
            root.join("proc/cpuinfo"),
            "processor\t: 0\nBogoMIPS\t: 48.00\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("sys/devices/system/cpu/smt")).unwrap();
        fs::write(root.join("sys/devices/system/cpu/smt/active"), "0\n").unwrap();
        fs::create_dir_all(root.join("sys/class/dmi/id")).unwrap();
        // DMI 文件是普通文本，带尾换行——不是 NUL 结尾的
        fs::write(root.join("sys/class/dmi/id/product_name"), "Radxa Orion O6N\n").unwrap();

        let mut warnings = Vec::new();
        let reported = probe(&root, "aarch64", &mut warnings);
        assert_eq!(reported.machine_model.as_deref(), Some("Radxa Orion O6N"));
        // 设备树优先：两者都在时用设备树
        fs::create_dir_all(root.join("sys/firmware/devicetree/base")).unwrap();
        fs::write(
            root.join("sys/firmware/devicetree/base/model"),
            b"Radxa ROCK 5B+\0",
        )
        .unwrap();
        assert_eq!(
            probe(&root, "aarch64", &mut Vec::new())
                .machine_model
                .as_deref(),
            Some("Radxa ROCK 5B+")
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_procfs_yields_zeros_and_a_warning() {
        let root = fake_root("empty");
        fs::create_dir_all(&root).unwrap();
        let mut warnings = Vec::new();
        let reported = probe(&root, "x86_64", &mut warnings);
        assert_eq!(reported.logical_cores, 0);
        assert_eq!(reported.physical_cores, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        fs::remove_dir_all(&root).ok();
    }
}
