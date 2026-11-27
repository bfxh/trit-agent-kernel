//! 工具批处理基准：并行只读批 vs 串行逐次执行。
//!
//! 运行：`cargo bench -p trit-host --bench tools_batch`
//!
//! 批量路径只在调用方显式给出多个只读请求时启用；这里固定文件规模与单文件大小，
//! 用"并行批 / 串行逐次"的比值判断并行收益是否仍然存在。

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;
use trit_host::tools::{LocalTools, WorkspaceConfig};
use trit_host::{ToolExecutor, ToolRequest};

const ROUNDS: usize = 5;
const FILES: usize = 200;
const BYTES: usize = 4 * 1024;

fn temp() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-tools-batch-bench-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

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

fn report(case: &str, files: usize, elapsed: Duration, ok: usize) {
    println!(
        "{}",
        json!({
            "case": case,
            "files": files,
            "bytes_each": BYTES,
            "us": elapsed.as_micros().max(1),
            "ok": ok,
        })
    );
}

fn read_request(index: usize) -> ToolRequest {
    ToolRequest {
        call_id: format!("read-{index}"),
        tool: "read_file".into(),
        read_only: true,
        command: None,
        paths: vec![format!("f{index:03}.txt")],
        operation: None,
        content: None,
        span: None,
        arguments: None,
    }
}

fn main() {
    let root = temp();
    let payload = "x".repeat(BYTES);
    for index in 0..FILES {
        fs::write(root.join(format!("f{index:03}.txt")), &payload).unwrap();
    }

    let mut config = WorkspaceConfig::new(&root);
    config.allow_write = false;
    let mut tools = LocalTools::new(config).unwrap();

    let requests: Vec<ToolRequest> = (0..FILES).map(read_request).collect();

    // 1) 并行批：一次 execute_batch，全部只读走线程并行。
    let (batch, ok) = timed(|| {
        tools
            .execute_batch(&requests)
            .iter()
            .filter(|outcome| outcome.is_ok())
            .count()
    });
    report("batch_parallel_read", FILES, batch, ok);

    // 2) 串行逐次：同一批请求逐个 execute，作为并行收益的基线。
    let (serial, ok) = timed(|| {
        requests
            .iter()
            .filter(|request| tools.execute(request).is_ok())
            .count()
    });
    report("serial_read", FILES, serial, ok);

    // 3) 混合批：一半只读、一半写，路径互不冲突（可并行分组）。
    let mut config = WorkspaceConfig::new(&root);
    config.allow_write = true;
    let mut tools = LocalTools::new(config).unwrap();
    let mixed: Vec<ToolRequest> = (0..FILES)
        .map(|index| {
            if index % 2 == 0 {
                read_request(index)
            } else {
                ToolRequest {
                    call_id: format!("write-{index}"),
                    tool: "write_file".into(),
                    read_only: false,
                    command: None,
                    paths: vec![format!("w{index:03}.txt")],
                    operation: None,
                    content: Some("bench".into()),
                    span: None,
                    arguments: None,
                }
            }
        })
        .collect();
    let (mixed_time, ok) = timed(|| {
        tools
            .execute_batch(&mixed)
            .iter()
            .filter(|outcome| outcome.is_ok())
            .count()
    });
    report("batch_mixed_read_write", FILES, mixed_time, ok);

    // 4) 冲突批：全部写同一个文件，必须串行且保留最后一次结果。
    let conflicting: Vec<ToolRequest> = (0..20)
        .map(|index| ToolRequest {
            call_id: format!("same-{index}"),
            tool: "write_file".into(),
            read_only: false,
            command: None,
            paths: vec!["same.txt".into()],
            operation: None,
            content: Some(format!("round {index}")),
            span: None,
            arguments: None,
        })
        .collect();
    let (conflict, ok) = timed(|| {
        tools
            .execute_batch(&conflicting)
            .iter()
            .filter(|outcome| outcome.is_ok())
            .count()
    });
    report("batch_conflicting_writes", conflicting.len(), conflict, ok);

    let _ = fs::remove_dir_all(&root);
    println!("tools_batch: ok");
}
