//! 跨进程写租约：OS 锁互斥与 epoch 递增。
//!
//! fencing 的“epoch 被替换后 `check()` 失败”由 `lease.rs` 的单测用持锁句柄覆盖：
//! Windows 上外部句柄写不进被锁文件，单测不依赖平台差异。

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use trit_host::{LeaseError, OperationLease};

fn root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("trit-lease-{tag}-{}-{nanos}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn lease_is_exclusive_and_increments_epoch() {
    let root = root("exclusive");
    let lease = OperationLease::acquire(&root, "host-a").unwrap();
    assert_eq!(lease.epoch(), 1);
    assert!(lease.check());

    let busy = OperationLease::acquire(&root, "host-b").unwrap_err();
    assert!(matches!(busy, LeaseError::Busy { .. }), "{busy:?}");

    drop(lease);
    let second = OperationLease::acquire(&root, "host-b").unwrap();
    assert_eq!(second.epoch(), 2);
    assert!(second.check());
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn outside_tamper_is_detected_by_check() {
    let root = root("tamper");
    let lease = OperationLease::acquire(&root, "host-a").unwrap();
    assert!(lease.check());
    let path = root.join(trit_host::LEASE_PATH);
    // flock 是 advisory：另一个句柄能改写文件内容；fencing 的责任是发现它。
    fs::write(
        &path,
        br#"{"owner":"host-b","epoch":99,"acquired_at":0,"released":false}"#,
    )
    .unwrap();
    assert!(!lease.check());
    drop(lease);
    let _ = fs::remove_dir_all(root);
}
