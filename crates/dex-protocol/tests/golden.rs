//! Golden wire fixtures.
//!
//! The protocol is encoded with `postcard`, which writes an externally tagged
//! enum as a *variant index* followed by the fields in declaration order. That
//! makes declaration order part of the wire format: reordering a variant, or
//! reordering its fields, changes the bytes without changing any Rust type.
//!
//! These fixtures pin the exact encoding of the shapes a frontend depends on.
//! If one of them changes, the change is visible in a diff rather than in a
//! mysterious decode failure against a running CLI.
//!
//! `dex-cli` asserts the same values, so an incompatible protocol change fails
//! the frontend build instead of surfacing at runtime.

use dex_protocol::*;

fn assert_bytes(name: &str, actual: &[u8], expected: &str) {
    let hex = actual
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(hex, expected, "encoding of {name} changed");
}

fn session(byte: u8) -> SessionId {
    SessionId([byte; 16])
}

#[test]
fn ack_encoding_is_stable() {
    // Ack variant order: CreateSession=0, Accepted=1, Finished=2, Closed=3,
    // Capabilities=4, Ok=5.
    assert_bytes("Ack::Ok", &encode_payload(&Ack::Ok).unwrap(), "05");
    assert_bytes("Ack::Accepted", &encode_payload(&Ack::Accepted).unwrap(), "01");
    assert_bytes("Ack::Closed", &encode_payload(&Ack::Closed).unwrap(), "03");
    assert_bytes(
        "Ack::Finished",
        &encode_payload(&Ack::Finished {
            status: SessionStatus::Completed,
        })
        .unwrap(),
        "02 03",
    );
}

#[test]
fn request_encoding_is_stable() {
    // ClientRequest variant order: CreateSession=0, SendMessage=1, Attach=2,
    // Cancel=3, CloseSession=4, ListCapabilities=5.
    assert_bytes(
        "ClientRequest::ListCapabilities",
        &encode_payload(&RequestFrame::new(RequestId(1), ClientRequest::ListCapabilities)).unwrap(),
        "01 05",
    );
    assert_bytes(
        "ClientRequest::CreateSession (model=None)",
        &encode_payload(&RequestFrame::new(
            RequestId(1),
            ClientRequest::CreateSession {
                working_dir: "/work".into(),
                model: None,
            },
        ))
        .unwrap(),
        "01 00 05 2f 77 6f 72 6b 00",
    );
    assert_bytes(
        "ClientRequest::Cancel",
        &encode_payload(&RequestFrame::new(
            RequestId(7),
            ClientRequest::Cancel {
                session_id: SessionId([
                    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7,
                ]),
            },
        ))
        .unwrap(),
        "07 03 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 07",
    );
}

#[test]
fn event_encoding_is_stable() {
    // Event variant order: SessionStarted=0, SessionFinished=1, UserMessage=2,
    // ModelStarted=3, ModelDelta=4, ProgramStarted=5, ProgramFinished=6,
    // ProgramFailed=7, CapabilityStarted=8, CapabilityOutput=9,
    // CapabilityFinished=10, FileChanged=11, ProcessStarted=12,
    // ProcessOutput=13, ProcessFinished=14, MemoryWrite=15, UiPrompt=16,
    // Answer=17, Error=18.
    assert_bytes(
        "Event::ModelDelta",
        &encode_payload(&EventFrame::new(
            session(0),
            5,
            Event::ModelDelta { text: "hi".into() },
        ))
        .unwrap(),
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 05 04 02 68 69",
    );
    assert_bytes(
        "Event::Answer",
        &encode_payload(&EventFrame::new(
            session(0),
            5,
            Event::Answer {
                text: "done".into(),
            },
        ))
        .unwrap(),
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 05 11 04 64 6f 6e 65",
    );
    assert_bytes(
        "Event::FileChanged",
        &encode_payload(&EventFrame::new(
            session(0),
            5,
            Event::FileChanged {
                path: "a.rs".into(),
                change: FileChange::Modified,
            },
        ))
        .unwrap(),
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 05 0b 04 61 2e 72 73 01",
    );
}

#[test]
fn error_encoding_is_stable() {
    // ErrorKind::Tool=1, CapabilityErrorKind::PermissionDenied=1.
    // ServerResponse variant order: Ack=0, Err=1, Event=2.
    assert_bytes(
        "ServerResponse::Err (PermissionDenied)",
        &encode_payload(&ServerResponse::err(
            RequestId(9),
            ErrorPayload::capability(CapabilityErrorKind::PermissionDenied, "nope"),
        ))
        .unwrap(),
        "01 09 01 01 01 04 6e 6f 70 65",
    );
}
