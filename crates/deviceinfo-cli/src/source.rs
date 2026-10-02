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
//! # 为什么有 `prefetch`
//!
//! 采集在动手之前就有完整的文件清单，所以远端实现可以**一次 ssh 把所有内容取回来**，
//! 而不是每个文件开一次连接（那会变成上百次握手）。
//!
//! stdout 上跑的是字节协议（`@@F <字节数> <路径>` 后面跟原始内容），不用 base64：
//! `ssh` 传二进制是安全的，而**按字节数截断**比找分隔符可靠——文件内容里什么都可能有。

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

    /// 列一个目录下的条目名（不递归）。
    pub(crate) fn list(&self, dir: &str) -> Vec<String> {
        match self {
            Self::Local(root) => list_local(root, dir),
            Self::Remote(remote) => remote.run(&format!("ls -1 -- {} 2>/dev/null", quoted(dir)))
                .map(|out| lines_of(&out))
                .unwrap_or_default(),
        }
    }

    /// 列一个目录下的**非目录**条目名。
    ///
    /// 用 `ls -1p`（`-p` 给目录名加 `/`）而不是 `find -type f`：库文件大多是软链，
    /// `-type f` 会把它们全漏掉；`-printf` 又是 GNU 专有，busybox 上没有。
    ///
    /// 需要"只要非目录"是有原因的：`LIBRARY_DIRS` 里既有 `usr/lib` 又有
    /// `usr/lib/aarch64-linux-gnu`，而 `usr/lib` 的列表里就包含 `aarch64-linux-gnu`
    /// 这个**目录名**——把它当库文件写成普通文件，下一步往它里面写就会 `EEXIST`。
    pub(crate) fn list_files(&self, dir: &str) -> Vec<String> {
        match self {
            Self::Local(root) => std::fs::read_dir(root.join(dir))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| !entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect(),
            Self::Remote(remote) => remote
                .run(&format!("ls -1p -- {} 2>/dev/null", quoted(dir)))
                .map(|out| {
                    lines_of(&out)
                        .into_iter()
                        .filter(|line| !line.ends_with('/'))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// 软链的目标字符串（不解析成绝对路径）。
    pub(crate) fn read_link(&self, path: &str) -> Option<String> {
        match self {
            Self::Local(root) => std::fs::read_link(root.join(path))
                .ok()?
                .to_str()
                .map(str::to_string),
            Self::Remote(remote) => {
                let out = remote
                    .run(&format!("readlink -- {} 2>/dev/null", quoted(path)))
                    .ok()?;
                let text = String::from_utf8_lossy(&out).trim().to_string();
                (!text.is_empty()).then_some(text)
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
        self.lookup(path)
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
        let input = paths.join("\n");
        let Ok(out) = remote.run_with_stdin(script, input.as_bytes()) else {
            return;
        };
        parse_batch(&out, &mut remote.contents.borrow_mut(), &mut remote.kinds.borrow_mut());
    }

    fn lookup(&self, path: &str) -> EntryKind {
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

        // 连接复用。实测单次 ssh 往返要 770ms（握手 + 认证 + 远端 shell 启动），
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
        }
        // 显式丢掉 stdin，让远端 `while read` 看到 EOF
        drop(child.stdin.take());
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

fn list_local(root: &Path, dir: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

fn lines_of(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
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
    fn local_source_reads_files_links_and_absence() {
        let root = std::env::temp_dir().join(format!("deviceinfo-src-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("d/inner")).unwrap();
        std::fs::write(root.join("d/one.txt"), b"hello").unwrap();
        std::fs::create_dir_all(root.join("d/other")).unwrap();
        std::os::unix::fs::symlink("inner", root.join("d/link")).unwrap();

        let source = Source::local(&root);
        assert_eq!(source.read("d/one.txt").as_deref(), Some(&b"hello"[..]));
        assert_eq!(source.kind("d/one.txt"), EntryKind::File);
        assert_eq!(source.kind("d/other"), EntryKind::Other, "目录不是普通文件");
        assert_eq!(source.kind("d/nope"), EntryKind::Missing);
        assert_eq!(source.read_link("d/link").as_deref(), Some("inner"));
        assert_eq!(source.read("d/nope"), None);

        let mut listed = source.list("d");
        listed.sort();
        assert_eq!(listed, vec!["inner", "link", "one.txt", "other"]);
        assert!(source.list("d/absent").is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn paths_with_quotes_do_not_become_commands() {
        // 单引号会把 shell 引号配平掉——这种路径宁可读不到，也不要拼出别的命令
        assert_eq!(quoted("a'b"), "''");
        assert_eq!(quoted("/proc/cpuinfo"), "'/proc/cpuinfo'");
    }
}
