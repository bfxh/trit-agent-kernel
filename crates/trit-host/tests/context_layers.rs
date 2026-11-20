use std::collections::BTreeMap;

use trit_host::{
    AutonomyPolicy, CacheClass, ContextDegradation, ContextDetail, ContextDocument,
    ContextDropReason, ContextError, ContextKind, ContextRef, ContextRequest, ContextScope,
    ContextSelection, ImpressionDelta, InMemoryContext, ModelProfile, PriorImpression,
    ReasoningEffort, Sensitivity, UsageCalibration, assemble_context,
};
use trit_orch::{Ternary, TokenUsage};

fn reference(
    id: &str,
    scope: ContextScope,
    project: Option<&str>,
    session: Option<&str>,
) -> ContextRef {
    ContextRef {
        id: id.into(),
        label: format!("label-{id}"),
        kind: ContextKind::Doc,
        scope,
        project: project.map(str::to_owned),
        session: session.map(str::to_owned),
        revision: "rev1".into(),
        content_hash: Some(format!("hash-{id}")),
        sensitivity: Sensitivity::Internal,
        cache: CacheClass::DynamicTail,
    }
}

fn document(id: &str, full: &str, summary: Option<&str>, tokens: Option<u64>) -> ContextDocument {
    ContextDocument {
        id: id.into(),
        full: full.into(),
        summary: summary.map(str::to_owned),
        tokens,
    }
}

fn model(tools: bool, reasoning: bool) -> ModelProfile {
    ModelProfile {
        name: "test-model".into(),
        provider: "test-provider".into(),
        context_window: 20_000,
        max_output_tokens: 1_000,
        supports_tools: tools,
        supports_context_read: tools,
        supports_reasoning: reasoning,
    }
}

fn request(
    model: ModelProfile,
    reasoning: ReasoningEffort,
    selections: Vec<ContextSelection>,
) -> ContextRequest {
    ContextRequest {
        project: "p1".into(),
        session: "s1".into(),
        query: "test".into(),
        selections,
        budget_tokens: 10_000,
        model,
        reasoning,
        allow_secret: false,
        autonomy: AutonomyPolicy::default(),
        priors: BTreeMap::new(),
        calibration: None,
    }
}

fn selection(id: &str, required: bool, score: Option<u16>) -> ContextSelection {
    ContextSelection {
        id: id.into(),
        required,
        score,
        shared_opt_in: false,
    }
}

#[test]
fn project_and_session_scopes_are_enforced_and_shared_needs_opt_in() {
    let mut context = InMemoryContext::default();
    context
        .insert(
            reference("s1", ContextScope::Session, Some("p1"), Some("s1")),
            document("s1", "session text", None, Some(10)),
        )
        .unwrap();
    context
        .insert(
            reference("p1", ContextScope::Project, Some("p1"), None),
            document("p1", "project text", None, Some(10)),
        )
        .unwrap();
    context
        .insert(
            reference("p2", ContextScope::Project, Some("p2"), None),
            document("p2", "other project", None, Some(10)),
        )
        .unwrap();
    context
        .insert(
            reference("shared", ContextScope::Shared, Some("p2"), None),
            document("shared", "shared text", None, Some(10)),
        )
        .unwrap();

    let mut selections = vec![
        selection("s1", true, None),
        selection("p1", false, Some(50)),
        selection("p2", false, Some(90)),
        selection("shared", false, Some(100)),
    ];
    let plan = assemble_context(
        &request(model(true, false), ReasoningEffort::Low, selections.clone()),
        &context,
    )
    .unwrap();
    assert_eq!(plan.items[0].id, "s1");
    assert_eq!(plan.items[0].detail, ContextDetail::Full);
    assert!(plan.items.iter().any(|item| item.id == "p1"));
    assert!(!plan.items.iter().any(|item| item.id == "p2"));
    assert!(!plan.items.iter().any(|item| item.id == "shared"));
    assert!(
        plan.dropped
            .iter()
            .any(|drop| drop.id == "p2" && drop.reason == ContextDropReason::OutOfScope)
    );
    assert!(
        plan.dropped
            .iter()
            .any(|drop| drop.id == "shared" && drop.reason == ContextDropReason::OutOfScope)
    );

    for selection in &mut selections {
        if selection.id == "shared" {
            selection.shared_opt_in = true;
        }
    }
    let plan = assemble_context(
        &request(model(true, false), ReasoningEffort::Low, selections),
        &context,
    )
    .unwrap();
    assert!(plan.items.iter().any(|item| item.id == "shared"));
    assert!(!plan.items.iter().any(|item| item.id == "p2"));
}

