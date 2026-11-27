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
}
