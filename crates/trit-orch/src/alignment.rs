//! 对齐门：目标 → 需求 → 产物/证据/变更。
//!
//! 这是 `bfxh/trit` 的 `alignment/1`–`alignment/3` 的 Rust 迁移版。它只做
//! **静态、可离线复算** 的结构检查：父链、目标覆盖、产物/证据引用、sha256、
//! locator、non-goal、声明变更、v3 的 run command provenance。
//!
//! 它不替代执行器、沙盒或人工复核：`aligned` 只表示这份清单在给定输入下
//! 没有断链，不表示产物在语义上真的实现了需求。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use globset::GlobBuilder;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Error, digest};

pub const SCHEMA_V1: &str = "alignment/1";
pub const SCHEMA_V2: &str = "alignment/2";
pub const SCHEMA_V3: &str = "alignment/3";
const MAX_ARRAY: usize = 4096;
const MAX_PATH: usize = 1024;

/// 一次检查的输入选项。
#[derive(Debug, Clone)]
pub struct AlignmentOptions {
    pub changed: Option<Vec<String>>,
    pub changed_known: bool,
    pub strict: bool,
    pub executed: Option<Vec<String>>,
}

impl Default for AlignmentOptions {
    fn default() -> Self {
        Self {
            changed: None,
            changed_known: false,
            strict: true,
            executed: None,
        }
    }
}

