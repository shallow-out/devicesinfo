//! 加速器探测：GPU 与 NPU。
//!
//! 这一层填三类事实：**设备是什么**（名字、PCI id、驱动）、**内存是什么语义**、
//! **用户态运行时是否就绪**。三者分开的原因见 [`crate::report`] 各字段的文档。
//!
//! 刻意不做的：不读 `cur_freq` / `busy_time` / `memory_utilization` 这类瞬时值。
//! 它们是运行时状态，混进硬件报告会让这份报告既不能缓存也不能跨设备比较。

use crate::pci;
use crate::report::{
    Accelerator, AcceleratorKind, AcceleratorMemory, PciId,
};
use crate::runtime::{self, LibraryIndex};
use crate::sysfs::{read_dt_property, read_trimmed, read_u64};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// PCI vendor id → 厂商名。
///
/// 只覆盖推理场景里会遇到的厂商；认不出来就原样返回 id，不猜。
fn vendor_name(vendor_id: &str) -> String {
    match vendor_id.to_ascii_lowercase().as_str() {
        "0x8086" | "8086" => "Intel".into(),
        "0x10de" | "10de" => "NVIDIA".into(),
        "0x1002" | "1002" | "0x1022" => "AMD".into(),
        other => other.to_string(),
    }
}

/// sysfs 的 `driver` 是个指向驱动模块目录的软链，取末段就是驱动名。
///
/// 读软链而不是读 `uevent` 的 `DRIVER=`：前者在设备未绑定驱动时直接不存在，
/// 后者可能残留旧值。
///
/// `pub(crate)`：状态采样要用它和 [`classify_drm_device`] 得到**同样的**类别，
/// 否则两边报告的第 N 项会对不上。
pub(crate) fn driver_of(device_dir: &Path) -> Option<String> {
    let link = fs::read_link(device_dir.join("driver")).ok()?;
    link.file_name().map(|s| s.to_string_lossy().into_owned())
}

/// 驱动版本，从 `<root>/sys/module/<驱动>/version` 读。
///
/// 只有部分驱动暴露这个文件：`intel_vpu` 有，`xe`/`i915`/`amdgpu` 都没有。
/// 拿不到就是 `None`，表示"驱动没暴露"，不表示"没版本"。
fn driver_version_of(root: &Path, driver: Option<&str>) -> Option<String> {
    let driver = driver?;
    read_trimmed(&root.join("sys/module").join(driver).join("version"))
}

/// 从 sysfs 读 PCI 标识。GPU 和 NPU 都有这两个文件。
///
/// `pub(crate)`：状态采样要用它当连接键，把两边报告的同一个设备对起来。
pub(crate) fn read_pci_id(device_dir: &Path) -> Option<PciId> {
    Some(PciId {
        vendor: read_trimmed(&device_dir.join("vendor"))?,
        device: read_trimmed(&device_dir.join("device"))?,
    })
}

/// 设备名：优先 `pci.ids` 查到的正式名，查不到才退回 id 形式。
///
/// 名字表可能缺失或过时，所以**原始 id 由 [`Accelerator::pci_id`] 单独承载**，
/// 不塞进名字字符串里——那样只能显示、不能查询。
fn name_from_pci(
    root: &Path,
    vendor: &Option<String>,
    pci_id: &Option<PciId>,
    fallback: impl FnOnce() -> String,
) -> String {
    let looked_up = pci_id
        .as_ref()
        .and_then(|id| pci::lookup(root, &id.vendor, &id.device));
    match (vendor, looked_up) {
        (Some(vendor), Some(device)) => format!("{vendor} {device}"),
        (None, Some(device)) => device,
        _ => fallback(),
    }
}

