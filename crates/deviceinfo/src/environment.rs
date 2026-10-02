//! 软件环境探测：**这台机器装了什么、配了什么**。
//!
//! 与 [`crate::probe`]（硬件）和 [`crate::sample_state`]（瞬时状态）并列的第三个入口。
//! 三者的区别是变化频率：硬件装了就不变；状态每秒在变；**环境装了/配了才变**——
//! 所以它可以缓存，但它不是机器本身的固有属性（不能拿来跨机器比较"谁更强"）。
//!
//! # 只读，而且尽量只读**文件**
//!
//! 这条限制是刻意的：本模块要能隔着 ssh 采到（[`crate::probe`] 那套 `Source` 抽象），
//! 也要能进夹具。所以**一概不执行命令**，只读文件与目录。
//!
//! 代价是有些东西读不到，其中最重要的是**版本号**：`docker --version` 要跑一遍；
//! 包数据库虽然能读，但在 Debian 上是一个 1 MB、装一次包就变一次的大文件
//! （`/var/lib/dpkg/status`），读完还要污染夹具。那些属于**实时探测**，
//! 见 README 的「已知未做」。
//!
//! # 为什么这件事值得单独做
//!
//! 实测四台机器，容器运行时是**四种状态**：podman / podman（不在 docker 组）/
//! docker（在组里）/ **一个都没有**；出网能力也分目标（有一台到不了 Docker Hub，
//! 靠 `daemon.json` 里的镜像源拉）。所以「统一用容器部署」这种方案**必须先探测再决定**
//! ——这就是本模块存在的理由。

use crate::sysfs::read_trimmed;
use crate::tags::{self, parse as tags_parse, TagLine};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 找可执行文件的候选目录。`bin` / `sbin` 在合并了 `/usr` 的发行版上是指向 `usr/*` 的软链。
const BIN_DIRS: [&str; 5] = ["usr/bin", "usr/local/bin", "bin", "usr/sbin", "sbin"];

/// 容器运行时，以及它可能使用的 socket。
///
/// 分开写是因为**「装了」和「在跑」是两件事**：`/usr/bin/docker` 在，不代表 daemon 起着。
const CONTAINER_RUNTIMES: [(&str, &[&str]); 3] = [
    ("docker", &["run/docker.sock", "var/run/docker.sock"]),
    ("podman", &["run/podman/podman.sock"]),
    ("nerdctl", &["run/containerd/containerd.sock"]),
];

/// podman 的 rootless socket 在每个用户的运行时目录下，所以要按编号枚举 `run/user/*`。
const ROOTLESS_SOCKET_SUFFIX: &str = "podman/podman.sock";

/// 已经装了就值得知道的推理框架 / 服务。
///
/// 这一项的用途是**别重复部署**：机器上已经有 `llama-server` 的话，
/// 环境助手就不该再去下一个 llama.cpp。
const INFERENCE_TOOLS: [&str; 7] = [
    "llama-server",
    "llama-cli",
    "llama-bench",
    "ollama",
    "vllm",
    "ovms",
    "text-generation-launcher",
];

/// 包管理器（只要能找到可执行文件就认）。
const PACKAGE_MANAGERS: [&str; 8] = [
    "pacman",
    "apt-get",
    "dnf",
    "zypper",
    "apk",
    "xbps-install",
    "emerge",
    "nix",
];

/// 容器镜像源的候选配置文件。
///
/// 关键是**分两层**：`/etc` 是这台机器**实际配了**什么，`/usr/share` 是发行版随包带的
/// 默认值（有的发行版会在那里配国内镜像）。实测那三台 Arch 上 `/usr/share` 那份是
/// 上游模板、**整份都被注释掉**，所以解析时必须跳过注释行。
const REGISTRY_CONFIGS: [&str; 2] = [
    "etc/containers/registries.conf",
    "usr/share/containers/registries.conf",
];

/// 分片配置目录（按编号读，配镜像源最常见的做法）。
const REGISTRY_CONFIG_DIR: &str = "etc/containers/registries.conf.d";

/// 一个容器运行时。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerRuntime {
    pub name: String,
    /// 可执行文件的位置。
    pub path: PathBuf,
    /// 已存在的守护进程 / 用户 socket。
    ///
    /// **空表示"装了但没看到它跑"**——rootless 的 podman 只在有用户会话时才有 socket，
    /// 所以这不一定是问题，但不该被当成"在跑"。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sockets: Vec<PathBuf>,
    /// 影响可用性的备注。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// 一个已安装的工具，只有名字和位置（版本要跑一遍才知道，见模块文档）。
