//! Synthetic ABI contracts, not physical Q8B or storage-health certification.
use deviceinfo::{
    StorageHealthOptions, StorageHealthState, StorageInterface, decode_nvme_smart_log, inspect_soc,
    inspect_storage, observe_storage_health,
};
use std::{fs, path::PathBuf};

struct Root(PathBuf);
impl Root {
    fn new(name: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "deviceinfo-platform-{name}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn link(&self, target: &str, path: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, path).unwrap();
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn soc_uses_soc_evidence_and_retains_unknown_or_conflicting_vendors() {
    let root = Root::new("soc");
    root.write(
        "sys/firmware/devicetree/base/compatible",
        "radxa,dragon-q8b\0qcom,qcs8550\0",
    );
    root.write("proc/cpuinfo", "CPU implementer : 0x41\n");
    let soc = inspect_soc(&root.0).data;
    assert_eq!(soc.vendor.as_deref(), Some("Qualcomm"));
    assert_eq!(soc.model.as_deref(), Some("qcs8550"));
    assert_eq!(
        soc.vendor_source,
        Some("/sys/firmware/devicetree/base/compatible".into())
    );
    root.write("sys/devices/soc0/family", "Rockchip\n");
    let conflict = inspect_soc(&root.0).data;
    assert_eq!(conflict.vendor, None);
    assert_eq!(conflict.vendor_source, None);
    assert_eq!(conflict.warnings.len(), 1);
    root.write(
        "sys/firmware/devicetree/base/compatible",
        "rockchip,unknown-board\0radxa,rock-5b\0",
    );
    fs::remove_dir_all(root.0.join("sys/devices/soc0")).unwrap();
    assert_eq!(inspect_soc(&root.0).data.vendor, None);
    assert_eq!(inspect_soc(&root.0).data.model, None);
}

#[test]
fn soc_bus_absolute_aliases_stay_in_fixture_root_and_do_not_duplicate() {
    let root = Root::new("soc-alias");
    root.write("sys/devices/soc0/family", "Qualcomm\n");
    root.write("sys/devices/soc0/machine", "SM8550\n");
    root.write("sys/devices/soc0/soc_id", "292\n");
    root.link("/sys/devices/soc0", "sys/bus/soc/devices/soc0");
    let soc = inspect_soc(&root.0).data;
    assert_eq!(soc.devices.len(), 1);
    assert_eq!(soc.vendor.as_deref(), Some("Qualcomm"));
    assert_eq!(soc.model.as_deref(), Some("SM8550"));
    assert_eq!(soc.devices[0].soc_id.as_deref(), Some("292"));
    assert!(soc.warnings.is_empty());
}

#[test]
fn common_sbc_soc_names_use_chip_not_board_manufacturer() {
    for (compatible, vendor, model) in [
        ("radxa,rock-5b\0rockchip,rk3588\0", "Rockchip", "rk3588"),
        ("radxa,orion-o6\0cix,sky1\0", "CIX", "sky1"),
        (
            "vendor,board\0allwinner,sun50i-h616\0",
            "Allwinner",
            "sun50i-h616",
        ),
        ("vendor,board\0spacemit,k1\0", "SpacemiT", "k1"),
    ] {
        let root = Root::new(model);
        root.write("sys/firmware/devicetree/base/compatible", compatible);
        let soc = inspect_soc(&root.0).data;
        assert_eq!(soc.vendor.as_deref(), Some(vendor));
        assert_eq!(soc.model.as_deref(), Some(model));
    }
}

fn disk(root: &Root, name: &str, number: &str, sectors: &str) {
    root.write(&format!("sys/class/block/{name}/dev"), number);
    root.write(&format!("sys/class/block/{name}/size"), sectors);
}

#[test]
fn storage_links_partitions_and_mapper_mounts_by_device_number() {
    let root = Root::new("storage");
    disk(&root, "nvme0n1", "259:0", "4096");
    root.write("sys/class/block/nvme0n1/queue/logical_block_size", "4096");
    root.write("sys/class/block/nvme0n1/removable", "1");
    root.write("sys/class/block/nvme0n1/device/model", "FIXTURE SSD");
    root.write("sys/class/block/nvme0n1/device/transport", "pcie");
    disk(&root, "nvme0n1p2", "259:2", "2048");
    root.write("sys/class/block/nvme0n1p2/partition", "2");
    root.write("sys/class/block/nvme0n1/nvme0n1p2/partition", "2");
    disk(&root, "dm-0", "253:0", "2048");
    root.write("sys/class/block/dm-0/dm/name", "cryptroot");
    fs::create_dir_all(root.0.join("sys/class/block/dm-0/slaves/nvme0n1p2")).unwrap();
    root.write("proc/self/mountinfo", "1 1 253:0 / / rw shared:1 - ext4 /dev/mapper/cryptroot rw\n2 1 259:2 / /boot rw - vfat /dev/root rw\n3 1 259:2 /docs /mnt/My\\040Disk rw - ext4 /dev/root rw\n4 1 0:45 / /proc rw - proc proc rw\n");
    let report = inspect_storage(&root.0).data;
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let nvme = report.devices.iter().find(|d| d.name == "nvme0n1").unwrap();
    assert_eq!(nvme.total_bytes, Some(4096 * 512)); // size is always 512-byte sectors, not logical sectors.
    assert_eq!(nvme.logical_block_bytes, Some(4096));
    let partition = report
        .devices
        .iter()
        .find(|d| d.name == "nvme0n1p2")
        .unwrap();
    assert_eq!(partition.parent.as_deref(), Some("nvme0n1"));
    assert_eq!(partition.removable, Some(true));
    assert_eq!(partition.interface, StorageInterface::Nvme);
    assert_eq!(report.devices[0].backing_devices, ["nvme0n1p2"]);
    assert_eq!(report.mounts[0].block_device.as_deref(), Some("dm-0"));
    assert_eq!(report.mounts[1].block_device.as_deref(), Some("nvme0n1p2"));
    assert_eq!(report.mounts[2].mount_point, PathBuf::from("/mnt/My Disk"));
    assert_eq!(report.mounts[3].block_device, None);
    assert_eq!(report.mounts.len(), 4); // Bind mounts are distinct observations, not extra disks.
}

#[test]
fn malformed_storage_does_not_invent_capacity_or_mount_associations() {
    let root = Root::new("bad-storage");
    disk(&root, "sda", "not-a-number", &u64::MAX.to_string());
    root.write("sys/class/block/sda/removable", "maybe");
    root.write("proc/self/mountinfo", "1 1 8:0 / /mnt\\999bad rw - ext4 /dev/sda rw\n2 1 8:0 / / rw - ext4 /dev/sda rw\n2 1 8:0 / / rw - ext4 /dev/sda rw\n");
    let report = inspect_storage(&root.0).data;
    assert_eq!(report.devices[0].total_bytes, None);
    assert_eq!(report.devices[0].removable, None);
    assert_eq!(report.devices[0].device_number, None);
    assert_eq!(report.mounts.len(), 1);
    assert_eq!(report.mounts[0].block_device, None);
    assert_eq!(report.warnings.len(), 4);
    serde_json::to_value(report).unwrap();
}

#[test]
fn absolute_block_symlinks_and_scsi_subsystem_are_root_relative() {
    let root = Root::new("block-links");
    root.write("sys/devices/fixture/block/sda/dev", "8:0");
    root.write("sys/devices/fixture/block/sda/size", "1000");
    root.write("sys/devices/fixture/block/sda/removable", "0");
    root.write("sys/devices/fixture/block/sda/sda1/dev", "8:1");
    root.write("sys/devices/fixture/block/sda/sda1/size", "500");
    root.write("sys/devices/fixture/block/sda/sda1/partition", "1");
    root.link(
        "/sys/bus/scsi",
        "sys/devices/fixture/block/sda/device/subsystem",
    );
    root.link("/sys/devices/fixture/block/sda", "sys/class/block/sda");
    root.link(
        "/sys/devices/fixture/block/sda/sda1",
        "sys/class/block/sda1",
    );
    root.write(
        "proc/self/mountinfo",
        "1 1 8:1 / / rw - ext4 /dev/root rw\n",
    );
    let report = inspect_storage(&root.0).data;
    assert_eq!(report.devices[1].parent.as_deref(), Some("sda"));
    assert_eq!(report.devices[1].removable, Some(false));
    assert_eq!(report.devices[1].interface, StorageInterface::Scsi);
}

#[test]
fn mmc_health_preserves_lifetime_buckets_and_unknown_data() {
    let root = Root::new("mmc");
    disk(&root, "mmcblk0", "179:0", "1000");
    root.write("sys/class/block/mmcblk0/device/type", "MMC");
    root.write("sys/class/block/mmcblk0/device/pre_eol_info", "0x02");
    root.write("sys/class/block/mmcblk0/device/life_time", "0x0a 0x01");
    disk(&root, "mmcblk1", "179:8", "1000");
    root.write("sys/class/block/mmcblk1/device/type", "SD");
    let options = StorageHealthOptions::default();
    let health = observe_storage_health(&root.0, &options).data;
    assert_eq!(health.mmc.len(), 1);
    assert_eq!(health.mmc[0].state, StorageHealthState::Warning);
    assert_eq!(health.mmc[0].life_time_a, Some(10));
    root.write("sys/class/block/mmcblk0/device/life_time", "0x0b 0x01");
    assert_eq!(
        observe_storage_health(&root.0, &options).data.mmc[0].state,
        StorageHealthState::Critical
    );
    root.write("sys/class/block/mmcblk0/device/pre_eol_info", "0x00");
    root.write("sys/class/block/mmcblk0/device/life_time", "0x00 0x00");
    let unknown = observe_storage_health(&root.0, &options).data;
    assert_eq!(unknown.mmc[0].state, StorageHealthState::Unknown);
    assert!(!unknown.warnings.is_empty());
}

#[test]
fn smart_decode_preserves_u128_unknown_bits_and_temperature_units() {
    let mut bytes = [0; 512];
    bytes[3] = 100;
    bytes[4] = 10;
    bytes[1..3].copy_from_slice(&300u16.to_le_bytes());
    bytes[32..48].copy_from_slice(&u128::MAX.to_le_bytes());
    let log = decode_nvme_smart_log(&bytes).unwrap();
    assert_eq!(log.temperature_kelvin, Some(300));
    assert_eq!(log.state, StorageHealthState::Normal);
    assert_eq!(log.data_units_read, u128::MAX.to_string());
    assert_eq!(
        serde_json::to_value(&log).unwrap()["data_units_read"],
        u128::MAX.to_string()
    );
    bytes[5] = 110;
    assert_eq!(
        decode_nvme_smart_log(&bytes).unwrap().state,
        StorageHealthState::Warning
    );
    bytes[0] = 0x80;
    assert_eq!(
        decode_nvme_smart_log(&bytes).unwrap().state,
        StorageHealthState::Critical
    );
    assert!(decode_nvme_smart_log(&bytes[..511]).is_err());
}

#[test]
fn fixture_health_never_opens_real_nvme_controllers() {
    let root = Root::new("no-ioctl");
    fs::create_dir_all(root.0.join("sys/class/block")).unwrap();
    let report = observe_storage_health(
        &root.0,
        &StorageHealthOptions {
            nvme_controllers: vec!["/dev/nvme0".into()],
        },
    )
    .data;
    assert_eq!(report.nvme[0].state, StorageHealthState::Unknown);
    assert!(report.nvme[0].smart.is_none());
    assert!(
        report.nvme[0]
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("fixture roots")
    );
}

#[test]
fn health_errors_preserve_permission_errno_and_controller_status() {
    use deviceinfo::{NvmeHealthError, StorageHealthError, StorageHealthErrorKind};
    let denied: StorageHealthError =
        NvmeHealthError::Io(std::io::Error::from_raw_os_error(libc::EACCES)).into();
    assert_eq!(denied.kind, StorageHealthErrorKind::PermissionDenied);
    assert_eq!(denied.code, Some(libc::EACCES));
    let status: StorageHealthError = NvmeHealthError::CompletionStatus(7).into();
    assert_eq!(status.kind, StorageHealthErrorKind::NvmeStatus);
    assert_eq!(status.code, Some(7));
    let unsupported: StorageHealthError = NvmeHealthError::Unsupported("fixture").into();
    assert_eq!(unsupported.kind, StorageHealthErrorKind::Unsupported);
    assert_eq!(unsupported.code, None);
    assert_eq!(
        serde_json::to_value(&denied).unwrap()["kind"],
        "permission_denied"
    );
}
