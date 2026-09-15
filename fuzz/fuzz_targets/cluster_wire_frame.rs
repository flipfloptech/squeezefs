//! Fuzz the **cluster wire's RPC vocabulary and its authenticated
//! framing** (`src/cluster_wire.rs`, DLM S3) — the ONE transport every
//! distributed plane rides: S4 lock verbs, S6 membership, S8 shipped
//! metadata, S9 custody + publish, S11 range custody.
//!
//! `job_wire_frame` drives the same `read_plain_frame` reader with the
//! job-shard `WireFrame` vocabulary; this target adds what 1.2 layered on
//! top of it:
//!
//! * the **`RpcFrame`** enum (challenge / prove / admitted / refused /
//!   call / reply) under all three class caps — the handshake class is
//!   what an UNAUTHENTICATED peer gets to spend against a listener that
//!   defaults to `0.0.0.0`;
//! * the **per-frame MAC path** (`FrameTx::send` / `FrameRx::recv`): a
//!   frame that verifies must decode to what was sent, and a tampered,
//!   replayed or reflected frame must fail verification — never be
//!   skipped, never panic;
//! * the **S8 verb bodies** an authenticated `Call`/`Reply` carries
//!   (`meta_ship::wire::{decode_request, decode_reply, decode_reclaim}`)
//!   — bounded bincode over a slice, total, round-tripping;
//! * `hex_decode` / `mac_eq`, the enrollment proof's helpers.
//!
//! Bounded allocation is the standing law (a lying length prefix is never
//! an allocation authority — the `kv_bset_record_count` precedent);
//! libFuzzer's malloc limit is the detector.
#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use squeezefs::cluster_wire::{
    hex_decode, hex_encode, mac_eq, read_plain_frame, session_framers, session_key,
    write_plain_frame, FrameClass, Role, RpcFrame, MAC_BYTES,
};
use squeezefs::meta_backend::crossvol_tx::{IntentRecord, XvOp, XvStep};
use squeezefs::meta_ship::wire::{
    decode_reclaim, decode_reply, decode_request, encode_reclaim, encode_reply, encode_request,
    MetaCall, MetaOp, MetaRequestFrame, META_SHIP_SCHEMA,
};

/// `RpcFrame` carries no `PartialEq` (it holds nothing the daemon ever
/// compares), so equality here is the CANONICAL encoding's: bincode
/// varints make `encode ∘ decode ∘ encode = encode`.
fn plain_bytes(frame: &RpcFrame, class: FrameClass) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    write_plain_frame(&mut out, class, frame).ok()?;
    Some(out)
}

fn check_plain_round_trip(frame: &RpcFrame, class: FrameClass) {
    // A frame that decoded under `class`'s cap re-encodes under it (the
    // encoder refuses only PAST the cap, and the body was inside it).
    let bytes = plain_bytes(frame, class).expect("an accepted frame re-encodes under its class");
    let mut cur = std::io::Cursor::new(&bytes[..]);
    let again: RpcFrame = read_plain_frame(&mut cur, class.cap(), None)
        .expect("a re-encoded frame reads")
        .expect("a re-encoded frame is one whole frame");
    assert_eq!(
        plain_bytes(&again, class).expect("re-encodes"),
        bytes,
        "the plain frame encoding is canonical"
    );
    assert_eq!(
        cur.position() as usize,
        bytes.len(),
        "a frame consumes exactly itself"
    );
}

