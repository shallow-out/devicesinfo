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

/// 探测 CPU 会读的**整机共享**输入（相对探测根）。
///
/// 公开是为了让采集夹具的一方按**同一份**清单去取。理由不是"省事"，而是**不允许漂移**：
/// `acpi_cppc/highest_perf` 就漏过一次——本地探测用它算出了 P/E 分档，而采集清单里没有
/// 这个文件，于是**远端夹具**把三档 capacity 全报成 1024，而本地（两台来源恰好一致）看不出
/// 任何异常。
pub const SHARED_INPUTS: [&str; 5] = [
    "proc/cpuinfo",
    "proc/meminfo",
    "sys/devices/system/cpu/smt/active",
    // arm64 的整机型号（`/proc/cpuinfo` 在 arm64 上未必有）
    "sys/firmware/devicetree/base/model",
    // ACPI 平台的整机型号
    "sys/class/dmi/id/product_name",
];

/// 探测 CPU 会**逐核**读的输入（相对 `sys/devices/system/cpu/cpuN/`）。
pub const PER_CORE_INPUTS: [&str; 4] = [
    // P/E 相对性能的**源头**（`cpu_capacity` 会退化，见 `observe_cores`）
    "acpi_cppc/highest_perf",
    "cpu_capacity",
    "cpufreq/cpuinfo_max_freq",
    "topology/core_cpus_list",
];

