use std::sync::{Arc, Mutex};

use trit_agent::{
    AgentContext, AgentContextTools, AgentError, CacheClass, ContextDocument, ContextKind,
    ContextPlan, ContextReadTools, ContextRef, ContextRequest, ContextScope, ContextSelection,
    ContextSpan, InMemoryContext, ModelProfile, ReasoningEffort, ScriptedTools, Sensitivity,
    ToolExecutor, ToolRequest, assemble_context,
};

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
