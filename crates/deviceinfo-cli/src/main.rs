//! `deviceinfo` 命令行前端。
//!
//! 存在意义有两个：一是让这个模块**能脱离任何上层单独验证**，二是给出一份
//! 可以直接 `diff` 两台机器的输出（`--json`）。

use clap::Parser;
use deviceinfo::{render, probe_with};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "deviceinfo",
    version,
    about = "打印本机的硬件与设备能力报告",
    long_about = "默认探测真实系统。用 --root 可以对着另一棵文件树探测（容器、镜像，\
                  或者调试用的假文件树夹具）。"
)]
struct Cli {
    /// 探测的根目录
    #[arg(long, value_name = "PATH", default_value = "/")]
    root: PathBuf,

    /// 输出 JSON 而不是给人看的文本（便于跨机器 diff）
    #[arg(long)]
    json: bool,

    /// 伪造架构（默认用编译期架构）
    #[arg(long, value_name = "ARCH")]
    arch: Option<String>,

    /// 有 warning 就以退出码 1 结束（给 CI 用）
    #[arg(long)]
    strict: bool,
}

fn main() {
    let cli = Cli::parse();
    let arch = cli.arch.unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let report = probe_with(&cli.root, &arch);

    if cli.json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("序列化失败: {error}");
                std::process::exit(2);
            }
        }
    } else {
        print!("{}", render::human(&report));
    }

    if cli.strict && !report.warnings.is_empty() {
        std::process::exit(1);
    }
}
