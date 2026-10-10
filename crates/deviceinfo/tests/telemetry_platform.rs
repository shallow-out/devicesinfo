//! Synthetic consumer contracts, not captures of a physical Q8B or fan wiring.
use deviceinfo::{
    Diagnostic, DiagnosticCode, DiagnosticOperation, Exposure, TemperatureUnit, ThermalOptions,
    probe_platform, sample_thermal, sample_thermal_with,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Self(std::env::temp_dir().join(format!(
            "deviceinfo-p0-{}-{stamp}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir_all(&root.0).unwrap();
        root
    }
    fn bytes(&self, path: &str, bytes: &[u8]) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn write(&self, path: &str, text: &str) {
        self.bytes(path, text.as_bytes());
    }
    fn dir(&self, path: &str) {
        fs::create_dir_all(self.0.join(path)).unwrap();
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn q8b_contract_allows_uefi_and_dt_and_independent_acpi_observation() {
    let root = Root::new();
    root.write("sys/firmware/efi/fw_platform_size", "64\n");
    root.write("sys/firmware/devicetree/base/model", "Radxa Dragon Q8B\0");
    root.write(
        "sys/firmware/devicetree/base/compatible",
        "radxa,dragon-q8b\0qcom,qcs6490\0",
    );
    let report = probe_platform(&root.0);
    assert_eq!(report.uefi.exposure, Exposure::Exposed);
    assert_eq!(report.device_tree.exposure, Exposure::Exposed);
    assert_eq!(report.acpi.exposure, Exposure::NotExposed);
    assert_eq!(report.uefi_platform_bits, Some(64));
    let model = report.device_tree_model.unwrap();
    assert_eq!(model.value, "Radxa Dragon Q8B");
    assert_eq!(
        model.source,
        Path::new("/sys/firmware/devicetree/base/model")
    );
    root.dir("sys/firmware/acpi/tables");
    assert_eq!(probe_platform(&root.0).acpi.exposure, Exposure::Exposed);
}

#[test]
fn dmi_versions_and_dt_model_are_evidence_not_an_arbitrary_winner() {
    let root = Root::new();
    root.write("sys/class/dmi/id/product_name", "Different DMI name\n");
    root.write("sys/class/dmi/id/board_version", "Board Rev 2\n");
    root.write("sys/class/dmi/id/bios_version", "Firmware 2026.10\n");
    root.write("sys/firmware/devicetree/base/model", "DT board\0");
    root.write("sys/firmware/devicetree/base/compatible", "vendor,board\0");
    let report = probe_platform(&root.0);
    assert_eq!(report.dmi.product_name.unwrap().value, "Different DMI name");
    assert_eq!(report.device_tree_model.unwrap().value, "DT board");
    assert_eq!(report.dmi.board_version.unwrap().value, "Board Rev 2");
    assert_eq!(report.dmi.bios_version.unwrap().value, "Firmware 2026.10");
    assert_eq!(report.uefi.exposure, Exposure::NotExposed);
}

#[test]
fn malformed_firmware_attributes_remain_unknown_with_path_and_code() {
    let root = Root::new();
    root.write("sys/firmware/efi/fw_platform_size", "128\n");
    root.write("sys/firmware/devicetree/base/model", "Not NUL terminated");
    root.bytes("sys/firmware/devicetree/base/compatible", &[255, 0]);
    root.write("sys/firmware/acpi/tables", "not a directory");
    let report = probe_platform(&root.0);
    assert_eq!(report.uefi.exposure, Exposure::Exposed);
    assert_eq!(report.uefi_platform_bits, None);
    assert_eq!(report.acpi.exposure, Exposure::Unknown);
    assert_eq!(report.device_tree_model, None);
    assert!(report.device_tree_compatible.is_empty());
    for path in [
        "/sys/firmware/efi/fw_platform_size",
        "/sys/firmware/devicetree/base/model",
        "/sys/firmware/devicetree/base/compatible",
        "/sys/firmware/acpi/tables",
    ] {
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == Path::new(path)
                    && diagnostic.code == DiagnosticCode::InvalidData),
            "{path}"
        );
    }
    assert!(
        report
            .diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains(root.0.to_str().unwrap()))
    );
}

#[test]
fn diagnostics_classify_all_five_failures_without_parsing_messages() {
    for (kind, expected) in [
        (io::ErrorKind::Unsupported, DiagnosticCode::Unsupported),
        (io::ErrorKind::NotFound, DiagnosticCode::NotExposed),
        (
            io::ErrorKind::PermissionDenied,
            DiagnosticCode::PermissionDenied,
        ),
        (io::ErrorKind::Other, DiagnosticCode::ReadFailed),
        (io::ErrorKind::InvalidData, DiagnosticCode::InvalidData),
    ] {
        let diagnostic = Diagnostic::from_io(
            Some("fan1"),
            Path::new("/sys/sensor"),
            DiagnosticOperation::Read,
            &io::Error::new(kind, "任意显示文本"),
        );
        assert_eq!(diagnostic.code, expected);
        assert_eq!(diagnostic.device.as_deref(), Some("fan1"));
        assert_eq!(diagnostic.path, Path::new("/sys/sensor"));
        assert_eq!(
            serde_json::from_value::<Diagnostic>(serde_json::to_value(&diagnostic).unwrap())
                .unwrap(),
            diagnostic
        );
    }
    let error = io::Error::from_raw_os_error(libc::EACCES);
    let diagnostic = Diagnostic::from_io(
        None,
        Path::new("/sys/private"),
        DiagnosticOperation::Inspect,
        &error,
    );
    assert_eq!(diagnostic.errno, Some(libc::EACCES));
    assert_eq!(diagnostic.code, DiagnosticCode::PermissionDenied);
    let error = io::Error::from_raw_os_error(libc::EINVAL);
    assert_eq!(
        Diagnostic::from_io(
            None,
            Path::new("/sys/sensor"),
            DiagnosticOperation::Read,
            &error
        )
        .code,
        DiagnosticCode::ReadFailed
    );
    assert_eq!(
        Diagnostic::from_io(
            None,
            Path::new("/sys/sensor"),
            DiagnosticOperation::Decode,
            &error
        )
        .code,
        DiagnosticCode::InvalidData
    );
}

#[test]
fn temperatures_preserve_negative_values_and_trip_semantics() {
    let root = Root::new();
    root.write("sys/class/thermal/thermal_zone2/type", "soc-thermal\n");
    root.write("sys/class/thermal/thermal_zone2/temp", "-1250\n");
    root.write(
        "sys/class/thermal/thermal_zone2/trip_point_0_temp",
        "95000\n",
    );
    root.write(
        "sys/class/thermal/thermal_zone2/trip_point_0_type",
        "critical\n",
    );
    root.write(
        "sys/class/thermal/thermal_zone2/trip_point_0_hyst",
        "2000\n",
    );
    root.write("sys/class/hwmon/hwmon0/name", "board-sensors\n");
    root.write("sys/class/hwmon/hwmon0/temp1_input", "0\n");
    root.write("sys/class/hwmon/hwmon0/temp1_label", "Ambient\n");
    root.write("sys/class/hwmon/hwmon0/temp1_crit", "100000\n");
    root.write("sys/class/hwmon/hwmon0/temp1_crit_alarm", "1\n");
    let report = sample_thermal(&root.0);
    assert_eq!(report.temperatures.len(), 2);
    assert_eq!(report.temperatures[0].temperature_millicelsius, Some(-1250));
    assert_eq!(report.temperatures[0].thresholds[0].kind, "critical");
    assert_eq!(
        report.temperatures[0].thresholds[0].hysteresis_raw,
        Some(2000)
    );
    assert_eq!(
        report.temperatures[0].thresholds[0].hysteresis_kind,
        deviceinfo::HysteresisKind::DeltaFromTrip
    );
    assert_eq!(
        report.temperatures[1].thresholds[0].hysteresis_kind,
        deviceinfo::HysteresisKind::AbsoluteThreshold
    );
    assert_eq!(report.temperatures[1].temperature_millicelsius, Some(0));
    assert_eq!(
        report.temperatures[1].alarms["temp1_crit_alarm"],
        Some(true)
    );
    assert!(report.diagnostics.is_empty());
}

#[test]
fn measured_rpm_pwm_target_and_cooling_state_are_never_substituted() {
    let root = Root::new();
    root.write("sys/class/hwmon/hwmon0/name", "fan-controller\n");
    root.write("sys/class/hwmon/hwmon0/fan1_input", "0\n");
    root.write("sys/class/hwmon/hwmon0/fan1_target", "3000\n");
    root.write("sys/class/hwmon/hwmon0/pwm2", "128\n");
    root.write("sys/class/hwmon/hwmon0/pwm2_enable", "3\n");
    root.write("sys/class/hwmon/hwmon0/pwm2_mode", "1\n");
    root.write("sys/class/hwmon/hwmon0/pwm2_freq", "25000\n");
    root.write("sys/class/thermal/cooling_device0/type", "pwm-fan\n");
    root.write("sys/class/thermal/cooling_device0/cur_state", "2\n");
    root.write("sys/class/thermal/cooling_device0/max_state", "5\n");
    let report = sample_thermal(&root.0);
    assert_eq!(report.fans[0].rpm, Some(0));
    assert_eq!(report.fans[0].target_rpm, Some(3000));
    assert_eq!(report.pwm[0].value_0_255, Some(128));
    assert_eq!(report.pwm[0].enable_mode, Some(3));
    assert_eq!(report.pwm[0].frequency_hz, Some(25000));
    assert_eq!(report.cooling_devices[0].current_state, Some(2));
    fs::remove_file(root.0.join("sys/class/hwmon/hwmon0/fan1_input")).unwrap();
    let report = sample_thermal(&root.0);
    assert_eq!(report.fans[0].rpm, None);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::NotExposed
                && diagnostic.device.as_deref() == Some("hwmon0/fan1"))
    );
}

