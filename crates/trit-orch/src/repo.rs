//! 仓库身份与任务作用域门：先证明“这是哪个仓”，再谈能不能派活。
//!
//! 三进制主库 `bfxh/trit` 与智能体内核 `bfxh/trit-agent-kernel` 是两类仓。
//! 串仓的根因不是模型不聪明，而是任务开始前没有把 repo identity 变成
//! 可拒绝的门。这里把 identity 写成仓内 `.agent/repo.json`，由
//! `trit-orch repo-gate` 在派发前做一次 fail-closed 判定。

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Error, Ternary};

/// identity 文件 schema。
pub const SCHEMA: &str = "repo-identity/1";
/// identity 文件默认路径。
pub const DEFAULT_PATH: &str = ".agent/repo.json";

/// 仓库在智能体体系里的角色。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RepoRole {
    /// 三进制 / 训练 / 其它工具链主库，不承载智能体运行时。
    TernaryMain,
    /// 智能体内核与编排。
    AgentKernel,
    /// 具体宿主（例如 Minecraft 托管）。
    Host,
    /// 未知角色；不能用于智能体任务。
    Unknown,
}

/// 仓内自报的身份清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoIdentity {
    pub schema: String,
    pub repo: String,
    pub role: RepoRole,
    pub owns_agent_runtime: bool,
    #[serde(default)]
    pub agent_runtime_repo: String,
    #[serde(default)]
    pub notes: String,
}

impl RepoIdentity {
    /// 从 `<root>/.agent/repo.json` 读取；文件缺失或坏 JSON 都返回错误。
    pub fn load(root: &Path) -> Result<Self, Error> {
        let path = root.join(DEFAULT_PATH);
        let bytes = fs::read(&path).map_err(|error| Error::InputParse {
            message: format!("read {}: {error}", path.display()),
        })?;
        serde_json::from_slice(&bytes).map_err(|error| Error::InputParse {
            message: format!("parse {}: {error}", path.display()),
        })
    }

    /// schema、repo、role 三者都必须是可判定的，不能拿空字段冒充身份。
    pub fn is_valid(&self) -> bool {
        self.schema == SCHEMA
            && !self.repo.trim().is_empty()
            && !matches!(self.role, RepoRole::Unknown)
    }
}

/// 一次作用域判定；`identity` 为 `None` 表示身份文件读不出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeDecision {
    pub gate: Ternary,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<RepoIdentity>,
}

impl ScopeDecision {
    fn deny(reason: impl Into<String>) -> Self {
        Self {
            gate: Ternary::False,
            reason: reason.into(),
            identity: None,
        }
    }
}

/// 任务类型是否要求“能承载智能体运行时”。
fn is_agent_task(task_kind: &str) -> bool {
    matches!(
        task_kind.trim().to_ascii_lowercase().as_str(),
        "agent" | "agent-runtime" | "智能体"
    )
}

/// 仓库身份门。
///
/// - 身份文件缺失/坏 JSON：`False`，不赌“大概是这个仓”。
/// - `expected_repo` 非空且与 identity 不一致：`False`。
/// - 智能体任务 + `owns_agent_runtime=false`：`False`。
/// - 其余：`True`。
pub fn gate(root: &Path, expected_repo: &str, task_kind: &str) -> ScopeDecision {
    let identity = match RepoIdentity::load(root) {
        Ok(identity) => identity,
        Err(error) => return ScopeDecision::deny(error.to_string()),
    };
    if !identity.is_valid() {
        return ScopeDecision {
            gate: Ternary::False,
            reason: "repo identity is invalid (schema/repo/role)".to_owned(),
            identity: Some(identity),
        };
    }
    if is_agent_task(task_kind) && !identity.owns_agent_runtime {
        return ScopeDecision {
            gate: Ternary::False,
            reason: format!(
                "repo {} does not own agent runtime; agent runtime is {}",
                identity.repo,
                if identity.agent_runtime_repo.trim().is_empty() {
                    "<unspecified>"
                } else {
                    identity.agent_runtime_repo.trim()
                }
            ),
            identity: Some(identity),
        };
    }
    let expected = expected_repo.trim();
    if !expected.is_empty() && identity.repo != expected {
        return ScopeDecision {
            gate: Ternary::False,
            reason: format!("repo mismatch: expected {expected}, got {}", identity.repo),
            identity: Some(identity),
        };
    }
    ScopeDecision {
        gate: Ternary::True,
        reason: String::new(),
        identity: Some(identity),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_kernel_identity_gate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let decision = gate(&root, "bfxh/trit-agent-kernel", "agent");
        assert_eq!(decision.gate, Ternary::True, "{decision:?}");
    }

    #[test]
    fn wrong_repo_and_non_agent_repo_are_refused() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let decision = gate(&root, "bfxh/trit", "agent");
        assert_eq!(decision.gate, Ternary::False);
        assert!(decision.reason.contains("repo mismatch"));
    }

    #[test]
    fn missing_identity_is_false() {
        let root = std::env::temp_dir().join(format!("trit-repo-gate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let decision = gate(&root, "", "agent");
        assert_eq!(decision.gate, Ternary::False);
        let _ = fs::remove_dir_all(&root);
    }
}
