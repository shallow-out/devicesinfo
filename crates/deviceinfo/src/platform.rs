//! Board and boot observations. UEFI, DT and ACPI can coexist; none is inferred
//! from architecture, OS name or the absence of a kernel attribute.
use crate::{Diagnostic, diagnostics::Reader};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DMI_DIR: &str = "sys/class/dmi/id";
pub const DT_DIR: &str = "sys/firmware/devicetree/base";
pub const EFI_DIR: &str = "sys/firmware/efi";
pub const ACPI_DIR: &str = "sys/firmware/acpi/tables";
pub const DMI_FIELDS: [&str; 9] = [
    "sys_vendor",
    "product_name",
    "product_version",
    "board_vendor",
    "board_name",
    "board_version",
    "bios_vendor",
    "bios_version",
    "bios_date",
];
pub const INPUT_DIRS: [&str; 4] = [DMI_DIR, DT_DIR, EFI_DIR, ACPI_DIR];
pub fn inputs() -> Vec<String> {
    DMI_FIELDS
        .iter()
        .map(|field| format!("{DMI_DIR}/{field}"))
        .chain([
            format!("{DT_DIR}/model"),
            format!("{DT_DIR}/compatible"),
            format!("{EFI_DIR}/fw_platform_size"),
        ])
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityValue {
    pub value: String,
    pub source: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exposure {
    Exposed,
    NotExposed,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareObservation {
    pub exposure: Exposure,
    pub source: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmiIdentity {
    pub system_vendor: Option<IdentityValue>,
    pub product_name: Option<IdentityValue>,
    pub product_version: Option<IdentityValue>,
    pub board_vendor: Option<IdentityValue>,
    pub board_name: Option<IdentityValue>,
    pub board_version: Option<IdentityValue>,
    pub bios_vendor: Option<IdentityValue>,
    pub bios_version: Option<IdentityValue>,
    pub bios_date: Option<IdentityValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformReport {
    pub uefi: FirmwareObservation,
    pub device_tree: FirmwareObservation,
    pub acpi: FirmwareObservation,
    /// Firmware platform bitness, not CPU bitness or firmware version.
    pub uefi_platform_bits: Option<u32>,
    pub device_tree_model: Option<IdentityValue>,
    pub device_tree_compatible: Vec<String>,
    /// Keep DMI and DT identities separately if they disagree.
    pub dmi: DmiIdentity,
    pub diagnostics: Vec<Diagnostic>,
}

pub(crate) fn probe_platform(root: &Path) -> PlatformReport {
    let mut reader = Reader::new(root);
    let supported = reader.supported("platform", "sys/firmware");
    let mut observation = |device: &str, path: &str| FirmwareObservation {
        exposure: if supported {
            match reader.directory(device, path) {
                Some(true) => Exposure::Exposed,
                Some(false) => Exposure::NotExposed,
                None => Exposure::Unknown,
            }
        } else {
            Exposure::Unknown
        },
        source: format!("/{path}").into(),
    };
    let uefi = observation("uefi", EFI_DIR);
    let device_tree = observation("device_tree", DT_DIR);
    let acpi = observation("acpi", ACPI_DIR);
    let mut report = PlatformReport {
        uefi,
        device_tree,
        acpi,
        uefi_platform_bits: None,
        device_tree_model: None,
        device_tree_compatible: Vec::new(),
        dmi: DmiIdentity::default(),
        diagnostics: Vec::new(),
    };
    if report.uefi.exposure == Exposure::Exposed {
        report.uefi_platform_bits = reader.parsed(
            "uefi",
            &format!("{EFI_DIR}/fw_platform_size"),
            |text| match text {
                "32" => Some(32),
                "64" => Some(64),
                _ => None,
            },
        );
    }
    if report.device_tree.exposure == Exposure::Exposed {
        let model = format!("{DT_DIR}/model");
        report.device_tree_model = dt_strings(&mut reader, &model).and_then(|values| {
            if values.len() != 1 {
                reader.invalid("device_tree", &model, "DT model must contain one string");
                None
            } else {
                Some(IdentityValue {
                    value: values[0].clone(),
                    source: format!("/{model}").into(),
                })
            }
        });
        report.device_tree_compatible =
            dt_strings(&mut reader, &format!("{DT_DIR}/compatible")).unwrap_or_default();
    }
    if supported && reader.directory("dmi", DMI_DIR) == Some(true) {
        let mut value = |field| {
            let path = format!("{DMI_DIR}/{field}");
            reader.text("dmi", &path).map(|value| IdentityValue {
                value,
                source: format!("/{path}").into(),
            })
        };
        report.dmi = DmiIdentity {
            system_vendor: value("sys_vendor"),
            product_name: value("product_name"),
            product_version: value("product_version"),
            board_vendor: value("board_vendor"),
            board_name: value("board_name"),
            board_version: value("board_version"),
            bios_vendor: value("bios_vendor"),
            bios_version: value("bios_version"),
            bios_date: value("bios_date"),
        };
    }
    report.diagnostics = reader.diagnostics;
    report
}

fn dt_strings(reader: &mut Reader<'_>, path: &str) -> Option<Vec<String>> {
    let bytes = reader.bytes("device_tree", path)?;
    let text = std::str::from_utf8(&bytes).ok();
    let values: Option<Vec<String>> = text
        .filter(|text| text.ends_with('\0'))
        .map(|text| {
            text[..text.len() - 1]
                .split('\0')
                .map(str::to_owned)
                .collect()
        })
        .filter(|values: &Vec<String>| {
            !values.is_empty() && values.iter().all(|value| !value.trim().is_empty())
        });
    if values.is_none() {
        reader.invalid(
            "device_tree",
            path,
            "Expected a non-empty NUL-terminated DT string list",
        );
    }
    values
}
