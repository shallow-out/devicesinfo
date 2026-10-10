//! Consumer contracts for reboot/hotplug-safe sampling. All captures are synthetic.
use deviceinfo::{
    CaptureMetadata, DeviceAssociation, DifferenceError as Error, ObservationOrigin, SampleContext,
    SampleStamp, Snapshot,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
const BOOT: &str = "01234567-89ab-cdef-0123-456789abcdef";
const OTHER_BOOT: &str = "fedcba98-7654-3210-fedc-ba9876543210";

fn stamp(time: u64, boot: &str) -> SampleStamp {
    SampleStamp {
        unix_time_ns: Some(1_792_000_000_123_456_789),
        boot_time_ns: Some(time),
        boot_clock_resolution_ns: Some(1),
        boot_id: Some(boot.into()),
        time_namespace: Some("time:[42]".into()),
        mount_namespace: Some("mnt:[43]".into()),
    }
}
fn context(time: u64, boot: &str) -> SampleContext {
    SampleContext::from_bounds(
        ObservationOrigin::Captured,
        stamp(time, boot),
        stamp(time + 100, boot),
    )
}
fn device() -> DeviceAssociation {
    DeviceAssociation {
        source: "/sys/class/accel/accel0".into(),
        physical_path: Some("/sys/devices/pci0000:00/0000:00:0b.0".into()),
        kernel_instance: Some("0:123:0:456".into()),
        driver: Some("intel_vpu".into()),
        device_number: Some("261:0".into()),
    }
}
fn snapshot(time: u64, boot: &str) -> Snapshot<()> {
    Snapshot {
        schema_version: deviceinfo::SCHEMA_VERSION,
        context: context(time, boot),
        devices: vec![device()],
        data: (),
    }
}

#[test]
fn differences_use_boot_time_even_when_wall_clock_moves_backwards() {
    let before = snapshot(1_000_000_000, BOOT);
    let mut after = snapshot(2_000_000_000, BOOT);
    after.context.started.unix_time_ns = Some(1);
    after.context.finished.unix_time_ns = Some(2);
    let difference = after
        .counter_since(&before, &device().source, Some(150), Some(100))
        .unwrap();
    assert_eq!(difference.elapsed_ns, 1_000_000_000);
    assert_eq!(difference.delta, 50);
    assert_eq!(difference.per_second(), 50.0);
}

#[test]
fn increasing_counters_from_another_boot_are_rejected() {
    let before = snapshot(1_000_000_000, BOOT);
    let after = snapshot(2_000_000_000, OTHER_BOOT);
    assert_eq!(
        after.counter_since(&before, &device().source, Some(1000), Some(10)),
        Err(Error::DifferentBoot)
    );
}

#[test]
fn reboot_inside_the_sampling_window_and_missing_identity_are_unknown() {
    let before = snapshot(1_000_000_000, BOOT);
    let mut after = snapshot(2_000_000_000, BOOT);
    after.context = SampleContext::from_bounds(
        ObservationOrigin::Live,
        stamp(2_000_000_000, BOOT),
        stamp(3_000_000_000, OTHER_BOOT),
    );
    assert_eq!(after.elapsed_since(&before), Err(Error::InvalidContext));
    // A deserialized boolean cannot override invalid or missing identity fields.
    after.context.consistent = true;
    assert_eq!(after.elapsed_since(&before), Err(Error::InvalidContext));
    after.context = context(2_000_000_000, BOOT);
    after.context.started.boot_id = None;
    assert_eq!(after.elapsed_since(&before), Err(Error::InvalidContext));
}

#[test]
fn time_and_mount_namespace_changes_are_separate_from_reboot() {
    let before = snapshot(1_000_000_000, BOOT);
    for time_namespace in [true, false] {
        let mut after = snapshot(2_000_000_000, BOOT);
        for stamp in [&mut after.context.started, &mut after.context.finished] {
            if time_namespace {
                stamp.time_namespace = Some("time:[99]".into());
            } else {
                stamp.mount_namespace = Some("mnt:[99]".into());
            }
        }
        assert_eq!(after.elapsed_since(&before), Err(Error::DifferentNamespace));
    }
}

#[test]
fn overlapping_windows_duplicate_captures_and_clock_precision_are_rejected() {
    let before = snapshot(1_000_000_000, BOOT);
    assert_eq!(before.elapsed_since(&before), Err(Error::NonIncreasingTime));
    let after = snapshot(1_000_000_050, BOOT);
    assert_eq!(after.elapsed_since(&before), Err(Error::NonIncreasingTime));
    let mut coarse = before.clone();
    coarse.context.finished.boot_clock_resolution_ns = Some(10_000_000);
    let after = snapshot(1_001_000_000, BOOT);
    assert_eq!(after.elapsed_since(&coarse), Err(Error::NonIncreasingTime));
    let mut unknown_resolution = before.clone();
    unknown_resolution.context.finished.boot_clock_resolution_ns = None;
    assert_eq!(
        snapshot(2_000_000_000, BOOT).elapsed_since(&unknown_resolution),
        Err(Error::InvalidContext)
    );
}

#[test]
fn device_replacement_remapping_driver_changes_and_counter_resets_are_rejected() {
    let before = snapshot(1_000_000_000, BOOT);
    for field in 0..4 {
        let mut after = snapshot(2_000_000_000, BOOT);
        let association = &mut after.devices[0];
        match field {
            0 => association.kernel_instance = Some("0:888:0:999".into()),
            1 => association.physical_path = Some("/sys/devices/pci0000:00/0000:00:0c.0".into()),
            2 => association.driver = Some("replacement".into()),
            _ => association.device_number = Some("261:1".into()),
        }
        assert_eq!(
            after.counter_since(&before, &device().source, Some(200), Some(100)),
            Err(Error::DeviceChanged)
        );
    }
    let after = snapshot(2_000_000_000, BOOT);
    assert_eq!(
        after.counter_since(&before, &device().source, Some(90), Some(100)),
        Err(Error::CounterReset)
    );
    assert_eq!(
        after.counter_since(&before, &device().source, None, Some(100)),
        Err(Error::MissingCounter)
    );
    let mut unknown = after.clone();
    unknown.devices[0].kernel_instance = None;
    assert_eq!(
        unknown.counter_since(&before, &device().source, Some(200), Some(100)),
        Err(Error::UnknownDevice)
    );
    unknown.devices.clear();
    assert_eq!(
        unknown.counter_since(&before, &device().source, Some(200), Some(100)),
        Err(Error::UnknownDevice)
    );
}

#[test]
fn nanosecond_json_is_lossless_and_schema_changes_require_migration() {
    let before = snapshot(1_000_000_000, BOOT);
    let value = serde_json::to_value(&before).unwrap();
    assert_eq!(
        value["context"]["started"]["unix_time_ns"],
        "1792000000123456789"
    );
    assert_eq!(value["context"]["started"]["boot_time_ns"], "1000000000");
    assert_eq!(
        serde_json::from_value::<Snapshot<()>>(value).unwrap(),
        before
    );
    let mut after = snapshot(2_000_000_000, BOOT);
    after.schema_version = 1;
    assert_eq!(after.elapsed_since(&before), Err(Error::SchemaMismatch));
}

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "deviceinfo-snapshot-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn capture(&self, time: u64, boot: &str, preserved: bool, devices: Vec<DeviceAssociation>) {
        self.write(deviceinfo::BOOT_ID_INPUT, boot);
        let metadata = CaptureMetadata {
            schema_version: deviceinfo::SCHEMA_VERSION,
            context: context(time, boot),
            devices,
            counters_preserved: preserved,
        };
        self.write(
            deviceinfo::CONTEXT_FILE,
            &serde_json::to_string(&metadata).unwrap(),
        );
    }
    fn sample(&self) -> Snapshot<deviceinfo::SystemState> {
        deviceinfo::observe_system(&self.0, &deviceinfo::SystemSampleOptions::default())
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn cpu_consumer_uses_context_and_repeated_replay_never_produces_a_percentage() {
    let root = Root::new();
    root.capture(1_000_000_000, BOOT, true, Vec::new());
    root.write("proc/stat", "cpu 100 0 0 100 0 0 0 0");
    let before = root.sample();
    root.capture(2_000_000_000, BOOT, true, Vec::new());
    assert_eq!(
        root.sample().cpu_usage_since(&before),
        Err(Error::NoCounterProgress)
    );
    root.write("proc/stat", "cpu 150 0 0 150 0 0 0 0");
    let after = root.sample();
    assert_eq!(after.cpu_usage_since(&before), Ok(50.0));
    assert_eq!(
        root.sample().cpu_usage_since(&after),
        Err(Error::NonIncreasingTime)
    );
    root.capture(3_000_000_000, OTHER_BOOT, true, Vec::new());
    root.write("proc/stat", "cpu 1000 0 0 1000 0 0 0 0");
    assert_eq!(
        root.sample().cpu_usage_since(&after),
        Err(Error::DifferentBoot)
    );
    root.capture(4_000_000_000, BOOT, false, Vec::new());
    assert_eq!(
        root.sample().cpu_usage_since(&after),
        Err(Error::InvalidContext)
    );
}

#[test]
fn missing_corrupt_or_mismatched_capture_metadata_never_borrows_host_identity() {
    let root = Root::new();
    let missing = root.sample();
    assert_eq!(missing.context.origin, ObservationOrigin::Unattributed);
    assert_eq!(missing.context.started.unix_time_ns, None);
    assert_eq!(missing.context.started.boot_time_ns, None);
    assert!(!missing.context.consistent);
    assert_eq!(
        missing.context.diagnostics[0].code,
        deviceinfo::DiagnosticCode::NotExposed
    );
    root.write(deviceinfo::CONTEXT_FILE, "broken");
    assert_eq!(
        root.sample().context.diagnostics[0].code,
        deviceinfo::DiagnosticCode::InvalidData
    );
    root.capture(1_000_000_000, BOOT, true, Vec::new());
    root.write(deviceinfo::BOOT_ID_INPUT, OTHER_BOOT);
    assert!(!root.sample().context.consistent);
}

#[test]
fn thermal_channels_join_source_device_and_preserve_captured_kernel_instance() {
    let root = Root::new();
    root.write("sys/devices/platform/fan-controller/label", "fixture");
    root.write("sys/class/hwmon/hwmon0/name", "fan-controller");
    root.write("sys/class/hwmon/hwmon0/temp1_input", "42000");
    root.write("sys/class/hwmon/hwmon0/fan1_input", "1000");
    root.write("sys/class/thermal/cooling_device0/type", "fan");
    root.write("sys/class/thermal/cooling_device0/cur_state", "1");
    root.write("sys/class/thermal/cooling_device0/max_state", "2");
    std::os::unix::fs::symlink(
        "/sys/devices/platform/fan-controller",
        root.0.join("sys/class/hwmon/hwmon0/device"),
    )
    .unwrap();
    let read = || deviceinfo::observe_thermal(&root.0, &deviceinfo::ThermalOptions::default());
    let unknown = read();
    let temperature = unknown
        .devices
        .iter()
        .find(|device| device.source == Path::new("/sys/class/hwmon/hwmon0/temp1_input"))
        .unwrap();
    assert_eq!(
        temperature.physical_path.as_deref(),
        Some(Path::new("/sys/devices/platform/fan-controller"))
    );
    assert_eq!(temperature.kernel_instance, None);
    let cooling = unknown
        .devices
        .iter()
        .find(|device| device.source == Path::new("/sys/class/thermal/cooling_device0"))
        .unwrap();
    assert_eq!(
        cooling.physical_path.as_deref(),
        Some(Path::new("/sys/class/thermal/cooling_device0"))
    );
    let mut association = temperature.clone();
    association.source = "/sys/class/hwmon/hwmon0".into();
    association.kernel_instance = Some("source-kernel-instance".into());
    root.capture(1_000_000_000, BOOT, true, vec![association]);
    let captured = read();
    let channels: Vec<_> = captured
        .devices
        .iter()
        .filter(|device| device.source.starts_with("/sys/class/hwmon/hwmon0"))
        .collect();
    assert_eq!(channels.len(), 2);
    assert!(
        channels
            .iter()
            .all(|device| device.kernel_instance.as_deref() == Some("source-kernel-instance"))
    );
}

#[test]
fn explicit_mmc_access_is_blocked_for_injected_roots() {
    let root = Root::new();
    let report = deviceinfo::observe_mmc_health(&root.0, Path::new("/dev/mmcblk0"));
    assert!(report.data.health.is_none());
    assert_eq!(
        report.data.diagnostics[0].code,
        deviceinfo::DiagnosticCode::Unsupported
    );
}

#[test]
fn identical_pci_models_and_display_without_render_node_join_by_source() {
    let root = Root::new();
    root.write("proc/cpuinfo", "processor: 0\nmodel name: fixture\n");
    root.write("proc/meminfo", "MemTotal: 1000 kB\n");
    for (node, render) in [("card0", "renderD128"), ("card1", "renderD129")] {
        let prefix = format!("sys/class/drm/{node}");
        root.write(&format!("{prefix}/device/vendor"), "0x8086");
        root.write(&format!("{prefix}/device/device"), "0x1234");
        root.write(&format!("{prefix}/device/drm/{render}"), "");
        root.write(&format!("dev/dri/{render}"), "");
    }
    fs::create_dir_all(root.0.join("sys/class/drm/card2/device/drm")).unwrap();
    let hardware = deviceinfo::inspect_hardware(&root.0, "x86_64");
    let state = deviceinfo::observe_accelerators(&root.0, &deviceinfo::SampleOptions::default());
    assert_eq!(hardware.data.accelerators.len(), 3);
    assert_eq!(
        hardware.data.accelerators[0].pci_id,
        hardware.data.accelerators[1].pci_id
    );
    assert_eq!(
        hardware.data.accelerators[2].kind,
        deviceinfo::AcceleratorKind::Display
    );
    assert_eq!(hardware.data.accelerators[2].device_path, None);
    for index in 0..3 {
        let expected = PathBuf::from(format!("/sys/class/drm/card{index}"));
        assert_eq!(hardware.data.accelerators[index].source, expected);
        assert_eq!(state.data.accelerators[index].source, expected);
        assert!(
            hardware
                .devices
                .iter()
                .any(|device| device.source == expected)
        );
        assert!(state.devices.iter().any(|device| device.source == expected));
    }
}
