//! Fuzz the **read-token wire** (`src/meta_ship/token_plane.rs`,
//! design-symmetric-metadata §5.7 / §6.3, `TOKEN_SCHEMA` 1 under
//! `CLUSTER_WIRE_SCHEMA` 5 — PR 5, review round 1 Issue 9): the
//! `TokenRequestFrame` a reader ships to its holder (`Grant` / `Recall` /
//! `RecallAck` / `Release`; since PR 9 the writer's `CustodyGrant`) and the
//! `TokenReplyFrame` the holder answers (`Granted` with the object's
//! RECORDS — attrs, xattrs, a dentry page — `NotHolder` / `Gone` /
//! `Recall` / `Acked` / `Released` / `Refused`; since PR 9
//! `CustodyGranted` carrying the S9 grant record beside the records,
//! `CustodyRefused` with the custody wire's status word, and — review
//! round 2, Issue 3 — `Rejected`, a wire word screened at the service
//! edge).
//!
//! Threat model: the body arrives on an AUTHENTICATED `cluster_wire`
//! session, but authentication is membership, not trust — a reader with a
//! bug is exactly the peer whose bytes must not crash the holder (the
//! volume's WRITER), and the reply direction is the holder's bytes landing
//! on every reader. The laws (spec §11 TEST-4):
//!
//! * **total** — `decode_*` returns or errors, never panics;
//! * **bounded** — a lying length prefix is never an allocation
//!   authority: decode rides bincode's `with_limit(CONTROL_MAX_FRAME_BYTES)`,
//!   and libFuzzer's `-malloc_limit_mb` is the detector;
//! * **round-trip** — whatever decodes re-encodes to bytes that decode
//!   to an equal frame, and the re-encode is canonical.
//!
//! Three arms: the raw bytes (the reject ladder, both directions), an
//! `Arbitrary`-built frame encoded then decoded — the constructive mirror
//! that reaches every variant, incl. a `Granted` carrying a dentry page —
//! and the SERVICE EDGE over a decoded `CustodyGrant`'s integers
//! (`screen_custody_words` — PR 9 review round 2, Issues 3/13: the `object`
//! word once reached the forest codec's `debug_assert` / release
//! truncation before any screen; the screen must be total over every
//! `u64` and reject what the forest does not name). The mirrors are
//! local: the wire vocabulary must not grow a derive for the fuzzer's
//! sake, and a mirror that falls out of step fails to compile here (a new
//! verb lands with its fuzz arm or not at all).
#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use squeezefs::data_grant::GrantRecord;
use squeezefs::meta_backend::kv::record::{FOREST_SLOT_MAX, NATIVE_FOREST_SLOT};
use squeezefs::meta_ship::token_plane::{
    decode_reply, decode_request, encode_reply, encode_request, screen_custody_words, DirRecord,
    TokenCall, TokenMode, TokenRecords, TokenReply, TokenReplyFrame, TokenRequestFrame, TokenWants,
    WireAttrs, TOKEN_SCHEMA,
};

#[derive(Arbitrary, Debug)]
enum ArbCall {
    Grant {
        object: u64,
        dentries: bool,
        records: bool,
        after: u64,
        xattr_after: Vec<u8>,
    },
    Recall {
        wait_ms: u32,
    },
    RecallAck {
        frame_id: u64,
    },
    Release {
        objects: Vec<u64>,
    },
    // PR 9 — custody by the slot holder.
    CustodyGrant {
        object: u64,
        span: Option<(u64, u64)>,
        concurrent_write: bool,
        wait_ms: u64,
        lease_epoch: u64,
    },
}

impl ArbCall {
    fn into_call(self) -> TokenCall {
        match self {
            ArbCall::Grant {
                object,
                dentries,
                records,
                after,
                xattr_after,
            } => TokenCall::Grant {
                object,
                mode: TokenMode::Read,
                wants: TokenWants { dentries, records },
                after,
                xattr_after: xattr_after.into_iter().take(256).collect(),
            },
            ArbCall::Recall { wait_ms } => TokenCall::Recall { wait_ms },
            ArbCall::RecallAck { frame_id } => TokenCall::RecallAck { frame_id },
            ArbCall::Release { objects } => TokenCall::Release {
                objects: objects.into_iter().take(64).collect(),
            },
            ArbCall::CustodyGrant {
                object,
                span,
                concurrent_write,
                wait_ms,
                lease_epoch,
            } => TokenCall::CustodyGrant {
                object,
                span,
                concurrent_write,
                wait_ms,
                lease_epoch,
            },
        }
    }
}