#[test]
fn required_refs_fail_closed() {
    let mut context = InMemoryContext::default();
    context
        .insert(
            reference("big", ContextScope::Project, Some("p1"), None),
            document("big", "x", None, Some(5_000)),
        )
        .unwrap();
    context
        .insert(
            reference("other", ContextScope::Project, Some("p2"), None),
            document("other", "x", None, Some(1)),
        )
        .unwrap();
    let mut secret = reference("secret", ContextScope::Project, Some("p1"), None);
    secret.sensitivity = Sensitivity::Secret;
    context
        .insert(secret, document("secret", "x", None, Some(1)))
        .unwrap();

    let mut over_budget = request(
        model(true, false),
        ReasoningEffort::Low,
        vec![selection("big", true, None)],
    );
    over_budget.budget_tokens = 10;
    assert!(matches!(
        assemble_context(&over_budget, &context),
        Err(ContextError::RequiredOverBudget { .. })
    ));

    let out_of_scope = request(
        model(true, false),
        ReasoningEffort::Low,
        vec![selection("other", true, None)],
    );
    assert!(matches!(
        assemble_context(&out_of_scope, &context),
        Err(ContextError::RequiredOutOfScope(id)) if id == "other"
    ));

    let secret_required = request(
        model(true, false),
        ReasoningEffort::Low,
        vec![selection("secret", true, None)],
    );
    assert!(matches!(
        assemble_context(&secret_required, &context),
        Err(ContextError::RequiredSecret(id)) if id == "secret"
    ));

    let missing = request(
        model(true, false),
        ReasoningEffort::Low,
        vec![selection("missing", true, None)],
    );
    assert!(matches!(
        assemble_context(&missing, &context),
        Err(ContextError::UnknownRequired(id)) if id == "missing"
    ));
}

#[test]
fn optional_refs_default_to_handles_when_tools_are_available() {
    let mut context = InMemoryContext::default();
    context
        .insert(
            reference("doc", ContextScope::Project, Some("p1"), None),
            document("doc", "full document", Some("short summary"), Some(100)),
        )
        .unwrap();
    let plan = assemble_context(
        &request(
            model(true, false),
            ReasoningEffort::Low,
            vec![selection("doc", false, Some(10))],
        ),
        &context,
    )
    .unwrap();
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].detail, ContextDetail::Handle);
    assert!(plan.items[0].text.is_none());
    assert_eq!(plan.handles().count(), 1);
    assert_eq!(plan.inline().count(), 0);
    assert!(plan.render_manifest().contains("C1 [project] label-doc"));
    assert!(!plan.render_for_model().contains("full document"));
}

#[test]
fn model_without_tools_falls_back_to_summary_or_full() {
    let mut context = InMemoryContext::default();
    context
        .insert(
            reference("with-summary", ContextScope::Project, Some("p1"), None),
            document("with-summary", "full", Some("summary"), Some(100)),
        )
        .unwrap();
    context
        .insert(
            reference("without-summary", ContextScope::Project, Some("p1"), None),
            document("without-summary", "full text", None, Some(100)),
        )
        .unwrap();
    let plan = assemble_context(
        &request(
            model(false, false),
            ReasoningEffort::Low,
            vec![
                selection("with-summary", false, Some(10)),
                selection("without-summary", false, Some(5)),
            ],
        ),
        &context,
    )
    .unwrap();
    assert!(
        plan.degradations
            .contains(&ContextDegradation::ToolsUnsupported)
    );
    assert_eq!(plan.items[0].id, "with-summary");
    assert_eq!(plan.items[0].detail, ContextDetail::Summary);
    assert_eq!(plan.items[0].text.as_deref(), Some("summary"));
    assert_eq!(plan.items[1].id, "without-summary");
    assert_eq!(plan.items[1].detail, ContextDetail::Full);
}

