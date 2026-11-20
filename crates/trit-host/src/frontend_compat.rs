//! 内核与前端的能力门禁。
//!
//! 内核只声明“提供什么能力”，前端只声明“认识哪些能力”。双方版本不一致、
//! 前端缺少入口或依赖不完整时，能力默认锁住并从面板隐藏，而不是让用户点到
//! 一个半残按钮。门禁结果同时给出责任方与通知目标，宿主负责真正发送通知。
//!
//! 这不是运行时功能开关：它不改变已有核心语义，只决定前端能否暴露入口。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// 前端自报的兼容信息。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendCapabilities {
    /// 单调递增的接口层级；内核按 `FeatureSpec::min_api_level` 比较。
    pub api_level: u32,
    /// 前端已实现入口的能力 id。
    #[serde(default)]
    pub features: BTreeSet<String>,
}

impl FrontendCapabilities {
    pub fn new(api_level: u32) -> Self {
        Self {
            api_level,
            features: BTreeSet::new(),
        }
    }

    pub fn with_feature(mut self, feature: impl Into<String>) -> Self {
        self.features.insert(feature.into());
        self
    }
}

/// 内核能力声明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureSpec {
    pub id: String,
    pub min_api_level: u32,
    /// 依赖的其他能力；任一依赖未启用则本能力锁住。
    #[serde(default)]
    pub requires: Vec<String>,
    /// 依赖满足且前端支持时是否默认开启。
    #[serde(default = "default_true")]
    pub default_enabled: bool,
    /// 责任归属（例如 `kernel.agent` / `frontend.desktop` / `platform`）。
    #[serde(default = "default_owner")]
    pub owner: String,
}

fn default_true() -> bool {
    true
}

fn default_owner() -> String {
    "kernel".to_owned()
}

impl FeatureSpec {
    pub fn new(id: impl Into<String>, min_api_level: u32) -> Self {
        Self {
            id: id.into(),
            min_api_level,
            requires: Vec::new(),
            default_enabled: true,
            owner: default_owner(),
        }
    }

    pub fn requires(mut self, features: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.requires = features.into_iter().map(Into::into).collect();
        self
    }

    pub fn default_enabled(mut self, enabled: bool) -> Self {
        self.default_enabled = enabled;
        self
    }

    pub fn owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = owner.into();
        self
    }
}

/// 一组必须同进退的前端入口，例如扩展坞或插件面板。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureGroupSpec {
    pub id: String,
    pub members: Vec<String>,
}

impl FeatureGroupSpec {
    pub fn new(
        id: impl Into<String>,
        members: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            id: id.into(),
            members: members.into_iter().map(Into::into).collect(),
        }
    }
}

/// 外部（用户/宿主配置）的开关请求；disable 优先于 enable。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRequest {
    #[serde(default)]
    pub enable: BTreeSet<String>,
    #[serde(default)]
    pub disable: BTreeSet<String>,
}

impl GateRequest {
    pub fn enabling(mut self, feature: impl Into<String>) -> Self {
        self.enable.insert(feature.into());
        self
    }

    pub fn disabling(mut self, feature: impl Into<String>) -> Self {
        self.disable.insert(feature.into());
        self
    }
}

/// 锁住的原因；所有原因都意味着 fail-closed。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum LockReason {
    /// 前端接口层级低于能力要求。
    FrontendApiTooOld { required: u32, actual: u32 },
    /// 前端没有实现该能力的入口。
    FrontendFeatureMissing { feature: String },
    /// 依赖能力未启用。
    DependencyUnavailable { feature: String },
    /// 依赖形成环，无法证明可用。
    DependencyCycle { feature: String },
}

