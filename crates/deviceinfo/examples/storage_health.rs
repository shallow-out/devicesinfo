//! `cargo run -p deviceinfo --example storage_health -- /dev/nvme0`
//! With no arguments, reads only MMC sysfs and never opens NVMe nodes.
use deviceinfo::{StorageHealthOptions, sample_storage_health};

fn main() {
    let options = StorageHealthOptions {
        nvme_controllers: std::env::args_os().skip(1).map(Into::into).collect(),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&sample_storage_health(&options)).unwrap()
    );
}