///
/// 所有路径都**相对被探测机器的根**（`/usr/bin/llama-server`）——本机、远端、
/// 夹具三种来源给出同一串，才可比，也不会把临时目录泄漏到输出里。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledTool {
    pub name: String,
    pub path: PathBuf,
}

/// 一条**人为声明**的角色标签。
///
/// 这是整个 crate 里唯一不是探测出来的东西——`always-on` 说的是"这台机器会被一直开着"，
/// 而 sysfs 不知道用户会不会合盖、会不会拔电。所以它必须由人声明，也必须标明来源：
/// 标签写错会让任务**静默**地找不到这台机器，那时候要知道去哪儿改。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredTag {
    pub tag: String,
    pub source: PathBuf,
}

/// 标签文件的候选路径。**管理员写的优先**，出厂/发行版那份当默认。
///
/// 分层是有理由的：硬件产品随附的声明（"这台是随身超算"）装在 `usr/share`，
/// 用户改自己的角色时不至于去动厂商的文件。
const TAG_CONFIGS: [&str; 2] = [
    "etc/deviceinfo/tags.conf",
    "usr/share/deviceinfo/tags.conf",
];

/// 分片标签目录，方便按用途拆开写。
const TAG_CONFIG_DIR: &str = "etc/deviceinfo/tags.d";

/// 这台机器被安排成**一直开着**。
///
/// 探测不出来，只能人为声明。它是任务路由里最常用的一个：
/// 「内核编译」需要一直开机且性能不差，「定时提醒」需要一直开机且省电。
pub const TAG_ALWAYS_ON: &str = "always-on";

/// 这台机器被安排成**省电优先**，适合轻任务（定时提醒、心跳、小模型）。
pub const TAG_POWERSAVE: &str = "powersave";

/// 一条容器镜像源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryMirror {
    /// 从哪个文件读到的。
    ///
    /// 镜像源可能来自 `daemon.json`，也可能来自 `registries.conf`——出问题的时候，
    /// 这个字段决定该去哪儿改。
    pub source: PathBuf,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentReport {
    /// 找到的包管理器（按 [`BIN_DIRS`] 顺序）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub package_managers: Vec<String>,
    /// init 系统。**`None` 意味着跑不了常驻服务**（容器里、或没见过的 init）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<String>,
    /// cgroup 版本（`v1` / `v2`）。它决定容器能限到什么程度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub containers: Vec<ContainerRuntime>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inference_tools: Vec<InstalledTool>,
    /// 配置好的容器镜像源。**空不等于"没有"**：没配镜像源就是真的空
    /// （这时拉镜像走默认 registry，能不能通是另一回事——那属于实时探测）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registry_mirrors: Vec<RegistryMirror>,
    /// **人为声明**的角色标签（见 [`DeclaredTag`]）。
    ///
    /// 名字里带 `declared` 是刻意的：它是这份报告里唯一不是探测出来的东西，
    /// 混在观测事实里不标出来，读的人会以为"机器自己知道它一直开着"。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declared_tags: Vec<DeclaredTag>,
    /// 探测过程中的不确定之处。
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl EnvironmentReport {
    /// 有没有某个声明标签。标签名不区分大小写（解析时已规范化）。
    pub fn has_tag(&self, tag: &str) -> bool {
        self.declared_tags.iter().any(|declared| declared.tag == tag)
    }

    /// 这台机器被声明成一直开着——任务路由的第一步筛的就是它。
    pub fn always_on(&self) -> bool {
        self.has_tag(TAG_ALWAYS_ON)
    }

    /// 这台机器被声明成省电优先。
    pub fn powersave(&self) -> bool {
        self.has_tag(TAG_POWERSAVE)
    }

    /// 有没有可用的容器运行时（装了就算，不要求在跑）。
    pub fn has_container_runtime(&self) -> bool {
        !self.containers.is_empty()
    }

    /// 有没有已经装好的推理框架——**别重复部署**。
    pub fn has_inference_tool(&self) -> bool {
        !self.inference_tools.is_empty()
    }

    pub fn container(&self, name: &str) -> Option<&ContainerRuntime> {
        self.containers.iter().find(|runtime| runtime.name == name)
    }
}

pub(crate) fn probe(root: &Path, warnings: &mut Vec<String>) -> EnvironmentReport {
    let package_managers = PACKAGE_MANAGERS
        .iter()
        .filter(|name| find_tool(root, name).is_some())
        .map(|name| name.to_string())
        .collect();

    EnvironmentReport {
        package_managers,
        init: probe_init(root),
        cgroup: probe_cgroup(root),
        containers: probe_containers(root),
        inference_tools: probe_inference_tools(root),
        declared_tags: probe_declared_tags(root, warnings),
        registry_mirrors: probe_registry_mirrors(root, warnings),
        // 子探测往**调用方**的 `warnings` 里写（和硬件那边同一个约定），
        // 由 `probe_environment` 把它装进报告——这里留空壳，不要 take，
        // take 会让调用方的 vec 被清空，而那是调用方要看的东西。
        warnings: Vec::new(),
    }
}

/// 采集夹具时需要**列目录**的路径（其余输入是固定路径）。
///
/// 只有 `run/user` 一处需要枚举：rootless 的 podman socket 按用户编号排列。
/// 这个函数和 [`inputs`] 一起，是"探测读什么"的**单一来源**——采集清单从它们生成，
/// 不在这里重抄一遍。硬件那边已经因为重抄栽过一次（`acpi_cppc/highest_perf`）。
pub fn input_dirs() -> Vec<String> {
    vec![
        REGISTRY_CONFIG_DIR.to_string(),
        TAG_CONFIG_DIR.to_string(),
        "run/user".to_string(),
    ]
}

/// 采集夹具时该怎么处理一个输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// **内容要读**（配置文件、计数）——复制过去。
    Content(String),
    /// **只需要"存在"**（可执行文件、socket、目录）——只放占位。
    ///
    /// 这个区分不是洁癖：可执行文件动辄几十 MB（`/usr/bin/podman` 45 MB、
    /// docker 42 MB），复制内容会让夹具从 33 KB 涨到 428 MB。而探测只看它们在不在。
    Existence(String),
}

