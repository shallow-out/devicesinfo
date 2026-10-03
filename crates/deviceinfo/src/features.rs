//! 指令集特征。
//!
//! # 为什么不再筛选
//!
//! 早先这里按前缀挑"与推理相关"的特征，结果两个方向都被打脸：
//!
//! - **假阳性**：x86 的 `smep`（Supervisor Mode Execution Prevention，内核安全特性）
//!   撞上 `sme` 前缀，混进了列表。
//! - **死项**：我以为 arm64 的叫 `dotprod`，内核实际报 `asimddp` ——
//!   那个前缀在真机上永远匹配不到（查 `arch/arm64/kernel/cpuinfo.c` 才确认）。
//!
//! 更根本的是**筛选在架构上就是错的**：消费方问的是开放式问题
//! （"这台机器有没有 AMX"、"有没有 SVE2"、"有没有 FP8 转换"），
//! 生产者一旦筛掉，信息就永久消失了，而且丢失是无声的。
//! 所以这里**原样上报内核给出的全部特征**；好看不好看是显示层的事
//! （见 [`is_highlighted`]）。

use crate::report::CpuFeatureGroup;
use std::collections::{BTreeMap, BTreeSet};

/// 解析 cpuinfo 的特征行，**原样**收集全部特征（排序去重）。
///
/// x86 用 `flags`，arm64 用 `Features`。两者都没有就返回空。
pub(crate) fn parse(cpuinfo: &str) -> Vec<String> {
    let mut found: Vec<String> = cpuinfo
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| matches!(key.trim(), "flags" | "Features"))
        .flat_map(|(_, value)| value.split_whitespace())
        .map(str::to_string)
        .collect();
    // 排序而不是保留 cpuinfo 顺序：内核那个顺序没有语义，
    // 输出稳定才能拿两台机器的报告直接 diff
    found.sort();
    found.dedup();
    found
}

/// 不依赖空行分段：每条 processor 声明开始一条核记录。
/// 缺特征行、重复 processor 或冲突的特征行都不能成为可信的核能力。
pub(crate) fn groups(
    cpuinfo: &str,
    candidates: &[usize],
    warnings: &mut Vec<String>,
) -> Vec<CpuFeatureGroup> {
    let mut records: BTreeMap<usize, Option<Vec<String>>> = BTreeMap::new();
    let mut current = None;
    let mut invalid = BTreeSet::new();
    for line in cpuinfo.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "processor" => {
                current = value.trim().parse::<usize>().ok();
                if let Some(cpu) = current {
                    if records.insert(cpu, None).is_some() {
                        invalid.insert(cpu);
                    }
                } else {
                    warnings.push("/proc/cpuinfo 有无效 processor 编号，不能关联其指令集".into());
                }
            }
            "flags" | "Features" => {
                if let Some(cpu) = current {
                    let features = parse(line);
                    let record = records.get_mut(&cpu).expect("processor 已登记");
                    if record
                        .as_ref()
                        .is_some_and(|previous| previous != &features)
                    {
                        invalid.insert(cpu);
                    }
                    *record = Some(features);
                }
            }
            _ => {}
        }
    }
    for cpu in &invalid {
        warnings.push(format!(
            "/proc/cpuinfo 的 cpu{cpu} 记录重复或特征冲突，其公共能力未知"
        ));
    }
    let mut grouped: BTreeMap<Vec<String>, Vec<usize>> = BTreeMap::new();
    for (cpu, features) in records {
        if candidates.contains(&cpu) && !invalid.contains(&cpu) {
            if let Some(features) = features {
                grouped.entry(features).or_default().push(cpu);
            }
        }
    }
    let mut groups: Vec<_> = grouped
        .into_iter()
        .map(|(features, cpus)| CpuFeatureGroup { cpus, features })
        .collect();
    groups.sort_by_key(|group| group.cpus[0]);
    let covered: usize = groups.iter().map(|group| group.cpus.len()).sum();
    if covered > 0 && covered < candidates.len() {
        warnings.push(format!(
            "只有 {covered}/{} 个逻辑核有完整指令集记录，全核公共特征未知",
            candidates.len()
        ));
    }
    groups
}

/// 名字形状撞上前缀、但**不是**加速指令集的项。
///
/// 形状本身分不开：ARM 的 `svebf16` 和 x86 的 `smep` 都是「前缀 + 小写字母」，
/// 而前者必须保留、后者是内核安全特性（Supervisor Mode Execution Prevention）。
/// 所以只能显式点名。
///
/// 这个名单**只影响显示层**（数据层原样上报全部特征），所以即使漏了也只是多露一行，
/// 不会丢信息。
const NOT_A_FEATURE: [&str; 2] = ["smep", "smap"];

