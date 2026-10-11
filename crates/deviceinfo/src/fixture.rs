//! 夹具的"检出后修复"。
//!
//! 夹具是要提交进 Git 的，而 **Git 不跟踪空目录**。偏偏有些目录的**存在本身**就是事实：
//! `<card>/device/drm` 是空是缺，决定这张卡算"只会显示输出的显示设备"还是"判不出来"。
//! `deviceinfo capture` 会把观测到的空目录写进 `meta.json` 的 `empty_directories`，
//! 检出夹具之后调用 [`materialize`]，探测才看得到和采集当时一样的树。

use std::fs;
use std::io;
use std::path::Path;

/// 按 `meta.json` 里的 `empty_directories` 把空目录补回来。
///
/// 返回**实际新建**的相对路径（本来就是目录的不动）。没有 `meta.json` 时返回空表——
/// 那不是错误：不是每棵树都来自 `capture`。
pub fn materialize(root: &Path) -> io::Result<Vec<String>> {
    let text = match fs::read_to_string(root.join("meta.json")) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let meta: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("meta.json 不是 JSON：{error}"),
        )
    })?;
    let mut created = Vec::new();
    let Some(list) = meta
        .get("empty_directories")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(created);
    };
    for relative in list.iter().filter_map(serde_json::Value::as_str) {
        // 路径必须是相对的：夹具是可提交的东西，一个写进 meta 的绝对路径会把
        // 探测引到采集机的真实文件系统上去。
        if Path::new(relative).is_absolute() || relative.split('/').any(|part| part == "..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("meta.json 里的空目录是绝对路径或越界：{relative}"),
            ));
        }
        let path = root.join(relative);
        if !path.is_dir() {
            fs::create_dir_all(&path)?;
            created.push(relative.to_string());
        }
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("deviceinfo-fixture-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn observed_empty_directories_are_restored_and_nothing_else_is() {
        let dir = tree("empty");
        fs::write(
            dir.join("meta.json"),
            r#"{"empty_directories":["sys/class/drm/card0/device/drm","sys/class/drm/card0"]}"#,
        )
        .unwrap();
        let created = materialize(&dir).unwrap();
        // 深的那条先建，父目录顺带就有了 —— created 只记**实际新建**的
        assert_eq!(created, vec!["sys/class/drm/card0/device/drm".to_string()]);
        assert!(dir.join("sys/class/drm/card0/device/drm").is_dir());
        assert!(dir.join("sys/class/drm/card0").is_dir());
        // 第二次调用：已经是目录了，不动
        assert!(materialize(&dir).unwrap().is_empty());
    }

    #[test]
    fn a_missing_meta_is_not_an_error_and_absolute_paths_are_refused() {
        let dir = tree("nometa");
        assert!(materialize(&dir).unwrap().is_empty());

        fs::write(dir.join("meta.json"), r#"{"empty_directories":["/etc"]}"#).unwrap();
        let error = materialize(&dir).unwrap_err().to_string();
        assert!(error.contains("绝对路径"), "{error}");

        fs::write(dir.join("meta.json"), r#"{"empty_directories":["../escaped"]}"#).unwrap();
        assert!(materialize(&dir).unwrap_err().to_string().contains("绝对路径"));
    }
}
