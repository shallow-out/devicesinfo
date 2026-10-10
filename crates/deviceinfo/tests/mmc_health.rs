//! Pure EXT_CSD and sysfs contracts; no tests issue commands to a physical MMC.
use deviceinfo::{
    StorageHealthError, StorageHealthErrorKind, StorageHealthOptions, StorageHealthState,
    decode_mmc_ext_csd, sample_storage_health_with,
};
use std::{fs, io, path::PathBuf};

struct Root(PathBuf);
impl Root {
    fn new(name: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Self(std::env::temp_dir().join(format!(
            "deviceinfo-mmc-{name}-{}-{stamp}",
            std::process::id()
        )));
        root.write("sys/class/block/mmcblk0/device/type", "MMC");
        root
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn health(&self) -> deviceinfo::StorageHealthReport {
        sample_storage_health_with(&self.0, &StorageHealthOptions::default())
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn corrupt_lifetime_cannot_become_normal_or_hide_a_critical_peer() {
    let root = Root::new("corrupt");
    root.write("sys/class/block/mmcblk0/device/pre_eol_info", "0x01");
    for (life, expected, a, b) in [
        ("0xff 0x01", StorageHealthState::Unknown, None, Some(1)),
        ("0xff 0x0b", StorageHealthState::Critical, None, Some(11)),
        ("0x0b 0xff", StorageHealthState::Critical, Some(11), None),
        ("0xff 0x0a", StorageHealthState::Warning, None, Some(10)),
        ("0x01", StorageHealthState::Unknown, None, None),
        ("0x01 0x01 extra", StorageHealthState::Unknown, None, None),
    ] {
        root.write("sys/class/block/mmcblk0/device/life_time", life);
        let report = root.health();
        let health = &report.mmc[0];
        assert_eq!(health.state, expected, "{life}");
        assert_eq!(health.life_time_a, a);
        assert_eq!(health.life_time_b, b);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("Invalid MMC lifetime"))
        );
    }
    root.write("sys/class/block/mmcblk0/device/pre_eol_info", "0xff");
    root.write("sys/class/block/mmcblk0/device/life_time", "0x01 0x01");
    assert_eq!(root.health().mmc[0].state, StorageHealthState::Unknown);
}

#[test]
fn old_or_invalid_sysfs_revision_is_not_normal_health_evidence() {
    let root = Root::new("revision");
    root.write("sys/class/block/mmcblk0/device/pre_eol_info", "0x01");
    root.write("sys/class/block/mmcblk0/device/life_time", "0x01 0x01");
    root.write("sys/class/block/mmcblk0/device/rev", "0x06");
    let report = root.health();
    assert_eq!(report.mmc[0].state, StorageHealthState::Unknown);
    assert_eq!(report.mmc[0].pre_eol_info, None);
    root.write("sys/class/block/mmcblk0/device/rev", "broken");
    assert_eq!(root.health().mmc[0].state, StorageHealthState::Unknown);
    root.write("sys/class/block/mmcblk0/device/rev", "0x08");
    assert_eq!(root.health().mmc[0].state, StorageHealthState::Normal);
}

#[test]
fn ext_csd_revision_gate_ignores_older_reserved_health_bytes() {
    let mut bytes = [0; 512];
    bytes[267] = 255;
    bytes[268] = 255;
    bytes[269] = 255;
    for revision in 0..7 {
        bytes[192] = revision;
        let health = decode_mmc_ext_csd(&bytes).unwrap();
        assert_eq!(health.revision, revision);
        assert_eq!(health.state, StorageHealthState::Unknown);
        assert_eq!(health.pre_eol_info, None);
        assert_eq!(health.life_time_a, None);
        assert_eq!(health.life_time_b, None);
    }
    bytes[192] = 7;
    assert_eq!(
        decode_mmc_ext_csd(&bytes).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn ext_csd_keeps_undefined_lifetime_and_ten_percent_buckets_explicit() {
    let mut bytes = [0; 512];
    bytes[192] = 8;
    let unknown = decode_mmc_ext_csd(&bytes).unwrap();
    assert_eq!(unknown.state, StorageHealthState::Unknown);
    assert!(serde_json::to_value(unknown).unwrap()["life_time_a"].is_null());
    for (pre, a, b, state) in [
        (1, 1, 4, StorageHealthState::Normal),
        (2, 1, 1, StorageHealthState::Warning),
        (1, 10, 1, StorageHealthState::Warning),
        (1, 1, 11, StorageHealthState::Critical),
        (3, 1, 1, StorageHealthState::Critical),
    ] {
        bytes[267] = pre;
        bytes[268] = a;
        bytes[269] = b;
        let health = decode_mmc_ext_csd(&bytes).unwrap();
        assert_eq!(health.state, state);
        assert_eq!(health.life_time_a, Some(a));
        assert_eq!(health.life_time_b, Some(b));
    }
}

#[test]
fn malformed_ext_csd_has_a_typed_error_instead_of_a_health_verdict() {
    for len in [0, 511, 513] {
        let error = decode_mmc_ext_csd(&vec![0; len]).unwrap_err();
        let error = StorageHealthError::from(error);
        assert_eq!(error.kind, StorageHealthErrorKind::InvalidData);
    }
    let mut bytes = [0; 512];
    bytes[192] = 7;
    bytes[268] = 12;
    let error = StorageHealthError::from(decode_mmc_ext_csd(&bytes).unwrap_err());
    assert_eq!(serde_json::to_value(error).unwrap()["kind"], "invalid_data");
}

#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64"
    )
))]
#[test]
fn linux_mmc_reader_rejects_regular_and_character_files_before_ioctl() {
    let root = Root::new("nondevice");
    let path = root.0.join("sys/class/block/mmcblk0/device/type");
    assert_eq!(
        deviceinfo::read_mmc_health(&path).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        deviceinfo::read_mmc_health(std::path::Path::new("/dev/null"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[cfg(not(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64"
    )
)))]
#[test]
fn other_platforms_return_unsupported_without_opening_a_path() {
    assert_eq!(
        deviceinfo::read_mmc_health(std::path::Path::new("/does-not-exist"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
}
