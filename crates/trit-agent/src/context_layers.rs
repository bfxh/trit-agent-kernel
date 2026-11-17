//! 分层上下文组装：项目/会话/公共 scope、模型能力、推理强度和引用句柄。
//!
//! 这是“编排分配上下文”的第一层：宿主先给出候选 ref 与语义分数，组装器只负责
//! 权限/scope 校验、模型能力降级、预算选择，以及句柄/摘要/全文的取舍。
//! 它不调用模型，也不把语义判断硬编码成词表。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use trit_orch::{SelectionPressure, Ternary, TokenUsage};

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchOrder {
    /// 保留 catalog 自己的顺序。
    #[default]
    Resolver,
    /// 低命中压力：动态候选排在稳定前缀候选之前。
    PreferDynamic,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheClass {
    /// 稳定前缀：顺序/内容尽量跨 turn 不变，优先命中 prompt cache。
    StablePrefix,
    /// 动态尾段：每轮可变；不进前缀缓存。
    #[default]
    DynamicTail,
    /// 明确不可缓存（敏感/一次性）。
    Uncacheable,
}

impl CacheClass {
    pub fn rank(self) -> u8 {
        match self {
            Self::StablePrefix => 0,
            Self::DynamicTail => 1,
            Self::Uncacheable => 2,
        }
    }
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
    #[serde(default)]
    pub cache: CacheClass,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSpan {
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorImpression {
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpressionDelta {
    Same,
    Changed,
    Unknown,
}

impl ImpressionDelta {
    /// 领域三态到三值逻辑的桥：Same=True、Changed=False、Unknown=Unknown。
    pub fn as_ternary(self) -> Ternary {
        match self {
            Self::Same => Ternary::True,
            Self::Changed => Ternary::False,
            Self::Unknown => Ternary::Unknown,
        }
    }
}

pub trait ContextResolver {
    fn resolve(&self, id: &str) -> Result<Option<ResolvedContext>, ContextError>;
}

/// 智能体可搜索的上下文目录；搜索只返回候选 ref，不直接展开正文。
pub trait ContextCatalog: ContextResolver {
    fn search(&self, query: &str, limit: usize) -> Vec<ContextRef>;
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

impl ContextCatalog for InMemoryContext {
    fn search(&self, query: &str, limit: usize) -> Vec<ContextRef> {
        let query = query.to_lowercase();
        let mut hits: Vec<ContextRef> = self
            .entries
            .values()
            .filter(|entry| {
                entry.reference.id.to_lowercase().contains(&query)
                    || entry.reference.label.to_lowercase().contains(&query)
            })
            .map(|entry| entry.reference.clone())
            .collect();
        hits.sort_by(|left, right| left.id.cmp(&right.id));
        hits.truncate(limit.max(1));
        hits
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
    #[serde(default)]
    pub provider: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomyPolicy {
    /// 智能体能否自行加入可选上下文；required 不受此闸门影响。
    pub may_add_optional: Ternary,
    /// 是否允许用句柄按需展开；False/Unknown 时会降级为摘要/全文。
    pub may_expand_handles: Ternary,
    /// 是否允许改变稳定前缀；False 时把变化降级到动态尾段。
    pub may_break_prefix: Ternary,
}

impl Default for AutonomyPolicy {
    fn default() -> Self {
        Self {
            may_add_optional: Ternary::True,
            may_expand_handles: Ternary::True,
            may_break_prefix: Ternary::False,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCalibration {
    /// 校准所属模型；空表示不校验。
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub samples: u64,
    #[serde(default)]
    pub exact_samples: u64,
    #[serde(default)]
    pub estimated_samples: u64,
    #[serde(default)]
    pub cache_hit_tokens: u64,
    #[serde(default)]
    pub cache_miss_tokens: u64,
    #[serde(default)]
    pub output_max: u64,
}

impl UsageCalibration {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }

    pub fn observe(&mut self, usage: &TokenUsage) {
        self.samples = self.samples.saturating_add(1);
        if usage.estimated {
            self.estimated_samples = self.estimated_samples.saturating_add(1);
        } else {
            self.exact_samples = self.exact_samples.saturating_add(1);
            self.output_max = self.output_max.max(usage.output);
        }
        self.cache_hit_tokens = self.cache_hit_tokens.saturating_add(usage.cached);
        self.cache_miss_tokens = self.cache_miss_tokens.saturating_add(usage.fresh);
    }

    /// 只有至少 3 次精确观测才敢把 output 预留降到实测值。
    pub fn confidence(&self) -> Ternary {
        if self.exact_samples >= 3 {
            Ternary::True
        } else if self.exact_samples > 0 {
            Ternary::Unknown
        } else {
            Ternary::False
        }
    }

    pub fn cache_hit_rate_bp(&self) -> u32 {
        let input = self.cache_hit_tokens.saturating_add(self.cache_miss_tokens);
        if input == 0 {
            0
        } else {
            ((u128::from(self.cache_hit_tokens) * 10_000) / u128::from(input)) as u32
        }
    }

    pub fn output_reserve(&self, declared: u64) -> u64 {
        if self.confidence() == Ternary::True && self.output_max > 0 {
            self.output_max
        } else {
            declared
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationReport {
    pub model: String,
    pub samples: u64,
    pub exact_samples: u64,
    pub confidence: Ternary,
    pub output_reserve: u64,
    pub cache_hit_rate_bp: u32,
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
    #[serde(default)]
    pub autonomy: AutonomyPolicy,
    /// 之前 turn 的“印象”，用于判断稳定前缀是否真的没变。
    #[serde(default)]
    pub priors: BTreeMap<String, PriorImpression>,
    #[serde(default)]
    pub calibration: Option<UsageCalibration>,
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
    pub cache: CacheClass,
    #[serde(default)]
    pub score: Option<u16>,
    #[serde(default)]
    pub content_hash: Option<String>,
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
    AutonomyDenied,
    AutonomyUnknown,
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
    AutonomyUnknown,
    CalibrationUncertain,
    CalibrationModelMismatch,
    PrefixChangeRequiresApproval,
    DynamicPrefixOverride,
    BudgetReduced { requested: u64, available: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheReport {
    pub prefix_items: usize,
    pub dynamic_items: usize,
    pub prefix_tokens: u64,
    pub dynamic_tokens: u64,
    /// 实际观测到的 prefix 命中率（基点）；没有观测时为 None。
    #[serde(default)]
    pub observed_hit_rate_bp: Option<u32>,
    /// 实际观测的置信度；没有观测时为 Unknown。
    #[serde(default)]
    pub calibration: Ternary,
    /// 稳定前缀的确定性指纹；前缀没变时它应保持不变。
    #[serde(default)]
    pub prefix_fingerprint: String,
    /// 稳定前缀本轮的复用把握：True 同印象，False 已失效/不可缓存，Unknown 未校准。
    pub hit_potential: Ternary,
    /// 低命中时建议的任务选批压力；Normal 表示不改变任务顺序。
    #[serde(default)]
    pub task_selection: SelectionPressure,
    pub invalidated: Vec<String>,
}

impl Default for CacheReport {
    fn default() -> Self {
        Self {
            prefix_items: 0,
            dynamic_items: 0,
            prefix_tokens: 0,
            dynamic_tokens: 0,
            observed_hit_rate_bp: None,
            calibration: Ternary::Unknown,
            prefix_fingerprint: String::new(),
            hit_potential: Ternary::Unknown,
            task_selection: SelectionPressure::Normal,
            invalidated: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InclusionReceipt {
    pub project: String,
    pub session: String,
    pub prefix_fingerprint: String,
    pub handles: Vec<String>,
    pub refs: Vec<String>,
    pub estimated_tokens: u64,
    pub hit_potential: Ternary,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheObservation {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    pub prefix_fingerprint: String,
    pub prompt_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub estimated: bool,
    #[serde(default)]
    pub observed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSample {
    #[serde(default)]
    pub at_ms: Option<u64>,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub estimated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheWindow {
    pub max_samples: usize,
    pub max_age_ms: Option<u64>,
}

impl Default for CacheWindow {
    fn default() -> Self {
        Self {
            max_samples: 64,
            max_age_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheWindowStats {
    pub samples: u64,
    pub exact_samples: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub exact_hit_tokens: u64,
    pub exact_miss_tokens: u64,
}

impl CacheWindowStats {
    pub fn hit_rate_bp(&self) -> u32 {
        let input = self.cache_hit_tokens.saturating_add(self.cache_miss_tokens);
        if input == 0 {
            0
        } else {
            ((u128::from(self.cache_hit_tokens) * 10_000) / u128::from(input)) as u32
        }
    }

    pub fn calibrated_hit_rate_bp(&self) -> u32 {
        let input = self.exact_hit_tokens.saturating_add(self.exact_miss_tokens);
        if input == 0 {
            0
        } else {
            ((u128::from(self.exact_hit_tokens) * 10_000) / u128::from(input)) as u32
        }
    }

    pub fn confidence(&self) -> Ternary {
        if self.exact_samples >= 3 {
            Ternary::True
        } else if self.exact_samples > 0 {
            Ternary::Unknown
        } else {
            Ternary::False
        }
    }
}

impl CacheObservation {
    pub fn cache_hit_rate_bp(&self) -> u32 {
        let input = self.cache_hit_tokens.saturating_add(self.cache_miss_tokens);
        if input == 0 {
            0
        } else {
            ((u128::from(self.cache_hit_tokens) * 10_000) / u128::from(input)) as u32
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStat {
    pub samples: u64,
    pub exact_samples: u64,
    pub estimated_samples: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    /// 仅精确样本的 hit/miss，用于校准。
    pub exact_hit_tokens: u64,
    pub exact_miss_tokens: u64,
    /// 最近样本（用于时间/样本窗口校准）；旧日志无时间戳也会保留。
    #[serde(default)]
    pub recent: VecDeque<CacheSample>,
    /// 最后访问时间（Unix 秒），用于 TTL 过期判断
    #[serde(default)]
    pub last_accessed: u64,
}

impl CacheStat {
    pub fn merge(&mut self, other: &CacheStat) {
        self.samples = self.samples.saturating_add(other.samples);
        self.exact_samples = self.exact_samples.saturating_add(other.exact_samples);
        self.estimated_samples = self
            .estimated_samples
            .saturating_add(other.estimated_samples);
        self.cache_hit_tokens = self.cache_hit_tokens.saturating_add(other.cache_hit_tokens);
        self.cache_miss_tokens = self
            .cache_miss_tokens
            .saturating_add(other.cache_miss_tokens);
        self.exact_hit_tokens = self.exact_hit_tokens.saturating_add(other.exact_hit_tokens);
        self.exact_miss_tokens = self
            .exact_miss_tokens
            .saturating_add(other.exact_miss_tokens);
    }

    pub fn observe(&mut self, observation: &CacheObservation) {
        self.samples = self.samples.saturating_add(1);
        self.recent.push_back(CacheSample {
            at_ms: observation.observed_at_ms,
            cache_hit_tokens: observation.cache_hit_tokens,
            cache_miss_tokens: observation.cache_miss_tokens,
            estimated: observation.estimated,
        });
        while self.recent.len() > MAX_RECENT_CACHE_SAMPLES {
            self.recent.pop_front();
        }
        self.cache_hit_tokens = self
            .cache_hit_tokens
            .saturating_add(observation.cache_hit_tokens);
        self.cache_miss_tokens = self
            .cache_miss_tokens
            .saturating_add(observation.cache_miss_tokens);
        if observation.estimated {
            self.estimated_samples = self.estimated_samples.saturating_add(1);
        } else {
            self.exact_samples = self.exact_samples.saturating_add(1);
            self.exact_hit_tokens = self
                .exact_hit_tokens
                .saturating_add(observation.cache_hit_tokens);
            self.exact_miss_tokens = self
                .exact_miss_tokens
                .saturating_add(observation.cache_miss_tokens);
        }
    }

    pub fn hit_rate_bp(&self) -> u32 {
        let input = self.cache_hit_tokens.saturating_add(self.cache_miss_tokens);
        if input == 0 {
            0
        } else {
            ((u128::from(self.cache_hit_tokens) * 10_000) / u128::from(input)) as u32
        }
    }

    pub fn calibrated_hit_rate_bp(&self) -> u32 {
        let input = self.exact_hit_tokens.saturating_add(self.exact_miss_tokens);
        if input == 0 {
            0
        } else {
            ((u128::from(self.exact_hit_tokens) * 10_000) / u128::from(input)) as u32
        }
    }

    pub fn confidence(&self) -> Ternary {
        if self.exact_samples >= 3 {
            Ternary::True
        } else if self.exact_samples > 0 {
            Ternary::Unknown
        } else {
            Ternary::False
        }
    }

    pub fn windowed(&self, now_ms: Option<u64>, window: CacheWindow) -> CacheWindowStats {
        let mut stats = CacheWindowStats::default();
        for sample in self.recent.iter().rev().take(window.max_samples.max(1)) {
            if window.max_age_ms.is_some() && sample.at_ms.is_none() {
                // 有时间窗时，无法证明“新”的无时间戳旧样本不参与校准。
                continue;
            }
            if let (Some(max_age), Some(now), Some(at)) = (window.max_age_ms, now_ms, sample.at_ms)
                && now.saturating_sub(at) > max_age
            {
                continue;
            }
            stats.samples = stats.samples.saturating_add(1);
            stats.cache_hit_tokens = stats
                .cache_hit_tokens
                .saturating_add(sample.cache_hit_tokens);
            stats.cache_miss_tokens = stats
                .cache_miss_tokens
                .saturating_add(sample.cache_miss_tokens);
            if !sample.estimated {
                stats.exact_samples = stats.exact_samples.saturating_add(1);
                stats.exact_hit_tokens = stats
                    .exact_hit_tokens
                    .saturating_add(sample.cache_hit_tokens);
                stats.exact_miss_tokens = stats
                    .exact_miss_tokens
                    .saturating_add(sample.cache_miss_tokens);
            }
        }
        stats
    }
}

const MAX_RECENT_CACHE_SAMPLES: usize = 256;

/// 缓存条目的压缩结果：保留键、丢弃键以及可注入下一轮的摘要。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressedContext {
    pub retained_keys: Vec<String>,
    pub dropped_keys: Vec<String>,
    pub summary: String,
    pub original_token_count: u64,
    pub compressed_token_count: u64,
}

/// 按命中质量与样本置信度划分的缓存温度。
///
/// 温度只决定回收顺序；实际新近程度仍由 LRU 顺序细化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTier {
    Hot,
    Warm,
    Cold,
}

impl CacheTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Warm => "warm",
            Self::Cold => "cold",
        }
    }
}

impl fmt::Display for CacheTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheTiers {
    pub hot: Vec<String>,
    pub warm: Vec<String>,
    pub cold: Vec<String>,
}

impl CacheTiers {
    pub fn len(&self) -> usize {
        self.hot.len() + self.warm.len() + self.cold.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 按 prefix fingerprint 聚合实际 cache 观测；用于把计划层 hit_potential
/// 校准成可验证的命中率。
///
/// `ttl_secs` / `max_entries` 为 0 表示不限制。旧版持久化 ledger 没有
/// `last_accessed` 与 LRU 顺序，加载后按 0 处理：不参与 TTL 淘汰，直到
/// 下一次观测刷新；容量淘汰前会先补齐 LRU 顺序。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheLedger {
    entries: BTreeMap<String, CacheStat>,
    #[serde(default)]
    lru_order: Vec<String>,
    #[serde(default)]
    ttl_secs: u64,
    #[serde(default)]
    max_entries: usize,
}

impl CacheLedger {
    pub fn new(ttl_secs: u64, max_entries: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            lru_order: Vec::new(),
            ttl_secs,
            max_entries,
        }
    }

    pub fn ttl_secs(&self) -> u64 {
        self.ttl_secs
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn touch(&mut self, key: &str) {
        if let Some(position) = self.lru_order.iter().position(|candidate| candidate == key) {
            self.lru_order.remove(position);
        }
        self.lru_order.push(key.to_owned());
    }

    /// 旧 ledger 或外部 merge 可能缺少 LRU 记录；按最后访问时间补齐。
    fn ensure_lru_order(&mut self) {
        self.lru_order.retain(|key| self.entries.contains_key(key));
        let mut seen: BTreeSet<String> = self.lru_order.iter().cloned().collect();
        let mut missing: Vec<(u64, &str)> = self
            .entries
            .iter()
            .filter(|(key, _)| !seen.contains(*key))
            .map(|(key, stat)| (stat.last_accessed, key.as_str()))
            .collect();
        missing.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
        let missing: Vec<String> = missing.into_iter().map(|(_, key)| key.to_owned()).collect();
        // 缺失顺序的条目视为更旧，必须排在新近访问的键之前。
        self.lru_order.splice(0..0, missing);
        seen.clear();
        self.lru_order.retain(|key| seen.insert(key.clone()));
    }

    fn evict_expired(&mut self, now: u64) {
        if self.ttl_secs == 0 {
            return;
        }
        self.entries.retain(|_, stat| {
            stat.last_accessed == 0 || now.saturating_sub(stat.last_accessed) <= self.ttl_secs
        });
        self.lru_order.retain(|key| self.entries.contains_key(key));
    }

    /// 命中率与样本置信度决定温度；LRU 只在同一温度内决定先后。
    pub fn tier_for(stat: &CacheStat) -> CacheTier {
        const HOT_HIT_RATE_BP: u32 = 8_000;
        const COLD_HIT_RATE_BP: u32 = 2_000;
        match stat.confidence() {
            Ternary::True => {
                let rate = stat.calibrated_hit_rate_bp();
                if rate >= HOT_HIT_RATE_BP {
                    CacheTier::Hot
                } else if rate < COLD_HIT_RATE_BP {
                    CacheTier::Cold
                } else {
                    CacheTier::Warm
                }
            }
            Ternary::Unknown => {
                if stat.hit_rate_bp() < COLD_HIT_RATE_BP {
                    CacheTier::Cold
                } else {
                    CacheTier::Warm
                }
            }
            Ternary::False => CacheTier::Cold,
        }
    }

    pub fn tiers(&self) -> CacheTiers {
        let mut tiers = CacheTiers::default();
        for (key, stat) in &self.entries {
            match Self::tier_for(stat) {
                CacheTier::Hot => tiers.hot.push(key.clone()),
                CacheTier::Warm => tiers.warm.push(key.clone()),
                CacheTier::Cold => tiers.cold.push(key.clone()),
            }
        }
        tiers
    }

    fn enforce_capacity(&mut self) {
        if self.max_entries == 0 || self.entries.len() <= self.max_entries {
            return;
        }
        self.ensure_lru_order();
        let excess = self.entries.len() - self.max_entries;
        let mut eviction_order = Vec::with_capacity(self.lru_order.len());
        for tier in [CacheTier::Cold, CacheTier::Warm, CacheTier::Hot] {
            eviction_order.extend(self.lru_order.iter().filter_map(|key| {
                self.entries
                    .get(key)
                    .filter(|stat| Self::tier_for(stat) == tier)
                    .map(|_| key.clone())
            }));
        }
        let evicted: BTreeSet<String> = eviction_order.into_iter().take(excess).collect();
        self.entries.retain(|key, _| !evicted.contains(key));
        self.lru_order.retain(|key| !evicted.contains(key));
    }

    pub fn observe(&mut self, observation: &CacheObservation) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let key = cache_key(
            &observation.provider,
            &observation.model,
            &observation.prefix_fingerprint,
        );
        let stat = self.entries.entry(key.clone()).or_default();
        stat.observe(observation);
        stat.last_accessed = now;
        self.touch(&key);
        self.evict_expired(now);
        self.enforce_capacity();
    }

    pub fn merge(&mut self, other: &CacheLedger) {
        for (key, stat) in &other.entries {
            let entry = self.entries.entry(key.clone()).or_default();
            entry.merge(stat);
            entry.last_accessed = entry.last_accessed.max(stat.last_accessed);
            self.touch(key);
        }
        if !other.entries.is_empty() {
            self.evict_expired(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            );
        }
        self.enforce_capacity();
    }

    pub fn stat_for(
        &self,
        provider: &str,
        model: &str,
        prefix_fingerprint: &str,
    ) -> Option<&CacheStat> {
        self.entries
            .get(&cache_key(provider, model, prefix_fingerprint))
    }

    pub fn entries(&self) -> impl Iterator<Item = (&String, &CacheStat)> {
        self.entries.iter()
    }

    /// 在预算内保留高置信度条目，其余折叠成摘要。
    ///
    /// `min_confidence` 是保留门槛；`budget_tokens == 0` 表示不限制预算。
    /// token 数使用观测到的 hit + miss token 总量，而不是条目数。
    pub fn compress(&self, min_confidence: Ternary, budget_tokens: u64) -> CompressedContext {
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_by(|left, right| {
            Self::tier_for(left.1)
                .cmp(&Self::tier_for(right.1))
                .then_with(|| {
                    right
                        .1
                        .confidence()
                        .index()
                        .cmp(&left.1.confidence().index())
                })
                .then_with(|| cache_entry_tokens(right.1).cmp(&cache_entry_tokens(left.1)))
                .then_with(|| left.0.cmp(right.0))
        });

        let original_token_count = self
            .entries
            .values()
            .map(cache_entry_tokens)
            .fold(0u64, u64::saturating_add);
        let mut retained = Vec::new();
        let mut dropped = Vec::new();
        let mut retained_tokens = 0u64;
        for (key, stat) in entries {
            let tokens = cache_entry_tokens(stat);
            let within_budget =
                budget_tokens == 0 || retained_tokens.saturating_add(tokens) <= budget_tokens;
            if stat.confidence() >= min_confidence && within_budget {
                retained.push(key.clone());
                retained_tokens = retained_tokens.saturating_add(tokens);
            } else {
                dropped.push(key.clone());
            }
        }

        let mut summary = String::new();
        for key in &dropped {
            if let Some(stat) = self.entries.get(key) {
                summary.push_str(&format!(
                    "[{key}] tier={} hits={} miss={} rate={}bp confidence={}\n",
                    Self::tier_for(stat),
                    stat.cache_hit_tokens,
                    stat.cache_miss_tokens,
                    stat.hit_rate_bp(),
                    stat.confidence()
                ));
            }
        }

        CompressedContext {
            retained_keys: retained,
            dropped_keys: dropped,
            summary,
            original_token_count,
            compressed_token_count: retained_tokens,
        }
    }

    /// 获取热点条目：先按访问 token 量，再按置信度排序。
    pub fn hot_entries(&self, limit: usize) -> Vec<(&String, &CacheStat)> {
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_by(|left, right| {
            cache_entry_tokens(right.1)
                .cmp(&cache_entry_tokens(left.1))
                .then_with(|| {
                    right
                        .1
                        .confidence()
                        .index()
                        .cmp(&left.1.confidence().index())
                })
                .then_with(|| left.0.cmp(right.0))
        });
        entries.into_iter().take(limit).collect()
    }

    pub fn total_hit_rate_bp(&self) -> u32 {
        let hits: u64 = self
            .entries
            .values()
            .map(|stat| stat.cache_hit_tokens)
            .sum();
        let misses: u64 = self
            .entries
            .values()
            .map(|stat| stat.cache_miss_tokens)
            .sum();
        let input = hits.saturating_add(misses);
        if input == 0 {
            0
        } else {
            ((u128::from(hits) * 10_000) / u128::from(input)) as u32
        }
    }

    /// 用实际观测把某个 prefix 的命中把握折叠成三值；样本不足返回 Unknown。
    pub fn hit_potential_for(
        &self,
        provider: &str,
        model: &str,
        prefix_fingerprint: &str,
        threshold_bp: u32,
    ) -> Ternary {
        match self.stat_for(provider, model, prefix_fingerprint) {
            Some(stat) if stat.confidence() == Ternary::True => {
                Ternary::from_bool(stat.calibrated_hit_rate_bp() >= threshold_bp)
            }
            _ => Ternary::Unknown,
        }
    }
}

fn cache_key(provider: &str, model: &str, prefix_fingerprint: &str) -> String {
    format!("{provider}|{model}|{prefix_fingerprint}")
}

fn cache_entry_tokens(stat: &CacheStat) -> u64 {
    stat.cache_hit_tokens.saturating_add(stat.cache_miss_tokens)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReceipt {
    #[serde(default)]
    pub inclusion: Option<InclusionReceipt>,
    #[serde(default)]
    pub cache: Option<CacheObservation>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPlan {
    pub project: String,
    pub session: String,
    pub model: String,
    #[serde(default)]
    pub provider: String,
    pub reasoning: ReasoningEffort,
    pub items: Vec<ContextItem>,
    pub dropped: Vec<DroppedContext>,
    pub degradations: Vec<ContextDegradation>,
    pub budget_tokens: u64,
    pub estimated_tokens: u64,
    pub cache: CacheReport,
    pub autonomy: AutonomyPolicy,
    #[serde(default)]
    pub calibration: Option<CalibrationReport>,
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
        self.render_subset(|_| true, false)
    }

    /// 句柄清单 + 被选择内联的摘要/全文；宿主可直接嵌入 prompt。
    pub fn render_for_model(&self) -> String {
        self.render_subset(|_| true, true)
    }

    /// 只渲染稳定前缀（应放在 cache-stable system prefix）。
    pub fn render_prefix(&self) -> String {
        self.render_subset(|item| item.cache == CacheClass::StablePrefix, true)
    }

    /// 只渲染动态尾段（应放在每轮变化的 user tail）。
    pub fn render_tail(&self) -> String {
        self.render_subset(|item| item.cache != CacheClass::StablePrefix, true)
    }

    /// 把稳定前缀整体降级为动态尾段；用于“实际命中率长期过低”的显式策略。
    pub fn demote_stable_prefix(&mut self) {
        for item in &mut self.items {
            if item.cache == CacheClass::StablePrefix {
                item.cache = CacheClass::DynamicTail;
            }
        }
        self.cache.prefix_items = 0;
        self.cache.prefix_tokens = 0;
        self.cache.dynamic_items = self.items.len();
        self.cache.dynamic_tokens = self.items.iter().map(|item| item.tokens).sum();
        self.cache.prefix_fingerprint.clear();
        self.cache.hit_potential = Ternary::False;
    }

    pub fn inclusion_receipt(&self) -> InclusionReceipt {
        InclusionReceipt {
            project: self.project.clone(),
            session: self.session.clone(),
            prefix_fingerprint: self.cache.prefix_fingerprint.clone(),
            handles: self.items.iter().map(|item| item.handle.clone()).collect(),
            refs: self.items.iter().map(|item| item.id.clone()).collect(),
            estimated_tokens: self.estimated_tokens,
            hit_potential: self.cache.hit_potential,
        }
    }

    fn render_subset<F: Fn(&ContextItem) -> bool>(
        &self,
        filter: F,
        include_inline: bool,
    ) -> String {
        let mut out = String::from("# Context\n");
        let mut any = false;
        for item in self.items.iter().filter(|item| filter(item)) {
            any = true;
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
        if !any {
            out.push_str("- (none)\n");
            return out;
        }
        if include_inline {
            for item in self
                .items
                .iter()
                .filter(|item| filter(item) && item.detail != ContextDetail::Handle)
            {
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
    #[error("context ref {0} is unknown")]
    UnknownRef(String),
    #[error("context ref {0} is already included")]
    AlreadyIncluded(String),
    #[error("context ref {0} was denied by autonomy policy")]
    AutonomyDenied(String),
    #[error("context ref {0} has unknown autonomy status")]
    AutonomyUnknown(String),
    #[error("context add {id} exceeds budget: needs {needed}, available {available}")]
    DynamicBudgetExceeded {
        id: String,
        needed: u64,
        available: u64,
    },
    #[error("context handle {0} is not in the current plan")]
    UnknownHandle(String),
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
    let autonomy = request.autonomy;
    let handle_gate = match autonomy.may_expand_handles {
        Ternary::True => true,
        Ternary::Unknown => {
            degradations.push(ContextDegradation::AutonomyUnknown);
            false
        }
        Ternary::False => false,
    };
    let can_read_handles =
        request.model.supports_tools && request.model.supports_context_read && handle_gate;
    let reasoning = if request.model.supports_reasoning {
        request.reasoning
    } else {
        if request.reasoning != ReasoningEffort::Off {
            degradations.push(ContextDegradation::ReasoningUnsupported);
        }
        ReasoningEffort::Off
    };

    let declared_output = request.model.max_output_tokens;
    let mut calibration_report = None;
    let output_reserve = match &request.calibration {
        Some(calibration)
            if calibration.model.is_empty() || calibration.model == request.model.name =>
        {
            let confidence = calibration.confidence();
            if confidence != Ternary::True {
                degradations.push(ContextDegradation::CalibrationUncertain);
            }
            let reserve = if confidence == Ternary::True {
                calibration.output_reserve(declared_output)
            } else {
                declared_output
            };
            calibration_report = Some(CalibrationReport {
                model: if calibration.model.is_empty() {
                    request.model.name.clone()
                } else {
                    calibration.model.clone()
                },
                samples: calibration.samples,
                exact_samples: calibration.exact_samples,
                confidence,
                output_reserve: reserve,
                cache_hit_rate_bp: calibration.cache_hit_rate_bp(),
            });
            reserve
        }
        Some(_) => {
            degradations.push(ContextDegradation::CalibrationModelMismatch);
            declared_output
        }
        None => declared_output,
    };

    let window = request.model.context_window;
    if window == 0 || output_reserve >= window {
        return Err(ContextError::InvalidModelProfile);
    }
    let available = window
        .saturating_sub(output_reserve)
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
    let mut stable_candidates = 0usize;
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
        if resolved.reference.cache == CacheClass::StablePrefix {
            stable_candidates += 1;
        }
        if !selection.required {
            match autonomy.may_add_optional {
                Ternary::True => {}
                Ternary::False => {
                    dropped.push(DroppedContext {
                        id: selection.id.clone(),
                        reason: ContextDropReason::AutonomyDenied,
                    });
                    continue;
                }
                Ternary::Unknown => {
                    degradations.push(ContextDegradation::AutonomyUnknown);
                    dropped.push(DroppedContext {
                        id: selection.id.clone(),
                        reason: ContextDropReason::AutonomyUnknown,
                    });
                    continue;
                }
            }
        }

        let delta = impression_delta(&resolved.reference, request.priors.get(&selection.id));
        let mut cache = resolved.reference.cache;
        if cache == CacheClass::StablePrefix && delta == ImpressionDelta::Changed {
            match autonomy.may_break_prefix {
                Ternary::True => {}
                Ternary::False => cache = CacheClass::DynamicTail,
                Ternary::Unknown => {
                    cache = CacheClass::DynamicTail;
                    degradations.push(ContextDegradation::PrefixChangeRequiresApproval);
                }
            }
        }
        candidates.push((selection.clone(), resolved, delta, cache));
    }

    candidates.sort_by(
        |(left, left_ref, _, left_cache), (right, right_ref, _, right_cache)| {
            right
                .required
                .cmp(&left.required)
                .then_with(|| left_cache.rank().cmp(&right_cache.rank()))
                .then_with(|| right.score.unwrap_or(0).cmp(&left.score.unwrap_or(0)))
                .then_with(|| {
                    scope_rank(left_ref.reference.scope).cmp(&scope_rank(right_ref.reference.scope))
                })
                .then_with(|| left_ref.reference.id.cmp(&right_ref.reference.id))
        },
    );

    let mut items = Vec::new();
    let mut used = 0u64;
    let mut invalidated = Vec::new();
    let mut deltas: BTreeMap<String, (CacheClass, ImpressionDelta)> = BTreeMap::new();
    for (selection, resolved, delta, cache) in candidates {
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
        if resolved.reference.cache == CacheClass::StablePrefix && delta == ImpressionDelta::Changed
        {
            invalidated.push(selection.id.clone());
        }
        used = used.saturating_add(tokens);
        deltas.insert(selection.id.clone(), (cache, delta));
        items.push(ContextItem {
            handle: String::new(),
            id: resolved.reference.id.clone(),
            label: resolved.reference.label.clone(),
            kind: resolved.reference.kind,
            scope: resolved.reference.scope,
            revision: resolved.reference.revision.clone(),
            required: selection.required,
            cache,
            score: selection.score,
            content_hash: resolved.reference.content_hash.clone(),
            detail,
            text: match detail {
                ContextDetail::Handle => None,
                ContextDetail::Summary => resolved.document.summary.clone(),
                ContextDetail::Full => Some(resolved.document.full.clone()),
            },
            tokens,
        });
    }

    // 稳定前缀按 id 排序，避免语义分数变化把 prefix 顺序打散；动态尾段仍按分数排序。
    items.sort_by(|left, right| {
        left.cache.rank().cmp(&right.cache.rank()).then_with(|| {
            if left.cache == CacheClass::StablePrefix {
                left.id.cmp(&right.id)
            } else {
                right
                    .required
                    .cmp(&left.required)
                    .then_with(|| right.score.unwrap_or(0).cmp(&left.score.unwrap_or(0)))
                    .then_with(|| left.id.cmp(&right.id))
            }
        })
    });
    for (index, item) in items.iter_mut().enumerate() {
        item.handle = format!("C{}", index + 1);
    }

    let mut cache_report = CacheReport::default();
    for item in &items {
        if item.cache == CacheClass::StablePrefix {
            cache_report.prefix_items += 1;
            cache_report.prefix_tokens = cache_report.prefix_tokens.saturating_add(item.tokens);
        } else {
            cache_report.dynamic_items += 1;
            cache_report.dynamic_tokens = cache_report.dynamic_tokens.saturating_add(item.tokens);
        }
    }
    cache_report.prefix_fingerprint = prefix_fingerprint(&items);
    let stable_deltas: Vec<ImpressionDelta> = items
        .iter()
        .filter(|item| item.cache == CacheClass::StablePrefix)
        .filter_map(|item| deltas.get(&item.id).map(|(_, delta)| *delta))
        .collect();
    invalidated.sort_unstable();
    invalidated.dedup();
    cache_report.hit_potential = if invalidated.is_empty()
        && !stable_deltas.is_empty()
        && stable_candidates == cache_report.prefix_items
    {
        if stable_deltas.contains(&ImpressionDelta::Unknown) {
            Ternary::Unknown
        } else {
            Ternary::True
        }
    } else {
        Ternary::False
    };
    cache_report.invalidated = invalidated;

    Ok(ContextPlan {
        project: request.project.clone(),
        session: request.session.clone(),
        model: request.model.name.clone(),
        provider: request.model.provider.clone(),
        reasoning,
        items,
        dropped,
        degradations,
        budget_tokens: budget,
        estimated_tokens: used,
        cache: cache_report,
        autonomy,
        calibration: calibration_report,
    })
}

/// 智能体自主上下文：在初始计划之上允许 agent 搜索、添加动态 ref、按句柄读取。
///
/// 动态添加永远落到 `DynamicTail`，不会改稳定前缀；ACL/预算/secret/autonomy
/// 仍由宿主侧这里统一执行。初始计划之外的新 ref 不会自动进入前缀缓存。
pub struct AgentContext<R> {
    request: ContextRequest,
    resolver: R,
    plan: ContextPlan,
    dynamic_budget_scale_bp: u32,
    search_limit_scale_bp: u32,
    search_order: SearchOrder,
}

impl<R: ContextCatalog> AgentContext<R> {
    pub fn new(request: ContextRequest, resolver: R, plan: ContextPlan) -> Self {
        Self {
            request,
            resolver,
            plan,
            dynamic_budget_scale_bp: 10_000,
            search_limit_scale_bp: 10_000,
            search_order: SearchOrder::Resolver,
        }
    }

    pub fn assemble(request: ContextRequest, resolver: R) -> Result<Self, ContextError> {
        let plan = assemble_context(&request, &resolver)?;
        Ok(Self::new(request, resolver, plan))
    }

    pub fn plan(&self) -> &ContextPlan {
        &self.plan
    }

    pub fn dynamic_budget_scale_bp(&self) -> u32 {
        self.dynamic_budget_scale_bp
    }

    /// 缓存压力策略设置；10000 表示不缩放。
    pub fn set_dynamic_budget_scale_bp(&mut self, scale_bp: u32) {
        self.dynamic_budget_scale_bp = scale_bp.min(10_000);
    }

    pub fn search_limit_scale_bp(&self) -> u32 {
        self.search_limit_scale_bp
    }

    pub fn set_search_limit_scale_bp(&mut self, scale_bp: u32) {
        self.search_limit_scale_bp = scale_bp.min(10_000);
    }

    pub fn search_order(&self) -> SearchOrder {
        self.search_order
    }

    pub fn set_search_order(&mut self, search_order: SearchOrder) {
        self.search_order = search_order;
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<ContextRef> {
        let scaled = limit
            .saturating_mul(usize::try_from(self.search_limit_scale_bp).unwrap_or(usize::MAX))
            / 10_000;
        let mut hits = self.resolver.search(query, scaled.max(1));
        if self.search_order == SearchOrder::PreferDynamic {
            hits.sort_by(|left, right| {
                (left.cache == CacheClass::StablePrefix)
                    .cmp(&(right.cache == CacheClass::StablePrefix))
                    .then_with(|| left.id.cmp(&right.id))
            });
        }
        hits
    }

    /// 智能体主动添加一个动态 ref；成功返回新句柄项。
    pub fn add(&mut self, id: &str) -> Result<ContextItem, ContextError> {
        if self.plan.items.iter().any(|item| item.id == id) {
            return Err(ContextError::AlreadyIncluded(id.to_owned()));
        }
        match self.request.autonomy.may_add_optional {
            Ternary::True => {}
            Ternary::False => return Err(ContextError::AutonomyDenied(id.to_owned())),
            Ternary::Unknown => return Err(ContextError::AutonomyUnknown(id.to_owned())),
        }
        let resolved = self
            .resolver
            .resolve(id)?
            .ok_or_else(|| ContextError::UnknownRef(id.to_owned()))?;
        if resolved.reference.id != id || resolved.document.id != id {
            return Err(ContextError::DocumentMismatch(id.to_owned()));
        }
        let selection = ContextSelection {
            id: id.to_owned(),
            required: false,
            score: None,
            shared_opt_in: false,
        };
        if !scope_allowed(&resolved.reference, &self.request, &selection) {
            return Err(ContextError::RequiredOutOfScope(id.to_owned()));
        }
        if resolved.reference.sensitivity == Sensitivity::Secret && !self.request.allow_secret {
            return Err(ContextError::RequiredSecret(id.to_owned()));
        }

        let mut can_read_handles = self.request.model.supports_tools
            && self.request.model.supports_context_read
            && self.request.autonomy.may_expand_handles == Ternary::True;
        if self.request.autonomy.may_expand_handles == Ternary::Unknown {
            self.plan
                .degradations
                .push(ContextDegradation::AutonomyUnknown);
            can_read_handles = false;
        }
        let preferred = preferred_detail(can_read_handles, self.plan.reasoning, &resolved.document);
        let remaining = self
            .plan
            .budget_tokens
            .saturating_sub(self.plan.estimated_tokens)
            .saturating_mul(u64::from(self.dynamic_budget_scale_bp))
            / 10_000;
        let mut chosen = None;
        for detail in detail_chain(
            preferred,
            resolved.document.summary.is_some(),
            can_read_handles,
        ) {
            let tokens = item_tokens(&resolved, detail);
            if tokens <= remaining {
                chosen = Some((detail, tokens));
                break;
            }
        }
        let Some((detail, tokens)) = chosen else {
            return Err(ContextError::DynamicBudgetExceeded {
                id: id.to_owned(),
                needed: item_tokens(&resolved, ContextDetail::Full),
                available: remaining,
            });
        };

        let mut cache = resolved.reference.cache;
        if cache == CacheClass::StablePrefix {
            // Agent 运行时新增内容不能改已经缓存的前缀。
            cache = CacheClass::DynamicTail;
            self.plan
                .degradations
                .push(ContextDegradation::DynamicPrefixOverride);
        }
        let item = ContextItem {
            handle: format!("C{}", self.plan.items.len() + 1),
            id: resolved.reference.id.clone(),
            label: resolved.reference.label.clone(),
            kind: resolved.reference.kind,
            scope: resolved.reference.scope,
            revision: resolved.reference.revision.clone(),
            required: false,
            cache,
            score: None,
            content_hash: resolved.reference.content_hash.clone(),
            detail,
            text: match detail {
                ContextDetail::Handle => None,
                ContextDetail::Summary => resolved.document.summary.clone(),
                ContextDetail::Full => Some(resolved.document.full.clone()),
            },
            tokens,
        };
        self.plan.estimated_tokens = self.plan.estimated_tokens.saturating_add(tokens);
        self.plan.cache.dynamic_items += 1;
        self.plan.cache.dynamic_tokens = self.plan.cache.dynamic_tokens.saturating_add(tokens);
        self.plan.items.push(item.clone());
        Ok(item)
    }

    /// 按句柄读取完整正文；读取本身不改变 prefix，也不自动加入计划。
    pub fn read(&self, handle: &str) -> Result<(ContextItem, String), ContextError> {
        let item = self
            .plan
            .items
            .iter()
            .find(|item| item.handle == handle)
            .ok_or_else(|| ContextError::UnknownHandle(handle.to_owned()))?;
        let resolved = self
            .resolver
            .resolve(&item.id)?
            .ok_or_else(|| ContextError::UnknownRef(item.id.clone()))?;
        Ok((item.clone(), resolved.document.full))
    }
}

fn impression_delta(reference: &ContextRef, prior: Option<&PriorImpression>) -> ImpressionDelta {
    let Some(prior) = prior else {
        return ImpressionDelta::Unknown;
    };
    let revision_same = !reference.revision.is_empty() && reference.revision == prior.revision;
    let hash_same = match (&reference.content_hash, &prior.content_hash) {
        (Some(current), Some(previous)) => Some(current == previous),
        _ => None,
    };
    match (revision_same, hash_same) {
        (true, Some(false)) => ImpressionDelta::Changed,
        (true, _) | (false, Some(true)) => ImpressionDelta::Same,
        (false, Some(false)) => ImpressionDelta::Changed,
        (false, None) => {
            if reference.revision.is_empty() && reference.content_hash.is_none() {
                ImpressionDelta::Unknown
            } else {
                ImpressionDelta::Changed
            }
        }
    }
}

fn prefix_fingerprint(items: &[ContextItem]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut any = false;
    for item in items
        .iter()
        .filter(|item| item.cache == CacheClass::StablePrefix)
    {
        any = true;
        for part in [
            item.id.as_str(),
            item.revision.as_str(),
            item.content_hash.as_deref().unwrap_or(""),
        ] {
            for byte in part.bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    if any {
        format!("fnv1a64:{hash:016x}")
    } else {
        String::new()
    }
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
