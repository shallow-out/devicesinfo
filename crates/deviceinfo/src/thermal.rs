//! Read-only thermal/hwmon observations. PWM register values, tachometer RPM and
//! thermal cooling states are different quantities; channel numbers do not prove
//! a fan/PWM wiring relationship. No control attributes are written.
use crate::{
    Diagnostic, DiagnosticCode, DiagnosticOperation,
    diagnostics::{Reader, indexed},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub const INPUT_DIRS: [&str; 2] = ["sys/class/thermal", "sys/class/hwmon"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemperatureUnit {
    MillidegreeCelsius,
    Millivolt,
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub struct ThermalOptions {
    /// Explicit per-input overrides for thermistor/ADC drivers with voltage ABI.
    /// Keys are observed absolute paths, without a fixture root prefix.
    pub hwmon_temperature_units: BTreeMap<PathBuf, TemperatureUnit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemperatureOrigin {
    ThermalZone,
    Hwmon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HysteresisKind {
    AbsoluteThreshold,
    DeltaFromTrip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemperatureThreshold {
    pub source: PathBuf,
    /// Raw hwmon limit suffix or thermal trip type (critical/hot/passive/active).
    pub kind: String,
    pub raw_value: Option<i64>,
    pub unit: TemperatureUnit,
    pub hysteresis_raw: Option<i64>,
    pub hysteresis_kind: HysteresisKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemperatureSensor {
    pub device: String,
    pub source: PathBuf,
    pub origin: TemperatureOrigin,
    pub chip_name: Option<String>,
    pub label: Option<String>,
    pub sensor_type: Option<u64>,
    pub raw_input: Option<i64>,
    pub unit: TemperatureUnit,
    /// None for fault/disabled sensors or voltage/unknown units. Negative is valid.
    pub temperature_millicelsius: Option<i64>,
    pub enabled: Option<bool>,
    pub alarms: BTreeMap<String, Option<bool>>,
    pub fault: Option<bool>,
    pub thresholds: Vec<TemperatureThreshold>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanSensor {
    pub device: String,
    pub source: PathBuf,
    pub chip_name: Option<String>,
    pub label: Option<String>,
    /// Measured tachometer speed only; a zero reading is not missing data.
    pub rpm: Option<u64>,
    pub raw_rpm: Option<u64>,
    pub target_rpm: Option<u64>,
    pub minimum_rpm: Option<u64>,
    pub maximum_rpm: Option<u64>,
    pub enabled: Option<bool>,
    pub alarm: Option<bool>,
    pub fault: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PwmChannel {
    pub device: String,
    pub source: PathBuf,
    pub chip_name: Option<String>,
    /// Reported PWM register setting (0..255), not an electrical duty measurement.
    pub value_0_255: Option<u8>,
    /// 0=no control, 1=manual, >=2=driver-defined automatic modes.
    pub enable_mode: Option<u64>,
    /// 0=DC, 1=PWM; retain separately from enable_mode.
    pub output_mode: Option<u8>,
    pub frequency_hz: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoolingDevice {
    pub device: String,
    pub source: PathBuf,
    pub kind: Option<String>,
    pub current_state: Option<u64>,
    pub maximum_state: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThermalReport {
    pub temperatures: Vec<TemperatureSensor>,
    pub fans: Vec<FanSensor>,
    pub pwm: Vec<PwmChannel>,
    pub cooling_devices: Vec<CoolingDevice>,
    pub diagnostics: Vec<Diagnostic>,
}

pub(crate) fn sample_thermal_with(root: &Path, options: &ThermalOptions) -> ThermalReport {
    let mut reader = Reader::new(root);
    let mut report = ThermalReport::default();
    if !reader.supported("thermal", INPUT_DIRS[0]) {
        report.diagnostics = reader.diagnostics;
        return report;
    }
    for name in reader.entries("thermal", INPUT_DIRS[0]) {
        let base = format!("{}/{name}", INPUT_DIRS[0]);
        if indexed(&name, "thermal_zone").is_some() {
            let entries = reader.entries(&name, &base);
            let source = format!("/{base}/temp").into();
            let value = reader.number(&name, &format!("{base}/temp"));
            let temperature = celsius_value(&mut reader, &name, &format!("{base}/temp"), value);
            let label = reader.text(&name, &format!("{base}/type"));
            let mut thresholds = Vec::new();
            let mut trips: BTreeSet<_> = entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .strip_suffix("_temp")
                        .and_then(|prefix| indexed(prefix, "trip_point_"))
                })
                .collect();
            trips.extend(entries.iter().filter_map(|entry| {
                entry
                    .strip_suffix("_type")
                    .and_then(|prefix| indexed(prefix, "trip_point_"))
            }));
            for trip in trips {
                let path = format!("{base}/trip_point_{trip}");
                let kind = reader
                    .text(&name, &format!("{path}_type"))
                    .unwrap_or_else(|| "unknown".into());
                let raw_value = reader.number(&name, &format!("{path}_temp"));
                let hysteresis_raw = optional_number(
                    &mut reader,
                    &name,
                    &base,
                    &entries,
                    &format!("trip_point_{trip}_hyst"),
                );
                thresholds.push(TemperatureThreshold {
                    source: format!("/{path}_temp").into(),
                    kind,
                    raw_value,
                    unit: TemperatureUnit::MillidegreeCelsius,
                    hysteresis_raw,
                    hysteresis_kind: HysteresisKind::DeltaFromTrip,
                });
            }
            report.temperatures.push(TemperatureSensor {
                device: name,
                source,
                origin: TemperatureOrigin::ThermalZone,
                chip_name: None,
                label,
                sensor_type: None,
                raw_input: value,
                unit: TemperatureUnit::MillidegreeCelsius,
                temperature_millicelsius: temperature,
                enabled: None,
                alarms: BTreeMap::new(),
                fault: None,
                thresholds,
            });
        } else if indexed(&name, "cooling_device").is_some() {
            let kind = reader.text(&name, &format!("{base}/type"));
            let maximum_state = reader.number(&name, &format!("{base}/max_state"));
            let mut current_state = reader.number(&name, &format!("{base}/cur_state"));
            if current_state
                .zip(maximum_state)
                .is_some_and(|(current, max)| current > max)
            {
                reader.invalid(
                    &name,
                    &format!("{base}/cur_state"),
                    "Cooling state exceeds max_state",
                );
                current_state = None;
            }
            report.cooling_devices.push(CoolingDevice {
                device: name,
                source: format!("/{base}").into(),
                kind,
                current_state,
                maximum_state,
            });
        }
    }
    for name in reader
        .entries("hwmon", INPUT_DIRS[1])
        .into_iter()
        .filter(|name| indexed(name, "hwmon").is_some())
    {
        let base = format!("{}/{name}", INPUT_DIRS[1]);
        let entries = reader.entries(&name, &base);
        let chip = reader.text(&name, &format!("{base}/name"));
        for index in channels(&entries, "temp") {
            let prefix = format!("temp{index}");
            let device = format!("{name}/{prefix}");
            let input = format!("{base}/{prefix}_input");
            let raw_input = reader.number(&device, &input);
            let sensor_type = optional_number(
                &mut reader,
                &device,
                &base,
                &entries,
                &format!("{prefix}_type"),
            );
            let invalid_type = sensor_type.is_some_and(|value| !(1..=6).contains(&value));
            if invalid_type {
                reader.invalid(
                    &device,
                    &format!("{base}/{prefix}_type"),
                    "Unsupported hwmon sensor type code",
                );
            }
            let uncertain_type = invalid_type
                || (entries.contains(&format!("{prefix}_type")) && sensor_type.is_none());
            let enabled = optional_boolean(
                &mut reader,
                &device,
                &base,
                &entries,
                &format!("{prefix}_enable"),
            );
            let fault = optional_boolean(
                &mut reader,
                &device,
                &base,
                &entries,
                &format!("{prefix}_fault"),
            );
            let unit = options
                .hwmon_temperature_units
                .get(Path::new(&format!("/{input}")))
                .copied()
                .unwrap_or(if sensor_type == Some(4) || uncertain_type {
                    TemperatureUnit::Unknown
                } else {
                    TemperatureUnit::MillidegreeCelsius
                });
            if unit == TemperatureUnit::Unknown {
                reader.issue(
                    &device,
                    &input,
                    DiagnosticOperation::Decode,
                    DiagnosticCode::Unsupported,
                    "Input unit is unverified; provide an explicit unit before interpreting it as Celsius",
                );
            }
            let temperature_millicelsius = (unit == TemperatureUnit::MillidegreeCelsius
                && usable(&entries, &prefix, fault, enabled))
            .then(|| celsius_value(&mut reader, &device, &input, raw_input))
            .flatten();
            let label = optional_text(
                &mut reader,
                &device,
                &base,
                &entries,
                &format!("{prefix}_label"),
            );
            let mut thresholds = Vec::new();
            for kind in ["min", "max", "lcrit", "crit", "emergency"] {
                let leaf = format!("{prefix}_{kind}");
                if entries.contains(&leaf) {
                    let raw_value = reader.number(&device, &format!("{base}/{leaf}"));
                    let hysteresis_raw = optional_number(
                        &mut reader,
                        &device,
                        &base,
                        &entries,
                        &format!("{leaf}_hyst"),
                    );
                    thresholds.push(TemperatureThreshold {
                        source: format!("/{base}/{leaf}").into(),
                        kind: kind.into(),
                        raw_value,
                        unit,
                        hysteresis_raw,
                        hysteresis_kind: HysteresisKind::AbsoluteThreshold,
                    });
                }
            }
            let mut alarms = BTreeMap::new();
            for leaf in entries.iter().filter(|leaf| {
                is_input(leaf)
                    && leaf.starts_with(&format!("{prefix}_"))
                    && leaf.ends_with("_alarm")
            }) {
                alarms.insert(
                    leaf.clone(),
                    reader.boolean(&device, &format!("{base}/{leaf}")),
                );
            }
            report.temperatures.push(TemperatureSensor {
                device,
                source: format!("/{input}").into(),
                origin: TemperatureOrigin::Hwmon,
                chip_name: chip.clone(),
                label,
                sensor_type,
                raw_input,
                unit,
                temperature_millicelsius,
                enabled,
                alarms,
                fault,
                thresholds,
            });
        }
        for index in channels(&entries, "fan") {
            let prefix = format!("fan{index}");
            let device = format!("{name}/{prefix}");
            let raw_rpm = reader.number(&device, &format!("{base}/{prefix}_input"));
            let fault = optional_boolean(
                &mut reader,
                &device,
                &base,
                &entries,
                &format!("{prefix}_fault"),
            );
            let enabled = optional_boolean(
                &mut reader,
                &device,
                &base,
                &entries,
                &format!("{prefix}_enable"),
            );
            report.fans.push(FanSensor {
                source: format!("/{base}/{prefix}_input").into(),
                chip_name: chip.clone(),
                label: optional_text(
                    &mut reader,
                    &device,
                    &base,
                    &entries,
                    &format!("{prefix}_label"),
                ),
                rpm: usable(&entries, &prefix, fault, enabled)
                    .then_some(raw_rpm)
                    .flatten(),
                raw_rpm,
                target_rpm: optional_number(
                    &mut reader,
                    &device,
                    &base,
                    &entries,
                    &format!("{prefix}_target"),
                ),
                minimum_rpm: optional_number(
                    &mut reader,
                    &device,
                    &base,
                    &entries,
                    &format!("{prefix}_min"),
                ),
                maximum_rpm: optional_number(
                    &mut reader,
                    &device,
                    &base,
                    &entries,
                    &format!("{prefix}_max"),
                ),
                alarm: optional_boolean(
                    &mut reader,
                    &device,
                    &base,
                    &entries,
                    &format!("{prefix}_alarm"),
                ),
                fault,
                enabled,
                device,
            });
        }
        for index in channels(&entries, "pwm") {
            let prefix = format!("pwm{index}");
            let device = format!("{name}/{prefix}");
            let value_0_255 = reader.number(&device, &format!("{base}/{prefix}"));
            let output_mode = if entries.contains(&format!("{prefix}_mode")) {
                reader.parsed(
                    &device,
                    &format!("{base}/{prefix}_mode"),
                    |text| match text {
                        "0" => Some(0),
                        "1" => Some(1),
                        _ => None,
                    },
                )
            } else {
                None
            };
            report.pwm.push(PwmChannel {
                source: format!("/{base}/{prefix}").into(),
                chip_name: chip.clone(),
                value_0_255,
                enable_mode: optional_number(
                    &mut reader,
                    &device,
                    &base,
                    &entries,
                    &format!("{prefix}_enable"),
                ),
                output_mode,
                frequency_hz: optional_number(
                    &mut reader,
                    &device,
                    &base,
                    &entries,
                    &format!("{prefix}_freq"),
                ),
                device,
            });
        }
    }
    report.diagnostics = reader.diagnostics;
    report
}

fn channels(entries: &[String], prefix: &str) -> BTreeSet<u32> {
    entries
        .iter()
        .filter(|entry| is_input(entry))
        .filter_map(|entry| {
            let head = entry.split('_').next()?;
            indexed(head, prefix).filter(|index| *index > 0)
        })
        .collect()
}
fn celsius_value(
    reader: &mut Reader<'_>,
    device: &str,
    path: &str,
    value: Option<i64>,
) -> Option<i64> {
    if value.is_some_and(|value| value < -273_150) {
        reader.invalid(
            device,
            path,
            "Temperature is below absolute zero; retain raw evidence only",
        );
        None
    } else {
        value
    }
}
fn usable(entries: &[String], prefix: &str, fault: Option<bool>, enabled: Option<bool>) -> bool {
    fault != Some(true)
        && enabled != Some(false)
        && (!entries.contains(&format!("{prefix}_fault")) || fault.is_some())
        && (!entries.contains(&format!("{prefix}_enable")) || enabled.is_some())
}
fn optional_number<T: std::str::FromStr>(
    reader: &mut Reader<'_>,
    device: &str,
    base: &str,
    entries: &[String],
    leaf: &str,
) -> Option<T> {
    entries
        .iter()
        .any(|entry| entry == leaf)
        .then(|| reader.number(device, &format!("{base}/{leaf}")))
        .flatten()
}
fn optional_boolean(
    reader: &mut Reader<'_>,
    device: &str,
    base: &str,
    entries: &[String],
    leaf: &str,
) -> Option<bool> {
    entries
        .iter()
        .any(|entry| entry == leaf)
        .then(|| reader.boolean(device, &format!("{base}/{leaf}")))
        .flatten()
}
fn optional_text(
    reader: &mut Reader<'_>,
    device: &str,
    base: &str,
    entries: &[String],
    leaf: &str,
) -> Option<String> {
    entries
        .iter()
        .any(|entry| entry == leaf)
        .then(|| reader.text(device, &format!("{base}/{leaf}")))
        .flatten()
}

/// Shared capture plan: enumerate only class devices, not entire /sys/devices.
pub fn input_dirs(list: &impl Fn(&str) -> Vec<String>) -> Vec<String> {
    INPUT_DIRS
        .iter()
        .flat_map(|dir| {
            list(dir)
                .into_iter()
                .filter(|name| {
                    indexed(name, "hwmon").is_some()
                        || indexed(name, "thermal_zone").is_some()
                        || indexed(name, "cooling_device").is_some()
                })
                .map(move |name| format!("{dir}/{name}"))
        })
        .collect()
}
pub fn inputs(list: &impl Fn(&str) -> Vec<String>) -> Vec<String> {
    input_dirs(list)
        .into_iter()
        .flat_map(|base| {
            list(&base)
                .into_iter()
                .filter(|leaf| is_input(leaf))
                .map(move |leaf| format!("{base}/{leaf}"))
        })
        .collect()
}
fn is_input(leaf: &str) -> bool {
    if ["name", "type", "temp", "max_state", "cur_state"].contains(&leaf) {
        return true;
    }
    if let Some(tail) = leaf.strip_prefix("trip_point_") {
        return tail.split_once('_').is_some_and(|(id, kind)| {
            indexed(id, "").is_some() && ["type", "temp", "hyst"].contains(&kind)
        });
    }
    for prefix in ["temp", "fan", "pwm"] {
        let (channel, suffix) = leaf.split_once('_').unwrap_or((leaf, ""));
        if indexed(channel, prefix).is_some_and(|index| index > 0) {
            return [
                "",
                "input",
                "label",
                "type",
                "enable",
                "mode",
                "freq",
                "target",
                "min",
                "max",
                "lcrit",
                "crit",
                "emergency",
                "min_hyst",
                "max_hyst",
                "lcrit_hyst",
                "crit_hyst",
                "emergency_hyst",
                "alarm",
                "min_alarm",
                "max_alarm",
                "lcrit_alarm",
                "crit_alarm",
                "emergency_alarm",
                "fault",
            ]
            .contains(&suffix);
        }
    }
    false
}
