//! Versioned observations and conservative counter differences.
use crate::{Diagnostic, DiagnosticCode, DiagnosticOperation};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SCHEMA_VERSION: u32 = 2;
pub const CONTEXT_FILE: &str = "deviceinfo-context.json";
pub const BOOT_ID_INPUT: &str = "proc/sys/kernel/random/boot_id";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationOrigin {
    Live,
    Captured,
    Unattributed,
}

/// Wall time is for display. Only boot-relative time is used for differences.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleStamp {
    #[serde(with = "optional_decimal")]
    pub unix_time_ns: Option<u64>,
    #[serde(with = "optional_decimal")]
    pub boot_time_ns: Option<u64>,
    pub boot_clock_resolution_ns: Option<u64>,
    pub boot_id: Option<String>,
    pub time_namespace: Option<String>,
    pub mount_namespace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleContext {
    pub origin: ObservationOrigin,
    pub started: SampleStamp,
    pub finished: SampleStamp,
    /// False for missing identities, reboot, namespace changes, clock reset,
    /// or a capture whose counters were deliberately scrubbed.
    pub consistent: bool,
    pub diagnostics: Vec<Diagnostic>,
}

/// Source context frozen by a collector. Inodes belong to the source kernel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureMetadata {
    pub schema_version: u32,
    pub context: SampleContext,
    pub devices: Vec<DeviceAssociation>,
    /// False when capture deliberately replaced volatile readings with placeholders.
    /// Such fixtures support inventory regression, never counter differences.
    pub counters_preserved: bool,
}

impl SampleContext {
    pub fn from_bounds(
        origin: ObservationOrigin,
        started: SampleStamp,
        finished: SampleStamp,
    ) -> Self {
        let consistent = valid_boot_id(started.boot_id.as_deref())
            && started.boot_id == finished.boot_id
            && valid_namespace(started.time_namespace.as_deref(), "time:")
            && started.time_namespace == finished.time_namespace
            && valid_namespace(started.mount_namespace.as_deref(), "mnt:")
            && started.mount_namespace == finished.mount_namespace
            && started
                .boot_time_ns
                .zip(finished.boot_time_ns)
                .is_some_and(|(a, b)| a <= b)
            && origin != ObservationOrigin::Unattributed;
        Self {
            origin,
            started,
            finished,
            consistent,
            diagnostics: Vec::new(),
        }
    }

    /// Read source identity; injected trees never inherit the collector's clocks.
    pub fn read(root: &Path) -> Self {
        if root == Path::new("/") {
            let stamp = native_stamp(root);
            let mut context = Self::from_bounds(ObservationOrigin::Live, stamp.clone(), stamp);
            let mut reader = crate::diagnostics::Reader::new(root);
            if reader.supported("sample_context", BOOT_ID_INPUT) {
                let boot = reader.text("sample_context", BOOT_ID_INPUT);
                if boot
                    .as_deref()
                    .is_some_and(|value| !valid_boot_id(Some(value)))
                {
                    reader.invalid("sample_context", BOOT_ID_INPUT, "Invalid boot UUID");
                }
            }
            context.diagnostics = reader.diagnostics;
            return context;
        }
        match fs::read(root.join(CONTEXT_FILE)) {
            Ok(bytes) => match serde_json::from_slice::<CaptureMetadata>(&bytes) {
                Ok(metadata) => {
                    let mut context = metadata.context;
                    context.origin = ObservationOrigin::Captured;
                    let checked = Self::from_bounds(
                        context.origin,
                        context.started.clone(),
                        context.finished.clone(),
                    );
                    context.consistent &= checked.consistent;
                    context.consistent &=
                        metadata.schema_version == SCHEMA_VERSION && metadata.counters_preserved;
                    let boot = read_boot_id(root);
                    if boot != context.started.boot_id {
                        context.consistent = false;
                    }
                    context
                }
                Err(error) => {
                    let mut context = unattributed(root);
                    context.diagnostics.push(Diagnostic {
                        code: DiagnosticCode::InvalidData,
                        device: None,
                        path: Path::new("/").join(CONTEXT_FILE),
                        operation: DiagnosticOperation::Decode,
                        errno: None,
                        message: error.to_string(),
                    });
                    context
                }
            },
            Err(error) => {
                let mut context = unattributed(root);
                context.diagnostics.push(Diagnostic::from_io(
                    None,
                    &Path::new("/").join(CONTEXT_FILE),
                    DiagnosticOperation::Read,
                    &error,
                ));
                context
            }
        }
    }

