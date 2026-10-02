//! `deviceinfo` 命令行前端。
//!
//! 存在意义有三个：让这个模块**能脱离任何上层单独验证**；给出一份可以直接
//! `diff` 两台机器的输出；把真机采集成测试夹具。

use clap::{Parser, Subcommand};
use deviceinfo::pci::{PCI_DATABASE_PATHS, extract_entries};
use deviceinfo::{
    HardwareReport, PciId, RuntimeStatus, SampleOptions, probe_with, render, sample_state_with,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "deviceinfo",
    version,
    about = "打印本机的硬件、能力与运行时状态",
    long_about = "硬件与能力（`hardware`）是\"这台机器是什么\"，装上就不变，可以跨机器 diff；\n\
                  运行时状态（`state`）是\"此刻怎样\"，每一秒都在变。两者刻意分开。"
)]
struct Cli {
    /// 输出 JSON 而不是给人看的文本
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 硬件与能力：这台机器是什么（不变的部分）
    Hardware {
        /// 探测的根目录
        #[arg(long, value_name = "PATH", default_value = "/")]
        root: PathBuf,
        /// 伪造架构（默认用编译期架构）
        #[arg(long, value_name = "ARCH")]
        arch: Option<String>,
        /// 有 warning 就以退出码 1 结束（给 CI 用）
        #[arg(long)]
        strict: bool,
    },
    /// 运行时状态：此刻怎样（瞬时值）
    State {
        /// 要查磁盘余量的路径，可重复
        #[arg(long = "watch", value_name = "PATH", default_value = "/")]
        watch: Vec<PathBuf>,
        /// 同时读取累积计数器（NPU 忙碌时间）。
        ///
        /// 驱动建议该值的读取间隔不低于 1 秒：频繁读会影响 NPU 作业提交性能。
        #[arg(long)]
        counters: bool,
        /// `/proc` 与 `/sys` 的根目录（一般不用改）
        #[arg(long, value_name = "PATH", default_value = "/")]
        root: PathBuf,
        /// 有 warning 就以退出码 1 结束
        #[arg(long)]
        strict: bool,
    },
    /// 把当前机器采集为测试夹具（含期望输出）
    Capture {
        /// 输出目录，例如 fixtures/lunar-lake-258v
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// 采集源
        #[arg(long, value_name = "PATH", default_value = "/")]
        root: PathBuf,
        /// 记录到 meta.json 里的架构
        #[arg(long, value_name = "ARCH")]
        arch: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Hardware {
        root: PathBuf::from("/"),
        arch: None,
        strict: false,
    }) {
        Command::Hardware { root, arch, strict } => {
            let arch = arch.unwrap_or_else(|| std::env::consts::ARCH.to_string());
            let report = probe_with(&root, &arch);
            if cli.json {
                print_json(&report);
            } else {
                print!("{}", render::human(&report));
            }
            if strict && !report.warnings.is_empty() {
                std::process::exit(1);
            }
        }
        Command::State {
            watch,
            counters,
            root,
            strict,
        } => {
            let state = sample_state_with(
                &root,
                &SampleOptions {
                    watch,
                    counters,
                },
            );
            if cli.json {
                print_json(&state);
            } else {
                print!("{}", render::human_state(&state));
            }
            if strict && !state.warnings.is_empty() {
                std::process::exit(1);
            }
        }
        Command::Capture { out, root, arch } => {
            let arch = arch.unwrap_or_else(|| std::env::consts::ARCH.to_string());
            if let Err(error) = capture(&root, &arch, &out) {
                eprintln!("采集失败: {error}");
                std::process::exit(2);
            }
            println!("已写入 {}", out.display());
        }
    }
}