/// 某个目录下形如 `<prefix><数字>` 的子目录，按编号排序。
///
/// xe 驱动把 GPU 按 `tile0/gt0`、`tile0/gt1` 组织，编号和数量都不固定，
/// 硬编码 `tile0/gt0` 在双 tile 或换代的机器上会静默失效。
///
/// `pub(crate)`：状态采样读当前频率时要走同一套布局。
pub(crate) fn numbered_dirs(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(usize, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let index = name.strip_prefix(prefix)?.parse::<usize>().ok()?;
            Some((index, entry.path()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, path)| path).collect()
}

/// GPU 频率上限（MHz）。
///
/// 驱动布局不统一：xe 是 `tileN/gtN/freq0/max_freq`（本机实测 1950），
/// i915 是 `gt/gt0/rps_max_freq_mhz`，更老的还有 `gt_max_freq_mhz`。
/// **只取上限**：同目录下的 `cur_freq` / `act_freq` 是瞬时值。
fn gpu_max_freq_mhz(device_dir: &Path) -> Option<u64> {
    for tile in numbered_dirs(device_dir, "tile") {
        for gt in numbered_dirs(&tile, "gt") {
            if let Some(freq) = read_u64(&gt.join("freq0/max_freq")) {
                return Some(freq);
            }
        }
    }
    [
        "gt/gt0/rps_max_freq_mhz",
        "gt/gt0/max_freq_mhz",
        "gt_max_freq_mhz",
    ]
    .iter()
    .find_map(|path| read_u64(&device_dir.join(path)))
}

/// GPU 可用的内存语义。
fn gpu_memory(device_dir: &Path, vendor: Option<&str>) -> AcceleratorMemory {
    // amdgpu 一直暴露 mem_info_vram_total；Intel 的 xe 在独显上也暴露。
    // 有它就说明有独立显存，这是唯一能确定"独立"的证据。
    if let Some(bytes) = read_u64(&device_dir.join("mem_info_vram_total")) {
        if bytes > 0 {
            return AcceleratorMemory::Dedicated { bytes };
        }
    }
    match vendor {
        // 走到这里说明驱动没暴露显存总量。Intel / AMD 在 Linux 上绝大多数是集显，
        // 集显没有独立显存、和 CPU 共享系统内存——这是**确定的语义**，不是"不知道"。
        Some("Intel") | Some("AMD") => AcceleratorMemory::SharedWithSystem,
        // NVIDIA 专有驱动不通过 sysfs 暴露显存（要 NVML）。这里不猜：
        // 编一个数会让人装上装不下的模型。
        Some(other) => AcceleratorMemory::Unknown {
            reason: format!("{other} 驱动未通过 sysfs 暴露显存上限"),
        },
        None => AcceleratorMemory::Unknown {
            reason: "厂商未识别，无法判断显存是独立还是共享".into(),
        },
    }
}

/// 枚举一个目录下匹配前缀的条目，**按名字排序**。
///
/// `read_dir` 的顺序取决于文件系统的目录项排列，不是内容的函数。实测同一份内容
/// 用 `cp -a` 复制与逐文件重建，得到的顺序就不一样——那会让 `--json` 输出无法跨机器
/// `diff`（本 crate 的核心用途），也会让按下标做的 `renderD` 配对随机出错。
/// 凡是“设备列表”都必须过这里。
fn sorted_entries(dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            keep(&name).then(|| (name, entry.path()))
        })
        .collect();
    found.sort();
    found
}

/// 设备树 `compatible`：非 PCI 平台上设备的权威标识。
///
/// `pub(crate)`：理由同 [`driver_of`]。
pub(crate) fn read_compatible(device_dir: &Path) -> Option<String> {
    read_dt_property(&device_dir.join("of_node/compatible"))
}

/// 已知走 DRM 而不是 `/dev/accel` 暴露的 NPU 驱动。
///
/// 这是个**有界白名单**，和别处"不列白名单"的原则相反——因为这里没有结构性的替代
/// 信号：在 DRM 层面 NPU 和 GPU 长得一模一样。好在绝大多数这类命名里就带 `npu`
/// （`RKNPU`、`rockchip,rk3588-rknpu`），子串匹配就能盖住，这张表只做补充。
const DRM_NPU_DRIVERS: [&str; 1] = ["rknpu"];

/// 判断一个 DRM 设备是 NPU 还是 GPU。
///
/// **不能一律当 GPU。** Rockchip 的 RKNPU 走 DRM 暴露，于是一台**确实有 NPU**
/// 的机器上，`has_npu()` 会返回 `false`——上层再也不会考虑 NPU 卸载，而且没有任何报错。
/// 实测那台机器上它出现在 `/sys/class/drm/card0`，驱动 `RKNPU`，
/// compatible `rockchip,rk3588-rknpu`。
///
/// `pub(crate)`：状态采样必须用**同一套**判据，否则两份报告里第 N 个设备不是同一个
/// （这个分歧真的发生过，被 `state_and_hardware_agree_on_which_devices_exist` 抓到）。
pub(crate) fn classify_drm_device(driver: Option<&str>, compatible: Option<&str>) -> AcceleratorKind {
    let identity = format!(
        "{} {}",
        driver.unwrap_or_default(),
        compatible.unwrap_or_default()
    )
    .to_ascii_lowercase();
    if identity.contains("npu") || DRM_NPU_DRIVERS.iter().any(|name| identity.contains(name)) {
        return AcceleratorKind::Npu;
    }
    AcceleratorKind::Gpu
}

