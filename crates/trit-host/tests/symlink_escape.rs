//! 安全回归：checkpoint 快照与 mutation journal 不得跟随父符号链接，
//! 回滚也只能在 workspace 内、经过逐段祖先检查后写入。

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use trit_host::{
    BackupEntry, CheckpointStore, JOURNAL_PATH, JOURNAL_SCHEMA, JournalPhase, JournalRecord,
    MutationJournal, ToolRequest,
};

#[cfg(unix)]
fn link_dir(target: &std::path::Path, link: &std::path::Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[cfg(windows)]
fn link_dir(target: &std::path::Path, link: &std::path::Path) -> bool {
    std::os::windows::fs::symlink_dir(target, link).is_ok()
}

#[cfg(not(any(unix, windows)))]
fn link_dir(_target: &std::path::Path, _link: &std::path::Path) -> bool {
    false
}

fn temp(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("trit-symlink-{tag}-{nanos}"));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn checkpoint_snapshot_refuses_symlinked_parent() {
    let root = temp("ws");
    let outside = temp("outside");
    fs::write(outside.join("secret.txt"), "T0PSECRET-CHECKPOINT").unwrap();
    if !link_dir(&outside, &root.join("link")) {
        eprintln!("skip: symlink creation unavailable");
        return;
    }

    let store_dir = temp("store");
    let store = CheckpointStore::open(&store_dir).unwrap();
    let mut recorder = store.begin(0, "probe");
    let error = recorder
        .snapshot(&root, "link/secret.txt")
        .expect_err("snapshot must refuse a symlinked parent");
    assert!(
        error.to_string().contains("symlink"),
        "unexpected error: {error}"
    );
    recorder.finish().unwrap();

    let saved = fs::read_to_string(store_dir.join("checkpoint-00000000.json")).unwrap();
    assert!(
        !saved.contains("T0PSECRET-CHECKPOINT"),
        "checkpoint leaked an outside file: {saved}"
    );
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
    let _ = fs::remove_dir_all(&store_dir);
}

#[test]
fn journal_backup_skips_symlinked_parent() {
    let root = temp("journal-ws");
    let outside = temp("journal-outside");
    fs::write(outside.join("secret.txt"), "T0PSECRET-JOURNAL").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    if !link_dir(&outside, &root.join("src/link")) {
        eprintln!("skip: symlink creation unavailable");
        return;
    }

    let mut journal = MutationJournal::open(&root).unwrap();
    let request = ToolRequest::write("c1", "write_file", "src/link/secret.txt", "new");
    let entry = journal.record_begin(&request, None).unwrap();
    journal.record_error(entry, &request, "blocked").unwrap();

    let lines = fs::read_to_string(root.join(JOURNAL_PATH)).unwrap();
    let begin = lines
        .lines()
        .filter_map(|line| serde_json::from_str::<JournalRecord>(line).ok())
        .find(|record| record.phase == JournalPhase::Begin)
        .expect("begin record");
    assert_eq!(begin.backups.len(), 1);
    assert!(begin.backups[0].skipped);
    assert!(begin.backups[0].backup.is_none());

    let backups = root.join(".agent/journal-backups");
    if backups.is_dir() {
        for entry in fs::read_dir(&backups).unwrap().filter_map(Result::ok) {
            for file in fs::read_dir(entry.path()).unwrap().filter_map(Result::ok) {
                let text = fs::read_to_string(file.path()).unwrap_or_default();
                assert!(
                    !text.contains("T0PSECRET-JOURNAL"),
                    "journal backup leaked an outside file"
                );
            }
        }
    }
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn journal_rollback_refuses_symlinked_target() {
    let root = temp("rollback-ws");
    let outside = temp("rollback-outside");
    fs::write(outside.join("victim.txt"), "original").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    if !link_dir(&outside, &root.join("src/link")) {
        eprintln!("skip: symlink creation unavailable");
        return;
    }

    let journal = MutationJournal::open(&root).unwrap();
    let backup_dir = root.join(".agent/journal-backups/7");
    fs::create_dir_all(&backup_dir).unwrap();
    fs::write(backup_dir.join("0"), "PWNED").unwrap();

    let record = JournalRecord {
        schema: JOURNAL_SCHEMA,
        seq: 1,
        op_id: 7,
        phase: JournalPhase::Begin,
        tool: "write_file".into(),
        call_id: "c1".into(),
        paths: vec!["src/link/victim.txt".into()],
        lease_epoch: None,
        lease_locks: Vec::new(),
        passed: false,
        in_doubt: true,
        output: String::new(),
        backups: vec![BackupEntry {
            path: "src/link/victim.txt".into(),
            existed: true,
            backup: Some("0".into()),
            skipped: false,
            bytes: 5,
        }],
        at: 1,
    };
    let outcome = journal.rollback(&record);
    assert!(
        !outcome.failed.is_empty(),
        "rollback must refuse symlinked parent: {outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(outside.join("victim.txt")).unwrap(),
        "original"
    );
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn journal_rollback_still_restores_regular_file() {
    let root = temp("rollback-normal");
    fs::write(root.join("a.txt"), "old").unwrap();
    let journal = MutationJournal::open(&root).unwrap();
    let backup_dir = root.join(".agent/journal-backups/9");
    fs::create_dir_all(&backup_dir).unwrap();
    fs::write(backup_dir.join("0"), "restored").unwrap();

    let record = JournalRecord {
        schema: JOURNAL_SCHEMA,
        seq: 1,
        op_id: 9,
        phase: JournalPhase::Begin,
        tool: "write_file".into(),
        call_id: "c1".into(),
        paths: vec!["a.txt".into()],
        lease_epoch: None,
        lease_locks: Vec::new(),
        passed: false,
        in_doubt: true,
        output: String::new(),
        backups: vec![BackupEntry {
            path: "a.txt".into(),
            existed: true,
            backup: Some("0".into()),
            skipped: false,
            bytes: 8,
        }],
        at: 1,
    };
    let outcome = journal.rollback(&record);
    assert_eq!(outcome.restored, vec!["a.txt"]);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "restored");
    let _ = fs::remove_dir_all(&root);
}
