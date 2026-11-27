//! tech-stack 策略基准：test-only 语言判定、路径审计、shell 写入目标提取。
//!
//! 运行：`cargo bench -p trit-host --bench tech_stack_matrix`
//!
//! `violations` 在每次外部子智能体跑完后对整份改动清单做审计，规模随提交变大；
//! `shell_write_targets` 在 claim 判定的热路径上（每个 bash 调用一次）。

use std::time::{Duration, Instant};

use serde_json::json;
use trit_host::tech_stack::{is_test_path, shell_write_targets, validate_write_path, violations};

const ROUNDS: usize = 5;

fn timed(mut run: impl FnMut() -> usize) -> (Duration, usize) {
    let mut count = run();
    let mut best = Duration::MAX;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        count = run();
        best = best.min(started.elapsed());
    }
    (best, std::hint::black_box(count))
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

/// 生产路径，绝大多数带 `src/` 前缀。
fn production_paths(n: usize) -> Vec<String> {
    (0..n)
        .map(|index| format!("src/module_{}/impl_{index}.rs", index % 37))
        .collect()
}

/// 测试路径：目录约定与文件名约定混合。
fn test_paths(n: usize) -> Vec<String> {
    (0..n)
        .map(|index| match index % 4 {
            0 => format!("tests/integration/case_{index}.rs"),
            1 => format!("src/feature/test_{index}.py"),
            2 => format!("spec/unit/case_{index}_test.py"),
            _ => "conftest.py".to_string(),
        })
        .collect()
}

/// 混合清单：生产路径里夹着 test-only 语言，用来验证审计确实挑出越界项。
fn mixed_paths(n: usize) -> Vec<String> {
    let mut paths = production_paths(n / 2);
    paths.extend(test_paths(n / 2));
    paths.push("src/service/worker.py".into());
    paths.push("scripts/deploy.py".into());
    paths.push("tools/gen.py".into());
    paths.sort();
    paths
}

fn main() {
    // 1) is_test_path：纯函数，输入规模 1k。
    for (case, paths) in [
        ("is_test_path/production", production_paths(1_000)),
        ("is_test_path/tests", test_paths(1_000)),
        ("is_test_path/mixed", mixed_paths(1_000)),
    ] {
        let n = paths.len();
        let (elapsed, hits) = timed(|| paths.iter().filter(|path| is_test_path(path)).count());
        report(case, n, elapsed);
        println!(
            "{}",
            json!({"case": format!("{case}/hits"), "n": n, "us": 0, "hits": hits})
        );
    }

    // 2) validate_write_path：每次 write_file 落盘前一次。
    let writes = mixed_paths(1_000);
    let (elapsed, rejected) = timed(|| {
        writes
            .iter()
            .filter(|path| validate_write_path(path).is_err())
            .count()
    });
    report("validate_write_path", writes.len(), elapsed);
    println!(
        "{}",
        json!({"case": "validate_write_path/rejected", "n": writes.len(), "us": 0, "rejected": rejected})
    );

    // 3) violations：外部子智能体的整份改动清单审计。
    let audit = mixed_paths(2_000);
    let (elapsed, found) = timed(|| violations(&audit).len());
    report("violations", audit.len(), elapsed);
    println!(
        "{}",
        json!({"case": "violations/found", "n": audit.len(), "us": 0, "found": found})
    );

    // 4) shell_write_targets：claim 热路径，各种写法。
    let commands: Vec<String> = (0..1_000)
        .map(|index| match index % 8 {
            0 => format!("cargo test && tee src/out_{index}.log"),
            1 => format!("echo done > tests/result_{index}.txt"),
            2 => format!("cp src/a.rs src/b_{index}.rs"),
            3 => format!("mv src/x.rs src/y_{index}.rs"),
            4 => "cargo build --release".to_owned(),
            5 => "git status --short".to_owned(),
            6 => format!("cat <<'EOF' > src/generated_{index}.rs"),
            _ => format!("python3 -c 'print(1)' >> /tmp/log_{index}.txt"),
        })
        .collect();
    let (elapsed, targets) = timed(|| {
        commands
            .iter()
            .map(|command| shell_write_targets(command).len())
            .sum()
    });
    report("shell_write_targets", commands.len(), elapsed);
    println!(
        "{}",
        json!({"case": "shell_write_targets/targets", "n": commands.len(), "us": 0, "targets": targets})
    );

    println!("tech_stack_matrix: ok");
}
