//! 分层上下文组装：项目/会话/公共 scope、模型能力、推理强度和引用句柄。
//!
//! 这是“编排分配上下文”的第一层：宿主先给出候选 ref 与语义分数，组装器只负责
//! 权限/scope 校验、模型能力降级、预算选择，以及句柄/摘要/全文的取舍。
//! 它不调用模型，也不把语义判断硬编码成词表。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextScope {
    /// 只属于当前会话。
    Session,
    /// 当前项目共享，但默认不自动注入。
    Project,
    /// 公共/跨项目候选；必须显式 opt-in。
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Doc,
    Memory,
    Artifact,
    Receipt,
    Resource,
    Reasoning,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    #[default]
    Public,
    Internal,
    Secret,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRef {
    /// 稳定 ID；模型可见的短句柄在组装时生成。
    pub id: String,
    pub label: String,
    pub kind: ContextKind,
    pub scope: ContextScope,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub revision: String,
    /// 可选：内容寻址用于去重/缓存；不是身份，也不默认暴露给模型。
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub sensitivity: Sensitivity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextDocument {
    pub id: String,
    pub full: String,
    #[serde(default)]
    pub summary: Option<String>,
    /// 已知全文 token 数时为 Some；否则组装器按字符估算。
    #[serde(default)]
    pub tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedContext {
    pub reference: ContextRef,
    pub document: ContextDocument,
}

pub trait ContextResolver {
    fn resolve(&self, id: &str) -> Result<Option<ResolvedContext>, ContextError>;
}

/// 进程内实现；宿主可换成数据库、对象存储或 Git 内容寻址存储。
#[derive(Debug, Clone, Default)]
pub struct InMemoryContext {
    entries: BTreeMap<String, ResolvedContext>,
}

impl InMemoryContext {
    pub fn insert(
        &mut self,
        reference: ContextRef,
        document: ContextDocument,
    ) -> Result<(), ContextError> {
        if reference.id != document.id {
            return Err(ContextError::DocumentMismatch(document.id));
        }
        self.entries.insert(
            reference.id.clone(),
            ResolvedContext {
                reference,
                document,
            },
        );
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Option<ResolvedContext> {
        self.entries.remove(id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl ContextResolver for InMemoryContext {
    fn resolve(&self, id: &str) -> Result<Option<ResolvedContext>, ContextError> {
        Ok(self.entries.get(id).cloned())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProfile {
    #[serde(default)]
    pub name: String,
    pub context_window: u64,
    #[serde(default)]
    pub max_output_tokens: u64,
    #[serde(default)]
    pub supports_tools: bool,
    /// 是否有 `context_read` 之类的句柄展开工具；只有 tools 不够。
    #[serde(default)]
    pub supports_context_read: bool,
    #[serde(default)]
    pub supports_reasoning: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSelection {
    pub id: String,
    #[serde(default)]
    pub required: bool,
    /// 外部语义/宿主排序分数；越高越优先。组装器不自己算语义。
    #[serde(default)]
    pub score: Option<u16>,
    /// `Shared` scope 必须显式 opt-in，避免“看起来相关”自动注入。
    #[serde(default)]
    pub shared_opt_in: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRequest {
    pub project: String,
    pub session: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub selections: Vec<ContextSelection>,
    /// 0 表示使用模型窗口扣掉输出/推理预留后的可用预算。
    #[serde(default)]
    pub budget_tokens: u64,
    pub model: ModelProfile,
    #[serde(default)]
    pub reasoning: ReasoningEffort,
    /// 默认不允许 Secret；显式开启时仍由宿主承担合规责任。
    #[serde(default)]
    pub allow_secret: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextDetail {
    Handle,
    Summary,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    /// 模型可见句柄，如 `C1`。
    pub handle: String,
    pub id: String,
    pub label: String,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub revision: String,
    pub required: bool,
    pub detail: ContextDetail,
    /// `Handle` 为 None；`Summary`/`Full` 带实际文本。
    #[serde(default)]
    pub text: Option<String>,
    /// 句柄 + 文本的估算 token。
    pub tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextDropReason {
    UnknownRef,
    OutOfScope,
    Secret,
    DocumentMismatch,
    NoBudget,
    NoUsableForm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedContext {
    pub id: String,
    pub reason: ContextDropReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextDegradation {
    ReasoningUnsupported,
    ToolsUnsupported,
    ContextReadUnsupported,
    BudgetReduced { requested: u64, available: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPlan {
    pub project: String,
    pub session: String,
    pub model: String,
    pub reasoning: ReasoningEffort,
    pub items: Vec<ContextItem>,
    pub dropped: Vec<DroppedContext>,
    pub degradations: Vec<ContextDegradation>,
    pub budget_tokens: u64,
    pub estimated_tokens: u64,
}

impl ContextPlan {
    pub fn handles(&self) -> impl Iterator<Item = &ContextItem> {
        self.items
            .iter()
            .filter(|item| item.detail == ContextDetail::Handle)
    }

    pub fn inline(&self) -> impl Iterator<Item = &ContextItem> {
        self.items
            .iter()
            .filter(|item| item.detail != ContextDetail::Handle)
    }

    /// 给模型看的句柄清单；不含正文。
    pub fn render_manifest(&self) -> String {
        let mut out = String::from("# Context\n");
        if self.items.is_empty() {
            out.push_str("- (none)\n");
            return out;
        }
        for item in &self.items {
            let _ = writeln!(
                out,
                "- {} [{}] {} (rev {})",
                item.handle,
                scope_name(item.scope),
                item.label,
                if item.revision.is_empty() {
                    "-"
                } else {
                    &item.revision
                }
            );
        }
        out
    }

    /// 句柄清单 + 被选择内联的摘要/全文；宿主可直接嵌入 prompt。
    pub fn render_for_model(&self) -> String {
        let mut out = self.render_manifest();
        for item in self.inline() {
            let _ = writeln!(
                out,
                "\n## {} [{}] {}",
                item.handle,
                scope_name(item.scope),
                item.label
            );
            if let Some(text) = &item.text {
                out.push_str(text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContextError {
    #[error("model context window must be greater than max_output_tokens")]
    InvalidModelProfile,
    #[error("required context ref {0} is unknown")]
    UnknownRequired(String),
    #[error("required context ref {0} is out of scope")]
    RequiredOutOfScope(String),
    #[error("required context ref {0} is secret")]
    RequiredSecret(String),
    #[error("required context ref {id} does not fit: needs {needed}, available {available}")]
    RequiredOverBudget {
        id: String,
        needed: u64,
        available: u64,
    },
    #[error("context document {0} does not match its ref")]
    DocumentMismatch(String),
    #[error("context resolver: {0}")]
    Resolver(String),
}

/// 组装一次请求的上下文。
///
/// 规则：
/// - `Session` 只能属于当前 session；`Project` 只能属于当前 project；
///   `Shared` 必须 `shared_opt_in`；
/// - `required` 永远要全文，装不下就 fail-closed；
/// - 可选 ref 按分数/scope 排序，优先句柄；模型不支持工具时降级为摘要/全文；
/// - 高推理强度优先摘要而不是句柄；推理 token 预留是估算值，由宿主按 provider 校准。
pub fn assemble_context(
    request: &ContextRequest,
    resolver: &dyn ContextResolver,
) -> Result<ContextPlan, ContextError> {
    let mut degradations = Vec::new();
    if !request.model.supports_tools {
        degradations.push(ContextDegradation::ToolsUnsupported);
    }
    if !request.model.supports_context_read {
        degradations.push(ContextDegradation::ContextReadUnsupported);
    }
    let can_read_handles = request.model.supports_tools && request.model.supports_context_read;
    let reasoning = if request.model.supports_reasoning {
        request.reasoning
    } else {
        if request.reasoning != ReasoningEffort::Off {
            degradations.push(ContextDegradation::ReasoningUnsupported);
        }
        ReasoningEffort::Off
    };
    let window = request.model.context_window;
    let output = request.model.max_output_tokens;
    if window == 0 || output >= window {
        return Err(ContextError::InvalidModelProfile);
    }
    let available = window
        .saturating_sub(output)
        .saturating_sub(reasoning_reserve(reasoning));
    let budget = if request.budget_tokens == 0 {
        available
    } else {
        request.budget_tokens.min(available)
    };
    if request.budget_tokens > available {
        degradations.push(ContextDegradation::BudgetReduced {
            requested: request.budget_tokens,
            available,
        });
    }

    let selections = merge_selections(&request.selections);
    let mut candidates = Vec::new();
    let mut dropped = Vec::new();
    for selection in selections.values() {
        let resolved = match resolver.resolve(&selection.id)? {
            Some(resolved) => resolved,
            None => {
                if selection.required {
                    return Err(ContextError::UnknownRequired(selection.id.clone()));
                }
                dropped.push(DroppedContext {
                    id: selection.id.clone(),
                    reason: ContextDropReason::UnknownRef,
                });
                continue;
            }
        };
        if resolved.reference.id != selection.id || resolved.document.id != selection.id {
            if selection.required {
                return Err(ContextError::DocumentMismatch(selection.id.clone()));
            }
            dropped.push(DroppedContext {
                id: selection.id.clone(),
                reason: ContextDropReason::DocumentMismatch,
            });
            continue;
        }
        if !scope_allowed(&resolved.reference, request, selection) {
            if selection.required {
                return Err(ContextError::RequiredOutOfScope(selection.id.clone()));
            }
            dropped.push(DroppedContext {
                id: selection.id.clone(),
                reason: ContextDropReason::OutOfScope,
            });
            continue;
        }
        if resolved.reference.sensitivity == Sensitivity::Secret && !request.allow_secret {
            if selection.required {
                return Err(ContextError::RequiredSecret(selection.id.clone()));
            }
            dropped.push(DroppedContext {
                id: selection.id.clone(),
                reason: ContextDropReason::Secret,
            });
            continue;
        }
        candidates.push((selection.clone(), resolved));
    }

    candidates.sort_by(|(left_selection, left), (right_selection, right)| {
        right_selection
            .required
            .cmp(&left_selection.required)
            .then_with(|| {
                right_selection
                    .score
                    .unwrap_or(0)
                    .cmp(&left_selection.score.unwrap_or(0))
            })
            .then_with(|| scope_rank(left.reference.scope).cmp(&scope_rank(right.reference.scope)))
            .then_with(|| left.reference.id.cmp(&right.reference.id))
    });

    let mut items = Vec::new();
    let mut used = 0u64;
    let mut handle_seq = 0usize;
    for (selection, resolved) in candidates {
        let preferred = if selection.required {
            ContextDetail::Full
        } else {
            preferred_detail(can_read_handles, reasoning, &resolved.document)
        };
        let chain = detail_chain(
            preferred,
            resolved.document.summary.is_some(),
            can_read_handles,
        );
        let mut chosen = None;
        for detail in chain {
            let tokens = item_tokens(&resolved, detail);
            if used.saturating_add(tokens) <= budget {
                chosen = Some((detail, tokens));
                break;
            }
        }
        let Some((detail, tokens)) = chosen else {
            if selection.required {
                let needed = item_tokens(&resolved, ContextDetail::Full);
                return Err(ContextError::RequiredOverBudget {
                    id: selection.id,
                    needed,
                    available: budget.saturating_sub(used),
                });
            }
            dropped.push(DroppedContext {
                id: selection.id,
                reason: if can_read_handles {
                    ContextDropReason::NoBudget
                } else {
                    ContextDropReason::NoUsableForm
                },
            });
            continue;
        };
        handle_seq += 1;
        used = used.saturating_add(tokens);
        items.push(ContextItem {
            handle: format!("C{handle_seq}"),
            id: resolved.reference.id.clone(),
            label: resolved.reference.label.clone(),
            kind: resolved.reference.kind,
            scope: resolved.reference.scope,
            revision: resolved.reference.revision.clone(),
            required: selection.required,
            detail,
            text: match detail {
                ContextDetail::Handle => None,
                ContextDetail::Summary => resolved.document.summary.clone(),
                ContextDetail::Full => Some(resolved.document.full.clone()),
            },
            tokens,
        });
    }

    Ok(ContextPlan {
        project: request.project.clone(),
        session: request.session.clone(),
        model: request.model.name.clone(),
        reasoning,
        items,
        dropped,
        degradations,
        budget_tokens: budget,
        estimated_tokens: used,
    })
}

/// 粗估 token；真实值必须由 provider tokenizer 或已知 usage 校正。
pub fn estimate_tokens(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    chars.saturating_add(3) / 4
}

fn scope_allowed(
    reference: &ContextRef,
    request: &ContextRequest,
    selection: &ContextSelection,
) -> bool {
    match reference.scope {
        ContextScope::Session => reference.session.as_deref() == Some(request.session.as_str()),
        ContextScope::Project => reference.project.as_deref() == Some(request.project.as_str()),
        ContextScope::Shared => selection.shared_opt_in,
    }
}

fn scope_rank(scope: ContextScope) -> u8 {
    match scope {
        ContextScope::Session => 0,
        ContextScope::Project => 1,
        ContextScope::Shared => 2,
    }
}

fn scope_name(scope: ContextScope) -> &'static str {
    match scope {
        ContextScope::Session => "session",
        ContextScope::Project => "project",
        ContextScope::Shared => "shared",
    }
}

fn reasoning_reserve(effort: ReasoningEffort) -> u64 {
    match effort {
        ReasoningEffort::Off => 0,
        ReasoningEffort::Low => 512,
        ReasoningEffort::Medium => 2_048,
        ReasoningEffort::High => 8_192,
    }
}

fn preferred_detail(
    can_read_handles: bool,
    effort: ReasoningEffort,
    document: &ContextDocument,
) -> ContextDetail {
    if matches!(effort, ReasoningEffort::Medium | ReasoningEffort::High) {
        if document.summary.is_some() {
            ContextDetail::Summary
        } else {
            ContextDetail::Full
        }
    } else if can_read_handles {
        ContextDetail::Handle
    } else if document.summary.is_some() {
        ContextDetail::Summary
    } else {
        ContextDetail::Full
    }
}

fn detail_chain(
    preferred: ContextDetail,
    has_summary: bool,
    can_read_handles: bool,
) -> Vec<ContextDetail> {
    let mut chain = vec![preferred];
    if preferred == ContextDetail::Full && has_summary {
        chain.push(ContextDetail::Summary);
    }
    if can_read_handles && preferred != ContextDetail::Handle {
        chain.push(ContextDetail::Handle);
    }
    if !can_read_handles && preferred == ContextDetail::Handle {
        if has_summary {
            chain.push(ContextDetail::Summary);
        }
        chain.push(ContextDetail::Full);
    }
    let mut seen = BTreeSet::new();
    chain.retain(|detail| seen.insert(*detail));
    chain
}

fn item_tokens(resolved: &ResolvedContext, detail: ContextDetail) -> u64 {
    let body = match detail {
        ContextDetail::Handle => 0,
        ContextDetail::Summary => resolved
            .document
            .summary
            .as_deref()
            .map(estimate_tokens)
            .unwrap_or(0),
        ContextDetail::Full => resolved
            .document
            .tokens
            .unwrap_or_else(|| estimate_tokens(&resolved.document.full)),
    };
    handle_overhead(&resolved.reference).saturating_add(body)
}

fn handle_overhead(reference: &ContextRef) -> u64 {
    estimate_tokens(&reference.id)
        .saturating_add(estimate_tokens(&reference.label))
        .saturating_add(8)
}

fn merge_selections(selections: &[ContextSelection]) -> BTreeMap<String, ContextSelection> {
    let mut merged: BTreeMap<String, ContextSelection> = BTreeMap::new();
    for selection in selections {
        merged
            .entry(selection.id.clone())
            .and_modify(|existing| {
                existing.required |= selection.required;
                existing.shared_opt_in |= selection.shared_opt_in;
                existing.score = match (existing.score, selection.score) {
                    (Some(left), Some(right)) => Some(left.max(right)),
                    (left, right) => left.or(right),
                };
            })
            .or_insert_with(|| selection.clone());
    }
    merged
}
