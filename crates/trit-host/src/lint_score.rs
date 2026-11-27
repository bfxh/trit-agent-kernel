//! 真实 lint 接入：把 ruff / golangci-lint / eslint 的机器可读输出折成设计分。
//!
//! 默认关闭：只有显式开启（`--lint-score`）才会跑外部进程；工具不存在或项目里
//! 没有对应清单时返回 `None`/`Err`，不阻塞主流程。子进程清空继承环境、有超时与输出上限。

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::design_score::{DesignAxis, DesignFinding, DesignReport};

/// 一次 lint 允许的最大输出字节数。
pub const MAX_LINT_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;

/// 支持的 lint 工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintTool {
    Ruff,
    Golangci,
    Eslint,
}

impl LintTool {
    pub fn id(&self) -> &'static str {
        match self {
            Self::Ruff => "ruff",
            Self::Golangci => "golangci-lint",
            Self::Eslint => "eslint",
        }
    }
}

/// 一次 lint 的结论（只读，不改文件）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LintSummary {
    pub tool: &'static str,
    pub errors: usize,
    pub warnings: usize,
    /// 进程退出码为 0（无问题）。
    pub ok: bool,
    /// stderr 末尾（截断），排障用。
    pub stderr_tail: String,
}

/// 按项目清单检测该用哪个 lint 工具。
pub fn detect(root: &Path) -> Option<LintTool> {
    if root.join("go.mod").is_file() {
        return Some(LintTool::Golangci);
    }
    if root.join("package.json").is_file() {
        return Some(LintTool::Eslint);
    }
    if root.join("pyproject.toml").is_file()
        || root.join("setup.cfg").is_file()
        || root.join("requirements.txt").is_file()
    {
        return Some(LintTool::Ruff);
    }
    None
}

/// 跑一次 lint；超时、启动失败返回 `Err`，工具不存在也走这里。
pub fn run(root: &Path, timeout: Duration) -> Result<LintSummary, String> {
    let tool = detect(root).ok_or_else(|| {
        "no lint tool detected (go.mod / package.json / pyproject.toml)".to_owned()
    })?;
    let (program, args) = match tool {
        LintTool::Ruff => ("ruff", vec!["check", "--output-format", "json", "."]),
        LintTool::Golangci => (
            "golangci-lint",
            vec!["run", "--out-format", "json", "./..."],
        ),
        LintTool::Eslint => ("eslint", vec!["-f", "json", "."]),
    };
    let mut command = Command::new(program);
    command
        .args(&args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::child_env::sanitize_with(&mut command, std::iter::empty::<(String, String)>());
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn {program}: {error}"))?;

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let reader_out = stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe
                .take(MAX_LINT_OUTPUT_BYTES + 1)
                .read_to_end(&mut buffer);
            buffer
        })
    });
    let reader_err = stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe
                .take(MAX_LINT_OUTPUT_BYTES + 1)
                .read_to_end(&mut buffer);
            buffer
        })
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(done)) => break done,
            Ok(None) => {}
            Err(error) => return Err(format!("wait {program}: {error}")),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program} timed out after {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let stdout = reader_out
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let stderr = reader_err
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    if stdout.len() as u64 > MAX_LINT_OUTPUT_BYTES {
        return Err(format!(
            "{program} output exceeds {MAX_LINT_OUTPUT_BYTES} bytes"
        ));
    }
    let text = String::from_utf8_lossy(&stdout);
    let (errors, warnings) = parse_output(tool, &text)?;
    let stderr_text = String::from_utf8_lossy(&stderr);
    let mut tail: Vec<String> = stderr_text
        .lines()
        .rev()
        .take(10)
        .map(|line| crate::diagnostics::redact_line(line, root))
        .collect();
    tail.reverse();
    Ok(LintSummary {
        tool: tool.id(),
        errors,
        warnings,
        ok: status.success(),
        stderr_tail: tail.join("\n"),
    })
}

