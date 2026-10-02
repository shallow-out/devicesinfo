//! 实时探测：**会执行命令、会连网络**的那些问题。
//!
//! 与另外三个入口（硬件 / 环境 / 状态）的区别是：这里**没有"事实"可言**。
//! `docker --version` 是"跑了一次"的结果；"能不能拉镜像"是"此刻连得上"。所以
//!
//! - **opt-in**：默认不进 [`EnvironmentReport`]，它是另一份报告
//! - **不进夹具**：一秒就变，冻进快照只会污染它（这正是 [`EnvironmentReport`]
//!   被限制在"只读文件"的原因）
//! - 每个检查都有超时，而且**并发**跑（串行的话 N 个目标 × 超时就是几十秒）
//!
//! # 为什么不在这里 spawn 进程，也不在这里连网络
//!
//! 这个模块**只做逻辑**：调用方传进来一个"在**被探测的那台机器上**跑一条命令"
//! 的回调（[`Runner`]）。库因此完全不需要知道那是本地进程还是 ssh——`--root` 与
//! `--ssh` 走同一套代码，而且全部逻辑都能用假 runner 测掉，不必真的执行东西。
//!
//! 反过来说：**只有被探测的那台机器自己的连通性才有意义**。"我这台能不能连上
//! Docker Hub"和"那台能否连上"是两件不同的事，后者必须在那台上量。

use crate::environment::EnvironmentReport;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 在**被探测的那台机器上**跑一条命令，返回标准输出。
///
/// 实现方负责超时与错误处理（超时要真杀进程，不是加个 `--max-time` 就算）。返回
/// `Err` 表示命令没跑成（不存在、超时、非零退出），`Ok` 里是标准输出。
///
/// 要 `Sync` 是因为连通性检查是**并发**跑的：串行的话 N 个目标 × 超时就是几十秒。
pub type Runner<'a> = &'a (dyn Fn(&str, &[&str]) -> std::io::Result<String> + Sync);

/// 版本探测：跑一次命令问出来的，不是读文件读到的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolVersion {
    pub tool: String,
    /// 实际执行的命令（人可读），失败时要能看出跑的是什么。
    pub command: String,
    /// 输出的第一行非空内容。**原样带上**——各工具的版本格式不一样，
    /// 在这里解析只会引入一个迟早过时的解析器。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 一个连通性目标的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reachability {
    /// 被查的 URL 或主机。
    pub target: String,
    /// 这个目标是从哪来的（比如某台机器配的镜像源）。没有表示是默认目标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub outcome: ReachabilityOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum ReachabilityOutcome {
    /// 拿到了 HTTP 状态码。**连得上不等于拉得动**，但连不上就一定拉不动。
    Reachable { http_status: Option<String> },
    /// 明确连不上，附原因。
    Unreachable { reason: String },
    /// 没法查（那台机器上既没有 curl 也没有 wget）——**不是"连不上"**。
    Unknown { reason: String },
}

