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
use crate::report::AcceleratorKind;
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

/// 采样选项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SampleOptions {
    /// 要查磁盘余量的路径。
    pub watch: Vec<PathBuf>,
    /// 是否读取**累积计数器**（目前只有 NPU 的 `npu_busy_time_us`）。
    ///
    /// **默认关**，这是有意为之：ivpu 驱动文档写着它
    /// > shouldn't be read too often as it may have an impact on job submission
    /// > performance，推荐周期 _1 second_
    ///
    /// 一个默认开启的 API 会让高频轮询的面板在无意中拖慢 NPU 作业提交，
    /// 而且这种损害在数据里看不出来。要拿这个值就先把轮询周期调到 ≥1 秒。
    pub counters: bool,
}

/// 一台加速器的瞬时指标。
///
/// 与 [`crate::Accelerator`] 的分工：那边是"这设备是什么"，这里是"它现在在什么状态"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceleratorState {
    pub kind: AcceleratorKind,
    /// 设备在 sysfs 里的名字：`accel0` / `card1`。
    ///
    /// 为什么需要它：平台设备（SoC 上集成的显示/图形单元）**没有 PCI 标识**，
    /// 于是一台有 3 个显示控制器的机器会给出 3 条一模一样的记录，只有按列表位置
    /// 才能区分——而"按位置对应"正是本模块在别处刻意避开的做法（见 `renderD` 的归属）。
    /// 这个名字是**每个设备唯一**的，而且人也能在 sysfs 里对上。
    pub node: String,
    /// 与硬件报告里同一台设备的连接键。
    ///
    /// 用 PCI 标识而不是下标：列表顺序不是契约，而 `8086:643e` 是。
    /// 没有 PCI 的加速器（ARM 上的 NPU 之类）这里是 `None`，只能靠 [`Self::kind`] 对应。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_id: Option<crate::report::PciId>,
    /// 当前频率（MHz）。
    ///
    /// ⚠️ **`Some(0)` 的含义是"设备空闲"，不是"读不到"**。ivpu 驱动文档：
    /// `freq/current_freq` 只在设备活跃时有效，空闲时返回 0。
    /// 把它当成未知是个真实的错误：空闲的 NPU 频率确实就是 0。
    /// 读不到才是 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_freq_mhz: Option<u64>,
    /// 常驻内存（字节）。
    ///
    /// NPU 上是 `npu_memory_utilization`（驱动文档确认**单位就是字节**，
    /// 指当前常驻的 NPU 内存总量）；GPU 上是已用显存（驱动暴露 `mem_info_vram_used` 时）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resident_memory_bytes: Option<u64>,
    /// 累积忙碌时间（微秒）。只有 [`SampleOptions::counters`] 打开时才读，
    /// 理由见那个字段的文档。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy_time_us: Option<u64>,
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
    /// 加速器的瞬时指标。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accelerators: Vec<AcceleratorState>,
    /// 采样过程中的不确定之处。
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// 采样一次运行时状态。
///
/// `root` 是 `/proc` 与 `/sys` 的注入前缀（测试用假文件树）；`options.watch` 里的路径
/// **不经过它**——`statvfs` 查的是真实挂载的文件系统，对着假文件树问"这块盘还剩多少"
/// 没有意义。
pub(crate) fn sample(root: &Path, options: &SampleOptions) -> RuntimeState {
    let mut warnings = Vec::new();
    let memory = sample_memory(root, &mut warnings);
    let accelerators = sample_accelerators(root, options, &mut warnings);

    let mut disks = Vec::new();
    for path in &options.watch {
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
        accelerators,
        warnings,
    }
}

