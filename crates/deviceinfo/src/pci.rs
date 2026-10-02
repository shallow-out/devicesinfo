//! PCI 名字表查询：把 `8086:64a0` 变成 `Arc Graphics 130V/140V GPU`。
//!
//! **解析 `pci.ids` 数据文件，而不是调用 `lspci`**：后者要求机器上装了 pciutils，
//! 而本模块只是给一个 id 配个人名，不该为此引入外部进程依赖和它的权限面。

use std::fs;
use std::path::Path;

/// 候选位置，按常见程度排序：Arch 系在 `hwdata`，Debian 系在 `misc`。
const DATABASE_PATHS: [&str; 4] = [
    "usr/share/hwdata/pci.ids",
    "usr/share/misc/pci.ids",
    "usr/local/share/hwdata/pci.ids",
    "usr/local/share/pci.ids",
];

/// 查 `vendor:device` 的人类可读名字。查不到返回 `None`——调用方退回显示原始 id。
pub(crate) fn lookup(root: &Path, vendor: &str, device: &str) -> Option<String> {
    let vendor_id = normalize_id(vendor)?;
    let device_id = normalize_id(device)?;
    for path in DATABASE_PATHS {
        let Ok(text) = fs::read_to_string(root.join(path)) else {
            continue;
        };
        if let Some(name) = lookup_in(&text, &vendor_id, &device_id) {
            return Some(name);
        }
    }
    None
}

/// 去掉 `0x` 前缀、转小写，并要求恰好 4 位十六进制。
///
/// 严格是有意的：`pci.ids` 里的 id 都是小写 4 位，格式不对说明来源本身有问题，
/// 这时候去查表只会得到一个错误的名字。
fn normalize_id(id: &str) -> Option<String> {
    let trimmed = id
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if trimmed.len() != 4 || !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

/// 在 `pci.ids` 文本里查。文件格式是缩进分层的（下面用空格示意，**真实文件用 tab**）：
///
/// ```text
/// 8086  Intel Corporation
///   64a0  Core Ultra 200V Series Processors Arc Graphics 130V/140V GPU
///     17aa 3da2  ThinkPad ...          <- 两层缩进是子系统，跳过
/// C 03  Display controller              <- 类段开始，厂商段到此为止
/// ```
///
/// 所以扫描时只关心三种行：厂商行（顶格）、设备行（1 层缩进）、类段（`C ` 开头）。
fn lookup_in(text: &str, vendor_id: &str, device_id: &str) -> Option<String> {
    let mut in_target_vendor = false;
    let mut class_section_seen = false;

    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // 类段之后不会再有厂商段，可以提前收工
        if line.starts_with("C ") {
            class_section_seen = true;
            in_target_vendor = false;
            continue;
        }
        if let Some(entry) = line.strip_prefix('\t') {
            // 两个 tab 起头的是子系统，不是设备自身
            if in_target_vendor && !entry.starts_with('\t') {
                if let Some((id, name)) = split_entry(entry) {
                    if id == device_id {
                        return Some(name);
                    }
                }
            }
            continue;
        }
        if class_section_seen {
            break;
        }
        in_target_vendor = split_entry(line).is_some_and(|(id, _)| id == vendor_id);
    }
    None
}

/// 切出 `64a0  Arc Graphics ...` 里的 id 和名字。
fn split_entry(line: &str) -> Option<(String, String)> {
    let (id, name) = line.split_once(char::is_whitespace)?;
    let name = name.trim();
    if id.len() != 4 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) || name.is_empty() {
        return None;
    }
    Some((id.to_ascii_lowercase(), name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份迷你名字表，形状与真实文件一致（含子系统行和类段）。
    const DATABASE: &str = "\
# 注释行
#	被注释的设备
8086  Intel Corporation
\t0007  82379AB
\t64a0  Core Ultra 200V Series Processors Arc Graphics 130V/140V GPU
\t\t17aa 3da2  ThinkPad SoC
\t643e  Core Ultra 200V Series Processors NPU
10de  NVIDIA Corporation
\t2204  GA102 [GeForce RTX 3090]
C 00  Unclassified device
\t00  Non-VGA unclassified device
";

    #[test]
    fn finds_device_names_and_skips_subsystem_lines() {
        assert_eq!(
            lookup_in(DATABASE, "8086", "64a0").as_deref(),
            Some("Core Ultra 200V Series Processors Arc Graphics 130V/140V GPU")
        );
        assert_eq!(
            lookup_in(DATABASE, "8086", "643e").as_deref(),
            Some("Core Ultra 200V Series Processors NPU")
        );
        // 换到另一个厂商段仍然能查
        assert_eq!(
            lookup_in(DATABASE, "10de", "2204").as_deref(),
            Some("GA102 [GeForce RTX 3090]")
        );
        // 子系统行（两个 tab）不能被当成设备：17aa 是子系统厂商，不是设备 id
        assert_eq!(lookup_in(DATABASE, "8086", "3da2"), None);
    }

    #[test]
    fn does_not_leak_across_vendor_sections() {
        // 10de 段里的 id 不能被当成 8086 的设备
        assert_eq!(lookup_in(DATABASE, "8086", "2204"), None);
        // 不存在的厂商
        assert_eq!(lookup_in(DATABASE, "1002", "64a0"), None);
    }

    #[test]
    fn class_section_does_not_masquerade_as_a_vendor() {
        // 类段里的 `00` 是设备类，不是厂商 0000 下的设备
        assert_eq!(lookup_in(DATABASE, "0000", "00"), None);
    }

    #[test]
    fn id_normalization_is_strict() {
        assert_eq!(normalize_id("0x64a0").as_deref(), Some("64a0"));
        assert_eq!(normalize_id("64A0").as_deref(), Some("64a0"));
        assert_eq!(normalize_id(" 64a0 ").as_deref(), Some("64a0"));
        // 长度或字符不对就不查表——宁可没有名字，也不要一个错的名字
        assert_eq!(normalize_id("64a"), None);
        assert_eq!(normalize_id("64a00"), None);
        assert_eq!(normalize_id("0xzzzz"), None);
        assert_eq!(normalize_id(""), None);
        assert_eq!(normalize_id("未知"), None);
    }

    #[test]
    fn missing_database_yields_none_not_an_error() {
        let root = std::env::temp_dir().join(format!("deviceinfo-pci-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        assert_eq!(lookup(&root, "0x8086", "0x64a0"), None);
        fs::remove_dir_all(&root).ok();
    }
}