impl Input {
    pub fn path(&self) -> &str {
        match self {
            Self::Content(path) | Self::Existence(path) => path,
        }
    }
}

/// 采集夹具时要复制/占位的输入（相对探测根）。
///
/// 传进来的 `list_dir` 用来枚举 [`input_dirs`] 里那些目录——远端采集和本地采集
/// 用的是同一份逻辑，只是列目录的实现不同。
pub fn inputs(list_dir: &dyn Fn(&str) -> Vec<String>) -> Vec<Input> {
    let mut inputs: Vec<Input> = vec![
        // 镜像源配置：内容要读
        Input::Content("etc/docker/daemon.json".into()),
        Input::Content(REGISTRY_CONFIGS[0].into()),
        Input::Content(REGISTRY_CONFIGS[1].into()),
        // 人为声明的标签：内容要读（它是唯一非观测的输入，更不能漏）
        Input::Content(TAG_CONFIGS[0].into()),
        Input::Content(TAG_CONFIGS[1].into()),
        // init / cgroup 的判据：只要"存在"
        Input::Existence("run/systemd/system".into()),
        Input::Existence("run/openrc".into()),
        Input::Existence("sys/fs/cgroup/cgroup.controllers".into()),
        Input::Existence("sys/fs/cgroup/memory".into()),
    ];

    // 可执行文件候选：候选目录 × 所有要找的名字。**只放占位**。
    let tools: Vec<&str> = CONTAINER_RUNTIMES
        .iter()
        .map(|(name, _)| *name)
        .chain(INFERENCE_TOOLS)
        .chain(PACKAGE_MANAGERS)
        .collect();
    for dir in BIN_DIRS {
        for tool in &tools {
            inputs.push(Input::Existence(format!("{dir}/{tool}")));
        }
    }

    // 固定位置的 socket。也是占位（socket 本来就读不出内容）。
    for (_, sockets) in CONTAINER_RUNTIMES {
        for socket in sockets {
            inputs.push(Input::Existence((*socket).to_string()));
        }
    }

    // 分片目录里的文件：内容都要读
    for dir in [REGISTRY_CONFIG_DIR, TAG_CONFIG_DIR] {
        for entry in list_dir(dir) {
            if entry.ends_with(".conf") {
                inputs.push(Input::Content(format!("{dir}/{entry}")));
            }
        }
    }
    // rootless socket：占位
    for entry in list_dir("run/user") {
        inputs.push(Input::Existence(format!(
            "run/user/{entry}/{ROOTLESS_SOCKET_SUFFIX}"
        )));
    }

    inputs.sort_by(|a, b| a.path().cmp(b.path()));
    inputs.dedup();
    inputs
}