#[test]
fn corrupt_inputs_and_invalid_states_are_not_zero_or_normal() {
    let root = Root::new();
    root.write("sys/class/hwmon/hwmon1/name", "bad-sensors");
    root.write(
        "sys/class/hwmon/hwmon1/temp1_input",
        "999999999999999999999999999999999999999",
    );
    root.write("sys/class/hwmon/hwmon1/fan1_input", "-1");
    root.write("sys/class/hwmon/hwmon1/pwm1", "256");
    root.write("sys/class/thermal/cooling_device1/type", "Processor");
    root.write("sys/class/thermal/cooling_device1/cur_state", "6");
    root.write("sys/class/thermal/cooling_device1/max_state", "5");
    let report = sample_thermal(&root.0);
    assert_eq!(report.temperatures[0].temperature_millicelsius, None);
    assert_eq!(report.fans[0].rpm, None);
    assert_eq!(report.pwm[0].value_0_255, None);
    assert_eq!(report.cooling_devices[0].current_state, None);
    assert_eq!(
        report
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::InvalidData)
            .count(),
        4
    );
}

#[test]
fn firmware_absolute_zero_sentinel_is_not_a_usable_temperature() {
    let root = Root::new();
    root.write("sys/class/thermal/thermal_zone0/type", "acpitz");
    root.write("sys/class/thermal/thermal_zone0/temp", "-273200");
    root.write("sys/class/hwmon/hwmon0/name", "acpitz");
    root.write("sys/class/hwmon/hwmon0/temp1_input", "-273200");
    let report = sample_thermal(&root.0);
    for sensor in report.temperatures {
        assert_eq!(sensor.raw_input, Some(-273200));
        assert_eq!(sensor.temperature_millicelsius, None);
    }
    assert_eq!(
        report
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::InvalidData
                && d.operation == DiagnosticOperation::Decode)
            .count(),
        2
    );
}

