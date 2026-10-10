//! `deviceinfo` 命令行前端。
//!
//! 存在意义有三个：让这个模块**能脱离任何上层单独验证**；给出一份可以直接
//! `diff` 两台机器的输出；把真机采集成测试夹具。

mod source;

use clap::{Parser, Subcommand};
use deviceinfo::pci::{PCI_DATABASE_PATHS, extract_entries};
use deviceinfo::{
    CPU_PER_CORE_INPUTS, CPU_SHARED_INPUTS, LIBRARY_DIRS, OPENCL_VENDOR_DIR, PciId, SampleOptions,
    probe_with, render, sample_state_with,
};
use source::{Entry, EntryKind, Source};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

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
        #[arg(long, value_name = "PATH", default_value = "/", conflicts_with = "ssh")]
        root: PathBuf,
        /// 隔着 ssh 探测另一台机器（不写夹具）。
        ///
        /// 做法和采集一样：先把探测要读的文件镜像成本地临时树，然后照常探测。
        /// 注意 `state` **没有**这个开关——磁盘余量查的是本机挂载的文件系统，
        /// 隔着 ssh 问"那块盘还剩多少"问的是错的对象。
        #[arg(long, value_name = "HOST")]
        ssh: Option<String>,
        /// 伪造架构（默认用编译期架构）
        #[arg(long, value_name = "ARCH")]
        arch: Option<String>,
        /// 有 warning 就以退出码 1 结束（给 CI 用）
        #[arg(long)]
        strict: bool,
    },
    /// 软件环境：装了什么、配了什么（变了才变）
    Environment {
        /// 探测的根目录
        #[arg(long, value_name = "PATH", default_value = "/", conflicts_with = "ssh")]
        root: PathBuf,
        /// 隔着 ssh 探测另一台机器
        #[arg(long, value_name = "HOST")]
        ssh: Option<String>,
        /// 有 warning 就以退出码 1 结束
        #[arg(long)]
        strict: bool,
    },
    /// 实时探测：会**执行命令**、会**连网络**的那些问题（opt-in）
    Live {
        /// 探测的根目录
        #[arg(long, value_name = "PATH", default_value = "/", conflicts_with = "ssh")]
        root: PathBuf,
        /// 隔着 ssh 探测另一台机器（命令也在那台上跑）
        #[arg(long, value_name = "HOST")]
        ssh: Option<String>,
        /// 只查版本号，不查连通性
        #[arg(long)]
        no_network: bool,
        /// 每次检查的超时（秒）
        #[arg(long, value_name = "SECS", default_value_t = 6)]
        timeout: u64,
        /// 有 warning 就以退出码 1 结束
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
        /// 额外采集温度/风扇瞬时读数；读取失败会终止采集。
        #[arg(long)]
        telemetry: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Hardware {
        root: PathBuf::from("/"),
        ssh: None,
        arch: None,
        strict: false,
    }) {
        Command::Hardware {
            root,
            ssh,
            arch,
            strict,
        } => {
            let source = match &ssh {
                Some(host) => Source::remote(host),
                None => Source::local(&root),
            };
            let (arch, arch_warnings) = resolve_arch(&source, &root, arch);
            let (local_root, _staging) = match resolve_local_root(&source) {
                Ok(pair) => pair,
                Err(error) => {
                    eprintln!("探测失败: {error}");
                    std::process::exit(2);
                }
            };
            let report = probe_with(&local_root, &arch);
            let mut report = report;
            report.warnings.splice(0..0, arch_warnings);
            if cli.json {
                print_json(&report);
            } else {
                print!("{}", render::human(&report));
            }
            if strict && !report.warnings.is_empty() {
                std::process::exit(1);
            }
        }
        Command::Environment { root, ssh, strict } => {
            let source = match &ssh {
                Some(host) => Source::remote(host),
                None => Source::local(&root),
            };
            let (local_root, _staging) = match resolve_local_root(&source) {
                Ok(pair) => pair,
                Err(error) => {
                    eprintln!("探测失败: {error}");
                    std::process::exit(2);
                }
            };
            let report = deviceinfo::probe_environment(&local_root);
            if cli.json {
                print_json(&report);
            } else {
                print!("{}", render::human_environment(&report));
            }
            if strict && !report.warnings.is_empty() {
                std::process::exit(1);
            }
        }
        Command::Live {
            root,
            ssh,
            no_network,
            timeout,
            strict,
        } => {
            let source = match &ssh {
                Some(host) => Source::remote(host),
                None => Source::local(&root),
            };
            let (local_root, _staging) = match resolve_local_root(&source) {
                Ok(pair) => pair,
                Err(error) => {
                    eprintln!("探测失败: {error}");
                    std::process::exit(2);
                }
            };
            let environment = deviceinfo::probe_environment(&local_root);
            let options = deviceinfo::LiveOptions {
                timeout: Duration::from_secs(timeout.max(1)),
                skip_network: no_network,
                ..Default::default()
            };
            let runner = Runner {
                root: local_root,
                host: ssh.clone(),
                timeout: options.timeout,
            };
            let report = deviceinfo::live::probe(&environment, &options, &|program, args| {
                runner.run(program, args)
            });
            if cli.json {
                print_json(&report);
            } else {
                print!("{}", render::human_live(&report));
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
            telemetry,
        } => {
            let source = match &ssh {
                Some(host) => Source::remote(host),
                None => Source::local(&root),
            };
            let (arch, arch_warnings) = resolve_arch(&source, &root, arch);
            let result = if telemetry {
                capture_with_telemetry(&source, &arch, &arch_warnings, &out, true)
            } else {
                capture(&source, &arch, &arch_warnings, &out)
            };
            match result {
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
fn capture(source: &Source, arch: &str, extra_warnings: &[String], out: &Path) -> io::Result<()> {
    capture_with_telemetry(source, arch, extra_warnings, out, false)
}

fn capture_with_telemetry(source: &Source, arch: &str, extra_warnings: &[String], out: &Path, telemetry: bool) -> io::Result<()> {
    // 不把新输入叠加到旧夹具上：源端消失的设备、库和标签必须随之消失。
    if out.exists() && fs::read_dir(out)?.next().transpose()?.is_some() {
        return Err(io::Error::other(format!(
            "输出目录 {} 非空，请采集到新目录，再比较或替换旧夹具",
            out.display()
        )));
    }
    // 远端先镜像成本地临时树，之后一切都按本地处理——这样本地采集的路径一字未变，
    // 远端采集只是多了一步落地。
    let staging = TempTree::new()?;
    let root = match source {
        Source::Local(root) => root.clone(),
        Source::Remote(_) => {
            mirror_with_telemetry(source, &staging.path, telemetry)?;
            staging.path.clone()
        }
    };

    // 采集源不对时**必须报错**，不能写出一份空壳夹具。
    //
    // 这条守卫来自一个真实的失败模式：`ssh` 不可用（没装、认证失败、主机名敲错）时，
    // 列目录全返回空，于是采集**不报错**、写出一份几乎空的夹具、还说"已采集"。
    // `--root` 指错地方同理。而夹具是要进回归测试的——**一份空壳夹具比没有夹具更坏**，
    // 它会让后来的所有探测变更都"通过"。
    // 夹具不该大：它的内容是"探测读了什么"，全是小文本文件。
    // 一旦混进了要复制内容的大文件（可执行文件、模型、数据库），这里会拦住——
    // 实测环境探测把 `/usr/bin/podman`（45 MB）当内容复制过，夹具从 33 KB 涨到 428 MB，
    // 而且**没有任何测试会红**。
    for required in ESSENTIAL_FILES {
        if !root.join(required).is_file() {
            return Err(io::Error::other(format!(
                "{} 里没有 {required}：采集源不对，或者远端没取到东西。\
                 拒绝写出一份空壳夹具。",
                source.describe()
            )));
        }
    }

    capture_from_root(&root, arch, extra_warnings, out, telemetry)?;
    refuse_if_oversized(out)
}

/// "顺手带上内容"的大小门槛。
///
/// 超过它的一律只占位：实测环境探测把 `/usr/bin/podman`（45 MB）和 docker（42 MB）
/// 当内容复制过，夹具从 33 KB 涨到 428 MB。
///
/// 门槛取得很小（8 KB）是因为**只有壳脚本的判据需要内容**，而壳脚本就是几百字节
/// （实测 `podman-docker` 的 `/usr/bin/docker` 是 228 字节）。取太大会把无关的小启动器
/// 也抄进夹具：本机的 `llama-server` 是个 14 KB 的启动器，六份就是 86 KB，而探测根本不读它。
///
/// 阈值选错的后果是**响亮的**：壳脚本一旦落到占位那一侧，夹具里的环境报告就与真机不符，
/// 夹具测试会红——所以这个数字可以按实际情况调。
const SMALL_FILE_BYTES: u64 = 8 * 1024;

/// 夹具的大小上限。所有正常夹具都在 1 MB 以内（全是小文本）。
const FIXTURE_SIZE_LIMIT_BYTES: u64 = 8 * 1024 * 1024;

/// 超过上限就报错。理由见 [`capture`] 里的那道守卫。
fn refuse_if_oversized(out: &Path) -> io::Result<()> {
    let mut total = 0_u64;
    let mut stack = vec![out.to_path_buf()];
    let mut biggest: Option<(u64, PathBuf)> = None;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            total += meta.len();
            if biggest.as_ref().is_none_or(|(size, _)| meta.len() > *size) {
                biggest = Some((meta.len(), path));
            }
        }
    }
    if total <= FIXTURE_SIZE_LIMIT_BYTES {
        return Ok(());
    }
    let hint = biggest
        .map(|(size, path)| format!("，最大的是 {}（{} 字节）", path.display(), size))
        .unwrap_or_default();
    Err(io::Error::other(format!(
        "夹具写到 {total} 字节，超过上限 {FIXTURE_SIZE_LIMIT_BYTES}{hint}；\
         采集清单里多半混进了该只放占位的东西（可执行文件？）"
    )))
}

/// 采集前必须存在的文件。理由见 [`capture`] 里的守卫。
const ESSENTIAL_FILES: [&str; 2] = ["proc/cpuinfo", "proc/meminfo"];

/// 得到一个**本地**的探测根。
///
/// 本地来源直接用自己；远端来源先镜像成一棵本地临时树。暂存树随返回值一起交出去，
/// 调用方 drop 它时就自动删掉——**必须持有它**，否则临时目录会在探测之前就被清了。
fn resolve_local_root(source: &Source) -> io::Result<(PathBuf, Option<TempTree>)> {
    match source {
        Source::Local(root) => Ok((root.clone(), None)),
        Source::Remote(_) => {
            let staging = TempTree::new()?;
            mirror(source, &staging.path)?;
            Ok((staging.path.clone(), Some(staging)))
        }
    }
}

/// 把远端的东西落到本地临时树。
///
/// 只拉探测**读**的那些文件，加上库目录的**文件名**——库内容对夹具毫无用处，
/// 而 `LibraryIndex` 只看名字。
fn mirror(source: &Source, stage: &Path) -> io::Result<()> {
    mirror_with_telemetry(source, stage, false)
}
fn mirror_with_telemetry(source: &Source, stage: &Path, telemetry: bool) -> io::Result<()> {
    let plan = if telemetry {
        capture_plan_with_telemetry(source, true)?
    } else {
        capture_plan(source)?
    };
    // 一次 ssh 把全部内容取回来，而不是每个文件开一次连接
    // 两批分开传：**内容一定要**的，和**只要"在不在"**的。
    // 后者如果混进"要内容"那批，远端会把 `/usr/bin/podman`（45 MB）之类整个传回来。
    let mut wanted = plan.files.clone();
    wanted.extend(plan.databases.iter().cloned());
    let mut existence = plan.existence_only.clone();
    existence.extend(plan.directories.iter().cloned());
    source.prefetch(&wanted, &existence)?;
    for rel in &plan.directories {
        match source.kind(rel) {
            EntryKind::Directory => fs::create_dir_all(stage.join(rel))?,
            EntryKind::File | EntryKind::Other => write_file(&stage.join(rel), b"")?,
            EntryKind::Missing => {},
        }
    }

    for rel in plan.existence_only.iter() {
        let kind = source.kind(rel);
        if kind == EntryKind::Missing {
            continue;
        }
        // **小文件顺手带上内容**：壳脚本的判据要读文件内容（`/usr/bin/docker` 是不是
        // 一个调用 podman 的脚本），而占位空文件会让夹具判不出来、于是夹具与真机不一致。
        // 大文件（真二进制，动辄几十 MB）当然还是只占位。
        let content = match kind {
            EntryKind::File => source
                .read(rel)
                .filter(|bytes| bytes.len() as u64 <= SMALL_FILE_BYTES)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        write_file(&stage.join(rel), &content)?;
    }

    for rel in plan.files.iter().chain(plan.databases.iter()) {
        let raw = match source.kind(rel) {
            // 设备节点（`/dev/accel/accel0` 这类字符设备）取不得内容：直接读会阻塞。
            // 夹具只需要"它存在"这个事实。
            EntryKind::Other | EntryKind::Directory => Vec::new(),
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
        // macOS clock resolution can give concurrent captures the same timestamp.
        // Keep per-process trees distinct so one capture cannot overwrite another.
        static NEXT_TREE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT_TREE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|taken| taken.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "deviceinfo-mirror-{}-{stamp}-{sequence}",
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
fn capture_from_root(root: &Path, arch: &str, extra_warnings: &[String], out: &Path, telemetry: bool) -> io::Result<()> {
    let source = Source::local(root);
    let mut report = probe_with(root, arch);
    // 架构是猜来的（或调用方指定的）时，报告里要跟着一句解释——否则夹具里那个
    // 架构值看起来就像探测出来的事实。
    report.warnings.splice(0..0, extra_warnings.iter().cloned());
    let plan = capture_plan_with_telemetry(&source, telemetry)?;

    // Preserve empty observed firmware/class directories without copying binary tables.
    for rel in &plan.directories {
        match source.kind(rel) {
            EntryKind::Directory => fs::create_dir_all(out.join(rel))?,
            EntryKind::File | EntryKind::Other => write_file(&out.join(rel), b"")?,
            EntryKind::Missing => {},
        }
    }

    // 0) **只需要"存在"**的输入：夹具里放空占位。可执行文件动辄几十 MB，
    //    复制内容会让夹具爆掉（真的发生过：428 MB）。
    for rel in &plan.existence_only {
        let from = match deviceinfo::resolve_path_in_root(root, rel) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            // **这一步只要"存在"这个事实**，所以穿不过去也算数：`/run/podman/podman.sock`
            // 就在一个 root-only 的目录里，以非 root 采本机时必然撞上。远端那边同样是
            // 这么处理的（这类输入根本不去取内容）——两边判定必须一致，否则"本机能采、
            // 远端也能采"这件事只对其中一边成立。
            //
            // 注意**只对"只要存在"这一类放宽**：下面阶段 1（要内容的文件）读不到仍然是硬错误，
            // 否则会得到一个"少了个文件"的夹具，而它探测出来和真机不一样。
            // 而且**不能写占位**：写下去，夹具树里就"有"它了，而真机上以同一个用户
            // 探测是看不到的——于是夹具探测出来的和真机不一样（测试当场抓到过）。
            // 夹具要冻结的是"探测会看到什么"，探测看不到的就不该出现在树里。
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => continue,
            Err(error) => return Err(error),
        };
        // 同 `mirror`：小文件带内容（壳脚本判据要读它），大文件只占位
        let content = match fs::metadata(&from) {
            Ok(meta) if meta.is_file() && meta.len() <= SMALL_FILE_BYTES => {
                fs::read(&from).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        write_file(&out.join(rel), &content)?;
    }

    // 1) 探测会读到的文件
    for rel in &plan.files {
        let from = match deviceinfo::resolve_path_in_root(root, rel) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
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
                // **错误里必须带路径**：报一句 "Permission denied (os error 13)" 而不说
                // 是哪个文件，唯一的线索就没了（本机以非 root 采 `/` 时会撞上 root-only
                // 的文件，而"哪个文件"决定了是"该用 sudo"还是"计划里不该有它"）。
                let raw = fs::read(&from).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("读 {} 失败：{error}", from.display()),
                    )
                })?;
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

    // 不完整栈中已有的组件也要保留，否则夹具会额外报告缺件。
    // 保持目录位置：ICD 注册可能引用库的绝对路径。
    for path in deviceinfo::runtime_library_inputs(root) {
        write_file(&out.join(path), b"")?;
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
        "includes_thermal_telemetry": telemetry,
        "note": "由 `deviceinfo capture` 生成。expected.json 是采集当时的探测结果快照——\
                 探测行为变化时它会失败，人工看 diff 决定是改了行为还是修了 bug。",
    });
    write_file(
        &out.join("meta.json"),
        format!("{}\n", serde_json::to_string_pretty(&meta)?).as_bytes(),
    )?;
    let to_json = |value: &serde_json::Value| -> io::Result<String> {
        serde_json::to_string_pretty(value)
            .map(|text| format!("{text}\n"))
            .map_err(|error| io::Error::other(format!("序列化失败: {error}")))
    };
    // 硬件报告与环境报告**都要快照**：只冻结硬件的话，环境探测的行为变化
    // （比如镜像时把目录写成了文件、于是 init 永远报 null）不会被任何测试发现。
    write_file(
        &out.join("expected.json"),
        to_json(&serde_json::to_value(&report).map_err(|error| {
            io::Error::other(format!("报告序列化失败: {error}"))
        })?)?
        .as_bytes(),
    )?;
    write_file(
        &out.join("expected-environment.json"),
        to_json(&serde_json::to_value(deviceinfo::probe_environment(root)).map_err(
            |error| io::Error::other(format!("环境报告序列化失败: {error}")),
        )?)? 
        .as_bytes(),
    )?;
    Ok(())
}

/// 采集清单：要复制的文件，以及要重建的软链。
///
/// **这份清单是探测逻辑的镜像**：探测读了什么，这里就得复制什么，两边要一起改。
/// 宁可多列几个不存在的路径（`fs::metadata` 会跳过），也不要漏。
struct CapturePlan {
    files: Vec<String>,
    directories: Vec<String>,
    /// **只需要"存在"**的路径：只放空占位，不复制内容。
    ///
    /// 必须和 `files` 分开：环境探测要问"`/usr/bin/podman` 在不在"，而那是 45 MB
    /// 的二进制——复制内容会让夹具从 33 KB 涨到 428 MB（真的发生过）。
    existence_only: Vec<String>,
    /// 只复制到暂存树的"探测输入数据库"（`pci.ids`）。
    ///
    /// 必须和 `files` 分开：**暂存树要它**（否则 `pci::lookup` 查不到名字，
    /// `hardware --ssh` 会把 `0x46a6` 原样印出来），但**夹具里不能放整份**
    /// （1.6 MB，而且会随系统 hwdata 更新而变）。夹具只留用到的那几条，
    /// 那一步由 `capture_from_root` 单独做。
    databases: Vec<String>,
    /// `(相对路径, 软链指向的字符串)`。指向什么不重要，探测只取末段做驱动名。
    symlinks: Vec<(String, String)>,
}

fn capture_plan(source: &Source) -> io::Result<CapturePlan> {
    capture_plan_with_telemetry(source, false)
}
fn capture_plan_with_telemetry(source: &Source, telemetry: bool) -> io::Result<CapturePlan> {
    // CPU 那部分**直接用库里的清单**，不在这里重抄一遍：抄一遍就会漂移，
    // 而漂移的后果（远端夹具静默少一个输入）很难发现。
    let mut files: Vec<String> = CPU_SHARED_INPUTS.iter().map(|path| path.to_string()).collect();
    files.extend(deviceinfo::system::INPUTS.iter().map(|path| path.to_string()));
    files.extend(deviceinfo::platform::inputs());
    let mut directories: Vec<String> = deviceinfo::platform::INPUT_DIRS.iter().map(|path| path.to_string()).collect();
    let mut symlinks = Vec::new();
    let mut existence_only = Vec::new();
    // 探测会读 pci.ids（`pci::lookup`），所以要镜像进暂存树——但写夹具时另走一条路
    let databases: Vec<String> = PCI_DATABASE_PATHS.iter().map(|path| path.to_string()).collect();
    let mut lists = ListCache::new(source);

    if telemetry {
        directories.extend(deviceinfo::thermal::INPUT_DIRS.map(str::to_owned));
        lists.ensure(&deviceinfo::thermal::INPUT_DIRS.map(str::to_owned))?;
        let thermal_dirs = deviceinfo::thermal::input_dirs(&|dir| lists.entries(dir).iter().map(|entry| entry.name.clone()).collect());
        lists.ensure(&thermal_dirs)?;
        directories.extend(thermal_dirs);
        files.extend(deviceinfo::thermal::inputs(&|dir| lists.entries(dir).iter().map(|entry| entry.name.clone()).collect()));
    }

    lists.ensure(&deviceinfo::soc::INPUT_DIRS.map(str::to_owned))?;
    files.extend(deviceinfo::soc::inputs(&|dir| {
        lists.entries(dir).iter().map(|e| e.name.clone()).collect()
    }));
    const BLOCK_DIR: &str = deviceinfo::storage::BLOCK_DIR;
    lists.ensure(&[BLOCK_DIR.to_string()])?;
    let blocks: Vec<_> = lists
        .entries(BLOCK_DIR)
        .iter()
        .map(|e| e.name.clone())
        .collect();
    lists.ensure(&deviceinfo::storage::input_dirs(&blocks))?;
    let (storage_files, storage_existence) = deviceinfo::storage::inputs(&blocks, &|dir| {
        lists.entries(dir).iter().map(|e| e.name.clone()).collect()
    });
    files.extend(storage_files);
    existence_only.extend(storage_existence);
    for name in &blocks {
        files.extend(
            deviceinfo::storage_health::MMC_INPUTS
                .iter()
                .map(|leaf| format!("{BLOCK_DIR}/{name}/{leaf}")),
        );
    }

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
        for rel in CPU_PER_CORE_INPUTS {
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
            "freq/hw_min_freq",
            "freq/hw_efficient_freq",
            "sched_mode",
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

    // card 目录与它的 device 目录都要：**频率节点分居两处**（见下面 i915 那段注释）
    let card_dirs: Vec<String> = lists
        .numeric(DRM_DIR, "card")
        .iter()
        .map(|card| format!("{DRM_DIR}/{card}"))
        .collect();
    let card_device_dirs: Vec<String> = card_dirs
        .iter()
        .map(|card_dir| format!("{card_dir}/device"))
        .collect();
    // 阶段二：所有 card 的 device 目录 + 各自的 drm/ 一次列完
    let drm_subdirs: Vec<String> = card_device_dirs
        .iter()
        .map(|device| format!("{device}/drm"))
        .collect();
    lists.ensure(&card_device_dirs)?;
    lists.ensure(&drm_subdirs)?;

    let mut tile_dirs = Vec::new();
    for card_dir in &card_dirs {
        let device = format!("{card_dir}/device");
        for rel in [
            "vendor",
            "device",
            "mem_info_vram_total",
            "mem_info_vram_used",
            "freq/hw_max_freq",
            "npu_max_frequency_mhz",
            "freq/hw_min_freq",
            "freq/hw_efficient_freq",
            "sched_mode",
            "of_node/compatible",
            // xe 的频率在 `<device>/tileN/gtN/freq0/` 下（见阶段三）
        ] {
            files.push(format!("{device}/{rel}"));
        }
        // **i915 的频率在 card 目录下，不在 `<card>/device` 下**（实测于 r1）：
        // 弄错层级不会报错，只会永远读不到。
        for rel in [
            "gt/gt0/rps_max_freq_mhz",
            "gt/gt0/rps_cur_freq_mhz",
            "gt_max_freq_mhz",
            "gt_cur_freq_mhz",
        ] {
            files.push(format!("{card_dir}/{rel}"));
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
        for tile in lists.numeric(&device, "tile") {
            tile_dirs.push(format!("{device}/{tile}"));
        }
        device_dirs.push(device);
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

    // 软件环境：**输入清单由库提供**（`environment::inputs`），这里不重抄一份。
    // 硬件那边因为重抄栽过一次（`acpi_cppc/highest_perf` 漏加 → 远端夹具静默少一个输入）。
    let mut environment_dirs = deviceinfo::environment::input_dirs();
    environment_dirs.push(OPENCL_VENDOR_DIR.to_string());
    lists.ensure(&environment_dirs)?;
    for input in deviceinfo::environment::inputs(&|dir| {
        lists
            .entries(dir)
            .iter()
            .map(|entry| entry.name.clone())
            .collect()
    }) {
        match input {
            deviceinfo::environment::Input::Content(path) => files.push(path),
            deviceinfo::environment::Input::Existence(path) => existence_only.push(path),
        }
    }

    for entry in lists.entries(OPENCL_VENDOR_DIR) {
        if !entry.is_dir && entry.name.ends_with(".icd") {
            files.push(format!("{OPENCL_VENDOR_DIR}/{}", entry.name));
        }
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
    link_paths.extend(
        blocks.iter().map(|name| format!("{BLOCK_DIR}/{name}/device/subsystem")),
    );
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

    Ok(CapturePlan {
        files,
        directories,
        existence_only,
        databases,
        symlinks,
    })
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

/// 在目标机器上跑命令：本地就直接起进程，远端就 ssh 过去起。
///
/// 两条路都**主动兜一层超时**（真杀进程）。库那边只能靠 `curl --max-time` 之类的
/// 自保参数，那是被动的；这里主动兜底，免得一个卡住的检查拖死整轮探测。
/// **不持有 `&Source`**：那个类型里有 `RefCell`（采集时的缓存），于是不是 `Sync`，
/// 而连通性检查要并发跑。这里只拿解析好的两样东西——根目录和主机名。
struct Runner {
    root: PathBuf,
    /// `Some(host)` 表示命令要在那台上跑，`None` 表示本机。
    host: Option<String>,
    timeout: Duration,
}

impl Runner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<String> {
        match &self.host {
            // 本地：直接执行**机器上的路径**（`/usr/bin/podman`）。拿夹具当 root 时
            // 那里是空的占位文件，会干净地失败——这是对的，实时探测本来就只对本机有意义。
            None => {
                // **绝对路径**（`/usr/bin/podman`，环境报告给的是机器上的路径）要拼探测根；
                // **裸名字**（`curl`/`wget`，检查工具）走 PATH。
                // 一律拼根会把 curl 变成 `/curl`，然后报出一个完全误导的"没法查"。
                let executable = match program.strip_prefix('/') {
                    Some(relative) => self.root.join(relative),
                    None => PathBuf::from(program),
                };
                let mut command = ProcessCommand::new(executable);
                command.args(args);
                run_with_timeout(command, self.timeout)
            }
            // 远端：整条命令交给 ssh，在那台上执行
            Some(host) => {
                let mut words: Vec<String> = vec![source::quoted(program)];
                words.extend(args.iter().map(|arg| source::quoted(arg)));
                ssh_exec(host, &words.join(" "), self.timeout + Duration::from_secs(5))
            }
        }
    }
}

/// 跑子进程，**超时就真杀**。
///
/// 前提：输出要小（`--version`、`curl -w` 都是几十字节）。这里不读管道，所以在子进程
/// 退出前写满管道缓冲会让它卡住——实时探测的命令都满足这个前提，但换命令时要留意。
fn run_with_timeout(mut command: ProcessCommand, timeout: Duration) -> io::Result<String> {
    use std::time::Instant;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("超时（{} 秒）", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        // 取**最后一行**非空 stderr——报错通常在那儿，而前面可能是几十行无关输出
        // （实测 `llama-bench --version` 先打印了一屏 Vulkan 设备信息）。
        // 再限长：这些字要进人看的报告，不是日志。
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .unwrap_or("")
            .chars()
            .take(160)
            .collect::<String>();
        // POSIX shell 的 127 表示远端命令不存在；保留结构化错误供 live 分类。
        let kind = if output.status.code() == Some(127) {
            io::ErrorKind::NotFound
        } else {
            io::ErrorKind::Other
        };
        return Err(io::Error::new(
            kind,
            format!(
                "退出码 {:?}{}",
                output.status.code(),
                if last.is_empty() {
                    String::new()
                } else {
                    format!(": {last}")
                }
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 目标架构。**从文件里读不出来**（`/proc` 和 `/sys` 里都没有这个字段），所以只能
/// 要么调用方给，要么问目标机——而架构是"能不能跑"最硬的过滤条件，猜错的代价是部署错东西。
fn resolve_arch(source: &Source, root: &Path, explicit: Option<String>) -> (String, Vec<String>) {
    match source {
        // 隔着一台机器却拿本机架构当默认值，会**静默产出一个看似合理的错答案**
        // （真的发生了：aarch64 的机器被记成 x86_64），而报告里没有任何东西提示它。
        Source::Remote(_) => {
            let host = source.host().to_string();
            let reported = ssh_exec(&host, "uname -m", Duration::from_secs(15))
                .map(|output| output.trim().to_string())
                .ok()
                .filter(|arch| !arch.is_empty());
            match (explicit, reported) {
                // 显式的 `--arch` 是有意的（比如 32 位用户态跑在 64 位内核上），
                // 所以**不静默覆盖**，但要把不一致说出来。
                (Some(explicit), Some(reported)) if explicit != reported => (
                    explicit.clone(),
                    vec![format!(
                        "指定的架构 {explicit} 与目标机 `uname -m` 报的 {reported} 不一致，报告里用的是指定的那个"
                    )],
                ),
                (Some(explicit), _) => (explicit, Vec::new()),
                (None, Some(reported)) => (reported, Vec::new()),
                (None, None) => {
                    let fallback = std::env::consts::ARCH.to_string();
                    (
                        fallback.clone(),
                        vec![format!(
                            "问不出目标机的架构（`uname -m` 没能执行），退回了本机的 {fallback}——这个值很可能是错的，用 --arch 指定"
                        )],
                    )
                }
            }
        }
        // 本机探测时，编译进来的架构是对的：二进制就跑在这台上。
        Source::Local(_) => {
            let mut warnings = Vec::new();
            let guessed = explicit.is_none();
            let arch = explicit.unwrap_or_else(|| std::env::consts::ARCH.to_string());
            // 只有**默认值**才算猜；调用方指定了架构就没有猜的成分了。
            if guessed && root != Path::new("/") {
                warnings.push(format!(
                    "架构用的是本机的 {arch}；`--root` 指向的是另一棵树，如果它来自别的机器，请用 --arch 指定"
                ));
            }
            (arch, warnings)
        }
    }
}

/// 隔着 ssh 在目标机上跑一条命令，返回标准输出。
///
/// 采集本来就要读目标机的文件（`run_with_stdin` 里那些脚本），这里只是借同一条通道
/// 多问一句。**库那边不碰执行**：`deviceinfo` 里的实时探测也是由调用方提供 runner 的。
fn ssh_exec(host: &str, command: &str, timeout: Duration) -> io::Result<String> {
    let mut ssh = ProcessCommand::new("ssh");
    ssh.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--"])
        .arg(host)
        .arg(command);
    run_with_timeout(ssh, timeout)
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

    /// `pci.ids` 必须进采集清单。
    ///
    /// 它是探测的**输入**（`pci::lookup` 读它），所以暂存树里必须有——否则
    /// `hardware --ssh` 会把 `0x46a6` 原样印出来。但夹具里只该留用到的那几条，
    /// 所以它走 `databases` 而不是 `files`。这个洞在只跑 ARM 机器时看不见
    /// （那些加速器没有 PCI 标识），换到一台 x86 机器才暴露。
    #[test]
    fn the_capture_plan_includes_the_pci_database() {
        let root = std::env::temp_dir().join(format!("deviceinfo-plan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proc")).unwrap();
        let plan = capture_plan(&Source::local(&root)).expect("本地规划不会失败");
        assert!(
            plan.databases.iter().any(|path| path.ends_with("pci.ids")),
            "{:?}",
            plan.databases
        );
        // 它不能混进 files：那会把整份 1.6 MB 的 pci.ids 抄进夹具
        assert!(
            !plan.files.iter().any(|path| path.ends_with("pci.ids")),
            "pci.ids 该在 databases 里，不是 files"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 标签是**唯一人为声明的输入**，也是最容易在采集清单里被漏掉的东西——
    /// 漏了的话远端夹具会报"没有标签"，而任务路由会因此永远绕过这台机器，
    /// 且没有任何测试会红。这条端到端钉住它。
    #[test]
    fn capture_preserves_declared_tags() {
        let root = std::env::temp_dir().join(format!("deviceinfo-tagcapture-{}", std::process::id()));
        let out = std::env::temp_dir().join(format!("deviceinfo-tagout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&out);

        // 一棵最小的"机器"：采集守卫要求 proc/cpuinfo 与 proc/meminfo 在
        fs::create_dir_all(root.join("proc")).unwrap();
        fs::write(root.join("proc/cpuinfo"), "processor\t: 0\nmodel name\t: x\n").unwrap();
        fs::write(root.join("proc/meminfo"), "MemTotal: 1000 kB\n").unwrap();
        fs::create_dir_all(root.join("etc/deviceinfo")).unwrap();
        fs::write(
            root.join("etc/deviceinfo/tags.conf"),
            "# 角色\nAlways_On   powersave\n",
        )
        .unwrap();

        capture(&Source::local(&root), "x86_64", &[], &out).expect("采集应当成功");

        // 夹具本身要能探测出来
        let report = deviceinfo::probe_environment(&out);
        assert!(report.always_on(), "{:#?}", report.declared_tags);
        assert!(report.powersave());
        // 期望值里也要有（否则夹具测试不会因为标签丢失而失败）
        let expected = fs::read_to_string(out.join("expected-environment.json")).unwrap();
        assert!(expected.contains("always-on"), "{expected}");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&out);
    }

    /// 采集源不对时**必须报错**。以前它会"成功"写出一份空壳夹具——而空壳夹具比没有
    /// 夹具更坏：它会让后来的所有探测变更都"通过"。
    #[test]
    fn capture_refuses_an_empty_source() {
        let out = std::env::temp_dir().join(format!("deviceinfo-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&out);

        let source = Source::local(Path::new("/definitely/not/here"));

        let error = capture(&source, "x86_64", &[], &out).unwrap_err();
        assert!(error.to_string().contains("proc/cpuinfo"), "{error}");
        assert!(
            !out.exists(),
            "失败时不该留下半个夹具目录——那比没有更坏"
        );

        let _ = fs::remove_dir_all(&out);
    }

    /// 架构猜错是**静默**的错：报告里看不出那个值是探测来的还是默认值，所以猜的时候必须出声。
    #[test]
    fn a_guessed_arch_must_say_so() {
        // 本机、根就是 "/"：编译进来的架构是对的，不该有噪声
        let (arch, warnings) = resolve_arch(&Source::local(Path::new("/")), Path::new("/"), None);
        assert_eq!(arch, std::env::consts::ARCH);
        assert!(warnings.is_empty(), "{warnings:?}");

        // 换了一棵树：它可能是别的机器的夹具，此时的默认值就只是猜
        let tree = Path::new("/tmp/another-machine");
        let (_, warnings) = resolve_arch(&Source::local(tree), tree, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("--arch"), "{warnings:?}");

        // 显式指定就不算猜
        let (arch, warnings) = resolve_arch(&Source::local(tree), tree, Some("aarch64".into()));
        assert_eq!(arch, "aarch64");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    fn minimal_machine() -> TempTree {
        let tree = TempTree::new().unwrap();
        write_file(
            &tree.path.join("proc/cpuinfo"),
            b"processor: 0\nmodel name: x\n",
        )
        .unwrap();
        write_file(&tree.path.join("proc/meminfo"), b"MemTotal: 1000 kB\n").unwrap();
        tree
    }

    fn assert_hardware_snapshot_matches(out: &Path) {
        let expected: deviceinfo::HardwareReport =
            serde_json::from_slice(&fs::read(out.join("expected.json")).unwrap()).unwrap();
        assert_eq!(probe_with(out, "x86_64"), expected);
    }

    #[test]
    fn a_shell_command_not_found_is_reported_with_a_structured_error_kind() {
        let mut command = ProcessCommand::new("sh");
        command.args(["-c", "echo 'sh: curl: not found' >&2; exit 127"]);
        let error = run_with_timeout(command, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("curl: not found"));
    }

    #[test]
    fn capture_preserves_components_of_an_incomplete_runtime() {
        let source = minimal_machine();
        let out = TempTree::new().unwrap();
        for (path, content) in [
            ("dev/accel/accel0", ""),
            ("sys/class/accel/accel0/device/vendor", "0x8086\n"),
            ("sys/class/accel/accel0/device/device", "0x643e\n"),
            ("sys/class/accel/accel0/device/freq/hw_max_freq", "1900\n"),
            ("sys/class/accel/accel0/device/freq/hw_min_freq", "650\n"),
            ("sys/class/accel/accel0/device/freq/hw_efficient_freq", "950\n"),
            ("sys/class/accel/accel0/device/sched_mode", "HW\n"),
            ("usr/lib/x86_64-linux-gnu/libze_loader.so.1", ""),
            ("usr/lib/x86_64-linux-gnu/libze_intel_npu.so", ""),
        ] {
            write_file(&source.path.join(path), content.as_bytes()).unwrap();
        }
        capture(&Source::local(&source.path), "x86_64", &[], &out.path).unwrap();
        assert_hardware_snapshot_matches(&out.path);
        assert!(
            out.path
                .join("usr/lib/x86_64-linux-gnu/libze_loader.so.1")
                .is_file()
        );
        let report = probe_with(&out.path, "x86_64");
        assert_eq!(report.accelerators[0].max_freq_mhz, Some(1900));
        assert_eq!(report.accelerators[0].min_freq_mhz, Some(650));
        assert_eq!(report.accelerators[0].efficient_freq_mhz, Some(950));
        assert_eq!(report.accelerators[0].scheduling_mode.as_deref(), Some("HW"));
        match &report.accelerators[0].runtime {
            deviceinfo::RuntimeStatus::Incomplete { missing, .. } => assert_eq!(missing.len(), 1),
            status => panic!("预期只缺编译器: {status:?}"),
        }
    }

    #[test]
    fn capture_preserves_heterogeneous_cpu_features_and_sparse_ids() {
        let source = minimal_machine();
        let out = TempTree::new().unwrap();
        write_file(
            &source.path.join("proc/cpuinfo"),
            b"processor: 2\nflags: aes avx2\nprocessor: 7\nflags: aes\n",
        )
        .unwrap();
        capture(&Source::local(&source.path), "x86_64", &[], &out.path).unwrap();
        assert_hardware_snapshot_matches(&out.path);
        let original = probe_with(&source.path, "x86_64");
        let captured = probe_with(&out.path, "x86_64");
        assert_eq!(original.cpu, captured.cpu);
        assert_eq!(captured.cpu.common_features, Some(vec!["aes".into()]));
        assert_eq!(captured.cpu.feature_groups[1].cpus, [7]);
    }

    #[test]
    fn capture_preserves_os_identity_and_kernel_without_host_paths() {
        let source = minimal_machine();
        let out = TempTree::new().unwrap();
        for (path, content) in [
            ("etc/os-release", "ID=ubuntu\nID_LIKE=debian\nPRETTY_NAME=\"Ubuntu Linux\"\nVERSION_ID=24.04\n"),
            ("usr/lib/os-release", "ID=default\n"),
            ("proc/sys/kernel/osrelease", "6.8.0-test\n"),
        ] {
            write_file(&source.path.join(path), content.as_bytes()).unwrap();
        }
        capture(&Source::local(&source.path), "x86_64", &[], &out.path).unwrap();
        let report = deviceinfo::probe_environment(&out.path);
        assert_eq!(report, deviceinfo::probe_environment(&source.path));
        let expected: serde_json::Value = serde_json::from_slice(
            &fs::read(out.path.join("expected-environment.json")).unwrap(),
        ).unwrap();
        assert_eq!(serde_json::to_value(&report).unwrap(), expected);
        let os = report.operating_system.as_ref().unwrap();
        assert_eq!(os.id.as_deref(), Some("ubuntu"));
        assert_eq!(os.version_id.as_deref(), Some("24.04"));
        assert_eq!(os.id_like, ["debian"]);
        assert_eq!(os.source, Path::new("/etc/os-release"));
        assert_eq!(report.kernel_release.as_deref(), Some("6.8.0-test"));
        let text = deviceinfo::render::human_environment(&report);
        assert!(text.contains("Ubuntu Linux"), "{text}");
        assert!(text.contains("6.8.0-test"), "{text}");
    }

    #[test]
    fn capture_preserves_lightweight_system_identity_and_state() {
        let source = minimal_machine();
        let out = TempTree::new().unwrap();
        for (path, content) in [
            ("etc/os-release", "ID=fixture-only\n"),
            ("proc/sys/kernel/osrelease", "fixture-kernel\n"),
            ("proc/sys/kernel/hostname", "fixture-host\n"),
            ("proc/stat", "cpu 10 20 30 40 50 60 70 80 9 8\n"),
            ("proc/loadavg", "1.0 2.0 3.0 1/100 4\n"),
            ("proc/uptime", "123.5 400.0\n"),
        ] {
            write_file(&source.path.join(path), content.as_bytes()).unwrap();
        }
        capture(&Source::local(&source.path), "x86_64", &[], &out.path).unwrap();
        assert_eq!(
            deviceinfo::probe_system_with(&source.path, "x86_64"),
            deviceinfo::probe_system_with(&out.path, "x86_64"),
        );
        let options = deviceinfo::SystemSampleOptions::default();
        assert_eq!(
            deviceinfo::sample_system_state_with(&source.path, &options),
            deviceinfo::sample_system_state_with(&out.path, &options),
        );
    }

    #[test]
    fn capture_preserves_soc_storage_layers_and_health_inputs() {
        let source = minimal_machine();
        let out = TempTree::new().unwrap();
        for (path, content) in [
            (
                "sys/firmware/devicetree/base/compatible",
                "radxa,fixture\0rockchip,rk3588\0",
            ),
            ("sys/devices/soc0/family", "Rockchip\n"),
            ("sys/bus/soc/devices/soc0/family", "Rockchip\n"),
            ("sys/class/block/mmcblk0/dev", "179:0\n"),
            ("sys/class/block/mmcblk0/size", "4000\n"),
            ("sys/class/block/mmcblk0/removable", "0\n"),
            ("sys/class/block/mmcblk0/device/type", "MMC\n"),
            ("sys/class/block/mmcblk0/device/rev", "0x06\n"),
            ("sys/class/block/mmcblk0/device/pre_eol_info", "0x02\n"),
            ("sys/class/block/mmcblk0/device/life_time", "0x0a 0x01\n"),
            ("sys/class/block/mmcblk0/mmcblk0p1/partition", "1\n"),
            ("sys/class/block/mmcblk0p1/partition", "1\n"),
            ("sys/class/block/mmcblk0p1/dev", "179:1\n"),
            ("sys/class/block/mmcblk0p1/size", "2000\n"),
            ("sys/class/block/dm-0/dev", "253:0\n"),
            ("sys/class/block/dm-0/size", "2000\n"),
            ("sys/class/block/dm-0/dm/name", "root\n"),
            (
                "proc/self/mountinfo",
                "1 1 253:0 / / rw - ext4 /dev/mapper/root rw\n",
            ),
        ] {
            write_file(&source.path.join(path), content.as_bytes()).unwrap();
        }
        fs::create_dir_all(source.path.join("sys/class/block/dm-0/slaves/mmcblk0p1")).unwrap();
        capture(&Source::local(&source.path), "x86_64", &[], &out.path).unwrap();
        assert_eq!(
            deviceinfo::probe_soc(&source.path),
            deviceinfo::probe_soc(&out.path)
        );
        assert_eq!(
            deviceinfo::probe_storage(&source.path),
            deviceinfo::probe_storage(&out.path)
        );
        let options = deviceinfo::StorageHealthOptions::default();
        assert_eq!(
            deviceinfo::sample_storage_health_with(&out.path, &options).mmc[0].state,
            deviceinfo::StorageHealthState::Unknown
        );
        assert_eq!(
            deviceinfo::sample_storage_health_with(&source.path, &options),
            deviceinfo::sample_storage_health_with(&out.path, &options)
        );
    }

    #[test]
    fn capture_preserves_coexisting_firmware_and_thermal_channels() {
        let source = minimal_machine();
        let out = TempTree::new().unwrap();
        for (path, text) in [
            ("sys/firmware/efi/fw_platform_size", "64\n"),
            ("sys/firmware/devicetree/base/model", "Q8B fixture\0"),
            ("sys/firmware/devicetree/base/compatible", "radxa,fixture\0qcom,qcs6490\0"),
            ("sys/class/dmi/id/board_version", "rev-fixture\n"),
            ("sys/class/dmi/id/bios_version", "firmware-fixture\n"),
            ("sys/class/hwmon/hwmon0/name", "fan-fixture\n"),
            ("sys/class/hwmon/hwmon0/temp1_input", "42000\n"),
            ("sys/class/hwmon/hwmon0/temp1_crit", "95000\n"),
            ("sys/class/hwmon/hwmon0/fan1_input", "2200\n"),
            ("sys/class/hwmon/hwmon0/pwm2", "128\n"),
            ("sys/class/thermal/cooling_device0/type", "pwm-fan\n"),
            ("sys/class/thermal/cooling_device0/cur_state", "2\n"),
            ("sys/class/thermal/cooling_device0/max_state", "5\n"),
        ] {
            write_file(&source.path.join(path), text.as_bytes()).unwrap();
        }
        fs::create_dir_all(source.path.join("sys/firmware/acpi/tables")).unwrap();
        let default_plan = capture_plan(&Source::local(&source.path)).unwrap();
        assert!(!default_plan.files.iter().any(|path| path.starts_with("sys/class/hwmon/")));
        capture_with_telemetry(&Source::local(&source.path), "aarch64", &[], &out.path, true).unwrap();
        assert!(out.path.join("sys/firmware/acpi/tables").is_dir());
        assert_eq!(deviceinfo::probe_platform(&source.path), deviceinfo::probe_platform(&out.path));
        assert_eq!(deviceinfo::sample_thermal(&source.path), deviceinfo::sample_thermal(&out.path));
        // Remote mirror uses the same directory marker protocol and input plan.
        let stage = TempTree::new().unwrap();
        mirror_with_telemetry(&Source::local(&source.path), &stage.path, true).unwrap();
        assert_eq!(deviceinfo::probe_platform(&source.path), deviceinfo::probe_platform(&stage.path));
        assert_eq!(deviceinfo::sample_thermal(&source.path), deviceinfo::sample_thermal(&stage.path));
    }

    #[test]
    fn capture_resolves_absolute_os_release_symlinks_in_the_target_root() {
        let source = minimal_machine();
        let out = TempTree::new().unwrap();
        write_file(&source.path.join("usr/lib/os-release"), b"ID=fixture-only\n").unwrap();
        fs::create_dir_all(source.path.join("etc")).unwrap();
        std::os::unix::fs::symlink("/usr/lib/os-release", source.path.join("etc/os-release")).unwrap();
        capture(&Source::local(&source.path), "x86_64", &[], &out.path).unwrap();
        let report = deviceinfo::probe_environment(&out.path);
        assert_eq!(report, deviceinfo::probe_environment(&source.path));
        assert_eq!(report.operating_system.unwrap().id.as_deref(), Some("fixture-only"));
    }

    #[test]
    fn capture_preserves_the_opencl_implementation_and_its_icd() {
        let source = minimal_machine();
        let out = TempTree::new().unwrap();
        for (path, content) in [
            ("sys/class/drm/card0/device/vendor", "0x8086\n"),
            ("sys/class/drm/card0/device/device", "0x64a0\n"),
            ("sys/class/drm/card0/device/drm/renderD128", ""),
            ("dev/dri/renderD128", ""),
            ("usr/lib/x86_64-linux-gnu/libOpenCL.so.1", ""),
            ("usr/lib/x86_64-linux-gnu/libigdrcl.so", ""),
            (
                "etc/OpenCL/vendors/intel.icd",
                "/usr/lib/x86_64-linux-gnu/libigdrcl.so\n",
            ),
        ] {
            write_file(&source.path.join(path), content.as_bytes()).unwrap();
        }
        capture(&Source::local(&source.path), "x86_64", &[], &out.path).unwrap();
        assert_hardware_snapshot_matches(&out.path);
        assert!(probe_with(&out.path, "x86_64").has_usable_gpu());
    }

    #[test]
    fn capture_refuses_to_mix_new_inputs_into_an_existing_fixture() {
        let source = minimal_machine();
        let out = TempTree::new().unwrap();
        write_file(
            &source.path.join("etc/deviceinfo/tags.conf"),
            b"always-on\n",
        )
        .unwrap();
        capture(&Source::local(&source.path), "x86_64", &[], &out.path).unwrap();
        let previous = fs::read(out.path.join("expected-environment.json")).unwrap();
        fs::remove_file(source.path.join("etc/deviceinfo/tags.conf")).unwrap();
        let error = capture(&Source::local(&source.path), "x86_64", &[], &out.path).unwrap_err();
        assert!(error.to_string().contains("非空"), "{error}");
        assert_eq!(
            fs::read(out.path.join("expected-environment.json")).unwrap(),
            previous
        );
        assert!(out.path.join("etc/deviceinfo/tags.conf").is_file());
        assert_hardware_snapshot_matches(&out.path);
    }
}
