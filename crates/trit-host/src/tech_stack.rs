//! 编程语言用途限制：某些语言只允许出现在测试代码里。
//!
//! 项目主栈之外的语言（默认 Python）不得进入生产代码：`write_file` 在落盘前校验，
//! 越界直接拒绝。这是硬约束，不靠评分扣分。

/// 只允许作为测试代码使用的扩展名（小写、不带点）。
pub const TEST_ONLY_EXTENSIONS: &[&str] = &["py"];

/// 测试代码的路径判定：目录（`tests` / `test` / `spec`）或文件名约定。
pub fn is_test_path(path: &str) -> bool {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or_default();
    lower.starts_with("tests/")
        || lower.contains("/tests/")
        || lower.starts_with("test/")
        || lower.contains("/test/")
        || lower.starts_with("spec/")
        || lower.contains("/spec/")
        || name == "conftest.py"
        || name.starts_with("test_")
        || name.ends_with("_test.py")
}

/// 校验一次写入：test-only 语言只能写测试路径。
pub fn validate_write_path(path: &str) -> Result<(), String> {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or_default();
    if TEST_ONLY_EXTENSIONS.contains(&ext) && !is_test_path(path) {
        return Err(format!(
            "{path}: .{ext} is only allowed under test paths (tests/, test_*.py, *_test.py, conftest.py)"
        ));
    }
    Ok(())
}

/// 从 shell 命令里粗略提取写入目标（重定向、`tee`、`cp`/`mv` 目标）。
///
/// 启发式：能挡住"顺手 `> src/x.py`"这类常见写法，但无法覆盖所有 shell 语法
/// （变量展开、`eval`、脚本内部写文件等）。只用于拦截明显越界，不作为安全边界。
pub fn shell_write_targets(command: &str) -> Vec<String> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut targets: Vec<String> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if let Some(position) = token.find('>') {
            let rest = token[position + 1..].trim_start_matches('>');
            if !rest.is_empty() && !rest.starts_with('&') {
                targets.push(rest.to_owned());
            } else if let Some(next) = tokens.get(index + 1)
                && !next.starts_with('&')
            {
                targets.push((*next).to_owned());
            }
        }
        if *token == "tee"
            && let Some(next) = tokens.get(index + 1)
            && !next.starts_with('-')
        {
            targets.push((*next).to_owned());
        }
        if (*token == "cp" || *token == "mv")
            && let Some(last) = tokens.last()
            && last != token
        {
            targets.push((*last).to_owned());
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_is_allowed_only_under_test_paths() {
        for path in [
            "tests/test_math.py",
            "tests/sub/test_x.py",
            "pkg/conftest.py",
            "pkg/test_api.py",
            "pkg/api_test.py",
            "spec/test_flow.py",
        ] {
            assert!(validate_write_path(path).is_ok(), "{path} 应允许");
        }
        for path in ["src/evil.py", "pkg/main.py", "app/scripts/run.py"] {
            let error = validate_write_path(path).unwrap_err();
            assert!(error.contains("test paths"), "{path}: {error}");
        }
        // 非 test-only 语言不受影响。
        for path in ["src/lib.rs", "docs/readme.md", "src/app.ts", "cmd/main.go"] {
            assert!(validate_write_path(path).is_ok(), "{path} 应允许");
        }
    }

    #[test]
    fn windows_separators_are_normalized() {
        assert!(validate_write_path("tests\\test_unit.py").is_ok());
        assert!(validate_write_path("src\\main.py").is_err());
    }

    #[test]
    fn shell_write_targets_catch_common_forms() {
        for command in [
            "echo x > src/a.py",
            "echo x >> src/a.py",
            "echo x 2> src/a.py",
            "echo x >src/a.py",
        ] {
            assert!(
                shell_write_targets(command).contains(&"src/a.py".to_owned()),
                "{command}"
            );
        }
        assert!(shell_write_targets("cat x | tee src/a.py").contains(&"src/a.py".to_owned()));
        assert!(shell_write_targets("cp tests/a.py src/a.py").contains(&"src/a.py".to_owned()));
        assert!(shell_write_targets("ls -la").is_empty());
        // 测试路径的写入允许。
        assert!(validate_write_path("tests/test_a.py").is_ok());
    }
}
