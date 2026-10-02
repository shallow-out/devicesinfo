//! 读取辅助。
//!
//! 所有探测都通过一个 `root` 前缀读文件，而不是写死 `/`：这样单元测试可以造一棵
//! 假文件树，容器探测和真实机器采样夹具也走同一条代码路径。

use std::fs;
use std::path::Path;

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
