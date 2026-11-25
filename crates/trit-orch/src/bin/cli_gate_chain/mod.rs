//! `trit-orch gate-chain` 的 JSON 桥。
//!
//! 输入沿用宿主桥的 stdin JSON：
//! `{"root":".","expected":"owner/repo","task_kind":"agent",
//!  "packet":".agent/handoff/h-001.json","receiver_state_hash":"H","at":0,
//!  "manifest":".agent/alignment.json","changed":[],"changed_known":false,
//!  "strict":true,"executed":[]}`
//!
//! 输出 `gate-chain/1` 报告；退出码与 `alignment` 一致：
//! `0` 全链通过、`1` 有明确拒绝、`3` 有无法判定的门。
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;
use trit_orch::Error;
use trit_orch::alignment::AlignmentOptions;
use trit_orch::gate_chain::{self, AlignmentStep, DeliveryStep, GateChainOptions};

#[derive(Deserialize, Default)]
#[serde(default)]
struct GateChainInput {
    root: String,
    expected: String,
    task_kind: String,
    packet: String,
    receiver_state_hash: String,
    at: u64,
    attach: bool,
    manifest: String,
    changed: Option<Vec<String>>,
    changed_known: bool,
    strict: Option<bool>,
    executed: Option<Vec<String>>,
}

pub(crate) fn run(input: &str) -> Result<(Value, i32), Error> {
    let request: GateChainInput =
        serde_json::from_str(input).map_err(|error| Error::InputParse {
            message: format!("parse gate-chain input: {error}"),
        })?;
    let root = if request.root.trim().is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(request.root.trim())
    };
    let packet = request.packet.trim();
    let delivery = (!packet.is_empty()).then(|| {
        let path = PathBuf::from(packet);
        // 门链里的 `root` 是所有相对路径的基准；交接包也不例外。
        DeliveryStep {
            packet: if path.is_absolute() {
                path
            } else {
                root.join(path)
            },
            receiver_state_hash: request.receiver_state_hash.trim().to_owned(),
            at: request.at,
            attach: request.attach,
        }
    });
    let manifest = if request.manifest.trim().is_empty() {
        PathBuf::from(".agent/alignment.json")
    } else {
        PathBuf::from(request.manifest.trim())
    };
    let options = GateChainOptions {
        root,
        expected_repo: request.expected.trim().to_owned(),
        task_kind: request.task_kind.trim().to_owned(),
        delivery,
        alignment: Some(AlignmentStep {
            manifest,
            options: AlignmentOptions {
                changed: request.changed,
                changed_known: request.changed_known,
                strict: request.strict.unwrap_or(true),
                executed: request.executed,
            },
        }),
    };
    let report = gate_chain::run(&options);
    let exit = report.exit;
    let value = serde_json::to_value(&report).map_err(|error| Error::InputParse {
        message: format!("serialize gate-chain report: {error}"),
    })?;
    Ok((value, exit))
}
