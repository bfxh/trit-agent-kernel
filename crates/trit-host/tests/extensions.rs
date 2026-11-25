//! 技能 / 命令 / hook 注册表：确定性匹配与 fail-closed。

use trit_host::{
    AgentError, CommandRegistry, CommandSpec, HookEvent, HookRegistry, HookSpec, SkillIndex,
    SkillSpec,
};

fn skill(name: &str, triggers: &[&str]) -> SkillSpec {
    SkillSpec {
        name: name.into(),
        description: String::new(),
        triggers: triggers.iter().map(|trigger| (*trigger).to_owned()).collect(),
        tools: Vec::new(),
        read_only: true,
        prompt: String::new(),
    }
}

fn command(name: &str, args: &[&str]) -> CommandSpec {
    CommandSpec {
        name: name.into(),
        description: String::new(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
    }
}

fn hook(name: &str, event: HookEvent, matcher: &str) -> HookSpec {
    HookSpec {
        name: name.into(),
        event,
        matcher: matcher.into(),
        invalidate_on: Vec::new(),
        action: String::new(),
    }
}

fn is_extension(error: &AgentError) -> bool {
    matches!(error, AgentError::Extension { .. })
}

#[test]
fn skills_reject_blank_and_duplicate() {
    let mut index = SkillIndex::new();
    assert!(is_extension(&index.register(skill("", &[])).unwrap_err()));
    assert!(is_extension(&index.register(skill("a", &[" "])).unwrap_err()));

    index.register(skill("a", &["x"])).unwrap();
    assert!(is_extension(&index.register(skill("a", &["y"])).unwrap_err()));
    assert_eq!(index.len(), 1);
    assert!(index.get("a").is_some());
}

#[test]
fn skill_triggers_prefer_longest_then_name() {
    let mut index = SkillIndex::new();
    index.register(skill("a", &["rev"])).unwrap();
    index.register(skill("b", &["review"])).unwrap();
    index.register(skill("c", &["review "])).unwrap();

    let hits: Vec<&str> = index
        .match_input("review src/lib.rs")
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    assert_eq!(hits, vec!["c", "b", "a"]);

    assert!(index.match_input("deploy now").is_empty());
}

#[test]
fn commands_parse_and_reject_bad_input() {
    let mut registry = CommandRegistry::new();
    registry.register(command("review", &["path"])).unwrap();
    registry.register(command("clear", &[])).unwrap();

    let parsed = registry.parse("/review src/lib.rs").unwrap();
    assert_eq!(parsed.name, "review");
    assert_eq!(parsed.args, vec!["src/lib.rs"]);

    assert!(registry.parse("/clear").unwrap().args.is_empty());
    assert!(is_extension(&registry.parse("review src").unwrap_err()));
    assert!(is_extension(&registry.parse("/unknown").unwrap_err()));
    assert!(is_extension(&registry.parse("/review").unwrap_err()));
    assert!(is_extension(&registry.parse("/").unwrap_err()));
    assert!(is_extension(&registry.register(command("review", &[])).unwrap_err()));
}

#[test]
fn hooks_match_event_and_tool_with_wildcards() {
    let mut registry = HookRegistry::new();
    registry.register(hook("all", HookEvent::PreTool, "")).unwrap();
    registry.register(hook("write", HookEvent::PreTool, "write_*")).unwrap();
    registry.register(hook("turn", HookEvent::TurnStart, "")).unwrap();

    let pre_write: Vec<&str> = registry
        .matching(HookEvent::PreTool, Some("write_file"))
        .iter()
        .map(|hook| hook.name.as_str())
        .collect();
    assert_eq!(pre_write, vec!["all", "write"]);

    let pre_read: Vec<&str> = registry
        .matching(HookEvent::PreTool, Some("read_file"))
        .iter()
        .map(|hook| hook.name.as_str())
        .collect();
    assert_eq!(pre_read, vec!["all"]);

    // 非工具事件只匹配空 matcher。
    let turn: Vec<&str> = registry
        .matching(HookEvent::TurnStart, None)
        .iter()
        .map(|hook| hook.name.as_str())
        .collect();
    assert_eq!(turn, vec!["turn"]);

    assert!(is_extension(&registry.register(hook("all", HookEvent::TurnEnd, "")).unwrap_err()));
}

#[test]
fn hooks_invalidate_on_checkpoint_events() {
    let mut registry = HookRegistry::new();
    let mut lint = hook("lint", HookEvent::PostTool, "write_*");
    lint.invalidate_on = vec![HookEvent::TurnEnd];
    registry.register(lint).unwrap();
    registry.register(hook("audit", HookEvent::PostTool, "")).unwrap();

    assert_eq!(
        registry.matching(HookEvent::PostTool, Some("write_file")).len(),
        2
    );
    registry.invalidate(HookEvent::TurnEnd);
    let remaining: Vec<&str> = registry
        .matching(HookEvent::PostTool, Some("write_file"))
        .iter()
        .map(|hook| hook.name.as_str())
        .collect();
    assert_eq!(remaining, vec!["audit"]);
    assert!(registry.is_invalidated("lint"));
    assert!(!registry.is_invalidated("audit"));
}
