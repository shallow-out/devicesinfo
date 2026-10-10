//! On-demand storage health observations. MMC uses sysfs; NVMe Get Log Page is
//! explicitly opted in. No helper commands, writes or implicit device opening.
use crate::{
    resolve_path_in_root,
    storage::{BLOCK_DIR, read},
};
use serde::{Deserialize, Serialize};
use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

pub const MMC_INPUTS: [&str; 2] = ["device/pre_eol_info", "device/life_time"];

/// State of the observed indicators, not a guarantee about the entire drive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageHealthState {
    #[default]
    Unknown,
    Normal,
    Warning,
    Critical,
}

/// eMMC lifetime uses 10% buckets. Preserve the raw codes (0 = undefined,
/// 1..=10 = 0..10% through 90..100% consumed, 11 = exceeded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmcHealth {
    pub device: String,
    pub pre_eol_info: Option<u8>,
    pub life_time_a: Option<u8>,
    pub life_time_b: Option<u8>,
    pub state: StorageHealthState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeSmartLog {
    pub critical_warning: u8,
    pub temperature_kelvin: Option<u16>,
    pub available_spare_percent: u8,
    pub available_spare_threshold_percent: u8,
    /// Can exceed 100; it is not a remaining-space percentage.
    pub percentage_used: u8,
    /// NVMe 128-bit counters use decimal strings so JSON preserves all bits.
    pub data_units_read: String,
    pub data_units_written: String,
    pub power_cycles: String,
    pub power_on_hours: String,
    pub unsafe_shutdowns: String,
    pub media_errors: String,
    pub state: StorageHealthState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeHealth {
    pub path: PathBuf,
    pub smart: Option<NvmeSmartLog>,
    pub state: StorageHealthState,
    /// Permission errors, controller status and unsupported platforms stay explicit.
    pub error: Option<StorageHealthError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageHealthErrorKind {
    PermissionDenied,
    Io,
    NvmeStatus,
    Unsupported,
}

/// Locale-neutral classification for consumer UI and authorization handling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageHealthError {
    pub kind: StorageHealthErrorKind,
    pub code: Option<i32>,
    pub message: String,
}

impl From<NvmeHealthError> for StorageHealthError {
    fn from(error: NvmeHealthError) -> Self {
        let (kind, code) = match &error {
            NvmeHealthError::Io(error) => (
                if error.kind() == io::ErrorKind::PermissionDenied {
                    StorageHealthErrorKind::PermissionDenied
                } else {
                    StorageHealthErrorKind::Io
                },
                error.raw_os_error(),
            ),
            NvmeHealthError::CompletionStatus(code) => {
                (StorageHealthErrorKind::NvmeStatus, Some(*code))
            }
            NvmeHealthError::Unsupported(_) => (StorageHealthErrorKind::Unsupported, None),
        };
        Self {
            kind,
            code,
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageHealthOptions {
    /// Explicit host controller nodes, e.g. /dev/nvme0. Empty by default.
    pub nvme_controllers: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageHealthReport {
    pub mmc: Vec<MmcHealth>,
    pub nvme: Vec<NvmeHealth>,
    pub warnings: Vec<String>,
}

pub fn sample_storage_health(options: &StorageHealthOptions) -> StorageHealthReport {
    sample_storage_health_with(Path::new("/"), options)
}

/// Fixture roots never open real device nodes, even with an NVMe opt-in list.
pub fn sample_storage_health_with(
    root: &Path,
    options: &StorageHealthOptions,
) -> StorageHealthReport {
    let mut report = StorageHealthReport::default();
    match resolve_path_in_root(root, BLOCK_DIR).and_then(fs::read_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        report
                            .warnings
                            .push(format!("Cannot list MMC devices: {error}"));
                        continue;
                    }
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let base = format!("{BLOCK_DIR}/{name}");
                if read(root, &format!("{base}/partition")).is_some()
                    || read(root, &format!("{base}/device/type")).as_deref() != Some("MMC")
                {
                    continue;
                }
                // Hardware boot/RPMB nodes are not separate cards.
                if !name
                    .strip_prefix("mmcblk")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                {
                    continue;
                }
                let pre_eol_info = read_code(
                    root,
                    &format!("{base}/{}", MMC_INPUTS[0]),
                    3,
                    &mut report.warnings,
                );
                let values = read(root, &format!("{base}/{}", MMC_INPUTS[1]));
                let parsed = values
                    .as_deref()
                    .and_then(|text| {
                        let parts: Vec<_> = text.split_whitespace().collect();
                        if parts.len() != 2 {
                            return None;
                        }
                        Some((parse_hex(parts[0])?, parse_hex(parts[1])?))
                    })
                    .filter(|(a, b)| *a <= 11 && *b <= 11);
                if values.is_some() && parsed.is_none() {
                    report
                        .warnings
                        .push(format!("Invalid MMC lifetime for {name}"));
                }
                let (a, b) = parsed
                    .map(|(a, b)| ((a != 0).then_some(a), (b != 0).then_some(b)))
                    .unwrap_or_default();
                let state = if pre_eol_info == Some(3) || a == Some(11) || b == Some(11) {
                    StorageHealthState::Critical
                } else if pre_eol_info == Some(2) || a == Some(10) || b == Some(10) {
                    StorageHealthState::Warning
                } else if pre_eol_info == Some(1) || a.is_some() || b.is_some() {
                    StorageHealthState::Normal
                } else {
                    StorageHealthState::Unknown
                };
                if state == StorageHealthState::Unknown {
                    report
                        .warnings
                        .push(format!("MMC health unavailable for {name}"));
                }
                report.mmc.push(MmcHealth {
                    device: name,
                    pre_eol_info,
                    life_time_a: a,
                    life_time_b: b,
                    state,
                });
            }
        }
        Err(error) => report
            .warnings
            .push(format!("Cannot list storage health inputs: {error}")),
    }
    report.mmc.sort_by(|a, b| a.device.cmp(&b.device));
    for path in &options.nvme_controllers {
        let result = if root == Path::new("/") {
            read_nvme_health(path)
        } else {
            Err(NvmeHealthError::Unsupported(
                "fixture roots cannot open host controllers",
            ))
        };
        let (smart, state, error) = match result {
            Ok(smart) => {
                let state = smart.state;
                (Some(smart), state, None)
            }
            Err(error) => (None, StorageHealthState::Unknown, Some(error.into())),
        };
        report.nvme.push(NvmeHealth {
            path: path.clone(),
            smart,
            state,
            error,
        });
    }
    report
}

fn read_code(root: &Path, relative: &str, max: u8, warnings: &mut Vec<String>) -> Option<u8> {
    let text = read(root, relative)?;
    let code = parse_hex(&text).filter(|v| *v <= max);
    if code.is_none() {
        warnings.push(format!("Invalid /{relative}"));
    }
    code.filter(|v| *v != 0)
}
fn parse_hex(text: &str) -> Option<u8> {
    u8::from_str_radix(
        text.strip_prefix("0x")
            .or_else(|| text.strip_prefix("0X"))
            .unwrap_or(text),
        16,
    )
    .ok()
}

pub fn decode_nvme_smart_log(bytes: &[u8]) -> io::Result<NvmeSmartLog> {
    if bytes.len() != 512 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "NVMe SMART log must contain exactly 512 bytes",
        ));
    }
    let counter = |offset| {
        u128::from_le_bytes(
            bytes[offset..offset + 16]
                .try_into()
                .expect("bounded NVMe field"),
        )
        .to_string()
    };
    let temperature = u16::from_le_bytes([bytes[1], bytes[2]]);
    let state = if bytes[0] != 0 {
        StorageHealthState::Critical
    } else if bytes[5] >= 100 || bytes[3] < bytes[4] {
        StorageHealthState::Warning
    } else {
        StorageHealthState::Normal
    };
    Ok(NvmeSmartLog {
        critical_warning: bytes[0],
        temperature_kelvin: (temperature != 0).then_some(temperature),
        available_spare_percent: bytes[3],
        available_spare_threshold_percent: bytes[4],
        percentage_used: bytes[5],
        data_units_read: counter(32),
        data_units_written: counter(48),
        power_cycles: counter(112),
        power_on_hours: counter(128),
        unsafe_shutdowns: counter(144),
        media_errors: counter(160),
        state,
    })
}