#[test]
fn faults_and_disabled_inputs_keep_raw_evidence_but_no_usable_measurement() {
    let root = Root::new();
    root.write("sys/class/hwmon/hwmon0/name", "untrusted");
    root.write("sys/class/hwmon/hwmon0/temp1_input", "30000");
    root.write("sys/class/hwmon/hwmon0/temp1_fault", "1");
    root.write("sys/class/hwmon/hwmon0/fan1_input", "2100");
    root.write("sys/class/hwmon/hwmon0/fan1_enable", "0");
    let report = sample_thermal(&root.0);
    assert_eq!(report.temperatures[0].temperature_millicelsius, None);
    assert_eq!(report.temperatures[0].raw_input, Some(30000));
    assert_eq!(report.fans[0].rpm, None);
    assert_eq!(report.fans[0].raw_rpm, Some(2100));
    root.write("sys/class/hwmon/hwmon0/temp1_fault", "2");
    root.write("sys/class/hwmon/hwmon0/fan1_enable", "2");
    let report = sample_thermal(&root.0);
    assert_eq!(report.temperatures[0].temperature_millicelsius, None);
    assert_eq!(report.fans[0].rpm, None);
    assert_eq!(
        report
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::InvalidData)
            .count(),
        2
    );
}

#[test]
fn thermistor_voltage_cannot_be_fabricated_as_a_celsius_temperature() {
    let root = Root::new();
    root.write("sys/class/hwmon/hwmon0/name", "adc");
    root.write("sys/class/hwmon/hwmon0/temp1_input", "1250");
    root.write("sys/class/hwmon/hwmon0/temp1_type", "4");
    let report = sample_thermal(&root.0);
    assert_eq!(report.temperatures[0].unit, TemperatureUnit::Unknown);
    assert_eq!(report.temperatures[0].temperature_millicelsius, None);
    let mut options = ThermalOptions::default();
    options.hwmon_temperature_units.insert(
        "/sys/class/hwmon/hwmon0/temp1_input".into(),
        TemperatureUnit::Millivolt,
    );
    let report = sample_thermal_with(&root.0, &options);
    assert_eq!(report.temperatures[0].raw_input, Some(1250));
    assert_eq!(report.temperatures[0].unit, TemperatureUnit::Millivolt);
    assert_eq!(report.temperatures[0].temperature_millicelsius, None);
    assert!(
        !report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::Unsupported)
    );
    for code in ["bad", "99"] {
        root.write("sys/class/hwmon/hwmon0/temp1_type", code);
        let report = sample_thermal(&root.0);
        assert_eq!(report.temperatures[0].unit, TemperatureUnit::Unknown);
        assert_eq!(report.temperatures[0].temperature_millicelsius, None);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::InvalidData
                    && d.path == Path::new("/sys/class/hwmon/hwmon0/temp1_type"))
        );
    }
}

#[cfg(unix)]
#[test]
fn absolute_sysfs_alias_is_resolved_inside_the_injected_root() {
    let root = Root::new();
    root.write("sys/devices/sensors/name", "fixture-only");
    root.write("sys/devices/sensors/temp1_input", "42500");
    root.dir("sys/class/hwmon");
    std::os::unix::fs::symlink(
        "/sys/devices/sensors",
        root.0.join("sys/class/hwmon/hwmon0"),
    )
    .unwrap();
    assert_eq!(
        sample_thermal(&root.0).temperatures[0].temperature_millicelsius,
        Some(42500)
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn non_linux_host_reports_unsupported_without_assuming_boot_mode() {
    let report = probe_platform(Path::new("/"));
    assert_eq!(report.uefi.exposure, Exposure::Unknown);
    assert_eq!(report.diagnostics[0].code, DiagnosticCode::Unsupported);
    let report = sample_thermal(Path::new("/"));
    assert!(report.temperatures.is_empty());
    assert_eq!(report.diagnostics[0].code, DiagnosticCode::Unsupported);
}
