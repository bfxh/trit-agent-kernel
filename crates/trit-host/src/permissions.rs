//! 权限策略与 write claim（B7.4 首版）。
//!
//! 这里是可组合的 `ToolExecutor` 包装器：默认不改变 `LocalTools` 的行为。
//! 宿主显式套上 `PermissionGate` / `ClaimingTools` 后才启用逐调用权限和路径 claim。

use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::journal::MutationJournal;
use crate::lease::OperationLease;
use crate::{AgentError, ToolExecutor, ToolOutcome, ToolRequest};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// 写入/命令默认请求审批；没有 approver 时 fail-closed。
    #[default]
    Ask,
    /// 普通写入/命令自动放行；显式 deny / ask 规则仍然生效。
    Auto,
    /// 跳过普通审批；显式 deny / ask 规则仍然生效。
    Yolo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    /// 工具名；`*` 匹配全部。
    pub tool: String,
    /// 可选精确 subject：bash 用 command，文件工具用第一个 path。
    #[serde(default)]
    pub subject: Option<String>,
    pub decision: PermissionDecision,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPolicy {
    pub mode: PermissionMode,
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

impl PermissionPolicy {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            rules: Vec::new(),
        }
    }

    pub fn with_rule(mut self, rule: PermissionRule) -> Self {
        self.rules.push(rule);
        self
    }

    pub fn decide(&self, request: &ToolRequest) -> PermissionDecision {
        let subject = request
            .command
            .as_deref()
            .or_else(|| request.paths.first().map(String::as_str));
        let matches = |rule: &PermissionRule| {
            (rule.tool == "*" || rule.tool == request.tool)
                && rule
                    .subject
                    .as_deref()
                    .is_none_or(|expected| Some(expected) == subject)
        };
        // deny 永远优先于 allow/模式回退。
        for decision in [
            PermissionDecision::Deny,
            PermissionDecision::Ask,
            PermissionDecision::Allow,
        ] {
            if self
                .rules
                .iter()
                .any(|rule| rule.decision == decision && matches(rule))
            {
                return decision;
            }
        }
        // `read_only` 必须是宿主可信元数据；shell 命令的只读属性无法证明，
        // 也绝不允许由模型参数自报，因此 bash 永远不享受只读快路径。
        if request.read_only && request.tool != "bash" {
            return PermissionDecision::Allow;
        }
        match self.mode {
            PermissionMode::Ask => PermissionDecision::Ask,
            PermissionMode::Auto | PermissionMode::Yolo => PermissionDecision::Allow,
        }
    }
}

pub trait PermissionApprover {
    fn approve(&mut self, request: &ToolRequest) -> Result<bool, AgentError>;
}

/// 逐调用权限包装器；`Ask` 没有 approver 或 approver 拒绝时返回 blocked 回执。
pub struct PermissionGate<T> {
    inner: T,
    policy: PermissionPolicy,
    approver: Option<Box<dyn PermissionApprover>>,
}

impl<T> PermissionGate<T> {
    pub fn new(inner: T, policy: PermissionPolicy) -> Self {
        Self {
            inner,
            policy,
            approver: None,
        }
    }

    pub fn with_approver(mut self, approver: impl PermissionApprover + 'static) -> Self {
        self.approver = Some(Box::new(approver));
        self
    }

    pub fn policy(&self) -> &PermissionPolicy {
        &self.policy
    }

