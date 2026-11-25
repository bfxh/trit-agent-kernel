use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

pub(crate) fn run(command: &str, input: &Value) -> (i32, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_trit-orch"))
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("binary should start");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let value: Value = serde_json::from_str(stdout.trim()).expect("stdout must be json");
    (output.status.code().unwrap_or(-1), value)
}

#[allow(dead_code)]
pub(crate) fn task(id: &str) -> Value {
    json!({
        "task": {"id": id, "priority": 0, "duration": 1, "deps": []},
        "requirements": {"cpu": 1, "memory": 1},
        "max_tokens": 100,
        "epoch": 0
    })
}

#[allow(dead_code)]
pub(crate) fn observation() -> Value {
    json!({
        "phase": "Pending",
        "epoch": 0,
        "authorization": "True",
        "freshness": "True",
        "evidence": null,
        "usage": {"fresh": 0, "cached": 0, "output": 0}
    })
}

#[allow(dead_code)]
pub(crate) fn snapshot(ids: &[&str]) -> Value {
    let mut observations = serde_json::Map::new();
    for id in ids {
        observations.insert((*id).into(), observation());
    }
    json!({"revision": 1, "observations": observations})
}

#[allow(dead_code)]
pub(crate) fn limits() -> Value {
    json!({"workers": 2, "capacity": {"cpu": 2, "memory": 2}, "tokens": 1000})
}
