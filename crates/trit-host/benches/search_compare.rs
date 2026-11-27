//! 同一 8 MiB 语料上的横向对比：`trit_search` vs `search_code` vs ripgrep。
//!
//! 运行：
//!
//! ```text
//! cargo bench -p trit-host --bench search_compare
//! ```
//!
//! 三个工具都从磁盘读取同一份语料，预热 1 轮后取 5 轮最小值；每个用例一行 JSON。
//! ripgrep 找不到时输出 `"rg": null` 并继续，不让基准失败。
//!
//! 语义差异是刻意的：`trit_search` 的 `?` 是 Kleene 未知位，`search_code` 的
//! regex `.` 是二值通配；因此 literal 用例比速度，wildcard 用例说明三值语义的代价。

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::json;
use trit_host::tools::{LocalTools, WorkspaceConfig};
use trit_host::{ToolExecutor, ToolRequest};

const TARGET_BYTES: usize = 8 * 1024 * 1024;
const ROUNDS: usize = 5;

fn corpus(target: usize) -> String {
    let mut out = String::with_capacity(target + 128);
    let mut i = 0usize;
    while out.len() < target {
        match i % 64 {
            0 => out.push_str(&format!(
                "pub fn open(path: &Path) -> io::Result<File> {{ // case {i}\n"
            )),
            1 => out.push_str(&format!(
                "    let handle = File::open(path)?; // needle {i}\n"
            )),
            _ => out.push_str(&format!(
                "    let value_{i} = compute({i}); // filler line\n"
            )),
        }
        i += 1;
    }
    out
}

fn temp_workspace() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-search-compare-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn request(tool: &str, arguments: serde_json::Value) -> ToolRequest {
    ToolRequest {
        call_id: format!("{tool}-1"),
        tool: tool.into(),
        arguments: Some(arguments),
        ..ToolRequest::default()
    }
}

fn timed(mut run: impl FnMut() -> usize) -> (Duration, usize) {
    std::hint::black_box(run());
    let mut best = Duration::MAX;
    let mut hits = 0usize;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        hits = std::hint::black_box(run());
        best = best.min(started.elapsed());
    }
    (best, hits)
}

fn report(tool: &str, case: &str, bytes: usize, elapsed: Duration, hits: usize) {
    let us = elapsed.as_micros().max(1);
    let mib_per_s = bytes as f64 / (1024.0 * 1024.0) / (us as f64 / 1_000_000.0);
    println!(
        "{}",
        json!({
            "tool": tool,
            "case": case,
            "bytes": bytes,
            "us": us,
            "mib_per_s": (mib_per_s * 100.0).round() / 100.0,
            "hits": hits,
        })
    );
}

fn find_rg() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("RIPGREP") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    let name = if cfg!(windows) { "rg.exe" } else { "rg" };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn main() {
    let root = temp_workspace();
    let text = corpus(TARGET_BYTES);
    std::fs::write(root.join("corpus.txt"), &text).unwrap();
    let bytes = text.len();
    drop(text);
    let mut config = WorkspaceConfig::new(&root);
    config.max_output_bytes = 8 * 1024 * 1024;
    config.max_search_file_bytes = 16 * 1024 * 1024;
    let mut tools = LocalTools::new(config).unwrap();
    println!("{}", json!({"corpus_bytes": bytes, "rounds": ROUNDS}));

    let (elapsed, hits) = timed(|| {
        let outcome = tools
            .execute(&request(
                "search_code",
                json!({"query": "fn open", "max_results": 1000}),
            ))
            .unwrap();
        assert!(outcome.passed);
        outcome.output.lines().count()
    });
    report("search_code", "literal", bytes, elapsed, hits);

    let (elapsed, hits) = timed(|| {
        let outcome = tools
            .execute(&request(
                "trit_search",
                json!({"query": "fn open", "max_results": 1000}),
            ))
            .unwrap();
        assert!(outcome.passed);
        outcome
            .output
            .lines()
            .filter(|line| line.contains(":T "))
            .count()
    });
    report("trit_search", "literal", bytes, elapsed, hits);

    let (elapsed, hits) = timed(|| {
        let outcome = tools
            .execute(&request(
                "search_code",
                json!({"query": "fn .pen", "regex": true, "max_results": 1000}),
            ))
            .unwrap();
        assert!(outcome.passed);
        outcome.output.lines().count()
    });
    report("search_code", "wildcard_regex", bytes, elapsed, hits);

    let (elapsed, hits) = timed(|| {
        let outcome = tools
            .execute(&request(
                "trit_search",
                json!({"query": "fn ?pen", "max_results": 1000}),
            ))
            .unwrap();
        assert!(outcome.passed);
        outcome
            .output
            .lines()
            .filter(|line| line.contains(":U "))
            .count()
    });
    report("trit_search", "wildcard_kleene", bytes, elapsed, hits);

    match find_rg() {
        Some(rg) => {
            let (elapsed, hits) = timed(|| {
                let output = Command::new(&rg)
                    .args([
                        "-F",
                        "-n",
                        "--no-heading",
                        "--color",
                        "never",
                        "--max-count",
                        "1000",
                        "fn open",
                        ".",
                    ])
                    .current_dir(&root)
                    .output()
                    .expect("run ripgrep");
                String::from_utf8_lossy(&output.stdout).lines().count()
            });
            report("ripgrep", "literal", bytes, elapsed, hits);
        }
        None => println!(
            "{}",
            json!({"tool": "ripgrep", "case": "literal", "rg": null})
        ),
    }

    // 多文件场景：文件级并行与目录遍历开销的对比。
    let multi = root.join("multi");
    std::fs::create_dir_all(&multi).unwrap();
    let per_file = TARGET_BYTES / 256;
    let mut multi_bytes = 0usize;
    for index in 0..256 {
        let text = corpus(per_file);
        multi_bytes += text.len();
        std::fs::write(multi.join(format!("f{index:03}.rs")), text).unwrap();
    }
    println!(
        "{}",
        json!({"multi_files": 256, "multi_bytes": multi_bytes})
    );

    let (elapsed, hits) = timed(|| {
        let outcome = tools
            .execute(&request(
                "search_code",
                json!({"query": "fn open", "path": "multi", "max_results": 1000}),
            ))
            .unwrap();
        assert!(outcome.passed);
        outcome.output.lines().count()
    });
    report("search_code", "multi_literal", multi_bytes, elapsed, hits);

    let (elapsed, hits) = timed(|| {
        let outcome = tools
            .execute(&request(
                "trit_search",
                json!({"query": "fn open", "path": "multi", "max_results": 1000}),
            ))
            .unwrap();
        assert!(outcome.passed);
        outcome
            .output
            .lines()
            .filter(|line| line.contains(":T "))
            .count()
    });
    report("trit_search", "multi_literal", multi_bytes, elapsed, hits);

    if let Some(rg) = find_rg() {
        let (elapsed, hits) = timed(|| {
            let output = Command::new(&rg)
                .args([
                    "-F",
                    "-n",
                    "--no-heading",
                    "--color",
                    "never",
                    "--max-count",
                    "1000",
                    "fn open",
                    "multi",
                ])
                .current_dir(&root)
                .output()
                .expect("run ripgrep");
            String::from_utf8_lossy(&output.stdout).lines().count()
        });
        report("ripgrep", "multi_literal", multi_bytes, elapsed, hits);
    }

    println!("compare: ok");
    let _ = std::fs::remove_dir_all(root);
}
