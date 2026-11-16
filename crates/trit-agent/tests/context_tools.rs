use std::sync::{Arc, Mutex};

use trit_agent::{
    AgentContext, AgentContextPlanner, AgentContextTools, AgentError, CacheClass, CacheLedger,
    CacheObservation, CachePressurePolicy, ContextDocument, ContextKind, ContextPlan,
    ContextPlanner, ContextReadTools, ContextRef, ContextRequest, ContextScope, ContextSelection,
    ContextSpan, InMemoryContext, LowHitPolicy, ModelProfile, ReasoningEffort, ScriptedTools,
    Sensitivity, ToolExecutor, ToolRequest, TurnContext, assemble_context,
};
use trit_orch::{TaskBrief, Ternary, TokenUsage};

fn plan_and_context() -> (ContextPlan, InMemoryContext, ContextRequest) {
    let mut context = InMemoryContext::default();
    context
        .insert(
            ContextRef {
                id: "doc".into(),
                label: "doc".into(),
                kind: ContextKind::Doc,
                scope: ContextScope::Project,
                project: Some("p1".into()),
                session: None,
                revision: "rev1".into(),
                content_hash: Some("hash".into()),
                sensitivity: Sensitivity::Internal,
                cache: CacheClass::StablePrefix,
            },
            ContextDocument {
                id: "doc".into(),
                full: "full context body".into(),
                summary: None,
                tokens: Some(10),
            },
        )
        .unwrap();
    let request = ContextRequest {
        project: "p1".into(),
        session: "s1".into(),
        query: "q".into(),
        selections: vec![ContextSelection {
            id: "doc".into(),
            required: true,
            score: None,
            shared_opt_in: false,
        }],
        budget_tokens: 1_000,
        model: ModelProfile {
            name: "model".into(),
            provider: "provider".into(),
            context_window: 4_000,
            max_output_tokens: 100,
            supports_tools: true,
            supports_context_read: true,
            supports_reasoning: false,
        },
        reasoning: ReasoningEffort::Off,
        allow_secret: false,
        autonomy: Default::default(),
        priors: Default::default(),
        calibration: None,
    };
    let plan = assemble_context(&request, &context).unwrap();
    (plan, context, request)
}

#[test]
fn context_read_expands_a_known_handle_and_records_a_receipt() {
    let (plan, context, _) = plan_and_context();
    let mut tools = ContextReadTools::new(ScriptedTools::new(Vec::new()), plan, context);
    let outcome = tools
        .execute(&ToolRequest::context_read("r1", "C1"))
        .unwrap();
    assert!(outcome.passed);
    assert_eq!(outcome.output, "full context body");
    assert_eq!(tools.reads().len(), 1);
    assert_eq!(tools.reads()[0].handle, "C1");
    assert_eq!(tools.reads()[0].id, "doc");
}

#[test]
fn context_read_rejects_unknown_handles_and_truncates() {
    let (plan, context, _) = plan_and_context();
    let mut tools =
        ContextReadTools::new(ScriptedTools::new(Vec::new()), plan, context).with_max_chars(5);
    let error = tools
        .execute(&ToolRequest::context_read("r1", "C9"))
        .unwrap_err();
    assert!(
        matches!(error, AgentError::Tool(message) if message.contains("unknown context handle"))
    );

    let outcome = tools
        .execute(&ToolRequest::context_read("r2", "C1"))
        .unwrap();
    assert!(outcome.output.ends_with("...(truncated)"));
}

#[test]
fn context_read_resets_reads_per_turn() {
    let (plan, context, _) = plan_and_context();
    let mut tools = ContextReadTools::new(ScriptedTools::new(Vec::new()), plan, context);
    tools
        .execute(&ToolRequest::context_read("r1", "C1"))
        .unwrap();
    assert_eq!(tools.reads().len(), 1);
    tools.begin_turn(1, "next").unwrap();
    assert!(tools.reads().is_empty());
}