impl ReachabilityOutcome {
    pub fn is_reachable(&self) -> bool {
        matches!(self, Self::Reachable { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveReport {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<ToolVersion>,
    /// 逐个目标的连通性。**必须分目标看**：实测那台 NAS 出得去 baidu 但到不了
    /// Docker Hub（DNS 被污染），靠镜像源拉——只测 `docker.io` 会得出错误结论。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reachability: Vec<Reachability>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// 没指定目标时要查的默认项：拉镜像、下模型最常用的三个入口。
///
/// 它们是**约定**而不是事实，所以放在公开常量里，调用方想改就改。
pub const DEFAULT_TARGETS: [&str; 3] = [
    "https://registry-1.docker.io/v2/",
    "https://huggingface.co",
    "https://www.modelscope.cn",
];

/// 每次检查的超时。给 `curl`/`wget` 也各带一份，免得卡在慢连接上。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(6);

pub struct LiveOptions {
    /// 每次检查的超时。
    pub timeout: Duration,
    /// 要查的额外目标（URL）。镜像源会自动加进来。
    pub targets: Vec<String>,
    /// 跳过连通性检查（只想查版本号时用）。
    pub skip_network: bool,
}

impl Default for LiveOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            targets: DEFAULT_TARGETS.iter().map(|url| url.to_string()).collect(),
            skip_network: false,
        }
    }
}

/// 跑一轮实时探测。
///
/// `environment` 决定查哪些工具的版本、以及要把哪些镜像源加进连通性目标——
/// 所以实时探测**依赖**环境报告，而不是重复一遍它的枚举逻辑。
pub fn probe(environment: &EnvironmentReport, options: &LiveOptions, run: Runner) -> LiveReport {
    let mut versions: Vec<ToolVersion> = environment
        .containers
        .iter()
        .map(|runtime| runtime.path.display().to_string())
        .chain(
            environment
                .inference_tools
                .iter()
                .map(|tool| tool.path.display().to_string()),
        )
        .map(|path| version_of(&path, run))
        .collect();
    versions.sort_by(|a, b| a.tool.cmp(&b.tool));

    let reachability = if options.skip_network {
        Vec::new()
    } else {
        let mut targets: Vec<(String, Option<String>)> = options
            .targets
            .iter()
            .map(|url| (url.clone(), None))
            .collect();
        // 机器自己配的镜像源也要查——**它才是那台机器实际拉镜像的通道**。
        for mirror in &environment.registry_mirrors {
            targets.push((mirror.url.clone(), Some(mirror.source.display().to_string())));
        }
        targets.sort_by(|a, b| a.0.cmp(&b.0));
        targets.dedup_by(|a, b| a.0 == b.0);
        check_targets(&targets, options, run)
    };

    LiveReport {
        versions,
        reachability,
        warnings: Vec::new(),
    }
}

/// 并发查所有目标：串行的话 N 个 × 超时就是几十秒。
fn check_targets(
    targets: &[(String, Option<String>)],
    options: &LiveOptions,
    run: Runner,
) -> Vec<Reachability> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|(target, from)| {
                scope.spawn(|| Reachability {
                    target: target.clone(),
                    from: from.clone(),
                    outcome: check_one(target, options.timeout, run),
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    })
}

fn check_one(target: &str, timeout: Duration, run: Runner) -> ReachabilityOutcome {
    let seconds = timeout.as_secs().max(1).to_string();
    // 先 curl（能给状态码），退回 wget（只报成不成）
    let attempts: [(&str, Vec<&str>); 2] = [
        (
            "curl",
            vec![
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                &seconds,
                target,
            ],
        ),
        (
            "wget",
            vec!["-q", "--spider", "-T", &seconds, target],
        ),
    ];

    let mut last: Option<String> = None;
    for (program, args) in attempts {
        match run(program, &args) {
            Ok(output) => {
                let status = output.trim();
                // 401/403 是**通的**：Docker Hub 的 v2 API 未鉴权就回 401。
                return ReachabilityOutcome::Reachable {
                    http_status: (!status.is_empty()).then(|| status.to_string()),
                };
            }
            Err(error) => {
                let note = format!("{program}: {error}");
                // **两条都留着**：只留最后一条会丢掉 curl 那条（它通常更精确，
                // 比如 "Could not resolve host"）。
                last = Some(match last {
                    Some(previous) => format!("{previous}；{note}"),
                    None => note,
                });
            }
        }
    }
    let reason = last.unwrap_or_else(|| "没有可用的检查工具".into());
    // 两个工具都不存在时，是**没法查**而不是"连不上"——这两件事对部署的含义相反
    if reason.contains("No such file") || reason.contains("not found") {
        ReachabilityOutcome::Unknown { reason }
    } else {
        ReachabilityOutcome::Unreachable { reason }
    }
}

/// 问一个工具的版本：跑 `<tool> --version`，取输出的第一行非空内容。
fn version_of(path: &str, run: Runner) -> ToolVersion {
    let command = format!("{path} --version");
    match run(path, &["--version"]) {
        Ok(output) => {
            let first = output
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_string);
            ToolVersion {
                tool: path.to_string(),
                command,
                output: first.clone(),
                // **成功但没输出**说明这个工具不认 `--version`，而不是"它没有版本"。
                // 这两件事对读报告的人含义不同（实测 llama.cpp 的某些二进制就是这样）。
                error: first
                    .is_none()
                    .then(|| "命令成功但没有输出（这个工具可能不认 --version）".into()),
            }
        }
        Err(error) => ToolVersion {
            tool: path.to_string(),
            command,
            output: None,
            error: Some(error.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::{ContainerRuntime, EnvironmentReport, RegistryMirror};
    use std::path::PathBuf;

    fn environment() -> EnvironmentReport {
        EnvironmentReport {
            package_managers: vec!["pacman".into()],
            init: None,
            cgroup: None,
            containers: vec![ContainerRuntime {
                name: "podman".into(),
                path: PathBuf::from("/usr/bin/podman"),
                sockets: Vec::new(),
                notes: Vec::new(),
            }],
            inference_tools: Vec::new(),
            registry_mirrors: vec![RegistryMirror {
                source: PathBuf::from("/etc/docker/daemon.json"),
                url: "https://docker.fnnas.com".into(),
            }],
            declared_tags: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn versions_use_the_first_non_empty_line_verbatim() {
        let run: Runner = &|program, args| {
            assert_eq!(program, "/usr/bin/podman");
            assert_eq!(args, ["--version"]);
            Ok("\npodman version 6.1.2\n".into())
        };
        let report = probe(
            &environment(),
            &LiveOptions {
                skip_network: true,
                ..Default::default()
            },
            run,
        );
        let version = &report.versions[0];
        assert_eq!(version.output.as_deref(), Some("podman version 6.1.2"));
        assert_eq!(version.command, "/usr/bin/podman --version");
        fs_assert_no_warnings(&report);
    }

    /// 版本查不到要**记下原因**，而不是安静地留空——"查不到"和"没有版本"不是一回事。
    #[test]
    fn a_failed_version_query_keeps_its_reason() {
        let run: Runner = &|_program, _args| Err(std::io::Error::other("超时"));
        let report = probe(
            &environment(),
            &LiveOptions {
                skip_network: true,
                ..Default::default()
            },
            run,
        );
        assert_eq!(report.versions[0].output, None);
        assert!(report.versions[0].error.as_deref().unwrap().contains("超时"));
    }

    /// **连通性必须分目标看**，而且机器自己配的镜像源一定要进列表——
    /// 实测那台 NAS 到不了 Docker Hub，全靠镜像源拉。
    #[test]
    fn reachability_covers_defaults_and_the_machines_own_mirrors() {
        let seen = std::sync::Mutex::new(Vec::new());
        let run: Runner = &|_program, args| {
            let url = args.last().unwrap().to_string();
            seen.lock().unwrap().push(url.clone());
            if url.contains("fnnas") {
                Ok("200".into())
            } else {
                Err(std::io::Error::other("Connection refused"))
            }
        };
        let report = probe(&environment(), &LiveOptions::default(), run);

        let targets: Vec<&str> = report
            .reachability
            .iter()
            .map(|entry| entry.target.as_str())
            .collect();
        assert!(targets.contains(&"https://registry-1.docker.io/v2/"), "{targets:?}");
        assert!(targets.contains(&"https://docker.fnnas.com"), "{targets:?}");

        let mirror = report
            .reachability
            .iter()
            .find(|entry| entry.target.contains("fnnas"))
            .unwrap();
        assert!(mirror.outcome.is_reachable());
        // 要说清它是从哪份配置来的
        assert_eq!(mirror.from.as_deref(), Some("/etc/docker/daemon.json"));

        let hub = report
            .reachability
            .iter()
            .find(|entry| entry.target.contains("docker.io"))
            .unwrap();
        assert!(!hub.outcome.is_reachable());
        match &hub.outcome {
            ReachabilityOutcome::Unreachable { reason } => {
                assert!(reason.contains("Connection refused"), "{reason}")
            }
            other => panic!("应当是 Unreachable: {other:#?}"),
        }
    }

    /// 401 算**通**：Docker Hub 的 v2 API 未鉴权就是回 401。
    /// 把它当失败会让"能拉镜像"的机器被误判。
    #[test]
    fn an_unauthorized_status_still_counts_as_reachable() {
        let run: Runner = &|_program, _args| Ok("401".into());
        let report = probe(
            &environment(),
            &LiveOptions {
                targets: vec!["https://registry-1.docker.io/v2/".into()],
                skip_network: false,
                ..Default::default()
            },
            run,
        );
        assert!(report.reachability[0].outcome.is_reachable());
        match &report.reachability[0].outcome {
            ReachabilityOutcome::Reachable { http_status } => {
                assert_eq!(http_status.as_deref(), Some("401"))
            }
            other => panic!("{other:#?}"),
        }
    }

    /// 两个检查工具都没有时是**没法查**，不是"连不上"——这两件事对部署的含义相反。
    #[test]
    fn missing_check_tools_is_unknown_not_unreachable() {
        let run: Runner = &|_program, _args| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No such file or directory",
            ))
        };
        let report = probe(
            &environment(),
            &LiveOptions {
                targets: vec!["https://example.invalid".into()],
                skip_network: false,
                ..Default::default()
            },
            run,
        );
        match &report.reachability[0].outcome {
            ReachabilityOutcome::Unknown { .. } => {}
            other => panic!("应当是 Unknown: {other:#?}"),
        }
    }

    /// 只查版本时不该去连网络。
    #[test]
    fn skip_network_leaves_reachability_empty() {
        let run: Runner = &|_program, _args| Ok("x".into());
        let options = LiveOptions {
            skip_network: true,
            ..Default::default()
        };
        let report = probe(&environment(), &options, run);
        assert!(report.reachability.is_empty());
        assert!(!report.versions.is_empty());
    }

    fn fs_assert_no_warnings(report: &LiveReport) {
        assert!(report.warnings.is_empty(), "{:#?}", report.warnings);
    }
}
