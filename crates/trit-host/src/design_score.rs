//! 设计质量评分：从代码设计、网络设计、前端设计、审美四个维度给产物打分。
//!
//! 只看产物内容（静态规则、可复算），不靠模型自评；每条扣分都带可读理由，
//! 供模型据此改进。评分是启发式，不替代测试与证据裁决。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// 设计维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesignAxis {
    /// 代码设计：模块边界、规模、错误处理、测试与注释。
    Code,
    /// 网络设计：超时、错误分类、响应上限、凭据。
    Network,
    /// 前端设计：无障碍、状态、响应式、令牌化。
    Frontend,
    /// 审美：克制、一致性、可读性。
    Aesthetics,
}

impl DesignAxis {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Network => "network",
            Self::Frontend => "frontend",
            Self::Aesthetics => "aesthetics",
        }
    }
}

/// 一条设计扣分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignFinding {
    pub axis: DesignAxis,
    pub rule: String,
    pub penalty: u8,
    pub detail: String,
}

/// 单个维度的得分（基础 100）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignAxisScore {
    pub axis: DesignAxis,
    pub score: u8,
    pub findings: Vec<DesignFinding>,
}

/// 一次设计评审的总报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignReport {
    pub axes: Vec<DesignAxisScore>,
    /// 四个维度的平均值；没有可评产物时为 100。
    pub overall: u8,
    /// 参与评分的文件数。
    pub files: usize,
}

/// 一个待评产物。
#[derive(Debug, Clone, Copy)]
pub struct Artifact<'a> {
    pub path: &'a str,
    pub content: &'a str,
}

