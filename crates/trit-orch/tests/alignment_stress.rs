//! `alignment_gate.py stress` 的 Rust 等价：确定性随机构造有效清单与三类
//! 必然拒绝的变异，验证 gate 没有把“有效”误拒或把“变异”误放。

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use trit_orch::alignment::{AlignmentOptions, check};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn range(&mut self, upper: u64) -> usize {
        (self.next() % upper) as usize
    }
}

fn base(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "trit-alignment-stress-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn write_manifest(root: &Path, document: &Value) {
    fs::write(
        root.join("alignment.json"),
        serde_json::to_vec(document).unwrap(),
    )
    .unwrap();
}

fn valid_document(requirements: usize) -> Value {
    let mut reqs = Vec::new();
    for index in 0..requirements {
        reqs.push(json!({
            "id": format!("R{index}"),
            "text": format!("需求 {index}"),
            "priority": "must",
            "parent": "G1",
            "artifacts": ["A1"],
            "evidence": ["E1"],
            "changes": ["src/a.py"],
        }));
    }
    json!({
        "schema": "alignment/1",
        "goal": {"id": "G1", "text": "压力用例"},
        "requirements": reqs,
        "artifacts": [{"id": "A1", "path": "src/a.py"}],
        "evidence": [{"id": "E1", "path": "tests/test_a.py"}],
        "non_goals": [{"id": "N1", "text": "不碰 vendor", "paths": ["vendor/**"]}],
    })
}

#[test]
fn stress_valid_manifests_align_and_mutations_reject() {
    const CASES: usize = 100;
    let base = base("matrix");
    let mut rng = Rng(20_261_124);
    let mut aligned = 0;
    let mut rejected = 0;
    let mut errors = Vec::new();

    for index in 0..CASES {
        let root = base.join(format!("v{index}"));
        fs::create_dir_all(&root).unwrap();
        write(
            &root,
            "src/a.py",
            &format!("def f():\n    return {index}\n"),
        );
        write(&root, "tests/test_a.py", "def test_a():\n    assert True\n");

        let document = valid_document(1 + rng.range(4));
        write_manifest(&root, &document);
        let report = check(
            &root,
            Path::new("alignment.json"),
            &AlignmentOptions::default(),
        );
        if report.status == "aligned" {
            aligned += 1;
        } else {
            errors.push(format!(
                "valid case {index} unexpectedly {}: {:?}",
                report.status, report.failures
            ));
        }

        let mut mutated = document.clone();
        let mode = rng.range(3);
        let changed = if mode == 1 {
            vec!["misc/x.py".to_owned()]
        } else {
            vec!["src/a.py".to_owned()]
        };
        match mode {
            0 => mutated["requirements"][0]["evidence"] = json!([]),
            1 => mutated["requirements"][0]["changes"] = json!(["unmapped.py"]),
            _ => mutated["non_goals"][0]["paths"] = json!(["src/**"]),
        }
        write_manifest(&root, &mutated);
        let report = check(
            &root,
            Path::new("alignment.json"),
            &AlignmentOptions {
                changed: Some(changed),
                ..AlignmentOptions::default()
            },
        );
        if report.status == "unaligned" {
            rejected += 1;
        } else {
            errors.push(format!(
                "mutated case {index} mode {mode} unexpectedly {}: {:?}",
                report.status, report.failures
            ));
        }
    }

    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(aligned, CASES);
    assert_eq!(rejected, CASES);
    let _ = fs::remove_dir_all(base);
}
