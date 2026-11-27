//! 权限判定基准：策略决策、write claim 路径匹配、shell 写入目标提取。
//!
//! 运行：`cargo bench -p trit-host --bench permission_matrix`
//!
//! 这些判定在每个工具调用上都会跑一遍，规则表随仓库策略增长；这里固定规模档位，
//! 避免"规则一多就退化成线性扫描"的问题被漏掉。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;
use trit_host::ToolRequest;
use trit_host::WriteClaim;
use trit_host::permissions::{
    PermissionDecision, PermissionMode, PermissionPolicy, PermissionRule,
};

const ROUNDS: usize = 5;

fn request(tool: &str, command: Option<&str>, paths: &[&str]) -> ToolRequest {
    ToolRequest {
        call_id: "call-1".into(),
        tool: tool.into(),
        read_only: false,
        command: command.map(str::to_owned),
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        operation: None,
        content: None,
        span: None,
        arguments: None,
    }
}

fn timed(mut run: impl FnMut()) -> Duration {
    run();
    let mut best = Duration::MAX;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        run();
        best = best.min(started.elapsed());
    }
    best
}

fn report(case: &str, n: usize, elapsed: Duration) {
    println!(
        "{}",
        json!({
            "case": case,
            "n": n,
            "us": elapsed.as_micros().max(1),
        })
    );
}

fn temp(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-perm-bench-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn policy_with(rules: usize, mode: PermissionMode) -> PermissionPolicy {
    let mut policy = PermissionPolicy::new(mode);
    for index in 0..rules {
        policy = policy.with_rule(PermissionRule {
            tool: if index % 3 == 0 {
                "*".into()
            } else {
                format!("tool_{index}")
            },
            subject: None,
            decision: if index % 7 == 0 {
                PermissionDecision::Deny
            } else {
                PermissionDecision::Allow
            },
        });
    }
    policy
}

fn main() {
    // 1) 策略决策：模式 × 规则表规模。
    let requests = [
        request("bash", Some("cargo test"), &[]),
        request("write_file", None, &["src/lib.rs"]),
        request("read_file", None, &["src/lib.rs"]),
        request("context_read", None, &[]),
    ];
    for mode in [
        PermissionMode::Ask,
        PermissionMode::Auto,
        PermissionMode::Yolo,
    ] {
        for rules in [0usize, 10, 100, 1_000] {
            let policy = policy_with(rules, mode);
            let elapsed = timed(|| {
                for request in &requests {
                    std::hint::black_box(policy.decide(request));
                }
            });
            report(
                &format!("decide/{mode:?}/{}_rules", rules),
                requests.len(),
                elapsed,
            );
        }
    }

    // 2) 写不进去的规则（每条都不匹配）：确认最坏路径不受"命中位置"影响。
    let policy = policy_with(1_000, PermissionMode::Ask);
    let miss = request("no_such_tool", Some("nope"), &[]);
    let elapsed = timed(|| {
        for _ in 0..100 {
            std::hint::black_box(policy.decide(&miss));
        }
    });
    report("decide/all_miss_1000_rules", 100, elapsed);

    // 3) write claim：字面路径 + glob 混合。
    let root = temp("claim");
    std::fs::create_dir_all(root.join("src/nested")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    let claim = WriteClaim::new(
        &root,
        vec![
            PathBuf::from("src"),
            PathBuf::from("tests/**/*.rs"),
            PathBuf::from("README.md"),
        ],
    )
    .unwrap();
    let probes = [
        "src/lib.rs",
        "src/nested/deep/module.rs",
        "tests/integration/case.rs",
        "README.md",
        "../escape.txt",
        "docs/guide.md",
    ];
    let elapsed = timed(|| {
        for probe in probes {
            std::hint::black_box(claim.allows_path(probe));
        }
    });
    report("claim/allow_path", probes.len(), elapsed);

    // 4) claim 请求判定：bash 需要从命令文本里提目标，成本更高。
    let bash_requests: Vec<ToolRequest> = (0..50)
        .map(|index| {
            request(
                "bash",
                Some(&format!(
                    "cargo test --lib && tee src/nested/out_{index}.log > /dev/null"
                )),
                &[],
            )
        })
        .collect();
    let elapsed = timed(|| {
        for request in &bash_requests {
            std::hint::black_box(claim.allows_request(request));
        }
    });
    report("claim/allow_request_bash", bash_requests.len(), elapsed);

    // 5) 写入请求的 claim 判定：每次 write_file 落盘前一次。
    let elapsed = timed(|| {
        for index in 0..200 {
            std::hint::black_box(claim.allows_request(&request(
                "write_file",
                None,
                &[if index % 2 == 0 {
                    "src/nested/file.rs"
                } else {
                    "tests/case.rs"
                }],
            )));
        }
    });
    report("claim/write_requests", 200, elapsed);

    let _ = std::fs::remove_dir_all(&root);
    println!("permission_matrix: ok");
}