/// 找到的可执行文件。
///
/// **两个路径必须分开**：`reported` 进报告（机器上的路径，如 `/usr/bin/docker`），
/// `on_disk` 才是探测进程真正要打开的那个（本地就是同一个，远端探测时它在暂存树里）。
/// 混用会读错文件——实测把远端机器的 docker 误判成了本机的 podman 壳脚本，
/// 因为那次读的是 `/usr/bin/docker`——**本机的那个**。
struct FoundTool {
    reported: PathBuf,
    on_disk: PathBuf,
}

fn find_tool(root: &Path, name: &str) -> Option<FoundTool> {
    BIN_DIRS
        .iter()
        .map(|dir| FoundTool {
            reported: PathBuf::from("/").join(dir).join(name),
            on_disk: root.join(dir).join(name),
        })
        .find(|tool| tool.on_disk.is_file())
}

fn probe_init(root: &Path) -> Option<String> {
    // 用 `exists()` 而不是 `is_dir()`：真实机器上 `/run/systemd/system` 只可能是目录
    // （`sd_booted()` 也是查它），而**夹具里的空目录进不了 git**，镜像时会被写成
    // 占位文件。弱化这一层判断没有实际代价，否则夹具永远报不出 init。
    if root.join("run/systemd/system").exists() {
        return Some("systemd".into());
    }
    if root.join("run/openrc").exists() {
        return Some("openrc".into());
    }
    None
}

fn probe_cgroup(root: &Path) -> Option<String> {
    // v2 只有一个统一层级：`cgroup.controllers` 是 v2 独有的
    if root.join("sys/fs/cgroup/cgroup.controllers").exists() {
        return Some("v2".into());
    }
    // v1 按子系统挂成一堆子目录（同上：用 exists，免得夹具里判不出来）
    if root.join("sys/fs/cgroup/memory").exists() {
        return Some("v1".into());
    }
    None
}

fn probe_containers(root: &Path) -> Vec<ContainerRuntime> {
    let mut found = Vec::new();
    for (name, socket_candidates) in CONTAINER_RUNTIMES {
        let Some(tool) = find_tool(root, name) else {
            continue;
        };
        // 报告里放机器上的路径（`/run/docker.sock`），读的时候才拼探测根
        let mut sockets: Vec<PathBuf> = socket_candidates
            .iter()
            .map(|socket| PathBuf::from("/").join(socket))
            .filter(|socket| root.join(socket.strip_prefix("/").unwrap_or(socket)).exists())
            .collect();
        // rootless：`run/user/<uid>/podman/podman.sock`
        if name == "podman" {
            for entry in fs::read_dir(root.join("run/user")).into_iter().flatten().flatten() {
                let socket = root
                    .join("run/user")
                    .join(entry.file_name())
                    .join(ROOTLESS_SOCKET_SUFFIX);
                if socket.exists() {
                    sockets.push(PathBuf::from("/run/user").join(entry.file_name()).join(ROOTLESS_SOCKET_SUFFIX));
                }
            }
        }
        let mut notes = Vec::new();
        // 注意读的是 `on_disk`——壳脚本的判据必须是**那台机器上**的文件
        if let Some(shim) = shim_target(&tool.on_disk) {
            // 实测两台 Arch 上 `/usr/bin/docker` 是 228 字节的壳脚本，实际跑的是 podman
            // ——不点出来的话，报告会让人以为这机器装了**两个**容器运行时。
            notes.push(format!("不是真的 {name}：是个壳脚本，实际调用 {shim}"));
        }
        found.push(ContainerRuntime {
            name: name.to_string(),
            path: tool.reported,
            sockets,
            notes,
        });
    }
    found
}

/// 壳脚本的大小上限。超过它就不可能是壳脚本——真二进制都是几十 MB。
const SHIM_MAX_BYTES: u64 = 16 * 1024;

/// 如果这是个"壳脚本"（某某兼容包），返回它实际调用的运行时名。
///
/// 判断很保守：**必须**是 `#!` 开头的脚本文本，而且内容里出现另一个已知运行时的名字。
/// 只读文件，不执行——所以宁可漏判，也不要把真的 docker 说成壳。
fn shim_target(path: &Path) -> Option<&'static str> {
    // **先看大小再读**：真二进制动辄几十 MB（`/usr/bin/podman` 45 MB），
    // 为了看一眼 shebang 把它整个读进来是纯浪费。壳脚本都很小。
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > SHIM_MAX_BYTES {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    if !text.starts_with("#!") {
        return None;
    }
    CONTAINER_RUNTIMES
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !path.ends_with(name))
        .find(|name| text.contains(name))
}

