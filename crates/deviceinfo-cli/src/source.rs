//! 采集时的文件来源。
//!
//! # 为什么需要这层抽象
//!
//! 夹具必须在**目标机**上采，而目标机往往装不了 Rust 工具链（路由器、嵌入式盒子），
//! 本机也未必能交叉编译到它的架构（本机 Rust 是 pacman 装的，只有 host target）。
//! 所以采集要能隔着 ssh 做，而不是先想办法在目标机上跑起 `deviceinfo`。
//!
//! # 为什么接口长这样
//!
//! 刻意保持"路径进、内容出"的形状，和 `std::fs` 一一对应——这样本地与远端两条路
//! 走的是**同一份采集逻辑**（`capture_plan`），不会各自漂移。
//!
//! # 为什么一切都围着"批量"设计
//!
//! 实测同一局域网内**单次 ssh 往返要 770ms**（握手 + 认证 + 远端起 shell），而一次
//! 采集原本有二十来次往返。所以三个操作都提供批量版本，远端各自把一轮压成一次往返：
//!
//! | 操作 | 单次 | 批量 |
//! |---|---|---|
//! | 列目录 | —— | [`Source::list_many`] |
//! | 读软链 | —— | [`Source::link_many`] |
//! | 读文件 | —— | [`Source::prefetch`] |
//!
//! stdout 上跑的是行协议（`@@F <字节数> <路径>` 后面跟原始内容），不用 base64：
//! `ssh` 传二进制是安全的，而**按字节数截断**比找分隔符可靠——文件内容里什么都可能有。

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 目录里的一个条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    /// 是不是目录。**必须带上这个信息**：`usr/lib` 的列表里含有
    /// `aarch64-linux-gnu` 这样的目录名，把它当库文件会一路错下去。
    pub is_dir: bool,
}

/// 一个路径在来源里的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    /// 普通文件（软链会跟随，和 `fs::metadata().is_file()` 一致）。
    File,
    /// A directory, including empty firmware observation markers.
    Directory,
    /// 存在，但不是普通文件或目录：设备节点等。
    Other,
    Missing,
}

pub(crate) enum Source {
    /// 本地的一棵文件树（`--root`）。
    Local(PathBuf),
    /// 远端主机（`--ssh`）。主机名交给 ssh 自己解析，别名、`~/.ssh/config` 都照常生效。
    Remote(RemoteSource),
}

pub(crate) struct RemoteSource {
    host: String,
    contents: RefCell<BTreeMap<String, Vec<u8>>>,
    kinds: RefCell<BTreeMap<String, EntryKind>>,
}

impl Source {
    /// Source-side capture bounds; a local mirror retains its frozen window.
    pub(crate) fn stamp(&self, started: bool) -> io::Result<deviceinfo::SampleStamp> {
        match self {
            Self::Local(root) => {
                let context = deviceinfo::SampleContext::read(root);
                Ok(if started {
                    context.started
                } else {
                    context.finished
                })
            }
            Self::Remote(remote) => parse_stamp(&remote.run_with_stdin(REMOTE_STAMP_SCRIPT, &[])?),
        }
    }