#[test]
fn agent_can_search_add_and_read_dynamic_context() {
    let (plan, mut context, request) = plan_and_context();
    for (id, cache) in [
        ("other", CacheClass::DynamicTail),
        ("stable", CacheClass::StablePrefix),
    ] {
        context
            .insert(
                ContextRef {
                    id: id.into(),
                    label: format!("label-{id}"),
                    kind: ContextKind::Doc,
                    scope: ContextScope::Project,
                    project: Some("p1".into()),
                    session: None,
                    revision: "rev1".into(),
                    content_hash: Some(format!("hash-{id}")),
                    sensitivity: Sensitivity::Internal,
                    cache,
                },
                ContextDocument {
                    id: id.into(),
                    full: format!("{id} body"),
                    summary: None,
                    tokens: Some(10),
                },
            )
            .unwrap();
    }
    let initial_prefix = plan.cache.prefix_fingerprint.clone();
    let shared = Arc::new(Mutex::new(AgentContext::new(request, context, plan)));
    let mut tools = AgentContextTools::new(ScriptedTools::new(Vec::new()), Arc::clone(&shared));

    let search = tools
        .execute(&ToolRequest::context_search("s1", "other", Some(5)))
        .unwrap();
    assert!(search.output.contains("other"));
    let add = tools
        .execute(&ToolRequest::context_add("a1", "other"))
        .unwrap();
    assert!(add.output.starts_with("added C"));
    let handle = tools.adds()[0].handle.clone();
    let read = tools
        .execute(&ToolRequest::context_read("r1", &handle))
        .unwrap();
    assert_eq!(read.output, "other body");

    let stable = tools
        .execute(&ToolRequest::context_add("a2", "stable"))
        .unwrap();
    assert!(stable.output.starts_with("added C"));
    let plan = shared.lock().unwrap().plan().clone();
    assert_eq!(plan.cache.prefix_fingerprint, initial_prefix);
    assert_eq!(plan.cache.dynamic_items, 2);
    assert!(
        plan.degradations
            .contains(&trit_agent::ContextDegradation::DynamicPrefixOverride)
    );
    let receipt = plan.inclusion_receipt();
    assert!(receipt.handles.contains(&handle));
}

#[test]
fn agent_context_denies_add_when_autonomy_is_not_allowed() {
    let (plan, context, mut request) = plan_and_context();
    request.autonomy.may_add_optional = trit_orch::Ternary::False;
    let mut context = AgentContext::new(request, context, plan);
    let error = context.add("doc").unwrap_err();
    assert!(matches!(
        error,
        trit_agent::ContextError::AlreadyIncluded(_)
    ));
    let error = context.add("missing").unwrap_err();
    assert!(matches!(error, trit_agent::ContextError::AutonomyDenied(_)));
}

#[test]
fn context_read_supports_spans_and_per_turn_budget() {
    let (plan, context, _) = plan_and_context();
    let mut tools =
        ContextReadTools::new(ScriptedTools::new(Vec::new()), plan, context).with_read_budget(7);
    let span = ContextSpan {
        offset: 5,
        length: 7,
    };
    let outcome = tools
        .execute(&ToolRequest::context_read_span("r1", "C1", span))
        .unwrap();
    assert_eq!(outcome.output, "context");
    assert_eq!(tools.reads()[0].span, Some(span));

    let error = tools
        .execute(&ToolRequest::context_read("r2", "C1"))
        .unwrap_err();
    assert!(matches!(error, AgentError::Tool(message) if message.contains("budget exhausted")));
}

fn brief() -> TaskBrief {
    TaskBrief {
        task: "t".into(),
        epoch: 0,
        guidance: Default::default(),
        resolves: Vec::new(),
        produces: Default::default(),
        effects: Vec::new(),
        requirements: Default::default(),
        max_tokens: 0,
        admission: Ternary::True,
        granted_tokens: 0,
        blockers: Vec::new(),
        obligations: Vec::new(),
    }
}

fn turn_context() -> TurnContext {
    TurnContext {
        task: "t".into(),
        step: 0,
        usage: TokenUsage::default(),
        receipts: Vec::new(),
        digest: None,
    }
}

