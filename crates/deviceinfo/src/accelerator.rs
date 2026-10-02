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

/// GPU 可用的内存语义，以及一条"这是推断"的说明（如果有）。
///
/// 只有一件事是**证据**：驱动暴露了 `mem_info_vram_total` → 独立显存。
/// 另一件也算证据：**没有 PCI 标识**的图形单元是 SoC 上集成的，必然共享系统内存。
///
/// 剩下那种情况（PCI 设备、驱动又没暴露显存总量）**从 sysfs 判不出来**：
/// 集显和独显在这里长得一样。实测本机 iGPU 是 `0000:00:02.0`、只有一个 256 MB 的
/// BAR，而**非 ReBAR 的独显 BAR 也是 256 MB**，所以连 BAR 大小都不能用。
/// 以前这里按"Intel/AMD 在 Linux 上绝大多数是集显"直接判成共享——那不是证据，
/// 只是概率，而 ARM 服务器插一张 AMD 卡就会判错。现在照旧给共享，
/// 但**必须附一条说明**，让上层知道这是推断。
fn gpu_memory(
    device_dir: &Path,
    vendor: Option<&str>,
    pci_id: Option<&PciId>,
) -> (AcceleratorMemory, Option<String>) {
    if let Some(bytes) = read_u64(&device_dir.join("mem_info_vram_total")) {
        if bytes > 0 {
            return (AcceleratorMemory::Dedicated { bytes }, None);
        }
    }
    // 没有 PCI 标识 → 是 SoC 上集成的单元，共享系统内存。这是证据，不是推断。
    if pci_id.is_none() {
        return (AcceleratorMemory::SharedWithSystem, None);
    }
    match vendor {
        Some(name @ ("Intel" | "AMD")) => (
            AcceleratorMemory::SharedWithSystem,
            Some(format!(
                "驱动未暴露显存上限；按 {name} 在 Linux 上的常见形态推断为共享系统内存——\
                 这是推断，不是本机证据（独显此处会判错）"
            )),
        ),
        // NVIDIA 专有驱动不通过 sysfs 暴露显存（要 NVML）。这里不猜数值：
        // 编一个数会让人装上装不下的模型。
        Some(other) => (
            AcceleratorMemory::Unknown {
                reason: format!("{other} 驱动未通过 sysfs 暴露显存上限"),
            },
            None,
        ),
        None => (
            AcceleratorMemory::Unknown {
                reason: "PCI 设备且厂商未识别，无法判断显存是独立还是共享".into(),
            },
            None,
        ),
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

/// 设备树 `compatible`：**没有 PCI 标识的设备**（SoC 上集成的单元）的权威标识。
///
/// 注意判据是"**这个设备**有没有 PCI 标识"，不是"这台机器是什么架构"：
/// ARM 服务器一样有 PCIe，一样能插独显 / 加速卡（本机那颗 Intel NPU 就是
/// PCI 设备，class `0x120000`）。身份来源是按设备选的。
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

/// 判断一个 DRM 设备到底是 NPU / GPU / 只会显示。
///
/// 两步，都不靠猜：
///
/// 1. **身份里有 `npu` → NPU。** Rockchip 的 RKNPU 走 DRM 暴露，于是一台**确实有
///    NPU** 的机器上 `has_npu()` 会返回 `false`——上层再也不会考虑 NPU 卸载，
///    而且没有任何报错。实测那台机器上它出现在 `/sys/class/drm/card0`，
///    驱动 `RKNPU`，compatible `rockchip,rk3588-rknpu`。
/// 2. **有没有 render 节点 → GPU / 只会显示。** DRM 的 render 节点就是给渲染/计算
///    用的（`DRIVER_RENDER`），纯显示控制器拿不到。这样不需要任何驱动名白名单：
///    实测的 `linlondp`（3 个）和 `rockchip-drm` 都因此被正确归为显示设备。
///
/// `render_nodes` 是三态，因为"没看到 render 节点"有两种原因，不能混：
/// `Some(0)` 是**确实看到了那个目录且里面没有**（→ 显示设备），
/// `None` 是我们**没能看到目录**（老内核？）——那就什么都不改，继续当 GPU 并注明。
/// 把后者也当成显示设备，会在老内核上把真 GPU 静默降级。
///
/// `pub(crate)`：状态采样必须用**同一套**判据，否则两份报告里第 N 个设备不是同一个
/// （这个分歧真的发生过，被 `state_and_hardware_agree_on_which_devices_exist` 抓到）。
pub(crate) fn classify_drm_device(
    driver: Option<&str>,
    compatible: Option<&str>,
    render_nodes: Option<usize>,
) -> AcceleratorKind {
    let identity = format!(
        "{} {}",
        driver.unwrap_or_default(),
        compatible.unwrap_or_default()
    )
    .to_ascii_lowercase();
    if identity.contains("npu") || DRM_NPU_DRIVERS.iter().any(|name| identity.contains(name)) {
        return AcceleratorKind::Npu;
    }
    match render_nodes {
        Some(0) => AcceleratorKind::Display,
        _ => AcceleratorKind::Gpu,
    }
}

/// PCI 槽位名（`0000:00:0b.0`）。
///
/// 用来回答"这个 PCI 设备是不是**已经**被当作加速器报过了"。从 `uevent` 里读
/// `PCI_SLOT_NAME`，而不是解 `device` 软链：软链在夹具里存不下来（capture 把 `device`
/// 建成真目录），而 `uevent` 是个普通文本文件，两条路都读得到。
fn pci_slot_name(device_dir: &Path) -> Option<String> {
    let text = read_trimmed(&device_dir.join("uevent"))?;
    text.lines()
        .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
        .map(str::trim)
        .filter(|slot| !slot.is_empty())
        .map(str::to_string)
}

/// PCI `class` 里属于"加速器"的两类。
///
/// `class` 文件形如 `0x120000`（class / subclass / prog-if 各一字节）：
///
/// - `0x12....` = **Processing accelerators**（处理加速器，本机的 Intel NPU 就是它）
/// - `0x0b40..` = **Co-processor**（协处理器）
///
/// 关键：**这是内核自己的分类**，不是白名单——内核已经把"这块卡是干什么的"写在
/// sysfs 里了，照读就行。
fn is_accelerator_class(class: &str) -> bool {
    // 先统一小写再剥 `0x`：内核写的是 `0x120000`，但没必要依赖这个大小写
    let normalized = class.trim().to_ascii_lowercase();
    let normalized = normalized.strip_prefix("0x").unwrap_or(&normalized);
    normalized.starts_with("12") || normalized.starts_with("0b40")
}

/// 扫 PCI 总线，把**本模块不建模**的加速器点名出来。
///
/// 为什么需要：内核给出的通用加速器入口只有 `sys/class/accel`
/// （→ `/dev/accel/accelN`）和 DRM，而有些卡两个都不用——Hailo-8 是 `/dev/hailo0`、
/// Coral 是 `/dev/apex_0`、FPGA 卡是 `/dev/xdma*`。那些卡会**静默消失**，
/// 而"我没看见"和"没有"是两件事，前者必须说出来。
///
/// 只发警告、**不进 `accelerators`**：我们只知道它是加速器，不知道它属于哪一类、
/// 能不能拿来跑模型。宁可说"我认不出它"，也不要给它编一个类别。
pub(crate) fn warn_unmodelled_pci_accelerators(
    root: &Path,
    known_slots: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) {
    let Ok(entries) = fs::read_dir(root.join("sys/bus/pci/devices")) else {
        return;
    };
    let mut unmodelled: Vec<(String, String, String)> = Vec::new();
    for entry in entries.flatten() {
        let slot = entry.file_name().to_string_lossy().into_owned();
        // 已经被当作加速器报过的（本机的 Intel NPU 就是）不再重复点名
        if known_slots.contains(&slot) {
            continue;
        }
        let Some(class) = read_trimmed(&entry.path().join("class")) else {
            continue;
        };
        if !is_accelerator_class(&class) {
            continue;
        }
        let driver = driver_of(&entry.path()).unwrap_or_else(|| "无驱动".into());
        unmodelled.push((slot, class, driver));
    }
    unmodelled.sort();
    for (slot, class, driver) in unmodelled {
        warnings.push(format!(
            "PCI {slot} 是加速器（class {class}，驱动 {driver}），但本模块不认识这类设备，\
             它不在 accelerators 里——要用它得先给这类设备加上探测"
        ));
    }
}

/// 一台 DRM 设备**自己**的 render 节点名（`renderD128` 这类）。
///
/// `<device_dir>/drm/` 列的就是该设备自己的 `cardN` / `renderD*` / `controlD*` 条目，
/// 这是 sysfs 里唯一**直接**给出归属关系的地方。旧代码按"第 N 个 render 节点配
/// 第 N 个 card"配对，实测那台 ARM 机器上 renderD128 属于 card0，
/// 而配对给了 card1——就算排序之后碰巧对了，规则本身也不成立。
///
/// 也刻意不去解 `sys/class/drm/renderD*` 的软链：软链在夹具里存不下来。
pub(crate) fn render_nodes_of(device_dir: &Path) -> Vec<String> {
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
    known_pci_slots: &mut BTreeSet<String>,
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
        // 记下 PCI 槽位：扫 PCI 总线时要用它去重（Intel 的 NPU 就是 PCI 设备）
        if let Some(slot) = pci_slot_name(&device_dir) {
            known_pci_slots.insert(slot);
        }
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
    known_pci_slots: &mut BTreeSet<String>,
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
        if let Some(slot) = pci_slot_name(&device_dir) {
            known_pci_slots.insert(slot);
        }

        // render 节点的**归属**由 `<device>/drm/` 的目录项给出。
        // 三态：看到了(Some) / 看不到那个目录(None)——理由见 classify_drm_device。
        let drm_dir = device_dir.join("drm");
        let nodes = render_nodes_of(&device_dir);
        let render_nodes = drm_dir.is_dir().then_some(nodes.len());

        // 一个 DRM card 未必是 GPU：见 classify_drm_device
        let kind = classify_drm_device(driver.as_deref(), compatible.as_deref(), render_nodes);
        let runtime = runtime::probe(kind, vendor.as_deref(), libraries);
        let name = name_from_pci(root, &vendor, &pci_id, || match (&compatible, &vendor) {
            // 没有 PCI 标识的设备：设备树 compatible 是唯一有信息量的标识
            (Some(compatible), _) => format!("{compatible} ({node})"),
            (None, Some(vendor)) => format!(
                "{vendor} {} ({node}, id {raw_id})",
                kind_label_for_name(kind)
            ),
            (None, None) => format!("{} ({node})", kind_label_for_name(kind)),
        });

        let device_path = nodes.first().map(|name| root.join("dev/dri").join(name));
        // 字面量里字段是按书写顺序求值的：先把借 vendor 的算完再 move 它
        let (memory, memory_note) = match kind {
            // 显示控制器拿系统内存做 framebuffer，没有"独立显存"这回事
            AcceleratorKind::Display => (AcceleratorMemory::SharedWithSystem, None),
            _ => gpu_memory(&device_dir, vendor.as_deref(), pci_id.as_ref()),
        };
        let max_freq_mhz = gpu_max_freq_mhz(&device_dir);

        let mut notes = Vec::new();
        if let Some(note) = memory_note {
            notes.push(note);
        }
        if driver.is_none() {
            notes.push("未绑定内核驱动，硬件加速不可用".into());
        }
        if render_nodes.is_none() {
            // 没有这个目录，就判不出是不是只做显示输出。宁可继续当 GPU 并注明，
            // 也不要在老内核上把真 GPU 静默降级。
            notes.push("看不到 <device>/drm/，无法判断是否只做显示输出".into());
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
        AcceleratorKind::Display => "display",
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
            "8086  Intel Corporation\n\t64a0  Arc Graphics 130V/140V GPU\n\t643e  Core Ultra NPU\n\
             1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t744c  Navi 31 [Radeon RX 7900 XT/XTX]\n",
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
        let npus = probe_npus(&root, &empty_libraries(&root), &mut std::collections::BTreeSet::new(), &mut warnings);
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
        let gpus = probe_gpus(&root, &empty_libraries(&root), &mut std::collections::BTreeSet::new(), &mut warnings);
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
        let gpus = probe_gpus(&root, &empty_libraries(&root), &mut std::collections::BTreeSet::new(), &mut warnings);
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
        let gpus = probe_gpus(&root, &empty_libraries(&root), &mut std::collections::BTreeSet::new(), &mut warnings);
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

    /// **ARM 服务器一样有 PCIe，一样能插独显 / 加速卡。**
    ///
    /// 性能相关的身份来源是**按设备**选的，不是按架构选的：这个设备有 PCI 标识就用
    /// PCI 标识（`pci.ids` 给人名 + 独立显存），没有才退到设备树 `compatible`。
    /// 这条测的就是那个组合：aarch64 + PCI 独显 + 独立显存 + 没有 `of_node`。
    /// （本机那颗 Intel NPU 也是 PCI 设备，class `0x120000`。）
    #[test]
    fn a_discrete_gpu_on_arm_uses_its_pci_identity() {
        let root = fake_root("arm-pcie");
        write_pci_ids(&root);
        let device = root.join("sys/class/drm/card0/device");
        fs::create_dir_all(device.join("drm/renderD128")).unwrap();
        fs::write(device.join("vendor"), "0x1002\n").unwrap();
        fs::write(device.join("device"), "0x744c\n").unwrap();
        fs::write(device.join("mem_info_vram_total"), "17163091968\n").unwrap();
        fs::create_dir_all(root.join("dev/dri")).unwrap();
        fs::write(root.join("dev/dri/renderD128"), "").unwrap();

        let report = crate::probe_with(&root, "aarch64");
        assert_eq!(report.accelerators.len(), 1);
        let gpu = &report.accelerators[0];
        assert_eq!(gpu.kind, AcceleratorKind::Gpu);
        assert_eq!(gpu.pci_id.as_ref().unwrap().compact(), "1002:744c");
        assert_eq!(gpu.vendor.as_deref(), Some("AMD"));
        assert!(
            gpu.compatible.is_none(),
            "PCI 卡一般没有 of_node，不该凭空编一个"
        );
        assert_eq!(gpu.name, "AMD Navi 31 [Radeon RX 7900 XT/XTX]");
        // 驱动暴露了显存上限 → 独立显存，而且不该附带"这是推断"的说明
        assert_eq!(
            gpu.memory,
            AcceleratorMemory::Dedicated {
                bytes: 17_163_091_968
            }
        );
        // 有证据的语义不需要附"这是推断"
        assert!(
            !gpu.notes.iter().any(|note| note.contains("推断")),
            "{:#?}",
            gpu.notes
        );
        // 架构是 aarch64 也照样算有 GPU
        assert!(report.has_gpu());

        fs::remove_dir_all(&root).ok();
    }

    /// PCI 设备、驱动又没暴露显存 → sysfs 判不出来。这时给共享只能算推断，
    /// **必须附说明**：ARM 服务器插一张 AMD 卡就会判错。
    #[test]
    fn an_unexposed_vram_limit_is_labelled_as_an_inference() {
        let root = fake_root("inferred-memory");
        let device = root.join("sys/class/drm/card0/device");
        fs::create_dir_all(device.join("drm/renderD128")).unwrap();
        fs::write(device.join("vendor"), "0x1002\n").unwrap();
        fs::write(device.join("device"), "0x744c\n").unwrap();
        fs::create_dir_all(root.join("dev/dri")).unwrap();
        fs::write(root.join("dev/dri/renderD128"), "").unwrap();

        let mut warnings = Vec::new();
        let found = probe_gpus(&root, &empty_libraries(&root), &mut std::collections::BTreeSet::new(), &mut warnings);
        assert_eq!(found[0].memory, AcceleratorMemory::SharedWithSystem);
        assert!(
            found[0].notes.iter().any(|note| note.contains("推断")),
            "给推断就必须说出来: {:#?}",
            found[0].notes
        );

        // 没有 PCI 标识（SoC 上集成的单元）→ 共享是**证据**，不需要说明
        let soc = fake_root("soc-gpu");
        fs::create_dir_all(soc.join("sys/class/drm/card0/device/drm/renderD128")).unwrap();
        assert_eq!(
            probe_gpus(&soc, &empty_libraries(&soc), &mut std::collections::BTreeSet::new(), &mut Vec::new())[0].memory,
            AcceleratorMemory::SharedWithSystem
        );
        assert!(
            !probe_gpus(&soc, &empty_libraries(&soc), &mut std::collections::BTreeSet::new(), &mut Vec::new())[0]
                .notes
                .iter()
                .any(|note| note.contains("推断")),
            "SoC 上集成的单元共享系统内存是证据，不该标成推断"
        );

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&soc).ok();
    }

    /// 内核自己的 PCI `class` 就是"这块卡是干什么的"。不在 `/dev/accel`、也不出 DRM
    /// 的加速器（Hailo-8、Coral、FPGA 卡）**必须点名警告**，不能静默消失。
    #[test]
    fn unmodelled_pci_accelerators_are_named_not_silently_dropped() {
        let root = fake_root("pci-accel");
        let pci = root.join("sys/bus/pci/devices");
        // 处理加速器（本机 Intel NPU 就是这类）
        fs::create_dir_all(pci.join("0000:01:00.0")).unwrap();
        fs::write(pci.join("0000:01:00.0/class"), "0x120000\n").unwrap();
        fs::write(
            pci.join("0000:01:00.0/uevent"),
            "DRIVER=hailo_pci\nPCI_SLOT_NAME=0000:01:00.0\n",
        )
        .unwrap();
        fs::create_dir_all(pci.join("0000:01:00.0/driver")).unwrap();
        // 协处理器
        fs::create_dir_all(pci.join("0000:03:00.0")).unwrap();
        fs::write(pci.join("0000:03:00.0/class"), "0x0b4000\n").unwrap();
        // 显卡 / 网卡：不是"未建模的加速器"，不该被点名
        fs::create_dir_all(pci.join("0000:02:00.0")).unwrap();
        fs::write(pci.join("0000:02:00.0/class"), "0x030000\n").unwrap();
        fs::create_dir_all(pci.join("0000:04:00.0")).unwrap();
        fs::write(pci.join("0000:04:00.0/class"), "0x020000\n").unwrap();

        let mut warnings = Vec::new();
        warn_unmodelled_pci_accelerators(&root, &BTreeSet::new(), &mut warnings);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("0000:01:00.0"), "{warnings:?}");
        assert!(warnings[1].contains("0000:03:00.0"), "{warnings:?}");
        // 要说清后果，而不只是"发现一个设备"
        assert!(warnings[0].contains("不在 accelerators 里"), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    /// 已经被当作加速器报过的 PCI 设备**不能重复点名**——Intel 的 NPU 正是
    /// class `0x1200` 的 PCI 设备，而且已经通过 `/dev/accel` 报过了。
    #[test]
    fn an_already_reported_pci_accelerator_is_not_named_twice() {
        let root = fake_root("pci-dedupe");
        // 已发现的 NPU
        let npu = root.join("sys/class/accel/accel0/device");
        fs::create_dir_all(&npu).unwrap();
        fs::write(npu.join("vendor"), "0x8086\n").unwrap();
        fs::write(npu.join("device"), "0x643e\n").unwrap();
        fs::write(
            npu.join("uevent"),
            "DRIVER=intel_vpu\nPCI_SLOT_NAME=0000:00:0b.0\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("dev/accel")).unwrap();
        fs::write(root.join("dev/accel/accel0"), "").unwrap();
        // 同一个设备的 PCI 视图
        let pci = root.join("sys/bus/pci/devices/0000:00:0b.0");
        fs::create_dir_all(&pci).unwrap();
        fs::write(pci.join("class"), "0x120000\n").unwrap();
        fs::write(
            pci.join("uevent"),
            "DRIVER=intel_vpu\nPCI_SLOT_NAME=0000:00:0b.0\n",
        )
        .unwrap();

        let mut known = BTreeSet::new();
        let mut warnings = Vec::new();
        let npus = probe_npus(
            &root,
            &empty_libraries(&root),
            &mut known,
            &mut warnings,
        );
        assert_eq!(npus.len(), 1);
        warn_unmodelled_pci_accelerators(&root, &known, &mut warnings);
        assert!(
            warnings.is_empty(),
            "同一个设备不该被点名两次: {warnings:?}"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pci_class_matching_is_by_class_and_subclass() {
        assert!(is_accelerator_class("0x120000"));
        assert!(is_accelerator_class("0x1200"));
        assert!(is_accelerator_class("0x0b4000"));
        assert!(is_accelerator_class("0X120000"));
        // 显卡、网卡、存储控制器都不是
        assert!(!is_accelerator_class("0x030000"));
        assert!(!is_accelerator_class("0x020000"));
        assert!(!is_accelerator_class("0x010802"));
        assert!(!is_accelerator_class(""));
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
        let found = probe_gpus(&root, &empty_libraries(&root), &mut std::collections::BTreeSet::new(), &mut warnings);
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
        let found = probe_gpus(&root, &empty_libraries(&root), &mut std::collections::BTreeSet::new(), &mut warnings);
        let card0 = found.iter().find(|a| a.name == "GPU (card0)").unwrap();
        let card1 = found.iter().find(|a| a.name == "display (card1)").unwrap();
        assert_eq!(card0.kind, AcceleratorKind::Gpu);
        assert_eq!(
            card0.device_path.as_deref(),
            Some(root.join("dev/dri/renderD128").as_path())
        );
        // 有 `drm/` 目录但里面只有 control 节点 → 只能显示输出，不是 GPU
        assert_eq!(card1.kind, AcceleratorKind::Display);
        assert!(card1.device_path.is_none(), "card1 没有 render 节点");
        assert!(
            matches!(card1.runtime, crate::RuntimeStatus::NotApplicable { .. }),
            "显示设备不涉及推理运行时: {:#?}",
            card1.runtime
        );
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    /// **只有显示控制器的机器，`has_gpu()` 必须是 false。** 这是加 `Display` 类别的全部理由。
    #[test]
    fn a_machine_with_only_display_controllers_has_no_gpu() {
        let root = fake_root("display-only");
        fs::create_dir_all(root.join("proc")).unwrap();
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();
        fs::write(root.join("proc/cpuinfo"), "processor\t: 0\nmodel name\t: x\n").unwrap();
        fs::create_dir_all(root.join("sys/devices/system/cpu/smt")).unwrap();
        fs::write(root.join("sys/devices/system/cpu/smt/active"), "0\n").unwrap();
        let cpu0 = root.join("sys/devices/system/cpu/cpu0/topology");
        fs::create_dir_all(&cpu0).unwrap();
        fs::write(cpu0.join("core_cpus_list"), "0\n").unwrap();
        for card in ["card0", "card1"] {
            let device = root.join(format!("sys/class/drm/{card}/device"));
            fs::create_dir_all(device.join("drm")).unwrap();
            fs::create_dir_all(device.join("of_node")).unwrap();
            fs::write(device.join("of_node/compatible"), b"rockchip,display-subsystem\0")
                .unwrap();
            fs::create_dir_all(device.join("drm/controlD65")).unwrap();
        }

        let report = crate::probe_with(&root, "aarch64");
        assert_eq!(report.accelerators.len(), 2);
        assert!(
            !report.has_gpu(),
            "两个显示控制器不该让 has_gpu() 为真: {:#?}",
            report.accelerators
        );

        fs::remove_dir_all(&root).ok();
    }

    /// 看不到 `<device>/drm/` 时**不能**下"只会显示"的结论——那会把老内核上的真 GPU
    /// 静默降级。宁可继续当 GPU 并注明。
    #[test]
    fn a_missing_drm_directory_does_not_demote_a_gpu() {
        let root = fake_root("no-drm-dir");
        fs::create_dir_all(root.join("sys/class/drm/card0/device")).unwrap();

        let mut warnings = Vec::new();
        let found = probe_gpus(&root, &empty_libraries(&root), &mut std::collections::BTreeSet::new(), &mut warnings);
        assert_eq!(found[0].kind, AcceleratorKind::Gpu);
        assert!(
            found[0].notes.iter().any(|note| note.contains("看不到")),
            "{:#?}",
            found[0].notes
        );

        fs::remove_dir_all(&root).ok();
    }

    /// 有 render 节点没被任何 card 认领——说明遇到了不认识的东西，不能默默丢掉。
    #[test]
    fn an_unclaimed_render_node_is_a_warning() {
        let root = fake_root("orphan-render");
        fs::create_dir_all(root.join("dev/dri")).unwrap();
        fs::write(root.join("dev/dri/renderD128"), "").unwrap();

        let mut warnings = Vec::new();
        let found = probe_gpus(&root, &empty_libraries(&root), &mut std::collections::BTreeSet::new(), &mut warnings);
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
        assert!(probe_gpus(&root, &empty_libraries(&root), &mut std::collections::BTreeSet::new(), &mut warnings).is_empty());
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