/// PR 9: the S9 grant record a `CustodyGranted` carries.
#[derive(Arbitrary, Debug)]
struct ArbGrant {
    schema: u32,
    grant_id: u64,
    ino: u64,
    span: Option<(u64, u64)>,
    token: u64,
    term: u64,
    custody_epoch: u64,
}

#[derive(Arbitrary, Debug)]
struct ArbAttrs {
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    flags: u32,
    rdev: u32,
    size: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
}

#[derive(Arbitrary, Debug)]
struct ArbDir {
    cookie: u64,
    child_ino: u64,
    file_type: u8,
    name: Vec<u8>,
}

#[derive(Arbitrary, Debug)]
enum ArbReply {
    Granted {
        attrs: ArbAttrs,
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
        xattrs_complete: bool,
        dir: Option<(Vec<ArbDir>, bool)>,
        already: bool,
    },
    NotHolder {
        holder: u32,
    },
    Gone,
    Recall {
        frame_id: u64,
        objects: Vec<u64>,
    },
    Acked,
    Released {
        count: u64,
    },
    Refused {
        reason: String,
    },
    // PR 9 — the slot holder's answers to `CustodyGrant`.
    CustodyGranted {
        grant: ArbGrant,
        attrs: ArbAttrs,
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
        xattrs_complete: bool,
        already: bool,
    },
    CustodyRefused {
        status: u16,
        reason: String,
    },
    Rejected {
        reason: String,
    },
}

/// The bounded `TokenRecords` of a granted reply (both grant shapes).
fn records_of(
    attrs: ArbAttrs,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    xattrs_complete: bool,
    dir: Option<(Vec<ArbDir>, bool)>,
) -> TokenRecords {
    TokenRecords {
        xattrs_complete,
        attrs: WireAttrs {
            mode: attrs.mode,
            uid: attrs.uid,
            gid: attrs.gid,
            nlink: attrs.nlink,
            flags: attrs.flags,
            rdev: attrs.rdev,
            size: attrs.size,
            atime: attrs.atime,
            mtime: attrs.mtime,
            ctime: attrs.ctime,
        },
        xattrs: xattrs
            .into_iter()
            .take(8)
            .map(|(n, v)| {
                (
                    n.into_iter().take(64).collect(),
                    v.into_iter().take(256).collect(),
                )
            })
            .collect(),
        dir: dir.map(|(entries, complete)| {
            (
                entries
                    .into_iter()
                    .take(64)
                    .map(|d| DirRecord {
                        cookie: d.cookie,
                        child_ino: d.child_ino,
                        file_type: d.file_type,
                        name: d.name.into_iter().take(255).collect(),
                    })
                    .collect(),
                complete,
            )
        }),
    }
}

