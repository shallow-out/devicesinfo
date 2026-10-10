//! Explicit read-only eMMC EXT_CSD health fallback. Default sysfs sampling never
//! calls this module. No writes, subprocesses or automatic privilege escalation.
use crate::{StorageHealthState, storage_health::mmc_state};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

/// Pure decoded health indicators; lifetime codes are 10% buckets, not exact wear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmcExtCsdHealth {
    pub revision: u8,
    pub pre_eol_info: Option<u8>,
    pub life_time_a: Option<u8>,
    pub life_time_b: Option<u8>,
    pub state: StorageHealthState,
}

/// Health fields were introduced in EXT_CSD revision 7 (eMMC 5.0). Older cards
/// remain unknown even if reserved bytes contain non-zero data. No device access.
pub fn decode_mmc_ext_csd(bytes: &[u8]) -> io::Result<MmcExtCsdHealth> {
    if bytes.len() != 512 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MMC EXT_CSD must contain exactly 512 bytes",
        ));
    }
    let revision = bytes[192];
    if revision < 7 {
        return Ok(MmcExtCsdHealth {
            revision,
            pre_eol_info: None,
            life_time_a: None,
            life_time_b: None,
            state: StorageHealthState::Unknown,
        });
    }
    if bytes[267] > 3 || bytes[268] > 11 || bytes[269] > 11 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MMC EXT_CSD contains invalid health codes",
        ));
    }
    let pre = (bytes[267] != 0).then_some(bytes[267]);
    let a = (bytes[268] != 0).then_some(bytes[268]);
    let b = (bytes[269] != 0).then_some(bytes[269]);
    Ok(MmcExtCsdHealth {
        revision,
        pre_eol_info: pre,
        life_time_a: a,
        life_time_b: b,
        state: mmc_state(pre, a, b, false),
    })
}

/// Open a host eMMC whole-disk node, verify its kernel identity, and read CMD8.
/// This is a separate opt-in API, never called from a fixture/root probe.
/// Unsupported platforms and non-MMC/partition nodes return explicit errors.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64"
    )
))]
pub fn read_mmc_health(path: &Path) -> io::Result<MmcExtCsdHealth> {
    use std::{
        fs,
        os::unix::fs::{FileTypeExt, MetadataExt},
    };
    let fd = fs::File::open(path)?;
    let metadata = fd.metadata()?;
    if !metadata.file_type().is_block_device() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MMC health requires a block device",
        ));
    }
    let dev = metadata.rdev();
    let major = libc::major(dev);
    let minor = libc::minor(dev);
    let sysfs = std::path::PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
    if major != 179 || sysfs.join("partition").try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "MMC health requires a whole eMMC disk",
        ));
    }
    if fs::read_to_string(sysfs.join("device/type"))?.trim() != "MMC" {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "EXT_CSD health is unavailable for SD/SDIO",
        ));
    }
    let name = fs::canonicalize(&sysfs)?
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    if !name
        .strip_prefix("mmcblk")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "MMC boot/RPMB nodes are not health targets",
        ));
    }
    read_ext_csd(&fd).and_then(|bytes| decode_mmc_ext_csd(&bytes))
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
fn read_ext_csd(fd: &std::fs::File) -> io::Result<[u8; 512]> {
    use std::os::fd::AsRawFd;
    // Matches Linux UAPI mmc_ioc_cmd, including explicit padding before data_ptr.
    #[repr(C)]
    struct Command {
        write_flag: libc::c_int,
        is_acmd: libc::c_int,
        opcode: u32,
        arg: u32,
        response: [u32; 4],
        flags: u32,
        blksz: u32,
        blocks: u32,
        postsleep_min_us: u32,
        postsleep_max_us: u32,
        data_timeout_ns: u32,
        cmd_timeout_ms: u32,
        pad: u32,
        data_ptr: u64,
    }
    const _: () = {
        assert!(std::mem::size_of::<Command>() == 72);
        assert!(std::mem::offset_of!(Command, data_ptr) == 64);
    };
    let mut bytes = [0u8; 512];
    let mut command = Command {
        write_flag: 0,
        is_acmd: 0,
        opcode: 8,
        arg: 0,
        response: [0; 4],
        flags: 0x15 | (1 << 5),
        blksz: 512,
        blocks: 1,
        postsleep_min_us: 0,
        postsleep_max_us: 0,
        data_timeout_ns: 2_000_000_000,
        cmd_timeout_ms: 5000,
        pad: 0,
        data_ptr: bytes.as_mut_ptr() as u64,
    };
    // SAFETY: the checked C ABI has a live pointer to the 512-byte receive buffer.
    // This synchronous ioctl uses only CMD8 SEND_EXT_CSD with write_flag=0.
    let status = unsafe { libc::ioctl(fd.as_raw_fd(), 0xc048_b300 as libc::c_ulong, &mut command) };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    if status != 0 {
        return Err(io::Error::other(format!(
            "Unexpected MMC ioctl status {status}"
        )));
    }
    // Transport success does not prove card success. Check R1 error status bits
    // before interpreting data; retain the status in the diagnostic.
    if command.response[0] & R1_READ_ERRORS != 0 {
        return Err(io::Error::other(format!(
            "MMC R1 error status: {:#x}",
            command.response[0]
        )));
    }
    Ok(bytes)
}

// Native R1 error bits defined in include/linux/mmc/mmc.h. State/ready bits
// must not be treated as errors (e.g. the TRAN state plus READY_FOR_DATA).
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64"
    )
))]
const R1_READ_ERRORS: u32 = (1 << 31)
    | (1 << 30)
    | (1 << 29)
    | (1 << 25)
    | (1 << 24)
    | (1 << 23)
    | (1 << 22)
    | (1 << 21)
    | (1 << 20)
    | (1 << 19)
    | (1 << 18)
    | (1 << 17);

#[cfg(not(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64"
    )
)))]
pub fn read_mmc_health(_path: &Path) -> io::Result<MmcExtCsdHealth> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "MMC health requires a supported Linux ioctl target",
    ))
}
