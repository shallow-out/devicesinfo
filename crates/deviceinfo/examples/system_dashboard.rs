//! `cargo run -p deviceinfo --example system_dashboard -- / [arch]`
//! A read-only consumer example; the explicit interval belongs to the caller.
use deviceinfo::{
    StorageHealthOptions, SystemSampleOptions, probe_storage, probe_system_with,
    sample_storage_health_with, sample_system_state_with,
};
use std::{path::PathBuf, time::Duration};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let root = args.get(1).map(PathBuf::from).unwrap_or_else(|| "/".into());
    let arch = args
        .get(2)
        .map(String::as_str)
        .unwrap_or(std::env::consts::ARCH);
    // Keep this report in the consumer's low-frequency cache, with an explicit
    // invalidation policy for reboot, OS upgrades and hostname changes.
    let identity = probe_system_with(&root, arch);
    let storage = probe_storage(&root);
    let health = sample_storage_health_with(&root, &StorageHealthOptions::default());
    // Watch paths belong to this host. No implicit filesystem probe for a fixture.
    let options = SystemSampleOptions::default();
    let previous = sample_system_state_with(&root, &options);
    std::thread::sleep(Duration::from_secs(1));
    let current = sample_system_state_with(&root, &options);
    let cpu_percent = current
        .cpu
        .zip(previous.cpu)
        .and_then(|(now, old)| now.usage_since(&old));
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "identity": identity,
            "storage": storage,
            "storage_health": health,
            "state": current,
            "cpu_percent": cpu_percent,
        }))
        .unwrap()
    );
}