#[test]
fn planner_calibrates_hit_potential_from_actual_cache_ledger() {
    let (plan, context, request) = plan_and_context();
    let fingerprint = plan.cache.prefix_fingerprint.clone();
    assert!(!fingerprint.is_empty());
    let shared = Arc::new(Mutex::new(AgentContext::new(request, context, plan)));

    let ledger = Arc::new(Mutex::new(CacheLedger::default()));
    {
        let mut ledger = ledger.lock().unwrap();
        for _ in 0..3 {
            ledger.observe(&CacheObservation {
                provider: "provider".into(),
                model: "model".into(),
                prefix_fingerprint: fingerprint.clone(),
                prompt_tokens: 100,
                cache_hit_tokens: 80,
                cache_miss_tokens: 20,
                estimated: false,
                observed_at_ms: None,
            });
        }
    }
    let mut planner = AgentContextPlanner::new(Arc::clone(&shared))
        .with_cache_ledger(Arc::clone(&ledger))
        .with_hit_threshold_bp(5_000);
    let plan = planner.plan(&brief(), &turn_context()).unwrap().unwrap();
    assert_eq!(plan.cache.hit_potential, Ternary::True);
    assert_eq!(plan.cache.observed_hit_rate_bp, Some(8_000));
    assert_eq!(plan.cache.calibration, Ternary::True);

    let low = Arc::new(Mutex::new(CacheLedger::default()));
    {
        let mut low = low.lock().unwrap();
        for _ in 0..3 {
            low.observe(&CacheObservation {
                provider: "provider".into(),
                model: "model".into(),
                prefix_fingerprint: fingerprint.clone(),
                prompt_tokens: 100,
                cache_hit_tokens: 20,
                cache_miss_tokens: 80,
                estimated: false,
                observed_at_ms: None,
            });
        }
    }
    let mut planner = AgentContextPlanner::new(Arc::clone(&shared))
        .with_cache_ledger(Arc::clone(&low))
        .with_hit_threshold_bp(5_000);
    let plan = planner.plan(&brief(), &turn_context()).unwrap().unwrap();
    assert_eq!(plan.cache.hit_potential, Ternary::False);
    assert_eq!(plan.cache.observed_hit_rate_bp, Some(2_000));
    assert_eq!(plan.cache.calibration, Ternary::True);
    assert_eq!(plan.cache.prefix_items, 1, "keep policy preserves prefix");
    assert_eq!(
        shared.lock().unwrap().dynamic_budget_scale_bp(),
        10_000,
        "pressure policy is opt-in"
    );

    let mut planner = AgentContextPlanner::new(Arc::clone(&shared))
        .with_cache_ledger(Arc::clone(&low))
        .with_hit_threshold_bp(5_000)
        .with_low_hit_policy(LowHitPolicy::DemotePrefix);
    let plan = planner.plan(&brief(), &turn_context()).unwrap().unwrap();
    assert_eq!(plan.cache.hit_potential, Ternary::False);
    assert_eq!(plan.cache.prefix_items, 0);
    assert!(plan.cache.prefix_fingerprint.is_empty());

    let mut planner = AgentContextPlanner::new(Arc::clone(&shared))
        .with_cache_ledger(Arc::clone(&low))
        .with_hit_threshold_bp(5_000)
        .with_cache_pressure_policy(CachePressurePolicy {
            dynamic_budget_scale_bp: 5_000,
            search_limit_scale_bp: 5_000,
            search_order: trit_agent::SearchOrder::PreferDynamic,
            task_selection: trit_orch::SelectionPressure::LowCacheHit,
        });
    let plan = planner.plan(&brief(), &turn_context()).unwrap().unwrap();
    assert_eq!(shared.lock().unwrap().dynamic_budget_scale_bp(), 5_000);
    assert_eq!(shared.lock().unwrap().search_limit_scale_bp(), 5_000);
    assert_eq!(
        shared.lock().unwrap().search_order(),
        trit_agent::SearchOrder::PreferDynamic
    );
    assert_eq!(
        plan.cache.task_selection,
        trit_orch::SelectionPressure::LowCacheHit
    );
    let mut planner = AgentContextPlanner::new(Arc::clone(&shared))
        .with_cache_ledger(ledger)
        .with_hit_threshold_bp(5_000);
    let plan = planner.plan(&brief(), &turn_context()).unwrap().unwrap();
    assert_eq!(shared.lock().unwrap().dynamic_budget_scale_bp(), 10_000);
    assert_eq!(shared.lock().unwrap().search_limit_scale_bp(), 10_000);
    assert_eq!(
        shared.lock().unwrap().search_order(),
        trit_agent::SearchOrder::Resolver
    );
    assert_eq!(
        plan.cache.task_selection,
        trit_orch::SelectionPressure::Normal
    );
}

#[test]
fn cache_pressure_scales_search_candidates() {
    let (plan, mut context, request) = plan_and_context();
    for index in 0..10 {
        context
            .insert(
                ContextRef {
                    id: format!("doc-{index}"),
                    label: format!("doc-{index}"),
                    kind: ContextKind::Doc,
                    scope: ContextScope::Project,
                    project: Some("p1".into()),
                    session: None,
                    revision: "rev1".into(),
                    content_hash: None,
                    sensitivity: Sensitivity::Internal,
                    cache: CacheClass::DynamicTail,
                },
                ContextDocument {
                    id: format!("doc-{index}"),
                    full: "x".into(),
                    summary: None,
                    tokens: Some(1),
                },
            )
            .unwrap();
    }
    let mut agent = AgentContext::new(request, context, plan);
    assert_eq!(agent.search("doc", 10).len(), 10);
    agent.set_search_limit_scale_bp(5_000);
    assert_eq!(agent.search("doc", 10).len(), 5);
    agent.set_search_limit_scale_bp(10_000);
    assert_eq!(agent.search("doc", 10).len(), 10);
    agent.set_search_order(trit_agent::SearchOrder::PreferDynamic);
    let ordered = agent.search("doc", 10);
    assert_eq!(
        ordered.last().map(|reference| reference.id.as_str()),
        Some("doc")
    );
}
