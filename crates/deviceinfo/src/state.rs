//! 运行时状态采样。
//!
//! # 为什么和 [`crate::probe`] 分开
//!
//! 两者的**生命周期完全不同**：
//!
//! | | 硬件报告 | 运行时状态 |
//! |---|---|---|
//! | 描述 | "这台机器是什么" | "此刻怎样" |
//! | 变化频率 | 装上就不变 | 每一秒都在变 |
//! | 能否缓存 | 能 | 不能 |
//! | 能否跨设备比较 | 能 | 不能 |
//!
//! 混在一起会让硬件报告既不能缓存、也不能拿两台机器直接 `diff`。
//!
//! # 一个具体的陷阱
//!
//! [`MemoryState::available_bytes`] 来自内核的 `MemAvailable`，它是**估算**：
//! 内核把"能换出去"的部分也算进可用。swap 已经用满时，这个数字会**系统性高估**
//! 还能装下多少——实测某台机器 swap 4 GiB 已用满，而 `MemAvailable` 仍报 9 GiB。
//! [`MemoryState::swap_exhausted`] 就是为这个场景准备的信号。

use crate::sysfs::kib_field;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 内存与交换。**全部是瞬时值。**
///
/// 用 `Option` 而不是拿 0 顶替：`Some(0)` 表示"确实没有"（比如没有 swap），
/// `None` 表示"读不到"。两者混淆会让上层把"读不到"当成"零可用"而拒绝一切任务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryState {
    pub total_bytes: Option<u64>,
    /// 内核的 `MemAvailable`。**是估算，不是空闲**。
    pub available_bytes: Option<u64>,
    /// 交换分区总量。`Some(0)` 表示这台机器没有 swap。
    pub swap_total_bytes: Option<u64>,
    pub swap_free_bytes: Option<u64>,
}

impl MemoryState {
    /// 可用于加载模型的内存（保守取 available，取不到退回 total）。
    pub fn usable_memory_bytes(&self) -> Option<u64> {
        self.available_bytes.or(self.total_bytes)
    }

    /// swap 是否已经基本用满（余量不足 5%）。
    ///
    /// 为真是重要信号：说明系统正在换页，此时 [`Self::available_bytes`] 会高估。
    /// 没有 swap 或读不到时为假——那种情况轮不到这个信号说话。
    pub fn swap_exhausted(&self) -> bool {
        match (self.swap_total_bytes, self.swap_free_bytes) {
            (Some(total), Some(free)) if total > 0 => free.saturating_mul(20) < total,
            _ => false,
        }
    }
}

