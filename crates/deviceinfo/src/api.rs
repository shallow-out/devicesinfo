//! The only public probing/sampling API, with a shared v2 envelope.
use crate::*;
use std::path::{Path, PathBuf};

fn collect<T>(
    root: &Path,
    classes: &[&str],
    read: impl FnOnce() -> T,
    sources: impl FnOnce(&T) -> Vec<(PathBuf, PathBuf)>,
) -> Snapshot<T> {
    let before = SampleContext::read(root);
    // Record sysfs instances before reading counters to catch removal/replacement.
    let previous = class_devices(root, classes);
    let data = read();
    let devices: Vec<_> = sources(&data)
        .into_iter()
        .map(|(source, class)| {
            let mut device = crate::snapshot::association(root, &source, &class);
            let prior = previous.iter().find(|old| old.source == class);
            if prior.is_none_or(|old| {
                old.physical_path != device.physical_path
                    || old.kernel_instance != device.kernel_instance
                    || old.driver != device.driver
                    || old.device_number != device.device_number
            }) {
                device.kernel_instance = None;
            }
            device
        })
        .collect();
    let after = SampleContext::read(root);
    let mut context = SampleContext::from_bounds(before.origin, before.started, after.finished);
    context.consistent &= before.consistent && after.consistent;
    context.diagnostics.extend(before.diagnostics);
    context.diagnostics.extend(after.diagnostics);
    context.diagnostics.dedup();
    Snapshot {
        schema_version: SCHEMA_VERSION,
        context,
        devices,
        data,
    }
}

pub(crate) fn class_devices(root: &Path, directories: &[&str]) -> Vec<DeviceAssociation> {
    let mut devices = Vec::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(root.join(directory)) else {
            continue;
        };
        for entry in entries.flatten() {
            let source = Path::new("/").join(directory).join(entry.file_name());
            devices.push(crate::snapshot::association(root, &source, &source));
        }
    }
    devices.sort_by(|a, b| a.source.cmp(&b.source));
    devices
}

pub fn inspect_hardware(root: &Path, arch: &str) -> Snapshot<HardwareReport> {
    collect(
        root,
        &["sys/class/drm", "sys/class/accel"],
        || crate::probe_with(root, arch),
        |report| {
            report
                .accelerators
                .iter()
                .map(|device| (device.source.clone(), device.source.clone()))
                .collect()
        },
    )
}
pub fn inspect_environment(root: &Path) -> Snapshot<EnvironmentReport> {
    collect(root, &[], || crate::probe_environment(root), |_| Vec::new())
}
pub fn inspect_system(root: &Path, arch: &str) -> Snapshot<SystemReport> {
    collect(
        root,
        &["sys/bus/soc/devices"],
        || crate::system::probe_system_with(root, arch),
        |report| {
            report
                .soc
                .devices
                .iter()
                .map(|device| (device.source.clone(), device.source.clone()))
                .collect()
        },
    )
}
pub fn inspect_platform(root: &Path) -> Snapshot<PlatformReport> {
    collect(
        root,
        &[],
        || crate::platform::probe_platform(root),
        |_| Vec::new(),
    )
}
pub fn inspect_soc(root: &Path) -> Snapshot<SocReport> {
    collect(
        root,
        &["sys/bus/soc/devices"],
        || crate::soc::probe_soc(root),
        |report| {
            report
                .devices
                .iter()
                .map(|device| (device.source.clone(), device.source.clone()))
                .collect()
        },
    )
}
pub fn inspect_storage(root: &Path) -> Snapshot<StorageReport> {
    collect(
        root,
        &["sys/class/block"],
        || crate::storage::probe_storage(root),
        |report| {
            report
                .devices
                .iter()
                .map(|device| {
                    let class = Path::new("/sys/class/block").join(&device.name);
                    (class.clone(), class)
                })
                .collect()
        },
    )
}
pub fn observe_system(root: &Path, options: &SystemSampleOptions) -> Snapshot<SystemState> {
    let local_options = SystemSampleOptions {
        watch: if root == Path::new("/") {
            options.watch.clone()
        } else {
            Vec::new()
        },
    };
    let mut snapshot = collect(
        root,
        &[],
        || crate::system::sample_system_state_with(root, &local_options),
        |_| Vec::new(),
    );
    filesystem_context(root, &options.watch, &mut snapshot);
    snapshot
}
pub fn observe_accelerators(root: &Path, options: &SampleOptions) -> Snapshot<RuntimeState> {
    let local_options = SampleOptions {
        watch: if root == Path::new("/") {
            options.watch.clone()
        } else {
            Vec::new()
        },
        counters: options.counters,
    };
    let mut snapshot = collect(
        root,
        &["sys/class/drm", "sys/class/accel"],
        || crate::state::sample(root, &local_options),
        |report| {
            report
                .accelerators
                .iter()
                .map(|device| (device.source.clone(), device.source.clone()))
                .collect()
        },
    );
    filesystem_context(root, &options.watch, &mut snapshot);
    snapshot
}

