//! `cargo run -p deviceinfo --example platform_thermal -- [captured-root]`
//! Linux-visible observations only; no writes or commands.
fn main() {
    let root = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/".into());
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "platform": deviceinfo::inspect_platform(&root),
            "thermal": deviceinfo::observe_thermal(&root, &deviceinfo::ThermalOptions::default()),
        }))
        .unwrap()
    );
}