/// 枚举加速器的 sysfs 设备目录。
///
/// 这份发现逻辑和 [`crate::accelerator`] 里的是两份实现（那边需要更多上下文，
/// 拆出来反而难读），靠 `state_and_hardware_agree_on_which_devices_exist`
/// 这个测试保证两边不会走偏——**包括顺序**：两边都按设备名排序，
/// 所以第 N 个状态对应第 N 个设备。
fn accelerator_devices(root: &Path) -> Vec<(AcceleratorKind, String, PathBuf)> {
    // 顺序必须由名字决定，不能由目录项顺序决定：否则状态列表和硬件列表对不上，
    // 而两份都声称自己在描述同一批设备
    let mut entries: Vec<(String, AcceleratorKind, String, PathBuf)> = Vec::new();
    if let Ok(dir) = fs::read_dir(root.join("dev/accel")) {
        for entry in dir.flatten() {
            let node = entry.file_name().to_string_lossy().into_owned();
            if node.starts_with("accel") {
                let device = root.join("sys/class/accel").join(&node).join("device");
                entries.push((
                    format!("0accel/{node}"),
                    AcceleratorKind::Npu,
                    node.clone(),
                    device,
                ));
            }
        }
    }
    if let Ok(dir) = fs::read_dir(root.join("sys/class/drm")) {
        for entry in dir.flatten() {
            let node = entry.file_name().to_string_lossy().into_owned();
            // `cardN-DP-1` 是显示连接器，不是 GPU
            if node.starts_with("card") && !node.contains('-') {
                let device = entry.path().join("device");
                // 类别必须和硬件探测用**同一套**判据：ARM 上的 RKNPU 走 DRM，
                // 硬件那边认成 NPU，这里不能认成 GPU——那样两份报告的第 N 项就不是同一个设备
                let kind = crate::accelerator::classify_drm_device(
                    crate::accelerator::driver_of(&device).as_deref(),
                    crate::accelerator::read_compatible(&device).as_deref(),
                    // 和硬件探测用同一个三态判据；这里也要求"看到了 drm 目录"
                    device
                        .join("drm")
                        .is_dir()
                        .then(|| crate::accelerator::render_nodes_of(&device).len()),
                );
                entries.push((
                    format!("1drm/{node}"),
                    kind,
                    node.clone(),
                    device,
                ));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
        .into_iter()
        .map(|(_, kind, node, dir)| (kind, node, dir))
        .collect()
}

fn sample_accelerators(
    root: &Path,
    options: &SampleOptions,
    warnings: &mut Vec<String>,
) -> Vec<AcceleratorState> {
    let mut found = Vec::new();
    for (kind, node, device_dir) in accelerator_devices(root) {
        // 设备节点在、但 sysfs 目录读不到：实例指标会全是 None，而"全是 None"
        // 与"设备真的没有这些指标"看起来一样，得说出来
        if !device_dir.exists() {
            warnings.push(format!(
                "加速器 {kind:?} 的 sysfs 目录 {} 不存在，瞬时指标不可用",
                device_dir.display()
            ));
        }
        let mut state = AcceleratorState {
            kind,
            node,
            pci_id: crate::accelerator::read_pci_id(&device_dir),
            current_freq_mhz: current_freq_mhz(&device_dir, kind),
            resident_memory_bytes: resident_memory_bytes(&device_dir),
            busy_time_us: None,
        };
        if options.counters {
            // 驱动建议间隔 ≥1 秒，见 SampleOptions::counters
            state.busy_time_us = crate::sysfs::read_u64(&device_dir.join("npu_busy_time_us"));
        }
        found.push(state);
    }
    found
}

/// 当前频率（MHz）。
///
/// NPU 优先读 `freq/current_freq`：驱动文档把 `npu_*_frequency_mhz` 明确标为
/// **Legacy attributes (backward compatibility)**，先读新路径、旧路径当兜底。
fn current_freq_mhz(device_dir: &Path, kind: AcceleratorKind) -> Option<u64> {
    if kind == AcceleratorKind::Npu {
        return crate::sysfs::read_u64(&device_dir.join("freq/current_freq"))
            .or_else(|| crate::sysfs::read_u64(&device_dir.join("npu_current_frequency_mhz")));
    }
    // xe 把 GPU 按 tileN/gtN 组织；i915 的布局不一样
    for tile in crate::accelerator::numbered_dirs(device_dir, "tile") {
        for gt in crate::accelerator::numbered_dirs(&tile, "gt") {
            if let Some(freq) = crate::sysfs::read_u64(&gt.join("freq0/cur_freq")) {
                return Some(freq);
            }
        }
    }
    ["gt/gt0/rps_cur_freq_mhz", "gt_cur_freq_mhz"]
        .iter()
        .find_map(|path| crate::sysfs::read_u64(&device_dir.join(path)))
}

/// 常驻内存（字节）。
fn resident_memory_bytes(device_dir: &Path) -> Option<u64> {
    // NPU 的 npu_memory_utilization 单位就是字节（驱动文档原话：report in bytes）
    crate::sysfs::read_u64(&device_dir.join("npu_memory_utilization"))
        // amdgpu 的已用显存
        .or_else(|| crate::sysfs::read_u64(&device_dir.join("mem_info_vram_used")))
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

        let state = sample(
            &root,
            &SampleOptions {
                watch: vec![PathBuf::from("/definitely/not/here")],
                counters: false,
            },
        );
        assert!(state.disks.is_empty());
        assert_eq!(state.warnings.len(), 1, "{:#?}", state.warnings);
        assert!(state.warnings[0].contains("not/here"), "{:#?}", state.warnings);

        fs::remove_dir_all(&root).ok();
    }

    /// 同类设备（比如三个显示控制器）必须能靠 `node` 区分开，不能只有列表位置——
    /// 平台设备没有 PCI 标识，否则三条记录看起来一模一样。
    #[test]
    fn same_kind_devices_are_distinguishable_by_node() {
        let root = fake_root("node-identity");
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();
        for card in ["card0", "card1", "card2"] {
            fs::create_dir_all(root.join(format!("sys/class/drm/{card}/device/drm"))).unwrap();
        }

        let state = sample(&root, &SampleOptions::default());
        let nodes: Vec<&str> = state
            .accelerators
            .iter()
            .map(|accel| accel.node.as_str())
            .collect();
        assert_eq!(nodes, vec!["card0", "card1", "card2"]);

        fs::remove_dir_all(&root).ok();
    }

    /// 设备节点在、sysfs 目录却读不到时要说出来：否则瞬时指标全是 `None`，
    /// 而那和"这台设备真的没有这些指标"看起来一样。
    #[test]
    fn a_missing_sysfs_dir_is_reported_not_silently_empty() {
        let root = fake_root("missing-sysfs");
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();
        // 有设备节点，但没有对应的 sys/class 条目
        fs::create_dir_all(root.join("dev/accel")).unwrap();
        fs::write(root.join("dev/accel/accel0"), "").unwrap();

        let state = sample(&root, &SampleOptions::default());
        assert_eq!(state.accelerators.len(), 1);
        assert_eq!(state.accelerators[0].current_freq_mhz, None);
        assert!(
            state.warnings.iter().any(|w| w.contains("sysfs")),
            "{:#?}",
            state.warnings
        );

        fs::remove_dir_all(&root).ok();
    }

    /// 手动搭一台只有 NPU 的假机器，值取自真机实测。
    fn fake_npu(root: &Path) {
        fs::create_dir_all(root.join("dev/accel")).unwrap();
        fs::write(root.join("dev/accel/accel0"), "").unwrap();
        let device = root.join("sys/class/accel/accel0/device");
        fs::create_dir_all(device.join("freq")).unwrap();
        fs::write(device.join("freq/current_freq"), "0\n").unwrap();
        fs::write(device.join("npu_memory_utilization"), "68714496\n").unwrap();
        fs::write(device.join("npu_busy_time_us"), "88694865\n").unwrap();
        fs::write(device.join("vendor"), "0x8086\n").unwrap();
        fs::write(device.join("device"), "0x643e\n").unwrap();
    }

    /// 驱动在设备空闲时报 0（文档：`freq/current_freq` 只在设备活跃时有效）。
    /// 把它当成"读不到"是个真实的错误：空闲的 NPU 频率确实就是 0。
    #[test]
    fn npu_idle_reports_zero_not_unknown() {
        let root = fake_root("npu-idle");
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();
        fake_npu(&root);

        let state = sample(&root, &SampleOptions::default());
        assert_eq!(state.accelerators.len(), 1);
        let npu = &state.accelerators[0];
        assert_eq!(npu.current_freq_mhz, Some(0), "0 是空闲，不是未知");
        // npu_memory_utilization 的单位是字节（驱动文档原话：report in bytes）
        assert_eq!(npu.resident_memory_bytes, Some(68_714_496));
        // 用 PCI 标识当连接键，而不是靠列表下标
        assert_eq!(npu.pci_id.as_ref().unwrap().compact(), "8086:643e");

        fs::remove_dir_all(&root).ok();
    }

    /// 累积计数器默认**不读**：驱动文档说它不宜频繁读取（会影响作业提交性能）。
    /// 一个默认开启的 API 会让高频轮询的面板在无意中拖慢 NPU。
    #[test]
    fn cumulative_counters_need_an_explicit_opt_in() {
        let root = fake_root("counters");
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();
        fake_npu(&root);

        let quiet = sample(&root, &SampleOptions::default());
        assert_eq!(
            quiet.accelerators[0].busy_time_us, None,
            "默认就不该读它，即使文件明明存在"
        );

        let verbose = sample(
            &root,
            &SampleOptions {
                watch: Vec::new(),
                counters: true,
            },
        );
        assert_eq!(verbose.accelerators[0].busy_time_us, Some(88_694_865));

        fs::remove_dir_all(&root).ok();
    }
}