    /// Clock/boot observations for newly executed checks, never capture replay.
    pub(crate) fn fresh_stamp(&self) -> io::Result<deviceinfo::SampleStamp> {
        match self {
            Self::Local(root) if root == Path::new("/") => {
                Ok(deviceinfo::SampleContext::read(root).started)
            }
            Self::Local(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "An injected root cannot supply fresh target clock/boot observations",
            )),
            Self::Remote(remote) => parse_stamp(&remote.run_with_stdin(REMOTE_STAMP_SCRIPT, &[])?),
        }
    }

    pub(crate) fn preserves_counters(&self) -> bool {
        match self {
            Self::Local(root) => deviceinfo::SampleContext::read(root).consistent,
            Self::Remote(_) => true,
        }
    }

    pub(crate) fn associations(
        &self,
        paths: &[String],
    ) -> io::Result<Vec<deviceinfo::DeviceAssociation>> {
        match self {
            Self::Local(root) => Ok(deviceinfo::inspect_device_links(root).devices),
            Self::Remote(remote) => parse_associations(
                &remote.run_with_stdin(REMOTE_ASSOCIATIONS_SCRIPT, &stdin_lines(paths))?,
            ),
        }
    }

    pub(crate) fn local(root: &Path) -> Self {
        Self::Local(root.to_path_buf())
    }

    /// 远端主机名。
    pub(crate) fn host(&self) -> &str {
        match self {
            Self::Remote(remote) => &remote.host,
            Self::Local(_) => "",
        }
    }

    pub(crate) fn remote(host: &str) -> Self {
        Self::Remote(RemoteSource {
            host: host.to_string(),
            contents: RefCell::new(BTreeMap::new()),
            kinds: RefCell::new(BTreeMap::new()),
        })
    }

    /// 某人可读的描述，用于日志。
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Local(root) => root.display().to_string(),
            Self::Remote(remote) => format!("ssh://{}", remote.host),
        }
    }

    /// 一次取回多个目录的条目名。不存在的目录返回空列表。
    ///
    /// 返回 `Result` 而不是"失败就给空"：采集会把结果写成一份要进回归测试的夹具，
    /// 而**空白结果会造出一份自洽但错误的夹具**（expected.json 是从同一棵暂存树
    /// 算出来的，所以测试反而会通过）。尤其是最后一步 `link_many` 失败时，
    /// 夹具会静默地没有驱动软链。
    pub(crate) fn list_many(&self, dirs: &[String]) -> io::Result<BTreeMap<String, Vec<Entry>>> {
        match self {
            Self::Local(root) => Ok(dirs
                .iter()
                .map(|dir| (dir.clone(), list_local(root, dir)))
                .collect()),
            Self::Remote(remote) => {
                let out = remote.run_with_stdin(REMOTE_LIST_SCRIPT, &stdin_lines(dirs))?;
                Ok(parse_listing(&out))
            }
        }
    }

    /// 一次取回多个路径的软链目标。不是软链、或读不到，对应 `None`。
    pub(crate) fn link_many(
        &self,
        paths: &[String],
    ) -> io::Result<BTreeMap<String, Option<String>>> {
        match self {
            Self::Local(root) => Ok(paths
                .iter()
                .map(|path| {
                    let target = std::fs::read_link(root.join(path))
                        .ok()
                        .and_then(|target| target.to_str().map(str::to_string));
                    (path.clone(), target)
                })
                .collect()),
            Self::Remote(remote) => {
                let out = remote.run_with_stdin(REMOTE_LINK_SCRIPT, &stdin_lines(paths))?;
                Ok(parse_links(&out))
            }
        }
    }

    /// 路径的状态。
    ///
    /// **三种状态必须分开**：镜像时"普通文件"要取内容、"设备节点"只占位、
    /// "不存在"要**跳过**。把最后一种也写成空文件，会让探测把不存在的
    /// `vendor`/`device` 读成空字符串，于是报告里出现 `GPU (card0, id )` 和
    /// `(:)` 这种垃圾——这个 bug 真的发生过一次。
    pub(crate) fn kind(&self, path: &str) -> EntryKind {
        match self {
            Self::Local(root) => match deviceinfo::resolve_path_in_root(root, path)
                .and_then(std::fs::metadata) {
                Ok(meta) if meta.is_file() => EntryKind::File,
                Ok(meta) if meta.is_dir() => EntryKind::Directory,
                Ok(_) => EntryKind::Other,
                Err(_) => EntryKind::Missing,
            },
            Self::Remote(remote) => remote
                .kinds
                .borrow()
                .get(path)
                .copied()
                // 没 prefetch 过就现问一次，慢但正确
                .unwrap_or_else(|| {
                    let answer = remote
                        .run(&format!(
                            "if [ -f {0} ]; then echo F; elif [ -d {0} ]; then echo R; elif [ -e {0} ]; then echo D; else echo M; fi",
                            quoted(path)
                        ))
                        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
                        .unwrap_or_else(|_| "M".into());
                    match answer.as_str() {
                        "F" => EntryKind::File,
                        "R" => EntryKind::Directory,
                        "D" => EntryKind::Other,
                        _ => EntryKind::Missing,
                    }
                }),
        }
    }

    /// 读文件内容。读不到返回 `None`。
    pub(crate) fn read(&self, path: &str) -> Option<Vec<u8>> {
        match self {
            Self::Local(root) => deviceinfo::resolve_path_in_root(root, path)
                .and_then(std::fs::read)
                .ok(),
            Self::Remote(remote) => remote
                .contents
                .borrow()
                .get(path)
                .cloned()
                .or_else(|| remote.run(&format!("cat -- {}", quoted(path))).ok()),
        }
    }

    /// 把一批路径的内容与状态一次性取回。
    ///
    /// 本地是空操作；远端把几十次连接压成一次。**采集在动手前就知道完整清单**，
    /// 所以这个批量是自然的，不是妥协。
    /// `content` 是**内容一定要**的路径；`existence_only` 是只要"在不在"的
    /// （可执行文件、socket、目录标记）——后者由远端按大小决定带不带内容。
    pub(crate) fn prefetch(&self, content: &[String], existence_only: &[String]) -> io::Result<()> {
        let Self::Remote(remote) = self else {
            return Ok(());
        };
        let mut items: Vec<String> = content.iter().map(|path| format!("c {path}")).collect();
        items.extend(existence_only.iter().map(|path| format!("e {path}")));
        let out = remote.run_with_stdin(&remote_fetch_script(), &stdin_lines(&items))?;
        let mut failures = Vec::new();
        parse_batch(
            &out,
            &mut remote.contents.borrow_mut(),
            &mut remote.kinds.borrow_mut(),
            &mut failures,
        );
        if !failures.is_empty() {
            return Err(io::Error::other(format!(
                "远端有 {} 个文件复制不出来（例如 {}），采集结果不可信",
                failures.len(),
                failures[0]
            )));
        }
        Ok(())
    }
}