fn check_authenticated(frame: &RpcFrame, class: FrameClass, secret: &[u8], nonce: &str) {
    let key = session_key(secret, "fuzz-peer", nonce, "peer-nonce", Some(secret));
    let (mut c_tx, mut c_rx) = session_framers(&key, Role::Coordinator);
    let (mut p_tx, mut p_rx) = session_framers(&key, Role::Peer);

    // Coordinator → peer verifies and decodes to the same frame.
    let mut wire = Vec::new();
    c_tx.send(&mut wire, class, frame)
        .expect("an accepted frame sends");
    assert_eq!(c_tx.frames(), 1);
    let mut cur = std::io::Cursor::new(&wire[..]);
    let got: RpcFrame = p_rx
        .recv(&mut cur, class.cap(), None)
        .expect("a genuine frame verifies")
        .expect("one whole frame");
    assert_eq!(p_rx.frames(), 1);
    assert_eq!(
        plain_bytes(&got, class),
        plain_bytes(frame, class),
        "the authenticated path decodes what was sent"
    );

    // Reflection: the coordinator's own receiver must refuse its own frame
    // (the direction byte is inside the MAC).
    let mut cur = std::io::Cursor::new(&wire[..]);
    assert!(
        c_rx.recv::<_, RpcFrame>(&mut cur, class.cap(), None)
            .is_err(),
        "a reflected frame must fail verification"
    );
    assert_eq!(
        c_rx.frames(),
        0,
        "a failed verification never advances the sequence"
    );

    // Replay: the peer already consumed sequence 0; the same bytes again
    // are sequence 1's slot and must fail.
    let mut cur = std::io::Cursor::new(&wire[..]);
    assert!(
        p_rx.recv::<_, RpcFrame>(&mut cur, class.cap(), None)
            .is_err(),
        "a replayed frame must fail verification"
    );

    // Tamper: flip one bit anywhere in the body or the tag.
    if wire.len() > 4 {
        let pos = 4 + (secret.first().copied().unwrap_or(0) as usize % (wire.len() - 4));
        let mut tampered = wire.clone();
        tampered[pos] ^= 0x01;
        let (_, mut fresh_rx) = session_framers(&key, Role::Peer);
        let mut cur = std::io::Cursor::new(&tampered[..]);
        let r = fresh_rx.recv::<_, RpcFrame>(&mut cur, class.cap(), None);
        assert!(
            r.is_err(),
            "a tampered frame (byte {pos}) must fail verification"
        );
    }

    // Peer → coordinator, the other direction, once.
    let mut wire2 = Vec::new();
    p_tx.send(&mut wire2, class, frame).expect("sends");
    let mut cur = std::io::Cursor::new(&wire2[..]);
    let (_, mut fresh_c_rx) = session_framers(&key, Role::Coordinator);
    assert!(fresh_c_rx
        .recv::<_, RpcFrame>(&mut cur, class.cap(), None)
        .expect("verifies")
        .is_some());
    assert_eq!(wire2.len(), wire.len(), "both directions frame identically");
    assert!(wire2.len() >= 4 + MAC_BYTES);
}

#[derive(Arbitrary, Debug)]
enum ArbRpc {
    Challenge {
        schema: u32,
        server_nonce: String,
        freshness_ms: u64,
    },
    Prove {
        schema: u32,
        peer_id: String,
        server_nonce: String,
        peer_nonce: String,
        mac: String,
    },
    Admitted {
        schema: u32,
    },
    Refused {
        reason: String,
    },
    Call {
        id: u64,
        verb: u16,
        body: Vec<u8>,
    },
    Reply {
        id: u64,
        status: u16,
        body: Vec<u8>,
    },
}

impl From<ArbRpc> for RpcFrame {
    fn from(a: ArbRpc) -> Self {
        match a {
            ArbRpc::Challenge {
                schema,
                server_nonce,
                freshness_ms,
            } => RpcFrame::Challenge {
                schema,
                server_nonce,
                freshness_ms,
            },
            ArbRpc::Prove {
                schema,
                peer_id,
                server_nonce,
                peer_nonce,
                mac,
            } => RpcFrame::Prove {
                schema,
                peer_id,
                server_nonce,
                peer_nonce,
                mac,
            },
            ArbRpc::Admitted { schema } => RpcFrame::Admitted { schema },
            ArbRpc::Refused { reason } => RpcFrame::Refused { reason },
            ArbRpc::Call { id, verb, body } => RpcFrame::Call { id, verb, body },
            ArbRpc::Reply { id, status, body } => RpcFrame::Reply { id, status, body },
        }
    }
}

#[derive(Arbitrary, Debug)]
struct ArbInput {
    secret: Vec<u8>,
    nonce: String,
    frame: ArbRpc,
}

/// One cross-owner step from the input bytes (the `MetaCall::XvStep`
/// wire form is bincode, the same as every S8 body): a step that decodes
/// rides a request frame and an intent record, and both round-trip to the
/// same step. The record codec is total on the raw bytes beside it.
fn check_xv_step_bodies(data: &[u8]) {
    let _ = IntentRecord::decode(data);
    let mut u = Unstructured::new(data);
    let Ok(tx_id) = u64::arbitrary(&mut u) else {
        return;
    };
    let Ok(kind) = u8::arbitrary(&mut u) else {
        return;
    };
    let Ok(ino) = u64::arbitrary(&mut u) else {
        return;
    };
    let Ok(other) = u64::arbitrary(&mut u) else {
        return;
    };
    let Ok(word) = u32::arbitrary(&mut u) else {
        return;
    };
    let Ok(name) = String::arbitrary(&mut u) else {
        return;
    };
    // Names on the wire are ≤ 255 bytes (the record codec's bound).
    let name: String = name.chars().take(60).collect();
    let step = match kind % 6 {
        0 => XvStep::RemoveDentry {
            parent: ino,
            name,
            expect_child: other,
            parent_update: (word & 3) as u8,
        },
        1 => XvStep::InsertDentry {
            parent: ino,
            name,
            child: other,
            ft_bits: word & 0o170000,
            parent_update: (word >> 8 & 3) as u8,
        },
        2 => XvStep::SetNlink {
            ino,
            pre: word,
            post: word.wrapping_add(1),
            ctime: (other & 1 == 1).then_some(other),
        },
        3 => XvStep::TouchCtime { ino, ctime: other },
        4 => XvStep::MintInode {
            ino,
            mode: word,
            uid: 0,
            gid: 0,
            rdev: 0,
        },
        _ => XvStep::CreateInode {
            ino,
            mode: word,
            uid: 1,
            gid: 2,
            rdev: 0,
            size: other,
            ts_ns: other ^ tx_id,
        },
    };
    let frame = MetaRequestFrame {
        schema: META_SHIP_SCHEMA,
        client_epoch: tx_id,
        client_id: String::new(),
        owner_term: 0,
        ops: vec![MetaOp {
            id: 1,
            call: MetaCall::XvStep {
                tx_id,
                step_idx: 0,
                step: step.clone(),
            },
        }],
    };
    let wire = encode_request(&frame).expect("a bounded step frame encodes");
    let back = decode_request(&wire).expect("re-decodes");
    assert_eq!(back, frame, "the S8 body round-trips the step");
    let record = IntentRecord {
        tx_id,
        op: XvOp::Create,
        steps: vec![step.clone()],
    };
    let image = record.encode().expect("one step encodes");
    let again = IntentRecord::decode(&image).expect("the record decodes");
    assert_eq!(again, record, "the intent record round-trips the step");
    assert_eq!(again.steps[0], step, "both codecs name the same step");
}