#[test]
fn tools_without_context_read_fall_back_to_summary() {
    let mut context = InMemoryContext::default();
    context
        .insert(
            reference("doc", ContextScope::Project, Some("p1"), None),
            document("doc", "full", Some("summary"), Some(100)),
        )
        .unwrap();
    let mut model = model(true, false);
    model.supports_context_read = false;
    let plan = assemble_context(
        &request(
            model,
            ReasoningEffort::Low,
            vec![selection("doc", false, Some(10))],
        ),
        &context,
    )
    .unwrap();
    assert!(
        plan.degradations
            .contains(&ContextDegradation::ContextReadUnsupported)
    );
    assert_eq!(plan.items[0].detail, ContextDetail::Summary);
}

#[test]
fn reasoning_effort_changes_detail_and_reserves_window() {
    let mut context = InMemoryContext::default();
    context
        .insert(
            reference("doc", ContextScope::Project, Some("p1"), None),
            document("doc", "full", Some("summary"), Some(100)),
        )
        .unwrap();
    let mut low_request = request(
        model(true, true),
        ReasoningEffort::Low,
        vec![selection("doc", false, Some(10))],
    );
    low_request.budget_tokens = 0;
    let low = assemble_context(&low_request, &context).unwrap();
    let mut high_request = request(
        model(true, true),
        ReasoningEffort::High,
        vec![selection("doc", false, Some(10))],
    );
    high_request.budget_tokens = 0;
    let high = assemble_context(&high_request, &context).unwrap();
    assert_eq!(low.items[0].detail, ContextDetail::Handle);
    assert_eq!(high.items[0].detail, ContextDetail::Summary);
    assert!(high.budget_tokens < low.budget_tokens);

    let unsupported = assemble_context(
        &request(
            model(true, false),
            ReasoningEffort::High,
            vec![selection("doc", false, Some(10))],
        ),
        &context,
    )
    .unwrap();
    assert_eq!(unsupported.reasoning, ReasoningEffort::Off);
    assert!(
        unsupported
            .degradations
            .contains(&ContextDegradation::ReasoningUnsupported)
    );
    assert_eq!(unsupported.items[0].detail, ContextDetail::Handle);
}

#[test]
fn semantic_scores_order_optional_refs_and_required_stays_first() {
    let mut context = InMemoryContext::default();
    for id in ["low", "high", "required"] {
        context
            .insert(
                reference(id, ContextScope::Project, Some("p1"), None),
                document(id, id, None, Some(1)),
            )
            .unwrap();
    }
    let plan = assemble_context(
        &request(
            model(true, false),
            ReasoningEffort::Low,
            vec![
                selection("low", false, Some(10)),
                selection("high", false, Some(90)),
                selection("required", true, Some(0)),
            ],
        ),
        &context,
    )
    .unwrap();
    assert_eq!(
        plan.items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["required", "high", "low"]
    );
    assert_eq!(
        plan.items
            .iter()
            .map(|item| item.handle.as_str())
            .collect::<Vec<_>>(),
        ["C1", "C2", "C3"]
    );
}

#[test]
fn duplicate_selections_merge_required_and_opt_in() {
    let mut context = InMemoryContext::default();
    let mut shared = reference("shared", ContextScope::Shared, Some("p1"), None);
    shared.sensitivity = Sensitivity::Public;
    context
        .insert(shared, document("shared", "shared", None, Some(1)))
        .unwrap();
    let mut optional = selection("shared", false, Some(10));
    optional.shared_opt_in = true;
    let plan = assemble_context(
        &request(
            model(true, false),
            ReasoningEffort::Low,
            vec![optional, selection("shared", true, Some(99))],
        ),
        &context,
    )
    .unwrap();
    assert_eq!(plan.items.len(), 1);
    assert!(plan.items[0].required);
    assert_eq!(plan.items[0].detail, ContextDetail::Full);
}

#[test]
fn secret_is_dropped_unless_explicitly_allowed() {
    let mut context = InMemoryContext::default();
    let mut secret = reference("secret", ContextScope::Project, Some("p1"), None);
    secret.sensitivity = Sensitivity::Secret;
    context
        .insert(secret, document("secret", "secret", None, Some(1)))
        .unwrap();
    let plan = assemble_context(
        &request(
            model(true, false),
            ReasoningEffort::Low,
            vec![selection("secret", false, None)],
        ),
        &context,
    )
    .unwrap();
    assert!(plan.items.is_empty());
    assert!(
        plan.dropped
            .iter()
            .any(|drop| drop.id == "secret" && drop.reason == ContextDropReason::Secret)
    );

    let mut request = request(
        model(true, false),
        ReasoningEffort::Low,
        vec![selection("secret", false, None)],
    );
    request.allow_secret = true;
    let plan = assemble_context(&request, &context).unwrap();
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].detail, ContextDetail::Handle);
}