#[derive(Debug)]
pub enum NvmeHealthError {
    Io(io::Error),
    CompletionStatus(i32),
    Unsupported(&'static str),
}
impl fmt::Display for NvmeHealthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::CompletionStatus(code) => write!(f, "NVMe completion status: {code:#x}"),
            Self::Unsupported(reason) => write!(f, "NVMe health unsupported: {reason}"),
        }
    }
}
impl std::error::Error for NvmeHealthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// Read-only Get Log Page, 5 second command timeout, no sudo or subprocess.
/// Supported ioctl layouts are explicitly limited to Linux x86/ARM targets.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64"
    )
))]
pub fn read_nvme_health(path: &Path) -> Result<NvmeSmartLog, NvmeHealthError> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct Command {
        opcode: u8,
        flags: u8,
        reserved: u16,
        nsid: u32,
        cdw2: u32,
        cdw3: u32,
        metadata: u64,
        addr: u64,
        metadata_len: u32,
        data_len: u32,
        cdw10: u32,
        cdw11: u32,
        cdw12: u32,
        cdw13: u32,
        cdw14: u32,
        cdw15: u32,
        timeout_ms: u32,
        result: u32,
    }
    const _: () = {
        assert!(std::mem::size_of::<Command>() == 72);
        assert!(std::mem::offset_of!(Command, addr) == 24);
    };
    let fd = fs::File::open(path).map_err(NvmeHealthError::Io)?;
    let mut data = [0; 512];
    let mut command = Command {
        opcode: 0x02,
        flags: 0,
        reserved: 0,
        nsid: u32::MAX,
        cdw2: 0,
        cdw3: 0,
        metadata: 0,
        addr: data.as_mut_ptr() as u64,
        metadata_len: 0,
        data_len: 512,
        cdw10: 0x02 | (127 << 16),
        cdw11: 0,
        cdw12: 0,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
        timeout_ms: 5000,
        result: 0,
    };
    // Linux UAPI NVME_IOCTL_ADMIN_CMD = _IOWR('N', 0x41, nvme_passthru_cmd).
    // SAFETY: fd is live; command follows the checked C ABI; its data pointer
    // addresses a 512-byte buffer valid throughout this synchronous ioctl.
    let status = unsafe { libc::ioctl(fd.as_raw_fd(), 0xc048_4e41 as libc::c_ulong, &mut command) };
    if status < 0 {
        return Err(NvmeHealthError::Io(io::Error::last_os_error()));
    }
    if status > 0 {
        return Err(NvmeHealthError::CompletionStatus(status));
    }
    decode_nvme_smart_log(&data).map_err(NvmeHealthError::Io)
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
pub fn read_nvme_health(_path: &Path) -> Result<NvmeSmartLog, NvmeHealthError> {
    Err(NvmeHealthError::Unsupported(
        "requires a supported Linux ioctl target",
    ))
}