    pub fn set_policy(&mut self, policy: PermissionPolicy) {
        self.policy = policy;
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: ToolExecutor> ToolExecutor for PermissionGate<T> {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        match self.policy.decide(request) {
            PermissionDecision::Allow => self.inner.execute(request),
            PermissionDecision::Deny => Ok(blocked(request, "permission denied by policy")),
            PermissionDecision::Ask => {
                let approval = match self.approver.as_mut() {
                    Some(approver) => Some(approver.approve(request)?),
                    None => None,
                };
                match approval {
                    Some(true) => self.inner.execute(request),
                    Some(false) => Ok(blocked(request, "permission request rejected")),
                    None => Ok(blocked(
                        request,
                        "approval required; no interactive approver",
                    )),
                }
            }
        }
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.inner.begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        self.inner.end_turn(complete)
    }
}

/// 任务级写入范围：已声明路径（文件/子树）或 glob 模式。
#[derive(Debug, Clone)]
pub struct WriteClaim {
    root: PathBuf,
    allowed: Vec<PathBuf>,
    patterns: Vec<GlobMatcher>,
}

impl WriteClaim {
    pub fn new(root: impl Into<PathBuf>, paths: Vec<PathBuf>) -> Result<Self, AgentError> {
        let root = std::fs::canonicalize(root.into())
            .map_err(|error| AgentError::Io(format!("write claim root: {error}")))?;
        let mut allowed = Vec::with_capacity(paths.len());
        let mut patterns = Vec::new();
        for path in paths {
            let text = path.to_string_lossy().replace('\\', "/");
            if has_glob(&text) {
                let normalized = normalize_relative(&text)?;
                let matcher = GlobBuilder::new(&normalized)
                    // `*` 不跨目录，跨目录必须显式 `**`。
                    .literal_separator(true)
                    .case_insensitive(cfg!(windows))
                    .build()
                    .map_err(|error| {
                        AgentError::Tool(format!("write claim glob {text:?}: {error}"))
                    })?
                    .compile_matcher();
                patterns.push(matcher);
                continue;
            }
            let absolute = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            let absolute = normalize_absolute(&absolute);
            if !absolute.starts_with(&root) {
                return Err(AgentError::Tool(format!(
                    "write claim path escapes root: {}",
                    absolute.display()
                )));
            }
            allowed.push(absolute);
        }
        allowed.sort();
        allowed.dedup();
        Ok(Self {
            root,
            allowed,
            patterns,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn allowed(&self) -> &[PathBuf] {
        &self.allowed
    }

    /// 声明的 glob 模式数（字面路径不计入）。
    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    pub fn allows_path(&self, raw: &str) -> bool {
        if raw.trim().is_empty() {
            return false;
        }
        let path = self.root.join(raw);
        let path = normalize_absolute(&path);
        if !path.starts_with(&self.root) {
            return false;
        }
        if self.allowed.iter().any(|allowed| path.starts_with(allowed)) {
            return true;
        }
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        self.patterns
            .iter()
            .any(|pattern| pattern.is_match(&relative))
    }

    pub fn allows_request(&self, request: &ToolRequest) -> bool {
        // shell 无法从命令文本证明路径范围；claim 存在时 fail-closed。
        // 这条判断必须在 read_only 快路径之前，否则模型自报 read_only 就能绕过 claim。
        if request.tool == "bash" {
            let Some(command) = request.command.as_deref() else {
                return false;
            };
            // 升级：能提取到写入目标时，要求全部落在 claim 内；提取不到仍 fail closed。
            let targets = crate::tech_stack::shell_write_targets(command);
            return !targets.is_empty() && targets.iter().all(|path| self.allows_path(path));
        }
        if request.read_only {
            return true;
        }
        match request.tool.as_str() {
            "write_file" => {
                !request.paths.is_empty() && request.paths.iter().all(|path| self.allows_path(path))
            }
            // move 的两端都必须被 claim 覆盖：源可读、目标可写。
            "move_path" => {
                request.paths.len() == 2 && request.paths.iter().all(|path| self.allows_path(path))
            }
            _ => true,
        }
    }
}

/// 工具名白名单：只放行 `allowed` 里列出的工具，其余调用直接失败并留下回执。
///
/// 用于 `SubagentProfile.tools` 的执行侧强制：白名单不能只停留在元数据。
pub struct ToolWhitelist<T> {
    inner: T,
    allowed: Vec<String>,
}

impl<T> ToolWhitelist<T> {
    pub fn new(inner: T, allowed: Vec<String>) -> Self {
        Self { inner, allowed }
    }

    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: ToolExecutor> ToolExecutor for ToolWhitelist<T> {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if !self.allowed.iter().any(|tool| tool == &request.tool) {
            return Ok(ToolOutcome {
                call_id: request.call_id.clone(),
                tool: request.tool.clone(),
                passed: false,
                output: format!("tool {} is not allowed by this profile", request.tool),
                ..ToolOutcome::default()
            });
        }
        self.inner.execute(request)
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.inner.begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        self.inner.end_turn(complete)
    }
}

pub struct ClaimingTools<T> {
    inner: T,
    claim: WriteClaim,
    lease: Option<OperationLease>,
    journal: Option<MutationJournal>,
}

/// 写入守卫：路径 claim + 可选跨进程租约 + 可选 mutation journal。
#[derive(Debug)]
pub struct WriteGuard {
    pub claim: WriteClaim,
    pub lease: Option<OperationLease>,
    pub journal: Option<MutationJournal>,
}

/// 把 guard 包到工具外层；没有 guard 时原样装箱。
pub fn wrap_claim<T: ToolExecutor + 'static>(
    tools: T,
    guard: Option<WriteGuard>,
) -> Box<dyn ToolExecutor> {
    match guard {
        Some(guard) => {
            let mut claiming = ClaimingTools::new(tools, guard.claim);
            if let Some(lease) = guard.lease {
                claiming = claiming.with_lease(lease);
            }
            if let Some(journal) = guard.journal {
                claiming = claiming.with_journal(journal);
            }
            Box::new(claiming)
        }
        None => Box::new(tools),
    }
}

impl<T> ClaimingTools<T> {
    pub fn new(inner: T, claim: WriteClaim) -> Self {
        Self {
            inner,
            claim,
            lease: None,
            journal: None,
        }
    }

    /// 绑定跨进程写租约：每次非只读调用前校验 fencing token，失效即 fail-closed。
    pub fn with_lease(mut self, lease: OperationLease) -> Self {
        self.lease = Some(lease);
        self
    }

    /// 绑定 mutation journal：每次非只读调用前后各追加一条记录。
    pub fn with_journal(mut self, journal: MutationJournal) -> Self {
        self.journal = Some(journal);
        self
    }

    pub fn lease(&self) -> Option<&OperationLease> {
        self.lease.as_ref()
    }

    pub fn journal(&self) -> Option<&MutationJournal> {
        self.journal.as_ref()
    }

    pub fn claim(&self) -> &WriteClaim {
        &self.claim
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: ToolExecutor> ToolExecutor for ClaimingTools<T> {
    fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
        if !request.read_only
            && let Some(lease) = self.lease.as_ref()
            && !lease.check()
        {
            return Ok(blocked(request, "write lease is no longer valid"));
        }
        if !self.claim.allows_request(request) {
            return Ok(blocked(
                request,
                "write claim does not allow this path or shell command",
            ));
        }
        let entry = if request.read_only {
            None
        } else {
            match self.journal.as_mut() {
                Some(journal) => {
                    let epoch = self.lease.as_ref().map(|lease| lease.epoch());
                    let locks = self
                        .lease
                        .as_ref()
                        .map(|lease| lease_lock_strings(lease, self.claim.root()))
                        .unwrap_or_default();
                    Some(
                        journal
                            .record_begin_with_lease(request, epoch, &locks)
                            .map_err(|error| AgentError::Io(format!("write journal: {error}")))?,
                    )
                }
                None => None,
            }
        };
        match self.inner.execute(request) {
            Ok(outcome) => {
                if let (Some(journal), Some(entry)) = (self.journal.as_mut(), entry) {
                    journal
                        .record_finish(entry, &outcome)
                        .map_err(|error| AgentError::Io(format!("write journal: {error}")))?;
                }
                Ok(outcome)
            }
            Err(error) => {
                if let (Some(journal), Some(entry)) = (self.journal.as_mut(), entry) {
                    let _ = journal.record_error(entry, request, &error.to_string());
                }
                Err(error)
            }
        }
    }

    fn begin_turn(&mut self, turn: usize, objective: &str) -> Result<(), AgentError> {
        self.inner.begin_turn(turn, objective)
    }

    fn end_turn(&mut self, complete: bool) -> Result<(), AgentError> {
        self.inner.end_turn(complete)
    }
}

fn blocked(request: &ToolRequest, reason: &str) -> ToolOutcome {
    ToolOutcome {
        call_id: request.call_id.clone(),
        tool: request.tool.clone(),
        command: request.command.clone(),
        passed: false,
        assertions: 0,
        written_paths: Vec::new(),
        in_doubt: false,
        output: format!("blocked: {reason}"),
    }
}

/// 把租约锁文件转成 workspace 相对路径，供 journal 记录与恢复重放。
fn lease_lock_strings(lease: &OperationLease, root: &Path) -> Vec<String> {
    lease
        .lock_paths()
        .into_iter()
        .map(|path| match path.strip_prefix(root) {
            Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
            Err(_) => path.to_string_lossy().replace('\\', "/"),
        })
        .collect()
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn has_glob(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

/// 把 glob 模式折成 workspace 相对、`/` 分隔的形式；拒绝绝对路径与 `..` 逃逸。
fn normalize_relative(raw: &str) -> Result<String, AgentError> {
    let mut parts: Vec<String> = Vec::new();
    for component in Path::new(raw).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(AgentError::Tool(format!(
                        "write claim pattern escapes root: {raw}"
                    )));
                }
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            _ => {
                return Err(AgentError::Tool(format!(
                    "write claim pattern must be workspace relative: {raw}"
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(AgentError::Tool(format!(
            "write claim pattern must name a file: {raw}"
        )));
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct FakeTools {
        calls: usize,
    }

    impl FakeTools {
        fn new() -> Self {
            Self { calls: 0 }
        }
    }

    impl ToolExecutor for FakeTools {
        fn execute(&mut self, request: &ToolRequest) -> Result<ToolOutcome, AgentError> {
            self.calls += 1;
            Ok(ToolOutcome {
                call_id: request.call_id.clone(),
                tool: request.tool.clone(),
                command: request.command.clone(),
                passed: true,
                assertions: 0,
                written_paths: request.paths.clone(),
                in_doubt: false,
                output: "ok".into(),
            })
        }
    }

    struct Approve(bool);

    impl PermissionApprover for Approve {
        fn approve(&mut self, _request: &ToolRequest) -> Result<bool, AgentError> {
            Ok(self.0)
        }
    }

    fn write_request() -> ToolRequest {
        ToolRequest::write("call-1", "write_file", "src/a.txt", "x")
    }

    #[test]
    fn deny_rule_beats_yolo_mode() {
        let policy = PermissionPolicy::new(PermissionMode::Yolo).with_rule(PermissionRule {
            tool: "bash".into(),
            subject: None,
            decision: PermissionDecision::Deny,
        });
        let mut gate = PermissionGate::new(FakeTools::new(), policy);
        let outcome = gate
            .execute(&ToolRequest::shell("call-1", "rm -rf build"))
            .unwrap();
        assert!(!outcome.passed);
        assert_eq!(gate.inner().calls, 0);
    }

    #[test]
    fn ask_without_approver_fails_closed() {
        let mut gate = PermissionGate::new(FakeTools::new(), PermissionPolicy::default());
        let outcome = gate.execute(&write_request()).unwrap();
        assert!(!outcome.passed);
        assert!(outcome.output.contains("no interactive approver"));
        assert_eq!(gate.inner().calls, 0);
    }

    #[test]
    fn ask_with_approver_allows_or_rejects() {
        let policy = PermissionPolicy::default();
        let mut allow =
            PermissionGate::new(FakeTools::new(), policy.clone()).with_approver(Approve(true));
        assert!(allow.execute(&write_request()).unwrap().passed);
        assert_eq!(allow.inner().calls, 1);

        let mut deny = PermissionGate::new(FakeTools::new(), policy).with_approver(Approve(false));
        assert!(!deny.execute(&write_request()).unwrap().passed);
        assert_eq!(deny.inner().calls, 0);
    }

    #[test]
    fn read_only_and_auto_writer_fallbacks() {
        let mut ask = PermissionGate::new(FakeTools::new(), PermissionPolicy::default());
        assert!(
            ask.execute(&ToolRequest::read("call-1", "src/a.txt"))
                .unwrap()
                .passed
        );

        let mut auto = PermissionGate::new(
            FakeTools::new(),
            PermissionPolicy::new(PermissionMode::Auto),
        );
        assert!(auto.execute(&write_request()).unwrap().passed);
        assert_eq!(auto.inner().calls, 1);
    }

    #[test]
    fn subject_deny_only_blocks_matching_command() {
        let policy = PermissionPolicy::new(PermissionMode::Yolo).with_rule(PermissionRule {
            tool: "bash".into(),
            subject: Some("rm -rf build".into()),
            decision: PermissionDecision::Deny,
        });
        let mut gate = PermissionGate::new(FakeTools::new(), policy);
        assert!(
            !gate
                .execute(&ToolRequest::shell("call-1", "rm -rf build"))
                .unwrap()
                .passed
        );
        assert!(
            gate.execute(&ToolRequest::shell("call-2", "cargo test"))
                .unwrap()
                .passed
        );
        assert_eq!(gate.inner().calls, 1);
    }

    #[test]
    fn explicit_ask_rule_survives_auto_mode() {
        let policy = PermissionPolicy::new(PermissionMode::Auto).with_rule(PermissionRule {
            tool: "write_file".into(),
            subject: None,
            decision: PermissionDecision::Ask,
        });
        let mut gate = PermissionGate::new(FakeTools::new(), policy);
        let outcome = gate.execute(&write_request()).unwrap();
        assert!(!outcome.passed);
        assert_eq!(gate.inner().calls, 0);
    }

    #[test]
    fn write_claim_limits_paths_and_shell() {
        let root = std::env::temp_dir().join(format!(
            "trit-claim-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let claim = WriteClaim::new(&root, vec![PathBuf::from("src")]).unwrap();
        assert!(claim.allows_path("src/a.txt"));
        assert!(!claim.allows_path("other.txt"));
        assert!(!claim.allows_path("../escape.txt"));

        let mut tools = ClaimingTools::new(FakeTools::new(), claim);
        assert!(
            tools
                .execute(&ToolRequest::write(
                    "call-1",
                    "write_file",
                    "src/a.txt",
                    "x"
                ))
                .unwrap()
                .passed
        );
        assert!(
            !tools
                .execute(&ToolRequest::write(
                    "call-2",
                    "write_file",
                    "other.txt",
                    "x"
                ))
                .unwrap()
                .passed
        );
        assert!(
            !tools
                .execute(&ToolRequest::shell("call-3", "touch other.txt"))
                .unwrap()
                .passed
        );
        assert!(
            tools
                .execute(&ToolRequest::read("call-4", "src/a.txt"))
                .unwrap()
                .passed
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_only_shell_is_not_trusted_by_claim_or_gate() {
        let root = std::env::temp_dir().join(format!(
            "trit-claim-shell-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let claim = WriteClaim::new(&root, vec![PathBuf::from("src")]).unwrap();

        let mut tools = ClaimingTools::new(FakeTools::new(), claim);
        let mut request = ToolRequest::shell("call-1", "echo pwn > hooks/pre-commit");
        request.read_only = true;
        let outcome = tools.execute(&request).unwrap();
        assert!(!outcome.passed, "{outcome:?}");
        assert_eq!(tools.inner().calls, 0, "shell must never reach the tool");

        let mut gate = PermissionGate::new(FakeTools::new(), PermissionPolicy::default());
        let mut request = ToolRequest::shell("call-2", "echo pwn > hooks/pre-commit");
        request.read_only = true;
        let outcome = gate.execute(&request).unwrap();
        assert!(!outcome.passed, "{outcome:?}");
        assert_eq!(gate.inner().calls, 0, "Ask mode must not auto-allow shell");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_claim_accepts_glob_patterns() {
        let root = std::env::temp_dir().join(format!(
            "trit-claim-glob-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let claim = WriteClaim::new(&root, vec![PathBuf::from("src/*.py")]).unwrap();
        assert_eq!(claim.pattern_count(), 1);
        assert!(claim.allows_path("src/a.py"));
        assert!(!claim.allows_path("src/sub/b.py"));
        assert!(!claim.allows_path("other/a.py"));

        let recursive = WriteClaim::new(&root, vec![PathBuf::from("src/**/*.py")]).unwrap();
        assert!(recursive.allows_path("src/a.py"));
        assert!(recursive.allows_path("src/sub/b.py"));
        assert!(!recursive.allows_path("tests/a.py"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_claim_glob_rejects_escape() {
        let root =
            std::env::temp_dir().join(format!("trit-claim-glob-escape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let error = WriteClaim::new(&root, vec![PathBuf::from("../*.py")]).unwrap_err();
        assert!(error.to_string().contains("escapes root"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn write_claim_glob_is_case_insensitive_on_windows() {
        let root =
            std::env::temp_dir().join(format!("trit-claim-glob-case-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let claim = WriteClaim::new(&root, vec![PathBuf::from("SRC/*.PY")]).unwrap();
        assert!(claim.allows_path("src/a.py"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_claim_requires_both_move_endpoints() {
        let root = std::env::temp_dir().join(format!(
            "trit-claim-move-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let claim = WriteClaim::new(&root, vec![PathBuf::from("src")]).unwrap();
        assert!(claim.allows_request(&ToolRequest::move_path("m1", "src/a.txt", "src/b.txt")));
        assert!(!claim.allows_request(&ToolRequest::move_path("m1", "src/a.txt", "other/b.txt")));
        assert!(!claim.allows_request(&ToolRequest::move_path("m1", "other/a.txt", "src/b.txt")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bash_writes_must_stay_inside_the_claim() {
        let root = std::env::temp_dir().join(format!(
            "trit-claim-bash-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let claim = WriteClaim::new(&root, vec![PathBuf::from("out")]).unwrap();
        let bash = |command: &str| ToolRequest {
            call_id: "c".into(),
            tool: "bash".into(),
            read_only: false,
            command: Some(command.into()),
            ..ToolRequest::default()
        };
        assert!(
            claim.allows_request(&bash("echo x > out/a.txt")),
            "claim 内的 shell 写入应放行"
        );
        assert!(
            !claim.allows_request(&bash("echo x > ../evil.txt")),
            "claim 外的 shell 写入必须拒绝"
        );
        assert!(
            !claim.allows_request(&bash("ls -la")),
            "无法证明写入目标时 fail closed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