fn filesystem_context<T>(root: &Path, paths: &[PathBuf], snapshot: &mut Snapshot<T>) {
    for path in paths {
        if root != Path::new("/") {
            snapshot.context.diagnostics.push(Diagnostic { code: DiagnosticCode::Unsupported, device: Some("filesystem".into()),
                path: path.clone(), operation: DiagnosticOperation::Inspect, errno: None,
                message: "Filesystem observation is disabled for injected roots; host data cannot be attributed to the source".into() });
            continue;
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(metadata) = std::fs::metadata(path) {
                let number = format!(
                    "{}:{}",
                    libc::major(metadata.dev()),
                    libc::minor(metadata.dev())
                );
                let physical_path =
                    crate::resolve_path_in_root(root, &format!("sys/dev/block/{number}")).ok();
                snapshot.devices.push(DeviceAssociation {
                    source: path.clone(),
                    physical_path,
                    kernel_instance: None,
                    driver: None,
                    device_number: Some(number),
                });
            }
        }
    }
}
pub fn observe_thermal(root: &Path, options: &ThermalOptions) -> Snapshot<ThermalReport> {
    collect(
        root,
        &["sys/class/thermal", "sys/class/hwmon"],
        || crate::thermal::sample_thermal_with(root, options),
        |report| {
            report
                .temperatures
                .iter()
                .map(|sensor| sensor.source.clone())
                .chain(report.fans.iter().map(|sensor| sensor.source.clone()))
                .chain(report.pwm.iter().map(|sensor| sensor.source.clone()))
                .chain(
                    report
                        .cooling_devices
                        .iter()
                        .map(|sensor| sensor.source.clone()),
                )
                .map(|source| {
                    let class = if source
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("cooling_device"))
                    {
                        source.clone()
                    } else {
                        source.parent().unwrap_or(&source).to_path_buf()
                    };
                    (source, class)
                })
                .collect()
        },
    )
}
pub fn observe_storage_health(
    root: &Path,
    options: &StorageHealthOptions,
) -> Snapshot<StorageHealthReport> {
    collect(
        root,
        &["sys/class/block", "sys/class/nvme"],
        || crate::storage_health::sample_storage_health_with(root, options),
        |report| {
            report
                .mmc
                .iter()
                .map(|device| Path::new("/sys/class/block").join(&device.device))
                .chain(report.nvme.iter().filter_map(|device| {
                    device
                        .path
                        .file_name()
                        .map(|name| Path::new("/sys/class/nvme").join(name))
                }))
                .map(|source| (source.clone(), source))
                .collect()
        },
    )
}
/// Explicit command/network checks; the runner must operate in the given root's source environment.
pub fn check_environment(
    root: &Path,
    environment: &EnvironmentReport,
    options: &LiveOptions,
    run: crate::live::Runner<'_>,
) -> Snapshot<LiveReport> {
    collect(
        root,
        &[],
        || crate::live::probe(environment, options, run),
        |_| Vec::new(),
    )
}

/// Inventory of kernel device links for collectors. No metric or command is read.
pub fn inspect_device_links(root: &Path) -> Snapshot<()> {
    collect(
        root,
        &[
            "sys/class/drm",
            "sys/class/accel",
            "sys/class/hwmon",
            "sys/class/thermal",
            "sys/class/block",
            "sys/class/nvme",
            "sys/bus/soc/devices",
        ],
        || (),
        |_| {
            class_devices(
                root,
                &[
                    "sys/class/drm",
                    "sys/class/accel",
                    "sys/class/hwmon",
                    "sys/class/thermal",
                    "sys/class/block",
                    "sys/class/nvme",
                    "sys/bus/soc/devices",
                ],
            )
            .into_iter()
            .map(|device| (device.source.clone(), device.source))
            .collect()
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MmcHealthObservation {
    pub path: PathBuf,
    pub health: Option<MmcExtCsdHealth>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Explicit native MMC health read. Injected roots never open host device nodes.
pub fn observe_mmc_health(root: &Path, path: &Path) -> Snapshot<MmcHealthObservation> {
    collect(
        root,
        &["sys/class/block"],
        || {
            let result = if root == Path::new("/") {
                crate::mmc_health::read_mmc_health(path)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "MMC device access is disabled for injected roots",
                ))
            };
            let (health, diagnostics) = match result {
                Ok(health) => (Some(health), Vec::new()),
                Err(error) => (
                    None,
                    vec![Diagnostic::from_io(
                        path.to_str(),
                        path,
                        DiagnosticOperation::Read,
                        &error,
                    )],
                ),
            };
            MmcHealthObservation {
                path: path.into(),
                health,
                diagnostics,
            }
        },
        |report| {
            report
                .path
                .file_name()
                .map(|name| {
                    let class = Path::new("/sys/class/block").join(name);
                    vec![(class.clone(), class)]
                })
                .unwrap_or_default()
        },
    )
}
