//! 声明标签的**文件格式**。
//!
//! 存在的理由是：读标签的和写标签的不是同一个程序。deviceinfo 只读文件，写标签属于
//! 另一个工具（envsetupd）——两边各抄一份格式，迟早会漂移（规范化规则一改，写进去的
//! 标签就读不出来了）。所以格式归这里管，两边都引它。
//!
//! 格式本身极简：一行若干标签，`#` 开头是注释，空行忽略。**规范化是刻意的**（转小写、
//! `_` 换成 `-`）：标签用来**匹配**，大小写或下划线不一致会让路由静默失配——"任务永远
//! 找不到这台机器"这种故障极难查。其他字符一律拒绝并出声，理由同上。

/// 机器自己的声明。**这是唯一适合被工具改写的地方**——另外两个分别属于用户和发行版。
pub const MACHINE_TAGS_PATH: &str = "/etc/deviceinfo/tags.conf";

/// 发行版/镜像预置的声明。工具不该改它：升级镜像时会丢。
pub const IMAGE_TAGS_PATH: &str = "/usr/share/deviceinfo/tags.conf";

/// 分片目录，方便按用途拆开写（比如按角色一个文件）。
pub const TAG_CONFIG_DIR: &str = "/etc/deviceinfo/tags.d";

/// 声明文件的候选顺序。**管理员的优先**：硬件产品随附的声明（"这台是随身超算"）装在
/// `usr/share`，用户改自己的角色时不必去动厂商的文件。两处都会读，是合并而不是覆盖。
/// 都是**机器上的绝对路径**（报告里记的就是这个）。要拼探测根的一方自己剥掉开头的
/// `/`——`Path::join` 遇到绝对路径会把前面整段替换掉，这个坑踩过一次。
pub fn config_paths() -> [&'static str; 2] {
    [MACHINE_TAGS_PATH, IMAGE_TAGS_PATH]
}

/// 一份声明文件里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagLine {
    /// 合法的标签，`line` 从 1 起（警告和删除都要指到那一行）。
    Tag { line: usize, tag: String },
    /// 不合法但非空的 token——**不能安静丢掉**，一个拼错的标签会让路由静默失配。
    Invalid { line: usize, token: String },
}

/// 解析一份声明文件的内容。注释行和空行不产生条目。
pub fn parse(contents: &str) -> Vec<TagLine> {
    let mut found = Vec::new();
    for (index, raw) in contents.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        for token in trimmed.split_whitespace() {
            found.push(match normalize(token) {
                Some(tag) => TagLine::Tag { line, tag },
                None => TagLine::Invalid {
                    line,
                    token: token.to_string(),
                },
            });
        }
    }
    found
}

/// 规范化一个标签：转小写、`_` 换成 `-`。含非法字符返回 `None`（由调用方出声）。
pub fn normalize(token: &str) -> Option<String> {
    if token.is_empty()
        || !token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return None;
    }
    Some(token.to_ascii_lowercase().replace('_', "-"))
}

/// 一台机器**一直开着**。
///
/// 探测不出来，只能人为声明。它是任务路由里最常用的一个："这条任务要常驻"的前提是
/// 目标不会去睡觉。
pub const ALWAYS_ON: &str = "always-on";

/// 省电优先的那台。和 [`ALWAYS_ON`] 并不冲突：NAS 可以既常开又省电，
/// 而"有预填充的大任务交给它"要看的就是后者。
pub const POWERSAVE: &str = "powersave";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_comments_and_inline_tokens() {
        let parsed = parse("# 注释\n\nalways-on  powersave\n# 又一行注释\n奇奇怪怪!的标签\n");
        assert_eq!(
            parsed,
            vec![
                TagLine::Tag {
                    line: 3,
                    tag: "always-on".into()
                },
                TagLine::Tag {
                    line: 3,
                    tag: "powersave".into()
                },
                TagLine::Invalid {
                    line: 5,
                    token: "奇奇怪怪!的标签".into()
                },
            ]
        );
    }

    /// 规范化是**匹配**的前提：`Always_On` 和 `always-on` 必须是同一个标签，
    /// 否则路由会静默失配。
    #[test]
    fn normalization_is_not_cosmetic() {
        assert_eq!(normalize("Always_On").as_deref(), Some("always-on"));
        assert_eq!(normalize("always-on").as_deref(), Some("always-on"));
        assert_eq!(normalize("ns-prefix.tag").as_deref(), Some("ns-prefix.tag"));
        assert_eq!(normalize("有中文"), None);
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("a/b"), None);
    }

    /// 行号要准——警告里指错行，比不警告还坏。
    #[test]
    fn line_numbers_are_one_based_and_survive_blank_lines() {
        let parsed = parse("\n\n\n");
        assert!(parsed.is_empty());
        let parsed = parse("x\n\n\n y \n");
        assert_eq!(
            parsed,
            vec![
                TagLine::Tag {
                    line: 1,
                    tag: "x".into()
                },
                TagLine::Tag {
                    line: 4,
                    tag: "y".into()
                },
            ]
        );
    }
}
