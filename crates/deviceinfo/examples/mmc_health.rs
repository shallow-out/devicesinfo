//! `cargo run -p deviceinfo --example mmc_health -- /dev/mmcblk0`
//! Explicit host-device read; no argument means no device access.
use deviceinfo::{StorageHealthError, read_mmc_health};

fn main() {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("Usage: mmc_health /dev/mmcblkN");
        std::process::exit(2);
    };
    let (health, error) = match read_mmc_health(std::path::Path::new(&path)) {
        Ok(health) => (Some(health), None),
        Err(error) => (None, Some(StorageHealthError::from(error))),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"health": health, "error": error}))
            .unwrap()
    );
}
