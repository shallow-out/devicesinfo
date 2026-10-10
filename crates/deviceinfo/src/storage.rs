//! Linux block inventory and mount relationships. Does not open block devices,
//! run commands or query SMART; filesystem space remains an explicit host sample.
use crate::resolve_path_in_root;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub const BLOCK_DIR: &str = "sys/class/block";
pub const SHARED_INPUTS: [&str; 1] = ["proc/self/mountinfo"];
pub const DEVICE_INPUTS: [&str; 18] = [
    "dev",
    "size",
    "partition",
    "removable",
    "ro",
    "queue/rotational",
    "queue/logical_block_size",
    "queue/physical_block_size",
    "device/model",
    "device/name",
    "device/vendor",
    "device/serial",
    "device/rev",
    "device/fwrev",
    "device/type",
    "device/transport",
    "dm/name",
    "md/level",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageInterface {
    Nvme,
    Mmc,
    Sd,
    Scsi,
    Virtio,
    Virtual,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDevice {
    pub name: String,
    pub path: PathBuf,
    pub device_number: Option<String>,
    pub partition_number: Option<u64>,
    pub parent: Option<String>,
    /// Layers such as dm-crypt/LVM/RAID reference other block devices here.
    pub backing_devices: Vec<String>,
    pub interface: StorageInterface,
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub total_bytes: Option<u64>,
    pub logical_block_bytes: Option<u64>,
    pub physical_block_bytes: Option<u64>,
    pub removable: Option<bool>,
    pub read_only: Option<bool>,
    pub rotational: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageMount {
    pub mount_id: u64,
    pub parent_mount_id: u64,
    pub device_number: String,
    /// Resolved by major:minor, never by guessing /dev/root or mapper aliases.
    pub block_device: Option<String>,
    pub root: PathBuf,
    pub mount_point: PathBuf,
    pub filesystem: String,
    pub source: String,
    pub options: Vec<String>,
    pub super_options: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageReport {
    pub devices: Vec<BlockDevice>,
    /// All namespace mounts, including bind, network and pseudo filesystems.
    /// Do not sum partitions, layered devices and bind mounts as independent disks.
    pub mounts: Vec<StorageMount>,
    pub warnings: Vec<String>,
}

/// A shared capture plan. Dynamic directories preserve partitions and slave names.
pub fn input_dirs(names: &[String]) -> Vec<String> {
    names
        .iter()
        .flat_map(|name| {
            [
                format!("{BLOCK_DIR}/{name}"),
                format!("{BLOCK_DIR}/{name}/slaves"),
            ]
        })
        .collect()
}
pub fn inputs(names: &[String], list: &impl Fn(&str) -> Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut files: Vec<_> = SHARED_INPUTS.iter().map(|s| s.to_string()).collect();
    let mut existence = Vec::new();
    for name in names {
        let base = format!("{BLOCK_DIR}/{name}");
        files.extend(DEVICE_INPUTS.iter().map(|leaf| format!("{base}/{leaf}")));
        for child in list(&base).iter().filter(|child| names.contains(child)) {
            files.push(format!("{base}/{child}/partition"));
        }
        // Each name is enough to preserve a device-mapper/RAID edge.
        for slave in list(&format!("{base}/slaves")) {
            existence.push(format!("{base}/slaves/{slave}"));
        }
    }
    (files, existence)
}

pub(crate) fn probe_storage(root: &Path) -> StorageReport {
    let mut report = StorageReport::default();
    let names = list(root, BLOCK_DIR, &mut report.warnings);
    let mut parents = BTreeMap::new();
    for name in &names {
        let base = format!("{BLOCK_DIR}/{name}");
        for child in list(root, &base, &mut report.warnings) {
            if names.contains(&child) && read(root, &format!("{base}/{child}/partition")).is_some()
            {
                parents.insert(child, name.clone());
            }
        }
    }
    for name in &names {
        let base = format!("{BLOCK_DIR}/{name}");
        let attr = |leaf: &str| read(root, &format!("{base}/{leaf}"));
        let number = attr("dev").filter(|s| valid_device_number(s));
        if number.is_none() {
            report
                .warnings
                .push(format!("Unknown block device number: /{base}/dev"));
        }
        let total_bytes = attr("size")
            .and_then(|s| s.parse::<u64>().ok())
            .and_then(|s| s.checked_mul(512));
        if total_bytes.is_none() {
            report
                .warnings
                .push(format!("Unknown block capacity: /{base}/size"));
        }
        let partition_number = attr("partition").and_then(|s| s.parse::<u64>().ok());
        let parent = if partition_number.is_some() {
            parents.get(name).cloned().or_else(|| {
                let path = resolve_path_in_root(root, &base).ok()?;
                let candidate = path.parent()?.file_name()?.to_str()?.to_owned();
                names.contains(&candidate).then_some(candidate)
            })
        } else {
            None
        };
        if partition_number.is_some() && parent.is_none() {
            report
                .warnings
                .push(format!("Unknown parent of partition {name}"));
        }
        let subsystem = read_link_name(root, &format!("{base}/device/subsystem"));
        let interface = match attr("device/type").as_deref() {
            Some("MMC") => StorageInterface::Mmc,
            Some("SD") => StorageInterface::Sd,
            _ if attr("device/transport").is_some() && name.starts_with("nvme") => {
                StorageInterface::Nvme
            }
            _ if attr("dm/name").is_some() || attr("md/level").is_some() => {
                StorageInterface::Virtual
            }
            _ => match subsystem.as_deref() {
                Some("nvme") => StorageInterface::Nvme,
                Some("scsi") => StorageInterface::Scsi,
                Some("virtio") => StorageInterface::Virtio,
                _ => StorageInterface::Unknown,
            },
        };
        let boolean = |leaf: &str| match attr(leaf).as_deref() {
            Some("0") => Some(false),
            Some("1") => Some(true),
            _ => None,
        };
        report.devices.push(BlockDevice {
            name: name.clone(),
            path: format!("/dev/{name}").into(),
            device_number: number,
            partition_number,
            parent,
            backing_devices: list_optional(root, &format!("{base}/slaves"), &mut report.warnings),
            interface,
            model: attr("device/model").or_else(|| attr("device/name")),
            vendor: attr("device/vendor"),
            serial: attr("device/serial"),
            firmware: attr("device/fwrev").or_else(|| attr("device/rev")),
            total_bytes,
            logical_block_bytes: attr("queue/logical_block_size").and_then(|s| s.parse().ok()),
            physical_block_bytes: attr("queue/physical_block_size").and_then(|s| s.parse().ok()),
            removable: boolean("removable"),
            read_only: boolean("ro"),
            rotational: boolean("queue/rotational"),
        });
    }
    // Partitions inherit disk attributes only through the observed parent edge.
    let disks: BTreeMap<_, _> = report
        .devices
        .iter()
        .filter(|d| d.partition_number.is_none())
        .map(|d| (d.name.clone(), d.clone()))
        .collect();
    for device in &mut report.devices {
        if let Some(parent) = device.parent.as_ref().and_then(|p| disks.get(p)) {
            device.removable = device.removable.or(parent.removable);
            device.rotational = device.rotational.or(parent.rotational);
            device.logical_block_bytes = device.logical_block_bytes.or(parent.logical_block_bytes);
            device.physical_block_bytes =
                device.physical_block_bytes.or(parent.physical_block_bytes);
            if device.interface == StorageInterface::Unknown {
                device.interface = parent.interface;
            }
        }
    }
    let by_number: BTreeMap<_, _> = report
        .devices
        .iter()
        .filter_map(|d| Some((d.device_number.as_deref()?, d.name.as_str())))
        .collect();
    match resolve_path_in_root(root, SHARED_INPUTS[0]).and_then(fs::read_to_string) {
        Ok(text) => {
            let mut seen = BTreeSet::new();
            for (line_number, line) in text.lines().enumerate() {
                match parse_mount(line, &by_number) {
                    Some(mount) if seen.insert(mount.mount_id) => report.mounts.push(mount),
                    _ => report.warnings.push(format!(
                        "Invalid/duplicate mountinfo line {}",
                        line_number + 1
                    )),
                }
            }
        }
        Err(error) => report
            .warnings
            .push(format!("Cannot read mountinfo: {error}")),
    }
    report
}

pub(crate) fn read(root: &Path, relative: &str) -> Option<String> {
    let value = resolve_path_in_root(root, relative)
        .and_then(fs::read_to_string)
        .ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}
fn read_link_name(root: &Path, relative: &str) -> Option<String> {
    let path = Path::new(relative);
    let parent = resolve_path_in_root(root, path.parent()?.to_str()?).ok()?;
    fs::read_link(parent.join(path.file_name()?))
        .ok()?
        .file_name()?
        .to_str()
        .map(str::to_owned)
}
fn list_optional(root: &Path, relative: &str, warnings: &mut Vec<String>) -> Vec<String> {
    if resolve_path_in_root(root, relative).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        return Vec::new();
    }
    list(root, relative, warnings)
}
fn list(root: &Path, relative: &str, warnings: &mut Vec<String>) -> Vec<String> {
    let entries = match resolve_path_in_root(root, relative).and_then(fs::read_dir) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(format!("Cannot list /{relative}: {error}"));
            return Vec::new();
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => names.push(entry.file_name().to_string_lossy().into_owned()),
            Err(error) => warnings.push(format!("Cannot list /{relative} entry: {error}")),
        }
    }
    names.sort();
    names
}
fn valid_device_number(text: &str) -> bool {
    text.split_once(':')
        .is_some_and(|(major, minor)| major.parse::<u32>().is_ok() && minor.parse::<u32>().is_ok())
}
fn unescape(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let escape = bytes.get(i + 1..i + 4)?;
            let value = match escape {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => return None,
            };
            out.push(value);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}
fn parse_mount(line: &str, devices: &BTreeMap<&str, &str>) -> Option<StorageMount> {
    let (before, after) = line.split_once(" - ")?;
    let fields: Vec<_> = before.split_whitespace().collect();
    let tail: Vec<_> = after.split_whitespace().collect();
    if fields.len() < 6 || tail.len() != 3 || !valid_device_number(fields[2]) {
        return None;
    }
    let root = unescape(fields[3])?;
    let mount_point = unescape(fields[4])?;
    if !root.starts_with('/') || !mount_point.starts_with('/') {
        return None;
    }
    Some(StorageMount {
        mount_id: fields[0].parse().ok()?,
        parent_mount_id: fields[1].parse().ok()?,
        device_number: fields[2].into(),
        block_device: devices.get(fields[2]).map(|s| s.to_string()),
        root: root.into(),
        mount_point: mount_point.into(),
        filesystem: tail[0].into(),
        source: unescape(tail[1])?,
        options: fields[5].split(',').map(str::to_owned).collect(),
        super_options: tail[2].split(',').map(str::to_owned).collect(),
    })
}
