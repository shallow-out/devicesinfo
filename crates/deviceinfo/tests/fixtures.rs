//! 真机采样夹具的快照回归。
//!
//! 夹具里的每一棵树都是**真实机器采样**，不是手写的假文件树——这正是重点。
//! 手写的假 flags 列表抓不到真实内核里的意外：`smep`（内核安全特性，不是指令集）
//! 就是这么混进 SIMD 列表的，因为写测试的时候我根本不知道会有那个 flag。
//!
//! 每个夹具带一份 `expected.json`，探测结果一变就失败。**失败不一定是 bug**：
//! 人工看 diff，决定是"有意改了行为"（重新 capture）还是"引入了回归"（修代码）。
//!
//! 加一台机器：`deviceinfo capture --out fixtures/<名字>`，然后把新目录提交。

use deviceinfo::{HardwareReport, probe_with};
use std::path::{Path, PathBuf};

fn fixtures_dir() -> PathBuf {
    // crates/deviceinfo/tests/ → 仓库根
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// 把报告里的绝对路径统一成"相对于探测根"的形式再比较。
///
/// 夹具要能在任何路径下跑，所以期望值里不能留 `<tmp>/fixtures/x/dev/...`。
/// `capture` 写期望值时用的是同一套规则。
fn relativize(report: &HardwareReport, root: &Path) -> HardwareReport {
    let json = serde_json::to_string(report).expect("报告一定可序列化");
    let prefix = format!("{}/", root.display().to_string().trim_end_matches('/'));
    serde_json::from_str(&json.replace(&prefix, "/")).expect("归一化后仍然可解析")
}

#[test]
fn captured_machines_still_probe_the_same() {
    let dir = fixtures_dir();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("读不到夹具目录 {}: {error}", dir.display()));

    let mut checked = 0;
    for entry in entries.flatten() {
        let fixture = entry.path();
        if !fixture.is_dir() {
            continue;
        }

        let meta_text = std::fs::read_to_string(fixture.join("meta.json"))
            .unwrap_or_else(|error| panic!("{} 缺 meta.json: {error}", fixture.display()));
        let meta: serde_json::Value = serde_json::from_str(&meta_text).expect("meta.json 应可解析");
        let arch = meta["arch"]
            .as_str()
            .unwrap_or_else(|| panic!("{} 的 meta.json 里没有 arch", fixture.display()));

        let expected_text = std::fs::read_to_string(fixture.join("expected.json"))
            .unwrap_or_else(|error| panic!("{} 缺 expected.json: {error}", fixture.display()));
        let expected: HardwareReport =
            serde_json::from_str(&expected_text).expect("expected.json 应可解析");

        let actual = relativize(&probe_with(&fixture, arch), &fixture);
        assert_eq!(
            actual,
            expected,
            "\n夹具 {} 的探测结果变了。\n\
             先看 diff 决定是「有意改了行为」（重新 capture）还是「引入了回归」（修代码）。\n",
            fixture.display()
        );

        checked += 1;
    }

    assert!(
        checked > 0,
        "{} 下没有任何夹具——夹具是这套回归测试的全部意义，不该为空",
        dir.display()
    );
}

/// 硬件探测和状态采样各自实现了一遍设备发现（两份都需要不同上下文，拆出来反而难读），
/// 所以这里盯住它们别走偏：同一棵夹具上必须发现同样数量的同一批设备。
#[test]
fn state_and_hardware_agree_on_which_devices_exist() {
    let dir = fixtures_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    for entry in entries.flatten() {
        let fixture = entry.path();
        if !fixture.is_dir() {
            continue;
        }
        let arch = "x86_64";
        let hardware = probe_with(&fixture, arch);
        let options = deviceinfo::SampleOptions {
            watch: Vec::new(),
            // 夹具里也有计数器文件，打开才能验证那条路径真的读得到
            counters: true,
        };
        let state = deviceinfo::sample_state_with(&fixture, &options);

        assert_eq!(
            state.accelerators.len(),
            hardware.accelerators.len(),
            "{}: 状态采样与硬件探测发现的设备数不一致——两份发现逻辑走偏了",
            fixture.display()
        );

        // 每一台都要能用 PCI 标识对上，且状态里至少读到了一个瞬时值
        for accel in &hardware.accelerators {
            let matched = state.accelerators.iter().find(|candidate| {
                candidate.kind == accel.kind && candidate.pci_id == accel.pci_id
            });
            let matched = matched.unwrap_or_else(|| {
                panic!(
                    "{}: 硬件里有 {:?} {:?}，状态里找不到",
                    fixture.display(),
                    accel.kind,
                    accel.pci_id
                )
            });
            // 只在厂商能认出来时才要求"至少读到一个瞬时值"：
            // 认不出的平台设备（ARM 上的 rockchip 之类）我们没有已知的指标路径，
            // 读到空是**正确的行为**，不是回归。
            if accel.vendor.is_some() {
                assert!(
                    matched.current_freq_mhz.is_some() || matched.resident_memory_bytes.is_some(),
                    "{}: {:?} 识别出了厂商却一个瞬时值都没读到，说明夹具里的路径和采样逻辑不一致",
                    fixture.display(),
                    accel.kind
                );
            }
        }
    }
}