fn probe_inference_tools(root: &Path) -> Vec<InstalledTool> {
    INFERENCE_TOOLS
        .iter()
        .filter_map(|name| {
            find_tool(root, name).map(|tool| InstalledTool {
                name: name.to_string(),
                path: tool.reported,
            })
        })
        .collect()
}

/// 读人为声明的标签。
///
/// 格式是一个极简的行格式：每行若干标签，`#` 开头是注释。
///
/// **规范化是刻意的**（转小写、`_` 换成 `-`）：标签的用途是**匹配**，大小写或下划线
/// 不一致会让路由静默失配——而"任务永远找不到这台机器"这种故障极难查。
/// 其他字符一律拒绝并出声，理由同上：静默忽略一个拼错的标签，比报错难查得多。
fn probe_declared_tags(root: &Path, warnings: &mut Vec<String>) -> Vec<DeclaredTag> {
    // 机器路径是 `/etc/...`；拼探测根之前要剥掉开头的 `/`（`join` 会把绝对路径当成整段替换）
    let mut configs: Vec<String> = tags::config_paths()
        .iter()
        .map(|path| path.trim_start_matches('/').to_string())
        .collect();
    let shard_dir = tags::TAG_CONFIG_DIR.trim_start_matches('/');
    if let Ok(entries) = fs::read_dir(root.join(shard_dir)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".conf") {
                configs.push(format!("{shard_dir}/{name}"));
            }
        }
    }

    let mut tags = Vec::new();
    for config in configs {
        let Some(text) = fs::read_to_string(root.join(&config)).ok() else {
            continue;
        };
        // 报告里记机器上的路径（`/etc/...`），读的时候才拼探测根
        parse_tags(&text, &PathBuf::from("/").join(&config), &mut tags, warnings);
    }

    tags.sort_by(|a, b| (&a.tag, &a.source).cmp(&(&b.tag, &b.source)));
    tags.dedup();
    tags
}

/// 解析标签文件。`source` 会记进每条标签里，方便回头找到那一行。
///
/// 行格式本身在 [`crate::tags`] 里——**写标签的是另一个程序**，格式只能有一份。
fn parse_tags(
    text: &str,
    source: &Path,
    tags: &mut Vec<DeclaredTag>,
    warnings: &mut Vec<String>,
) {
    for entry in tags_parse(text) {
        match entry {
            TagLine::Tag { tag, .. } => tags.push(DeclaredTag {
                tag,
                source: source.to_path_buf(),
            }),
            TagLine::Invalid { line, token } => warnings.push(format!(
                "{} 第 {} 行的 {:?} 不是合法标签（只接受字母、数字、`.`、`_`、`-`），已跳过",
                source.display(),
                line,
                token
            )),
        }
    }
}

