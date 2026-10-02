//! 加速器探测：GPU 与 NPU。
//!
//! 这里只回答**存在性与驱动绑定**，不回答"能不能用来跑模型"。
//! 后者取决于用户态栈（OpenVINO 的 NPU 插件、CUDA/ROCm 运行时等），是能力判定的范畴，
//! 混进来会让"设备事实"和"环境状态"纠缠在一起。

use crate::report::{Accelerator, AcceleratorKind};
use crate::sysfs::read_trimmed;
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
fn driver_of(device_dir: &Path) -> Option<String> {
    let link = fs::read_link(device_dir.join("driver")).ok()?;
    link.file_name().map(|s| s.to_string_lossy().into_owned())
}

/// NPU：现代内核把 NPU 暴露在 `/dev/accel/accelN`。
pub(crate) fn probe_npus(root: &Path, warnings: &mut Vec<String>) -> Vec<Accelerator> {
    let dir = root.join("dev/accel");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("accel") {
            continue;
        }
        let device_path = entry.path();
        let sys = root.join("sys/class/accel").join(&name);
        let vendor = read_trimmed(&sys.join("device/vendor")).map(|v| vendor_name(&v));
        let driver = driver_of(&sys.join("device"));
        let mut accel = Accelerator {
            kind: AcceleratorKind::Npu,
            name: match &vendor {
                Some(vendor) => format!("{vendor} NPU ({name})"),
                None => format!("NPU ({name})"),
            },
            device_path: Some(device_path),
            driver,
            vendor,
            memory_bytes: None,
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

/// GPU：以 `/sys/class/drm` 的 `cardN` 为存在性判据，名字尽量从 sysfs 取。
pub(crate) fn probe_gpus(root: &Path, warnings: &mut Vec<String>) -> Vec<Accelerator> {
    let drm = root.join("sys/class/drm");
    let mut found: Vec<Accelerator> = Vec::new();

    if let Ok(entries) = fs::read_dir(&drm) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // 只要 cardN，不要 cardN-DP-1 这类连接器
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let device_dir = entry.path().join("device");
            let vendor = read_trimmed(&device_dir.join("vendor")).map(|v| vendor_name(&v));
            let device_id =
                read_trimmed(&device_dir.join("device")).unwrap_or_else(|| "未知".into());
            let driver = driver_of(&device_dir);
            let mut notes = Vec::new();
            if driver.is_none() {
                notes.push("未绑定内核驱动，硬件加速不可用".into());
            }
            found.push(Accelerator {
                kind: AcceleratorKind::Gpu,
                name: match &vendor {
                    Some(vendor) => format!("{vendor} GPU ({name}, id {device_id})"),
                    None => format!("GPU ({name}, id {device_id})"),
                },
                device_path: None,
                driver,
                vendor,
                memory_bytes: None,
                notes,
            });
        }
    }

    // 把 /dev/dri/renderD* 关联到 cardN（顺序通常一致，但不保证，因此只做标注不硬绑）。
    let mut render_nodes: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(root.join("dev/dri")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("renderD") {
                render_nodes.push(entry.path());
            }
        }
    }
    render_nodes.sort();
    for (index, node) in render_nodes.iter().enumerate() {
        match found.get_mut(index) {
            Some(gpu) => {
                gpu.device_path = Some(node.clone());
                if !node.exists() {
                    gpu.notes.push(format!("{} 不存在", node.display()));
                }
            }
            None => found.push(Accelerator {
                kind: AcceleratorKind::Gpu,
                name: format!("GPU ({})", node.display()),
                device_path: Some(node.clone()),
                driver: None,
                vendor: None,
                memory_bytes: None,
                notes: vec!["sysfs 中没有对应的 cardN，信息不完整".into()],
            }),
        }
    }

    if found.is_empty() && drm.exists() {
        warnings.push("存在 /sys/class/drm 但没有可用 GPU 条目".into());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("deviceinfo-accel-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn finds_npu_and_gpu_from_sysfs() {
        let root = fake_root("both");
        fs::create_dir_all(root.join("proc")).unwrap();
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();
        fs::write(root.join("proc/cpuinfo"), "processor\t: 0\nmodel name\t: x\n").unwrap();
        // 这棵假树专注测加速器，先把 CPU 侧搭全，免得混入无关警告
        fs::create_dir_all(root.join("sys/devices/system/cpu/smt")).unwrap();
        fs::write(root.join("sys/devices/system/cpu/smt/active"), "0\n").unwrap();
        let cpu0 = root.join("sys/devices/system/cpu/cpu0/topology");
        fs::create_dir_all(&cpu0).unwrap();
        fs::write(cpu0.join("core_cpus_list"), "0\n").unwrap();

        // NPU
        fs::create_dir_all(root.join("dev/accel")).unwrap();
        fs::write(root.join("dev/accel/accel0"), "").unwrap();
        fs::create_dir_all(root.join("sys/class/accel/accel0/device")).unwrap();
        fs::write(root.join("sys/class/accel/accel0/device/vendor"), "0x8086\n").unwrap();

        // GPU：card0 有 vendor/device，/dev/dri 有 renderD128
        fs::create_dir_all(root.join("sys/class/drm/card0/device")).unwrap();
        fs::write(root.join("sys/class/drm/card0/device/vendor"), "0x8086\n").unwrap();
        fs::write(root.join("sys/class/drm/card0/device/device"), "0x7d55\n").unwrap();
        fs::create_dir_all(root.join("dev/dri")).unwrap();
        fs::write(root.join("dev/dri/renderD128"), "").unwrap();

        let report = crate::probe_with(&root, "x86_64");
        assert!(report.has_npu(), "应识别出 NPU: {report:#?}");
        assert!(report.has_gpu(), "应识别出 GPU: {report:#?}");

        let npu = report
            .accelerators
            .iter()
            .find(|a| a.kind == AcceleratorKind::Npu)
            .unwrap();
        assert_eq!(npu.vendor.as_deref(), Some("Intel"));
        assert!(npu.notes.is_empty(), "sysfs 存在时不应报缺驱动: {npu:#?}");

        let gpu = report
            .accelerators
            .iter()
            .find(|a| a.kind == AcceleratorKind::Gpu)
            .unwrap();
        assert_eq!(
            gpu.device_path.as_deref(),
            Some(root.join("dev/dri/renderD128").as_path())
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn gpu_without_a_bound_driver_is_noted() {
        let root = fake_root("nodriver");
        fs::create_dir_all(root.join("sys/class/drm/card0/device")).unwrap();
        fs::write(root.join("sys/class/drm/card0/device/vendor"), "0x1002\n").unwrap();

        let mut warnings = Vec::new();
        let gpus = probe_gpus(&root, &mut warnings);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].vendor.as_deref(), Some("AMD"));
        assert!(
            gpus[0].notes.iter().any(|n| n.contains("未绑定内核驱动")),
            "{:#?}",
            gpus[0]
        );

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
        assert!(probe_gpus(&root, &mut warnings).is_empty());
        fs::remove_dir_all(&root).ok();
    }
}