/// 对一组产物做设计评分。
pub fn score_artifacts(artifacts: &[Artifact<'_>]) -> DesignReport {
    let mut findings: Vec<DesignFinding> = Vec::new();
    let mut frontend_files = 0usize;
    let mut network_files = 0usize;

    for artifact in artifacts {
        let kind = classify(artifact.path);
        if matches!(kind, Kind::Frontend | Kind::Style | Kind::Markup) {
            frontend_files += 1;
        }
        let network = looks_network(artifact.content);
        if network {
            network_files += 1;
        }
        code_rules(artifact, &mut findings);
        if network {
            network_rules(artifact, &mut findings);
        }
        if matches!(kind, Kind::Frontend | Kind::Style | Kind::Markup) {
            frontend_rules(artifact, &mut findings);
        }
        aesthetics_rules(artifact, kind, &mut findings);
    }

    let _ = (frontend_files, network_files);

    let mut axes = Vec::new();
    for axis in [
        DesignAxis::Code,
        DesignAxis::Network,
        DesignAxis::Frontend,
        DesignAxis::Aesthetics,
    ] {
        let own: Vec<DesignFinding> = findings
            .iter()
            .filter(|finding| finding.axis == axis)
            .cloned()
            .collect();
        let penalty: u32 = own.iter().map(|finding| u32::from(finding.penalty)).sum();
        let score = 100u32.saturating_sub(penalty).min(100) as u8;
        axes.push(DesignAxisScore {
            axis,
            score,
            findings: own,
        });
    }
    let overall = if axes.is_empty() {
        100
    } else {
        let sum: u32 = axes.iter().map(|axis| u32::from(axis.score)).sum();
        (sum / axes.len() as u32) as u8
    };
    DesignReport {
        axes,
        overall,
        files: artifacts.len(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Code,
    Frontend,
    Style,
    Markup,
    Other,
}

fn classify(path: &str) -> Kind {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or_default();
    match ext {
        "rs" | "ts" | "js" | "py" | "go" | "java" | "cs" | "kt" | "swift" => Kind::Code,
        "tsx" | "jsx" | "vue" | "svelte" => Kind::Frontend,
        "css" | "scss" | "less" => Kind::Style,
        "html" | "htm" => Kind::Markup,
        _ => Kind::Other,
    }
}

fn looks_network(content: &str) -> bool {
    [
        "reqwest",
        "ureq",
        "hyper",
        "axios",
        "fetch(",
        "http://",
        "https://",
        "HttpClient",
        ".post(",
        ".send()",
        "client.",
    ]
    .iter()
    .any(|needle| content.contains(needle))
}

fn push(
    findings: &mut Vec<DesignFinding>,
    axis: DesignAxis,
    rule: &str,
    penalty: u8,
    detail: impl Into<String>,
) {
    findings.push(DesignFinding {
        axis,
        rule: rule.to_owned(),
        penalty,
        detail: detail.into(),
    });
}

fn code_rules(artifact: &Artifact<'_>, findings: &mut Vec<DesignFinding>) {
    if !matches!(classify(artifact.path), Kind::Code) {
        return;
    }
    let lines: Vec<&str> = artifact.content.lines().collect();
    let count = lines.len();
    if count > 800 {
        push(
            findings,
            DesignAxis::Code,
            "file-too-long",
            10,
            format!("{}: {count} 行，超过 800", artifact.path),
        );
    }
    let panics = artifact
        .content
        .matches("unwrap()")
        .count()
        .saturating_add(artifact.content.matches("expect(").count())
        .saturating_add(artifact.content.matches("panic!").count());
    if panics > 5 {
        push(
            findings,
            DesignAxis::Code,
            "error-handling",
            10,
            format!(
                "{}: {panics} 处 unwrap/expect/panic，错误处理过于粗暴",
                artifact.path
            ),
        );
    }
    let tests = artifact.content.matches("#[test]").count()
        + artifact.content.matches("fn test_").count()
        + artifact.content.matches("describe(").count()
        + artifact.content.matches("it(").count();
    if tests == 0 && count > 200 {
        push(
            findings,
            DesignAxis::Code,
            "no-tests",
            10,
            format!("{}: {count} 行没有测试", artifact.path),
        );
    }
    let comments = lines
        .iter()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with("/*")
        })
        .count();
    if count > 200 && comments * 100 / count < 2 {
        push(
            findings,
            DesignAxis::Code,
            "no-comments",
            5,
            format!("{}: {count} 行里注释不足 2%", artifact.path),
        );
    }
    if has_repeated_block(&lines, 3) {
        push(
            findings,
            DesignAxis::Code,
            "duplicated-block",
            10,
            format!("{}: 存在连续 3 行以上完全重复的代码块", artifact.path),
        );
    }
}

fn has_repeated_block(lines: &[&str], window: usize) -> bool {
    if lines.len() < window * 2 {
        return false;
    }
    let mut seen: BTreeSet<Vec<&str>> = BTreeSet::new();
    for chunk in lines.windows(window) {
        let normalized: Vec<&str> = chunk.iter().map(|line| line.trim()).collect();
        if normalized
            .iter()
            .all(|line| line.is_empty() || *line == "{" || *line == "}")
        {
            continue;
        }
        let key: Vec<&str> = normalized
            .into_iter()
            .filter(|line| !line.is_empty())
            .collect();
        if key.is_empty() {
            continue;
        }
        if !seen.insert(key) {
            return true;
        }
    }
    false
}

fn network_rules(artifact: &Artifact<'_>, findings: &mut Vec<DesignFinding>) {
    let content = artifact.content;
    if !content.contains("timeout") && !content.contains("timeout_ms") {
        push(
            findings,
            DesignAxis::Network,
            "no-timeout",
            15,
            format!("{}: 有网络调用但没有超时设置", artifact.path),
        );
    }
    let has_status_branch = content.contains("status")
        || content.contains(".code(")
        || content.contains("response.ok")
        || content.contains("is_success");
    if !has_status_branch {
        push(
            findings,
            DesignAxis::Network,
            "no-status-classification",
            10,
            format!("{}: 未区分 4xx/5xx 等状态", artifact.path),
        );
    }
    if (content.contains("read_to_end") || content.contains(".text()"))
        && !content.contains("take(")
        && !content.contains("limit")
        && !content.contains("max_bytes")
        && !content.contains("MAX_")
    {
        push(
            findings,
            DesignAxis::Network,
            "unbounded-response",
            10,
            format!("{}: 响应体没有上限", artifact.path),
        );
    }
    if looks_like_hardcoded_secret(content) {
        push(
            findings,
            DesignAxis::Network,
            "hardcoded-secret",
            30,
            format!("{}: 疑似硬编码密钥", artifact.path),
        );
    }
}

fn looks_like_hardcoded_secret(content: &str) -> bool {
    let mut run = 0usize;
    for ch in content.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            run += 1;
        } else {
            run = 0;
        }
        if run >= 32 && content.contains("sk-") {
            return true;
        }
    }
    content.contains("Bearer ") && content.contains("sk-")
}