const REMOTE_STAMP_SCRIPT: &str = r#"
boot=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null)
IFS=' ' read -r uptime idle < /proc/uptime
printf '%s\n' "$boot" "$uptime" "$(date +%s%N 2>/dev/null)" "$(readlink /proc/self/ns/time 2>/dev/null)" "$(readlink /proc/self/ns/mnt 2>/dev/null)"
cat /proc/sys/kernel/random/boot_id 2>/dev/null
"#;

// Linux GNU/coreutils and BusyBox support these read-only sysfs operations.
const REMOTE_ASSOCIATIONS_SCRIPT: &str = r#"
while IFS= read -r relative; do
  source=/$relative
  [ -d "$source" ] || continue
  physical=$(readlink -f "$source/device" 2>/dev/null)
  [ -d "$physical" ] || physical=$(readlink -f "$source" 2>/dev/null)
  driver=$(readlink "$physical/driver" 2>/dev/null)
  instance=$(stat -L -c '%d:%i' "$physical" 2>/dev/null)
  channel=$(stat -L -c '%d:%i' "$source" 2>/dev/null)
  printf '%s\t%s\t%s\t%s\t%s\n' "$source" "$physical" "${driver##*/}" "$(cat "$source/dev" 2>/dev/null)" "${instance:+${channel:+$instance:$channel}}"
done
"#;

fn parse_stamp(raw: &[u8]) -> io::Result<deviceinfo::SampleStamp> {
    let text = std::str::from_utf8(raw)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let fields: Vec<_> = text.lines().collect();
    if fields.len() != 6 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source sampling stamp is incomplete",
        ));
    }
    let value = |index: usize| (!fields[index].is_empty()).then(|| fields[index].to_owned());
    let boot_time_ns = fields[1].split_once('.').and_then(|(seconds, fraction)| {
        if fraction.len() > 9 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        seconds
            .parse::<u64>()
            .ok()?
            .checked_mul(1_000_000_000)?
            .checked_add(
                fraction
                    .parse::<u64>()
                    .ok()?
                    .checked_mul(10u64.pow(9 - fraction.len() as u32))?,
            )
    });
    Ok(deviceinfo::SampleStamp {
        boot_id: (fields[0] == fields[5]).then(|| value(0)).flatten(),
        boot_time_ns,
        boot_clock_resolution_ns: boot_time_ns.map(|_| 10_000_000),
        unix_time_ns: fields[2].parse().ok(),
        time_namespace: value(3),
        mount_namespace: value(4),
    })
}

fn parse_associations(raw: &[u8]) -> io::Result<Vec<deviceinfo::DeviceAssociation>> {
    let text = std::str::from_utf8(raw)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    text.lines()
        .map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 5 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "source device association is incomplete",
                ));
            }
            let value =
                |index: usize| (!fields[index].is_empty()).then(|| fields[index].to_owned());
            Ok(deviceinfo::DeviceAssociation {
                source: PathBuf::from(fields[0]),
                physical_path: value(1).map(PathBuf::from),
                driver: value(2),
                device_number: value(3),
                kernel_instance: value(4),
            })
        })
        .collect()
}

impl RemoteSource {
    fn run(&self, script: &str) -> io::Result<Vec<u8>> {
        self.run_with_stdin(script, &[])
    }

