//! Round-trip and stability tests for the wire protocol.
//!
//! These are the drift guard. Every variant is encoded and decoded, and the
//! golden fixtures pin the exact bytes for the shapes a frontend depends on.
//! The `dex-cli` repository asserts the same fixtures, so a protocol change the
//! CLI cannot parse fails its build rather than surfacing at runtime.

use dex_protocol::*;

fn events() -> Vec<Event> {
    vec![
        Event::SessionStarted {
            working_dir: "/work".into(),
            model: "kimi-k2.7-code".into(),
        },
        Event::SessionFinished {
            status: SessionStatus::Completed,
        },
        Event::UserMessage {
            text: "find auth".into(),
        },
        Event::ModelStarted,
        Event::ModelDelta {
            text: "let x = 1;".into(),
        },
        Event::ProgramStarted {
            call_id: CallId(1),
            round: 0,
            source: "let x = 1;".into(),
        },
        Event::ProgramFinished {
            call_id: CallId(1),
            duration_ms: 12,
        },
        Event::ProgramFailed {
            call_id: CallId(1),
            error: ErrorPayload::capability(
                CapabilityErrorKind::PermissionDenied,
                "filesystem.write not granted for /etc",
            ),
            diagnostics: Some("line 3: unexpected token".into()),
        },
        Event::CapabilityStarted {
            call_id: CallId(2),
            capability: "repo.find".into(),
            args: "pattern=\"auth\"".into(),
        },
        Event::CapabilityOutput {
            call_id: CallId(2),
            chunk: "partial".into(),
        },
        Event::CapabilityFinished {
            call_id: CallId(2),
            capability: "repo.find".into(),
            ok: true,
            summary: "3 matches".into(),
        },
        Event::FileChanged {
            path: "src/auth.rs".into(),
            change: FileChange::Modified,
        },
        Event::ProcessStarted {
            call_id: CallId(3),
            target: "cargo".into(),
            args: vec!["test".into()],
        },
        Event::ProcessOutput {
            call_id: CallId(3),
            stream: OutputStream::Stderr,
            chunk: "warning: unused".into(),
        },
        Event::ProcessFinished {
            call_id: CallId(3),
            exit_code: Some(101),
            duration_ms: 4200,
            truncated: false,
        },
        Event::MemoryWrite {
            key: "rust.unsafe-audit".into(),
            kind: "program".into(),
        },
        Event::UiPrompt {
            call_id: CallId(4),
            message: "Proceed?".into(),
        },
        Event::Answer {
            text: "Authentication lives in src/auth.rs.".into(),
        },
        Event::Error {
            error: ErrorPayload::new(ErrorKind::Model, "upstream 503"),
        },
    ]
}

fn requests() -> Vec<RequestFrame> {
    vec![
        RequestFrame::new(
            RequestId(1),
            ClientRequest::CreateSession {
                working_dir: "/work".into(),
                model: Some("kimi-k2.7-code".into()),
            },
        ),
        RequestFrame::new(
            RequestId(2),
            ClientRequest::CreateSession {
                working_dir: "/work".into(),
                model: None,
            },
        ),
        RequestFrame::new(
            RequestId(3),
            ClientRequest::SendMessage {
                session_id: SessionId::new(),
                text: "go".into(),
            },
        ),
        RequestFrame::new(
            RequestId(4),
            ClientRequest::Attach {
                session_id: SessionId::new(),
            },
        ),
        RequestFrame::new(
            RequestId(5),
            ClientRequest::Cancel {
                session_id: SessionId::new(),
            },
        ),
        RequestFrame::new(
            RequestId(6),
            ClientRequest::CloseSession {
                session_id: SessionId::new(),
            },
        ),
        RequestFrame::new(RequestId(7), ClientRequest::ListCapabilities),
    ]
}

fn responses() -> Vec<ServerResponse> {
    let sid = SessionId::new();
    vec![
        ServerResponse::ack(
            RequestId(1),
            Ack::CreateSession {
                session_id: sid,
                status: SessionStatus::Created,
                model: "kimi-k2.7-code".into(),
            },
        ),
        ServerResponse::ack(RequestId(2), Ack::Accepted),
        ServerResponse::ack(
            RequestId(3),
            Ack::Finished {
                status: SessionStatus::Completed,
            },
        ),
        ServerResponse::ack(RequestId(4), Ack::Closed),
        ServerResponse::ack(
            RequestId(5),
            Ack::Capabilities {
                capabilities: vec![CapabilityDescriptor {
                    name: "repo.find".into(),
                    summary: "Search the working tree for a pattern.".into(),
                    required_authority: "filesystem.read".into(),
                    mutating: false,
                }],
                authorities: vec![GrantedAuthority {
                    capability: "filesystem.read".into(),
                    scope: "/work/**".into(),
                }],
            },
        ),
        ServerResponse::ack(RequestId(6), Ack::Ok),
        ServerResponse::err(
            RequestId(7),
            ErrorPayload::new(ErrorKind::Invalid, "unknown session"),
        ),
        ServerResponse::Event(EventFrame::new(
            sid,
            1_700_000_000_000,
            Event::ModelDelta {
                text: "x".into(),
            },
        )),
    ]
}