fn frontend_rules(artifact: &Artifact<'_>, findings: &mut Vec<DesignFinding>) {
    let content = artifact.content;
    let accessible = content.contains("aria-")
        || content.contains("alt=")
        || content.contains("role=")
        || content.contains("<label")
        || content.contains("aria_");
    if !accessible {
        push(
            findings,
            DesignAxis::Frontend,
            "no-accessibility",
            10,
            format!("{}: 没有 aria/alt/label 等无障碍信息", artifact.path),
        );
    }
    let states = ["loading", "empty", "error", "skeleton", "placeholder"]
        .iter()
        .filter(|needle| content.to_ascii_lowercase().contains(*needle))
        .count();
    if states < 2 {
        push(
            findings,
            DesignAxis::Frontend,
            "missing-ui-states",
            10,
            format!("{}: 缺少加载/空/错误等状态设计", artifact.path),
        );
    }
    if !content.contains("@media")
        && !content.contains("sm:")
        && !content.contains("md:")
        && !content.contains("lg:")
    {
        push(
            findings,
            DesignAxis::Frontend,
            "no-responsive",
            5,
            format!("{}: 没有响应式断点", artifact.path),
        );
    }
}

fn aesthetics_rules(artifact: &Artifact<'_>, kind: Kind, findings: &mut Vec<DesignFinding>) {
    if !matches!(kind, Kind::Frontend | Kind::Style | Kind::Markup) {
        return;
    }
    let content = artifact.content;
    let colors = count_hex_colors(content);
    if colors > 8 {
        push(
            findings,
            DesignAxis::Aesthetics,
            "too-many-colors",
            10,
            format!("{}: {colors} 种颜色，不够克制", artifact.path),
        );
    }
    let font_sizes = count_distinct_values(content, "font-size");
    if font_sizes > 5 {
        push(
            findings,
            DesignAxis::Aesthetics,
            "too-many-font-sizes",
            5,
            format!("{}: {font_sizes} 种字号，缺少层级令牌", artifact.path),
        );
    }
    let spacings = count_distinct_values(content, "padding");
    if spacings > 6 {
        push(
            findings,
            DesignAxis::Aesthetics,
            "inconsistent-spacing",
            5,
            format!("{}: {spacings} 种 padding 值，间距不成体系", artifact.path),
        );
    }
    let long_lines = content
        .lines()
        .filter(|line| line.chars().count() > 120)
        .count();
    let total = content.lines().count().max(1);
    if long_lines * 100 / total > 20 {
        push(
            findings,
            DesignAxis::Aesthetics,
            "long-lines",
            5,
            format!(
                "{}: 超长行占比 {}/{}，影响可读性",
                artifact.path, long_lines, total
            ),
        );
    }
}