    fn elapsed_since(&self, previous: &Self) -> Result<u64, DifferenceError> {
        if !self.consistent || !previous.consistent {
            return Err(DifferenceError::InvalidContext);
        }
        if !Self::from_bounds(self.origin, self.started.clone(), self.finished.clone()).consistent
            || !Self::from_bounds(
                previous.origin,
                previous.started.clone(),
                previous.finished.clone(),
            )
            .consistent
        {
            return Err(DifferenceError::InvalidContext);
        }
        if self.started.boot_id != previous.finished.boot_id {
            return Err(DifferenceError::DifferentBoot);
        }
        if self.started.time_namespace != previous.finished.time_namespace
            || self.started.mount_namespace != previous.finished.mount_namespace
        {
            return Err(DifferenceError::DifferentNamespace);
        }
        // Windows must not overlap; replaying the same capture cannot create a rate.
        let resolution = previous
            .finished
            .boot_clock_resolution_ns
            .filter(|value| *value > 0)
            .ok_or(DifferenceError::InvalidContext)?;
        if self
            .started
            .boot_clock_resolution_ns
            .filter(|value| *value > 0)
            .is_none()
        {
            return Err(DifferenceError::InvalidContext);
        }
        if self.started.boot_time_ns.unwrap_or(0)
            <= previous
                .finished
                .boot_time_ns
                .unwrap_or(u64::MAX)
                .saturating_add(resolution)
        {
            return Err(DifferenceError::NonIncreasingTime);
        }
        let midpoint = |context: &Self| -> Option<u64> {
            let start = context.started.boot_time_ns?;
            start.checked_add(context.finished.boot_time_ns?.checked_sub(start)? / 2)
        };
        midpoint(self)
            .and_then(|now| now.checked_sub(midpoint(previous)?))
            .filter(|elapsed| *elapsed > 0)
            .ok_or(DifferenceError::NonIncreasingTime)
    }
}

fn valid_boot_id(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value.len() == 36
            && value.bytes().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            })
    })
}
fn valid_namespace(value: Option<&str>, kind: &str) -> bool {
    value
        .and_then(|value| value.strip_prefix(kind))
        .and_then(|value| value.strip_prefix('['))
        .and_then(|value| value.strip_suffix(']'))
        .is_some_and(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
}
fn read_boot_id(root: &Path) -> Option<String> {
    let value = fs::read_to_string(root.join(BOOT_ID_INPUT))
        .ok()?
        .trim()
        .to_ascii_lowercase();
    valid_boot_id(Some(&value)).then_some(value)
}
fn unattributed(root: &Path) -> SampleContext {
    let stamp = SampleStamp {
        boot_id: read_boot_id(root),
        ..Default::default()
    };
    SampleContext::from_bounds(ObservationOrigin::Unattributed, stamp.clone(), stamp)
}
fn native_stamp(root: &Path) -> SampleStamp {
    #[cfg(target_os = "linux")]
    let boot_time_ns = {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // CLOCK_BOOTTIME is non-settable and includes suspend.
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } == 0 {
            u64::try_from(value.tv_sec)
                .ok()
                .and_then(|seconds| seconds.checked_mul(1_000_000_000))
                .and_then(|seconds| seconds.checked_add(u64::try_from(value.tv_nsec).ok()?))
        } else {
            None
        }
    };
    #[cfg(not(target_os = "linux"))]
    let boot_time_ns = None;
    let namespace = |kind| {
        fs::read_link(root.join(format!("proc/self/ns/{kind}")))
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    };
    SampleStamp {
        unix_time_ns: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_nanos()).ok()),
        boot_time_ns,
        boot_clock_resolution_ns: native_resolution(),
        boot_id: read_boot_id(root),
        time_namespace: namespace("time"),
        mount_namespace: namespace("mnt"),
    }
}

