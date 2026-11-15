use trit_agent::{
    AgentError, CacheClass, ContextDocument, ContextKind, ContextPlan, ContextReadTools,
    ContextRef, ContextRequest, ContextScope, ContextSelection, InMemoryContext, ModelProfile,
    ReasoningEffort, ScriptedTools, Sensitivity, ToolExecutor, ToolRequest, assemble_context,
};

fn plan_and_context() -> (ContextPlan, InMemoryContext) {
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
                cache: CacheClass::DynamicTail,
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
    (plan, context)
}

#[test]
fn context_read_expands_a_known_handle_and_records_a_receipt() {
    let (plan, context) = plan_and_context();
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
    let (plan, context) = plan_and_context();
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
    let (plan, context) = plan_and_context();
    let mut tools = ContextReadTools::new(ScriptedTools::new(Vec::new()), plan, context);
    tools
        .execute(&ToolRequest::context_read("r1", "C1"))
        .unwrap();
    assert_eq!(tools.reads().len(), 1);
    tools.begin_turn(1, "next").unwrap();
    assert!(tools.reads().is_empty());
}