const CLASSES: [FrameClass; 3] = [FrameClass::Handshake, FrameClass::Control, FrameClass::Bulk];

fuzz_target!(|data: &[u8]| {
    // --- arm 1: an arbitrary byte STREAM at the plain reader -------------
    for class in CLASSES {
        let mut cur = std::io::Cursor::new(data);
        for _ in 0..64 {
            match read_plain_frame::<_, RpcFrame>(&mut cur, class.cap(), None) {
                Ok(Some(frame)) => check_plain_round_trip(&frame, class),
                Ok(None) | Err(_) => break,
            }
        }
    }

    // --- arm 2: raw bytes at the authenticated reader ----------------------
    // A random tag verifies with probability 2^-256: `Ok(Some(_))` here is
    // a finding, never noise.
    {
        let key = session_key(data, "fuzz-peer", "n0", "n1", None);
        let (_, mut rx) = session_framers(&key, Role::Peer);
        let mut cur = std::io::Cursor::new(data);
        let r = rx.recv::<_, RpcFrame>(&mut cur, FrameClass::Bulk.cap(), None);
        assert!(
            !matches!(r, Ok(Some(_))),
            "arbitrary bytes must never verify as an authenticated frame"
        );
    }

    // --- arm 3: the S8 verb bodies (bounded bincode over a slice) ----------
    if let Ok(f) = decode_request(data) {
        let re = encode_request(&f).expect("an accepted request re-encodes");
        assert_eq!(decode_request(&re).expect("re-decodes"), f);
    }
    if let Ok(f) = decode_reply(data) {
        let re = encode_reply(&f).expect("an accepted reply re-encodes");
        assert_eq!(decode_reply(&re).expect("re-decodes"), f);
    }
    if let Ok(f) = decode_reclaim(data) {
        let re = encode_reclaim(&f).expect("an accepted reclaim re-encodes");
        assert_eq!(decode_reclaim(&re).expect("re-decodes"), f);
    }

    // --- arm 3b (PR 6): a cross-owner STEP as the S8 body carries it, and
    // as the intent RECORD carries it — one step, both codecs, both
    // round-tripping and never disagreeing about the step.
    check_xv_step_bodies(data);

    // --- arm 4: the proof helpers -------------------------------------------
    if let Ok(s) = std::str::from_utf8(data) {
        if let Some(bytes) = hex_decode(s) {
            assert_eq!(bytes.len() * 2, s.len());
            assert_eq!(
                hex_encode(&bytes),
                s.to_ascii_lowercase(),
                "hex round-trips"
            );
        }
        // `str::split_at` needs a char boundary; the law is tested on the
        // inputs whose midpoint is one.
        if let Some((a, b)) = s.split_at_checked(s.len() / 2) {
            assert_eq!(mac_eq(a, b), a == b, "mac_eq is equality");
        }
        assert!(mac_eq(s, s));
    }
    assert_eq!(hex_decode(&hex_encode(data)).as_deref(), Some(data));

    // --- arm 5: constructive — every RpcFrame arm through both paths -------
    let Ok(input) = ArbInput::arbitrary_take_rest(Unstructured::new(data)) else {
        return;
    };
    let frame: RpcFrame = input.frame.into();
    for class in CLASSES {
        // Past the class cap the ENCODER refuses, naming the cap — that is
        // the contract for a frame we build; under it the frame must ride
        // both the plain and the authenticated path.
        if plain_bytes(&frame, class).is_some() {
            check_plain_round_trip(&frame, class);
            check_authenticated(&frame, class, &input.secret, &input.nonce);
        }
    }
});
