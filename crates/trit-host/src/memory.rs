//! 记忆条目与 BM25 历史检索（B7.9）。
//!
//! 设计目标：
//! - **确定性**：同一查询在同一库上总是得到同一排序（分数降序、id 升序）；
//! - **隐私边界**：`Secret` 条目默认不参与检索，必须显式 `allow_secret`；
//! - **无 I/O**：这一层只做内存索引与打分，持久化由宿主负责。
//!
//! BM25 参数固定为 `k1 = 1.2`、`b = 0.75`，与常见实现一致；token 化按
//! “字母数字/下划线连续段 + 小写”，CJK 连续段作为单个 token。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::AgentError;
use crate::context_layers::Sensitivity;

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// 一条记忆。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// 唯一 ID。
    pub id: String,
    /// 可检索文本。
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// 敏感级别；`Secret` 默认不参与检索。
    #[serde(default)]
    pub sensitivity: Sensitivity,
}

/// 一条检索命中。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit<'a> {
    pub entry: &'a MemoryEntry,
    pub score: f64,
}

/// 内存记忆库。
#[derive(Debug, Clone, Default)]
pub struct MemoryStore {
    entries: BTreeMap<String, MemoryEntry>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, entry: MemoryEntry) -> Result<(), AgentError> {
        if entry.id.trim().is_empty() {
            return Err(memory("memory id is blank"));
        }
        if entry.text.trim().is_empty() {
            return Err(memory(format!("memory {} has blank text", entry.id)));
        }
        if self.entries.contains_key(&entry.id) {
            return Err(memory(format!("duplicate memory {}", entry.id)));
        }
        self.entries.insert(entry.id.clone(), entry);
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> bool {
        self.entries.remove(id).is_some()
    }

    pub fn get(&self, id: &str) -> Option<&MemoryEntry> {
        self.entries.get(id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// BM25 检索：分数降序、同分按 id 升序；`limit` 为 0 或空查询返回空。
    pub fn search(&self, query: &str, limit: usize, allow_secret: bool) -> Vec<MemoryHit<'_>> {
        let mut terms = tokenize(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() || limit == 0 {
            return Vec::new();
        }

        let docs: Vec<&MemoryEntry> = self
            .entries
            .values()
            .filter(|entry| allow_secret || entry.sensitivity != Sensitivity::Secret)
            .collect();
        if docs.is_empty() {
            return Vec::new();
        }

        let doc_tokens: Vec<Vec<String>> = docs.iter().map(|entry| tokenize(&entry.text)).collect();
        let total_len: usize = doc_tokens.iter().map(Vec::len).sum();
        let avgdl = total_len as f64 / docs.len() as f64;
        let total_docs = docs.len() as f64;

        let mut hits = Vec::new();
        for (index, entry) in docs.iter().enumerate() {
            let tokens = &doc_tokens[index];
            let dl = tokens.len() as f64;
            let mut score = 0.0f64;
            for term in &terms {
                let tf = tokens.iter().filter(|token| *token == term).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let df = doc_tokens
                    .iter()
                    .filter(|tokens| tokens.iter().any(|token| token == term))
                    .count() as f64;
                let idf = (1.0 + (total_docs - df + 0.5) / (df + 0.5)).ln();
                score += idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avgdl));
            }
            if score > 0.0 {
                hits.push(MemoryHit { entry, score });
            }
        }

        hits.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.entry.id.cmp(&right.entry.id))
        });
        hits.truncate(limit);
        hits
    }
}

/// 字母数字/下划线连续段 + 小写；CJK 连续段作为单个 token。
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn memory(message: impl Into<String>) -> AgentError {
    AgentError::Memory {
        message: message.into(),
    }
}
