//! Consumer-level regression tests. Q8B-style inputs below are synthetic contracts,
//! not captures of a physical board or evidence of its exact frequencies.
use deviceinfo::{
    AcceleratorKind, AcceleratorMemory, SystemSampleOptions, probe_system_with, probe_with,
    sample_system_state_with,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

struct Root(PathBuf);
impl Root {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("deviceinfo-consumer-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn q8b_contract(root: &Root) {
    root.write(
        "etc/os-release",
        "ID=ubuntu\nID_LIKE=debian\nPRETTY_NAME=\"Ubuntu fixture\"\nVERSION_ID=26.04\n",
    );
    root.write("proc/sys/kernel/osrelease", "fixture-qcom-kernel\n");
    root.write("proc/sys/kernel/hostname", "q8b-fixture\n");
    root.write("sys/firmware/devicetree/base/model", "Radxa Dragon Q8B\0");
    root.write("proc/meminfo", "MemTotal: 16000000 kB\nMemAvailable: 12000000 kB\nSwapTotal: 4000000 kB\nSwapFree: 1000000 kB\n");
    root.write("proc/stat", "cpu 100 20 30 400 50 2 3 4 80 10\n");
    root.write("proc/loadavg", "0.25 0.50 0.75 2/100 400\n");
    root.write("proc/uptime", "123.50 500.0\n");
    let mut cpuinfo = String::new();
    for cpu in 0..8 {
        // Deliberately sparse feature availability: no fabricated all-core claim.
        let features = if cpu < 4 {
            "fp asimd aes"
        } else {
            "fp asimd aes sve"
        };
        cpuinfo.push_str(&format!("processor : {cpu}\nFeatures : {features}\n\n"));
        root.write(
            &format!("sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list"),
            &cpu.to_string(),
        );
        let (capacity, max_khz) = if cpu < 4 {
            (600, 2_400_000)
        } else {
            (1024, 3_000_000)
        };
        root.write(
            &format!("sys/devices/system/cpu/cpu{cpu}/cpu_capacity"),
            &capacity.to_string(),
        );
        root.write(
            &format!("sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq"),
            &max_khz.to_string(),
        );
    }
    root.write("proc/cpuinfo", &cpuinfo);
}

#[test]
fn q8b_style_cpu_memory_and_identity_use_only_lightweight_inputs() {
    let root = Root::new("q8b-system");
    q8b_contract(&root);
    let report = probe_system_with(&root.0, "aarch64");
    assert_eq!(report.cpu.arch, "aarch64");
    assert_eq!(
        report.cpu.machine_model.as_deref(),
        Some("Radxa Dragon Q8B")
    );
    assert_eq!(report.cpu.logical_cores, 8);
    assert_eq!(report.cpu.physical_cores, Some(8));
    assert_eq!(report.cpu.core_tiers.len(), 2);
    assert!(
        !report
            .cpu
            .features_for_cpus(&[0])
            .unwrap()
            .contains(&"sve".into())
    );
    assert!(
        report
            .cpu
            .features_for_cpus(&[4])
            .unwrap()
            .contains(&"sve".into())
    );
    assert!(
        !report
            .cpu
            .common_features
            .as_ref()
            .unwrap()
            .contains(&"sve".into())
    );
    assert_eq!(
        report
            .cpu
            .core_tiers
            .iter()
            .map(|tier| tier.physical_cores.unwrap())
            .sum::<usize>(),
        8
    );
    assert_eq!(report.memory.total_bytes, Some(16_000_000 * 1024));
    assert_eq!(
        report.operating_system.as_ref().unwrap().id.as_deref(),
        Some("ubuntu")
    );
    assert_eq!(
        report.kernel_release.as_deref(),
        Some("fixture-qcom-kernel")
    );
    assert_eq!(report.hostname.as_deref(), Some("q8b-fixture"));
    // No /dev, DRM, PCI or runtime library inputs exist. They must not warn.
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let all = probe_with(&root.0, "aarch64");
    assert_eq!(report.cpu, all.cpu);
    assert_eq!(report.memory, all.memory);
}

#[test]
fn system_sampling_observes_changes_without_rescanning_cached_identity() {
    let root = Root::new("q8b-state");
    q8b_contract(&root);
    let options = SystemSampleOptions::default();
    let before = sample_system_state_with(&root.0, &options);
    root.write("proc/stat", "cpu 120 20 40 430 60 2 3 4 100 10\n");
    root.write(
        "proc/meminfo",
        "MemTotal: 16000000 kB\nMemAvailable: 8000000 kB\nSwapTotal: 4000000 kB\nSwapFree: 0 kB\n",
    );
    root.write("proc/loadavg", "12.0 3.0 2.0\n");
    let after = sample_system_state_with(&root.0, &options);
    assert_eq!(before.memory.used_bytes(), Some(4_000_000 * 1024));
    assert_eq!(after.memory.used_bytes(), Some(8_000_000 * 1024));
    assert!(after.memory.swap_exhausted());
    let percent = after
        .cpu
        .unwrap()
        .usage_since(&before.cpu.unwrap())
        .unwrap();
    assert!((percent - 100.0 * 30.0 / 70.0).abs() < 1e-10);
    assert_eq!(after.load_average, Some([12.0, 3.0, 2.0]));
    assert_eq!(after.uptime_seconds, Some(123.5));
    assert!(after.warnings.is_empty(), "{:?}", after.warnings);
}

#[test]
fn unknown_inputs_and_invalid_metrics_are_explicit_and_serializable() {
    let root = Root::new("missing");
    root.write("proc/loadavg", "NaN 1 2");
    root.write("proc/uptime", "-1");
    let state = sample_system_state_with(&root.0, &SystemSampleOptions::default());
    assert_eq!(state.memory.used_bytes(), None);
    let inconsistent_memory = deviceinfo::MemoryState {
        total_bytes: Some(100),
        available_bytes: Some(101),
        ..state.memory.clone()
    };
    assert_eq!(inconsistent_memory.used_bytes(), None);
    assert_eq!(state.cpu, None);
    assert_eq!(state.load_average, None);
    assert_eq!(state.uptime_seconds, None);
    assert_eq!(state.warnings.len(), 4);
    let json = serde_json::to_value(&state).unwrap();
    assert!(json["cpu"].is_null());
    assert!(json["load_average"].is_null());
    let report = probe_system_with(&root.0, "aarch64");
    assert_eq!(report.operating_system, None);
    assert_eq!(report.memory.total_bytes, None);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("os-release"))
    );
}