fn count_hex_colors(content: &str) -> usize {
    let mut colors = BTreeSet::new();
    let bytes = content.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'#' && index + 3 < bytes.len() {
            let mut end = index + 1;
            while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                end += 1;
            }
            if end - index - 1 >= 3 && end - index - 1 <= 8 {
                colors.insert(content[index..end].to_ascii_lowercase());
            }
            index = end;
        } else {
            index += 1;
        }
    }
    colors.len()
}

fn count_distinct_values(content: &str, property: &str) -> usize {
    let mut values = BTreeSet::new();
    for line in content.lines() {
        let mut rest = line;
        while let Some(position) = rest.find(property) {
            rest = &rest[position + property.len()..];
            let Some(colon) = rest.find(':') else {
                continue;
            };
            let after = rest[colon + 1..].trim_start();
            let value: String = after
                .chars()
                .take_while(|ch| !matches!(ch, ';' | '}' | '\n'))
                .collect();
            let value = value.trim().to_ascii_lowercase();
            if !value.is_empty() {
                values.insert(value);
            }
            break;
        }
    }
    values.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art<'a>(path: &'a str, content: &'a str) -> Artifact<'a> {
        Artifact { path, content }
    }

    #[test]
    fn clean_artifact_scores_full_marks() {
        let report = score_artifacts(&[art(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )]);
        assert_eq!(report.overall, 100, "{:?}", report.axes);
        assert_eq!(report.files, 1);
    }

    #[test]
    fn code_design_flags_panics_and_missing_tests() {
        let mut content = String::new();
        for index in 0..40 {
            content.push_str(&format!("fn f{index}() {{ let x = some().unwrap(); }}\n"));
        }
        for index in 0..200 {
            content.push_str(&format!("let v{index} = {index};\n"));
        }
        let report = score_artifacts(&[art("src/big.rs", &content)]);
        let code = report
            .axes
            .iter()
            .find(|axis| axis.axis == DesignAxis::Code)
            .unwrap();
        assert!(code.score < 100);
        let rules: Vec<&str> = code.findings.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"error-handling"), "{rules:?}");
        assert!(rules.contains(&"no-tests"), "{rules:?}");
    }

    #[test]
    fn network_design_flags_missing_timeout_and_secret() {
        let content = "let key = \"sk-abcdefghijklmnopqrstuvwxyz012345\";\n\
                       let body = client.post(url).send();\n\
                       let text = response.text();\n";
        let report = score_artifacts(&[art("src/http.rs", content)]);
        let network = report
            .axes
            .iter()
            .find(|axis| axis.axis == DesignAxis::Network)
            .unwrap();
        let rules: Vec<&str> = network.findings.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"no-timeout"), "{rules:?}");
        assert!(rules.contains(&"hardcoded-secret"), "{rules:?}");
        assert!(network.score < 100);
    }

    #[test]
    fn frontend_design_flags_missing_states_and_a11y() {
        let content = "<div class=\"card\"><span>hi</span></div>\n";
        let report = score_artifacts(&[art("src/App.tsx", content)]);
        let frontend = report
            .axes
            .iter()
            .find(|axis| axis.axis == DesignAxis::Frontend)
            .unwrap();
        let rules: Vec<&str> = frontend.findings.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"no-accessibility"), "{rules:?}");
        assert!(rules.contains(&"missing-ui-states"), "{rules:?}");
    }

    #[test]
    fn aesthetics_flags_color_sprawl() {
        let mut content = String::new();
        for index in 0..12 {
            content.push_str(&format!(
                ".c{index} {{ color: #{:06x}; }}\n",
                index * 0x111111
            ));
        }
        let report = score_artifacts(&[art("src/app.css", &content)]);
        let aesthetics = report
            .axes
            .iter()
            .find(|axis| axis.axis == DesignAxis::Aesthetics)
            .unwrap();
        let rules: Vec<&str> = aesthetics
            .findings
            .iter()
            .map(|f| f.rule.as_str())
            .collect();
        assert!(rules.contains(&"too-many-colors"), "{rules:?}");
    }
}
