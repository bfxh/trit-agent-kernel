//! 技能 / 命令 / hook 的静态注册表（B7.9）。
//!
//! 三块都是**纯数据 + 确定性匹配**，不做 I/O、不发请求：
//!
//! 1. [`SkillIndex`]：技能索引。名字唯一；触发词匹配按“最长触发词优先、平局按名字”
//!    排序，保证同一输入得到同一结果。
//! 2. [`CommandRegistry`]：斜杠命令解析。必须 `/name args...`；未知命令、参数数量
//!    不符、缺 `/` 前缀一律 fail-closed。
//! 3. [`HookRegistry`]：按事件 + 工具名匹配 hook；`matcher` 支持 `*` 通配，
//!    空 pattern 表示全部；`invalidate_on` 与检查点/失效联动——被失效的 hook
//!    不再出现在匹配结果里。
//!
//! 这一层只回答“有哪些、匹配谁、是否失效”，不执行 hook 动作；执行由宿主完成。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::AgentError;

/// 一个技能定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSpec {
    /// 唯一名字。
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// 触发词；输入以其中任一开头即匹配。
    #[serde(default)]
    pub triggers: Vec<String>,
    /// 技能允许调用的工具名（显式白名单）。
    #[serde(default)]
    pub tools: Vec<String>,
    /// 技能是否只读；只读技能的工具白名单必须全部是只读工具。
    #[serde(default)]
    pub read_only: bool,
    /// 注入给模型的指令文本。
    #[serde(default)]
    pub prompt: String,
}

impl SkillSpec {
    fn validate(&self) -> Result<(), AgentError> {
        if self.name.trim().is_empty() {
            return Err(extension("skill name is blank"));
        }
        if self
            .triggers
            .iter()
            .any(|trigger| trigger.trim().is_empty())
        {
            return Err(extension(format!(
                "skill {} has a blank trigger",
                self.name
            )));
        }
        Ok(())
    }
}

/// 技能索引：名字唯一，触发词确定性匹配。
#[derive(Debug, Clone, Default)]
pub struct SkillIndex {
    skills: BTreeMap<String, SkillSpec>,
}

impl SkillIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, skill: SkillSpec) -> Result<(), AgentError> {
        skill.validate()?;
        if self.skills.contains_key(&skill.name) {
            return Err(extension(format!("duplicate skill {}", skill.name)));
        }
        self.skills.insert(skill.name.clone(), skill);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&SkillSpec> {
        self.skills.get(name)
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// 按名字升序的技能列表。
    pub fn skills(&self) -> impl Iterator<Item = &SkillSpec> {
        self.skills.values()
    }

    /// 触发词匹配：返回命中的技能，最长触发词优先，平局按名字升序。
    pub fn match_input(&self, input: &str) -> Vec<&SkillSpec> {
        let input = input.trim();
        let mut hits: Vec<(&SkillSpec, usize)> = Vec::new();
        for skill in self.skills.values() {
            if let Some(len) = skill
                .triggers
                .iter()
                .filter(|trigger| input.starts_with(trigger.as_str()))
                .map(|trigger| trigger.len())
                .max()
            {
                hits.push((skill, len));
            }
        }
        hits.sort_by(|(left, left_len), (right, right_len)| {
            right_len
                .cmp(left_len)
                .then_with(|| left.name.cmp(&right.name))
        });
        hits.into_iter().map(|(skill, _)| skill).collect()
    }
}

/// 一个斜杠命令定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// 命令名（不含 `/`）。
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// 参数名列表；解析出的参数数量必须与之一致。
    #[serde(default)]
    pub args: Vec<String>,
}

/// 一次成功解析的命令。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedCommand {
    pub name: String,
    pub args: Vec<String>,
}

