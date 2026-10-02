//! 用户态运行时探测。
//!
//! 设备节点存在只说明**驱动认了这块硬件**，不说明**应用能用它跑推理**。
//! Intel NPU 还需要编译器（由独立的 `intel-npu-compiler` 包提供，不随驱动一起装），
//! Intel GPU 还需要 Level Zero 或 OpenCL 运行时。缺这一层时，模型会被选中、
//! 然后在使用时失败。
//!
//! # 为什么是"看文件在不在"
//!
//! 要真正验证可用性，得 dlopen 一遍整个推理栈或者跑一次最小推理——那要求本 crate
//! 链接 OpenVINO/Level Zero，把"硬件探测"变成一个重依赖的组件，不值得。
//! 所以判据是库文件是否存在，并且**明确承认这是启发式**：
//! 认不出的 (kind, vendor) 组合返回 [`RuntimeStatus::Unknown`]，
//! 而不是猜一个"就绪"。
//!
//! # 为什么不读 `ld.so.conf`
//!
//! 那是动态链接器的配置，解析它要处理 `include`、hwcap、各种发行版方言和不存在的目录。
//! 收益不抵复杂度：多列几个候选目录就能覆盖绝大多数发行版。

use crate::report::{AcceleratorKind, RuntimeStatus};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// 库文件的候选目录（相对探测根）。
///
/// 发行版把同一批库放在不同地方：`/usr/lib`、`/usr/lib64`、Debian 系的 multiarch 目录。
/// 全部列出来比"猜一个再修"便宜。
///
/// 公开是因为采集夹具的一方需要按同一份清单去目标机上取库名——两边的清单必须一致，
/// 不能各写一份。
pub const LIBRARY_DIRS: [&str; 7] = [
    "usr/lib",
    "usr/lib64",
    "usr/local/lib",
    "usr/lib/x86_64-linux-gnu",
    "usr/lib/aarch64-linux-gnu",
    "usr/lib/arm-linux-gnueabihf",
    "usr/lib/riscv64-linux-gnu",
];

/// 一个已知加速栈的组成。
///
/// `parts` 的每一项是一组**候选**文件名前缀，命中任一即算这一项满足——
/// 因为同一个能力常有多条等效路径（Intel GPU 走 Level Zero 或 OpenCL 都行）。
struct StackSpec {
    /// 给人看的栈名，出现在报告里。
    stack: &'static str,
    parts: &'static [&'static [&'static str]],
}

/// Intel NPU：Level Zero 加载器 + NPU 驱动 + OpenVINO NPU 编译器。
///
/// 编译器那一项**经常缺**：它是独立的发行版包（Arch 上是 `intel-npu-compiler`），
/// 不随驱动一起装。缺它的话模型能选中、编译时报错——正是这里要拦住的情况。
const INTEL_NPU: StackSpec = StackSpec {
    stack: "OpenVINO NPU",
    parts: &[
        // 所有 Intel 加速器共用的加载器
        &["libze_loader.so"],
        // Level Zero 的 NPU 后端驱动
        &["libze_intel_npu.so"],
        // 设备上的模型编译器（intel-npu-compiler 包）
        &["libopenvino_intel_npu_compiler_loader.so"],
    ],
};

/// Intel GPU：Level Zero GPU 驱动**或** OpenCL 运行时，命中一个就够
/// （OpenVINO 的 GPU 插件两条路都支持）。
const INTEL_GPU: StackSpec = StackSpec {
    stack: "Level Zero / OpenCL",
    parts: &[&["libze_intel_gpu.so", "libOpenCL.so"]],
};

/// 候选目录下所有库文件名的索引。
///
/// **构建一次，所有加速器共用**：每次探测都重新遍历 `/usr/lib`（几千个条目）
/// 是没必要的浪费，而一台机器上通常有好几个加速器。
pub(crate) struct LibraryIndex {
    names: BTreeSet<String>,
}

impl LibraryIndex {
    pub(crate) fn build(root: &Path) -> Self {
        let mut names = BTreeSet::new();
        for dir in LIBRARY_DIRS {
            let Ok(entries) = fs::read_dir(root.join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    names.insert(name.to_string());
                }
            }
        }
        Self { names }
    }

    /// 命中的第一个文件名。候选按给定顺序试，所以列候选时把更具体的放前面。
    fn hit(&self, candidates: &[&str]) -> Option<String> {
        for candidate in candidates {
            if let Some(name) = self
                .names
                .iter()
                .find(|name| name.starts_with(*candidate))
            {
                return Some(name.clone());
            }
        }
        None
    }
}

/// 按 (kind, vendor) 选栈。**认不出就返回 `None`**，由调用方转成 `Unknown`。
fn spec_for(kind: AcceleratorKind, vendor: Option<&str>) -> Option<&'static StackSpec> {
    match (kind, vendor) {
        (AcceleratorKind::Npu, Some("Intel")) => Some(&INTEL_NPU),
        (AcceleratorKind::Gpu, Some("Intel")) => Some(&INTEL_GPU),
        _ => None,
    }
}

/// 只做显示输出的设备不涉及推理运行时——这是**知道不用找**，不是"不知道"。
fn not_applicable(kind: AcceleratorKind) -> Option<RuntimeStatus> {
    (kind == AcceleratorKind::Display).then(|| RuntimeStatus::NotApplicable {
        reason: "只能做显示输出，不涉及推理运行时".into(),
    })
}