impl LockReason {
    pub fn code(&self) -> &'static str {
        match self {
            Self::FrontendApiTooOld { .. } => "frontend_api_too_old",
            Self::FrontendFeatureMissing { .. } => "frontend_feature_missing",
            Self::DependencyUnavailable { .. } => "dependency_unavailable",
            Self::DependencyCycle { .. } => "dependency_cycle",
        }
    }

    /// 该原因首先该找谁；宿主据此路由通知。
    pub fn owner<'a>(&'a self, feature_owner: &'a str) -> &'a str {
        match self {
            Self::FrontendApiTooOld { .. } | Self::FrontendFeatureMissing { .. } => "frontend",
            Self::DependencyUnavailable { .. } | Self::DependencyCycle { .. } => feature_owner,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::FrontendApiTooOld { required, actual } => {
                format!("前端接口层级 {actual} 低于要求的 {required}")
            }
            Self::FrontendFeatureMissing { feature } => {
                format!("前端没有实现能力入口 {feature}")
            }
            Self::DependencyUnavailable { feature } => {
                format!("依赖能力 {feature} 未启用")
            }
            Self::DependencyCycle { feature } => {
                format!("依赖成环：{feature}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureState {
    Enabled,
    Disabled,
    Locked,
}

impl FeatureState {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }

    pub fn is_locked(&self) -> bool {
        matches!(self, Self::Locked)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureNotice {
    pub feature: String,
    pub owner: String,
    pub reason: LockReason,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureDecision {
    pub id: String,
    pub state: FeatureState,
    /// 前端是否允许显示入口；锁住的能力必须为 false。
    pub visible: bool,
    #[serde(default)]
    pub reasons: Vec<LockReason>,
}

impl FeatureDecision {
    fn locked(id: &str, reasons: Vec<LockReason>) -> Self {
        Self {
            id: id.to_owned(),
            state: FeatureState::Locked,
            visible: false,
            reasons,
        }
    }

    fn visible(id: &str, state: FeatureState) -> Self {
        Self {
            id: id.to_owned(),
            state,
            visible: true,
            reasons: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureGroupDecision {
    pub id: String,
    /// 组内没有锁住的能力；用户主动关闭不算阻塞。
    pub available: bool,
    /// 任一成员锁住则整组隐藏，避免打开一个空壳面板。
    pub visible: bool,
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateReport {
    pub features: BTreeMap<String, FeatureDecision>,
    pub groups: BTreeMap<String, FeatureGroupDecision>,
    #[serde(default)]
    pub notices: Vec<FeatureNotice>,
}

impl GateReport {
    pub fn enabled(&self, feature: &str) -> bool {
        self.features
            .get(feature)
            .is_some_and(|decision| decision.state.is_enabled())
    }

    pub fn visible(&self, feature: &str) -> bool {
        self.features
            .get(feature)
            .is_some_and(|decision| decision.visible)
    }
}

#[derive(Debug, Clone, Default)]
pub struct FeatureGate {
    specs: BTreeMap<String, FeatureSpec>,
    groups: Vec<FeatureGroupSpec>,
}

impl FeatureGate {
    pub fn new(specs: impl IntoIterator<Item = FeatureSpec>) -> Self {
        Self {
            specs: specs
                .into_iter()
                .map(|spec| (spec.id.clone(), spec))
                .collect(),
            groups: Vec::new(),
        }
    }

    pub fn with_groups(mut self, groups: impl IntoIterator<Item = FeatureGroupSpec>) -> Self {
        self.groups = groups.into_iter().collect();
        self
    }

    /// 当前内核公开的能力清单；前端可据此生成自己的能力表。
    pub fn kernel_defaults() -> Self {
        Self::new([
            FeatureSpec::new("context.catalog", 1).owner("kernel.agent"),
            FeatureSpec::new("context.cache_ledger", 1)
                .requires(["context.catalog"])
                .owner("kernel.agent"),
            FeatureSpec::new("mcp.tools", 1).owner("kernel.agent"),
            FeatureSpec::new("checkpoint.files", 1).owner("kernel.agent"),
            FeatureSpec::new("checkpoint.git", 1).owner("kernel.agent"),
            FeatureSpec::new("shell.persistent", 1)
                .default_enabled(false)
                .owner("kernel.agent"),
            FeatureSpec::new("session.migration", 1).owner("kernel.agent"),
            FeatureSpec::new("context.compression", 1)
                .requires(["context.catalog"])
                .owner("kernel.agent"),
        ])
    }

    pub fn resolve(&self, frontend: &FrontendCapabilities, request: &GateRequest) -> GateReport {
        let mut decisions = BTreeMap::new();
        let mut stack = Vec::new();
        for id in self.specs.keys() {
            self.resolve_feature(id, frontend, request, &mut decisions, &mut stack);
        }

        let mut notices = Vec::new();
        for decision in decisions.values() {
            let spec = self.specs.get(&decision.id);
            let owner = spec.map(|spec| spec.owner.as_str()).unwrap_or("kernel");
            for reason in &decision.reasons {
                notices.push(FeatureNotice {
                    feature: decision.id.clone(),
                    owner: reason.owner(owner).to_owned(),
                    reason: reason.clone(),
                    message: reason.message(),
                });
            }
        }
        notices.sort_by(|left, right| {
            left.owner
                .cmp(&right.owner)
                .then_with(|| left.feature.cmp(&right.feature))
                .then_with(|| left.reason.code().cmp(right.reason.code()))
        });

        let mut groups = BTreeMap::new();
        for group in &self.groups {
            let mut blocked_by = Vec::new();
            for member in &group.members {
                match decisions.get(member) {
                    Some(decision) if decision.state.is_locked() => {
                        blocked_by.push(member.clone());
                    }
                    Some(_) => {}
                    None => blocked_by.push(member.clone()),
                }
            }
            groups.insert(
                group.id.clone(),
                FeatureGroupDecision {
                    id: group.id.clone(),
                    available: blocked_by.is_empty(),
                    visible: blocked_by.is_empty(),
                    blocked_by,
                },
            );
        }

        GateReport {
            features: decisions,
            groups,
            notices,
        }
    }

    fn resolve_feature(
        &self,
        id: &str,
        frontend: &FrontendCapabilities,
        request: &GateRequest,
        decisions: &mut BTreeMap<String, FeatureDecision>,
        stack: &mut Vec<String>,
    ) -> FeatureDecision {
        if let Some(decision) = decisions.get(id) {
            return decision.clone();
        }
        if stack.iter().any(|visiting| visiting == id) {
            return FeatureDecision::locked(
                id,
                vec![LockReason::DependencyCycle {
                    feature: id.to_owned(),
                }],
            );
        }
        let Some(spec) = self.specs.get(id) else {
            return FeatureDecision::locked(
                id,
                vec![LockReason::DependencyUnavailable {
                    feature: id.to_owned(),
                }],
            );
        };

        stack.push(id.to_owned());
        let mut reasons = Vec::new();
        if frontend.api_level < spec.min_api_level {
            reasons.push(LockReason::FrontendApiTooOld {
                required: spec.min_api_level,
                actual: frontend.api_level,
            });
        }
        if !frontend.features.contains(id) {
            reasons.push(LockReason::FrontendFeatureMissing {
                feature: id.to_owned(),
            });
        }
        if reasons.is_empty() {
            for dependency in &spec.requires {
                let decision =
                    self.resolve_feature(dependency, frontend, request, decisions, stack);
                if !decision.state.is_enabled() {
                    reasons.push(LockReason::DependencyUnavailable {
                        feature: dependency.clone(),
                    });
                }
            }
        }
        stack.pop();

        let decision = if !reasons.is_empty() {
            FeatureDecision::locked(id, reasons)
        } else if request.disable.contains(id) {
            FeatureDecision::visible(id, FeatureState::Disabled)
        } else if request.enable.contains(id) || spec.default_enabled {
            FeatureDecision::visible(id, FeatureState::Enabled)
        } else {
            FeatureDecision::visible(id, FeatureState::Disabled)
        };
        decisions.insert(id.to_owned(), decision.clone());
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> FeatureGate {
        FeatureGate::new([
            FeatureSpec::new("base", 1),
            FeatureSpec::new("feature", 2)
                .requires(["base"])
                .owner("kernel.feature"),
        ])
    }

    fn frontend(api: u32, features: &[&str]) -> FrontendCapabilities {
        let mut frontend = FrontendCapabilities::new(api);
        frontend
            .features
            .extend(features.iter().map(|id| (*id).to_owned()));
        frontend
    }

    #[test]
    fn kernel_ahead_of_frontend_locks_and_hides_feature() {
        let report = gate().resolve(&frontend(2, &["base"]), &GateRequest::default());
        assert!(report.enabled("base"));
        assert!(!report.enabled("feature"));
        assert!(!report.visible("feature"));
        assert!(report.notices.iter().any(|notice| {
            notice.feature == "feature"
                && notice.owner == "frontend"
                && notice.reason.code() == "frontend_feature_missing"
        }));
    }

    #[test]
    fn old_frontend_api_locks_every_feature_above_its_level() {
        let report = gate().resolve(&frontend(1, &["base", "feature"]), &GateRequest::default());
        assert!(report.enabled("base"));
        assert!(!report.enabled("feature"));
        assert!(report.notices.iter().any(|notice| {
            notice.reason
                == LockReason::FrontendApiTooOld {
                    required: 2,
                    actual: 1,
                }
        }));
    }

    #[test]
    fn missing_dependency_fails_closed_even_if_frontend_supports_child() {
        let report = gate().resolve(&frontend(2, &["feature"]), &GateRequest::default());
        assert!(!report.enabled("feature"));
        assert!(report.notices.iter().any(|notice| {
            notice.feature == "feature"
                && notice.owner == "kernel.feature"
                && notice.reason.code() == "dependency_unavailable"
        }));
    }

    #[test]
    fn disabled_feature_remains_visible_but_dependents_lock() {
        let report = gate().resolve(
            &frontend(2, &["base", "feature"]),
            &GateRequest::default().disabling("base"),
        );
        assert!(!report.enabled("base"));
        assert!(report.visible("base"));
        assert!(!report.enabled("feature"));
        assert!(!report.visible("feature"));
    }

    #[test]
    fn explicit_enable_can_turn_on_default_off_feature() {
        let gate = FeatureGate::new([FeatureSpec::new("optional", 1).default_enabled(false)]);
        let off = gate.resolve(&frontend(1, &["optional"]), &GateRequest::default());
        assert!(!off.enabled("optional"));
        assert!(off.visible("optional"));
        let on = gate.resolve(
            &frontend(1, &["optional"]),
            &GateRequest::default().enabling("optional"),
        );
        assert!(on.enabled("optional"));
    }

    #[test]
    fn dependency_cycle_is_locked_without_recursing_forever() {
        let gate = FeatureGate::new([
            FeatureSpec::new("a", 1).requires(["b"]),
            FeatureSpec::new("b", 1).requires(["a"]),
        ]);
        let report = gate.resolve(&frontend(1, &["a", "b"]), &GateRequest::default());
        assert!(!report.enabled("a"));
        assert!(!report.enabled("b"));
        assert!(report.notices.iter().any(|notice| {
            matches!(notice.reason, LockReason::DependencyCycle { .. })
                || notice.reason.code() == "dependency_unavailable"
        }));
    }

    #[test]
    fn group_hides_entire_dock_if_any_member_is_locked() {
        let gate = gate().with_groups([FeatureGroupSpec::new("dock", ["base", "feature"])]);
        let report = gate.resolve(&frontend(2, &["base"]), &GateRequest::default());
        let dock = &report.groups["dock"];
        assert!(!dock.available);
        assert!(!dock.visible);
        assert_eq!(dock.blocked_by, vec!["feature".to_owned()]);
    }

    #[test]
    fn report_serializes_stable_state_and_reason_codes() {
        let report = gate().resolve(&frontend(2, &["base"]), &GateRequest::default());
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["features"]["base"]["state"], "enabled");
        assert_eq!(value["features"]["feature"]["state"], "locked");
        assert_eq!(value["features"]["feature"]["visible"], false);
        assert_eq!(
            value["notices"][0]["reason"]["code"],
            "frontend_feature_missing"
        );
        assert_eq!(value["notices"][0]["owner"], "frontend");
    }

    #[test]
    fn default_kernel_manifest_is_internally_consistent() {
        let gate = FeatureGate::kernel_defaults();
        let features: Vec<_> = gate.specs.keys().cloned().collect();
        let mut frontend = FrontendCapabilities::new(1);
        frontend.features.extend(features.iter().cloned());
        let report = gate.resolve(&frontend, &GateRequest::default());
        for (id, spec) in &gate.specs {
            if spec.default_enabled {
                assert!(
                    report.enabled(id),
                    "kernel default feature {id} should be enabled when the frontend supports it"
                );
            } else {
                assert!(!report.enabled(id), "feature {id} defaults to off");
                assert!(report.visible(id), "feature {id} stays user-toggleable");
            }
        }
    }
}