/// `scaffold` 的输入。骨架故意留空，不是自动对齐声明。
#[derive(Debug, Clone)]
pub struct ScaffoldOptions {
    pub goal: String,
    pub requirements: Vec<String>,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScaffoldOutcome {
    pub manifest: String,
    pub goals: usize,
    pub requirements: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlignmentFailure {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlignmentWarning {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AlignmentCounts {
    pub goals: usize,
    pub requirements: usize,
    pub must: usize,
    pub artifacts: usize,
    pub evidence: usize,
    pub provenance: usize,
    pub provenance_verified: usize,
    pub changed: usize,
    pub changed_known: usize,
    pub linked_changes: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AlignmentCoverage {
    pub requirements_with_artifact: usize,
    pub requirements_with_evidence: usize,
    pub requirements_with_provenance: usize,
    pub requirements_provenance_verified: usize,
    pub requirements_with_changes: usize,
    pub requirements_changes_realized: usize,
    pub requirements_full: usize,
    pub must_full: usize,
    pub goals_covered: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlignmentReport {
    pub schema: String,
    pub status: String,
    pub exit: i32,
    pub manifest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    pub score: f64,
    pub must_score: f64,
    pub counts: AlignmentCounts,
    pub coverage: AlignmentCoverage,
    pub failures: Vec<AlignmentFailure>,
    pub warnings: Vec<AlignmentWarning>,
}

impl AlignmentReport {
    fn new(schema: &str, manifest: &str) -> Self {
        Self {
            schema: schema.to_owned(),
            status: "unknown".to_owned(),
            exit: 3,
            manifest: manifest.to_owned(),
            goal: None,
            score: 0.0,
            must_score: 0.0,
            counts: AlignmentCounts::default(),
            coverage: AlignmentCoverage::default(),
            failures: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn fail(&mut self, code: &str, message: impl Into<String>, node: Option<&str>) {
        self.failures.push(AlignmentFailure {
            code: code.to_owned(),
            message: message.into(),
            node: node.map(ToOwned::to_owned),
        });
    }

    fn warn(&mut self, code: &str, message: impl Into<String>, node: Option<&str>) {
        self.warnings.push(AlignmentWarning {
            code: code.to_owned(),
            message: message.into(),
            node: node.map(ToOwned::to_owned),
        });
    }

    fn settle(&mut self) {
        if self.failures.is_empty() {
            self.status = "aligned".to_owned();
            self.exit = 0;
        } else {
            self.status = "unaligned".to_owned();
            self.exit = 1;
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    #[serde(default)]
    goal: Option<Goal>,
    #[serde(default)]
    goals: Vec<Goal>,
    #[serde(default)]
    requirements: Vec<Requirement>,
    #[serde(default)]
    artifacts: Vec<Artifact>,
    #[serde(default)]
    evidence: Vec<Evidence>,
    #[serde(default)]
    non_goals: Vec<NonGoal>,
    #[serde(default)]
    allow_unlinked: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    notes: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Goal {
    id: String,
    text: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    notes: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Requirement {
    id: String,
    text: String,
    #[serde(default = "default_priority")]
    priority: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    changes: Vec<String>,
    #[serde(default)]
    contradicts: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    notes: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    tags: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    owner: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    id: String,
    path: String,
    #[serde(default)]
    locator: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    kind: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    notes: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    id: String,
    path: String,
    #[serde(default)]
    locator: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    notes: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    tags: Vec<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    command: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct NonGoal {
    id: String,
    text: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    notes: Option<serde_json::Value>,
}

fn default_priority() -> String {
    "must".to_owned()
}

/// 从磁盘读清单并检查。清单缺失/读不出是 `unknown`（退出 3），不是 aligned。
pub fn check(root: &Path, manifest: &Path, options: &AlignmentOptions) -> AlignmentReport {
    let root = normalize_root(root);
    let root_text = root.to_string_lossy().to_string();
    let manifest_abs = if manifest.is_absolute() {
        manifest.to_path_buf()
    } else {
        root.join(manifest)
    };
    let display = display_path(&root, &manifest_abs);
    let mut report = AlignmentReport::new(SCHEMA_V1, &display);
    if let Err(reason) = manifest_inside_root(&root, &manifest_abs) {
        report.fail("manifest-outside-root", reason, None);
        return report;
    }
    let bytes = match fs::read(&manifest_abs) {
        Ok(bytes) => bytes,
        Err(error) => {
            report.fail(
                "manifest-missing",
                format!("对齐清单不存在或读不出：{display} ({error})"),
                None,
            );
            return report;
        }
    };
    let doc: Manifest = match serde_json::from_slice(&bytes) {
        Ok(doc) => doc,
        Err(error) => {
            report.fail(
                "manifest-unreadable",
                format!("对齐清单读不出或坏 JSON：{error}"),
                None,
            );
            return report;
        }
    };
    if !matches!(doc.schema.as_str(), SCHEMA_V1 | SCHEMA_V2 | SCHEMA_V3) {
        report.fail(
            "schema-mismatch",
            format!(
                "schema 必须是 alignment/1、alignment/2 或 alignment/3，实际 {}",
                doc.schema
            ),
            None,
        );
        return report;
    }
    report.schema = doc.schema.clone();
    validate(&root, &root_text, &doc, options, &mut report);
    report.settle();
    report
}

/// 生成一份故意不完整的 `alignment/2` 骨架。
///
/// 只写目标与需求文本，`artifacts` / `evidence` / `changes` 全留空：调用方
/// 必须自己补齐图，门在此之前不会返回 `aligned`。这不是自动对齐。
pub fn scaffold(
    root: &Path,
    manifest: &Path,
    options: &ScaffoldOptions,
) -> Result<ScaffoldOutcome, Error> {
    let goal = options.goal.trim();
    if goal.is_empty() {
        return Err(Error::AlignmentScaffold {
            reason: "--goal 不能为空".to_owned(),
        });
    }
    let requirements: Vec<String> = options
        .requirements
        .iter()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect();
    if requirements.is_empty() {
        return Err(Error::AlignmentScaffold {
            reason: "至少给一个 requirement（重复传参）".to_owned(),
        });
    }
    if requirements.len() > MAX_ARRAY {
        return Err(Error::AlignmentScaffold {
            reason: format!("需求数量 {} 超过上限 {MAX_ARRAY}", requirements.len()),
        });
    }

    let root = fs::canonicalize(root).map_err(|error| Error::AlignmentScaffoldIo {
        path: root.display().to_string(),
        message: format!("root 不可用：{error}"),
    })?;
    if !root.is_dir() {
        return Err(Error::AlignmentScaffold {
            reason: format!("root 不是目录：{}", root.display()),
        });
    }
    let manifest_text = manifest.to_str().ok_or_else(|| Error::AlignmentScaffold {
        reason: "manifest 路径必须能表示为 UTF-8".to_owned(),
    })?;
    let (normalized, full) =
        safe_rel(&root, manifest_text).map_err(|reason| Error::AlignmentScaffold { reason })?;
    if full.is_dir() {
        return Err(Error::AlignmentScaffold {
            reason: format!("目标是目录而不是文件：{normalized}"),
        });
    }
    if let Ok(metadata) = fs::symlink_metadata(&full)
        && metadata.file_type().is_symlink()
    {
        return Err(Error::AlignmentScaffold {
            reason: format!("manifest 不能是符号链接：{normalized}"),
        });
    }
    if full.exists() && !options.force {
        return Err(Error::AlignmentScaffold {
            reason: format!("已存在，未覆盖（要覆盖加 --force）：{normalized}"),
        });
    }

    // 目标可以尚不存在，但它最近的已存在祖先必须仍在 root 内，防止
    // 通过父目录符号链接把 MANIFEST 写到仓外。
    let mut probe = full.parent();
    while let Some(parent) = probe {
        if parent.exists() {
            let canonical =
                fs::canonicalize(parent).map_err(|error| Error::AlignmentScaffoldIo {
                    path: parent.display().to_string(),
                    message: error.to_string(),
                })?;
            if !canonical.starts_with(&root) {
                return Err(Error::AlignmentScaffold {
                    reason: format!("manifest 经父目录逃出 root：{normalized}"),
                });
            }
            break;
        }
        probe = parent.parent();
    }

    let parent = full.parent().ok_or_else(|| Error::AlignmentScaffold {
        reason: format!("manifest 没有父目录：{normalized}"),
    })?;
    fs::create_dir_all(parent).map_err(|error| Error::AlignmentScaffoldIo {
        path: parent.display().to_string(),
        message: error.to_string(),
    })?;
    let canonical_parent =
        fs::canonicalize(parent).map_err(|error| Error::AlignmentScaffoldIo {
            path: parent.display().to_string(),
            message: error.to_string(),
        })?;
    if !canonical_parent.starts_with(&root) {
        return Err(Error::AlignmentScaffold {
            reason: format!("manifest 经父目录逃出 root：{normalized}"),
        });
    }

    let document = json!({
        "schema": SCHEMA_V2,
        "goals": [{"id": "G1", "text": goal}],
        "requirements": requirements
            .iter()
            .enumerate()
            .map(|(index, text)| json!({
                "id": format!("R{}", index + 1),
                "text": text,
                "priority": "must",
                "parent": "G1",
                "artifacts": [],
                "evidence": [],
                "changes": [],
            }))
            .collect::<Vec<_>>(),
        "artifacts": [],
        "evidence": [],
        "non_goals": [],
        "allow_unlinked": [],
        "notes": "scaffold: artifacts/evidence/changes 故意留空；补齐前 gate 不会 aligned",
    });
    let mut bytes =
        serde_json::to_vec_pretty(&document).map_err(|error| Error::AlignmentScaffoldIo {
            path: normalized.clone(),
            message: format!("序列化失败：{error}"),
        })?;
    bytes.push(b'\n');

    if full.exists() {
        // `force` 明确覆盖；为了不产生半截旧文件，先写同目录临时文件再替换。
        let temp = scaffold_temp_path(&full);
        write_scaffold_temp(&temp, &bytes)?;
        if let Err(error) = fs::remove_file(&full) {
            let _ = fs::remove_file(&temp);
            return Err(Error::AlignmentScaffoldIo {
                path: full.display().to_string(),
                message: error.to_string(),
            });
        }
        if let Err(error) = fs::rename(&temp, &full) {
            let _ = fs::remove_file(&temp);
            return Err(Error::AlignmentScaffoldIo {
                path: full.display().to_string(),
                message: error.to_string(),
            });
        }
    } else {
        let temp = scaffold_temp_path(&full);
        write_scaffold_temp(&temp, &bytes)?;
        if let Err(error) = fs::rename(&temp, &full) {
            let _ = fs::remove_file(&temp);
            return Err(Error::AlignmentScaffoldIo {
                path: full.display().to_string(),
                message: error.to_string(),
            });
        }
    }

    Ok(ScaffoldOutcome {
        manifest: normalized,
        goals: 1,
        requirements: requirements.len(),
    })
}

fn scaffold_temp_path(full: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = full
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "alignment.json".to_owned());
    full.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()))
}

fn write_scaffold_temp(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| Error::AlignmentScaffoldIo {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
    if let Err(error) = file.write_all(bytes) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(Error::AlignmentScaffoldIo {
            path: path.display().to_string(),
            message: error.to_string(),
        });
    }
    Ok(())
}

fn manifest_inside_root(root: &Path, manifest: &Path) -> Result<(), String> {
    if manifest
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!("manifest 不能包含 ..：{}", manifest.display()));
    }
    let mut probe = manifest.parent();
    while let Some(parent) = probe {
        if parent.exists() {
            let canonical = fs::canonicalize(parent)
                .map_err(|error| format!("manifest 父目录读不出：{error}"))?;
            if !canonical.starts_with(root) {
                return Err(format!("manifest 逃出 root：{}", manifest.display()));
            }
            break;
        }
        probe = parent.parent();
    }
    if manifest.exists() {
        let canonical =
            fs::canonicalize(manifest).map_err(|error| format!("manifest 读不出：{error}"))?;
        if !canonical.starts_with(root) {
            return Err(format!(
                "manifest 经符号链接逃出 root：{}",
                manifest.display()
            ));
        }
    }
    Ok(())
}

fn normalize_root(root: &Path) -> PathBuf {
    fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn display_path(root: &Path, path: &Path) -> String {
    let abs = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match abs.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

fn validate(
    root: &Path,
    root_text: &str,
    doc: &Manifest,
    options: &AlignmentOptions,
    report: &mut AlignmentReport,
) {
    if doc.requirements.len() > MAX_ARRAY
        || doc.artifacts.len() > MAX_ARRAY
        || doc.evidence.len() > MAX_ARRAY
        || doc.goals.len() > MAX_ARRAY
    {
        report.fail("manifest-too-large", "清单数组超过上限", None);
        return;
    }

    let goals = read_goals(doc, report);
    let goal_map: BTreeMap<String, &Goal> = goals.iter().map(|g| (g.id.clone(), g)).collect();
    if let Some(root_goal) = unique_root(&goal_map, report) {
        report.goal = Some(root_goal);
    }
    report.counts.goals = goals.len();

    // 一个 id 命名空间；重复 id 直接拒绝。
    let mut ids: BTreeMap<String, &'static str> = BTreeMap::new();
    for goal in &goals {
        ids.insert(goal.id.clone(), "goal");
    }
    collect_ids(&doc.requirements, "requirement", &mut ids, report);
    collect_ids(&doc.artifacts, "artifact", &mut ids, report);
    collect_ids(&doc.evidence, "evidence", &mut ids, report);
    collect_ids(&doc.non_goals, "non_goal", &mut ids, report);

    let req_map: BTreeMap<String, &Requirement> =
        doc.requirements.iter().map(|r| (r.id.clone(), r)).collect();
    let artifact_map: BTreeMap<String, &Artifact> =
        doc.artifacts.iter().map(|a| (a.id.clone(), a)).collect();
    let evidence_map: BTreeMap<String, &Evidence> =
        doc.evidence.iter().map(|e| (e.id.clone(), e)).collect();

    if doc.requirements.is_empty() {
        report.fail(
            "no-requirements",
            "至少需要一条 requirement；空清单不算对齐",
            None,
        );
    }

    // 目标树 + 需求父链。
    validate_requirement_parents(&goal_map, &req_map, report);
    validate_goal_coverage(&goal_map, &req_map, report);

    // non-goal 的对象级排除。
    let mut ng_art_refs = BTreeSet::new();
    let mut ng_ev_refs = BTreeSet::new();
    for ng in &doc.non_goals {
        if ng.text.trim().is_empty() {
            report.fail(
                "non-goal-text",
                format!("{} 的 text 不能为空", ng.id),
                Some(&ng.id),
            );
        }
        if ng.paths.is_empty() {
            report.fail(
                "non-goal-paths",
                format!("{} 必须给出至少一个 paths glob", ng.id),
                Some(&ng.id),
            );
        }
        for path in &ng.paths {
            if let Err(error) = safe_rel(root, path) {
                report.fail("non-goal-paths", error, Some(&ng.id));
            }
        }
        for reference in &ng.artifacts {
            if !artifact_map.contains_key(reference) {
                report.fail(
                    "non-goal-artifacts-ref",
                    format!("{} 引用了不存在的 artifacts：{reference}", ng.id),
                    Some(&ng.id),
                );
            } else {
                ng_art_refs.insert(reference.clone());
            }
        }
        for reference in &ng.evidence {
            if !evidence_map.contains_key(reference) {
                report.fail(
                    "non-goal-evidence-ref",
                    format!("{} 引用了不存在的 evidence：{reference}", ng.id),
                    Some(&ng.id),
                );
            } else {
                ng_ev_refs.insert(reference.clone());
            }
        }
    }

    // 产物/证据的磁盘检查。
    let mut artifact_ok: BTreeSet<String> = BTreeSet::new();
    for artifact in &doc.artifacts {
        if artifact.path.trim().is_empty() {
            report.fail(
                "artifact-path",
                format!("{} 缺少 path", artifact.id),
                Some(&artifact.id),
            );
            continue;
        }
        match checked_file(
            root,
            &artifact.path,
            artifact.sha256.as_deref(),
            artifact.locator.as_deref(),
            "artifact",
        ) {
            Ok(norm) => {
                artifact_ok.insert(artifact.id.clone());
                for ng in &doc.non_goals {
                    if glob_match(&norm, &ng.paths) {
                        report.fail(
                            "non-goal-artifact-path",
                            format!("{} 的产物路径落进 non_goal {}：{norm}", artifact.id, ng.id),
                            Some(&artifact.id),
                        );
                    }
                }
            }
            Err((code, message)) => report.fail(&code, message, Some(&artifact.id)),
        }
    }

    let mut evidence_ok: BTreeSet<String> = BTreeSet::new();
    let mut evidence_prov: BTreeSet<String> = BTreeSet::new();
    let mut evidence_verified: BTreeSet<String> = BTreeSet::new();
    let executed_norm: Option<BTreeSet<String>> = options
        .executed
        .as_ref()
        .map(|items| items.iter().map(|s| norm_command(s)).collect());
    for evidence in &doc.evidence {
        if evidence.path.trim().is_empty() {
            report.fail(
                "evidence-path",
                format!("{} 缺少 path", evidence.id),
                Some(&evidence.id),
            );
            continue;
        }
        let kind = if doc.schema == SCHEMA_V3 {
            evidence.kind.as_deref().unwrap_or("file")
        } else {
            if evidence.kind.is_some() || evidence.command.is_some() {
                report.fail(
                    "unknown-field",
                    format!("{} 在 v1/v2 里不能使用 kind/command", evidence.id),
                    Some(&evidence.id),
                );
            }
            "file"
        };
        if doc.schema == SCHEMA_V3 && !matches!(kind, "file" | "run") {
            report.fail(
                "evidence-kind",
                format!("{} 的 kind 必须是 file 或 run：{kind:?}", evidence.id),
                Some(&evidence.id),
            );
            continue;
        }
        match checked_file(
            root,
            &evidence.path,
            evidence.sha256.as_deref(),
            evidence.locator.as_deref(),
            "evidence",
        ) {
            Ok(norm) => {
                evidence_ok.insert(evidence.id.clone());
                if doc.schema == SCHEMA_V3 && kind == "run" {
                    let command = evidence.command.as_deref().unwrap_or("");
                    if command.trim().is_empty() {
                        report.fail(
                            "evidence-command",
                            format!("{} 是 run 证据，但没有声明 command", evidence.id),
                            Some(&evidence.id),
                        );
                        continue;
                    }
                    if !norm_command(command).contains(&norm.to_lowercase()) {
                        report.fail(
                            "evidence-command-path",
                            format!("{} 的 command 没有点名证据文件 {norm}", evidence.id),
                            Some(&evidence.id),
                        );
                        continue;
                    }
                    evidence_prov.insert(evidence.id.clone());
                    if let Some(executed) = &executed_norm {
                        if executed.contains(&norm_command(command)) {
                            evidence_verified.insert(evidence.id.clone());
                        } else {
                            report.fail(
                                "evidence-not-executed",
                                format!(
                                    "{} 声明的 command 没有出现在本次成功 run 记录里：{command:?}",
                                    evidence.id
                                ),
                                Some(&evidence.id),
                            );
                        }
                    }
                }
            }
            Err((code, message)) => report.fail(&code, message, Some(&evidence.id)),
        }
    }

    // 声明范围与 non-goal 重叠。
    for raw in &doc.allow_unlinked {
        if let Err(error) = safe_rel(root, raw) {
            report.fail("allow-unlinked-invalid", error, Some("allow_unlinked"));
        }
        for ng in &doc.non_goals {
            if scope_overlap(raw, &ng.paths) {
                report.fail(
                    "non-goal-scope",
                    format!("allow_unlinked {raw:?} 与 non_goal {} 重叠", ng.id),
                    Some("allow_unlinked"),
                );
            }
        }
    }

    let mut total_full = 0usize;
    let mut must_total = 0usize;
    let mut must_full = 0usize;
    for req in &doc.requirements {
        if req.text.trim().is_empty() {
            report.fail(
                "requirement-text",
                format!("{} 的 text 不能为空", req.id),
                Some(&req.id),
            );
        }
        let priority = match req.priority.as_str() {
            "must" | "should" | "may" => req.priority.as_str(),
            other => {
                report.fail(
                    "priority-invalid",
                    format!("{} 的 priority 必须是 must/should/may：{other:?}", req.id),
                    Some(&req.id),
                );
                "must"
            }
        };
        if priority == "must" {
            must_total += 1;
        }
        for change in &req.changes {
            if let Err(error) = safe_rel(root, change) {
                report.fail("changes-invalid", error, Some(&req.id));
            }
            for ng in &doc.non_goals {
                if scope_overlap(change, &ng.paths) {
                    report.fail(
                        "non-goal-scope",
                        format!(
                            "{} 的 declared change 与 non_goal {} 重叠：{change}",
                            req.id, ng.id
                        ),
                        Some(&req.id),
                    );
                }
            }
        }
        let mut has_art = false;
        for reference in &req.artifacts {
            if !artifact_map.contains_key(reference) {
                report.fail(
                    "missing-artifact-ref",
                    format!("{} 引用了不存在的 artifact：{reference}", req.id),
                    Some(&req.id),
                );
            } else if ng_art_refs.contains(reference) {
                report.fail(
                    "non-goal-artifact",
                    format!(
                        "{} 引用了 non_goal 明确排除的 artifact：{reference}",
                        req.id
                    ),
                    Some(&req.id),
                );
            } else if artifact_ok.contains(reference) {
                has_art = true;
            }
        }
        let mut has_ev = false;
        for reference in &req.evidence {
            if !evidence_map.contains_key(reference) {
                report.fail(
                    "missing-evidence-ref",
                    format!("{} 引用了不存在的 evidence：{reference}", req.id),
                    Some(&req.id),
                );
            } else if ng_ev_refs.contains(reference) {
                report.fail(
                    "non-goal-evidence",
                    format!(
                        "{} 引用了 non_goal 明确排除的 evidence：{reference}",
                        req.id
                    ),
                    Some(&req.id),
                );
            } else if evidence_ok.contains(reference) {
                has_ev = true;
            }
        }
        let has_prov =
            doc.schema == SCHEMA_V3 && req.evidence.iter().any(|e| evidence_prov.contains(e));
        let has_verified =
            doc.schema == SCHEMA_V3 && req.evidence.iter().any(|e| evidence_verified.contains(e));
        if has_art {
            report.coverage.requirements_with_artifact += 1;
        }
        if has_ev {
            report.coverage.requirements_with_evidence += 1;
        }
        if has_prov {
            report.coverage.requirements_with_provenance += 1;
        }
        if has_verified {
            report.coverage.requirements_provenance_verified += 1;
        }
        if has_art && has_ev {
            total_full += 1;
        }
        match priority {
            "must" => {
                if has_art && has_ev {
                    must_full += 1;
                }
                if !has_art {
                    report.fail(
                        "missing-artifact",
                        format!("{} 是 must，但没有有效产物", req.id),
                        Some(&req.id),
                    );
                }
                if !has_ev {
                    report.fail(
                        "missing-evidence",
                        format!("{} 是 must，但没有有效证据", req.id),
                        Some(&req.id),
                    );
                }
                if doc.schema == SCHEMA_V3 && !has_prov {
                    report.fail(
                        "missing-provenance",
                        format!("{} 是 must，但没有绑定 run command 的证据", req.id),
                        Some(&req.id),
                    );
                }
            }
            "should" => {
                if !has_art {
                    if options.strict {
                        report.fail(
                            "missing-artifact",
                            format!("{} 是 should（strict），但没有有效产物", req.id),
                            Some(&req.id),
                        );
                    } else {
                        report.warn(
                            "missing-artifact",
                            format!("{} 是 should，但没有有效产物", req.id),
                            Some(&req.id),
                        );
                    }
                }
                if !has_ev {
                    if options.strict {
                        report.fail(
                            "missing-evidence",
                            format!("{} 是 should（strict），但没有有效证据", req.id),
                            Some(&req.id),
                        );
                    } else {
                        report.warn(
                            "missing-evidence",
                            format!("{} 是 should，但没有有效证据", req.id),
                            Some(&req.id),
                        );
                    }
                }
                if doc.schema == SCHEMA_V3 && !has_prov {
                    if options.strict {
                        report.fail(
                            "missing-provenance",
                            format!(
                                "{} 是 should（strict），但没有绑定 run command 的证据",
                                req.id
                            ),
                            Some(&req.id),
                        );
                    } else {
                        report.warn(
                            "missing-provenance",
                            format!("{} 是 should，但没有绑定 run command 的证据", req.id),
                            Some(&req.id),
                        );
                    }
                }
            }
            _ => {
                if !has_art {
                    report.warn(
                        "optional-no-artifact",
                        format!("{} 是 may，没有产物（仅警告）", req.id),
                        Some(&req.id),
                    );
                }
                if !has_ev {
                    report.warn(
                        "optional-no-evidence",
                        format!("{} 是 may，没有证据（仅警告）", req.id),
                        Some(&req.id),
                    );
                }
                if doc.schema == SCHEMA_V3 && !has_prov {
                    report.warn(
                        "optional-no-provenance",
                        format!("{} 是 may，没有绑定 run command 的证据（仅警告）", req.id),
                        Some(&req.id),
                    );
                }
            }
        }
    }

    validate_contradictions(&req_map, report);

    // 声明变更必须实际发生；单独 `changed` 只是覆盖子集，`changed_known` 才是完整集。
    let mut changed_norm = Vec::new();
    let coverage_known = options.changed.is_some() || options.changed_known;
    if coverage_known {
        for raw in options.changed.clone().unwrap_or_default() {
            match safe_rel(root, &raw) {
                Ok((norm, _)) => changed_norm.push(norm),
                Err(error) => report.fail("bad-change", error, None),
            }
        }
        report.counts.changed = changed_norm.len();
        report.counts.changed_known = usize::from(options.changed_known);
        let mut linked = BTreeSet::new();
        for path in &changed_norm {
            if let Some(ng) = doc.non_goals.iter().find(|ng| glob_match(path, &ng.paths)) {
                report.fail(
                    "non-goal-change",
                    format!("变更落进 non_goal {}：{path}", ng.id),
                    Some(&ng.id),
                );
                continue;
            }
            if glob_match(path, &doc.allow_unlinked) {
                linked.insert(path.clone());
                continue;
            }
            let hit_req = doc
                .requirements
                .iter()
                .any(|req| glob_match(path, &req.changes));
            let hit_art = doc.artifacts.iter().any(|artifact| {
                safe_rel(root, &artifact.path)
                    .map(|(norm, _)| path_eq(&norm, path))
                    .unwrap_or(false)
            });
            if hit_req || hit_art {
                linked.insert(path.clone());
            } else {
                report.fail(
                    "unlinked-change",
                    format!("变更没有对齐到任何 requirement：{path}"),
                    None,
                );
            }
        }
        report.counts.linked_changes = linked.len();
        if options.changed_known {
            for req in &doc.requirements {
                if req.changes.is_empty() {
                    continue;
                }
                report.coverage.requirements_with_changes += 1;
                let missing: Vec<&String> = req
                    .changes
                    .iter()
                    .filter(|pattern| {
                        !changed_norm
                            .iter()
                            .any(|path| glob_match(path, &[(*pattern).clone()]))
                    })
                    .collect();
                if missing.is_empty() {
                    report.coverage.requirements_changes_realized += 1;
                    continue;
                }
                let message = format!(
                    "{} 声明的变更在本次实际变更中不存在：{}",
                    req.id,
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if req.priority == "must" || (req.priority == "should" && options.strict) {
                    report.fail("declared-change-missing", message, Some(&req.id));
                } else {
                    report.warn("declared-change-missing", message, Some(&req.id));
                }
            }
        }
    }

    report.counts.requirements = req_map.len();
    report.counts.artifacts = artifact_ok.len();
    report.counts.evidence = evidence_ok.len();
    report.counts.provenance = evidence_prov.len();
    report.counts.provenance_verified = evidence_verified.len();
    report.counts.must = must_total;
    report.coverage.requirements_full = total_full;
    report.coverage.must_full = must_full;
    report.coverage.goals_covered = covered_goal_count(&goal_map, &req_map);
    report.score = if req_map.is_empty() {
        0.0
    } else {
        round3(total_full as f64 / req_map.len() as f64)
    };
    report.must_score = if must_total == 0 {
        0.0
    } else {
        round3(must_full as f64 / must_total as f64)
    };
    let _ = root_text;
}

fn read_goals(doc: &Manifest, report: &mut AlignmentReport) -> Vec<Goal> {
    match doc.schema.as_str() {
        SCHEMA_V1 => {
            if !doc.goals.is_empty() {
                report.fail(
                    "goals-field",
                    "alignment/1 使用 goal 单对象；要子目标请改用 alignment/2 的 goals 数组",
                    None,
                );
            }
            let Some(goal) = &doc.goal else {
                report.fail("goal-missing", "必须有一个 goal 对象", None);
                return Vec::new();
            };
            if goal.parent.is_some() {
                report.fail(
                    "unknown-field",
                    "alignment/1 的 goal 不能带 parent",
                    Some(&goal.id),
                );
            }
            let mut goal = goal.clone();
            if goal.id.trim().is_empty() {
                report.fail("goal-invalid", "goal.id 不能为空", None);
                return Vec::new();
            }
            goal.id = goal.id.trim().to_owned();
            if goal.text.trim().is_empty() {
                report.fail("goal-invalid", "goal.text 不能为空", Some(&goal.id));
            }
            vec![goal]
        }
        SCHEMA_V2 | SCHEMA_V3 => {
            if doc.goal.is_some() {
                report.fail(
                    "goal-field",
                    format!(
                        "{} 使用 goals 数组；单目标也写成 goals:[{{...}}]",
                        doc.schema
                    ),
                    None,
                );
            }
            if doc.goals.is_empty() {
                report.fail(
                    "goals-missing",
                    format!("{} 至少需要一个 goal", doc.schema),
                    None,
                );
                return Vec::new();
            }
            let mut seen = BTreeSet::new();
            let mut out = Vec::new();
            for goal in &doc.goals {
                let id = goal.id.trim().to_owned();
                if id.is_empty() {
                    report.fail("goal-invalid", "goal 缺少 id", None);
                    continue;
                }
                if !seen.insert(id.clone()) {
                    report.fail("duplicate-id", format!("goal id 重复：{id}"), Some(&id));
                    continue;
                }
                if goal.text.trim().is_empty() {
                    report.fail("goal-invalid", format!("{id} 的 text 不能为空"), Some(&id));
                }
                let mut goal = goal.clone();
                goal.id = id;
                out.push(goal);
            }
            out
        }
        _ => Vec::new(),
    }
}

fn unique_root<'a>(
    goal_map: &'a BTreeMap<String, &'a Goal>,
    report: &mut AlignmentReport,
) -> Option<String> {
    if goal_map.is_empty() {
        return None;
    }
    // 先报更具体的父引用/成环错误，再判“必须有且只有一个根”。
    // 否则一个成环的目标树会先被误报成 goal-root。
    for goal in goal_map.values() {
        if let Some(parent) = goal
            .parent
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            && !goal_map.contains_key(parent)
        {
            report.fail(
                "unknown-goal-parent",
                format!("{} 的 parent 不存在：{parent}", goal.id),
                Some(&goal.id),
            );
            return None;
        }
    }
    for goal in goal_map.values() {
        let mut seen = BTreeSet::new();
        let mut current = goal.id.clone();
        loop {
            if !seen.insert(current.clone()) {
                report.fail(
                    "goal-cycle",
                    format!("goal parent 链成环：{}", goal.id),
                    Some(&goal.id),
                );
                return None;
            }
            let Some(parent) = goal_map
                .get(&current)
                .and_then(|item| item.parent.as_deref())
                .map(str::trim)
                .filter(|p| !p.is_empty())
            else {
                break;
            };
            current = parent.to_owned();
        }
    }
    let roots: Vec<String> = goal_map
        .values()
        .filter(|goal| {
            goal.parent
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .is_none()
        })
        .map(|goal| goal.id.clone())
        .collect();
    if roots.len() != 1 {
        report.fail(
            "goal-root",
            format!(
                "必须有且只有一个根 goal（parent=null），实际 {} 个：{:?}",
                roots.len(),
                roots
            ),
            None,
        );
        return None;
    }
    let root = roots.into_iter().next()?;
    for goal in goal_map.values() {
        let mut seen = BTreeSet::new();
        let mut current = goal.id.clone();
        loop {
            if !seen.insert(current.clone()) {
                report.fail(
                    "goal-cycle",
                    format!("goal parent 链成环：{}", goal.id),
                    Some(&goal.id),
                );
                break;
            }
            if current == root {
                break;
            }
            let Some(parent) = goal_map
                .get(&current)
                .and_then(|item| item.parent.as_deref())
                .map(str::trim)
                .filter(|p| !p.is_empty())
            else {
                report.fail(
                    "goal-parent",
                    format!("{} 的父链没有落到根 {}", goal.id, root),
                    Some(&goal.id),
                );
                break;
            };
            if !goal_map.contains_key(parent) {
                report.fail(
                    "unknown-goal-parent",
                    format!("{} 的 parent 不存在：{parent}", goal.id),
                    Some(&goal.id),
                );
                break;
            }
            current = parent.to_owned();
        }
    }
    Some(root)
}

fn validate_requirement_parents(
    goal_map: &BTreeMap<String, &Goal>,
    req_map: &BTreeMap<String, &Requirement>,
    report: &mut AlignmentReport,
) {
    let valid: BTreeSet<&str> = goal_map
        .keys()
        .map(String::as_str)
        .chain(req_map.keys().map(String::as_str))
        .collect();
    let mut parent_of: HashMap<String, String> = HashMap::new();
    for req in req_map.values() {
        let Some(parent) = req
            .parent
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            report.fail(
                "missing-parent",
                format!("{} 没有 parent（需求必须显式挂到目标或上层需求）", req.id),
                Some(&req.id),
            );
            continue;
        };
        parent_of.insert(req.id.clone(), parent.to_owned());
        if !valid.contains(parent) {
            report.fail(
                "unknown-parent",
                format!("{} 的 parent 不存在：{parent}", req.id),
                Some(&req.id),
            );
        }
    }
    for rid in req_map.keys() {
        let mut seen = BTreeSet::new();
        let mut current = rid.clone();
        loop {
            if !seen.insert(current.clone()) {
                report.fail("parent-cycle", format!("parent 链成环：{}", rid), Some(rid));
                break;
            }
            let Some(parent) = parent_of.get(&current) else {
                report.fail(
                    "goal-not-reached",
                    format!("{rid} 的 parent 链没有终止在任何 goal（实际 <empty>）"),
                    Some(rid),
                );
                break;
            };
            if !parent_of.contains_key(parent) {
                if goal_map.contains_key(parent) {
                    break;
                }
                report.fail(
                    "goal-not-reached",
                    format!("{rid} 的 parent 链没有终止在任何 goal（实际 {parent}）"),
                    Some(rid),
                );
                break;
            }
            current = parent.clone();
        }
    }
}

fn validate_goal_coverage(
    goal_map: &BTreeMap<String, &Goal>,
    req_map: &BTreeMap<String, &Requirement>,
    report: &mut AlignmentReport,
) {
    let mut covered = BTreeSet::new();
    for rid in req_map.keys() {
        let mut cur = rid.clone();
        let mut seen = BTreeSet::new();
        while req_map.contains_key(&cur) && seen.insert(cur.clone()) {
            cur = req_map
                .get(&cur)
                .and_then(|req| req.parent.as_deref())
                .unwrap_or("")
                .trim()
                .to_owned();
        }
        let mut goal_cur = cur;
        let mut goal_seen = BTreeSet::new();
        while goal_map.contains_key(&goal_cur) && goal_seen.insert(goal_cur.clone()) {
            covered.insert(goal_cur.clone());
            goal_cur = goal_map
                .get(&goal_cur)
                .and_then(|goal| goal.parent.as_deref())
                .unwrap_or("")
                .trim()
                .to_owned();
        }
    }
    for gid in goal_map.keys() {
        if !covered.contains(gid) {
            report.fail(
                "goal-uncovered",
                format!("{gid} 没有任何 requirement 的父链覆盖它"),
                Some(gid),
            );
        }
    }
    report.coverage.goals_covered = covered.len();
}

fn covered_goal_count(
    goal_map: &BTreeMap<String, &Goal>,
    req_map: &BTreeMap<String, &Requirement>,
) -> usize {
    let mut covered = BTreeSet::new();
    for rid in req_map.keys() {
        let mut cur = rid.clone();
        let mut seen = BTreeSet::new();
        while req_map.contains_key(&cur) && seen.insert(cur.clone()) {
            cur = req_map
                .get(&cur)
                .and_then(|req| req.parent.as_deref())
                .unwrap_or("")
                .trim()
                .to_owned();
        }
        let mut goal_cur = cur;
        let mut goal_seen = BTreeSet::new();
        while goal_map.contains_key(&goal_cur) && goal_seen.insert(goal_cur.clone()) {
            covered.insert(goal_cur.clone());
            goal_cur = goal_map
                .get(&goal_cur)
                .and_then(|goal| goal.parent.as_deref())
                .unwrap_or("")
                .trim()
                .to_owned();
        }
    }
    covered.len()
}

fn validate_contradictions(req_map: &BTreeMap<String, &Requirement>, report: &mut AlignmentReport) {
    let mut pairs = BTreeSet::new();
    for req in req_map.values() {
        for other in &req.contradicts {
            if req_map.get(other).is_none() {
                report.fail(
                    "unknown-contradiction",
                    format!("{} 声明矛盾的对象不存在：{other}", req.id),
                    Some(&req.id),
                );
                continue;
            }
            if other == &req.id {
                report.fail(
                    "self-contradiction",
                    format!("{} 声明与自己矛盾", req.id),
                    Some(&req.id),
                );
                continue;
            }
            let pair = if req.id < *other {
                (req.id.clone(), other.clone())
            } else {
                (other.clone(), req.id.clone())
            };
            if pairs.insert(pair.clone()) {
                report.fail(
                    "contradiction",
                    format!("需求互相矛盾：{} <-> {}", pair.0, pair.1),
                    Some(&format!("{}|{}", pair.0, pair.1)),
                );
            }
        }
    }
}

fn collect_ids<T>(
    items: &[T],
    kind: &'static str,
    ids: &mut BTreeMap<String, &'static str>,
    report: &mut AlignmentReport,
) where
    T: HasId,
{
    for item in items {
        let id = item.id().trim().to_owned();
        if id.is_empty() {
            report.fail(&format!("{kind}-id"), format!("{kind} 缺少 id"), None);
            continue;
        }
        if ids.insert(id.clone(), kind).is_some() {
            report.fail("duplicate-id", format!("id 重复：{id}"), Some(&id));
        }
    }
}

trait HasId {
    fn id(&self) -> &str;
}

impl HasId for Requirement {
    fn id(&self) -> &str {
        &self.id
    }
}
impl HasId for Artifact {
    fn id(&self) -> &str {
        &self.id
    }
}
impl HasId for Evidence {
    fn id(&self) -> &str {
        &self.id
    }
}
impl HasId for NonGoal {
    fn id(&self) -> &str {
        &self.id
    }
}

fn checked_file(
    root: &Path,
    rel: &str,
    sha256: Option<&str>,
    locator: Option<&str>,
    what: &str,
) -> Result<String, (String, String)> {
    let (norm, full) = safe_rel(root, rel).map_err(|e| ("bad-path".to_owned(), e))?;
    let canonical = fs::canonicalize(&full).map_err(|_| {
        (
            format!("{what}-missing"),
            format!("{what} 文件不存在：{norm}"),
        )
    })?;
    if !canonical.is_file() {
        return Err((
            format!("{what}-missing"),
            format!("{what} 不是普通文件：{norm}"),
        ));
    }
    let root_canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if !canonical.starts_with(&root_canon) {
        return Err((
            "bad-path".to_owned(),
            format!("{what} 经符号链接逃出 root：{norm}"),
        ));
    }
    if let Some(sha) = sha256 {
        let sha = sha.trim();
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err((
                "sha-invalid".to_owned(),
                format!("sha256 不是 64 位十六进制：{sha:?}"),
            ));
        }
        let bytes = fs::read(&canonical)
            .map_err(|e| ("hash-unreadable".to_owned(), format!("文件读不出：{e}")))?;
        if digest::hex(&digest::sha256(&bytes)) != sha.to_ascii_lowercase() {
            return Err((
                "hash-mismatch".to_owned(),
                "sha256 与磁盘内容不符".to_owned(),
            ));
        }
    }
    if let Some(locator) = locator {
        check_locator(&canonical, locator)?;
    }
    Ok(norm)
}

fn check_locator(full: &Path, locator: &str) -> Result<(), (String, String)> {
    let token = locator.trim();
    if token.is_empty() {
        return Err((
            "locator-invalid".to_owned(),
            "locator 必须是非空字符串".to_owned(),
        ));
    }
    if let Some(rest) = token.strip_prefix('L') {
        let mut parts = rest.split('-');
        let start = parts
            .next()
            .and_then(|s| s.trim_start_matches('L').parse::<usize>().ok());
        let end = parts
            .next()
            .and_then(|s| s.trim_start_matches('L').parse::<usize>().ok());
        let Some(start) = start else {
            return Err((
                "locator-invalid".to_owned(),
                format!("locator 行号非法：{token}"),
            ));
        };
        let end = end.unwrap_or(start);
        if start < 1 || end < start {
            return Err((
                "locator-invalid".to_owned(),
                format!("locator 行号非法：{token}"),
            ));
        }
        let text = fs::read_to_string(full)
            .map_err(|e| ("locator-unreadable".to_owned(), format!("文件读不出：{e}")))?;
        let lines = text.lines().count();
        if end > lines {
            return Err((
                "locator-out-of-range".to_owned(),
                format!("locator {token} 超出文件行数 {lines}"),
            ));
        }
        return Ok(());
    }
    if let Some(needle) = token.strip_prefix("text:") {
        if needle.is_empty() {
            return Err((
                "locator-invalid".to_owned(),
                "text: locator 缺少内容".to_owned(),
            ));
        }
        let text = fs::read_to_string(full)
            .map_err(|e| ("locator-unreadable".to_owned(), format!("文件读不出：{e}")))?;
        if !text.contains(needle) {
            return Err((
                "locator-missing".to_owned(),
                format!("locator 文本未出现：{needle:?}"),
            ));
        }
        return Ok(());
    }
    Err((
        "locator-invalid".to_owned(),
        format!("locator 只支持 Lx、Lx-Ly 或 text:<片段>：{token:?}"),
    ))
}

fn safe_rel(root: &Path, raw: &str) -> Result<(String, PathBuf), String> {
    let raw = raw.trim().replace('\\', "/");
    if raw.is_empty() || raw.len() > MAX_PATH {
        return Err("路径为空或过长".to_owned());
    }
    if raw.starts_with('/')
        || raw.starts_with("//")
        || (raw.len() >= 2 && raw.as_bytes()[0].is_ascii_alphabetic() && raw.as_bytes()[1] == b':')
    {
        return Err(format!("路径必须是仓库相对路径：{raw:?}"));
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(format!("路径逃出 root：{raw:?}"));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(format!("路径逃出 root：{raw:?}"));
    }
    let norm = parts.join("/");
    Ok((norm.clone(), root.join(norm)))
}

fn glob_match(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        GlobBuilder::new(&pattern.replace('\\', "/"))
            .literal_separator(false)
            .case_insensitive(cfg!(windows))
            .build()
            .map(|glob| glob.compile_matcher().is_match(path))
            .unwrap_or(false)
    })
}

/// 路径字面比较：Windows 文件系统不区分大小写，Linux 区分。
fn path_eq(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn has_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

fn scope_overlap(pattern: &str, ng_paths: &[String]) -> bool {
    let pattern = pattern.replace('\\', "/");
    if has_glob(&pattern) {
        return ng_paths.iter().any(|p| {
            let p = p.replace('\\', "/");
            path_eq(&pattern, &p)
                || (!has_glob(&p) && glob_match(&p, std::slice::from_ref(&pattern)))
        });
    }
    ng_paths
        .iter()
        .any(|p| glob_match(&pattern, std::slice::from_ref(p)))
}

fn norm_command(value: &str) -> String {
    value
        .replace('\\', "/")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trit-alignment-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn manifest(schema: &str, goal: &str, goals: &str, reqs: &str, extra: &str) -> String {
        format!(r#"{{"schema":"{schema}"{goal}{goals},"requirements":[{reqs}]{extra}}}"#)
    }

    #[test]
    fn v1_simple_is_aligned() {
        let root = root("v1");
        write(&root, "src/a.py", "print(1)\n");
        write(&root, "tests/a.py", "def test_a(): pass\n");
        let path = root.join("alignment.json");
        let doc = manifest(
            SCHEMA_V1,
            r#","goal":{"id":"G1","text":"ship it"}"#,
            "",
            r#"{"id":"R1","text":"a","priority":"must","parent":"G1","artifacts":["A1"],"evidence":["E1"],"changes":["src/a.py"]}"#,
            r#","artifacts":[{"id":"A1","path":"src/a.py"}],"evidence":[{"id":"E1","path":"tests/a.py","locator":"text:test_a"}]"#,
        );
        fs::write(&path, doc).unwrap();
        let report = check(&root, &path, &AlignmentOptions::default());
        assert_eq!(report.status, "aligned", "{report:?}");
    }

    #[test]
    fn v2_uncovered_goal_and_bad_parent_are_unaligned() {
        let root = root("v2");
        write(&root, "src/a.py", "x\n");
        write(&root, "tests/a.py", "test_a\n");
        let path = root.join("alignment.json");
        let doc = manifest(
            SCHEMA_V2,
            "",
            r#","goals":[{"id":"G1","text":"root"},{"id":"G2","text":"child","parent":"G1"},{"id":"G3","text":"orphan"}]"#,
            r#"{"id":"R1","text":"a","parent":"G1","artifacts":["A1"],"evidence":["E1"]}"#,
            r#","artifacts":[{"id":"A1","path":"src/a.py"}],"evidence":[{"id":"E1","path":"tests/a.py"}]"#,
        );
        fs::write(&path, doc).unwrap();
        let report = check(&root, &path, &AlignmentOptions::default());
        assert_eq!(report.status, "unaligned");
        assert!(report.failures.iter().any(|f| f.code == "goal-root"));
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.code == "goal-uncovered" || f.code == "goal-parent")
        );
    }

    #[test]
    fn v3_run_evidence_must_match_executed_command() {
        let root = root("v3");
        write(&root, "src/a.py", "x\n");
        write(&root, "tests/a.py", "test_a\n");
        let path = root.join("alignment.json");
        let doc = manifest(
            SCHEMA_V3,
            "",
            r#","goals":[{"id":"G1","text":"root"}]"#,
            r#"{"id":"R1","text":"a","parent":"G1","artifacts":["A1"],"evidence":["E1"]}"#,
            r#","artifacts":[{"id":"A1","path":"src/a.py"}],"evidence":[{"id":"E1","path":"tests/a.py","kind":"run","command":"python tests/a.py"}]"#,
        );
        fs::write(&path, doc).unwrap();
        let options = AlignmentOptions {
            executed: Some(vec!["python tests/a.py".to_owned()]),
            ..AlignmentOptions::default()
        };
        let report = check(&root, &path, &options);
        assert_eq!(report.status, "aligned", "{report:?}");
        assert_eq!(report.counts.provenance_verified, 1);

        let report = check(&root, &path, &AlignmentOptions::default());
        assert_eq!(report.status, "aligned");
        assert_eq!(report.counts.provenance_verified, 0);
    }

    #[test]
    fn changed_known_rejects_unlinked_and_missing_declared_change() {
        let root = root("changed");
        write(&root, "src/a.py", "x\n");
        write(&root, "tests/a.py", "test_a\n");
        let path = root.join("alignment.json");
        let doc = manifest(
            SCHEMA_V1,
            r#","goal":{"id":"G1","text":"root"}"#,
            "",
            r#"{"id":"R1","text":"a","parent":"G1","artifacts":["A1"],"evidence":["E1"],"changes":["src/a.py"]}"#,
            r#","artifacts":[{"id":"A1","path":"src/a.py"}],"evidence":[{"id":"E1","path":"tests/a.py"}]"#,
        );
        fs::write(&path, doc).unwrap();
        let options = AlignmentOptions {
            changed: Some(vec!["other.py".to_owned()]),
            changed_known: true,
            ..AlignmentOptions::default()
        };
        let report = check(&root, &path, &options);
        assert_eq!(report.status, "unaligned");
        assert!(report.failures.iter().any(|f| f.code == "unlinked-change"));
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.code == "declared-change-missing")
        );
    }

    #[test]
    fn non_goal_path_rejects_artifact() {
        let root = root("non-goal");
        write(&root, "src/a.py", "x\n");
        write(&root, "tests/a.py", "test_a\n");
        let path = root.join("alignment.json");
        let doc = manifest(
            SCHEMA_V1,
            r#","goal":{"id":"G1","text":"root"}"#,
            "",
            r#"{"id":"R1","text":"a","parent":"G1","artifacts":["A1"],"evidence":["E1"]}"#,
            r#","artifacts":[{"id":"A1","path":"src/a.py"}],"evidence":[{"id":"E1","path":"tests/a.py"}],"non_goals":[{"id":"N1","text":"not src","paths":["src/**"]}]"#,
        );
        fs::write(&path, doc).unwrap();
        let report = check(&root, &path, &AlignmentOptions::default());
        assert_eq!(report.status, "unaligned");
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.code == "non-goal-artifact-path")
        );
    }

    #[test]
    fn sha_mismatch_and_locator_range_fail() {
        let root = root("sha");
        write(&root, "src/a.py", "x\n");
        write(&root, "tests/a.py", "test_a\n");
        let path = root.join("alignment.json");
        let doc = manifest(
            SCHEMA_V1,
            r#","goal":{"id":"G1","text":"root"}"#,
            "",
            r#"{"id":"R1","text":"a","parent":"G1","artifacts":["A1"],"evidence":["E1"]}"#,
            &format!(
                r#","artifacts":[{{"id":"A1","path":"src/a.py","sha256":"{}"}}],"evidence":[{{"id":"E1","path":"tests/a.py","locator":"L1-L99"}}]"#,
                "0".repeat(64)
            ),
        );
        fs::write(&path, doc).unwrap();
        let report = check(&root, &path, &AlignmentOptions::default());
        assert_eq!(report.status, "unaligned");
        assert!(report.failures.iter().any(|f| f.code == "hash-mismatch"));
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.code == "locator-out-of-range")
        );
    }

    #[test]
    fn v3_run_evidence_not_in_executed_set_fails() {
        let root = root("not-executed");
        write(&root, "src/a.py", "x\n");
        write(&root, "tests/a.py", "test_a\n");
        let path = root.join("alignment.json");
        let doc = manifest(
            SCHEMA_V3,
            "",
            r#","goals":[{"id":"G1","text":"root"}]"#,
            r#"{"id":"R1","text":"a","parent":"G1","artifacts":["A1"],"evidence":["E1"]}"#,
            r#","artifacts":[{"id":"A1","path":"src/a.py"}],"evidence":[{"id":"E1","path":"tests/a.py","kind":"run","command":"python tests/a.py"}]"#,
        );
        fs::write(&path, doc).unwrap();
        let options = AlignmentOptions {
            executed: Some(vec!["cargo test other".to_owned()]),
            ..AlignmentOptions::default()
        };
        let report = check(&root, &path, &options);
        assert_eq!(report.status, "unaligned");
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.code == "evidence-not-executed")
        );
    }
}