    fn run_with_stdin(&self, script: &str, input: &[u8]) -> io::Result<Vec<u8>> {
        // **每条命令都要先 `cd /`。** 远端 shell 的 cwd 是用户的 HOME，而采集清单里的
        // 路径都是从根起算的相对路径（`proc/cpuinfo`、`sys/class/drm/...`）；
        // 少了这一步，每条路径都会解析到 HOME 下，然后**整份清单全报"不存在"**——
        // 采集会"成功"，只是内容全是空的。这个 bug 真的发生过一次。
        let script = format!("cd / || exit 1\n{script}");

        // 连接复用。实测单次 ssh 往返要 770ms（握手 + 认证 + 远端起 shell），
        // 一次采集有二十来次往返，不复用就是十几秒。`%h` 由 ssh 展开成主机名，
        // 所以不同主机不会共用同一条 master 连接。
        let control_path = format!(
            "{}/deviceinfo-ssh-{}-%h",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let mut child = Command::new("ssh")
            // BatchMode：宁可直接失败，也不要卡在密码提示上（agent 场景下那会永久挂住）
            .args(["-o", "BatchMode=yes"])
            // 主机不可达时别挂在那里：默认 TCP 超时可能两分钟以上，
            // 而采集是交互式跑的，挂着比失败更糟
            .args(["-o", "ConnectTimeout=10"])
            .args(["-o", "ControlMaster=auto"])
            .args(["-o", &format!("ControlPath={control_path}")])
            .args(["-o", "ControlPersist=30"])
            // `--` 必须紧贴主机名：放在选项中间会把后面的 `-o` 当成主机名
            .arg("--")
            .arg(&self.host)
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if !input.is_empty() {
            let mut stdin = child.stdin.take().expect("刚设了 piped");
            stdin.write_all(input)?;
            // stdin 在这里 drop，远端 `while read` 才会看到 EOF
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "ssh {} 失败（退出码 {:?}）：{}",
                self.host,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }
}

/// 远端列目录的脚本。
///
/// 抽成常量不只是为了整洁：`remote_scripts_all_run_under_busybox` 会拿本机的 busybox
/// 真跑一遍——目标机常常是 OpenWrt / 嵌入式，那边只有 busybox，而这类不兼容
/// 只有到目标机上才会发现，那时候你人未必在机器旁边。
const REMOTE_LIST_SCRIPT: &str = "\
while IFS= read -r d; do
  printf '@@L %s\\n' \"$d\"
  ls -1p -- \"$d\" 2>/dev/null
  printf '@@E\\n'
done";

/// 远端读软链的脚本。`readlink` 失败（不是软链、不存在）时输出 `@@N`。
const REMOTE_LINK_SCRIPT: &str = "\
while IFS= read -r p; do
  printf '@@K %s\\n' \"$p\"
  t=$(readlink -- \"$p\" 2>/dev/null)
  if [ -n \"$t\" ]; then printf '@@T %s\\n' \"$t\"; else printf '@@N\\n'; fi
done";

/// 取内容脚本（把大小门槛代入）。
///
/// **统一走这个函数**：常量里那个 `{SMALL}` 占位一旦有哪一处忘了代入，
/// 脚本会以 `-le {SMALL}` 跑起来——shell 报个错、然后一路走 else 分支，
/// 于是所有"只要存在"的路径都报 0 字节，**看起来像"文件都是空的"**。
/// 这个坑真踩过一次（单元测试先红了）。
fn remote_fetch_script() -> String {
    REMOTE_FETCH_SCRIPT.replace("{SMALL}", &SMALL_FILE_BYTES.to_string())
}

/// "只要存在"的路径在多小时才值得把内容也带回来。
///
/// 与 CLI 侧的 `SMALL_FILE_BYTES` **必须是同一个数**，所以这个脚本由格式串生成。
const SMALL_FILE_BYTES: u64 = 8 * 1024;

/// 远端取内容的脚本。
///
/// 两个细节都是踩过才知道的：
///
/// 1. **先落到临时文件，再量它的长度。** 直接写 `wc -c < "$p"; cat -- "$p"` 是读了
///    两次，而 `/proc/meminfo`、`npu_busy_time_us`、`freq0/cur_freq` 这些**中途会变长短**。
///    长度对不上，本地按长度截断就会**从此错位**，而错位的结果是静默的：
///    expected.json 是从同一棵暂存树算出来的，夹具测试照样通过。
/// 2. 复制不出来就报 `@@X`，让本地**直接失败**，而不是当成"不存在"——把读失败
///    伪装成缺失，又会造出"自洽但错误"的夹具。
const REMOTE_FETCH_SCRIPT: &str = "\
while IFS= read -r line; do
  mode=${line%% *}
  p=${line#* }
  if [ \"$mode\" = c ] && [ -f \"$p\" ]; then
    t=\"${TMPDIR:-/tmp}/.deviceinfo-cat-$$\"
    if cat -- \"$p\" > \"$t\" 2>/dev/null; then
      printf '@@F %s %s\\n' \"$(wc -c < \"$t\")\" \"$p\"
      cat -- \"$t\"
      rm -f \"$t\"
    else
      printf '@@X %s\\n' \"$p\"
    fi
  elif [ -f \"$p\" ]; then
    # 只要\"存在\"的路径：**只有小文件才带内容**。壳脚本只有几百字节，
    # 而 `/usr/bin/podman` 是 45 MB——把它的内容传回来会让一次采集白走几十 MB，
    # 现象是\"看起来卡住\"（实测就这样，而且夹具的尺寸守卫拦不住：内容根本没写进夹具）。
    n=$(wc -c < \"$p\" 2>/dev/null) || n=0
    if [ \"$n\" -le {SMALL} ]; then
      printf '@@F %s %s\\n' \"$n\" \"$p\"
      cat -- \"$p\"
    else
      # 报 0 字节且不带内容：本地据此写占位
      printf '@@F 0 %s\\n' \"$p\"
    fi
  elif [ -d \"$p\" ]; then
    printf '@@R %s\\n' \"$p\"
  elif [ -e \"$p\" ]; then
    printf '@@D %s\\n' \"$p\"
  else
    printf '@@M %s\\n' \"$p\"
  fi
done";

/// 拼成喂给远端 `while read` 的输入。
///
/// **末尾必须有换行。** `read` 在遇到 EOF 且没有换行时返回非零，于是 `while` 的循环体
/// 不会执行——**最后一行会被静默丢掉**。这个 bug 让每台机器**最后一个 DRM 设备**的
/// `drm/` 目录列表凭空消失（现象只是"夹具里少一个条目"），而 `prefetch` 上的同一处
/// 漏掉则表现为"每次采集都多花一次往返"，因为那条路会退化成逐个现问。
fn stdin_lines(items: &[String]) -> Vec<u8> {
    let mut text = items.join("\n");
    text.push('\n');
    text.into_bytes()
}

/// 把路径安全地放进远端 shell 的单引号里。
///
/// 路径里出现单引号就直接拒绝：采集路径只会有 `/`、字母、数字、`_`、`.`、`-`，
/// 真出现了说明来源不对，宁可失败也不要拼出一条能被解释成别的东西的命令。
pub(crate) fn quoted(path: &str) -> String {
    if path.contains('\'') {
        // 用一个必然失败的表达式，让这一步读不到东西而不是执行别的
        return "''".to_string();
    }
    format!("'{path}'")
}

fn list_local(root: &Path, dir: &str) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| Entry {
            is_dir: entry.path().is_dir(),
            name: entry.file_name().to_string_lossy().into_owned(),
        })
        .collect()
}

fn lines_of(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

/// 解析 `@@L <目录>` … `@@E` 分节组成的列目录响应。
///
/// 条目用 `ls -1p` 取，目录名带尾 `/`——这个标记必须保留，见 [`Entry::is_dir`]。
fn parse_listing(out: &[u8]) -> BTreeMap<String, Vec<Entry>> {
    let mut result: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in lines_of(out) {
        if let Some(dir) = line.strip_prefix("@@L ") {
            current = Some(dir.to_string());
            result.entry(dir.to_string()).or_default();
            continue;
        }
        if line == "@@E" {
            current = None;
            continue;
        }
        if let Some(dir) = &current {
            let (name, is_dir) = match line.strip_suffix('/') {
                Some(name) => (name.to_string(), true),
                None => (line, false),
            };
            result
                .entry(dir.clone())
                .or_default()
                .push(Entry { name, is_dir });
        }
    }
    result
}

/// 解析 `@@K <路径>` 后面跟 `@@T <目标>` 或 `@@N` 的读软链响应。
fn parse_links(out: &[u8]) -> BTreeMap<String, Option<String>> {
    let mut result = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in lines_of(out) {
        if let Some(path) = line.strip_prefix("@@K ") {
            current = Some(path.to_string());
            result.insert(path.to_string(), None);
            continue;
        }
        if let Some(target) = line.strip_prefix("@@T ") {
            if let Some(path) = current.take() {
                result.insert(path, Some(target.to_string()));
            }
        } else if line == "@@N" {
            current = None;
        }
    }
    result
}

/// 解析 `@@F <字节数> <路径>` / `@@D <路径>` / `@@M <路径>` 的批量响应。
///
/// 文件内容按**字节数**截断而不是找结束标记：内容里什么都可能有，
/// 唯一可靠的定界是长度。
fn parse_batch(
    out: &[u8],
    contents: &mut BTreeMap<String, Vec<u8>>,
    kinds: &mut BTreeMap<String, EntryKind>,
    failures: &mut Vec<String>,
) {
    let mut pos = 0;
    while pos < out.len() {
        // 跳过 cat 出来的内容之后可能残留的换行
        if out[pos] != b'@' {
            pos += 1;
            continue;
        }
        let Some(end) = out[pos..].iter().position(|byte| *byte == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&out[pos..pos + end]).into_owned();
        let body_start = pos + end + 1;

        let mut parts = header.splitn(3, ' ');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("@@F"), Some(size), Some(path)) => {
                let Ok(size) = size.parse::<usize>() else {
                    pos = body_start;
                    continue;
                };
                let body_end = (body_start + size).min(out.len());
                contents.insert(path.to_string(), out[body_start..body_end].to_vec());
                kinds.insert(path.to_string(), EntryKind::File);
                pos = body_end;
            }
            (Some("@@D"), Some(path), None) => {
                kinds.insert(path.to_string(), EntryKind::Other);
                pos = body_start;
            }
            (Some("@@R"), Some(path), None) => {
                kinds.insert(path.to_string(), EntryKind::Directory);
                pos = body_start;
            }
            (Some("@@M"), Some(path), None) => {
                kinds.insert(path.to_string(), EntryKind::Missing);
                pos = body_start;
            }
            // 远端主诉"存在但我复制不出来"——本地必须当错误，不能当缺失
            (Some("@@X"), Some(path), None) => {
                failures.push(path.to_string());
                pos = body_start;
            }
            _ => {
                // 内容里恰好有 `@@`：跳过这一行继续找
                pos = body_start;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_clock_never_replays_a_local_mirrors_frozen_context() {
        let root =
            std::env::temp_dir().join(format!("deviceinfo-live-source-{}", std::process::id()));
        std::fs::create_dir_all(root.join("proc/sys/kernel/random")).unwrap();
        let boot = "01234567-89ab-cdef-0123-456789abcdef";
        std::fs::write(root.join(deviceinfo::BOOT_ID_INPUT), boot).unwrap();
        let stamp = deviceinfo::SampleStamp {
            boot_time_ns: Some(123_450_000_000),
            boot_clock_resolution_ns: Some(10_000_000),
            boot_id: Some(boot.into()),
            time_namespace: Some("time:[42]".into()),
            mount_namespace: Some("mnt:[43]".into()),
            ..Default::default()
        };
        let metadata = deviceinfo::CaptureMetadata {
            schema_version: deviceinfo::SCHEMA_VERSION,
            context: deviceinfo::SampleContext::from_bounds(
                deviceinfo::ObservationOrigin::Captured,
                stamp.clone(),
                stamp.clone(),
            ),
            counters_preserved: true,
            devices: Vec::new(),
        };
        std::fs::write(
            root.join(deviceinfo::CONTEXT_FILE),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        let source = Source::local(&root);
        assert!(deviceinfo::SampleContext::read(&root).consistent);
        assert_eq!(source.stamp(true).unwrap(), stamp);
        assert_eq!(source.stamp(false).unwrap(), stamp);
        assert_eq!(
            source.fresh_stamp().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_a_batch_response_with_binary_content() {
        // 构造一个含 NUL 与 `@@` 的内容：定界必须靠长度，不能靠标记
        let content = b"a\0b@@F 999 fake\nc";
        let mut out = Vec::new();
        out.extend_from_slice(b"@@F ");
        out.extend_from_slice(content.len().to_string().as_bytes());
        out.extend_from_slice(b" /x/one\n");
        out.extend_from_slice(content);
        out.extend_from_slice(b"@@D /x/dev\n@@M /x/gone\n");

        let mut contents = BTreeMap::new();
        let mut kinds = BTreeMap::new();
        parse_batch(&out, &mut contents, &mut kinds, &mut Vec::new());

        assert_eq!(
            contents.get("/x/one").map(Vec::as_slice),
            Some(&content[..])
        );
        assert_eq!(kinds.get("/x/one"), Some(&EntryKind::File));
        assert_eq!(kinds.get("/x/dev"), Some(&EntryKind::Other));
        assert_eq!(kinds.get("/x/gone"), Some(&EntryKind::Missing));
        // 内容里的假头部不能被当成真的
        assert!(!kinds.contains_key("fake"));
    }

    /// 远端说"存在但我复制不出来"时，本地必须当**错误**，不能当缺失——
    /// 把读失败伪装成缺失，又会造出自洽但错误的夹具。
    #[test]
    fn a_read_failure_from_the_remote_is_collected_not_ignored() {
        let out = b"@@F 5 /x/ok\nhello@@X /x/broken\n@@M /x/gone\n";
        let mut contents = BTreeMap::new();
        let mut kinds = BTreeMap::new();
        let mut failures = Vec::new();
        parse_batch(out, &mut contents, &mut kinds, &mut failures);

        assert_eq!(failures, vec!["/x/broken"]);
        assert!(!kinds.contains_key("/x/broken"), "读失败不能被伪装成缺失");
        assert_eq!(kinds.get("/x/gone"), Some(&EntryKind::Missing));
        assert_eq!(
            contents.get("/x/ok").map(Vec::as_slice),
            Some(&b"hello"[..])
        );
    }

    #[test]
    fn parses_an_empty_batch_response() {
        let mut contents = BTreeMap::new();
        let mut kinds = BTreeMap::new();
        parse_batch(b"", &mut contents, &mut kinds, &mut Vec::new());
        assert!(contents.is_empty() && kinds.is_empty());
    }

    #[test]
    fn listing_markers_label_directories() {
        // `ls -1p` 给目录加尾 `/`；这个标记决定哪些名字能当库文件用
        let out = b"@@L usr/lib\naarch64-linux-gnu/\nlibz.so.1\n@@E\n@@L absent\n@@E\n";
        let listing = parse_listing(out);
        assert_eq!(
            listing["usr/lib"],
            vec![
                Entry {
                    name: "aarch64-linux-gnu".into(),
                    is_dir: true
                },
                Entry {
                    name: "libz.so.1".into(),
                    is_dir: false
                },
            ]
        );
        assert!(listing["absent"].is_empty());
    }

    #[test]
    fn batch_directory_marker_does_not_become_a_regular_file() {
        let mut contents = BTreeMap::new();
        let mut kinds = BTreeMap::new();
        let mut failures = Vec::new();
        parse_batch(
            b"@@R sys/firmware/efi\n@@D dev/sensor\n",
            &mut contents,
            &mut kinds,
            &mut failures,
        );
        assert_eq!(kinds["sys/firmware/efi"], EntryKind::Directory);
        assert_eq!(kinds["dev/sensor"], EntryKind::Other);
        assert!(contents.is_empty());
        assert!(failures.is_empty());
    }

    #[test]
    fn link_markers_separate_targets_from_absence() {
        let out = b"@@K a/driver\n@@T ../../../bus/platform/drivers/panthor\n@@K a/notalink\n@@N\n";
        let links = parse_links(out);
        assert_eq!(
            links["a/driver"].as_deref(),
            Some("../../../bus/platform/drivers/panthor")
        );
        assert_eq!(links["a/notalink"], None);
    }

    #[test]
    fn local_listing_marks_directories_and_absent_dirs() {
        let root = std::env::temp_dir().join(format!("deviceinfo-src-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("d/sub")).unwrap();
        std::fs::write(root.join("d/plain"), b"x").unwrap();

        let source = Source::local(&root);
        let dirs = vec!["d".to_string(), "d/absent".to_string()];
        let listing = source.list_many(&dirs).expect("本地列表不会失败");

        let mut names: Vec<(String, bool)> = listing["d"]
            .iter()
            .map(|entry| (entry.name.clone(), entry.is_dir))
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![("plain".to_string(), false), ("sub".to_string(), true)]
        );
        assert!(listing["d/absent"].is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn local_source_reads_links_and_files_and_absence() {
        let root = std::env::temp_dir().join(format!("deviceinfo-src2-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("d/inner")).unwrap();
        std::fs::write(root.join("d/one.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("inner", root.join("d/link")).unwrap();

        let source = Source::local(&root);
        assert_eq!(source.read("d/one.txt").as_deref(), Some(&b"hello"[..]));
        assert_eq!(source.kind("d/one.txt"), EntryKind::File);
        assert_eq!(source.kind("d/inner"), EntryKind::Directory, "保留目录类型");
        assert_eq!(source.kind("d/nope"), EntryKind::Missing);
        assert_eq!(source.read("d/nope"), None);

        let links = source
            .link_many(&["d/link".to_string(), "d/one.txt".to_string()])
            .expect("本地读软链不会失败");
        assert_eq!(links["d/link"].as_deref(), Some("inner"));
        assert_eq!(links["d/one.txt"], None, "普通文件不是软链");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stdin_input_always_ends_with_a_newline() {
        // 少了这个换行，远端 `while read` 会把最后一行丢掉
        assert_eq!(stdin_lines(&["a".into(), "b".into()]), b"a\nb\n");
        assert_eq!(stdin_lines(&["only".into()]), b"only\n");
        assert_eq!(stdin_lines(&[]), b"\n");
    }

    #[test]
    fn source_stamp_preserves_precision_and_rejects_reboot_or_broken_protocol() {
        let boot = "01234567-89ab-cdef-0123-456789abcdef";
        let raw = format!("{boot}\n123.45\n1792000000123456789\ntime:[42]\nmnt:[43]\n{boot}\n");
        let stamp = parse_stamp(raw.as_bytes()).unwrap();
        assert_eq!(stamp.boot_time_ns, Some(123_450_000_000));
        assert_eq!(stamp.boot_clock_resolution_ns, Some(10_000_000));
        assert_eq!(stamp.unix_time_ns, Some(1_792_000_000_123_456_789));
        let reset = raw.replace(
            &format!("\n{boot}\n"),
            "\nfedcba98-7654-3210-fedc-ba9876543210\n",
        );
        assert_eq!(parse_stamp(reset.as_bytes()).unwrap().boot_id, None);
        assert!(parse_stamp(b"partial\n").is_err());
        assert_eq!(
            parse_stamp(raw.replace("123.45", "-1.00").as_bytes())
                .unwrap()
                .boot_time_ns,
            None
        );
        assert_eq!(
            parse_stamp(
                raw.replace("1792000000123456789", "1792000000%N")
                    .as_bytes()
            )
            .unwrap()
            .unix_time_ns,
            None
        );
    }

    #[test]
    fn source_associations_keep_kernel_instance_and_explicit_unknown_fields() {
        let devices = parse_associations(b"/sys/class/accel/accel0\t/sys/devices/pci0000:00/0000:00:0b.0\tintel_vpu\t261:0\t0:123:0:456\n/sys/class/hwmon/hwmon0\t/sys/devices/platform/thermal\t\t\t\n").unwrap();
        assert_eq!(devices[0].kernel_instance.as_deref(), Some("0:123:0:456"));
        assert_eq!(devices[0].device_number.as_deref(), Some("261:0"));
        assert_eq!(devices[1].kernel_instance, None);
        assert_eq!(devices[1].driver, None);
        assert!(parse_associations(b"partial\n").is_err());
    }

    /// 找一个**真的** busybox（不是那种只转发几条命令的 wrapper 脚本）。
    fn find_busybox() -> Option<PathBuf> {
        let usable = |path: &Path| {
            Command::new(path)
                .arg("--help")
                .output()
                .map(|out| String::from_utf8_lossy(&out.stdout).contains("BusyBox v"))
                .unwrap_or(false)
        };
        for candidate in [
            "/usr/lib/initcpio/busybox",
            "/usr/bin/busybox",
            "/bin/busybox",
        ] {
            let path = PathBuf::from(candidate);
            if usable(&path) {
                return Some(path);
            }
        }
        let in_path = PathBuf::from("busybox");
        usable(&in_path).then_some(in_path)
    }

    /// **远端脚本必须能在 busybox 上跑。**
    ///
    /// 目标机常常是 OpenWrt / 嵌入式，那边只有 busybox：`ls -1p`、`readlink --`、
    /// `wc -c` 都有，但 `find -printf`、`readlink -f` 这类 GNU 扩展没有。
    /// 以前这只是"我读了一遍觉得应该行"——现在拿本机的 busybox 真跑一遍。
    /// 没有 busybox 的环境直接跳过（不让别人因为缺个工具就红）。
    #[test]
    fn remote_scripts_all_run_under_busybox() {
        let Some(busybox) = find_busybox() else {
            eprintln!("跳过：本机没有可用的 busybox");
            return;
        };

        let root = std::env::temp_dir().join(format!("deviceinfo-bb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("d");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("one.txt"), b"hello\n").unwrap();
        std::fs::write(dir.join("two.txt"), b"world\n").unwrap();
        std::os::unix::fs::symlink("inner", dir.join("link")).unwrap();

        // busybox 按 argv[0] 选 applet，所以造一个指向它的软链农场当 PATH
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for applet in [
            "ls", "readlink", "cat", "wc", "printf", "test", "[", "date", "stat",
        ] {
            std::os::unix::fs::symlink(&busybox, bin.join(applet)).unwrap();
        }

        let run = |script: &str, input: &str| -> Vec<u8> {
            let mut child = Command::new(&busybox)
                .args(["sh", "-c", script])
                .env("PATH", &bin)
                // 让脚本在一个空环境里跑，免得继承本机的东西
                .env_remove("IFS")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("busybox 应当能启动");
            let mut stdin = child.stdin.take().expect("piped");
            stdin.write_all(input.as_bytes()).unwrap();
            drop(stdin);
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "busybox 跑挂了: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };

        // 列目录：`ls -1p` 必须给目录加尾 `/`（这是判 is_dir 的唯一依据）
        let dir_text = dir.display().to_string();
        let listing = parse_listing(&run(REMOTE_LIST_SCRIPT, &format!("{dir_text}\n")));
        let mut names: Vec<(String, bool)> = listing[&dir_text]
            .iter()
            .map(|entry| (entry.name.clone(), entry.is_dir))
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                ("inner".to_string(), true),
                ("link".to_string(), false),
                ("one.txt".to_string(), false),
                ("two.txt".to_string(), false),
            ]
        );

        // 读软链：软链给目标，普通文件给 `@@N`
        let link = dir.join("link").display().to_string();
        let plain = dir.join("one.txt").display().to_string();
        let missing = dir.join("nope").display().to_string();
        let links = parse_links(&run(
            REMOTE_LINK_SCRIPT,
            &format!("{link}\n{plain}\n{missing}\n"),
        ));
        assert_eq!(links[&link].as_deref(), Some("inner"));
        assert_eq!(links[&plain], None);
        assert_eq!(links[&missing], None);

        // 取内容：普通文件带内容，设备节点式的不存在路径报 M
        let one = dir.join("one.txt").display().to_string();
        let mut contents = BTreeMap::new();
        let mut kinds = BTreeMap::new();
        let mut failures = Vec::new();
        let inner = dir.join("inner").display().to_string();
        parse_batch(
            &run(
                &remote_fetch_script(),
                &format!("c {one}\nc {missing}\ne {inner}\n"),
            ),
            &mut contents,
            &mut kinds,
            &mut failures,
        );
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(contents[&one], b"hello\n");
        assert_eq!(kinds[&one], EntryKind::File);
        assert_eq!(kinds[&missing], EntryKind::Missing);
        assert_eq!(kinds[&inner], EntryKind::Directory);

        #[cfg(target_os = "linux")]
        {
            let stamp = parse_stamp(&run(REMOTE_STAMP_SCRIPT, "")).unwrap();
            let context = deviceinfo::SampleContext::from_bounds(
                deviceinfo::ObservationOrigin::Captured,
                stamp.clone(),
                stamp,
            );
            assert!(context.consistent, "{context:?}");
            let devices = parse_associations(&run(
                REMOTE_ASSOCIATIONS_SCRIPT,
                &format!("{}\n", dir_text.trim_start_matches('/')),
            ))
            .unwrap();
            assert_eq!(devices.len(), 1);
            assert_eq!(devices[0].source, dir);
            assert!(devices[0].kernel_instance.is_some());
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn paths_with_quotes_do_not_become_commands() {
        // 单引号会把 shell 引号配平掉——这种路径宁可读不到，也不要拼出别的命令
        assert_eq!(quoted("a'b"), "''");
        assert_eq!(quoted("/proc/cpuinfo"), "'/proc/cpuinfo'");
    }
}