/// 一台 DRM 设备**自己**的 render 节点名（`renderD128` 这类）。
///
/// `<device_dir>/drm/` 列的就是该设备自己的 `cardN` / `renderD*` / `controlD*` 条目，
/// 这是 sysfs 里唯一**直接**给出归属关系的地方。旧代码按"第 N 个 render 节点配
/// 第 N 个 card"配对，实测那台 ARM 机器上 renderD128 属于 card0，
/// 而配对给了 card1——就算排序之后碰巧对了，规则本身也不成立。
///
/// 也刻意不去解 `sys/class/drm/renderD*` 的软链：软链在夹具里存不下来。
fn render_nodes_of(device_dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(device_dir.join("drm"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("renderD"))
        .collect();
    found.sort();
    found
}

/// NPU：现代内核把 NPU 暴露在 `/dev/accel/accelN`。
pub(crate) fn probe_npus(
    root: &Path,
    libraries: &LibraryIndex,
    warnings: &mut Vec<String>,
) -> Vec<Accelerator> {
    let dir = root.join("dev/accel");
    let mut found = Vec::new();
    for (node, node_path) in sorted_entries(&dir, |name| name.starts_with("accel")) {
        let sys = root.join("sys/class/accel").join(&node);
        let device_dir = sys.join("device");
        let vendor = read_trimmed(&device_dir.join("vendor")).map(|v| vendor_name(&v));
        let driver = driver_of(&device_dir);
        let pci_id = read_pci_id(&device_dir);
        let runtime = runtime::probe(AcceleratorKind::Npu, vendor.as_deref(), libraries);
        // 先算成局部变量：struct 字面量里字段是按书写顺序求值的，
        // 把"借 vendor"和"move vendor"放在同一个字面量里会打架。
        let name = name_from_pci(root, &vendor, &pci_id, || match &vendor {
            Some(vendor) => format!("{vendor} NPU ({node})"),
            None => format!("NPU ({node})"),
        });
        let driver_version = driver_version_of(root, driver.as_deref());

        let mut accel = Accelerator {
            kind: AcceleratorKind::Npu,
            name,
            device_path: Some(node_path),
            driver_version,
            driver,
            vendor,
            pci_id,
            // Intel 的 NPU 不在设备树上
            compatible: read_compatible(&device_dir),
            // NPU 没有独立显存：权重和中间张量都在系统内存里。
            memory: AcceleratorMemory::SharedWithSystem,
            // `npu_max_frequency_mhz` 是 Legacy alias（驱动文档原话），先读 freq/hw_max_freq
            max_freq_mhz: read_u64(&device_dir.join("freq/hw_max_freq"))
                .or_else(|| read_u64(&device_dir.join("npu_max_frequency_mhz"))),
            runtime,
            notes: Vec::new(),
        };
        if !sys.exists() {
            accel
                .notes
                .push("sysfs 中没有对应条目，可能未加载加速器驱动".into());
        }
        found.push(accel);
    }
    if found.is_empty() && dir.exists() {
        warnings.push("存在 /dev/accel 但其中没有 accelN 设备节点".into());
    }
    found
}

/// GPU：以 `/sys/class/drm` 的 `cardN` 为存在性判据。
pub(crate) fn probe_gpus(
    root: &Path,
    libraries: &LibraryIndex,
    warnings: &mut Vec<String>,
) -> Vec<Accelerator> {
    let drm = root.join("sys/class/drm");
    let mut found: Vec<Accelerator> = Vec::new();

    // 只要 cardN，不要 cardN-DP-1 这类连接器；顺序必须由名字决定，不能由目录项决定
    for (node, card_path) in sorted_entries(&drm, |name| {
        name.starts_with("card") && !name.contains('-')
    }) {
        let device_dir = card_path.join("device");
        let vendor = read_trimmed(&device_dir.join("vendor")).map(|v| vendor_name(&v));
        let driver = driver_of(&device_dir);
        let pci_id = read_pci_id(&device_dir);
        let compatible = read_compatible(&device_dir);
        let raw_id = read_trimmed(&device_dir.join("device")).unwrap_or_else(|| "未知".into());

        // 一个 DRM card 未必是 GPU：见 classify_drm_device
        let kind = classify_drm_device(driver.as_deref(), compatible.as_deref());
        let runtime = runtime::probe(kind, vendor.as_deref(), libraries);
        let name = name_from_pci(root, &vendor, &pci_id, || match (&compatible, &vendor) {
            // 非 PCI 平台：设备树 compatible 是唯一有信息量的标识
            (Some(compatible), _) => format!("{compatible} ({node})"),
            (None, Some(vendor)) => format!(
                "{vendor} {} ({node}, id {raw_id})",
                kind_label_for_name(kind)
            ),
            (None, None) => format!("{} ({node})", kind_label_for_name(kind)),
        });

        let nodes = render_nodes_of(&device_dir);
        let device_path = nodes.first().map(|name| root.join("dev/dri").join(name));
        // 字面量里字段是按书写顺序求值的：先把借 vendor 的算完再 move 它
        let memory = gpu_memory(&device_dir, vendor.as_deref());
        let max_freq_mhz = gpu_max_freq_mhz(&device_dir);

        let mut notes = Vec::new();
        if driver.is_none() {
            notes.push("未绑定内核驱动，硬件加速不可用".into());
        }
        if nodes.is_empty() {
            // DRM 的 render 节点就是给渲染/计算用的；没有它基本只做显示输出
            notes.push("没有 render 节点，可能只做显示输出，不能用于计算".into());
        } else if let Some(path) = &device_path {
            if !path.exists() {
                notes.push(format!("{} 不存在", path.display()));
            }
        }

        found.push(Accelerator {
            kind,
            name,
            device_path,
            driver_version: driver_version_of(root, driver.as_deref()),
            driver,
            vendor,
            pci_id,
            compatible,
            memory,
            max_freq_mhz,
            runtime,
            notes,
        });
    }

    // 有 render 节点没被任何 card 认领：说明发现了不认识的东西，值得出声
    let claimed: BTreeSet<String> = found
        .iter()
        .filter_map(|accel| Some(accel.device_path.as_ref()?.file_name()?.to_str()?.to_string()))
        .collect();
    let unclaimed: Vec<String> = sorted_entries(&root.join("dev/dri"), |name| {
        name.starts_with("renderD")
    })
    .into_iter()
    .map(|(name, _)| name)
    .filter(|name| !claimed.contains(name))
    .collect();
    if !unclaimed.is_empty() {
        warnings.push(format!(
            "{} 没有被任何 DRM 设备认领",
            unclaimed.join("、")
        ));
    }

    if found.is_empty() && drm.exists() {
        warnings.push("存在 /sys/class/drm 但没有可用 GPU 条目".into());
    }
    found
}

/// 给名字用的类别标签（NPU / GPU）。
fn kind_label_for_name(kind: AcceleratorKind) -> &'static str {
    match kind {
        AcceleratorKind::Npu => "NPU",
        _ => "GPU",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("deviceinfo-accel-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    /// 一份最小的 pci.ids，让名字查询有东西可查。
    fn write_pci_ids(root: &Path) {
        let dir = root.join("usr/share/hwdata");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("pci.ids"),
            "8086  Intel Corporation\n\t64a0  Arc Graphics 130V/140V GPU\n\t643e  Core Ultra NPU\n",
        )
        .unwrap();
    }

    fn empty_libraries(root: &Path) -> LibraryIndex {
        LibraryIndex::build(root)
    }

    #[test]
    fn finds_npu_and_gpu_and_resolves_their_names() {
        let root = fake_root("both");
        fs::create_dir_all(root.join("proc")).unwrap();
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();
        fs::write(root.join("proc/cpuinfo"), "processor\t: 0\nmodel name\t: x\n").unwrap();
        fs::create_dir_all(root.join("sys/devices/system/cpu/smt")).unwrap();
        fs::write(root.join("sys/devices/system/cpu/smt/active"), "0\n").unwrap();
        let cpu0 = root.join("sys/devices/system/cpu/cpu0/topology");
        fs::create_dir_all(&cpu0).unwrap();
        fs::write(cpu0.join("core_cpus_list"), "0\n").unwrap();
        write_pci_ids(&root);
        // 齐全的 Intel NPU 运行时 + Level Zero GPU 驱动
        let lib = root.join("usr/lib");
        fs::create_dir_all(&lib).unwrap();
        for name in [
            "libze_loader.so",
            "libze_intel_npu.so",
            "libopenvino_intel_npu_compiler_loader.so",
            "libze_intel_gpu.so",
        ] {
            fs::write(lib.join(name), b"").unwrap();
        }

        // NPU：vendor/device/npu_max_frequency_mhz + 驱动软链
        let npu_device = root.join("sys/class/accel/accel0/device");
        fs::create_dir_all(&npu_device).unwrap();
        fs::write(npu_device.join("vendor"), "0x8086\n").unwrap();
        fs::write(npu_device.join("device"), "0x643e\n").unwrap();
        fs::write(npu_device.join("npu_max_frequency_mhz"), "1900\n").unwrap();
        fs::create_dir_all(root.join("dev/accel")).unwrap();
        fs::write(root.join("dev/accel/accel0"), "").unwrap();

        // GPU：xe 的 tile0/gt0/freq0/max_freq，没有 vram 节点
        let gpu_device = root.join("sys/class/drm/card0/device");
        fs::create_dir_all(gpu_device.join("tile0/gt0/freq0")).unwrap();
        fs::write(gpu_device.join("vendor"), "0x8086\n").unwrap();
        fs::write(gpu_device.join("device"), "0x64a0\n").unwrap();
        fs::write(gpu_device.join("tile0/gt0/freq0/max_freq"), "1950\n").unwrap();
        fs::create_dir_all(root.join("dev/dri")).unwrap();
        fs::write(root.join("dev/dri/renderD128"), "").unwrap();
        // render 节点的**归属**由 `<device>/drm/` 给出，不靠下标猜。
        // 夹具里少了这一项就会得到"renderD128 没有被任何 DRM 设备认领"——
        // 这正是那个警告存在的意义。
        fs::create_dir_all(gpu_device.join("drm/renderD128")).unwrap();

        let report = crate::probe_with(&root, "x86_64");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        let npu = report
            .accelerators
            .iter()
            .find(|a| a.kind == AcceleratorKind::Npu)
            .expect("应识别出 NPU");
        // A4：设备在 + 运行时齐 → 可用
        assert!(npu.runtime.is_ready(), "{:#?}", npu.runtime);
        assert!(report.has_npu() && report.has_usable_npu());
        // B1：id 翻成了人名，id 本身也没丢
        assert_eq!(npu.name, "Intel Core Ultra NPU");
        assert_eq!(npu.pci_id.as_ref().unwrap().compact(), "8086:643e");
        // B2：只收"上限"这类事实
        assert_eq!(npu.max_freq_mhz, Some(1900));
        // A5：NPU 共享系统内存，不是"未知"
        assert_eq!(npu.memory, AcceleratorMemory::SharedWithSystem);

        let gpu = report
            .accelerators
            .iter()
            .find(|a| a.kind == AcceleratorKind::Gpu)
            .expect("应识别出 GPU");
        assert!(gpu.runtime.is_ready(), "{:#?}", gpu.runtime);
        assert_eq!(gpu.name, "Intel Arc Graphics 130V/140V GPU");
        assert_eq!(gpu.max_freq_mhz, Some(1950));
        assert_eq!(gpu.memory, AcceleratorMemory::SharedWithSystem);
        assert_eq!(
            gpu.device_path.as_deref(),
            Some(root.join("dev/dri/renderD128").as_path())
        );

        fs::remove_dir_all(&root).ok();
    }

    /// A4 的核心场景：设备在、驱动绑了，但用户态栈不全。
    #[test]
    fn device_present_but_runtime_incomplete_is_reported_as_such() {
        let root = fake_root("npu-nostack");
        // 只有驱动，没有 level-zero 和编译器
        let npu_device = root.join("sys/class/accel/accel0/device");
        fs::create_dir_all(&npu_device).unwrap();
        fs::write(npu_device.join("vendor"), "0x8086\n").unwrap();
        fs::write(npu_device.join("device"), "0x643e\n").unwrap();
        fs::create_dir_all(root.join("dev/accel")).unwrap();
        fs::write(root.join("dev/accel/accel0"), "").unwrap();

        let mut warnings = Vec::new();
        let npus = probe_npus(&root, &empty_libraries(&root), &mut warnings);
        assert_eq!(npus.len(), 1);
        // 设备确实在……
        assert_eq!(npus[0].vendor.as_deref(), Some("Intel"));
        // ……但"能用"是另一回事
        match &npus[0].runtime {
            crate::RuntimeStatus::Incomplete { missing, .. } => assert_eq!(missing.len(), 3, "{missing:?}"),
            other => panic!("应当是 Incomplete: {other:#?}"),
        }
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    /// NVIDIA 的显存读不到时必须说"未知"，不能编一个数，也不能说"共享"。
    #[test]
    fn nvidia_memory_is_unknown_not_zero_and_not_shared() {
        let root = fake_root("nvidia");
        let device = root.join("sys/class/drm/card0/device");
        fs::create_dir_all(&device).unwrap();
        fs::write(device.join("vendor"), "0x10de\n").unwrap();
        fs::write(device.join("device"), "0x2204\n").unwrap();

        let mut warnings = Vec::new();
        let gpus = probe_gpus(&root, &empty_libraries(&root), &mut warnings);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].vendor.as_deref(), Some("NVIDIA"));
        match &gpus[0].memory {
            AcceleratorMemory::Unknown { reason } => assert!(reason.contains("NVIDIA"), "{reason}"),
            other => panic!("应当是 Unknown: {other:#?}"),
        }
        assert_eq!(gpus[0].memory.dedicated_bytes(), None);
        // NVIDIA 认不出栈 → 不猜就绪
        assert!(!gpus[0].runtime.is_ready());

        fs::remove_dir_all(&root).ok();
    }

    /// 驱动暴露了显存总量 → 独立显存。
    #[test]
    fn exposed_vram_total_means_dedicated_memory() {
        let root = fake_root("vram");
        let device = root.join("sys/class/drm/card0/device");
        fs::create_dir_all(&device).unwrap();
        fs::write(device.join("vendor"), "0x1002\n").unwrap();
        fs::write(device.join("device"), "0x744c\n").unwrap();
        fs::write(device.join("mem_info_vram_total"), "17163091968\n").unwrap();

        let mut warnings = Vec::new();
        let gpus = probe_gpus(&root, &empty_libraries(&root), &mut warnings);
        assert_eq!(
            gpus[0].memory,
            AcceleratorMemory::Dedicated {
                bytes: 17_163_091_968
            }
        );
        assert_eq!(gpus[0].memory.dedicated_bytes(), Some(17_163_091_968));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn gpu_without_a_bound_driver_is_noted() {
        let root = fake_root("nodriver");
        fs::create_dir_all(root.join("sys/class/drm/card0/device")).unwrap();
        fs::write(root.join("sys/class/drm/card0/device/vendor"), "0x1002\n").unwrap();

        let mut warnings = Vec::new();
        let gpus = probe_gpus(&root, &empty_libraries(&root), &mut warnings);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].vendor.as_deref(), Some("AMD"));
        assert!(
            gpus[0].notes.iter().any(|n| n.contains("未绑定内核驱动")),
            "{:#?}",
            gpus[0]
        );

        fs::remove_dir_all(&root).ok();
    }

    /// 驱动版本读的是 `<root>/sys/module/<驱动>/version`——本机 `intel_vpu` 有，
    /// `xe` 没有。两种情形都要能在报告里区分出来。
    #[test]
    fn driver_version_is_read_through_the_injected_root() {
        let root = fake_root("drvver");
        fs::create_dir_all(root.join("sys/module/intel_vpu")).unwrap();
        fs::write(
            root.join("sys/module/intel_vpu/version"),
            "1.0.0 7.2.6-arch2-1\n",
        )
        .unwrap();
        assert_eq!(
            driver_version_of(&root, Some("intel_vpu")).as_deref(),
            Some("1.0.0 7.2.6-arch2-1")
        );
        // 驱动没暴露 version 文件 → None，而不是空字符串
        fs::create_dir_all(root.join("sys/module/xe")).unwrap();
        assert_eq!(driver_version_of(&root, Some("xe")), None);
        // 连驱动名都没有
        assert_eq!(driver_version_of(&root, None), None);
        fs::remove_dir_all(&root).ok();
    }

    /// Rockchip 的 RKNPU 走 **DRM** 而不是 `/dev/accel` 暴露。
    /// 一台**确实有 NPU** 的机器上 `has_npu()` 不能返回 false——
    /// 那会让上层再也不会考虑 NPU 卸载，而且没有任何报错。
    #[test]
    fn a_drm_card_whose_identity_says_npu_is_an_npu_not_a_gpu() {
        let root = fake_root("rknpu");
        let device = root.join("sys/class/drm/card0/device");
        fs::create_dir_all(device.join("of_node")).unwrap();
        // 设备树属性是 NUL 结尾的
        fs::write(device.join("of_node/compatible"), b"rockchip,rk3588-rknpu\0").unwrap();
        fs::create_dir_all(device.join("drm/renderD128")).unwrap();
        fs::create_dir_all(root.join("dev/dri")).unwrap();
        fs::write(root.join("dev/dri/renderD128"), "").unwrap();

        let mut warnings = Vec::new();
        let found = probe_gpus(&root, &empty_libraries(&root), &mut warnings);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].kind,
            AcceleratorKind::Npu,
            "驱动/compatible 说是 NPU 就不能当 GPU"
        );
        assert_eq!(found[0].compatible.as_deref(), Some("rockchip,rk3588-rknpu"));
        assert_eq!(found[0].name, "rockchip,rk3588-rknpu (card0)");
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    /// render 节点的归属由 `<device>/drm/` 给出，不按顺序猜。
    /// 实测那台 ARM 机器上 `renderD128` 属于 card0，而旧的"按下标配对"给了 card1。
    #[test]
    fn render_nodes_belong_to_the_device_that_lists_them() {
        let root = fake_root("render-owner");
        // card0 有自己的 render 节点，card1 只有 control 节点
        fs::create_dir_all(root.join("sys/class/drm/card0/device/drm/renderD128")).unwrap();
        fs::create_dir_all(root.join("sys/class/drm/card1/device/drm/controlD65")).unwrap();
        fs::create_dir_all(root.join("dev/dri")).unwrap();
        fs::write(root.join("dev/dri/renderD128"), "").unwrap();

        let mut warnings = Vec::new();
        let found = probe_gpus(&root, &empty_libraries(&root), &mut warnings);
        let card0 = found.iter().find(|a| a.name == "GPU (card0)").unwrap();
        let card1 = found.iter().find(|a| a.name == "GPU (card1)").unwrap();
        assert_eq!(
            card0.device_path.as_deref(),
            Some(root.join("dev/dri/renderD128").as_path())
        );
        assert!(card1.device_path.is_none(), "card1 没有 render 节点");
        assert!(
            card1.notes.iter().any(|note| note.contains("render 节点")),
            "没有 render 节点这件事得说出来: {:#?}",
            card1.notes
        );
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    /// 有 render 节点没被任何 card 认领——说明遇到了不认识的东西，不能默默丢掉。
    #[test]
    fn an_unclaimed_render_node_is_a_warning() {
        let root = fake_root("orphan-render");
        fs::create_dir_all(root.join("dev/dri")).unwrap();
        fs::write(root.join("dev/dri/renderD128"), "").unwrap();

        let mut warnings = Vec::new();
        let found = probe_gpus(&root, &empty_libraries(&root), &mut warnings);
        assert!(found.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("renderD128"), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn connector_entries_are_not_mistaken_for_gpus() {
        let root = fake_root("connector");
        // card0-DP-1 是显示连接器，不是 GPU
        for name in ["card0-DP-1", "card0-HDMI-A-1"] {
            fs::create_dir_all(root.join("sys/class/drm").join(name)).unwrap();
        }
        let mut warnings = Vec::new();
        assert!(probe_gpus(&root, &empty_libraries(&root), &mut warnings).is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn max_freq_prefers_the_xe_layout_then_falls_back_to_i915() {
        let root = fake_root("freq");
        let device = root.join("sys/class/drm/card0/device");
        // i915 的老布局：gt_max_freq_mhz
        fs::create_dir_all(&device).unwrap();
        fs::write(device.join("gt_max_freq_mhz"), "1200\n").unwrap();
        assert_eq!(gpu_max_freq_mhz(&device), Some(1200));
        // 有 xe 布局时以它为准
        fs::create_dir_all(device.join("tile0/gt1/freq0")).unwrap();
        fs::write(device.join("tile0/gt1/freq0/max_freq"), "1950\n").unwrap();
        assert_eq!(gpu_max_freq_mhz(&device), Some(1950));
        fs::remove_dir_all(&root).ok();
    }
}