/// 收集配置好的容器镜像源。
fn probe_registry_mirrors(root: &Path, warnings: &mut Vec<String>) -> Vec<RegistryMirror> {
    let mut found = Vec::new();

    // Docker：`daemon.json` 的 `registry-mirrors`
    let daemon_json = PathBuf::from("/etc/docker/daemon.json");
    if let Some(text) = read_trimmed(&root.join("etc/docker/daemon.json")) {
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => {
                let mirrors = value
                    .get("registry-mirrors")
                    .and_then(|mirrors| mirrors.as_array())
                    .map(|mirrors| {
                        mirrors
                            .iter()
                            .filter_map(|mirror| mirror.as_str())
                            .map(|url| RegistryMirror {
                                source: daemon_json.clone(),
                                url: url.to_string(),
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                found.extend(mirrors);
            }
            // **解析不了必须出声**：不然"读不到镜像源"和"没配镜像源"就分不开了，
            // 而这两件事对部署的含义完全相反。
            Err(error) => warnings.push(format!(
                "{} 解析失败（{error}），镜像源可能读不全",
                daemon_json.display()
            )),
        }
    }

    // containers（podman）：`registries.conf` 里未注释的 `location = "..."`
    let mut configs: Vec<PathBuf> = REGISTRY_CONFIGS
        .iter()
        .map(|config| PathBuf::from("/").join(config))
        .collect();
    if let Ok(entries) = fs::read_dir(root.join(REGISTRY_CONFIG_DIR)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".conf") {
                configs.push(PathBuf::from("/").join(REGISTRY_CONFIG_DIR).join(name));
            }
        }
    }
    for config in configs {
        let Some(text) = fs::read_to_string(root.join(config.strip_prefix("/").unwrap_or(&config))).ok() else {
            continue;
        };
        for line in text.lines() {
            // 发行版随包带的那份**整份都是注释**，所以必须跳过注释行，
            // 否则会把上游模板里的示例镜像源当成真配好的。
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some(url) = parse_location(line) {
                found.push(RegistryMirror {
                    source: config.clone(),
                    url,
                });
            }
        }
    }

    found.sort_by(|a, b| (&a.source, &a.url).cmp(&(&b.source, &b.url)));
    found.dedup();
    found
}

/// 从 `location = "example.com"` 里取出值。只认这一种写法——宁可漏，
/// 也不要把 `prefix` 之类的东西当成镜像源报出去。
fn parse_location(line: &str) -> Option<String> {
    let value = line.trim().strip_prefix("location")?.trim_start();
    let value = value.strip_prefix('=')?.trim();
    let value = value.strip_prefix('"')?;
    let (url, _) = value.split_once('"')?;
    (!url.is_empty()).then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("deviceinfo-env-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// 这台机器的形状：podman + rootless socket + systemd + cgroup v2 + 已有 llama.cpp。
    #[test]
    fn finds_a_rootless_container_runtime_and_existing_tools() {
        let root = fake_root("podman");
        write(&root, "usr/bin/podman", "");
        write(&root, "usr/bin/pacman", "");
        write(&root, "usr/bin/llama-server", "");
        write(&root, "run/user/1000/podman/podman.sock", "");
        write(&root, "run/systemd/system/.keep", "");
        write(&root, "sys/fs/cgroup/cgroup.controllers", "cpuset cpu\n");
        // 只有 socket 目录、没有 socket 的那个用户不该被算进来
        write(&root, "run/user/1001/.keep", "");

        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");

        assert_eq!(report.package_managers, vec!["pacman"]);
        assert_eq!(report.init.as_deref(), Some("systemd"));
        assert_eq!(report.cgroup.as_deref(), Some("v2"));
        assert!(report.has_container_runtime());
        let podman = report.container("podman").expect("应当找到 podman");
        // 路径从探测根起算（本机探测就是绝对路径），和硬件报告的 device_path 同一约定
        assert_eq!(podman.path, Path::new("/usr/bin/podman"));
        assert!(podman.sockets.iter().any(|socket| {
            socket.ends_with("run/user/1000/podman/podman.sock")
        }));
        // 已经有 llama.cpp → 别再装一个
        assert!(report.has_inference_tool());
        assert_eq!(report.inference_tools[0].name, "llama-server");

        fs::remove_dir_all(&root).ok();
    }

    /// 实测两台 Arch 上 `/usr/bin/docker` 是 podman 的壳脚本（`podman-docker`）——
    /// 不点出来的话，报告会让人以为这机器装了**两个**容器运行时。
    #[test]
    fn a_shim_script_is_labelled_as_what_it_really_runs() {
        let root = fake_root("shim");
        write(&root, "usr/bin/podman", "");
        write(
            &root,
            "usr/bin/docker",
            "#!/bin/sh\n# podman-docker\nexec podman \"$@\"\n",
        );

        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        let docker = report.container("docker").expect("壳脚本也要报出来");
        assert!(
            docker.notes.iter().any(|note| note.contains("podman")),
            "要说清它其实在跑什么: {docker:#?}"
        );
        // 真的 podman 不该被标成壳
        assert!(report.container("podman").unwrap().notes.is_empty());

        fs::remove_dir_all(&root).ok();
    }

    /// 标签是**人为声明**的：从 `/etc/deviceinfo/tags.conf` 读，规范化后能用
    /// 大小写不敏感的方式匹配。
    #[test]
    fn declared_tags_are_read_and_normalized() {
        let root = fake_root("tags");
        write(
            &root,
            "etc/deviceinfo/tags.conf",
            "# 这台机器的角色\nAlways_On   powersave\n\n",
        );
        // 出厂那份当默认，管理员那份优先（这里两者都有，取并集）
        write(&root, "usr/share/deviceinfo/tags.conf", "vendor-preinstalled\n");

        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");

        // `Always_On` → `always-on`（匹配要可预测）
        assert!(report.always_on(), "{:#?}", report.declared_tags);
        assert!(report.powersave());
        assert!(report.has_tag("always-on"));
        assert!(!report.has_tag("ALWAYS-ON"), "存的是规范化后的形式");
        assert_eq!(
            report
                .declared_tags
                .iter()
                .map(|declared| declared.tag.as_str())
                .collect::<Vec<_>>(),
            vec!["always-on", "powersave", "vendor-preinstalled"]
        );
        // 来源要记下来：标签写错时得能直接找到那一行
        let always_on = report
            .declared_tags
            .iter()
            .find(|declared| declared.tag == "always-on")
            .unwrap();
        assert_eq!(always_on.source, Path::new("/etc/deviceinfo/tags.conf"));

        fs::remove_dir_all(&root).ok();
    }

    /// 拼错的标签必须**出声**：静默忽略一张拼错的标签，等于让任务永远找不到这台机器，
    /// 而这种故障极难查。
    #[test]
    fn an_invalid_tag_is_a_warning_not_a_silent_skip() {
        let root = fake_root("bad-tags");
        write(
            &root,
            "etc/deviceinfo/tags.conf",
            "always-on\n不只是拼错\nalways on\n",
        );

        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        // 合法的留下；不合法的两个都出声（`always on` 会被拆成两个合法标签，
        // 所以这里只断言"有出声"这件事）
        assert!(report.always_on());
        assert!(!warnings.is_empty(), "{:#?}", report.declared_tags);

        fs::remove_dir_all(&root).ok();
    }

    /// 没有标签文件就是没有标签——不是错误，也不该出声。
    #[test]
    fn no_tag_file_means_no_tags() {
        let root = fake_root("no-tags");
        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        assert!(report.declared_tags.is_empty());
        assert!(!report.always_on());
        assert!(!report.powersave());
        assert!(warnings.is_empty(), "{warnings:?}");
        fs::remove_dir_all(&root).ok();
    }

    /// 标签分片目录也要读。
    #[test]
    fn tag_drop_ins_are_merged() {
        let root = fake_root("tag-dropins");
        write(&root, "etc/deviceinfo/tags.d/10-role.conf", "always-on\n");
        write(&root, "etc/deviceinfo/tags.d/20-power.conf", "powersave\n");

        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        assert!(report.always_on() && report.powersave(), "{:#?}", report.declared_tags);
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    /// 真二进制不能被当成壳脚本。
    ///
    /// 这条是回归测试：曾经因为读了**错误路径上的文件**（本机的 `/usr/bin/docker`，
    /// 一个 228 字节的 podman 壳），把远端机器上 42 MB 的真 docker 二进制
    /// 误标成了壳脚本。所以判据必须钉在 `on_disk` 上。
    #[test]
    fn a_real_binary_is_not_a_shim() {
        let root = fake_root("real-binary");
        // 非 UTF-8 的内容 → 读不成字符串 → 判不出来，就不该下结论
        let path = root.join("usr/bin/docker");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [0x7f, b'E', b'L', b'F', 0x00, 0xff, 0xfe, 0x01]).unwrap();

        assert!(shim_target(&path).is_none());

        fs::remove_dir_all(&root).ok();
    }

    /// **装了 ≠ 在跑**：有可执行文件但没有 socket 时要如实反映。
    #[test]
    fn an_installed_runtime_without_a_socket_reports_no_socket() {
        let root = fake_root("no-socket");
        write(&root, "usr/bin/docker", "");

        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        let docker = report.container("docker").expect("装了就要报");
        assert!(
            docker.sockets.is_empty(),
            "没看到 socket 就不该假装它在跑: {docker:#?}"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// 一台什么都没有的机器（实测那台 CIX）——不能凭空报出运行时而报错。
    #[test]
    fn a_bare_machine_reports_nothing_rather_than_failing() {
        let root = fake_root("bare");
        write(&root, "usr/bin/apt-get", "");
        write(&root, "run/systemd/system/.keep", "");
        write(&root, "sys/fs/cgroup/cgroup.controllers", "");

        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        assert!(!report.has_container_runtime());
        assert!(!report.has_inference_tool());
        assert!(report.registry_mirrors.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    /// Docker 的镜像源要从 `daemon.json` 读出来——**实测那台 NAS 就是这样**
    /// （到不了 Docker Hub，全靠 `docker.fnnas.com`）。
    #[test]
    fn reads_docker_registry_mirrors_from_daemon_json() {
        let root = fake_root("docker-mirrors");
        write(&root, "usr/bin/docker", "");
        write(&root, "run/docker.sock", "");
        write(
            &root,
            "etc/docker/daemon.json",
            r#"{"data-root":"/vol1/docker","registry-mirrors":["https://docker.fnnas.com","https://registry.hub.docker.com"]}"#,
        );

        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        assert_eq!(
            report
                .registry_mirrors
                .iter()
                .map(|mirror| mirror.url.as_str())
                .collect::<Vec<_>>(),
            vec!["https://docker.fnnas.com", "https://registry.hub.docker.com"]
        );
        assert_eq!(
            report.registry_mirrors[0].source,
            Path::new("/etc/docker/daemon.json"),
            "要说清是哪份文件配的；路径是机器上的路径，不带探测根"
        );
        let docker = report.container("docker").unwrap();
        assert!(docker.sockets.iter().any(|socket| socket.ends_with("run/docker.sock")));

        fs::remove_dir_all(&root).ok();
    }

    /// `daemon.json` 坏掉时必须**出声**：否则"读不到镜像源"和"没配镜像源"就分不开了，
    /// 而这两件事对部署的含义完全相反。
    #[test]
    fn a_broken_daemon_json_is_a_warning_not_silence() {
        let root = fake_root("broken-json");
        write(&root, "usr/bin/docker", "");
        write(&root, "etc/docker/daemon.json", "{ not json");

        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        assert!(report.registry_mirrors.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("daemon.json"), "{warnings:?}");

        fs::remove_dir_all(&root).ok();
    }

    /// `registries.conf` 里发行版随包带的那份**整份是注释**（实测三台 Arch 都是这样），
    /// 解析时必须跳过，否则会把上游模板里的示例当成真配好的镜像源。
    ///
    /// 注意：**没有一台机器真配过 podman 镜像源**，所以这条只有合成样本——
    /// 格式照 `registries.conf(5)` 与实测文件里的注释写的。
    #[test]
    fn registry_conf_skips_comments_and_reads_locations() {
        let root = fake_root("registries");
        write(&root, "usr/bin/podman", "");
        write(
            &root,
            "usr/share/containers/registries.conf",
            "# unqualified-search-registries = [\"example.com\"]\n\
             #\n# [[registry]]\n# location = \"internal-registry-for-example.com/bar\"\n",
        );
        write(
            &root,
            "etc/containers/registries.conf",
            "unqualified-search-registries = [\"docker.io\"]\n\
             [[registry]]\n\
             location = \"docker.io\"\n\
             [[registry.mirror]]\n\
             location = \"mirror.example.cn\"\n",
        );
        write(
            &root,
            "etc/containers/registries.conf.d/10-mirror.conf",
            "[[registry.mirror]]\nlocation = \"another.example.cn\"\n",
        );

        let mut warnings = Vec::new();
        let report = probe(&root, &mut warnings);
        let urls: Vec<&str> = report
            .registry_mirrors
            .iter()
            .map(|mirror| mirror.url.as_str())
            .collect();
        // 注释里的示例一个都不能进
        assert!(!urls.iter().any(|url| url.contains("example.com")), "{urls:?}");
        assert!(urls.contains(&"mirror.example.cn"), "{urls:?}");
        assert!(urls.contains(&"another.example.cn"), "{urls:?}");
        // 分片目录也要读
        assert!(report
            .registry_mirrors
            .iter()
            .any(|mirror| mirror.source.ends_with("10-mirror.conf")));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn location_parsing_is_strict() {
        assert_eq!(
            parse_location("location = \"a.example.cn\"").as_deref(),
            Some("a.example.cn")
        );
        assert_eq!(
            parse_location("  location=\"b.example.cn\"").as_deref(),
            Some("b.example.cn")
        );
        // 只认 location；prefix / path 之类不该被当成镜像源
        assert_eq!(parse_location("prefix = \"x\""), None);
        assert_eq!(parse_location("location = "), None);
        assert_eq!(parse_location("location = \"\""), None);
        assert_eq!(parse_location(""), None);
    }

    /// init 认不出来时是 `None`——那意味着**跑不了常驻服务**，不是"探测失败"。
    #[test]
    fn no_known_init_is_reported_as_none() {
        let root = fake_root("no-init");
        let mut warnings = Vec::new();
        assert_eq!(probe(&root, &mut warnings).init, None);
        assert!(warnings.is_empty(), "{warnings:?}");
        fs::remove_dir_all(&root).ok();
    }
}