#[test]
fn watched_disks_are_host_filesystems_even_with_an_injected_proc_root() {
    let root = Root::new("disks");
    q8b_contract(&root);
    let missing = root.0.join("absent");
    let state = sample_system_state_with(
        &root.0,
        &SystemSampleOptions {
            watch: vec![root.0.clone(), missing],
        },
    );
    assert_eq!(state.disks.len(), 1);
    assert_eq!(state.disks[0].path, root.0);
    assert!(state.disks[0].total_bytes > 0);
    assert_eq!(
        state.disks[0].used_bytes(),
        state.disks[0].total_bytes - state.disks[0].free_bytes
    );
    assert_eq!(state.warnings.len(), 1);
    // Reserved free blocks must not be counted as already allocated space.
    let reserved_blocks = deviceinfo::DiskUsage {
        path: root.0.clone(),
        total_bytes: 100,
        free_bytes: 30,
        available_bytes: 20,
    };
    assert_eq!(reserved_blocks.used_bytes(), 70);
}

#[test]
fn q8b_style_adreno_compute_and_display_only_cards_remain_distinct() {
    use std::os::unix::fs::symlink;
    let root = Root::new("q8b-drm");
    q8b_contract(&root);
    let compute = "sys/class/drm/card0/device";
    root.write(
        &format!("{compute}/of_node/compatible"),
        "qcom,adreno-690\0",
    );
    fs::create_dir_all(root.0.join(compute).join("drm/renderD128")).unwrap();
    root.write("sys/module/msm/version", "fixture-only\n");
    fs::create_dir_all(root.0.join("sys/bus/platform/drivers/msm")).unwrap();
    symlink(
        Path::new("../../../../bus/platform/drivers/msm"),
        root.0.join(compute).join("driver"),
    )
    .unwrap();
    let display = "sys/class/drm/card1/device";
    root.write(
        &format!("{display}/of_node/compatible"),
        "qcom,sc8280xp-mdss\0",
    );
    fs::create_dir_all(root.0.join(display).join("drm/card1")).unwrap();
    let report = probe_with(&root.0, "aarch64");
    assert_eq!(report.accelerators.len(), 2);
    assert_eq!(report.accelerators[0].kind, AcceleratorKind::Gpu);
    assert_eq!(
        report.accelerators[0].memory,
        AcceleratorMemory::SharedWithSystem
    );
    assert_eq!(report.accelerators[1].kind, AcceleratorKind::Display);
    assert!(!report.has_npu());
}
