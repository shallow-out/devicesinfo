//! 部署所需的系统身份，只读 os-release 与内核导出的发行版本。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) const INPUTS: [&str; 3] = [
    "etc/os-release",
    "usr/lib/os-release",
    "proc/sys/kernel/osrelease",
];

/// 发行版自报的身份。缺字段保持未知，不由包管理器推测发行版。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingSystem {
    /// 被探测机器上的来源路径。
    pub source: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub id_like: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pretty_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

pub(crate) fn probe(root: &Path, warnings: &mut Vec<String>) -> Option<OperatingSystem> {
    // /etc 优先，不能合并发行版默认值，也不能因管理员文件损坏就静默退回默认值。
    for relative in &INPUTS[..2] {
        let source = Path::new("/").join(relative);
        let text = match crate::resolve_path_in_root(root, relative).and_then(fs::read_to_string) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                warnings.push(format!("读不到 {}：{error}", source.display()));
                return None;
            }
        };
        let mut values = BTreeMap::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                warnings.push(format!(
                    "{} 第 {} 行不是键值声明",
                    source.display(),
                    index + 1
                ));
                continue;
            };
            let key = key.trim();
            if !["ID", "ID_LIKE", "NAME", "PRETTY_NAME", "VERSION_ID"].contains(&key) {
                continue;
            }
            match parse_value(value.trim()) {
                Some(value) => {
                    if values.insert(key, value).is_some() {
                        warnings.push(format!(
                            "{} 第 {} 行重复声明 {key}，使用最后一次声明",
                            source.display(),
                            index + 1
                        ));
                    }
                }
                None => warnings.push(format!(
                    "{} 第 {} 行的 {key} 格式不合法，已跳过",
                    source.display(),
                    index + 1
                )),
            }
        }
        let take = |key| values.get(key).filter(|value| !value.is_empty()).cloned();
        return Some(OperatingSystem {
            source,
            id: take("ID"),
            id_like: take("ID_LIKE")
                .map(|value| value.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            name: take("NAME"),
            pretty_name: take("PRETTY_NAME"),
            version_id: take("VERSION_ID"),
        });
    }
    None
}

/// 读取 shell 风格的字符串，不执行文件、不展开变量或命令，也不接受字符串拼接。
fn parse_value(value: &str) -> Option<String> {
    let mut chars = value.chars().peekable();
    let quote = match chars.peek() {
        Some('\'' | '"') => chars.next(),
        _ => None,
    };
    let mut result = String::new();
    while let Some(c) = chars.next() {
        if Some(c) == quote {
            let rest: String = chars.collect();
            return (rest.trim().is_empty() || rest.trim_start().starts_with('#'))
                .then_some(result);
        }
        if quote.is_none() && c.is_whitespace() {
            let rest: String = chars.collect();
            return (rest.trim().is_empty() || rest.trim_start().starts_with('#'))
                .then_some(result);
        }
        if c == '\\' && quote != Some('\'') {
            let next = chars.next()?;
            if quote == Some('"') && !['"', '\\', '$', '`'].contains(&next) {
                result.push('\\');
            }
            result.push(next);
        } else if quote.is_none() && ['\'', '"'].contains(&c) {
            return None;
        } else {
            result.push(c);
        }
    }
    quote.is_none().then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_quoting_is_decoded_without_shell_evaluation() {
        assert_eq!(parse_value("arch"), Some("arch".into()));
        assert_eq!(
            parse_value("\"Debian GNU/Linux\""),
            Some("Debian GNU/Linux".into())
        );
        assert_eq!(parse_value("'debian ubuntu'"), Some("debian ubuntu".into()));
        assert_eq!(
            parse_value(r#""A \"quote\" and \$value""#),
            Some("A \"quote\" and $value".into())
        );
        assert_eq!(
            parse_value("\"$(touch /tmp/example)\""),
            Some("$(touch /tmp/example)".into())
        );
        assert_eq!(parse_value("arch # comment"), Some("arch".into()));
        assert_eq!(parse_value("\"unfinished"), None);
        assert_eq!(parse_value("\"one\"\"two\""), None);
        assert_eq!(parse_value("Arch Linux"), None);
    }

    #[test]
    fn administrator_identity_takes_precedence_and_reports_invalid_values() {
        let root = std::env::temp_dir().join(format!("deviceinfo-os-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::write(
            root.join("usr/lib/os-release"),
            "ID=default\nVERSION_ID=99\n",
        )
        .unwrap();
        fs::write(root.join("etc/os-release"), "ID=first\nID=ubuntu\nID_LIKE='debian linux'\nPRETTY_NAME=\"Ubuntu Linux\"\nVERSION_ID=\"broken\n").unwrap();
        let mut warnings = Vec::new();
        let os = probe(&root, &mut warnings).unwrap();
        assert_eq!(os.id.as_deref(), Some("ubuntu"));
        assert_eq!(os.id_like, ["debian", "linux"]);
        assert_eq!(os.pretty_name.as_deref(), Some("Ubuntu Linux"));
        assert_eq!(os.version_id, None);
        assert_eq!(os.source, Path::new("/etc/os-release"));
        assert_eq!(warnings.len(), 2);
        fs::remove_file(root.join("etc/os-release")).unwrap();
        let os = probe(&root, &mut Vec::new()).unwrap();
        assert_eq!(os.id.as_deref(), Some("default"));
        assert_eq!(os.source, Path::new("/usr/lib/os-release"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn absolute_symlinks_are_resolved_inside_the_target_and_loops_warn() {
        let root = std::env::temp_dir().join(format!("deviceinfo-os-links-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::create_dir_all(root.join("opt/vendor/lib")).unwrap();
        fs::write(root.join("opt/vendor/lib/os-release"), "ID=fixture-only\n").unwrap();
        std::os::unix::fs::symlink("/opt/vendor", root.join("usr")).unwrap();
        std::os::unix::fs::symlink("/usr/lib/os-release", root.join("etc/os-release")).unwrap();
        let mut warnings = Vec::new();
        assert_eq!(
            probe(&root, &mut warnings).unwrap().id.as_deref(),
            Some("fixture-only")
        );
        assert!(warnings.is_empty());
        fs::remove_file(root.join("etc/os-release")).unwrap();
        std::os::unix::fs::symlink("os-release", root.join("etc/os-release")).unwrap();
        assert_eq!(probe(&root, &mut warnings), None);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("软链循环"));
        fs::remove_dir_all(root).unwrap();
    }
}