/// 某个路径所在文件系统的余量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    /// 被查询的路径。
    pub path: PathBuf,
    pub total_bytes: u64,
    /// 文件系统层面的空闲，**包含** root 保留块。
    pub free_bytes: u64,
    /// **当前用户实际可写**的量，不含 root 保留块。
    ///
    /// 判断"装不装得下"要用这个：ext4 默认给 root 留 5%，拿 `free_bytes` 判断
    /// 会让普通用户以为还能装下但实际上写不进去。
    pub available_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeState {
    pub memory: MemoryState,
    /// 被查询路径所在文件系统的余量。空表示没指定要查的路径。
    ///
    /// **不做去重**：`/` 和 `/var/cache` 可能落在同一个文件系统上，但调用方问的是
    /// 两个具体路径，报告里就如实出现两条——静默合并会让"我的缓存目录到底在哪块盘上"
    /// 这个本来想问的问题失去答案。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<DiskUsage>,
    /// 采样过程中的不确定之处。
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// 采样一次运行时状态。
///
/// `root` 是 `/proc` 的注入前缀（测试用假文件树）；`watch` 里的路径**不经过它**——
/// `statvfs` 查的是真实挂载的文件系统，对着假文件树问"这块盘还剩多少"没有意义。
pub(crate) fn sample(root: &Path, watch: &[PathBuf]) -> RuntimeState {
    let mut warnings = Vec::new();
    let memory = sample_memory(root, &mut warnings);

    let mut disks = Vec::new();
    for path in watch {
        match filesystem_usage(path) {
            Ok(usage) => disks.push(DiskUsage {
                path: path.clone(),
                ..usage
            }),
            Err(error) => warnings.push(format!("查不到 {} 的余量: {error}", path.display())),
        }
    }

    RuntimeState {
        memory,
        disks,
        warnings,
    }
}

fn sample_memory(root: &Path, warnings: &mut Vec<String>) -> MemoryState {
    let path = root.join("proc/meminfo");
    let Ok(text) = fs::read_to_string(&path) else {
        warnings.push(format!("读不到 {}，内存状态缺失", path.display()));
        return MemoryState {
            total_bytes: None,
            available_bytes: None,
            swap_total_bytes: None,
            swap_free_bytes: None,
        };
    };
    if kib_field(&text, "MemTotal").is_none() {
        warnings.push(format!("{} 里没有 MemTotal", path.display()));
    }
    MemoryState {
        total_bytes: kib_field(&text, "MemTotal"),
        available_bytes: kib_field(&text, "MemAvailable"),
        swap_total_bytes: kib_field(&text, "SwapTotal"),
        swap_free_bytes: kib_field(&text, "SwapFree"),
    }
}

/// `statvfs` 的安全包装。
///
/// 标准库没有 `statvfs`（`std::fs::statvfs` 至今不存在），所以要么用 `libc`，
/// 要么拉起一个 `df` 子进程去解析输出——后者更糟：多一个外部依赖、输出格式要解析、
/// 还要处理不同发行版的 `df` 方言。
fn filesystem_usage(path: &Path) -> io::Result<DiskUsage> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径里含 NUL"))?;
    // SAFETY: `c_path` 是刚构造出来的合法 C 字符串，`stats` 是可写的局部变量，
    // `statvfs` 只在这两个指针上读写，不会保留它们。
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
        return Err(io::Error::last_os_error());
    }

    // f_frsize 是"基本块大小"，算字节数要用它而不是 f_bsize（后者是 I/O 块大小）。
    // 取 max(1) 是防 0：某些伪文件系统会报 0，除零会 panic，乘 0 会给出假的全零。
    let block = (stats.f_frsize as u64).max(1);
    Ok(DiskUsage {
        path: path.to_path_buf(),
        total_bytes: (stats.f_blocks as u64).saturating_mul(block),
        free_bytes: (stats.f_bfree as u64).saturating_mul(block),
        available_bytes: (stats.f_bavail as u64).saturating_mul(block),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("deviceinfo-state-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proc")).unwrap();
        root
    }

    #[test]
    fn swap_exhausted_is_the_signal_for_overestimated_available_memory() {
        // 本机实测：SwapTotal 4 GiB、SwapFree 只剩 1.5 MB
        let state = MemoryState {
            total_bytes: Some(33_129_861_120),
            available_bytes: Some(9_945_219_072),
            swap_total_bytes: Some(4_294_963_200),
            swap_free_bytes: Some(1_552_384),
        };
        assert!(state.swap_exhausted());
        assert_eq!(state.usable_memory_bytes(), Some(9_945_219_072));

        // 有 swap 且余量充足 → 不是"用满"
        let healthy = MemoryState {
            swap_total_bytes: Some(4_294_963_200),
            swap_free_bytes: Some(3_000_000_000),
            ..state.clone()
        };
        assert!(!healthy.swap_exhausted());

        // 没有 swap：轮不到这个信号说话，不能报"用满"
        let no_swap = MemoryState {
            swap_total_bytes: Some(0),
            swap_free_bytes: Some(0),
            ..state.clone()
        };
        assert!(!no_swap.swap_exhausted());

        // 读不到 swap：同样不报
        let unknown = MemoryState {
            swap_total_bytes: None,
            swap_free_bytes: None,
            ..state
        };
        assert!(!unknown.swap_exhausted());
    }

    #[test]
    fn missing_values_stay_none_instead_of_becoming_zero() {
        let root = fake_root("partial");
        // 只有 MemTotal，没有 MemAvailable 也没有 swap 字段
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();

        let mut warnings = Vec::new();
        let state = sample_memory(&root, &mut warnings);
        assert_eq!(state.total_bytes, Some(1_024_000));
        assert_eq!(state.available_bytes, None);
        assert_eq!(state.swap_total_bytes, None);
        // 退回到总量，而不是 0
        assert_eq!(state.usable_memory_bytes(), Some(1_024_000));
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_meminfo_warns_and_yields_all_none() {
        let root = fake_root("empty");
        let mut warnings = Vec::new();
        let state = sample_memory(&root, &mut warnings);
        assert_eq!(state, MemoryState {
            total_bytes: None,
            available_bytes: None,
            swap_total_bytes: None,
            swap_free_bytes: None,
        });
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn real_filesystem_usage_is_plausible() {
        let root = fake_root("statvfs");
        let usage = filesystem_usage(&root).expect("临时目录一定查得到");
        assert!(usage.total_bytes > 0);
        // f_bavail <= f_bfree 恒成立：后者含 root 保留块
        assert!(
            usage.available_bytes <= usage.free_bytes,
            "{usage:#?}"
        );
        assert!(usage.free_bytes <= usage.total_bytes, "{usage:#?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unwatchable_path_is_a_warning_not_a_failure() {
        let root = fake_root("nowatch");
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();

        let state = sample(&root, &[PathBuf::from("/definitely/not/here")]);
        assert!(state.disks.is_empty());
        assert_eq!(state.warnings.len(), 1, "{:#?}", state.warnings);
        assert!(state.warnings[0].contains("not/here"), "{:#?}", state.warnings);

        fs::remove_dir_all(&root).ok();
    }
}
