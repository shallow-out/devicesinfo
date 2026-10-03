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
use std::path::{Path, PathBuf};

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

/// Intel OpenCL 路径需要加载器、Intel 实现库以及 ICD 注册。
const INTEL_GPU: StackSpec = StackSpec {
    stack: "Level Zero / OpenCL",
    parts: &[&["libOpenCL.so"], &["libigdrcl.so"]],
};

/// OpenCL 加载器读取的 ICD 注册目录（相对探测根）。
pub const OPENCL_VENDOR_DIR: &str = "etc/OpenCL/vendors";

fn library_name_matches(name: &str, prefix: &str) -> bool {
    name == prefix
        || name
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|version| {
                version
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            })
}

/// 探测使用的库文件名，包含不完整栈中已安装的组件；采集端只需为空文件占位。
pub fn library_inputs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in LIBRARY_DIRS {
        let Ok(entries) = fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let known = INTEL_NPU
                .parts
                .iter()
                .chain(INTEL_GPU.parts.iter())
                .flat_map(|candidates| candidates.iter().copied())
                .chain(["libze_intel_gpu.so"])
                .any(|prefix| library_name_matches(name, prefix));
            if known && entry.path().is_file() {
                found.push(Path::new(dir).join(name));
            }
        }
    }
    found.sort();
    found
}

/// 候选目录下所有库文件名的索引。
///
/// **构建一次，所有加速器共用**：每次探测都重新遍历 `/usr/lib`（几千个条目）
/// 是没必要的浪费，而一台机器上通常有好几个加速器。
pub(crate) struct LibraryIndex {
    names: BTreeSet<String>,
    intel_opencl_registered: bool,
}

impl LibraryIndex {
    pub(crate) fn build(root: &Path) -> Self {
        let mut names = BTreeSet::new();
        for path in library_inputs(root) {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                names.insert(name.to_string());
            }
        }
        let intel_opencl_registered = fs::read_dir(root.join(OPENCL_VENDOR_DIR))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "icd"))
            .filter_map(|entry| fs::read_to_string(entry.path()).ok())
            .any(|text| {
                let library = Path::new(text.trim());
                let Some(name) = library.file_name().and_then(|name| name.to_str()) else {
                    return false;
                };
                if !library_name_matches(name, "libigdrcl.so") {
                    return false;
                }
                if library.is_absolute() {
                    root.join(library.strip_prefix("/").expect("绝对路径"))
                        .is_file()
                } else {
                    library == Path::new(name) && names.contains(name)
                }
            });
        Self {
            names,
            intel_opencl_registered,
        }
    }

    /// 命中的第一个文件名。候选按给定顺序试，所以列候选时把更具体的放前面。
    fn hit(&self, candidates: &[&str]) -> Option<String> {
        for candidate in candidates {
            if let Some(name) = self
                .names
                .iter()
                .find(|name| library_name_matches(name, candidate))
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
    if kind == AcceleratorKind::Gpu && vendor == Some("Intel") {
        if let Some(driver) = libraries.hit(&["libze_intel_gpu.so"]) {
            return RuntimeStatus::Ready {
                stack: INTEL_GPU.stack.into(),
                components: vec![driver],
            };
        }
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

    if kind == AcceleratorKind::Gpu && vendor == Some("Intel") && !libraries.intel_opencl_registered
    {
        missing.push("Intel OpenCL ICD 注册（/etc/OpenCL/vendors/*.icd）".into());
    }
    if missing.is_empty() {
        RuntimeStatus::Ready {
            stack: spec.stack.into(),
            components,
        }
    } else {
        if kind == AcceleratorKind::Gpu && vendor == Some("Intel") {
            missing = vec![format!(
                "libze_intel_gpu.so 或完整 OpenCL 栈（缺 {}）",
                missing.join("、")
            )];
        }
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
            ("gpu-cl", vec!["libOpenCL.so", "libigdrcl.so"]),
        ] {
            let root = fake_libraries(tag, "", &files);
            if tag == "gpu-cl" {
                fs::create_dir_all(root.join(OPENCL_VENDOR_DIR)).unwrap();
                fs::write(
                    root.join(OPENCL_VENDOR_DIR).join("intel.icd"),
                    "/usr/lib/libigdrcl.so\n",
                )
                .unwrap();
            }
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

    #[test]
    fn an_opencl_loader_without_an_intel_implementation_is_not_ready() {
        let root = fake_libraries("gpu-loader-only", "", &["libOpenCL.so.1"]);
        let status = probe(
            AcceleratorKind::Gpu,
            Some("Intel"),
            &LibraryIndex::build(&root),
        );
        assert!(!status.is_ready(), "{status:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn intel_opencl_requires_a_registration_pointing_to_an_installed_library() {
        let root = fake_libraries("gpu-icd", "", &["libOpenCL.so.1", "libigdrcl.so"]);
        let status = || {
            probe(
                AcceleratorKind::Gpu,
                Some("Intel"),
                &LibraryIndex::build(&root),
            )
        };
        assert!(!status().is_ready());
        fs::create_dir_all(root.join(OPENCL_VENDOR_DIR)).unwrap();
        let icd = root.join(OPENCL_VENDOR_DIR).join("intel.icd");
        fs::write(&icd, "/missing/libigdrcl.so\n").unwrap();
        assert!(!status().is_ready());
        fs::write(&icd, "libigdrcl.so\n").unwrap();
        assert!(status().is_ready());
        fs::remove_dir_all(root).unwrap();
    }
}
