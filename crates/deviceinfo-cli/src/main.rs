//! `deviceinfo` 命令行前端。
//!
//! 存在意义有三个：让这个模块**能脱离任何上层单独验证**；给出一份可以直接
//! `diff` 两台机器的输出；把真机采集成测试夹具。

mod source;

use clap::{Parser, Subcommand};
use deviceinfo::pci::{PCI_DATABASE_PATHS, extract_entries};
use deviceinfo::{
    HardwareReport, LIBRARY_DIRS, PciId, RuntimeStatus, SampleOptions, probe_with, render,
    sample_state_with,
};
use source::{Entry, EntryKind, Source};
use std::collections::BTreeMap;
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
        /// 采集源（本地的一棵文件树）
        #[arg(long, value_name = "PATH", default_value = "/", conflicts_with = "ssh")]
        root: PathBuf,
        /// 隔着 ssh 采集另一台机器。
        ///
        /// 夹具必须在**目标机**上采，而目标机往往装不了 Rust 工具链（路由器、嵌入式盒子），
        /// 本机也未必能交叉编译到它的架构。主机名可以是 `~/.ssh/config` 里的别名。
        #[arg(long, value_name = "HOST")]
        ssh: Option<String>,
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
        Command::Capture {
            out,
            root,
            ssh,
            arch,
        } => {
            let arch = arch.unwrap_or_else(|| std::env::consts::ARCH.to_string());
            let source = match &ssh {
                Some(host) => Source::remote(host),
                None => Source::local(&root),
            };
            match capture(&source, &arch, &out) {
                Ok(()) => println!("已从 {} 采集到 {}", source.describe(), out.display()),
                Err(error) => {
                    eprintln!("采集失败: {error}");
                    std::process::exit(2);
                }
            }
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
fn capture(source: &Source, arch: &str, out: &Path) -> io::Result<()> {
    // 远端先镜像成本地临时树，之后一切都按本地处理——这样本地采集的路径一字未变，
    // 远端采集只是多了一步落地。
    let staging = TempTree::new()?;
    let root = match source {
        Source::Local(root) => root.clone(),
        Source::Remote(_) => {
            mirror(source, &staging.path)?;
            staging.path.clone()
        }
    };

    // 采集源不对时**必须报错**，不能写出一份空壳夹具。
    //
    // 这条守卫来自一个真实的失败模式：`ssh` 不可用（没装、认证失败、主机名敲错）时，
    // 列目录全返回空，于是采集**不报错**、写出一份几乎空的夹具、还说"已采集"。
    // `--root` 指错地方同理。而夹具是要进回归测试的——**一份空壳夹具比没有夹具更坏**，
    // 它会让后来的所有探测变更都"通过"。
    for required in ESSENTIAL_FILES {
        if !root.join(required).is_file() {
            return Err(io::Error::other(format!(
                "{} 里没有 {required}：采集源不对，或者远端没取到东西。\
                 拒绝写出一份空壳夹具。",
                source.describe()
            )));
        }
    }

    capture_from_root(&root, arch, out)
}

/// 采集前必须存在的文件。理由见 [`capture`] 里的守卫。
const ESSENTIAL_FILES: [&str; 2] = ["proc/cpuinfo", "proc/meminfo"];

/// 把远端的东西落到本地临时树。
///
/// 只拉探测**读**的那些文件，加上库目录的**文件名**——库内容对夹具毫无用处，
/// 而 `LibraryIndex` 只看名字。
fn mirror(source: &Source, stage: &Path) -> io::Result<()> {
    let plan = capture_plan(source)?;
    // 一次 ssh 把全部内容取回来，而不是每个文件开一次连接
    source.prefetch(&plan.files)?;

    for rel in &plan.files {
        let raw = match source.kind(rel) {
            // 设备节点（`/dev/accel/accel0` 这类字符设备）取不得内容：直接读会阻塞。
            // 夹具只需要"它存在"这个事实。
            EntryKind::Other => Vec::new(),
            // **说它是普通文件就得真读到内容。** 这里以前是 `unwrap_or_default()`——
            // 读失败就写一个空文件，而那正是我修过两次的病：空文件看起来像正常数据，
            // 探测会把缺的值读成空字符串。读到就报错，至少能看出是哪一条。
            EntryKind::File => source.read(rel).ok_or_else(|| {
                io::Error::other(format!("{rel} 报为普通文件但读不到内容"))
            })?,
            // **不存在就跳过。** 写成空文件会让探测把缺的项读成空字符串
            EntryKind::Missing => continue,
        };
        write_file(&stage.join(rel), &raw)?;
    }
    for (rel, target) in &plan.symlinks {
        let to = stage.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let _ = fs::remove_file(&to);
        std::os::unix::fs::symlink(target, &to)?;
    }
    // 库目录一次列完。**目录名要滤掉**：`usr/lib` 的列表里含 `aarch64-linux-gnu`
    // 这样的目录，把它当库文件写成普通文件，下一步往它里面写就会 `EEXIST`。
    let library_dirs: Vec<String> = LIBRARY_DIRS.iter().map(|dir| dir.to_string()).collect();
    for (dir, entries) in source.list_many(&library_dirs)? {
        for entry in entries.into_iter().filter(|entry| !entry.is_dir) {
            write_file(&stage.join(&dir).join(entry.name), b"")?;
        }
    }
    Ok(())
}

/// 采集期间用的临时目录，用完自动删。
struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn new() -> io::Result<Self> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|taken| taken.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "deviceinfo-mirror-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// 从一棵**本地**文件树写夹具。
fn capture_from_root(root: &Path, arch: &str, out: &Path) -> io::Result<()> {
    let source = Source::local(root);
    let report = probe_with(root, arch);
    let plan = capture_plan(&source)?;

    // 1) 探测会读到的文件
    for rel in &plan.files {
        let from = root.join(rel);
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
            let Ok(text) = fs::read_to_string(root.join(path)) else {
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
        format!("{}\n", relativize_json(&report, root)?).as_bytes(),
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

fn capture_plan(source: &Source) -> io::Result<CapturePlan> {
    let mut files = vec![
        "proc/cpuinfo".to_string(),
        "proc/meminfo".to_string(),
        "sys/devices/system/cpu/smt/active".to_string(),
        // arm64 的型号来源之一（`/proc/cpuinfo` 在 arm64 上**未必**有型号，
        // 取决于内核；这台 CIX 的有，那台 Rockchip 的没有）
        "sys/firmware/devicetree/base/model".to_string(),
        // ACPI 平台没有设备树，DMI 才是对应物
        "sys/class/dmi/id/product_name".to_string(),
    ];
    let mut symlinks = Vec::new();
    let mut lists = ListCache::new(source);

    // 阶段一：固定目录一次列完（远端每次调用都要在那边起一个 shell，很贵）
    const CPU_DIR: &str = "sys/devices/system/cpu";
    const ACCEL_DIR: &str = "dev/accel";
    const DRM_DIR: &str = "sys/class/drm";
    const DRI_DIR: &str = "dev/dri";
    lists.ensure(&[
        CPU_DIR.to_string(),
        ACCEL_DIR.to_string(),
        DRM_DIR.to_string(),
        DRI_DIR.to_string(),
    ])?;

    for cpu in lists.numeric(CPU_DIR, "cpu") {
        for rel in [
            "cpu_capacity",
            "cpufreq/cpuinfo_max_freq",
            "topology/core_cpus_list",
        ] {
            files.push(format!("{CPU_DIR}/{cpu}/{rel}"));
        }
    }

    // 加速器：设备事实 + 驱动软链 + 软链指向的模块版本
    let mut device_dirs = Vec::new();
    for node in lists.numeric(ACCEL_DIR, "accel") {
        files.push(format!("{ACCEL_DIR}/{node}"));
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
            "of_node/compatible",
        ] {
            files.push(format!("{device}/{rel}"));
        }
        device_dirs.push(device);
    }

    let cards: Vec<String> = lists
        .numeric(DRM_DIR, "card")
        .iter()
        .map(|card| format!("{DRM_DIR}/{card}/device"))
        .collect();
    // 阶段二：所有 card 的 device 目录 + 各自的 drm/ 一次列完
    let drm_subdirs: Vec<String> = cards.iter().map(|d| format!("{d}/drm")).collect();
    lists.ensure(&cards)?;
    lists.ensure(&drm_subdirs)?;

    let mut tile_dirs = Vec::new();
    for device in &cards {
        for rel in [
            "vendor",
            "device",
            "mem_info_vram_total",
            "mem_info_vram_used",
            "gt/gt0/rps_max_freq_mhz",
            "gt/gt0/rps_cur_freq_mhz",
            "gt_max_freq_mhz",
            "gt_cur_freq_mhz",
            "of_node/compatible",
        ] {
            files.push(format!("{device}/{rel}"));
        }
        // render 节点的**归属**靠 `<device>/drm/` 的目录项表达，而不是靠
        // sys/class/drm 的软链（软链存不进夹具）。
        //
        // **必须把该目录的条目全部记下来**，不能只取 `renderD*`：一个只有
        // `controlD*` 的 card 说明它只会显示输出，而"目录里没有 render 节点"和
        // "这个目录压根不存在"是两件事——前者是显示设备，后者是判不出来。
        // 只记 renderD* 会让夹具里连目录都没有，于是分类结果和真机不一样。
        let drm_subdir = format!("{device}/drm");
        for entry in lists.entries(&drm_subdir) {
            files.push(format!("{drm_subdir}/{}", entry.name));
        }
        // xe 的 tile*/gt*/freq0/{max,cur}_freq
        for tile in lists.numeric(device, "tile") {
            tile_dirs.push(format!("{device}/{tile}"));
        }
        device_dirs.push(device.clone());
    }
    // 阶段三：tile 下面的 gt 目录
    lists.ensure(&tile_dirs)?;
    for tile_dir in &tile_dirs {
        for gt in lists.numeric(tile_dir, "gt") {
            for leaf in ["max_freq", "cur_freq"] {
                files.push(format!("{tile_dir}/{gt}/freq0/{leaf}"));
            }
        }
    }

    for node in lists.numeric(DRI_DIR, "renderD") {
        files.push(format!("{DRI_DIR}/{node}"));
    }

    // PCI 总线：找"本模块不建模的加速器"。照抄内核自己的分类（`class` 0x12xx / 0x0b40），
    // 这样 Hailo-8 / Coral / FPGA 卡这类既不走 /dev/accel 也不出 DRM 的设备不会静默消失。
    const PCI_DIR: &str = "sys/bus/pci/devices";
    lists.ensure(&[PCI_DIR.to_string()])?;
    let mut pci_driver_links = Vec::new();
    for device in lists.entries(PCI_DIR) {
        let base = format!("{PCI_DIR}/{}", device.name);
        files.push(format!("{base}/class"));
        // uevent 里有 `PCI_SLOT_NAME`，去重要用它（软链在夹具里存不下来）
        files.push(format!("{base}/uevent"));
        pci_driver_links.push(format!("{base}/driver"));
    }

    // 已发现加速器的 uevent：里面有 `PCI_SLOT_NAME`，扫 PCI 总线时用它去重
    for device in &device_dirs {
        files.push(format!("{device}/uevent"));
    }

    // 阶段四：所有驱动软链一次读完
    let mut link_paths: Vec<String> = device_dirs
        .iter()
        .map(|device| format!("{device}/driver"))
        .collect();
    link_paths.extend(pci_driver_links);
    let links = source.link_many(&link_paths)?;
    for (path, target) in links {
        let Some(name) = target
            .as_deref()
            .and_then(|target| Path::new(target).file_name())
            .and_then(|name| name.to_str())
        else {
            continue;
        };
        // 指向哪儿无所谓，探测只读软链的末段；用 module/<名字> 便于人看
        symlinks.push((path.clone(), format!("module/{name}")));
        files.push(format!("sys/module/{name}/version"));
    }

    Ok(CapturePlan { files, symlinks })
}

/// 目录列表的缓存。
///
/// 采集要列十来个目录，而远端每次调用都要在那边起一个 shell（实测 ~190ms）。
/// 把"还缺哪些目录"攒起来一次性问，是把采集时间降下来的唯一办法。
struct ListCache<'a> {
    source: &'a Source,
    dirs: BTreeMap<String, Vec<Entry>>,
}

impl<'a> ListCache<'a> {
    fn new(source: &'a Source) -> Self {
        Self {
            source,
            dirs: BTreeMap::new(),
        }
    }

    /// 确保这些目录都列过了。没列过的**合并成一次往返**。
    fn ensure(&mut self, dirs: &[String]) -> io::Result<()> {
        let missing: Vec<String> = dirs
            .iter()
            .filter(|dir| !self.dirs.contains_key(*dir))
            .cloned()
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        self.dirs.extend(self.source.list_many(&missing)?);
        // 远端没返回的（目录不存在）也要记下来，免得反复问
        for dir in missing {
            self.dirs.entry(dir).or_default();
        }
        Ok(())
    }

    fn entries(&self, dir: &str) -> &[Entry] {
        self.dirs.get(dir).map(Vec::as_slice).unwrap_or(&[])
    }

    /// `<前缀><纯数字>` 形式的条目名，字典序排序。
    ///
    /// 要求"全是数字"而不是"以数字开头"：`card0-DP-1` 是显示连接器不是 GPU，
    /// 而 `cpufreq` / `cpuidle` 会和 `cpu0` 一起混进 `cpu` 前缀里。
    fn numeric(&self, dir: &str, prefix: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .entries(dir)
            .iter()
            .filter(|entry| {
                entry.name.strip_prefix(prefix).is_some_and(|rest| {
                    !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
            .map(|entry| entry.name.clone())
            .collect();
        names.sort();
        names
    }
}

/// 把报告里的绝对路径统一成"相对于探测根"的形式（`/dev/accel/accel0`）。
///
/// 夹具要能在任何机器、任何路径下跑，期望值里就不能留 `/tmp/...` 这种绝对前缀。
/// `tests/fixtures.rs` 用同一套规则把实际结果归一化，两边才能比。
fn relativize_json(report: &HardwareReport, root: &Path) -> io::Result<String> {
    // 不用 `expect`：`PathBuf` 的 serde 实现会在路径不是合法 UTF-8 时失败，
    // 而设备路径来自外部输入。报错比 panic 好。
    let json = serde_json::to_string_pretty(report)
        .map_err(|error| io::Error::other(format!("报告序列化失败: {error}")))?;
    let prefix = format!("{}/", root.display().to_string().trim_end_matches('/'));
    Ok(json.replace(&prefix, "/"))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 采集源不对时**必须报错**。以前它会"成功"写出一份空壳夹具——而空壳夹具比没有
    /// 夹具更坏：它会让后来的所有探测变更都"通过"。
    #[test]
    fn capture_refuses_an_empty_source() {
        let out = std::env::temp_dir().join(format!("deviceinfo-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&out);

        let source = Source::local(Path::new("/definitely/not/here"));
        let error = capture(&source, "x86_64", &out).unwrap_err();
        assert!(error.to_string().contains("proc/cpuinfo"), "{error}");
        assert!(
            !out.exists(),
            "失败时不该留下半个夹具目录——那比没有更坏"
        );

        let _ = fs::remove_dir_all(&out);
    }
}
