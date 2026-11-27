//! 子智能体协议兼容：旧节点响应（缺新字段）、帧分块/粘包、请求回显校验。
//!
//! 协议要能同时接新旧节点：`model` / `usage` / `dissent` 都是后来加的字段，
//! 旧节点不返回时必须能解码（`None`），而不是整帧失败。反过来，新节点返回的
//! 字段必须原样保留，供成本折算与异议处理。

use trit_orch::{
    AccessMode, ContextCapsule, DelegationStatus, Dissent, SubagentProfile, SubagentRequest,
    SubagentResponse, SubagentUsage, decode_frame, encode_frame,
};

fn request(task: &str, epoch: u64) -> SubagentRequest {
    SubagentRequest {
        task: task.to_owned(),
        epoch,
        capsule: ContextCapsule::default(),
        profile: SubagentProfile {
            name: "worker".into(),
            access: AccessMode::ReadOnly,
            write_roots: Vec::new(),
            tools: vec!["read_file".into()],
            max_tokens: 1_000,
        },
    }
}

fn response(task: &str, epoch: u64) -> SubagentResponse {
    SubagentResponse {
        task: task.into(),
        epoch,
        status: DelegationStatus::Completed,
        content: b"done".to_vec(),
        tokens: 7,
        model: None,
        usage: None,
        dissent: None,
    }
}

#[test]
fn legacy_response_without_new_fields_decodes() {
    // 旧节点只发这五个字段；解析必须成功，新字段为 None。
    // `DelegationStatus` 是 internally tagged（tag = "status"），unit variant 编码成
    // 一个单键对象，而不是裸字符串 "completed"。
    // `SubagentResponse.content` 是 `Vec<u8>`，serde 默认编码成字节数组（不是 base64）。
    let json = r#"{"task":"t1","epoch":3,"status":{"status":"completed"},"content":[100,111,110,101],"tokens":7}"#;
    let decoded: SubagentResponse = serde_json::from_str(json).expect("旧节点响应应可解码");
    assert_eq!(decoded.task, "t1");
    assert_eq!(decoded.epoch, 3);
    assert_eq!(decoded.status, DelegationStatus::Completed);
    assert_eq!(decoded.content, b"done");
    assert_eq!(decoded.tokens, 7);
    assert!(decoded.model.is_none());
    assert!(decoded.usage.is_none());
    assert!(decoded.dissent.is_none());
}

#[test]
fn new_fields_round_trip_through_serde() {
    let mut response = response("t1", 1);
    response.model = Some("deepseek-chat".into());
    response.usage = Some(SubagentUsage {
        fresh: 10,
        cached: 20,
        output: 5,
        estimated: false,
    });
    response.dissent = Some(Dissent {
        target: "task".into(),
        reason: "范围太大".into(),
        suggestion: "拆成两步".into(),
    });

    let json = serde_json::to_string(&response).unwrap();
    let decoded: SubagentResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, response);
    assert_eq!(decoded.model.as_deref(), Some("deepseek-chat"));
    let usage = decoded.usage.unwrap();
    assert_eq!(usage.fresh, 10);
    assert_eq!(usage.cached, 20);
    assert_eq!(usage.output, 5);
    assert_eq!(decoded.dissent.unwrap().suggestion, "拆成两步");
}

#[test]
fn none_fields_are_omitted_not_null() {
    let json = serde_json::to_string(&response("t1", 1)).unwrap();
    assert!(!json.contains("\"model\""), "{json}");
    assert!(!json.contains("\"usage\""), "{json}");
    assert!(!json.contains("\"dissent\""), "{json}");
}

#[test]
fn response_must_echo_task_and_epoch() {
    let request = request("t1", 3);
    response("t1", 3)
        .validate_for(&request)
        .expect("回显一致应通过");

    for (task, epoch) in [("t2", 3u64), ("t1", 4u64)] {
        let error = response(task, epoch)
            .validate_for(&request)
            .expect_err("回显不一致必须失败");
        let message = error.to_string();
        assert!(
            message.contains("does not match request"),
            "{task}@{epoch}: {message}"
        );
    }
}

#[test]
fn frame_decode_waits_for_the_whole_frame() {
    let frame = encode_frame(&response("t1", 1)).unwrap();
    let mut buffer = Vec::new();

    // 空缓冲与半帧都返回 None，不消费任何字节。
    assert!(
        decode_frame::<SubagentResponse>(&mut buffer)
            .unwrap()
            .is_none()
    );
    buffer.extend_from_slice(&frame[..frame.len() / 2]);
    assert!(
        decode_frame::<SubagentResponse>(&mut buffer)
            .unwrap()
            .is_none()
    );

    // 补齐后一次取出，缓冲清空。
    buffer.extend_from_slice(&frame[frame.len() / 2..]);
    let decoded = decode_frame::<SubagentResponse>(&mut buffer)
        .unwrap()
        .expect("完整帧应解码");
    assert_eq!(decoded.task, "t1");
    assert!(buffer.is_empty());
}

#[test]
fn frame_decode_keeps_following_bytes() {
    let mut buffer = encode_frame(&response("t1", 1)).unwrap();
    buffer.extend_from_slice(&encode_frame(&response("t2", 2)).unwrap());

    let first = decode_frame::<SubagentResponse>(&mut buffer)
        .unwrap()
        .expect("第一帧");
    assert_eq!(first.task, "t1");
    let second = decode_frame::<SubagentResponse>(&mut buffer)
        .unwrap()
        .expect("第二帧");
    assert_eq!(second.task, "t2");
    assert!(buffer.is_empty());
}

#[test]
fn corrupt_frame_body_fails_closed() {
    let frame = encode_frame(&response("t1", 1)).unwrap();
    let mut buffer = frame;
    // 把 JSON 主体的第一个字节改坏（跳过 4 字节长度前缀）。
    buffer[4] = b'!';
    let error = decode_frame::<SubagentResponse>(&mut buffer).expect_err("坏帧必须报错");
    assert!(error.to_string().contains("subagent protocol"), "{error}");
}

#[test]
fn oversized_frame_length_is_rejected_before_allocating() {
    let mut buffer = Vec::new();
    // 声明一个不可能的大长度：不能按声明去分配缓冲区。
    buffer.extend_from_slice(&u32::MAX.to_be_bytes());
    let error = decode_frame::<SubagentResponse>(&mut buffer).expect_err("超大长度必须报错");
    assert!(error.to_string().contains("subagent protocol"), "{error}");
}