/// 解析三种工具的 JSON 输出，返回 `(errors, warnings)`。
pub(crate) fn parse_output(tool: LintTool, text: &str) -> Result<(usize, usize), String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("bad {} json: {error}", tool.id()))?;
    match tool {
        LintTool::Ruff => {
            let items = value
                .as_array()
                .ok_or_else(|| "ruff output must be an array".to_owned())?;
            Ok((items.len(), 0))
        }
        LintTool::Golangci => {
            let issues = value
                .get("Issues")
                .and_then(Value::as_array)
                .ok_or_else(|| "golangci output missing Issues".to_owned())?;
            let mut errors = 0;
            let mut warnings = 0;
            for issue in issues {
                let severity = issue
                    .get("Severity")
                    .and_then(Value::as_str)
                    .unwrap_or("warning");
                if severity.eq_ignore_ascii_case("error") {
                    errors += 1;
                } else {
                    warnings += 1;
                }
            }
            Ok((errors, warnings))
        }
        LintTool::Eslint => {
            let files = value
                .as_array()
                .ok_or_else(|| "eslint output must be an array".to_owned())?;
            let mut errors = 0;
            let mut warnings = 0;
            for file in files {
                errors += file.get("errorCount").and_then(Value::as_u64).unwrap_or(0) as usize;
                warnings += file
                    .get("warningCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
            }
            Ok((errors, warnings))
        }
    }
}

/// 把 lint 结论折进设计分的代码轴：每 10 个 error 扣 5，上限 25。
pub fn apply(report: &mut DesignReport, summary: &LintSummary) {
    let penalty = (((summary.errors as u32) / 10) * 5).min(25) as u8;
    if penalty == 0 {
        return;
    }
    if let Some(axis) = report
        .axes
        .iter_mut()
        .find(|axis| axis.axis == DesignAxis::Code)
    {
        axis.score = axis.score.saturating_sub(penalty);
        axis.findings.push(DesignFinding {
            axis: DesignAxis::Code,
            rule: "lint-errors".to_owned(),
            penalty,
            detail: format!(
                "{}: {} error(s), {} warning(s)",
                summary.tool, summary.errors, summary.warnings
            ),
        });
    }
    let sum: u32 = report.axes.iter().map(|axis| u32::from(axis.score)).sum();
    report.overall = (sum / report.axes.len().max(1) as u32) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trit-lint-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn detect_uses_project_manifests() {
        let root = temp("detect");
        assert!(detect(&root).is_none());
        std::fs::write(root.join("go.mod"), "module x\n").unwrap();
        assert_eq!(detect(&root), Some(LintTool::Golangci));
        let _ = std::fs::remove_dir_all(root);

        let root = temp("detect-py");
        std::fs::write(root.join("pyproject.toml"), "[tool.ruff]\n").unwrap();
        assert_eq!(detect(&root), Some(LintTool::Ruff));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parses_ruff_golangci_and_eslint() {
        let ruff = r#"[{"code":"E501"},{"code":"F401"}]"#;
        assert_eq!(parse_output(LintTool::Ruff, ruff).unwrap(), (2, 0));

        let golangci =
            r#"{"Issues":[{"Severity":"error"},{"Severity":"warning"},{"Severity":"warning"}]}"#;
        assert_eq!(parse_output(LintTool::Golangci, golangci).unwrap(), (1, 2));

        let eslint = r#"[{"errorCount":3,"warningCount":1},{"errorCount":2,"warningCount":0}]"#;
        assert_eq!(parse_output(LintTool::Eslint, eslint).unwrap(), (5, 1));
    }

    #[test]
    fn malformed_output_fails_closed() {
        assert!(parse_output(LintTool::Ruff, "not-json").is_err());
        assert!(parse_output(LintTool::Golangci, "{}").is_err());
        assert!(parse_output(LintTool::Eslint, "{}").is_err());
    }

    #[test]
    fn apply_penalizes_code_axis() {
        let mut report = crate::design_score::score_artifacts(&[]);
        let before = report.overall;
        let summary = LintSummary {
            tool: "ruff",
            errors: 25,
            warnings: 0,
            ok: false,
            stderr_tail: String::new(),
        };
        apply(&mut report, &summary);
        let code = report
            .axes
            .iter()
            .find(|axis| axis.axis == DesignAxis::Code)
            .unwrap();
        assert_eq!(code.score, 100 - 10, "25 error -> 10 分");
        assert!(
            code.findings
                .iter()
                .any(|finding| finding.rule == "lint-errors")
        );
        assert!(report.overall < before);
    }

    #[test]
    fn no_errors_means_no_penalty() {
        let mut report = crate::design_score::score_artifacts(&[]);
        let summary = LintSummary {
            tool: "eslint",
            errors: 0,
            warnings: 9,
            ok: true,
            stderr_tail: String::new(),
        };
        apply(&mut report, &summary);
        assert_eq!(report.overall, 100);
    }
}
