//! `cargo run -p deviceinfo --example system_dashboard -- / [arch]`
//! A read-only consumer example; the explicit interval belongs to the caller.
use deviceinfo::{
    StorageHealthOptions, SystemSampleOptions, inspect_storage, inspect_system,
    observe_storage_health, observe_system,
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
    let identity = inspect_system(&root, arch);
    let storage = inspect_storage(&root);
    let health = observe_storage_health(&root, &StorageHealthOptions::default());
    // Watch paths belong to this host. No implicit filesystem probe for a fixture.
    let options = SystemSampleOptions::default();
    let previous = observe_system(&root, &options);
    std::thread::sleep(Duration::from_secs(1));
    let current = observe_system(&root, &options);
    let cpu_percent = current.cpu_usage_since(&previous).ok();
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
