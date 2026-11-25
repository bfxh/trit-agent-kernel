//! 记忆库：CRUD、BM25 排序、确定性与隐私边界。

use trit_host::{AgentError, MemoryEntry, MemoryStore, Sensitivity};

fn entry(id: &str, text: &str, sensitivity: Sensitivity) -> MemoryEntry {
    MemoryEntry {
        id: id.into(),
        text: text.into(),
        tags: Vec::new(),
        sensitivity,
    }
}

#[test]
fn memory_crud_rejects_blank_and_duplicate() {
    let mut store = MemoryStore::new();
    assert!(matches!(
        store
            .insert(entry("", "x", Sensitivity::Public))
            .unwrap_err(),
        AgentError::Memory { .. }
    ));
    assert!(matches!(
        store
            .insert(entry("a", "  ", Sensitivity::Public))
            .unwrap_err(),
        AgentError::Memory { .. }
    ));

    store
        .insert(entry("a", "rust ownership", Sensitivity::Public))
        .unwrap();
    assert!(matches!(
        store
            .insert(entry("a", "other", Sensitivity::Public))
            .unwrap_err(),
        AgentError::Memory { .. }
    ));
    assert_eq!(store.len(), 1);
    assert!(store.get("a").is_some());
    assert!(store.remove("a"));
    assert!(!store.remove("a"));
    assert!(store.is_empty());
}

#[test]
fn bm25_ranks_matching_document_first() {
    let mut store = MemoryStore::new();
    store
        .insert(entry(
            "rust",
            "rust ownership and memory safety",
            Sensitivity::Public,
        ))
        .unwrap();
    store
        .insert(entry(
            "python",
            "python garbage collection",
            Sensitivity::Public,
        ))
        .unwrap();
    store
        .insert(entry(
            "go",
            "go channels and goroutines",
            Sensitivity::Public,
        ))
        .unwrap();

    let hits = store.search("memory", 10, false);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry.id, "rust");

    let hits = store.search("garbage", 10, false);
    assert_eq!(hits[0].entry.id, "python");
}

#[test]
fn bm25_prefers_shorter_document_on_equal_term() {
    let mut store = MemoryStore::new();
    store
        .insert(entry("a", "rust", Sensitivity::Public))
        .unwrap();
    store
        .insert(entry(
            "b",
            "rust rust rust extra extra extra",
            Sensitivity::Public,
        ))
        .unwrap();

    let hits = store.search("rust", 10, false);
    assert_eq!(hits[0].entry.id, "a");
}

#[test]
fn bm25_is_deterministic_and_respects_limit() {
    let mut store = MemoryStore::new();
    store
        .insert(entry("b", "same text", Sensitivity::Public))
        .unwrap();
    store
        .insert(entry("a", "same text", Sensitivity::Public))
        .unwrap();

    let first: Vec<&str> = store
        .search("same", 10, false)
        .iter()
        .map(|hit| hit.entry.id.as_str())
        .collect();
    let second: Vec<&str> = store
        .search("same", 10, false)
        .iter()
        .map(|hit| hit.entry.id.as_str())
        .collect();
    assert_eq!(first, second);
    // 同分按 id 升序。
    assert_eq!(first, vec!["a", "b"]);
    assert_eq!(store.search("same", 1, false).len(), 1);
}

#[test]
fn secret_memories_require_explicit_flag() {
    let mut store = MemoryStore::new();
    store
        .insert(entry("public", "rust", Sensitivity::Public))
        .unwrap();
    store
        .insert(entry("secret", "rust internal", Sensitivity::Secret))
        .unwrap();

    let hits = store.search("rust", 10, false);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry.id, "public");

    let hits = store.search("rust", 10, true);
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().any(|hit| hit.entry.id == "secret"));
}

#[test]
fn empty_query_and_empty_store_return_nothing() {
    let store = MemoryStore::new();
    assert!(store.search("rust", 10, false).is_empty());

    let mut store = MemoryStore::new();
    store
        .insert(entry("a", "rust", Sensitivity::Public))
        .unwrap();
    assert!(store.search("", 10, false).is_empty());
    assert!(store.search("   ", 10, false).is_empty());
    assert!(store.search("rust", 0, false).is_empty());
    assert!(store.search("nonexistent", 10, false).is_empty());
}