impl ArbReply {
    fn into_reply(self) -> TokenReply {
        match self {
            ArbReply::Granted {
                attrs,
                xattrs,
                xattrs_complete,
                dir,
                already,
            } => TokenReply::Granted {
                records: records_of(attrs, xattrs, xattrs_complete, dir),
                already,
            },
            ArbReply::CustodyGranted {
                grant,
                attrs,
                xattrs,
                xattrs_complete,
                already,
            } => TokenReply::CustodyGranted {
                grant: GrantRecord {
                    schema: grant.schema,
                    grant_id: grant.grant_id,
                    ino: grant.ino,
                    span: grant.span,
                    token: grant.token,
                    term: grant.term,
                    custody_epoch: grant.custody_epoch,
                },
                records: records_of(attrs, xattrs, xattrs_complete, None),
                already,
            },
            ArbReply::CustodyRefused { status, reason } => TokenReply::CustodyRefused {
                status,
                reason: reason.chars().take(256).collect(),
            },
            ArbReply::Rejected { reason } => TokenReply::Rejected {
                reason: reason.chars().take(256).collect(),
            },
            ArbReply::NotHolder { holder } => TokenReply::NotHolder { holder },
            ArbReply::Gone => TokenReply::Gone,
            ArbReply::Recall { frame_id, objects } => TokenReply::Recall {
                frame_id,
                objects: objects.into_iter().take(64).collect(),
            },
            ArbReply::Acked => TokenReply::Acked,
            ArbReply::Released { count } => TokenReply::Released { count },
            ArbReply::Refused { reason } => TokenReply::Refused {
                reason: reason.chars().take(256).collect(),
            },
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // Arm 1: the raw bytes, both directions — total, bounded, and a
    // canonical round trip for whatever decodes.
    if let Ok(frame) = decode_request(data) {
        let re = encode_request(&frame).expect("an accepted request frame re-encodes");
        assert_eq!(decode_request(&re).expect("re-decodes"), frame);
        assert_eq!(
            encode_request(&decode_request(&re).unwrap()).unwrap(),
            re,
            "canonical"
        );
    }
    if let Ok(frame) = decode_reply(data) {
        let re = encode_reply(&frame).expect("an accepted reply frame re-encodes");
        assert_eq!(decode_reply(&re).expect("re-decodes"), frame);
        assert_eq!(
            encode_reply(&decode_reply(&re).unwrap()).unwrap(),
            re,
            "canonical"
        );
    }

    // Arm 2: the constructive mirror — every variant reached.
    let mut u = Unstructured::new(data);
    if let (Ok(request_id), Ok(volume), Ok(client), Ok(call)) = (
        u64::arbitrary(&mut u),
        u16::arbitrary(&mut u),
        String::arbitrary(&mut u),
        ArbCall::arbitrary(&mut u),
    ) {
        let frame = TokenRequestFrame {
            schema: TOKEN_SCHEMA,
            request_id,
            volume,
            client: client.chars().take(128).collect(),
            call: call.into_call(),
        };
        if let Ok(enc) = encode_request(&frame) {
            assert_eq!(decode_request(&enc).expect("decodes"), frame);
        }
    }
    if let (Ok(request_id), Ok(reply)) = (u64::arbitrary(&mut u), ArbReply::arbitrary(&mut u)) {
        let frame = TokenReplyFrame {
            schema: TOKEN_SCHEMA,
            request_id,
            reply: reply.into_reply(),
        };
        if let Ok(enc) = encode_reply(&frame) {
            assert_eq!(decode_reply(&enc).expect("decodes"), frame);
        }
    }

    // Arm 3: the service edge — every decoded `CustodyGrant`'s words
    // through the screen against a small forest (the native slot + the
    // guest slots 1..=8 named, routing slot = forest slot - 1), and the
    // raw words directly: total over every u64, and the accept set is
    // exactly the forest's.
    if let Ok(frame) = decode_request(data) {
        if let TokenCall::CustodyGrant { object, span, .. } = frame.call {
            let _ = screen_custody_words(object, span, &small_forest);
        }
    }
    if let (Ok(object), Ok(span)) = (
        u64::arbitrary(&mut u),
        Option::<(u64, u64)>::arbitrary(&mut u),
    ) {
        let verdict = screen_custody_words(object, span, &small_forest);
        let forest_slot = object >> 40;
        let raw = if forest_slot == u64::from(NATIVE_FOREST_SLOT) {
            object
        } else {
            object & ((1u64 << 40) - 1)
        };
        let named = forest_slot == u64::from(NATIVE_FOREST_SLOT) || (1..=8).contains(&forest_slot);
        let span_ok = span.is_none_or(|(s, e)| s < e);
        let accept = forest_slot <= u64::from(FOREST_SLOT_MAX) && raw >= 2 && named && span_ok;
        assert_eq!(
            verdict.is_ok(),
            accept,
            "the screen's accept set is the forest's: {object:#x} {span:?} → {verdict:?}"
        );
        if let Ok((routing, r)) = verdict {
            assert_eq!(r, raw);
            assert_eq!(
                u64::from(routing),
                if forest_slot == u64::from(NATIVE_FOREST_SLOT) {
                    0
                } else {
                    forest_slot - 1
                }
            );
        }
    }
});

/// The service-edge arm's forest: the native slot (routing 0) and guest
/// forest slots 1..=8 (routing slot = forest slot - 1); nothing else.
fn small_forest(forest_slot: u32) -> Option<u16> {
    if forest_slot == NATIVE_FOREST_SLOT {
        Some(0)
    } else if (1..=8).contains(&forest_slot) {
        Some((forest_slot - 1) as u16)
    } else {
        None
    }
}