fn print_json<T: serde::Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(error) => {
            eprintln!("序列化失败: {error}");
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------- 夹具采集

/// 把一台真实机器采集成一个可重现的夹具。
///
/// 产出三样东西：
///
/// - 探测会读到的那些文件的副本（保持相对路径）
/// - `meta.json`：架构等元信息
/// - `expected.json`：当时的探测结果
///
/// 这样 `tests/fixtures.rs` 就能"拿真机形状跑一遍，结果必须一样"——
/// 手写的假 flags 列表抓不到真实内核里的意外（`smep` 就是这么混进指令集列表的）。
fn capture(source: &Path, arch: &str, out: &Path) -> io::Result<()> {
    let report = probe_with(source, arch);
    let plan = capture_plan(source);

    // 1) 探测会读到的文件
    for rel in &plan.files {
        let from = source.join(rel);
        // 只有状态采样读的瞬时值：夹具只保留"读到得到"这个事实，不保留读数
        if is_volatile_leaf(rel) {
            if from.exists() {
                write_file(&out.join(rel), b"0\n")?;
            }
            continue;
        }
        match fs::metadata(&from) {
            // 只读普通文件：`/dev/accel/accel0` 这类字符设备直接读会阻塞
            Ok(meta) if meta.is_file() => {
                let raw = fs::read(&from)?;
                match String::from_utf8(raw.clone()) {
                    Ok(text) => write_file(&out.join(rel), filter_volatile(rel, &text).as_bytes())?,
                    Err(_) => write_file(&out.join(rel), &raw)?,
                }
            }
            // 设备节点之类只需要"存在"这个事实
            Ok(_) => write_file(&out.join(rel), b"")?,
            // 可选文件不存在就跳过——sysfs 里一大半都是可选的
            Err(_) => continue,
        }
    }

    // 2) 重建软链。**必须真的是软链**：驱动名是从 `device/driver` 的末段读出来的，
    //    夹具里放个空文件的话 `read_link` 会失败，"夹具探测出 driver=None"
    //    这种偏差不会报错，只会静默地让夹具和真机不一样。
    for (rel, target) in &plan.symlinks {
        let to = out.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let _ = fs::remove_file(&to);
        std::os::unix::fs::symlink(target, &to)?;
    }

    // 2) 用户态库：只放命中的那几个**文件名**（空文件即可）。
    //    整个 /usr/lib 显然不能进夹具，而 LibraryIndex 只关心文件名。
    for accel in &report.accelerators {
        if let RuntimeStatus::Ready { components, .. } = &accel.runtime {
            for name in components {
                write_file(&out.join("usr/lib").join(name), b"")?;
            }
        }
    }

    // 3) pci.ids 只留用到的条目
    let ids: Vec<PciId> = report
        .accelerators
        .iter()
        .filter_map(|accel| accel.pci_id.clone())
        .collect();
    if !ids.is_empty() {
        for path in PCI_DATABASE_PATHS {
            let Ok(text) = fs::read_to_string(source.join(path)) else {
                continue;
            };
            write_file(&out.join(path), extract_entries(&text, &ids).as_bytes())?;
            break;
        }
    }

    // 4) 元信息与期望输出
    let meta = serde_json::json!({
        "arch": arch,
        "note": "由 `deviceinfo capture` 生成。expected.json 是采集当时的探测结果快照——\
                 探测行为变化时它会失败，人工看 diff 决定是改了行为还是修了 bug。",
    });
    write_file(
        &out.join("meta.json"),
        format!("{}\n", serde_json::to_string_pretty(&meta)?).as_bytes(),
    )?;
    write_file(
        &out.join("expected.json"),
        format!("{}\n", relativize_json(&report, source)).as_bytes(),
    )?;
    Ok(())
}

/// 采集清单：要复制的文件，以及要重建的软链。
///
/// **这份清单是探测逻辑的镜像**：探测读了什么，这里就得复制什么，两边要一起改。
/// 宁可多列几个不存在的路径（`fs::metadata` 会跳过），也不要漏。
struct CapturePlan {
    files: Vec<String>,
    /// `(相对路径, 软链指向的字符串)`。指向什么不重要，探测只取末段做驱动名。
    symlinks: Vec<(String, String)>,
}

fn capture_plan(source: &Path) -> CapturePlan {
    let mut files = vec![
        "proc/cpuinfo".to_string(),
        "proc/meminfo".to_string(),
        "sys/devices/system/cpu/smt/active".to_string(),
    ];
    let mut symlinks = Vec::new();

    for cpu in numeric_entries(source, "sys/devices/system/cpu", "cpu") {
        for rel in [
            "cpu_capacity",
            "cpufreq/cpuinfo_max_freq",
            "topology/core_cpus_list",
        ] {
            files.push(format!("sys/devices/system/cpu/{cpu}/{rel}"));
        }
    }

    // 加速器：设备事实 + 驱动软链 + 软链指向的模块版本
    let mut device_dirs = Vec::new();
    for node in numeric_entries(source, "dev/accel", "accel") {
        files.push(format!("dev/accel/{node}"));
        let device = format!("sys/class/accel/{node}/device");
        for rel in [
            "vendor",
            "device",
            // 新路径优先，npu_max_frequency_mhz 是 legacy alias
            "freq/hw_max_freq",
            "npu_max_frequency_mhz",
            // 运行时状态读的（瞬时值，但夹具有了就能测"读得到"这件事本身）
            "freq/current_freq",
            "npu_memory_utilization",
            "npu_busy_time_us",
        ] {
            files.push(format!("{device}/{rel}"));
        }
        device_dirs.push(device);
    }
    for card in numeric_entries(source, "sys/class/drm", "card") {
        let device = format!("sys/class/drm/{card}/device");
        for rel in [
            "vendor",
            "device",
            "mem_info_vram_total",
            "mem_info_vram_used",
            "gt/gt0/rps_max_freq_mhz",
            "gt/gt0/rps_cur_freq_mhz",
            "gt_max_freq_mhz",
            "gt_cur_freq_mhz",
        ] {
            files.push(format!("{device}/{rel}"));
        }
        // xe 的 tile*/gt*/freq0/{max,cur}_freq
        for tile in numeric_entries(source, &device, "tile") {
            for gt in numeric_entries(source, &format!("{device}/{tile}"), "gt") {
                for leaf in ["max_freq", "cur_freq"] {
                    files.push(format!("{device}/{tile}/{gt}/freq0/{leaf}"));
                }
            }
        }
        device_dirs.push(device);
    }
    for node in numeric_entries(source, "dev/dri", "renderD") {
        files.push(format!("dev/dri/{node}"));
    }

    for device in device_dirs {
        let Ok(target) = fs::read_link(source.join(&device).join("driver")) else {
            continue;
        };
        let Some(name) = target.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // 指向哪儿无所谓，探测只读软链的末段；用 module/<名字> 便于人看
        symlinks.push((format!("{device}/driver"), format!("module/{name}")));
        files.push(format!("sys/module/{name}/version"));
    }

    CapturePlan { files, symlinks }
}

/// `<前缀><纯数字>` 形式的条目名，字典序排序。
///
/// 要求"数字"而不是"以数字开头"：`card0-DP-1` 是显示连接器不是 GPU，
/// 而 `cpufreq` / `cpuidle` 会和 `cpu0` 一起混进 `cpu` 前缀里。
fn numeric_entries(root: &Path, rel_dir: &str, prefix: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root.join(rel_dir)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.strip_prefix(prefix).is_some_and(|rest| {
                !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
        .collect();
    names.sort();
    names
}

/// 把报告里的绝对路径统一成"相对于探测根"的形式（`/dev/accel/accel0`）。
///
/// 夹具要能在任何机器、任何路径下跑，期望值里就不能留 `/tmp/...` 这种绝对前缀。
/// `tests/fixtures.rs` 用同一套规则把实际结果归一化，两边才能比。
fn relativize_json(report: &HardwareReport, root: &Path) -> String {
    let json = serde_json::to_string_pretty(report).expect("报告一定可序列化");
    let prefix = format!("{}/", root.display().to_string().trim_end_matches('/'));
    json.replace(&prefix, "/")
}

/// `/proc/cpuinfo` 里探测器会读的键。其余的（尤其是 `cpu MHz`）都是瞬时值。
const CPUINFO_KEYS: [&str; 9] = [
    "processor",
    "model name",
    "Model",
    "Hardware",
    "Processor",
    "physical id",
    "core id",
    "flags",
    "Features",
];

/// `/proc/meminfo` 里**非瞬时**的键。
///
/// 刻意不收 `MemAvailable` / `MemFree` / `SwapFree`：那些是运行时状态，
/// 它们读得到但**不进任何报告字段**（见 `deviceinfo::state`），写进夹具只会让
/// 每次采集都产生无意义的 git diff，把真正的变化淹没在噪声里。
const MEMINFO_KEYS: [&str; 2] = ["MemTotal", "SwapTotal"];

/// 只有状态采样会读、而且下一秒就变的叶子文件名。夹具里把它们的**读数**归一化成 0。
///
/// 保留真实读数会让**每次**重新采集都产生 diff（`npu_busy_time_us` 是单调计数器，
/// `*_cur_freq` / `npu_memory_utilization` 每时每刻都在变），把"探测逻辑真的变了"
/// 这个信号淹没在噪声里——而夹具的价值恰好就在那个信号。
/// 这些文件在夹具里的**存在**才是被测的东西：采样路径能不能读到它们。
///
/// 注意 `freq/hw_max_freq`、`mem_info_vram_total` 这类**上限**不在名单里：
/// 它们是硬件事实，夹具要如实保存。
const VOLATILE_LEAVES: [&str; 8] = [
    "current_freq",
    "cur_freq",
    "npu_current_frequency_mhz",
    "npu_memory_utilization",
    "npu_busy_time_us",
    "mem_info_vram_used",
    "rps_cur_freq_mhz",
    "gt_cur_freq_mhz",
];

fn is_volatile_leaf(rel: &str) -> bool {
    Path::new(rel)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| VOLATILE_LEAVES.contains(&name))
}

/// 把 `/proc` 文件里探测器不读的行剔掉。
///
/// 夹具的价值在于"探测读了什么"能被人一眼看完；把几百行瞬时值也塞进去，
/// 既让每次重新采集都产生 diff，也让人看不出真正被依赖的是哪几行。
fn filter_volatile(rel: &str, text: &str) -> String {
    let keys: &[&str] = match rel {
        "proc/cpuinfo" => &CPUINFO_KEYS,
        "proc/meminfo" => &MEMINFO_KEYS,
        _ => return text.to_string(),
    };
    let mut out: Vec<&str> = text
        .lines()
        .filter(|line| {
            line.is_empty()
                || line
                    .split_once(':')
                    .is_some_and(|(key, _)| keys.contains(&key.trim()))
        })
        .collect();
    // 末尾的空行是分段留下的，去掉后文件不会以空行结束
    while out.last().is_some_and(|line| line.is_empty()) {
        out.pop();
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

fn write_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}