fn native_resolution() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_getres(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
            return None;
        }
        u64::try_from(value.tv_sec)
            .ok()?
            .checked_mul(1_000_000_000)?
            .checked_add(u64::try_from(value.tv_nsec).ok()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

// Preserve nanoseconds in JavaScript dashboards; JSON numbers lose precision.
mod optional_decimal {
    use serde::{Deserialize, Deserializer, Serializer, ser::Serialize};
    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        value.map(|value| value.to_string()).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|value| value.parse().map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// Join by source/physical path, never by list index or PCI vendor:product alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAssociation {
    pub source: PathBuf,
    pub physical_path: Option<PathBuf>,
    /// A kernel sysfs object instance scoped to boot_id and namespaces.
    /// Capture/fixture filesystem inodes are never substituted for source inodes.
    pub kernel_instance: Option<String>,
    pub driver: Option<String>,
    pub device_number: Option<String>,
}

pub(crate) fn association(root: &Path, source: &Path, class: &Path) -> DeviceAssociation {
    if root != Path::new("/") {
        if let Some(mut device) = fs::read(root.join(CONTEXT_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CaptureMetadata>(&bytes).ok())
            .filter(|metadata| metadata.schema_version == SCHEMA_VERSION)
            .and_then(|metadata| {
                metadata
                    .devices
                    .into_iter()
                    .find(|device| device.source == class)
            })
        {
            device.source = source.to_path_buf();
            return device;
        }
    }
    let relative = |path: &Path| {
        path.strip_prefix("/")
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    let resolved = crate::resolve_path_in_root(root, &relative(&class.join("device")))
        .or_else(|_| crate::resolve_path_in_root(root, &relative(class)))
        .ok();
    let physical_path = resolved
        .as_ref()
        .and_then(|path| path.strip_prefix(root).ok())
        .map(|path| Path::new("/").join(path));
    let driver = resolved
        .as_ref()
        .and_then(|path| fs::read_link(path.join("driver")).ok())
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
    let device_number = crate::resolve_path_in_root(root, &relative(&class.join("dev")))
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|value| value.trim().to_string());
    #[cfg(unix)]
    let kernel_instance = if cfg!(target_os = "linux") && root == Path::new("/") {
        use std::os::unix::fs::MetadataExt;
        resolved
            .as_ref()
            .and_then(|path| fs::metadata(path).ok())
            .zip(fs::metadata(class).ok())
            .map(|(physical, channel)| {
                format!(
                    "{}:{}:{}:{}",
                    physical.dev(),
                    physical.ino(),
                    channel.dev(),
                    channel.ino()
                )
            })
    } else {
        None
    };
    #[cfg(not(unix))]
    let kernel_instance = None;
    DeviceAssociation {
        source: source.to_path_buf(),
        physical_path,
        kernel_instance,
        driver,
        device_number,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot<T> {
    pub schema_version: u32,
    pub context: SampleContext,
    pub devices: Vec<DeviceAssociation>,
    pub data: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DifferenceError {
    SchemaMismatch,
    InvalidContext,
    DifferentBoot,
    DifferentNamespace,
    NonIncreasingTime,
    UnknownDevice,
    DeviceChanged,
    CounterReset,
    MissingCounter,
    NoCounterProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CounterDifference {
    pub delta: u64,
    pub elapsed_ns: u64,
}
impl CounterDifference {
    pub fn per_second(&self) -> f64 {
        self.delta as f64 * 1_000_000_000.0 / self.elapsed_ns as f64
    }
}
impl<T> Snapshot<T> {
    pub fn elapsed_since(&self, previous: &Self) -> Result<u64, DifferenceError> {
        if self.schema_version != SCHEMA_VERSION || previous.schema_version != SCHEMA_VERSION {
            return Err(DifferenceError::SchemaMismatch);
        }
        self.context.elapsed_since(&previous.context)
    }
    /// Guard arbitrary cumulative device counters, including future ARM/network metrics.
    pub fn counter_since(
        &self,
        previous: &Self,
        source: &Path,
        current: Option<u64>,
        old: Option<u64>,
    ) -> Result<CounterDifference, DifferenceError> {
        let elapsed_ns = self.elapsed_since(previous)?;
        let new_device = self
            .devices
            .iter()
            .find(|device| device.source == source)
            .ok_or(DifferenceError::UnknownDevice)?;
        let old_device = previous
            .devices
            .iter()
            .find(|device| device.source == source)
            .ok_or(DifferenceError::UnknownDevice)?;
        if new_device.kernel_instance.is_none()
            || old_device.kernel_instance.is_none()
            || new_device.physical_path.is_none()
            || old_device.physical_path.is_none()
        {
            return Err(DifferenceError::UnknownDevice);
        }
        if new_device != old_device {
            return Err(DifferenceError::DeviceChanged);
        }
        let delta = current.zip(old).ok_or(DifferenceError::MissingCounter)?;
        let delta = delta
            .0
            .checked_sub(delta.1)
            .ok_or(DifferenceError::CounterReset)?;
        Ok(CounterDifference { delta, elapsed_ns })
    }
}

impl Snapshot<crate::SystemState> {
    pub fn cpu_usage_since(&self, previous: &Self) -> Result<f64, DifferenceError> {
        self.elapsed_since(previous)?;
        let (current, old) = self
            .data
            .cpu
            .zip(previous.data.cpu)
            .ok_or(DifferenceError::MissingCounter)?;
        if current == old {
            return Err(DifferenceError::NoCounterProgress);
        }
        current
            .usage_since(&old)
            .ok_or(DifferenceError::CounterReset)
    }
}

impl Snapshot<crate::RuntimeState> {
    /// NPU busy microseconds over the matching kernel device instance.
    pub fn accelerator_busy_since(
        &self,
        previous: &Self,
        node: &str,
    ) -> Result<CounterDifference, DifferenceError> {
        let current = self
            .data
            .accelerators
            .iter()
            .find(|device| device.node == node)
            .ok_or(DifferenceError::UnknownDevice)?;
        let old = previous
            .data
            .accelerators
            .iter()
            .find(|device| device.node == node)
            .ok_or(DifferenceError::UnknownDevice)?;
        if current.source != old.source {
            return Err(DifferenceError::DeviceChanged);
        }
        self.counter_since(
            previous,
            &current.source,
            current.busy_time_us,
            old.busy_time_us,
        )
    }
}
