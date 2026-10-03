//! 读取辅助。
//!
//! 所有探测都通过一个 `root` 前缀读文件，而不是写死 `/`：这样单元测试可以造一棵
//! 假文件树，容器探测和真实机器采样夹具也走同一条代码路径。

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 读一个文本文件并去掉首尾空白。读不到返回 `None`（调用方决定是警告还是留空）。
pub(crate) fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// 读一个十进制整数文件（sysfs 里的数字几乎都带一个尾随换行）。
pub(crate) fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

/// 从 `key : value` 形式的文本里取值。
///
/// cpuinfo 的键宽不固定（`model name` / `Features` / `MemTotal`），分隔符也时有时无空格，
/// 所以按冒号切分再 trim，不做列对齐假设。
pub(crate) fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.split_once(':').filter(|(k, _)| k.trim() == key))
        .map(|(_, value)| value.trim())
}

/// 读设备树属性。
///
/// 这类文件是 **NUL 结尾的字节串**，而且可能是 NUL 分隔的**多个值**
/// （`compatible` 就是这么一张按优先级排列的列表）。取第一个非空值。
///
/// 不能用 `read_to_string` + `trim`：`trim` 不会去掉 NUL。
pub(crate) fn read_dt_property(path: &Path) -> Option<String> {
    let raw = fs::read(path).ok()?;
    String::from_utf8_lossy(&raw)
        .split('\0')
        .map(str::trim)
        .find(|part| !part.is_empty())
        .map(str::to_string)
}

/// 读 `<值> kB` 形式的字段并转成字节。
///
/// `/proc/meminfo` 的所有值都以 kB 为单位。数值解析失败返回 `None`，不填 0 冒充。
pub(crate) fn kib_field(text: &str, key: &str) -> Option<u64> {
    field(text, key)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}

/// 在探测根内解析输入路径，绝对软链目标也相对于该根。
///
/// 供探测与采集共用；缺路径或软链循环返回错误。
/// 这是文件树读取辅助，不提供抵御并发路径替换的安全隔离。
pub fn resolve_path_in_root(root: &Path, relative: &str) -> io::Result<PathBuf> {
    let mut pending: VecDeque<_> = Path::new(relative)
        .components()
        .map(|part| part.as_os_str().to_os_string())
        .collect();
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if part == "/" {
            resolved.clear();
        } else if part == ".." {
            resolved.pop();
        } else if part != "." {
            match fs::read_link(root.join(&resolved).join(&part)) {
                Ok(target) => {
                    links += 1;
                    if links > 40 {
                        return Err(io::Error::other("输入路径软链循环或超过 40 层"));
                    }
                    for component in target.components().rev() {
                        pending.push_front(component.as_os_str().to_os_string());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => resolved.push(part),
                // **错误里必须带上是哪一段**：这条路径上逐段解析、动辄上百个输入，
                // 一个裸的 "Permission denied (os error 13)" 等于没有线索
                // （本机以非 root 采 `/` 时会撞上 root-only 的目录）。
                Err(error) => {
                    let at = root.join(&resolved).join(&part);
                    return Err(io::Error::new(
                        error.kind(),
                        format!("解析 {} 失败：{error}", at.display()),
                    ));
                }
            }
        }
    }
    Ok(root.join(resolved))
}
