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
    /// 存在，但不是普通文件：设备节点、目录、断掉的软链。
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
    pub(crate) fn local(root: &Path) -> Self {
        Self::Local(root.to_path_buf())
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
    pub(crate) fn list_many(&self, dirs: &[String]) -> BTreeMap<String, Vec<Entry>> {
        match self {
            Self::Local(root) => dirs
                .iter()
                .map(|dir| (dir.clone(), list_local(root, dir)))
                .collect(),
            Self::Remote(remote) => {
                let script = "\
while IFS= read -r d; do
  printf '@@L %s\\n' \"$d\"
  ls -1p -- \"$d\" 2>/dev/null
  printf '@@E\\n'
done";
                remote
                    .run_with_stdin(script, &stdin_lines(dirs))
                    .map(|out| parse_listing(&out))
                    .unwrap_or_default()
            }
        }
    }

    /// 一次取回多个路径的软链目标。不是软链、或读不到，对应 `None`。
    pub(crate) fn link_many(&self, paths: &[String]) -> BTreeMap<String, Option<String>> {
        match self {
            Self::Local(root) => paths
                .iter()
                .map(|path| {
                    let target = std::fs::read_link(root.join(path))
                        .ok()
                        .and_then(|target| target.to_str().map(str::to_string));
                    (path.clone(), target)
                })
                .collect(),
            Self::Remote(remote) => {
                let script = "\
while IFS= read -r p; do
  printf '@@K %s\\n' \"$p\"
  t=$(readlink -- \"$p\" 2>/dev/null)
  if [ -n \"$t\" ]; then printf '@@T %s\\n' \"$t\"; else printf '@@N\\n'; fi
done";
                remote
                    .run_with_stdin(script, &stdin_lines(paths))
                    .map(|out| parse_links(&out))
                    .unwrap_or_default()
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
            Self::Local(root) => match std::fs::metadata(root.join(path)) {
                Ok(meta) if meta.is_file() => EntryKind::File,
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
                            "if [ -f {0} ]; then echo F; elif [ -e {0} ]; then echo D; else echo M; fi",
                            quoted(path)
                        ))
                        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
                        .unwrap_or_else(|_| "M".into());
                    match answer.as_str() {
                        "F" => EntryKind::File,
                        "D" => EntryKind::Other,
                        _ => EntryKind::Missing,
                    }
                }),
        }
    }

    /// 读文件内容。读不到返回 `None`。
    pub(crate) fn read(&self, path: &str) -> Option<Vec<u8>> {
        match self {
            Self::Local(root) => std::fs::read(root.join(path)).ok(),
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
    pub(crate) fn prefetch(&self, paths: &[String]) {
        let Self::Remote(remote) = self else {
            return;
        };
        let script = "\
while IFS= read -r p; do
  if [ -f \"$p\" ]; then
    n=$(wc -c < \"$p\" 2>/dev/null) || n=0
    printf '@@F %s %s\\n' \"$n\" \"$p\"
    cat -- \"$p\"
  elif [ -e \"$p\" ]; then
    printf '@@D %s\\n' \"$p\"
  else
    printf '@@M %s\\n' \"$p\"
  fi
done";
        let Ok(out) = remote.run_with_stdin(script, &stdin_lines(paths)) else {
            return;
        };
        parse_batch(
            &out,
            &mut remote.contents.borrow_mut(),
            &mut remote.kinds.borrow_mut(),
        );
    }
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
fn quoted(path: &str) -> String {
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
            result.entry(dir.clone()).or_default().push(Entry { name, is_dir });
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
            (Some("@@M"), Some(path), None) => {
                kinds.insert(path.to_string(), EntryKind::Missing);
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
        parse_batch(&out, &mut contents, &mut kinds);

        assert_eq!(contents.get("/x/one").map(Vec::as_slice), Some(&content[..]));
        assert_eq!(kinds.get("/x/one"), Some(&EntryKind::File));
        assert_eq!(kinds.get("/x/dev"), Some(&EntryKind::Other));
        assert_eq!(kinds.get("/x/gone"), Some(&EntryKind::Missing));
        // 内容里的假头部不能被当成真的
        assert!(!kinds.contains_key("fake"));
    }

    #[test]
    fn parses_an_empty_batch_response() {
        let mut contents = BTreeMap::new();
        let mut kinds = BTreeMap::new();
        parse_batch(b"", &mut contents, &mut kinds);
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
        let listing = source.list_many(&dirs);

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
        assert_eq!(source.kind("d/inner"), EntryKind::Other, "目录不是普通文件");
        assert_eq!(source.kind("d/nope"), EntryKind::Missing);
        assert_eq!(source.read("d/nope"), None);

        let links = source.link_many(&["d/link".to_string(), "d/one.txt".to_string()]);
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
    fn paths_with_quotes_do_not_become_commands() {
        // 单引号会把 shell 引号配平掉——这种路径宁可读不到，也不要拼出别的命令
        assert_eq!(quoted("a'b"), "''");
        assert_eq!(quoted("/proc/cpuinfo"), "'/proc/cpuinfo'");
    }
}
