//! A fresh command result must never inherit a mirrored inventory's time window.
use deviceinfo::{
    CaptureMetadata, DiagnosticCode, EnvironmentReport, LiveOptions, ObservationOrigin,
    SampleContext, SampleStamp, check_environment,
};
use std::{
    fs, io,
    sync::atomic::{AtomicU64, Ordering},
};

const BOOT: &str = "01234567-89ab-cdef-0123-456789abcdef";
const OTHER_BOOT: &str = "fedcba98-7654-3210-fedc-ba9876543210";

fn stamp(time: u64) -> SampleStamp {
    SampleStamp {
        unix_time_ns: Some(1_792_000_000_000_000_000 + time),
        boot_time_ns: Some(time),
        boot_clock_resolution_ns: Some(1),
        boot_id: Some(BOOT.into()),
        time_namespace: Some("time:[42]".into()),
        mount_namespace: Some("mnt:[43]".into()),
    }
}

fn environment() -> EnvironmentReport {
    serde_json::from_value(serde_json::json!({
        "inference_tools": [{"name": "llama-server", "path": "/usr/bin/llama-server"}],
        "warnings": []
    }))
    .unwrap()
}

fn no_network() -> LiveOptions {
    LiveOptions {
        skip_network: true,
        ..Default::default()
    }
}

#[test]
fn fresh_stamps_bracket_all_commands_and_network_checks_after_inventory_capture() {
    let root = std::env::temp_dir().join(format!("deviceinfo-live-context-{}", std::process::id()));
    fs::create_dir_all(root.join("proc/sys/kernel/random")).unwrap();
    fs::create_dir_all(root.join("usr/bin")).unwrap();
    fs::write(root.join(deviceinfo::BOOT_ID_INPUT), BOOT).unwrap();
    fs::write(root.join("usr/bin/llama-server"), "").unwrap();
    let metadata = CaptureMetadata {
        schema_version: deviceinfo::SCHEMA_VERSION,
        context: SampleContext::from_bounds(ObservationOrigin::Captured, stamp(100), stamp(200)),
        counters_preserved: true,
        devices: Vec::new(),
    };
    fs::write(
        root.join(deviceinfo::CONTEXT_FILE),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    let inventory = deviceinfo::inspect_environment(&root);
    assert!(inventory.context.consistent);
    assert_eq!(inventory.context.finished.boot_time_ns, Some(200));
    assert_eq!(inventory.data.inference_tools.len(), 1);

    let clock_calls = AtomicU64::new(0);
    let command_calls = AtomicU64::new(0);
    let report = check_environment(
        &inventory.data,
        &LiveOptions {
            targets: vec!["https://one.example".into(), "https://two.example".into()],
            ..Default::default()
        },
        &|program, _args| {
            assert_eq!(clock_calls.load(Ordering::SeqCst), 1);
            command_calls.fetch_add(1, Ordering::SeqCst);
            Ok(if program == "curl" {
                "200"
            } else {
                "version 1"
            }
            .into())
        },
        &|| {
            let index = clock_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                command_calls.load(Ordering::SeqCst),
                if index == 0 { 0 } else { 3 }
            );
            Ok(stamp(2_000 + index * 1_000))
        },
    );
    assert_eq!(clock_calls.load(Ordering::SeqCst), 2);
    assert_eq!(report.context.origin, ObservationOrigin::Live);
    assert!(report.context.consistent);
    assert_eq!(report.context.started, stamp(2_000));
    assert_eq!(report.context.finished, stamp(3_000));
    assert_eq!(report.data.versions.len(), 1);
    assert_eq!(report.data.reachability.len(), 2);
    // The inventory remains a replay with its original frozen window.
    assert_eq!(
        deviceinfo::inspect_environment(&root).context,
        inventory.context
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_clock_reads_keep_check_results_and_clear_only_the_failed_boundaries() {
    for failures in 1..=3 {
        let calls = AtomicU64::new(0);
        let report = check_environment(
            &environment(),
            &no_network(),
            &|_, _| Ok("version 1".into()),
            &|| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                if failures & (1 << index) != 0 {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "target clock denied",
                    ))
                } else {
                    Ok(stamp(2_000 + index * 1_000))
                }
            },
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(report.context.origin, ObservationOrigin::Unattributed);
        assert!(!report.context.consistent);
        assert_eq!(
            report.context.started,
            if failures & 1 != 0 {
                SampleStamp::default()
            } else {
                stamp(2_000)
            }
        );
        assert_eq!(
            report.context.finished,
            if failures & 2 != 0 {
                SampleStamp::default()
            } else {
                stamp(3_000)
            }
        );
        assert_eq!(
            report.context.diagnostics.len(),
            (failures as u32).count_ones() as usize
        );
        assert!(
            report
                .context
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code == DiagnosticCode::PermissionDenied)
        );
        assert_eq!(report.data.versions[0].output.as_deref(), Some("version 1"));
        assert_eq!(
            report.elapsed_since(&report),
            Err(deviceinfo::DifferenceError::InvalidContext)
        );
    }
}

#[test]
fn reboot_namespace_change_clock_reset_and_missing_identity_in_live_checks_are_unknown() {
    let mut endings = vec![stamp(3_000); 5];
    endings[0].boot_id = Some(OTHER_BOOT.into());
    endings[1].time_namespace = Some("time:[99]".into());
    endings[2].mount_namespace = Some("mnt:[99]".into());
    endings[3].boot_time_ns = Some(1_000);
    endings[4].boot_id = None;
    for ending in endings {
        let calls = AtomicU64::new(0);
        let report = check_environment(
            &environment(),
            &no_network(),
            &|_, _| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "version check timed out",
                ))
            },
            &|| {
                Ok(if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    stamp(2_000)
                } else {
                    ending.clone()
                })
            },
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!report.context.consistent);
        assert_eq!(report.context.finished, ending);
        assert!(report.data.versions[0].error.is_some());
    }
}
