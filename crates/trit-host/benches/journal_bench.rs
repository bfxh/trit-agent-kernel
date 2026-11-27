//! 变更台账基准：begin/finish 记录、错误记录与对账报告。
//!
//! 运行：`cargo bench -p trit-host --bench journal_bench`
//!
//! 每个写工具调用都会走一次 `record_begin` + `record_finish`（含 pre-image 备份），
//! 崩溃恢复时再对整份台账做 `report`。这里固定记录条数，观察每条的摊还成本。

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;
use trit_host::journal::MutationJournal;
use trit_host::{ToolOutcome, ToolRequest};

const OPS: usize = 200;

fn temp(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "trit-journal-bench-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn request(index: usize) -> ToolRequest {
    ToolRequest {
        call_id: format!("call-{index}"),
        tool: "write_file".into(),
        read_only: false,
        command: None,
        paths: vec![format!("file_{index:03}.txt")],
        operation: None,
        content: Some(format!("content {index}")),
        span: None,
        arguments: None,
    }
}

fn outcome(index: usize) -> ToolOutcome {
    ToolOutcome {
        call_id: format!("call-{index}"),
        tool: "write_file".into(),
        command: None,
        passed: true,
        assertions: 0,
        written_paths: vec![format!("file_{index:03}.txt")],
        in_doubt: false,
        output: "written".into(),
    }
}

fn report(case: &str, n: usize, elapsed: Duration) {
    println!(
        "{}",
        json!({
            "case": case,
            "n": n,
            "us": elapsed.as_micros().max(1),
            "per_op_us": (elapsed.as_micros() as f64 / n.max(1) as f64),
        })
    );
}

fn main() {
    // 1) begin + finish：新文件（无 pre-image），只记记录与截断输出。
    let root = temp("begin-finish");
    let mut journal = MutationJournal::open(&root).unwrap();
    let started = Instant::now();
    for index in 0..OPS {
        let entry = journal.record_begin(&request(index), None).unwrap();
        journal.record_finish(entry, &outcome(index)).unwrap();
    }
    report("begin_finish", OPS, started.elapsed());

    // 2) 已有文件：begin 需要复制 pre-image 备份，成本显著更高。
    let root_existing = temp("pre-image");
    for index in 0..OPS {
        fs::write(
            root_existing.join(format!("file_{index:03}.txt")),
            "old content",
        )
        .unwrap();
    }
    let mut journal_existing = MutationJournal::open(&root_existing).unwrap();
    let started = Instant::now();
    for index in 0..OPS {
        let entry = journal_existing
            .record_begin(&request(index), None)
            .unwrap();
        journal_existing
            .record_finish(entry, &outcome(index))
            .unwrap();
    }
    report("begin_finish_with_pre_image", OPS, started.elapsed());

    // 3) 错误记录：不产生备份，只追加一条 in_doubt。
    let root_errors = temp("errors");
    let mut journal_errors = MutationJournal::open(&root_errors).unwrap();
    let started = Instant::now();
    for index in 0..OPS {
        let entry = journal_errors.record_begin(&request(index), None).unwrap();
        journal_errors
            .record_error(entry, &request(index), "boom")
            .unwrap();
    }
    report("begin_error", OPS, started.elapsed());

    // 4) 对账：新建台账里全是 begin 没有 finish，report 必须全部挑出来。
    let started = Instant::now();
    let found = journal_errors.report().unwrap().in_doubt.len();
    report("report", found.max(1), started.elapsed());

    // 5) 大台账对账：把 1) 的台账复制多份后重开，测读取 + 对账规模。
    let big = temp("big-report");
    let source = root.join(".agent");
    for copy in 0..5 {
        let target = big.join(format!(".agent/copy{copy}"));
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        copy_dir(&source, &target);
    }
    let started = Instant::now();
    let _ = MutationJournal::open(&big).unwrap().report().unwrap();
    report("open_and_report", 5, started.elapsed());

    for path in [root, root_existing, root_errors, big] {
        let _ = fs::remove_dir_all(path);
    }
    println!("journal_bench: ok");
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    if !from.is_dir() {
        return;
    }
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            let _ = fs::copy(entry.path(), target);
        }
    }
}
