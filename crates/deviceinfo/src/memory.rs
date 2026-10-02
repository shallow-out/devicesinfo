//! 内存的**硬件事实**：只有总量。
//!
//! 可用内存和 swap **不在这里**，它们在 [`crate::state`]——那些是瞬时值，
//! 混进硬件报告会让这份报告既不能缓存、也不能跨设备比较。

use crate::report::MemoryInfo;
use crate::sysfs::kib_field;
use std::fs;
use std::path::Path;

pub(crate) fn probe(root: &Path, warnings: &mut Vec<String>) -> MemoryInfo {
    let path = root.join("proc/meminfo");
    let Ok(text) = fs::read_to_string(&path) else {
        warnings.push(format!("读不到 {}，内存信息缺失", path.display()));
        return MemoryInfo { total_bytes: None };
    };
    let total_bytes = kib_field(&text, "MemTotal");
    if total_bytes.is_none() {
        warnings.push(format!("{} 里没有可解析的 MemTotal", path.display()));
    }
    MemoryInfo { total_bytes }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_total_from_meminfo() {
        let root = std::env::temp_dir().join(format!("deviceinfo-mem-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(root.join("proc")).unwrap();
        fs::write(
            root.join("proc/meminfo"),
            "MemTotal:       32000000 kB\nMemFree: 1000 kB\nMemAvailable: 16000000 kB\n",
        )
        .unwrap();

        let mut warnings = Vec::new();
        let info = probe(&root, &mut warnings);
        assert_eq!(info.total_bytes, Some(32_000_000 * 1024));
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_meminfo_is_a_warning_with_no_total() {
        let root = std::env::temp_dir().join(format!("deviceinfo-mem-none-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();

        let mut warnings = Vec::new();
        let info = probe(&root, &mut warnings);
        // 读不到就是 None，不是 0——0 会让上层算出“一字节都装不下”
        assert_eq!(info.total_bytes, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn meminfo_without_memtotal_is_reported() {
        let root = std::env::temp_dir().join(format!("deviceinfo-mem-junk-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(root.join("proc")).unwrap();
        fs::write(root.join("proc/meminfo"), "MemFree: 1000 kB\n").unwrap();

        let mut warnings = Vec::new();
        assert_eq!(probe(&root, &mut warnings).total_bytes, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }
}