/// 斜杠命令注册表。
#[derive(Debug, Clone, Default)]
pub struct CommandRegistry {
    commands: BTreeMap<String, CommandSpec>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, command: CommandSpec) -> Result<(), AgentError> {
        if command.name.trim().is_empty() {
            return Err(extension("command name is blank"));
        }
        if self.commands.contains_key(&command.name) {
            return Err(extension(format!("duplicate command {}", command.name)));
        }
        self.commands.insert(command.name.clone(), command);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&CommandSpec> {
        self.commands.get(name)
    }

    /// 解析 `/name arg...`。
    ///
    /// 缺 `/` 前缀、未知命令、参数数量不符都返回 `extension` 错误。
    pub fn parse(&self, input: &str) -> Result<ParsedCommand, AgentError> {
        let input = input.trim();
        let Some(rest) = input.strip_prefix('/') else {
            return Err(extension(format!("command must start with '/': {input:?}")));
        };
        let mut parts = rest.split_whitespace();
        let name = parts.next().unwrap_or_default();
        if name.is_empty() {
            return Err(extension("command name is blank"));
        }
        let spec = self
            .commands
            .get(name)
            .ok_or_else(|| extension(format!("unknown command {name}")))?;
        let args: Vec<String> = parts.map(str::to_owned).collect();
        if args.len() != spec.args.len() {
            return Err(extension(format!(
                "command {name} expects {} arguments, got {}",
                spec.args.len(),
                args.len()
            )));
        }
        Ok(ParsedCommand {
            name: name.to_owned(),
            args,
        })
    }
}

/// hook 触发事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    TurnStart,
    TurnEnd,
    PreTool,
    PostTool,
}

/// 一个 hook 定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSpec {
    /// 唯一名字。
    pub name: String,
    /// 触发事件。
    pub event: HookEvent,
    /// 工具名匹配模式：空表示全部；支持 `*` 通配。
    #[serde(default)]
    pub matcher: String,
    /// 这些事件发生时不匹配该 hook。
    #[serde(default)]
    pub invalidate_on: Vec<HookEvent>,
    /// hook 动作描述（由宿主执行）。
    #[serde(default)]
    pub action: String,
}

/// hook 注册表：按事件 + 工具名匹配，支持失效联动。
#[derive(Debug, Clone, Default)]
pub struct HookRegistry {
    hooks: Vec<HookSpec>,
    invalidated: BTreeSet<String>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, hook: HookSpec) -> Result<(), AgentError> {
        if hook.name.trim().is_empty() {
            return Err(extension("hook name is blank"));
        }
        if self.hooks.iter().any(|existing| existing.name == hook.name) {
            return Err(extension(format!("duplicate hook {}", hook.name)));
        }
        self.hooks.push(hook);
        Ok(())
    }

    /// 按注册顺序返回匹配的 hook；已失效的跳过。
    ///
    /// `tool` 为 `None` 表示非工具事件（TurnStart/TurnEnd），此时只按事件匹配。
    pub fn matching(&self, event: HookEvent, tool: Option<&str>) -> Vec<&HookSpec> {
        self.hooks
            .iter()
            .filter(|hook| hook.event == event && !self.invalidated.contains(&hook.name))
            .filter(|hook| match tool {
                Some(tool) => matches_pattern(&hook.matcher, tool),
                None => hook.matcher.trim().is_empty(),
            })
            .collect()
    }

    /// 触发失效：`invalidate_on` 含 `event` 的 hook 被标记失效。
    pub fn invalidate(&mut self, event: HookEvent) {
        for hook in &self.hooks {
            if hook.invalidate_on.contains(&event) {
                self.invalidated.insert(hook.name.clone());
            }
        }
    }

    pub fn is_invalidated(&self, name: &str) -> bool {
        self.invalidated.contains(name)
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}

/// 简单的 `*` 通配匹配；空 pattern 表示全部。
fn matches_pattern(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return true;
    }
    globset::Glob::new(pattern)
        .map(|glob| glob.compile_matcher().is_match(value))
        .unwrap_or(false)
}

fn extension(message: impl Into<String>) -> AgentError {
    AgentError::Extension {
        message: message.into(),
    }
}