pub(crate) fn probe(
    kind: AcceleratorKind,
    vendor: Option<&str>,
    libraries: &LibraryIndex,
) -> RuntimeStatus {
    if let Some(status) = not_applicable(kind) {
        return status;
    }
    let Some(spec) = spec_for(kind, vendor) else {
        return RuntimeStatus::Unknown {
            reason: match vendor {
                Some(vendor) => format!("还没有 {vendor} 的用户态运行时判据"),
                None => "厂商未识别，不知道该找哪些组件".into(),
            },
        };
    };

    let mut components = Vec::new();
    let mut missing = Vec::new();
    for candidates in spec.parts {
        match libraries.hit(candidates) {
            Some(found) => components.push(found),
            // 缺件提示直接把候选列出来，用户照着装就行
            None => missing.push(candidates.join(" 或 ")),
        }
    }

    if missing.is_empty() {
        RuntimeStatus::Ready {
            stack: spec.stack.into(),
            components,
        }
    } else {
        RuntimeStatus::Incomplete {
            stack: spec.stack.into(),
            missing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 造一棵只有库文件的假 root。传空字符串表示建 `/usr/lib` 本身。
    fn fake_libraries(tag: &str, dir: &str, files: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "deviceinfo-runtime-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let lib = if dir.is_empty() {
            root.join("usr/lib")
        } else {
            root.join(dir)
        };
        fs::create_dir_all(&lib).unwrap();
        for file in files {
            fs::write(lib.join(file), b"").unwrap();
        }
        root
    }

    /// 本机实测齐全的那一套（Arch，装了 intel-npu-driver + intel-npu-compiler + level-zero）。
    #[test]
    fn complete_intel_npu_stack_is_ready() {
        let root = fake_libraries(
            "npu-ok",
            "",
            &[
                "libze_loader.so",
                "libze_loader.so.1",
                "libze_intel_npu.so",
                "libopenvino_intel_npu_compiler_loader.so",
            ],
        );
        let index = LibraryIndex::build(&root);
        match probe(AcceleratorKind::Npu, Some("Intel"), &index) {
            RuntimeStatus::Ready { stack, components } => {
                assert_eq!(stack, "OpenVINO NPU");
                assert_eq!(components.len(), 3, "{components:?}");
                assert!(components.contains(&"libze_loader.so".to_string()));
            }
            other => panic!("应当是 Ready: {other:#?}"),
        }
        fs::remove_dir_all(&root).ok();
    }

    /// 装了驱动但没装编译器——这正是 `AGENTS.md` 里记过的坑。
    #[test]
    fn npu_without_compiler_is_incomplete() {
        let root = fake_libraries(
            "npu-nocompiler",
            "",
            &["libze_loader.so", "libze_intel_npu.so"],
        );
        let index = LibraryIndex::build(&root);
        match probe(AcceleratorKind::Npu, Some("Intel"), &index) {
            RuntimeStatus::Incomplete { stack, missing } => {
                assert_eq!(stack, "OpenVINO NPU");
                assert_eq!(missing.len(), 1, "{missing:?}");
                // 提示要是可以照抄去装的
                assert!(
                    missing[0].contains("libopenvino_intel_npu_compiler_loader.so"),
                    "{missing:?}"
                );
            }
            other => panic!("应当是 Incomplete: {other:#?}"),
        }
        fs::remove_dir_all(&root).ok();
    }

    /// Intel GPU 走 OpenCL 或 Level Zero，命中任一即可。
    #[test]
    fn intel_gpu_accepts_either_runtime_path() {
        for (tag, files) in [
            ("gpu-ze", vec!["libze_intel_gpu.so"]),
            ("gpu-cl", vec!["libOpenCL.so"]),
        ] {
            let root = fake_libraries(tag, "", &files);
            let index = LibraryIndex::build(&root);
            assert!(
                probe(AcceleratorKind::Gpu, Some("Intel"), &index).is_ready(),
                "{files:?} 应当算就绪"
            );
            fs::remove_dir_all(&root).ok();
        }

        // 两条路都没有 → 缺件
        let root = fake_libraries("gpu-none", "", &["libc.so"]);
        let index = LibraryIndex::build(&root);
        match probe(AcceleratorKind::Gpu, Some("Intel"), &index) {
            RuntimeStatus::Incomplete { missing, .. } => {
                assert!(missing[0].contains("libze_intel_gpu.so"), "{missing:?}");
            }
            other => panic!("应当是 Incomplete: {other:#?}"),
        }
        fs::remove_dir_all(&root).ok();
    }

    /// 认不出的厂商不猜"就绪"。
    #[test]
    fn unknown_vendor_is_unknown_not_ready() {
        let root = fake_libraries("unknown", "", &["libze_loader.so"]);
        let index = LibraryIndex::build(&root);
        let status = probe(AcceleratorKind::Gpu, Some("NVIDIA"), &index);
        assert!(!status.is_ready(), "不该猜就绪: {status:#?}");
        match status {
            RuntimeStatus::Unknown { reason } => assert!(reason.contains("NVIDIA"), "{reason}"),
            other => panic!("应当是 Unknown: {other:#?}"),
        }
        // 厂商都没识别出来也不能猜
        assert!(!probe(AcceleratorKind::Npu, None, &index).is_ready());
        fs::remove_dir_all(&root).ok();
    }

    /// multiarch 目录也要能找到。
    #[test]
    fn multiarch_library_dirs_are_searched() {
        let root = fake_libraries(
            "multiarch",
            "usr/lib/x86_64-linux-gnu",
            &["libze_loader.so", "libze_intel_npu.so"],
        );
        let index = LibraryIndex::build(&root);
        match probe(AcceleratorKind::Npu, Some("Intel"), &index) {
            RuntimeStatus::Incomplete { missing, .. } => {
                // 前两项在 multiarch 目录里找到了，只缺编译器
                assert_eq!(missing.len(), 1, "{missing:?}");
            }
            other => panic!("应当是 Incomplete: {other:#?}"),
        }
        fs::remove_dir_all(&root).ok();
    }
}
