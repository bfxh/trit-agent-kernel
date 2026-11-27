//! 子 agent IPC 契约：帧往返、半包/粘包、上限、引用校验与结局映射。

use trit_orch::{
    AccessMode, CapsuleEntry, ContextCapsule, DelegationStatus, RemoteEndpoint, SubagentProfile,
    SubagentRequest, SubagentResponse, Task, TaskSpec, decode_frame, encode_frame, exit_status,
};

fn task(id: &str, epoch: u64) -> TaskSpec {
    TaskSpec {
        task: Task {
            id: id.into(),
            duration: 1,
            priority: 0,
            deps: Vec::new(),
        },
        epoch,
        ..TaskSpec::default()
    }
}

fn capsule() -> ContextCapsule {
    ContextCapsule::inherit(
        vec![CapsuleEntry {
            key: "goal".into(),
            value: "ship".into(),
        }],
        &["goal"],
        64,
    )
    .unwrap()
}

fn profile() -> SubagentProfile {
    SubagentProfile {
        name: "child".into(),
        access: AccessMode::ReadOnly,
        write_roots: Vec::new(),
        tools: vec!["read_file".into()],
        max_tokens: 100,
    }
}

fn request() -> SubagentRequest {
    SubagentRequest::new(&task("a", 3), capsule(), profile())
}

fn response(task: &str, epoch: u64, status: DelegationStatus) -> SubagentResponse {
    SubagentResponse {
        task: task.into(),
        epoch,
        status,
        content: b"done".to_vec(),
        tokens: 5,
        model: None,
        usage: None,
    }
}

#[test]
fn frames_round_trip() {
    let request = request();
    let frame = encode_frame(&request).unwrap();
    let mut buffer = frame.clone();
    let decoded: Option<SubagentRequest> = decode_frame(&mut buffer).unwrap();
    assert_eq!(decoded.unwrap(), request);
    assert!(buffer.is_empty(), "decoder must consume the frame");
}

#[test]
fn decoder_handles_partial_and_coalesced_frames() {
    let response = response("a", 3, DelegationStatus::Completed);
    let frame = encode_frame(&response).unwrap();

    // 半包：长度前缀只到了一半。
    let mut buffer = frame[..2].to_vec();
    assert!(
        decode_frame::<SubagentResponse>(&mut buffer)
            .unwrap()
            .is_none()
    );

    // 补全后能解出一帧。
    buffer.extend_from_slice(&frame[2..]);
    assert_eq!(
        decode_frame::<SubagentResponse>(&mut buffer)
            .unwrap()
            .unwrap(),
        response
    );

    // 粘包：连续两帧。
    buffer.extend_from_slice(&frame);
    buffer.extend_from_slice(&frame);
    assert!(
        decode_frame::<SubagentResponse>(&mut buffer)
            .unwrap()
            .is_some()
    );
    assert!(
        decode_frame::<SubagentResponse>(&mut buffer)
            .unwrap()
            .is_some()
    );
    assert!(buffer.is_empty());
}

#[test]
fn oversize_frame_is_rejected() {
    let mut buffer = u32::MAX.to_be_bytes().to_vec();
    let error = decode_frame::<SubagentResponse>(&mut buffer).unwrap_err();
    assert_eq!(error.kind(), "subagent_protocol");
}

#[test]
fn malformed_payload_is_rejected() {
    let payload = b"not-json";
    let mut buffer = (payload.len() as u32).to_be_bytes().to_vec();
    buffer.extend_from_slice(payload);
    let error = decode_frame::<SubagentResponse>(&mut buffer).unwrap_err();
    assert_eq!(error.kind(), "subagent_protocol");
}

#[test]
fn response_must_match_request() {
    let request = request();

    let other_task = response("other", 3, DelegationStatus::Completed);
    assert_eq!(
        other_task.validate_for(&request).unwrap_err().kind(),
        "subagent_protocol"
    );

    let stale_epoch = response("a", 9, DelegationStatus::Completed);
    assert_eq!(
        stale_epoch.validate_for(&request).unwrap_err().kind(),
        "subagent_protocol"
    );

    let matched = response("a", 3, DelegationStatus::Completed);
    assert!(matched.validate_for(&request).is_ok());
}

#[test]
fn response_becomes_stable_reference() {
    let response = response("a", 3, DelegationStatus::Completed);
    let result = response.clone().into_delegation_result();
    assert_eq!(result.task, "a");
    assert_eq!(result.epoch, 3);
    assert!(result.reference().starts_with("a@3#"));
    assert!(result.reusable());
    assert_eq!(result.tokens, 5);

    // 同一内容得到同一 digest。
    let again = response.into_delegation_result();
    assert_eq!(again.digest, result.digest);
}

#[test]
fn exit_status_never_claims_completion() {
    assert!(matches!(exit_status(None), DelegationStatus::Aborted));
    for code in [Some(0), Some(1), Some(2), Some(-1)] {
        let status = exit_status(code);
        assert!(
            matches!(status, DelegationStatus::Failed { .. }),
            "{code:?} must not become Completed: {status:?}"
        );
    }
}

#[test]
fn remote_endpoint_requires_https_or_loopback() {
    let endpoint = |url: &str| RemoteEndpoint {
        url: url.into(),
        auth_token: None,
        timeout_secs: 30,
    };
    for url in [
        "https://agents.example.com/run",
        "http://127.0.0.1:8080/run",
        "http://localhost/run",
        "http://[::1]:9000/run",
    ] {
        assert!(endpoint(url).validate().is_ok(), "{url}");
    }
    for url in [
        "http://agents.example.com/run",
        "",
        "ftp://example.com",
        "https://",
        "https://:8080/run",
    ] {
        assert_eq!(
            endpoint(url).validate().unwrap_err().kind(),
            "subagent_protocol",
            "{url}"
        );
    }

    // 认证字段与超时同样 fail-closed。
    let mut endpoint = RemoteEndpoint {
        url: "https://agents.example.com/run".into(),
        auth_token: Some("  ".into()),
        timeout_secs: 30,
    };
    assert_eq!(endpoint.validate().unwrap_err().kind(), "subagent_protocol");
    endpoint.auth_token = Some("token".into());
    assert!(endpoint.validate().is_ok());
    endpoint.timeout_secs = 0;
    assert_eq!(endpoint.validate().unwrap_err().kind(), "subagent_protocol");
}