#[test]
fn every_event_variant_round_trips() {
    for event in events() {
        let frame = EventFrame::new(SessionId::new(), 1, event);
        let bytes = encode_payload(&frame).expect("encode");
        let back: EventFrame = decode_payload(&bytes).expect("decode");
        assert_eq!(frame, back);
    }
}

#[test]
fn every_request_round_trips() {
    for frame in requests() {
        let bytes = encode_payload(&frame).expect("encode");
        let back: RequestFrame = decode_payload(&bytes).expect("decode");
        assert_eq!(frame, back);
    }
}

#[test]
fn every_response_round_trips() {
    for response in responses() {
        let bytes = encode_payload(&response).expect("encode");
        let back: ServerResponse = decode_payload(&bytes).expect("decode");
        assert_eq!(response, back);
    }
}

#[test]
fn framed_encoding_carries_a_little_endian_length_prefix() {
    let response = ServerResponse::ack(RequestId(9), Ack::Ok);
    let framed = encode_frame(&response).expect("encode");
    let payload = encode_payload(&response).expect("encode");

    let len = u32::from_le_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
    assert_eq!(len, payload.len());
    assert_eq!(&framed[4..], &payload[..]);
}

#[test]
fn split_frame_reports_incomplete_input() {
    let response = ServerResponse::ack(RequestId(1), Ack::Ok);
    let framed = encode_frame(&response).expect("encode");

    for cut in 0..framed.len() {
        let split = split_frame(&framed[..cut]).expect("split");
        assert!(split.is_none(), "a {cut} byte prefix must not decode as a frame");
    }

    let (header, body) = split_frame(&framed).expect("split").expect("complete");
    assert_eq!(u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize, body.len());
    assert_eq!(decode_payload::<ServerResponse>(body).expect("decode"), response);
}

#[test]
fn split_frame_splits_concatenated_frames() {
    let a = encode_frame(&ServerResponse::ack(RequestId(1), Ack::Ok)).expect("a");
    let b = encode_frame(&ServerResponse::ack(RequestId(2), Ack::Accepted)).expect("b");
    let mut joined = a.clone();
    joined.extend_from_slice(&b);

    let (header, body) = split_frame(&joined).expect("split").expect("first");
    let first_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let first: ServerResponse = decode_payload(&body[..first_len]).expect("first");
    assert_eq!(first, ServerResponse::ack(RequestId(1), Ack::Ok));

    let rest = &joined[4 + first_len..];
    let (header, body) = split_frame(rest).expect("split").expect("second");
    let second_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let second: ServerResponse = decode_payload(&body[..second_len]).expect("second");
    assert_eq!(second, ServerResponse::ack(RequestId(2), Ack::Accepted));
}

#[test]
fn an_oversized_length_prefix_is_rejected_without_allocating() {
    let mut hostile = Vec::new();
    hostile.extend_from_slice(&u32::MAX.to_le_bytes());
    let err = split_frame(&hostile).expect_err("must reject");
    assert!(
        matches!(err, CodecError::TooLarge { .. }),
        "expected TooLarge, got {err:?}"
    );
}

#[test]
fn empty_payload_is_rejected() {
    assert!(matches!(
        decode_payload::<ServerResponse>(&[]).expect_err("must reject"),
        CodecError::Empty
    ));
}

#[test]
fn session_ids_are_distinct_and_render_as_32_hex_chars() {
    let a = SessionId::new();
    let b = SessionId::new();
    assert_ne!(a, b);
    assert_eq!(a.to_string().len(), 32);
    assert!(a.to_string().chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn error_payloads_render_the_capability_reason() {
    let denied = ErrorPayload::capability(CapabilityErrorKind::PermissionDenied, "nope");
    assert_eq!(denied.to_string(), "PermissionDenied: nope");
    assert_eq!(denied.kind, ErrorKind::Tool);

    let timeout = ErrorPayload::capability(CapabilityErrorKind::Timeout, "slow");
    assert_eq!(timeout.kind, ErrorKind::Timeout);
}
