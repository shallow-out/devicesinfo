//! `cargo run -p deviceinfo --example mmc_health -- /dev/mmcblk0`
//! Explicit host-device read; no argument means no device access.
use deviceinfo::observe_mmc_health;

fn main() {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("Usage: mmc_health /dev/mmcblkN");
        std::process::exit(2);
    };
    let observation = observe_mmc_health(std::path::Path::new("/"), std::path::Path::new(&path));
    println!("{}", serde_json::to_string_pretty(&observation).unwrap());
}
