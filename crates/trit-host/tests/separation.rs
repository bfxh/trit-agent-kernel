//! planner/executor 分离的三条不变量：工具隔离、会话隔离、缓存账本隔离。

use std::path::PathBuf;
use std::sync::Arc;

use trit_host::{
    AgentError, READ_ONLY_TOOLS, ReadOnlyCatalog, ReadOnlyTools, ScriptedModel, ScriptedTools,
    Session, SessionPaths, ToolCatalog, ToolExecutor, ToolOutcome, ToolRequest, ToolSpec,
    is_read_only_tool, planner_tool_specs,
};

fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("trit-host-separation-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn outcome(call_id: &str, tool: &str) -> ToolOutcome {
    ToolOutcome {
        call_id: call_id.into(),
        tool: tool.into(),
        passed: true,
        ..ToolOutcome::default()
    }
}

#[test]
fn planner_denies_writes_before_reaching_inner() {
    let mut tools = ReadOnlyTools::new(ScriptedTools::new(vec![outcome("r1", "read_file")]));
    for request in [
        ToolRequest::write("w1", "write_file", "a.txt", "x"),
        ToolRequest::shell("s1", "echo hi"),
    ] {
        let error = tools.execute(&request).unwrap_err();
        assert!(
            matches!(error, AgentError::ToolDenied { .. }),
            "{:?} should be denied, got {error:?}",
            request.tool
        );
    }
    // 拒绝没有消耗内层队列：随后的只读请求仍能拿到结果。
    let got = tools.execute(&ToolRequest::read("r1", "a.txt")).unwrap();
    assert_eq!(got.call_id, "r1");
}

#[test]
fn planner_rejects_mislabelled_read_only_requests() {
    let mut tools = ReadOnlyTools::new(ScriptedTools::new(vec![]));
    let mut disguised = ToolRequest::read("r2", "a.txt");
    disguised.read_only = false;
    let error = tools.execute(&disguised).unwrap_err();
    assert!(matches!(error, AgentError::ToolDenied { .. }));
}

#[test]
fn read_only_whitelist_is_explicit() {
    assert!(is_read_only_tool("read_file"));
    assert!(is_read_only_tool("search_code"));
    // submit_plan 是 planner 的输出通道，不写工作区。
    assert!(is_read_only_tool("submit_plan"));
    for write_tool in ["write_file", "move_path", "bash", "shell", "checkpoint"] {
        assert!(
            !is_read_only_tool(write_tool),
            "{write_tool} must not be read-only"
        );
    }
    assert!(READ_ONLY_TOOLS.contains(&"read_file"));
    assert!(READ_ONLY_TOOLS.contains(&"submit_plan"));
}

#[test]
fn session_paths_are_distinct_under_dir() {
    let dir = temp_dir("paths");
    let paths = SessionPaths::under(&dir);
    assert!(paths.are_distinct());
    assert_eq!(paths.planner, dir.join("planner.jsonl"));
    assert_eq!(paths.executor, dir.join("executor.jsonl"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn planner_and_executor_sessions_have_independent_state() {
    let dir = temp_dir("sessions");
    let paths = SessionPaths::under(&dir);
    // planner 已有一次完整的历史；executor 从零开始。
    std::fs::write(
        &paths.planner,
        concat!(
            "{\"schema\":1,\"seq\":1,\"event\":{\"type\":\"turn_started\",\"task\":\"plan\",\"objective\":\"research\",\"max_steps\":4}}\n",
            "{\"schema\":1,\"seq\":2,\"event\":{\"type\":\"turn_finished\",\"steps\":0,\"complete\":false}}\n",
        ),
    )
    .unwrap();

    let planner = Session::open(
        ScriptedModel::new(vec![]),
        ReadOnlyTools::new(ScriptedTools::new(vec![])),
        paths.planner.clone(),
    )
    .unwrap();
    let executor = Session::open(
        ScriptedModel::new(vec![]),
        ScriptedTools::new(vec![]),
        paths.executor.clone(),
    )
    .unwrap();

    assert_ne!(planner.log().path(), executor.log().path());
    assert!(
        !Arc::ptr_eq(&planner.cache_ledger(), &executor.cache_ledger()),
        "planner and executor must not share a cache ledger"
    );
    assert_eq!(planner.log().load().unwrap().len(), 2);
    assert!(
        executor.log().load().unwrap().is_empty(),
        "executor must not see planner records"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn tool_spec(name: &str, read_only: bool) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("{name} tool"),
        read_only,
        input_schema: None,
    }
}

#[test]
fn planner_catalog_drops_write_tools() {
    let specs = vec![
        tool_spec("read_file", true),
        tool_spec("write_file", false),
        tool_spec("search_code", true),
        tool_spec("bash", false),
        tool_spec("move_path", false),
        tool_spec("glob", true),
    ];
    let filtered = planner_tool_specs(&specs);
    assert_eq!(
        filtered
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read_file", "search_code", "glob"]
    );

    struct Catalog(Vec<ToolSpec>);
    impl ToolCatalog for Catalog {
        fn tools(&self) -> Vec<ToolSpec> {
            self.0.clone()
        }
    }
    let catalog = ReadOnlyCatalog::new(Catalog(specs));
    let names: Vec<String> = catalog.tools().into_iter().map(|tool| tool.name).collect();
    assert_eq!(names, vec!["read_file", "search_code", "glob"]);
}

#[test]
fn planner_catalog_requires_both_name_and_flag() {
    // 名字在白名单但标志声称可写：仍然过滤。
    let specs = vec![tool_spec("read_file", false), tool_spec("read_file", true)];
    let filtered = planner_tool_specs(&specs);
    assert_eq!(filtered.len(), 1);
    assert!(filtered[0].read_only);
}