#[test]
fn rejects_model_profile_without_room() {
    let context = InMemoryContext::default();
    let mut model = model(true, false);
    model.max_output_tokens = model.context_window;
    let request = request(model, ReasoningEffort::Off, Vec::new());
    assert_eq!(
        assemble_context(&request, &context),
        Err(ContextError::InvalidModelProfile)
    );
}

#[test]
fn impression_delta_maps_to_ternary() {
    assert_eq!(ImpressionDelta::Same.as_ternary(), Ternary::True);
    assert_eq!(ImpressionDelta::Changed.as_ternary(), Ternary::False);
    assert_eq!(ImpressionDelta::Unknown.as_ternary(), Ternary::Unknown);
}

#[test]
fn rejects_document_id_mismatch_on_insert() {
    let mut context = InMemoryContext::default();
    let error = context
        .insert(
            reference("ref", ContextScope::Project, Some("p1"), None),
            document("other", "text", None, None),
        )
        .unwrap_err();
    assert_eq!(error, ContextError::DocumentMismatch("other".into()));
}

#[test]
fn stable_prefix_order_and_fingerprint_ignore_semantic_scores() {
    let mut context = InMemoryContext::default();
    for (id, cache) in [
        ("b", CacheClass::StablePrefix),
        ("a", CacheClass::StablePrefix),
        ("c", CacheClass::DynamicTail),
    ] {
        let mut reference = reference(id, ContextScope::Project, Some("p1"), None);
        reference.cache = cache;
        context
            .insert(reference, document(id, id, None, Some(10)))
            .unwrap();
    }
    let mut request = request(
        model(true, false),
        ReasoningEffort::Low,
        vec![
            selection("c", false, Some(100)),
            selection("a", false, Some(10)),
            selection("b", false, Some(5)),
        ],
    );
    let plan = assemble_context(&request, &context).unwrap();
    assert_eq!(
        plan.items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    assert_eq!(plan.cache.prefix_items, 2);
    assert_eq!(plan.cache.dynamic_items, 1);
    assert!(plan.cache.prefix_fingerprint.starts_with("fnv1a64:"));
    assert_eq!(plan.cache.hit_potential, Ternary::Unknown);
    let fingerprint = plan.cache.prefix_fingerprint.clone();

    request.priors.insert(
        "a".into(),
        prior(&reference("a", ContextScope::Project, Some("p1"), None)),
    );
    request.priors.insert(
        "b".into(),
        prior(&reference("b", ContextScope::Project, Some("p1"), None)),
    );
    let plan = assemble_context(&request, &context).unwrap();
    assert_eq!(plan.cache.prefix_fingerprint, fingerprint);
    assert_eq!(plan.cache.hit_potential, Ternary::True);
    assert!(plan.cache.invalidated.is_empty());
}

#[test]
fn changed_impressions_invalidate_or_demote_the_prefix() {
    let mut context = InMemoryContext::default();
    for id in ["a", "b"] {
        let mut reference = reference(id, ContextScope::Project, Some("p1"), None);
        reference.cache = CacheClass::StablePrefix;
        context
            .insert(reference, document(id, id, None, Some(10)))
            .unwrap();
    }
    let mut request = request(
        model(true, false),
        ReasoningEffort::Low,
        vec![
            selection("a", false, Some(10)),
            selection("b", false, Some(5)),
        ],
    );
    request.priors.insert(
        "a".into(),
        prior(&reference("a", ContextScope::Project, Some("p1"), None)),
    );
    request.priors.insert(
        "b".into(),
        PriorImpression {
            revision: "old".into(),
            content_hash: Some("old-hash".into()),
            summary: None,
        },
    );

    let plan = assemble_context(&request, &context).unwrap();
    assert_eq!(plan.cache.hit_potential, Ternary::False);
    assert_eq!(plan.cache.invalidated, ["b"]);
    assert_eq!(
        plan.items.iter().find(|item| item.id == "b").unwrap().cache,
        CacheClass::DynamicTail
    );
    assert_eq!(plan.cache.prefix_items, 1);

    request.autonomy.may_break_prefix = Ternary::True;
    let plan = assemble_context(&request, &context).unwrap();
    assert_eq!(plan.cache.hit_potential, Ternary::False);
    assert_eq!(plan.cache.invalidated, ["b"]);
    assert_eq!(plan.cache.prefix_items, 2);

    request.autonomy.may_break_prefix = Ternary::Unknown;
    let plan = assemble_context(&request, &context).unwrap();
    assert_eq!(plan.cache.invalidated, ["b"]);
    assert!(
        plan.degradations
            .contains(&ContextDegradation::PrefixChangeRequiresApproval)
    );
}

#[test]
fn autonomy_can_deny_optional_context_and_handle_expansion() {
    let mut context = InMemoryContext::default();
    context
        .insert(
            reference("doc", ContextScope::Project, Some("p1"), None),
            document("doc", "full", Some("summary"), Some(100)),
        )
        .unwrap();
    let mut request = request(
        model(true, false),
        ReasoningEffort::Low,
        vec![selection("doc", false, None)],
    );
    request.autonomy.may_add_optional = Ternary::False;
    let plan = assemble_context(&request, &context).unwrap();
    assert!(plan.items.is_empty());
    assert!(
        plan.dropped
            .iter()
            .any(|drop| drop.reason == ContextDropReason::AutonomyDenied)
    );

    request.autonomy.may_add_optional = Ternary::Unknown;
    let plan = assemble_context(&request, &context).unwrap();
    assert!(plan.items.is_empty());
    assert!(
        plan.dropped
            .iter()
            .any(|drop| drop.reason == ContextDropReason::AutonomyUnknown)
    );
    assert!(
        plan.degradations
            .contains(&ContextDegradation::AutonomyUnknown)
    );

    request.autonomy.may_add_optional = Ternary::True;
    request.autonomy.may_expand_handles = Ternary::False;
    let plan = assemble_context(&request, &context).unwrap();
    assert_eq!(plan.items[0].detail, ContextDetail::Summary);
}

#[test]
fn real_usage_calibration_overrides_declared_output_reserve() {
    let mut calibration = UsageCalibration::new("test-model");
    for (output, cached, fresh) in [(1_000, 100, 900), (2_000, 800, 200), (1_500, 0, 1_000)] {
        calibration.observe(&TokenUsage {
            fresh,
            cached,
            output,
            estimated: false,
        });
    }
    let mut request = request(model(true, false), ReasoningEffort::Off, Vec::new());
    request.calibration = Some(calibration);
    let plan = assemble_context(&request, &NoSuchResolver).unwrap();
    let report = plan.calibration.unwrap();
    assert_eq!(report.confidence, Ternary::True);
    assert_eq!(report.output_reserve, 2_000);
    assert_eq!(report.cache_hit_rate_bp, 3_000);
    assert_eq!(plan.budget_tokens, 10_000);
}

#[test]
fn estimated_or_mismatched_calibration_is_not_trusted() {
    let mut estimated = UsageCalibration::new("test-model");
    for _ in 0..3 {
        estimated.observe(&TokenUsage {
            fresh: 10,
            cached: 0,
            output: 1_000,
            estimated: true,
        });
    }
    let mut request = request(model(true, false), ReasoningEffort::Off, Vec::new());
    request.calibration = Some(estimated);
    let plan = assemble_context(&request, &NoSuchResolver).unwrap();
    let report = plan.calibration.unwrap();
    assert_eq!(report.confidence, Ternary::False);
    assert_eq!(report.output_reserve, 1_000);
    assert!(
        plan.degradations
            .contains(&ContextDegradation::CalibrationUncertain)
    );

    let mut mismatch = UsageCalibration::new("other-model");
    mismatch.observe(&TokenUsage {
        fresh: 10,
        cached: 0,
        output: 2_000,
        estimated: false,
    });
    request.calibration = Some(mismatch);
    let plan = assemble_context(&request, &NoSuchResolver).unwrap();
    assert!(plan.calibration.is_none());
    assert!(
        plan.degradations
            .contains(&ContextDegradation::CalibrationModelMismatch)
    );
}

struct NoSuchResolver;

impl trit_host::ContextResolver for NoSuchResolver {
    fn resolve(&self, _id: &str) -> Result<Option<trit_host::ResolvedContext>, ContextError> {
        Ok(None)
    }
}

fn prior(reference: &ContextRef) -> PriorImpression {
    PriorImpression {
        revision: reference.revision.clone(),
        content_hash: reference.content_hash.clone(),
        summary: None,
    }
}

#[test]
fn cache_ledger_calibrates_prefix_hit_potential_from_real_observations() {
    let mut ledger = trit_host::CacheLedger::default();
    for _ in 0..3 {
        ledger.observe(&trit_host::CacheObservation {
            provider: "p".into(),
            model: "m".into(),
            prefix_fingerprint: "fnv1a64:ok".into(),
            prompt_tokens: 100,
            cache_hit_tokens: 80,
            cache_miss_tokens: 20,
            estimated: false,
            observed_at_ms: None,
        });
    }
    assert_eq!(
        ledger.hit_potential_for("p", "m", "fnv1a64:ok", 5_000),
        Ternary::True
    );
    assert_eq!(
        ledger.hit_potential_for("p", "m", "fnv1a64:ok", 9_000),
        Ternary::False
    );
    assert_eq!(
        ledger.hit_potential_for("p", "m", "fnv1a64:missing", 5_000),
        Ternary::Unknown
    );
}

#[test]
fn estimated_cache_samples_do_not_move_calibrated_rate() {
    let mut ledger = trit_host::CacheLedger::default();
    for _ in 0..3 {
        ledger.observe(&trit_host::CacheObservation {
            provider: "p".into(),
            model: "m".into(),
            prefix_fingerprint: "fnv1a64:mixed".into(),
            prompt_tokens: 100,
            cache_hit_tokens: 20,
            cache_miss_tokens: 80,
            estimated: false,
            observed_at_ms: None,
        });
    }
    for _ in 0..10 {
        ledger.observe(&trit_host::CacheObservation {
            provider: "p".into(),
            model: "m".into(),
            prefix_fingerprint: "fnv1a64:mixed".into(),
            prompt_tokens: 100,
            cache_hit_tokens: 100,
            cache_miss_tokens: 0,
            estimated: true,
            observed_at_ms: None,
        });
    }
    let stat = ledger.stat_for("p", "m", "fnv1a64:mixed").unwrap();
    assert_eq!(stat.hit_rate_bp(), 8_153);
    assert_eq!(stat.calibrated_hit_rate_bp(), 2000);
    assert_eq!(
        ledger.hit_potential_for("p", "m", "fnv1a64:mixed", 5000),
        Ternary::False
    );
}

#[test]
fn cache_window_excludes_old_samples() {
    let mut ledger = trit_host::CacheLedger::default();
    for (at, hit, miss) in [
        (1_000, 20, 80),
        (1_000, 20, 80),
        (1_000, 20, 80),
        (10_000, 80, 20),
        (10_000, 80, 20),
        (10_000, 80, 20),
    ] {
        ledger.observe(&trit_host::CacheObservation {
            provider: "p".into(),
            model: "m".into(),
            prefix_fingerprint: "fnv1a64:window".into(),
            prompt_tokens: 100,
            cache_hit_tokens: hit,
            cache_miss_tokens: miss,
            estimated: false,
            observed_at_ms: Some(at),
        });
    }
    let stat = ledger.stat_for("p", "m", "fnv1a64:window").unwrap();
    let window = stat.windowed(
        Some(10_000),
        trit_host::CacheWindow {
            max_samples: 64,
            max_age_ms: Some(5_000),
        },
    );
    assert_eq!(window.calibrated_hit_rate_bp(), 8_000);
    assert_eq!(window.confidence(), Ternary::True);
}

#[test]
fn cache_ledger_separates_provider_endpoints() {
    let mut ledger = trit_host::CacheLedger::default();
    for (provider, hit, miss) in [("https://a", 80, 20), ("https://b", 20, 80)] {
        for _ in 0..3 {
            ledger.observe(&trit_host::CacheObservation {
                provider: provider.into(),
                model: "same-model".into(),
                prefix_fingerprint: "fnv1a64:same".into(),
                prompt_tokens: 100,
                cache_hit_tokens: hit,
                cache_miss_tokens: miss,
                estimated: false,
                observed_at_ms: None,
            });
        }
    }
    assert_eq!(
        ledger.hit_potential_for("https://a", "same-model", "fnv1a64:same", 5_000),
        Ternary::True
    );
    assert_eq!(
        ledger.hit_potential_for("https://b", "same-model", "fnv1a64:same", 5_000),
        Ternary::False
    );
}

#[test]
fn time_window_excludes_untimestamped_legacy_samples() {
    let mut ledger = trit_host::CacheLedger::default();
    ledger.observe(&trit_host::CacheObservation {
        provider: "p".into(),
        model: "m".into(),
        prefix_fingerprint: "fnv1a64:legacy".into(),
        prompt_tokens: 100,
        cache_hit_tokens: 80,
        cache_miss_tokens: 20,
        estimated: false,
        observed_at_ms: None,
    });
    let stat = ledger.stat_for("p", "m", "fnv1a64:legacy").unwrap();
    let no_age = stat.windowed(
        Some(10_000),
        trit_host::CacheWindow {
            max_samples: 64,
            max_age_ms: None,
        },
    );
    assert_eq!(no_age.samples, 1);
    let with_age = stat.windowed(
        Some(10_000),
        trit_host::CacheWindow {
            max_samples: 64,
            max_age_ms: Some(5_000),
        },
    );
    assert_eq!(with_age.samples, 0);
}

#[test]
fn cache_ledger_roundtrips_through_json() {
    let mut ledger = trit_host::CacheLedger::default();
    ledger.observe(&trit_host::CacheObservation {
        provider: "p".into(),
        model: "m".into(),
        prefix_fingerprint: "fnv1a64:persist".into(),
        prompt_tokens: 100,
        cache_hit_tokens: 80,
        cache_miss_tokens: 20,
        estimated: false,
        observed_at_ms: Some(1_000),
    });
    let text = serde_json::to_string(&ledger).unwrap();
    let restored: trit_host::CacheLedger = serde_json::from_str(&text).unwrap();
    let stat = restored.stat_for("p", "m", "fnv1a64:persist").unwrap();
    assert_eq!(stat.cache_hit_tokens, 80);
    assert_eq!(stat.recent.len(), 1);
}

fn record_cache(
    ledger: &mut trit_host::CacheLedger,
    prefix: &str,
    hit: u64,
    miss: u64,
    estimated: bool,
) {
    ledger.observe(&trit_host::CacheObservation {
        provider: "p".into(),
        model: "m".into(),
        prefix_fingerprint: prefix.into(),
        prompt_tokens: hit + miss,
        cache_hit_tokens: hit,
        cache_miss_tokens: miss,
        estimated,
        observed_at_ms: None,
    });
}

#[test]
fn cache_ledger_lru_evicts_least_recently_used_entry() {
    let mut ledger = trit_host::CacheLedger::new(0, 2);
    record_cache(&mut ledger, "a", 10, 0, false);
    record_cache(&mut ledger, "b", 20, 0, false);
    record_cache(&mut ledger, "a", 10, 0, false);
    record_cache(&mut ledger, "c", 30, 0, false);

    assert_eq!(ledger.len(), 2);
    assert!(ledger.stat_for("p", "m", "a").is_some());
    assert!(ledger.stat_for("p", "m", "c").is_some());
    assert!(ledger.stat_for("p", "m", "b").is_none());
    assert_eq!(
        ledger.hot_entries(2).first().map(|(key, _)| key.as_str()),
        Some("p|m|c")
    );
}

#[test]
fn cache_ledger_ttl_drops_expired_entries_but_keeps_legacy_untimestamped() {
    let mut ledger = trit_host::CacheLedger::new(1, 0);
    record_cache(&mut ledger, "old", 10, 0, false);
    record_cache(&mut ledger, "legacy", 20, 0, false);

    let mut value = serde_json::to_value(&ledger).unwrap();
    value["ttl_secs"] = serde_json::json!(1);
    value["entries"]["p|m|old"]["last_accessed"] = serde_json::json!(1);
    value["entries"]["p|m|legacy"]["last_accessed"] = serde_json::json!(0);
    let mut restored: trit_host::CacheLedger = serde_json::from_value(value).unwrap();

    record_cache(&mut restored, "fresh", 30, 0, false);
    assert!(restored.stat_for("p", "m", "old").is_none());
    assert!(restored.stat_for("p", "m", "legacy").is_some());
    assert!(restored.stat_for("p", "m", "fresh").is_some());
}

#[test]
fn cache_ledger_capacity_repairs_missing_lru_order_after_load() {
    let mut ledger = trit_host::CacheLedger::default();
    record_cache(&mut ledger, "a", 10, 0, false);
    record_cache(&mut ledger, "b", 20, 0, false);
    record_cache(&mut ledger, "c", 30, 0, false);

    let mut value = serde_json::to_value(&ledger).unwrap();
    value.as_object_mut().unwrap().remove("lru_order");
    value["max_entries"] = serde_json::json!(2);
    let mut restored: trit_host::CacheLedger = serde_json::from_value(value).unwrap();

    record_cache(&mut restored, "d", 40, 0, false);
    assert_eq!(restored.len(), 2);
    assert!(restored.stat_for("p", "m", "d").is_some());
    assert!(
        restored.stat_for("p", "m", "a").is_none(),
        "same-second entries must fall back to deterministic key order"
    );
}

#[test]
fn cache_ledger_merge_keeps_limits_and_combines_counts() {
    let mut left = trit_host::CacheLedger::new(0, 2);
    record_cache(&mut left, "a", 10, 5, false);
    record_cache(&mut left, "b", 20, 5, false);
    let mut right = trit_host::CacheLedger::new(0, 2);
    record_cache(&mut right, "a", 30, 5, false);
    record_cache(&mut right, "c", 40, 5, false);

    left.merge(&right);
    assert_eq!(left.len(), 2);
    let stat = left.stat_for("p", "m", "a").unwrap();
    assert_eq!(stat.cache_hit_tokens, 40);
    assert_eq!(stat.cache_miss_tokens, 10);
    assert!(left.stat_for("p", "m", "c").is_some());
}

#[test]
fn cache_ledger_tiers_by_hit_quality_and_confidence() {
    let mut ledger = trit_host::CacheLedger::default();
    for _ in 0..3 {
        record_cache(&mut ledger, "hot", 90, 10, false);
    }
    for _ in 0..2 {
        record_cache(&mut ledger, "warm", 50, 50, false);
    }
    for _ in 0..3 {
        record_cache(&mut ledger, "cold", 10, 90, false);
    }
    record_cache(&mut ledger, "estimated", 100, 0, true);

    let tiers = ledger.tiers();
    assert_eq!(tiers.hot, vec!["p|m|hot".to_owned()]);
    assert_eq!(tiers.warm, vec!["p|m|warm".to_owned()]);
    assert_eq!(tiers.cold.len(), 2);
    assert!(tiers.cold.contains(&"p|m|cold".to_owned()));
    assert!(tiers.cold.contains(&"p|m|estimated".to_owned()));
    assert_eq!(tiers.len(), 4);
}

#[test]
fn cache_ledger_capacity_evicts_cold_before_newer_hot() {
    let mut ledger = trit_host::CacheLedger::new(0, 2);
    for _ in 0..3 {
        record_cache(&mut ledger, "hot", 90, 10, false);
    }
    record_cache(&mut ledger, "cold", 0, 100, true);
    record_cache(&mut ledger, "warm", 50, 50, false);

    assert_eq!(ledger.len(), 2);
    assert!(ledger.stat_for("p", "m", "hot").is_some());
    assert!(ledger.stat_for("p", "m", "warm").is_some());
    assert!(ledger.stat_for("p", "m", "cold").is_none());
    assert_eq!(
        ledger.tiers().hot,
        vec!["p|m|hot".to_owned()],
        "hot entry must survive a newer cold entry"
    );
}

#[test]
fn cache_ledger_compress_retains_confidence_and_respects_budget() {
    let mut ledger = trit_host::CacheLedger::default();
    for _ in 0..3 {
        record_cache(&mut ledger, "high", 80, 20, false);
    }
    record_cache(&mut ledger, "medium", 20, 80, false);
    record_cache(&mut ledger, "none", 100, 0, true);

    let all = ledger.compress(Ternary::Unknown, 0);
    assert_eq!(all.original_token_count, 500);
    assert_eq!(all.retained_keys.len(), 2);
    assert_eq!(all.dropped_keys, vec!["p|m|none".to_owned()]);
    assert!(all.summary.contains("p|m|none"));
    assert_eq!(all.compressed_token_count, 400);

    let budgeted = ledger.compress(Ternary::False, 150);
    assert_eq!(budgeted.original_token_count, 500);
    assert_eq!(budgeted.compressed_token_count, 100);
    assert_eq!(budgeted.retained_keys, vec!["p|m|medium".to_owned()]);
    assert!(budgeted.summary.contains("p|m|high"));
    assert!(budgeted.summary.contains("p|m|none"));
}
