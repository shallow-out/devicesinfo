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

use deviceinfo::{HardwareReport, inspect_hardware};
use std::path::{Path, PathBuf};

/// 递归统计一个目录的字节数。
fn walk_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => walk_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

fn fixtures_dir() -> PathBuf {
    // crates/deviceinfo/tests/ → 仓库根
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
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

        // **直接比，不做任何路径归一化**：报告里的路径是"机器上的路径"
        // （`/dev/accel/accel0`），本机、远端、夹具三种来源本来就该给出同一串。
        // 以前要归一化，是因为路径里混了探测根——那掩盖了"夹具与真机不同"这类偏差。
        let actual = inspect_hardware(&fixture, arch).data;
        assert_eq!(
            actual,
            expected,
            "\n夹具 {} 的探测结果变了。\n\
             先看 diff 决定是「有意改了行为」（重新 capture）还是「引入了回归」（修代码）。\n",
            fixture.display()
        );

        // 环境报告也要对：它和硬件报告一样是可缓存的事实，而且它的输入
        // （可执行文件候选、socket、镜像源配置）最容易被采集清单漏掉。
        let expected_environment = std::fs::read_to_string(
            fixture.join("expected-environment.json"),
        )
        .unwrap_or_else(|error| {
            panic!(
                "{} 缺 expected-environment.json: {error}",
                fixture.display()
            )
        });
        let expected_environment: deviceinfo::EnvironmentReport =
            serde_json::from_str(&expected_environment)
                .expect("expected-environment.json 应可解析");
        assert_eq!(
            deviceinfo::inspect_environment(&fixture).data,
            expected_environment,
            "\n夹具 {} 的环境探测结果变了。\n",
            fixture.display()
        );

        checked += 1;
    }

    // 夹具必须是**小文本**的集合。这条守卫是有原因的：环境探测要问
    // "`/usr/bin/podman` 在不在"，而那是 45 MB 的二进制——一旦被当成"要复制内容"，
    // 夹具会从 33 KB 涨到 428 MB，而且**没有任何测试会红**。
    // 现在采集端也在拦（`refuse_if_oversized`），这里再钉一次。
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let fixture = entry.path();
        if !fixture.is_dir() {
            continue;
        }
        let bytes: u64 = walk_size(&fixture);
        assert!(
            bytes < 1024 * 1024,
            "{} 有 {bytes} 字节——夹具该全是小文本",
            fixture.display()
        );
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
        let hardware = inspect_hardware(&fixture, arch).data;
        let options = deviceinfo::SampleOptions {
            watch: Vec::new(),
            // 夹具里也有计数器文件，打开才能验证那条路径真的读得到
            counters: true,
        };
        let state = deviceinfo::observe_accelerators(&fixture, &options).data;

        assert_eq!(
            state.accelerators.len(),
            hardware.accelerators.len(),
            "{}: 状态采样与硬件探测发现的设备数不一致——两份发现逻辑走偏了",
            fixture.display()
        );

        // 每一台都要能用来源路径对上，不能把同型号卡当成同一个实例，且状态里至少读到了一个瞬时值
        for accel in &hardware.accelerators {
            let matched = state
                .accelerators
                .iter()
                .find(|candidate| candidate.kind == accel.kind && candidate.source == accel.source);
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
