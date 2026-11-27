//! 会话扫描基准：全量扫描 vs `size + mtime` 缓存。
//!
//! 运行：`cargo bench -p trit-host --bench session_scan`
//! 输出每个用例一行 JSON，便于在设置栏/CI 里比较扫描耗时。

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use trit_host::{
    SessionEvent, SessionLog, ToolExecutor, scan_sessions, scan_sessions_cached,
    scan_sessions_roots,
};

const FILES: usize = 200;
const EVENTS: usize = 20;
const ROUNDS: usize = 5;

fn temp() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-session-scan-bench-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn timed(mut run: impl FnMut() -> usize) -> (Duration, usize) {
    std::hint::black_box(run());
    let mut best = Duration::MAX;
    let mut sessions = 0usize;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        sessions = std::hint::black_box(run());
        best = best.min(started.elapsed());
    }
    (best, sessions)
}

fn report(case: &str, files: usize, elapsed: Duration, sessions: usize) {
    let us = elapsed.as_micros().max(1);
    println!(
        "{}",
        serde_json::json!({
            "case": case,
            "files": files,
            "events_per_file": EVENTS,
            "us": us,
            "sessions": sessions,
        })
    );
}

fn main() {
    let root = temp();
    for index in 0..FILES {
        let mut log = SessionLog::open(root.join(format!("s{index:03}.jsonl"))).unwrap();
        for turn in 0..EVENTS {
            log.append(SessionEvent::TurnStarted {
                task: format!("task-{index}-{turn}"),
                objective: "bench".into(),
                max_steps: 1,
            })
            .unwrap();
            log.append(SessionEvent::TurnFinished {
                steps: 1,
                complete: true,
                metrics: None,
            })
            .unwrap();
        }
    }
    let cache = root.join("scan-cache.json");

    let (full, sessions) = timed(|| scan_sessions(&root).unwrap().sessions.len());
    report("full", FILES, full, sessions);

    let (build, sessions) = timed(|| scan_sessions_cached(&root, &cache).unwrap().sessions.len());
    report("cache_build", FILES, build, sessions);

    // 单独测一次真正的缓存构建（上面 timed 的第一轮建、后四轮命中）。
    let _ = fs::remove_file(&cache);
    let started = Instant::now();
    let sessions = scan_sessions_cached(&root, &cache).unwrap().sessions.len();
    report("cache_build_cold", FILES, started.elapsed(), sessions);

    let (hit, sessions) = timed(|| scan_sessions_cached(&root, &cache).unwrap().sessions.len());
    report("cache_hit", FILES, hit, sessions);

    // 多 root 并行扫描：4 个 root × 50 会话。
    let mut multi_roots = Vec::new();
    for root_index in 0..4 {
        let multi = temp();
        for index in 0..50 {
            let mut log = SessionLog::open(multi.join(format!("m{index:03}.jsonl"))).unwrap();
            log.append(SessionEvent::TurnStarted {
                task: format!("root-{root_index}-{index}"),
                objective: "bench".into(),
                max_steps: 1,
            })
            .unwrap();
        }
        multi_roots.push(multi);
    }
    let (multi, sessions) = timed(|| scan_sessions_roots(&multi_roots).unwrap().sessions.len());
    report("multi_root_parallel", 200, multi, sessions);
    for multi in &multi_roots {
        let _ = fs::remove_dir_all(multi);
    }

    // 写批量：200 个不同路径，验证按路径分段并行。
    let write_root = temp();
    let mut config = trit_host::tools::WorkspaceConfig::new(&write_root);
    config.allow_write = true;
    let mut tools = trit_host::tools::LocalTools::new(config).unwrap();
    let requests: Vec<trit_host::ToolRequest> = (0..200)
        .map(|index| trit_host::ToolRequest {
            call_id: format!("write-{index}"),
            tool: "write_file".into(),
            read_only: false,
            command: None,
            paths: vec![format!("w{index:03}.txt")],
            operation: None,
            content: Some(format!("bench {index}")),
            span: None,
            arguments: None,
        })
        .collect();
    let (writes, ok) = timed(|| {
        tools
            .execute_batch(&requests)
            .iter()
            .filter(|outcome| outcome.is_ok())
            .count()
    });
    report("write_batch_parallel", 200, writes, ok);
    let _ = fs::remove_dir_all(write_root);

    println!("scan: ok");
    let _ = fs::remove_dir_all(root);
}
