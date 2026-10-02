//! 内存探测。
//!
//! 只报两个数字，但语义差别很大，别混：
//! `total_bytes` 是**硬件事实**，`available_bytes` 是**瞬时状态**——
//! 缓存这份报告、或者拿它跨设备比较时，后者会骗人。

use crate::report::MemoryInfo;
use crate::sysfs::field;
use std::fs;
use std::path::Path;

pub(crate) fn probe(root: &Path, warnings: &mut Vec<String>) -> MemoryInfo {
    let path = root.join("proc/meminfo");
    let Some(text) = fs::read_to_string(&path).ok() else {
        warnings.push(format!("读不到 {}，内存信息缺失", path.display()));
        return MemoryInfo {
            total_bytes: 0,
            available_bytes: None,
        };
    };
    MemoryInfo {
        total_bytes: parse_kib_field(&text, "MemTotal").unwrap_or(0),
        available_bytes: parse_kib_field(&text, "MemAvailable"),
    }
}

/// meminfo 的所有值都以 kB 为单位。数值解析失败就返回 `None`，不填 0 冒充。
fn parse_kib_field(text: &str, key: &str) -> Option<u64> {
    field(text, key)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kib_values_into_bytes() {
        let text = "MemTotal:       32000000 kB\nMemFree: 1000 kB\nMemAvailable:   16000000 kB\n";
        assert_eq!(parse_kib_field(text, "MemTotal"), Some(32_000_000 * 1024));
        assert_eq!(
            parse_kib_field(text, "MemAvailable"),
            Some(16_000_000 * 1024)
        );
        // 没有这一项不等于 0
        assert_eq!(parse_kib_field(text, "SwapTotal"), None);
    }

    #[test]
    fn missing_meminfo_is_a_warning_with_zero_total() {
        let root = std::env::temp_dir().join(format!("deviceinfo-mem-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();

        let mut warnings = Vec::new();
        let reported = probe(&root, &mut warnings);
        assert_eq!(reported.total_bytes, 0);
        assert_eq!(reported.available_bytes, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }
}