/// 显示时值得单独点出来的特征。
///
/// **这只影响 `render::human` 的排版，不参与任何判断。** 数据层已经完整上报，
/// 这里漏一个、多一个都不影响正确性——这正是把筛选从数据层挪到显示层的目的。
pub(crate) fn is_highlighted(name: &str) -> bool {
    if NOT_A_FEATURE.contains(&name) {
        return false;
    }
    const PREFIXES: [&str; 12] = [
        "sse", "ssse", "avx", "amx", "asimd", "sve", "sme", "i8mm", "bf16", "f16", "f8", "sha",
    ];
    const EXACT: [&str; 12] = [
        "fma",
        "bmi1",
        "bmi2",
        "crc32",
        "aes",
        "gfni",
        "vaes",
        "vpclmulqdq",
        "fphp",
        // 显式列出：`ebf16` 不以 `bf16` 开头，但同样得高亮
        "ebf16",
        // arm64 的历史别名，新内核报 asimddp；两个都留着
        "dotprod",
        "faminmax",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix)) || EXACT.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内核 `arch/arm64/kernel/cpuinfo.c` 里 hwcap_str 的真实内容（64 位部分）。
    ///
    /// 这份清单是去查源码抄下来的，不是我想象的——`dotprod` 就是这么被证伪的。
    const ARM64_KERNEL_FEATURES: &str = "\
fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm jscvt fcma \
lrcpc dcpop sha3 sm3 sm4 asimddp sha512 sve asimdfhm dit uscat ilrcpc flagm ssbs sb paca pacg \
gcs ls64 dcpodp sve2 sveaes svepmull svebitperm svesha3 svesm4 flagm2 frint svei8mm svef32mm \
svef64mm svebf16 i8mm bf16 dgh rng bti mte ecv afp rpres mte3 sme smei16i64 smef64f64 smei8i32 \
smef16f32 smeb16f32 smef32f32 smefa64 wfxt ebf16 sveebf16 cssc rprfm sve2p1 sme2 sme2p1 \
smei16i32 smebi32i32 smeb16b16 smef16f16 mops hbc sveb16b16 lrcpc3 lse128 fpmr lut faminmax \
f8cvt f8fma f8dp4 f8dp2 f8e4m3 f8e5m2 smelutv2 smef8f16 smef8f32 smesf8fma smesf8dp4 smesf8dp2 \
poe cmpbr fprcvt f8mm8 f8mm4 svef16mm sveeltperm sveaes2 svebfscale sve2p2 sme2p2 smesbitperm \
smeaes smesfexpa smestmop smesmop4 mtefar mtestoreonly lsfe sveb16mm sve2p3 smelut6 sme2p3 \
f16mm f16f32dot f16f32mm svelut6";

    #[test]
    fn every_feature_line_is_kept_but_invalid_ids_are_not_associated() {
        let text = "processor: 2\nflags: aes avx2\nprocessor: nope\nflags: sve2\nprocessor: 8\nFeatures: aes asimd\n";
        assert_eq!(parse(text), ["aes", "asimd", "avx2", "sve2"]);
        let mut warnings = Vec::new();
        let groups = groups(text, &[2, 8], &mut warnings);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].cpus, [2]);
        assert_eq!(groups[1].cpus, [8]);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn every_kernel_feature_is_reported() {
        let cpuinfo = format!("processor\t: 0\nFeatures\t: {ARM64_KERNEL_FEATURES}\n");
        let reported = parse(&cpuinfo);

        // 一个都不能少：这是"原样上报"的全部意义
        for feature in ARM64_KERNEL_FEATURES.split_whitespace() {
            assert!(
                reported.contains(&feature.to_string()),
                "漏了 {feature}；只报了 {} 项",
                reported.len()
            );
        }
        assert_eq!(
            reported.len(),
            ARM64_KERNEL_FEATURES.split_whitespace().count(),
            "不应多报也不应少报"
        );
    }

    #[test]
    fn x86_noise_is_reported_but_not_highlighted() {
        // `smep` 曾经因为 `sme` 前缀混进"指令集"列表——现在它在数据里（完整上报），
        // 但不会被显示层当成加速指令集高亮出来
        let cpuinfo = "flags\t\t: fpu vme smep smap tpr_shadow avx2 avx_vnni\n";
        let reported = parse(cpuinfo);
        assert!(reported.contains(&"smep".to_string()));
        assert!(reported.contains(&"vme".to_string()));
        assert!(!is_highlighted("smep"));
        assert!(!is_highlighted("smap"));
        assert!(!is_highlighted("vme"));
        assert!(!is_highlighted("tpr_shadow"));
        assert!(is_highlighted("avx_vnni"));
        assert!(is_highlighted("avx2"));
    }

    #[test]
    fn highlights_cover_the_arm64_features_that_matter() {
        for feature in [
            "asimd",
            "asimddp",
            "asimdhp",
            "asimdfhm",
            "asimdrdm",
            "i8mm",
            "bf16",
            "ebf16",
            "fphp",
            "faminmax",
            "f8cvt",
            "f8fma",
            "f8dp4",
            "f16mm",
            "f16f32mm",
            "sve",
            "sve2",
            "svebf16",
            "svei8mm",
            "sme",
            "sme2",
            "sha3",
            "aes",
            "crc32",
        ] {
            assert!(is_highlighted(feature), "{feature} 应当被高亮");
        }
    }

    #[test]
    fn missing_feature_line_yields_nothing() {
        assert!(parse("processor\t: 0\nBogoMIPS\t: 38.40\n").is_empty());
        assert!(parse("").is_empty());
    }

    #[test]
    fn features_are_sorted_and_deduped() {
        // arm64 的 32 位兼容段会和 64 位段重复，去重必须发生
        let cpuinfo = "Features\t: sve asimd aes asimd aes\n";
        assert_eq!(parse(cpuinfo), vec!["aes", "asimd", "sve"]);
    }
}
