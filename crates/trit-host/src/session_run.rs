//! 执行派发单：读取 `.resume.jsonl` 里的 `TurnStarted`，构造一次 `run` 调用。
//!
//! 默认 dry-run，只输出将执行的命令；`--apply` 才启动新的 trit-host 进程。
//! 执行结果写入新的 `<resume>.run.jsonl`，不往派发单本身追加 turn。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::AgentError;
use crate::session::{SessionEvent, SessionLog};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeSpec {
    pub source: String,
    pub task: String,
    pub objective: String,
    pub max_steps: usize,
}

/// 读取派发单里最后一个 `TurnStarted`；没有则 fail closed。
pub fn read_resume_spec(path: &Path) -> Result<ResumeSpec, AgentError> {
    let log = SessionLog::open(path)?;
    let records = log.load()?;
    let Some((task, objective, max_steps)) = records.iter().rev().find_map(|record| {
        if let SessionEvent::TurnStarted {
            task,
            objective,
            max_steps,
        } = &record.event
        {
            Some((task.clone(), objective.clone(), *max_steps))
        } else {
            None
        }
    }) else {
        return Err(AgentError::SessionState(format!(
            "{} has no TurnStarted",
            path.display()
        )));
    };
    Ok(ResumeSpec {
        source: path.display().to_string(),
        task,
        objective,
        max_steps,
    })
}

/// 执行会话路径：与派发单同目录的 `<stem>.run.jsonl`；占用时加序号。
pub fn resume_target(source: &Path) -> PathBuf {
    let dir = source.parent().unwrap_or(Path::new("."));
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("resume");
    let base = dir.join(format!("{stem}.run.jsonl"));
    if !base.exists() {
        return base;
    }
    for index in 1..1000 {
        let candidate = dir.join(format!("{stem}.run.{index}.jsonl"));
        if !candidate.exists() {
            return candidate;
        }
    }
    base
}

/// 构造 `run` 子命令参数；dry-run 与 `--apply` 共用同一份。
pub fn resume_command(spec: &ResumeSpec, workspace: &Path, target: &Path) -> Vec<String> {
    vec![
        "run".to_owned(),
        "--workspace".to_owned(),
        workspace.display().to_string(),
        "--session".to_owned(),
        target.display().to_string(),
        "--task".to_owned(),
        spec.task.clone(),
        "--max-steps".to_owned(),
        spec.max_steps.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionLog;

    fn temp(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "trit-session-run-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn resume_file(root: &Path) -> PathBuf {
        let path = root.join("open.resume.jsonl");
        let mut log = SessionLog::open(&path).unwrap();
        log.append(SessionEvent::TurnStarted {
            task: "unfinished".into(),
            objective: "resume source".into(),
            max_steps: 6,
        })
        .unwrap();
        path
    }

    #[test]
    fn reads_last_turn_started() {
        let root = temp("read");
        let path = resume_file(&root);
        let spec = read_resume_spec(&path).unwrap();
        assert_eq!(spec.task, "unfinished");
        assert_eq!(spec.objective, "resume source");
        assert_eq!(spec.max_steps, 6);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn target_never_overwrites_an_existing_run() {
        let root = temp("target");
        let source = resume_file(&root);
        let first = resume_target(&source);
        assert_eq!(first.file_name().unwrap(), "open.resume.run.jsonl");
        std::fs::write(&first, "").unwrap();
        let second = resume_target(&source);
        assert_eq!(second.file_name().unwrap(), "open.resume.run.1.jsonl");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn command_carries_task_and_target() {
        let root = temp("cmd");
        let source = resume_file(&root);
        let spec = read_resume_spec(&source).unwrap();
        let target = root.join("run.jsonl");
        let command = resume_command(&spec, &root, &target);
        assert_eq!(command[0], "run");
        assert!(command.contains(&"--task".to_owned()));
        assert!(command.contains(&"unfinished".to_owned()));
        assert!(command.contains(&"--session".to_owned()));
        assert!(command.contains(&target.display().to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_turn_started_fails_closed() {
        let root = temp("empty");
        let path = root.join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        assert!(read_resume_spec(&path).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