pub(crate) fn probe(root: &Path, arch: &str, warnings: &mut Vec<String>) -> CpuInfo {
    let cpuinfo_path = root.join("proc/cpuinfo");
    let cpuinfo = fs::read_to_string(&cpuinfo_path).ok();
    if cpuinfo.is_none() {
        warnings.push(format!(
            "读不到 {}，CPU 型号与指令集缺失",
            cpuinfo_path.display()
        ));
    }

    let sysfs_ids = sysfs_cpu_ids(root);
    let cpu_ids = if sysfs_ids.is_empty() {
        cpuinfo.as_deref().map(processor_ids).unwrap_or_default()
    } else {
        sysfs_ids.clone()
    };
    let logical_cores = cpu_ids.len();
    if sysfs_ids.is_empty() && cpuinfo.is_some() {
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
    let feature_groups = cpuinfo.as_deref()
        .map(|text| features::groups(text, &cpu_ids, warnings))
        .unwrap_or_default();

    let mut report = CpuInfo {
        arch: arch.to_string(),
        cpu_model,
        machine_model: machine_model(root),
        logical_cores,
        physical_cores: probe_physical_cores(root, &sysfs_ids, cpuinfo.as_deref(), logical_cores),
        core_tiers: probe_core_tiers(root, &sysfs_ids, logical_cores, warnings),
        features: simd,
        common_features: None,
        feature_groups,
    };
    report.common_features = report.features_for_cpus(&cpu_ids);
    report
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

/// cpuinfo 里的真实逻辑核编号，不按记录数量猜连续编号。
fn processor_ids(text: &str) -> Vec<usize> {
    let mut ids: Vec<_> = text.lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.trim() == "processor")
        .filter_map(|(_, value)| value.trim().parse::<usize>().ok())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// sysfs 下的 CPU 编号（`cpuN` 目录），包含可能离线的核。
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

/// 一个核的观测值。
struct CoreObservation {
    cpu: usize,
    /// 相对性能，最强核为 1024。
    capacity: Option<u64>,
    max_freq_mhz: Option<u64>,
    /// 它属于哪个物理核（同一个 `core_cpus_list` 的线程共享同一个编号）。
    core: Option<usize>,
}

/// 逐个核读观测值，并把相对性能归一化到 1024。
///
/// # 为什么相对性能不从 `cpu_capacity` 取
///
/// **`cpu_capacity` 会在整机全同的时候退化。** 实测那台 i9-12900H（6 P + 8 E）上
/// 它是**全 1024**，而 `acpi_cppc/highest_perf` 是 64/63/38、频率是 5.0/4.9/3.8 GHz
/// ——只看 `cpu_capacity` 会把 P/E 完全抹平，报告就成了一句"20 核 @5.00 GHz"。
///
/// 内核的 `cpu_capacity` 本来就是从 `highest_perf` 推出来的（本机 56/55/37 ↔
/// 1024/1005/676 完全对应），所以这里直接看源头。`highest_perf` 的**绝对值是平台相关**的
/// （本机 56、那台 i9 是 64），只有同机内的比率有意义，所以归一化到 1024
/// ——与 `cpu_capacity` 同一个刻度：最强核 = 1024。
fn observe_cores(root: &Path, candidates: &[usize]) -> Vec<CoreObservation> {
    let mut raw = Vec::with_capacity(candidates.len());
    // `core_cpus_list` → 物理核编号（第一次见到分组时编号）
    let mut core_index: BTreeMap<Vec<usize>, usize> = BTreeMap::new();

    for &cpu in candidates {
        let base = root.join(format!("sys/devices/system/cpu/cpu{cpu}"));
        let highest_perf = read_u64(&base.join("acpi_cppc/highest_perf"));
        let cpu_capacity = read_u64(&base.join("cpu_capacity"));
        // cpuinfo_max_freq 的单位是 kHz
        let max_freq_mhz = read_u64(&base.join("cpufreq/cpuinfo_max_freq")).map(|khz| khz / 1000);
        let core = read_trimmed(&base.join("topology/core_cpus_list"))
            .map(|list| parse_cpu_list(&list))
            .filter(|group| !group.is_empty())
            .map(|group| {
                let next = core_index.len() + 1;
                *core_index.entry(group).or_insert(next)
            });
        raw.push((cpu, highest_perf, cpu_capacity, max_freq_mhz, core));
    }

    // 只有**所有**核都报了 highest_perf 才用它归一化：一半有、一半没有的话，
    // 把两种来源混在一个刻度上只会更乱，不如退回 cpu_capacity。
    let highest = |entry: &(usize, Option<u64>, Option<u64>, Option<u64>, Option<usize>)| entry.1;
    let scale = raw
        .iter()
        .filter_map(highest)
        .max()
        .filter(|max| *max > 0 && raw.iter().all(|entry| entry.1.is_some()));

    raw.into_iter()
        .map(
            |(cpu, highest_perf, cpu_capacity, max_freq_mhz, core)| CoreObservation {
                cpu,
                capacity: match (scale, highest_perf) {
                    (Some(max), Some(value)) => Some(value * 1024 / max),
                    _ => cpu_capacity,
                },
                max_freq_mhz,
                core,
            },
        )
        .collect()
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

    let observations = observe_cores(root, &candidates);
    let seen = observations.len();
    let with_capacity = observations
        .iter()
        .filter(|observation| observation.capacity.is_some())
        .count();
    if with_capacity > 0 && with_capacity < seen {
        // 部分核没有相对性能：等效核数要么算不出来、要么偏低，必须说出来
        warnings.push(format!(
            "只有 {with_capacity}/{seen} 个核暴露相对性能（highest_perf/cpu_capacity），等效核数不可信"
        ));
    }
    let with_core = observations
        .iter()
        .filter(|observation| observation.core.is_some())
        .count();
    let smt_inactive =
        read_trimmed(&root.join("sys/devices/system/cpu/smt/active")).as_deref() == Some("0");
    if with_core < seen && !smt_inactive && !cpu_ids.is_empty() {
        warnings.push(format!(
            "只有 {with_core}/{seen} 个核能读出 topology/core_cpus_list，拓扑不完整的档无法确定物理核数与等效算力"
        ));
    }

    // 分组键：优先相对性能；**当它在整机范围内全同时会退化**（实测那台 i9-12900H
    // 的 `cpu_capacity` 全是 1024），那时退到频率——频率不反映 P/E 的 IPC 差异，
    // 但至少能把两类核分开。两个都没有的核不进分组。
    let capacities: Vec<u64> = observations
        .iter()
        .filter_map(|observation| observation.capacity)
        .collect();
    let capacity_is_uniform =
        capacities.len() > 1 && capacities.iter().all(|value| *value == capacities[0]);

    let mut groups: BTreeMap<TierKey, Vec<&CoreObservation>> = BTreeMap::new();
    for observation in &observations {
        let by_capacity = observation.capacity.map(TierKey::Capacity);
        let by_freq = observation.max_freq_mhz.map(TierKey::Freq);
        let key = if capacity_is_uniform {
            by_freq.or(by_capacity)
        } else {
            by_capacity.or(by_freq)
        };
        if let Some(key) = key {
            groups.entry(key).or_default().push(observation);
        }
    }

    // 排序而不是靠 `rev()`：`TierKey` 是个枚举，混合键（部分核有 capacity、部分只有频率）
    // 时按枚举顺序排会得到“只有频率的那几档反而排在前面”这种莫名其妙的结果。
    // 按**实际强弱**排，缺 capacity 的排最后。
    let mut tiers: Vec<CoreTier> = groups
        .into_values()
        .map(|members| {
            let cpus: Vec<usize> = members.iter().map(|member| member.cpu).collect();
            // 同一 `core_cpus_list` 的线程算一个；缺拓扑时不能把线程数当物理核数。
            let mut cores: Vec<usize> = members
                .iter()
                .filter_map(|member| member.core)
                .collect();
            let unknown = members.iter().filter(|member| member.core.is_none()).count();
            cores.sort_unstable();
            cores.dedup();
            CoreTier {
                cpus,
                physical_cores: if smt_inactive {
                    Some(members.len())
                } else {
                    (unknown == 0).then_some(cores.len())
                },
                max_freq_mhz: members.iter().filter_map(|member| member.max_freq_mhz).max(),
                // 这一档的相对性能：取成员里的最大值。**按频率分组时也要报**
                // ——那种情况说明整机 capacity 全同，它依然是已知事实（只是区分不了核），
                // 丢掉它会让 `effective_cores` 变成 None。
                capacity: members
                    .iter()
                    .filter_map(|member| member.capacity)
                    .max(),
            }
        })
        .collect();
    tiers.sort_by_key(|tier| {
        (
            std::cmp::Reverse(tier.capacity.unwrap_or(0)),
            std::cmp::Reverse(tier.max_freq_mhz.unwrap_or(0)),
        )
    });
    tiers
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
    let mut covered = 0_usize;
    for cpu in cpu_ids {
        let Some(list) = read_trimmed(&root.join(format!(
            "sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list"
        ))) else {
            continue;
        };
        covered += 1;
        let group = parse_cpu_list(&list);
        if !group.is_empty() && !core_groups.contains(&group) {
            core_groups.push(group);
        }
    }
    // **只有把所有核都读到了才敢用这个来源。** 只读到一部分的话，去重后的组数必然偏少，
    // 而"物理核数偏少"会静默地让上层以为这机器比实际弱，还看不出哪里错了。
    // 多于逻辑核数说明这个来源不可信，宁可不用
    if !cpu_ids.is_empty()
        && covered == cpu_ids.len()
        && !core_groups.is_empty()
        && core_groups.len() <= logical_cores
    {
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
    fn heterogeneous_features_are_not_advertised_as_common() {
        let root = fake_root("feature-groups");
        fake_meminfo(&root);
        fake_cpus(
            &root,
            &[
                (2, Some(2_000_000), Some(1024)),
                (7, Some(2_000_000), Some(1024)),
            ],
        );
        fs::write(
            root.join("proc/cpuinfo"),
            "processor: 2\nflags: fpu aes avx2\nprocessor: 7\nflags: fpu aes sve2\n",
        )
        .unwrap();
        let mut warnings = Vec::new();
        let cpu = probe(&root, "x86_64", &mut warnings);
        assert_eq!(cpu.features, ["aes", "avx2", "fpu", "sve2"]);
        assert_eq!(cpu.common_features, Some(vec!["aes".into(), "fpu".into()]));
        assert_eq!(cpu.feature_groups.len(), 2);
        assert_eq!(cpu.feature_groups[0].cpus, [2]);
        assert_eq!(cpu.feature_groups[1].cpus, [7]);
        assert_eq!(
            cpu.features_for_cpus(&[2]),
            Some(vec!["aes".into(), "avx2".into(), "fpu".into()])
        );
        assert_eq!(cpu.features_for_cpus(&[7, 2, 7]), cpu.common_features);
        assert_eq!(cpu.features_for_cpus(&[0]), None);
        assert_eq!(cpu.features_for_cpus(&[]), None);
        assert!(warnings.is_empty(), "{warnings:?}");
        let report = crate::probe_with(&root, "x86_64");
        let text = crate::render::human(&report);
        assert!(text.contains("全核公共特征: 2 项"), "{text}");
        assert!(text.contains("cpu2: 3 项"), "{text}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sparse_cpuinfo_ids_work_without_sysfs_and_old_json_stays_unknown() {
        let root = fake_root("sparse-features");
        fake_meminfo(&root);
        fs::write(
            root.join("proc/cpuinfo"),
            "processor: 3\nflags: aes avx2\n\nprocessor: 11\nflags: aes\n",
        )
        .unwrap();
        let cpu = probe(&root, "x86_64", &mut Vec::new());
        assert_eq!(cpu.logical_cores, 2);
        assert_eq!(cpu.common_features, Some(vec!["aes".into()]));
        assert_eq!(cpu.feature_groups[0].cpus, [3]);
        assert_eq!(cpu.feature_groups[1].cpus, [11]);
        let mut json = serde_json::to_value(&cpu).unwrap();
        json.as_object_mut().unwrap().remove("common_features");
        json.as_object_mut().unwrap().remove("feature_groups");
        let old: CpuInfo = serde_json::from_value(json).unwrap();
        assert_eq!(old.common_features, None);
        assert_eq!(old.features_for_cpus(&[3]), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_and_conflicting_records_cannot_claim_full_cpu_capability() {
        let root = fake_root("incomplete-features");
        fake_meminfo(&root);
        fake_cpus(&root, &[(0, None, Some(1024)), (1, None, Some(1024))]);
        for text in [
            "processor: 0\nflags: aes avx2\nprocessor: 1\nmodel name: missing flags\n",
            "processor: 0\nflags: aes avx2\nprocessor: 1\nflags: aes\nprocessor: 1\nflags: aes\n",
            "processor: 0\nflags: aes avx2\nprocessor: 1\nflags: aes\nflags: aes avx2\n",
        ] {
            fs::write(root.join("proc/cpuinfo"), text).unwrap();
            let mut warnings = Vec::new();
            let cpu = probe(&root, "x86_64", &mut warnings);
            assert_eq!(cpu.common_features, None, "{text}");
            assert_eq!(cpu.features_for_cpus(&[1]), None, "{text}");
            assert!(cpu.features_for_cpus(&[0]).is_some());
            assert!(!warnings.is_empty(), "{text}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disjoint_or_empty_feature_lines_have_a_known_empty_intersection() {
        let root = fake_root("empty-features");
        fake_meminfo(&root);
        fake_cpus(&root, &[(0, None, Some(1024)), (1, None, Some(1024))]);
        for text in [
            "processor: 0\nflags: aes\nprocessor: 1\nflags: avx2\n",
            "processor: 0\nflags: \nprocessor: 1\nflags: aes\n",
        ] {
            fs::write(root.join("proc/cpuinfo"), text).unwrap();
            let cpu = probe(&root, "x86_64", &mut Vec::new());
            assert_eq!(cpu.common_features, Some(Vec::new()));
            assert_eq!(
                serde_json::to_value(&cpu).unwrap()["common_features"],
                serde_json::json!([])
            );
        }
        fs::remove_dir_all(root).unwrap();
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
        // 此合成样本仅 cpu0 有 Features，不能替其它七个核承诺能力。
        assert_eq!(reported.common_features, None);
        assert_eq!(reported.feature_groups[0].cpus, [0]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("1/8"), "{warnings:?}");

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

    /// `core_cpus_list` 只读到一部分时**不能**拿它算物理核数：去重后的组数必然偏少，
    /// 而"物理核数偏少"会静默地让上层以为这机器比实际弱，还看不出哪里错了。
    #[test]
    fn a_partially_readable_topology_is_not_trusted() {
        let root = fake_root("partial-topology");
        fake_meminfo(&root);
        let mut cpuinfo = String::new();
        for cpu in 0..4 {
            cpuinfo.push_str(&format!(
                "processor\t: {cpu}\nmodel name\t: x\nphysical id\t: 0\ncore id\t: {cpu}\n\n"
            ));
        }
        fs::write(root.join("proc/cpuinfo"), cpuinfo).unwrap();
        fs::create_dir_all(root.join("sys/devices/system/cpu/smt")).unwrap();
        fs::write(root.join("sys/devices/system/cpu/smt/active"), "1\n").unwrap();
        for cpu in 0..4 {
            fs::create_dir_all(root.join(format!("sys/devices/system/cpu/cpu{cpu}"))).unwrap();
        }
        // 只有前两个核的 core_cpus_list 读得到
        for cpu in 0..2 {
            let topology = root.join(format!("sys/devices/system/cpu/cpu{cpu}/topology"));
            fs::create_dir_all(&topology).unwrap();
            fs::write(topology.join("core_cpus_list"), format!("{cpu}\n")).unwrap();
        }

        let mut warnings = Vec::new();
        let reported = probe(&root, "x86_64", &mut warnings);
        assert_eq!(reported.logical_cores, 4);
        assert_eq!(
            reported.physical_cores,
            Some(4),
            "只读到一半就该退回 cpuinfo，而不是少报成 2"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// 一台真实的 **i9-12900H**（6 P + 8 E，20 线程）的简化形状：
    ///
    /// - `cpu_capacity` **全是 1024**（在这台内核上它就是退化的）
    /// - `acpi_cppc/highest_perf` 是 64 / 63 / 38 → P/E 只能从这里看出来
    /// - P 核有 SMT（两个线程一个物理核），E 核没有
    ///
    /// 这条同时盯两件事：相对性能的来源（否则 P/E 会被抹平成一档），
    /// 以及等效算力必须**按物理核**折算（否则 SMT 的两个线程会当两个核）。
    #[test]
    fn uniform_capacity_falls_back_to_cppc_and_counts_physical_cores() {
        let root = fake_root("hybrid-cppc");
        fake_meminfo(&root);
        fs::write(
            root.join("proc/cpuinfo"),
            "processor\t: 0\nmodel name\t: 12th Gen Intel(R) Core(TM) i9-12900H\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("sys/devices/system/cpu/smt")).unwrap();
        fs::write(root.join("sys/devices/system/cpu/smt/active"), "1\n").unwrap();

        // (cpu, highest_perf, max_freq_khz, core_cpus_list)
        let cores: [(usize, u64, u64, &str); 4] = [
            (0, 63, 4_900_000, "0-1"),
            (1, 63, 4_900_000, "0-1"),
            (2, 64, 5_000_000, "2"),
            (3, 38, 3_800_000, "3"),
        ];
        for (cpu, highest, freq, group) in cores {
            let base = root.join(format!("sys/devices/system/cpu/cpu{cpu}"));
            fs::create_dir_all(base.join("acpi_cppc")).unwrap();
            fs::create_dir_all(base.join("cpufreq")).unwrap();
            fs::create_dir_all(base.join("topology")).unwrap();
            // 关键：capacity 一律 1024，只看它是分不出 P/E 的
            fs::write(base.join("cpu_capacity"), "1024\n").unwrap();
            fs::write(base.join("acpi_cppc/highest_perf"), format!("{highest}\n")).unwrap();
            fs::write(
                base.join("cpufreq/cpuinfo_max_freq"),
                format!("{freq}\n"),
            )
            .unwrap();
            fs::write(base.join("topology/core_cpus_list"), format!("{group}\n")).unwrap();
        }

        let mut warnings = Vec::new();
        let reported = probe(&root, "x86_64", &mut warnings);
        assert_eq!(reported.logical_cores, 4);
        assert_eq!(reported.physical_cores, Some(3), "P 核的两个线程算一个核");

        let tiers = &reported.core_tiers;
        assert_eq!(tiers.len(), 3, "P/E 必须分开: {tiers:#?}");
        // 归一化到 1024：64→1024、63→1008、38→608
        assert_eq!(tiers[0].capacity, Some(1024));
        assert_eq!(tiers[0].cpus, vec![2]);
        assert_eq!(tiers[1].capacity, Some(1008));
        assert_eq!(tiers[1].cpus, vec![0, 1]);
        assert_eq!(
            tiers[1].physical_cores,
            Some(1),
            "两个线程属于同一个物理核: {tiers:#?}"
        );
        assert_eq!(tiers[2].capacity, Some(608));
        assert_eq!(tiers[2].max_freq_mhz, Some(3800));

        // 按**物理核**折算：(1024 + 1008 + 608) / 1024 ≈ 2.58
        // 按逻辑核会算成 (1024 + 1008×2 + 608) / 1024 ≈ 3.56，那是高估
        let effective = reported.effective_cores().expect("应当算得出");
        assert!(
            (effective - 2.58).abs() < 0.01,
            "等效核数应为 2.58（按物理核），实际 {effective}"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// 相对性能**和**频率都区分不了核时（同构机器），只有一档——不能凭空造分档。
    #[test]
    fn a_homogeneous_machine_stays_one_tier() {
        let root = fake_root("homogeneous");
        fake_meminfo(&root);
        fs::write(root.join("proc/cpuinfo"), "processor\t: 0\nmodel name\t: x\n").unwrap();
        fake_cpus(
            &root,
            &[
                (0, Some(2_400_000), Some(1024)),
                (1, Some(2_400_000), Some(1024)),
                (2, Some(2_400_000), Some(1024)),
                (3, Some(2_400_000), Some(1024)),
            ],
        );

        let reported = probe(&root, "x86_64", &mut Vec::new());
        assert_eq!(reported.core_tiers.len(), 1, "{:#?}", reported.core_tiers);
        assert_eq!(reported.core_tiers[0].physical_cores, Some(4));
        assert_eq!(reported.effective_cores(), Some(4.0));

        fs::remove_dir_all(&root).ok();
    }

    /// 部分核有 `cpu_capacity`、部分只有频率时，分档顺序要按**实际强弱**，
    /// 不能按枚举顺序——那会让"只有频率"的档莫名其妙排在前面。
    #[test]
    fn tiers_without_capacity_do_not_sort_ahead() {
        let root = fake_root("mixed-capacity");
        fake_meminfo(&root);
        fs::write(
            root.join("proc/cpuinfo"),
            "processor\t: 0\nmodel name\t: x\n",
        )
        .unwrap();
        // cpu0 有 capacity（已知是弱核），cpu1 只有很高的频率但没 capacity
        fake_cpus(&root, &[(0, Some(900_000), Some(300)), (1, Some(2_800_000), None)]);

        let mut warnings = Vec::new();
        let reported = probe(&root, "x86_64", &mut warnings);
        assert_eq!(reported.core_tiers.len(), 2, "{:#?}", reported.core_tiers);
        // 有 capacity 的那档排在前面：它是"已知强弱"，只有频率的那档什么都不知道
        assert_eq!(reported.core_tiers[0].capacity, Some(300));
        assert_eq!(reported.core_tiers[1].capacity, None);
        // 而且这件事要说出来（否则等效核数看起来只是个偏低的数字）
        assert!(
            warnings.iter().any(|w| w.contains("cpu_capacity")),
            "{warnings:?}"
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

    #[test]
    fn missing_smt_topology_does_not_turn_threads_into_effective_cores() {
        for partial in [false, true] {
            let root = fake_root(if partial {
                "partial-smt-tiers"
            } else {
                "no-smt-tiers"
            });
            fake_meminfo(&root);
            fs::write(root.join("proc/cpuinfo"),
                "processor: 0\nphysical id: 0\ncore id: 0\n\nprocessor: 1\nphysical id: 0\ncore id: 0\n").unwrap();
            fake_cpus(&root, &[(0, None, Some(1024)), (1, None, Some(1024))]);
            fs::remove_file(root.join("sys/devices/system/cpu/cpu1/topology/core_cpus_list"))
                .unwrap();
            let first = root.join("sys/devices/system/cpu/cpu0/topology/core_cpus_list");
            if partial {
                fs::write(first, "0-1\n").unwrap();
            } else {
                fs::remove_file(first).unwrap();
            }
            let mut warnings = Vec::new();
            let report = probe(&root, "x86_64", &mut warnings);
            assert_eq!(report.physical_cores, Some(1));
            assert_eq!(report.core_tiers[0].physical_cores, None);
            assert_eq!(report.effective_cores(), None);
            assert!(
                warnings
                    .iter()
                    .any(|w| w.contains("topology/core_cpus_list")),
                "{warnings:?}"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn inactive_smt_allows_counting_cores_without_per_cpu_topology() {
        let root = fake_root("no-smt-no-topology");
        fake_meminfo(&root);
        fs::write(root.join("proc/cpuinfo"), "processor: 0\nprocessor: 1\n").unwrap();
        fake_cpus(&root, &[(0, None, Some(1024)), (1, None, Some(1024))]);
        for cpu in 0..2 {
            fs::remove_file(root.join(format!(
                "sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list"
            )))
            .unwrap();
        }
        fs::create_dir_all(root.join("sys/devices/system/cpu/smt")).unwrap();
        fs::write(root.join("sys/devices/system/cpu/smt/active"), "0\n").unwrap();
        let mut warnings = Vec::new();
        let report = probe(&root, "x86_64", &mut warnings);
        assert_eq!(report.core_tiers[0].physical_cores, Some(2));
        assert_eq!(report.effective_cores(), Some(2.0));
        assert!(warnings.is_empty(), "{warnings:?}");
        fs::remove_dir_all(root).unwrap();
    }
}
