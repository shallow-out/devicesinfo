//! Read-only SoC identity with explicit evidence. CPU implementer and board vendor
//! are not evidence of the SoC manufacturer.
use crate::resolve_path_in_root;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

pub const SHARED_INPUTS: [&str; 1] = ["sys/firmware/devicetree/base/compatible"];
pub const DEVICE_INPUTS: [&str; 4] = ["machine", "family", "soc_id", "revision"];
pub const INPUT_DIRS: [&str; 2] = ["sys/devices", "sys/bus/soc/devices"];

/// Raw platform-provided attributes. soc_id is not necessarily a model name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocDevice {
    pub source: PathBuf,
    pub machine: Option<String>,
    pub family: Option<String>,
    pub soc_id: Option<String>,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocReport {
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub vendor_source: Option<PathBuf>,
    pub model_source: Option<PathBuf>,
    pub compatible: Vec<String>,
    pub devices: Vec<SocDevice>,
    pub warnings: Vec<String>,
}

/// Shared with capture; only the SoC bus's numeric socN entries are inspected.
pub fn inputs(list: &impl Fn(&str) -> Vec<String>) -> Vec<String> {
    let mut inputs: Vec<_> = SHARED_INPUTS.iter().map(|s| s.to_string()).collect();
    for dir in INPUT_DIRS {
        for name in list(dir).into_iter().filter(|s| is_soc_name(s)) {
            inputs.extend(
                DEVICE_INPUTS
                    .iter()
                    .map(|leaf| format!("{dir}/{name}/{leaf}")),
            );
        }
    }
    inputs
}

fn is_soc_name(name: &str) -> bool {
    name.strip_prefix("soc")
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

pub(crate) fn probe_soc(root: &Path) -> SocReport {
    let mut report = SocReport::default();
    let dt_path = SHARED_INPUTS[0];
    if let Some(raw) = read(root, dt_path) {
        report.compatible = raw
            .split('\0')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
    let mut seen = BTreeSet::new();
    for dir in INPUT_DIRS {
        let path = match resolve_path_in_root(root, dir) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                report
                    .warnings
                    .push(format!("Cannot inspect /{dir}: {error}"));
                continue;
            }
        };
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                report
                    .warnings
                    .push(format!("Cannot inspect /{dir}: {error}"));
                continue;
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if is_soc_name(&name) {
                        names.push(name);
                    }
                }
                Err(error) => report
                    .warnings
                    .push(format!("Cannot inspect /{dir} entry: {error}")),
            }
        }
        names.sort();
        for name in names {
            let relative = format!("{dir}/{name}");
            let Ok(_resolved) = resolve_path_in_root(root, &relative) else {
                report.warnings.push(format!("Cannot resolve /{relative}"));
                continue;
            };
            // The bus and /sys/devices expose aliases of the same numeric socN.
            // Captures flatten the input paths, so deduplicate by kernel name.
            if !seen.insert(name) {
                continue;
            }
            report.devices.push(SocDevice {
                source: format!("/{relative}").into(),
                machine: read(root, &format!("{relative}/machine")),
                family: read(root, &format!("{relative}/family")),
                soc_id: read(root, &format!("{relative}/soc_id")),
                revision: read(root, &format!("{relative}/revision")),
            });
        }
    }
    let mut vendors = BTreeSet::new();
    for device in &report.devices {
        for (leaf, value) in [("family", &device.family), ("machine", &device.machine)] {
            if let Some(vendor) = value.as_deref().and_then(named_vendor) {
                vendors.insert(vendor);
                report
                    .vendor_source
                    .get_or_insert_with(|| device.source.join(leaf));
            }
        }
    }
    // DT lists board-specific to general compatibles. Recognize SoC identifiers,
    // not arbitrary vendor-prefixed board names or unrelated peripheral nodes.
    for value in report.compatible.iter().rev() {
        if let Some((vendor, model)) = soc_compatible(value) {
            vendors.insert(vendor);
            report
                .vendor_source
                .get_or_insert_with(|| format!("/{dt_path}").into());
            if report.model.is_none() {
                report.model = Some(model.into());
                report.model_source = Some(format!("/{dt_path}").into());
            }
        }
    }
    if vendors.len() == 1 {
        report.vendor = vendors.into_iter().next().map(str::to_owned);
    } else if vendors.len() > 1 {
        report.vendor_source = None;
        report
            .warnings
            .push("Conflicting SoC vendor evidence; vendor remains unknown".into());
    }
    if report.model.is_none() {
        if let Some(device) = report.devices.iter().find(|d| d.machine.is_some()) {
            report.model = device.machine.clone();
            report.model_source = Some(device.source.join("machine"));
        }
    }
    report
}

fn read(root: &Path, path: &str) -> Option<String> {
    let text = resolve_path_in_root(root, path)
        .and_then(fs::read_to_string)
        .ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.into())
}

fn named_vendor(value: &str) -> Option<&'static str> {
    let lower = value.to_ascii_lowercase();
    [
        ("rockchip", "Rockchip"),
        ("qualcomm", "Qualcomm"),
        ("allwinner", "Allwinner"),
        ("cix", "CIX"),
        ("amlogic", "Amlogic"),
        ("broadcom", "Broadcom"),
        ("mediatek", "MediaTek"),
        ("nvidia", "NVIDIA"),
        ("nxp", "NXP"),
        ("starfive", "StarFive"),
        ("sophgo", "Sophgo"),
        ("spacemit", "SpacemiT"),
    ]
    .into_iter()
    .find_map(|(prefix, vendor)| {
        let rest = lower.strip_prefix(prefix)?;
        (rest.is_empty() || rest.starts_with([' ', ',', '-', ':'])).then_some(vendor)
    })
}

fn soc_compatible(value: &str) -> Option<(&'static str, &str)> {
    let (namespace, model) = value.split_once(',')?;
    let (vendor, prefixes): (_, &[&str]) = match namespace {
        "rockchip" => ("Rockchip", &["rk", "rv", "px"]),
        "qcom" => (
            "Qualcomm",
            &["sc", "sm", "qcs", "qcm", "sdm", "msm", "apq", "ipq", "sa"],
        ),
        "allwinner" => ("Allwinner", &["sun"]),
        "cix" => ("CIX", &["sky", "p"]),
        "brcm" => ("Broadcom", &["bcm"]),
        "mediatek" => ("MediaTek", &["mt"]),
        "nvidia" => ("NVIDIA", &["tegra"]),
        "fsl" | "nxp" => ("NXP", &["imx", "ls", "lx"]),
        "starfive" => ("StarFive", &["jh"]),
        "sophgo" => ("Sophgo", &["cv", "sg", "bm"]),
        "spacemit" => ("SpacemiT", &["k"]),
        "amlogic" if model.starts_with("meson-") => return Some(("Amlogic", model)),
        _ => return None,
    };
    prefixes
        .iter()
        .any(|p| {
            model
                .strip_prefix(p)
                .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_digit()))
        })
        .then_some((vendor, model))
}
