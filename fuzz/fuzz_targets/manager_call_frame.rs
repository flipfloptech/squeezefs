//! Fuzz the **symmetric manager wire** (`src/meta_ship/manager.rs`,
//! design-symmetric-metadata §6.3, `MANAGER_SCHEMA` 1 under
//! `CLUSTER_WIRE_SCHEMA` 5): the `ManagerRequestFrame` an appender ships
//! to the volume's manager (`JoinAppender` / `ExtentGrant` /
//! `ReturnExtents`) and the `ManagerReplyFrame` the manager answers
//! (`Joined` / `Granted` / `Returned` / `Refused`).
//!
//! Threat model: the body arrives on an AUTHENTICATED `cluster_wire`
//! session, but authentication is membership, not trust — a joiner with
//! a bug (or a compromised member) is exactly the peer whose bytes must
//! not crash the node holding the manager lease, and the reply direction
//! is the manager's bytes landing on every appender. The laws (spec §11
//! TEST-4):
//!
//! * **total** — `decode_*` returns or errors, never panics;
//! * **bounded** — a lying length prefix is never an allocation
//!   authority: decode rides bincode's `with_limit(CONTROL_MAX_FRAME_BYTES)`,
//!   and libFuzzer's `-malloc_limit_mb` is the detector;
//! * **round-trip** — whatever decodes re-encodes to bytes that decode
//!   to an equal frame, and the re-encode is canonical (the bytes compare
//!   equal too).
//!
//! Two arms: the raw bytes (the reject ladder), and an `Arbitrary`-built
//! frame encoded then decoded — the constructive mirror that reaches
//! every variant. The mirrors are local: the wire vocabulary must not
//! grow a derive for the fuzzer's sake, and a mirror that falls out of
//! step fails to compile here (a new verb lands with its fuzz arm or not
//! at all).
#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use squeezefs::meta_ship::manager::{
    decode_reply, decode_request, encode_reply, encode_request, ManagerCall, ManagerReply,
    ManagerReplyFrame, ManagerRequestFrame, WireIdentity, MANAGER_SCHEMA,
};

fn check_request(frame: &ManagerRequestFrame) {
    let re = encode_request(frame).expect("an accepted request frame re-encodes");
    let again = decode_request(&re).expect("a re-encoded request frame decodes");
    assert_eq!(&again, frame, "request frame must round-trip");
    assert_eq!(
        encode_request(&again).expect("re-encodes"),
        re,
        "the request encoding is canonical"
    );
}

fn check_reply(frame: &ManagerReplyFrame) {
    let re = encode_reply(frame).expect("an accepted reply frame re-encodes");
    let again = decode_reply(&re).expect("a re-encoded reply frame decodes");
    assert_eq!(&again, frame, "reply frame must round-trip");
    assert_eq!(
        encode_reply(&again).expect("re-encodes"),
        re,
        "the reply encoding is canonical"
    );
}

#[derive(Arbitrary, Debug)]
struct ArbIdentity {
    node_token: u64,
    mount_slot: u32,
    writer_id: u128,
}

impl From<ArbIdentity> for WireIdentity {
    fn from(i: ArbIdentity) -> Self {
        WireIdentity {
            node_token: i.node_token,
            mount_slot: i.mount_slot,
            writer_id: i.writer_id,
        }
    }
}

#[derive(Arbitrary, Debug)]
enum ArbCall {
    JoinAppender {
        identity: ArbIdentity,
        ring_want_bytes: u64,
    },
    ExtentGrant {
        appender_id: u32,
        want: u32,
    },
    ReturnExtents {
        appender_id: u32,
        runs: Vec<(u64, u32)>,
    },
}

impl From<ArbCall> for ManagerCall {
    fn from(c: ArbCall) -> Self {
        match c {
            ArbCall::JoinAppender {
                identity,
                ring_want_bytes,
            } => ManagerCall::JoinAppender {
                identity: identity.into(),
                ring_want_bytes,
            },
            ArbCall::ExtentGrant { appender_id, want } => {
                ManagerCall::ExtentGrant { appender_id, want }
            }
            ArbCall::ReturnExtents { appender_id, runs } => {
                ManagerCall::ReturnExtents { appender_id, runs }
            }
        }
    }
}

#[derive(Arbitrary, Debug)]
enum ArbReply {
    Joined {
        appender_id: u32,
        page_addr: u64,
        ring_segments: Vec<(u64, u64)>,
        grant: Vec<(u64, u32)>,
        already: bool,
    },
    Granted {
        runs: Vec<(u64, u32)>,
    },
    Returned {
        cleared: u64,
        already: u64,
    },
    Refused {
        reason: String,
    },
}

impl From<ArbReply> for ManagerReply {
    fn from(r: ArbReply) -> Self {
        match r {
            ArbReply::Joined {
                appender_id,
                page_addr,
                ring_segments,
                grant,
                already,
            } => ManagerReply::Joined {
                appender_id,
                page_addr,
                ring_segments,
                grant,
                already,
            },
            ArbReply::Granted { runs } => ManagerReply::Granted { runs },
            ArbReply::Returned { cleared, already } => ManagerReply::Returned { cleared, already },
            ArbReply::Refused { reason } => ManagerReply::Refused { reason },
        }
    }
}

#[derive(Arbitrary, Debug)]
struct ArbInput {
    // The raw bytes both decoders see (the reject-ladder arm).
    raw: Vec<u8>,
    // The constructive arm.
    schema_is_current: bool,
    schema: u32,
    request_id: u64,
    call: ArbCall,
    reply: ArbReply,
}

fuzz_target!(|data: &[u8]| {
    // --- arm 1: raw bytes at both decoders ----------------------------------
    if let Ok(frame) = decode_request(data) {
        check_request(&frame);
    }
    if let Ok(frame) = decode_reply(data) {
        check_reply(&frame);
    }

    // --- arm 2: constructive ------------------------------------------------
    let Ok(input) = ArbInput::arbitrary_take_rest(Unstructured::new(data)) else {
        return;
    };
    if let Ok(frame) = decode_request(&input.raw) {
        check_request(&frame);
    }
    if let Ok(frame) = decode_reply(&input.raw) {
        check_reply(&frame);
    }
    let schema = if input.schema_is_current {
        MANAGER_SCHEMA
    } else {
        input.schema
    };
    let request = ManagerRequestFrame {
        schema,
        request_id: input.request_id,
        call: input.call.into(),
    };
    // Past the CONTROL cap the encoder REFUSES (a return of that many
    // runs never rides one frame) — that refusal is the contract, not a
    // failure; below it the frame must round-trip.
    if let Ok(body) = encode_request(&request) {
        let back = decode_request(&body).expect("an encoded request frame decodes");
        assert_eq!(back, request, "request round-trip (constructive)");
    }
    let reply = ManagerReplyFrame {
        schema,
        request_id: input.request_id,
        reply: input.reply.into(),
    };
    if let Ok(body) = encode_reply(&reply) {
        let back = decode_reply(&body).expect("an encoded reply frame decodes");
        assert_eq!(back, reply, "reply round-trip (constructive)");
    }
});
