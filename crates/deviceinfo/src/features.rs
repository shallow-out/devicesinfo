//! 指令集特征筛选。
//!
//! 这里只回答一个问题：**这条 cpuinfo 特征值跟推理性能有关吗。**

/// 名字形状撞上前缀、但**不是**加速指令集的项。
///
/// 形状本身分不开：ARM 的 `svebf16` 和 x86 的 `smep` 都是「前缀 + 小写字母」，
/// 而前者必须保留、后者是内核安全特性（Supervisor Mode Execution Prevention）。
/// 所以只能显式点名。
const NOT_A_FEATURE: [&str; 1] = ["smep"];

/// 与推理性能相关的指令集特征。
///
/// **按族匹配，不列全集白名单**：硬编码列表追不上新指令集（AVX-VNNI、AMX、AVX10
/// 都是后来才有的），而且漏项是静默的——报告看起来完全正常，只是少了一行。
///
/// `pni`（内核给 SSE3 的别名）故意不收：它在列表里毫无信息量，而 SSE3/SSSE3
/// 在任何还能跑现代推理框架的 x86_64 上都是基准。
pub(crate) fn is_relevant_feature(name: &str) -> bool {
    if NOT_A_FEATURE.contains(&name) {
        return false;
    }
    const PREFIXES: [&str; 15] = [
        "sse", "ssse", "avx", "amx", "sve", "sme", "neon", "asimd", "dotprod", "i8mm", "bf16",
        "fp16", "fphp", "sha", "aes",
    ];
    const EXACT: [&str; 9] = [
        "f16c",
        "fma",
        "bmi1",
        "bmi2",
        "crc32",
        "vpclmulqdq",
        "gfni",
        "vaes",
        "vfpu",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix)) || EXACT.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_is_by_family_not_a_whitelist() {
        // 这些项在朴素的白名单里全会被漏掉，而且是静默漏掉
        for feature in [
            "avx_vnni",
            "avx512_vnni",
            "avx10_1",
            "amx_tile",
            "sse4_2",
            "ssse3",
            "sve2",
            "svebf16",
            "svei8mm",
            "dotprod",
            "i8mm",
            "bf16",
            "sha2",
            "f16c",
        ] {
            assert!(is_relevant_feature(feature), "{feature} 应当被识别");
        }
        // 与推理无关的内核 flag 不该混进来
        for noise in [
            "fpu",
            "vme",
            "tpr_shadow",
            "xsave",
            "lm",
            "cx16",
            "hypervisor",
            "smap",
            // 实测撞上前缀的假阳性：`smep` 和 `svebf16` 形状一模一样，只能显式排除
            "smep",
        ] {
            assert!(!is_relevant_feature(noise), "{noise} 不是加速指令集");
        }
    }
}
