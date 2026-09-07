//! Fuzz the **S9 publish wire** (`src/meta_ship/publish.rs`, schema 13):
//! the multi-call `PublishRequestFrame` a co-writer ships to the volume
//! authority (D-1b framing — N `PublishCall`s per frame, the kvmap
//! `MigrateBlockMap` train with its whole `entries` map + refs frame +
//! `base_gen` included) and the `PublishReplyFrame` the authority answers
//! (one `PublishCallOutcome` per call — `MapMigrated { recomputed, gen,
//! … }` among them).
//!
//! Threat model: the request body arrives on an AUTHENTICATED
//! `cluster_wire` session, but authentication is membership, not
//! trust — a co-writer with a bug (or a compromised member) is exactly
//! the peer whose bytes must not crash the ONE metadata authority of a
//! volume set; and the reply direction is the authority's bytes landing
//! on every co-writer. The laws (spec §11 TEST-4):
//!
//! * **total** — `decode_*_frame` returns or errors, never panics;
//! * **bounded** — a lying length prefix is never an allocation
//!   authority (the `kv_bset_record_count` precedent): decode rides
//!   bincode's `with_limit(CONTROL_MAX_FRAME_BYTES)` over a slice
//!   reader, and libFuzzer's `-malloc_limit_mb` (= `-rss_limit_mb`,
//!   2 GiB default) is the detector — one oversize `Vec::with_capacity`
//!   is a reported crash;
//! * **round-trip** — whatever decodes re-encodes to bytes that decode
//!   to an equal frame (a decoder accepting a form its encoder cannot
//!   produce is a wire hole; bincode varints make the re-encode
//!   canonical, so the bytes themselves are compared too).
//!
//! Two arms: the raw bytes (the reject ladder + whatever valid varint
//! prefix the corpus grows into), and an `Arbitrary`-built frame
//! encoded then decoded — the constructive mirror that reaches every
//! variant of a 15-arm enum a blind fuzzer would take hours to spell.
#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use squeezefs::meta_ship::publish::{
    decode_reply_frame, decode_request_frame, encode_reply_frame, encode_request_frame,
    FreeVerdict, PublishCall, PublishCallOutcome, PublishReply, PublishReplyFrame,
    PublishRequestFrame, WireBlockRefOp, WireFreedBlock, PUBLISH_SCHEMA,
};
use squeezefs::meta_ship::wire::{WireDirEntry, WireError, WireInode};

fn check_request(frame: &PublishRequestFrame) {
    // A frame we accepted must re-encode (it is inside the CONTROL cap by
    // construction — decode bounded it) and re-decode to itself.
    let re = encode_request_frame(frame).expect("an accepted request frame re-encodes");
    let again = decode_request_frame(&re).expect("a re-encoded request frame decodes");
    assert_eq!(&again, frame, "request frame must round-trip");
    assert_eq!(
        encode_request_frame(&again).expect("re-encodes"),
        re,
        "the request encoding is canonical"
    );
}

fn check_reply(frame: &PublishReplyFrame) {
    let re = encode_reply_frame(frame).expect("an accepted reply frame re-encodes");
    let again = decode_reply_frame(&re).expect("a re-encoded reply frame decodes");
    assert_eq!(&again, frame, "reply frame must round-trip");
    assert_eq!(
        encode_reply_frame(&again).expect("re-encodes"),
        re,
        "the reply encoding is canonical"
    );
}

// --- the constructive arm ----------------------------------------------------
//
// Local mirrors with `Arbitrary` derived, mapped onto the wire types: the
// wire vocabulary must not grow a derive for the fuzzer's sake, and a
// mirror that falls out of step fails to compile here — which is the
// point (a new call variant lands with its fuzz arm or not at all).

#[derive(Arbitrary, Debug)]
struct ArbRef {
    vol_tag: u64,
    block_idx: u64,
    owner_ino: u64,
    block_index: u32,
    take: bool,
}

impl From<ArbRef> for WireBlockRefOp {
    fn from(r: ArbRef) -> Self {
        WireBlockRefOp {
            vol_tag: r.vol_tag,
            block_idx: r.block_idx,
            owner_ino: r.owner_ino,
            block_index: r.block_index,
            take: r.take,
        }
    }
}

fn refs(v: Vec<ArbRef>) -> Vec<WireBlockRefOp> {
    v.into_iter().map(Into::into).collect()
}

/// The schema-15 freed set's element — the durable identity pair.
#[derive(Arbitrary, Debug)]
struct ArbFreed {
    vol_tag: u64,
    block_idx: u64,
}

fn freed(v: Vec<ArbFreed>) -> Vec<WireFreedBlock> {
    v.into_iter()
        .map(|f| WireFreedBlock {
            vol_tag: f.vol_tag,
            block_idx: f.block_idx,
        })
        .collect()
}

#[derive(Arbitrary, Debug)]
enum ArbCall {
    SetLayoutAndSize {
        ino: u64,
        layout: Vec<u8>,
        size: u64,
        refs: Vec<ArbRef>,
        lease_epoch: u64,
        request_id: u64,
    },
    MergeLayoutAndSize {
        ino: u64,
        delta: Vec<u8>,
        full_layout: Vec<u8>,
        size: u64,
        refs: Vec<ArbRef>,
        lease_epoch: u64,
        request_id: u64,
    },
    CommitBlockRefs {
        ino: u64,
        refs: Vec<ArbRef>,
        lease_epoch: u64,
        request_id: u64,
    },
    ParkWriteTimes {
        ino: u64,
        mtime: u64,
        ctime: u64,
        lease_epoch: u64,
    },
    DestroyInodes {
        inos: Vec<u64>,
        lease_epoch: u64,
    },
    CreateWithRdevSize {
        parent: u64,
        name: String,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        initial_size: u64,
        lease_epoch: u64,
    },
    XattrValueCap {
        ino: u64,
    },
    ReaddirStream {
        dir: u64,
        offset: u64,
        max: u32,
    },
    RaiseAllocLane {
        vol_tag: u64,
        lane: u16,
        writers: u16,
        upto: u64,
        lease_epoch: u64,
    },
    FreeBlocks {
        vol_tag: u64,
        blocks: Vec<u64>,
        lease_epoch: u64,
        request_id: u64,
    },
    HarvestLaneFree {
        vol_tag: u64,
        lane: u16,
        writers: u16,
        max: u64,
        lease_epoch: u64,
        request_id: u64,
    },
    WriteExtent {
        ino: u64,
        block_index: u64,
        offset_in_block: u32,
        data: Vec<u8>,
        token: u64,
        lease_epoch: u64,
        request_id: u64,
    },
    FlushExtents {
        ino: u64,
        lease_epoch: u64,
        request_id: u64,
    },
    BlockRefPopulation {
        vol_tag: u64,
        block_idxs: Vec<u64>,
    },
    MigrateBlockMap {
        ino: u64,
        layout: Vec<u8>,
        size: u64,
        entries: Vec<(u32, String)>,
        refs: Vec<ArbRef>,
        base_gen: u64,
        lease_epoch: u64,
        request_id: u64,
    },
}

impl From<ArbCall> for PublishCall {
    fn from(c: ArbCall) -> Self {
        match c {
            ArbCall::SetLayoutAndSize {
                ino,
                layout,
                size,
                refs: r,
                lease_epoch,
                request_id,
            } => PublishCall::SetLayoutAndSize {
                ino,
                layout,
                size,
                refs: refs(r),
                lease_epoch,
                request_id,
            },
            ArbCall::MergeLayoutAndSize {
                ino,
                delta,
                full_layout,
                size,
                refs: r,
                lease_epoch,
                request_id,
            } => PublishCall::MergeLayoutAndSize {
                ino,
                delta,
                full_layout,
                size,
                refs: refs(r),
                lease_epoch,
                request_id,
            },
            ArbCall::CommitBlockRefs {
                ino,
                refs: r,
                lease_epoch,
                request_id,
            } => PublishCall::CommitBlockRefs {
                ino,
                refs: refs(r),
                lease_epoch,
                request_id,
            },
            ArbCall::ParkWriteTimes {
                ino,
                mtime,
                ctime,
                lease_epoch,
            } => PublishCall::ParkWriteTimes {
                ino,
                mtime,
                ctime,
                lease_epoch,
            },
            ArbCall::DestroyInodes { inos, lease_epoch } => {
                PublishCall::DestroyInodes { inos, lease_epoch }
            }
            ArbCall::CreateWithRdevSize {
                parent,
                name,
                mode,
                uid,
                gid,
                rdev,
                initial_size,
                lease_epoch,
            } => PublishCall::CreateWithRdevSize {
                parent,
                name,
                mode,
                uid,
                gid,
                rdev,
                initial_size,
                lease_epoch,
            },
            ArbCall::XattrValueCap { ino } => PublishCall::XattrValueCap { ino },
            ArbCall::ReaddirStream { dir, offset, max } => {
                PublishCall::ReaddirStream { dir, offset, max }
            }
            ArbCall::RaiseAllocLane {
                vol_tag,
                lane,
                writers,
                upto,
                lease_epoch,
            } => PublishCall::RaiseAllocLane {
                vol_tag,
                lane,
                writers,
                upto,
                lease_epoch,
            },
            ArbCall::FreeBlocks {
                vol_tag,
                blocks,
                lease_epoch,
                request_id,
            } => PublishCall::FreeBlocks {
                vol_tag,
                blocks,
                lease_epoch,
                request_id,
            },
            ArbCall::HarvestLaneFree {
                vol_tag,
                lane,
                writers,
                max,
                lease_epoch,
                request_id,
            } => PublishCall::HarvestLaneFree {
                vol_tag,
                lane,
                writers,
                max,
                lease_epoch,
                request_id,
            },
            ArbCall::WriteExtent {
                ino,
                block_index,
                offset_in_block,
                data,
                token,
                lease_epoch,
                request_id,
            } => PublishCall::WriteExtent {
                ino,
                block_index,
                offset_in_block,
                data,
                token,
                lease_epoch,
                request_id,
            },
            ArbCall::FlushExtents {
                ino,
                lease_epoch,
                request_id,
            } => PublishCall::FlushExtents {
                ino,
                lease_epoch,
                request_id,
            },
            ArbCall::BlockRefPopulation {
                vol_tag,
                block_idxs,
            } => PublishCall::BlockRefPopulation {
                vol_tag,
                block_idxs,
            },
            ArbCall::MigrateBlockMap {
                ino,
                layout,
                size,
                entries,
                refs: r,
                base_gen,
                lease_epoch,
                request_id,
            } => PublishCall::MigrateBlockMap {
                ino,
                layout,
                size,
                entries,
                refs: refs(r),
                base_gen,
                lease_epoch,
                request_id,
            },
        }
    }
}

#[derive(Arbitrary, Debug)]
enum ArbReply {
    Unit,
    PutDone {
        recomputed: bool,
        freed: Vec<ArbFreed>,
    },
    DeltaUsed {
        used: bool,
        version: u64,
        recomputed: bool,
        freed: Vec<ArbFreed>,
    },
    ExtentAck {
        covering_version: Option<u64>,
    },
    FlushDone {
        covering_version: u64,
    },
    Inode {
        ino: u64,
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
    },
    Cap(u64),
    Page(Vec<(u64, u64, String, u32)>),
    LaneFrontier(u64),
    FreeVerdicts(Vec<u8>),
    LaneFreeGrant {
        blocks: Vec<u64>,
        bound_age_ms: u64,
        release_ages_ms: Vec<u64>,
    },
    Populations(Vec<u64>),
    MapMigrated {
        records: u64,
        record_bytes: u64,
        preexisting: u64,
        recomputed: bool,
        freed: Vec<ArbFreed>,
        gen: u64,
    },
}

#[derive(Arbitrary, Debug)]
enum ArbOutcome {
    Done(ArbReply),
    Failed { errno: i32, msg: String },
    Refused { status: u16, detail: String },
}

#[derive(Arbitrary, Debug)]
struct ArbInput {
    // The raw bytes both decoders see (the reject-ladder arm).
    raw: Vec<u8>,
    // The constructive arm.
    schema_is_current: bool,
    schema: u32,
    client: String,
    calls: Vec<ArbCall>,
    outcomes: Vec<ArbOutcome>,
}

fn reply_from(r: ArbReply) -> PublishReply {
    match r {
        ArbReply::Unit => PublishReply::Unit,
        ArbReply::PutDone {
            recomputed,
            freed: f,
        } => PublishReply::PutDone {
            recomputed,
            freed: freed(f),
        },
        ArbReply::DeltaUsed {
            used,
            version,
            recomputed,
            freed: f,
        } => PublishReply::DeltaUsed {
            used,
            version,
            recomputed,
            freed: freed(f),
        },
        ArbReply::ExtentAck { covering_version } => PublishReply::ExtentAck { covering_version },
        ArbReply::FlushDone { covering_version } => PublishReply::FlushDone { covering_version },
        ArbReply::Inode {
            ino,
            mode,
            uid,
            gid,
            nlink,
            flags,
            rdev,
            size,
            atime,
            mtime,
            ctime,
        } => PublishReply::Inode(WireInode {
            ino,
            mode,
            uid,
            gid,
            size,
            nlink,
            atime,
            mtime,
            ctime,
            flags,
            rdev,
        }),
        ArbReply::Cap(c) => PublishReply::Cap(c),
        ArbReply::Page(entries) => PublishReply::Page(
            entries
                .into_iter()
                .map(|(cookie, ino, name, file_type)| {
                    (
                        cookie,
                        WireDirEntry {
                            ino,
                            name,
                            file_type,
                        },
                    )
                })
                .collect(),
        ),
        ArbReply::LaneFrontier(f) => PublishReply::LaneFrontier(f),
        // Picked by byte so every verdict arm is reachable without a
        // derive on the wire type.
        ArbReply::FreeVerdicts(vs) => PublishReply::FreeVerdicts(
            vs.into_iter()
                .map(|b| match b % 3 {
                    0 => FreeVerdict::Freed,
                    1 => FreeVerdict::NonTerminal,
                    _ => FreeVerdict::Refused,
                })
                .collect(),
        ),
        ArbReply::LaneFreeGrant {
            blocks,
            bound_age_ms,
            release_ages_ms,
        } => PublishReply::LaneFreeGrant {
            blocks,
            bound_age_ms,
            release_ages_ms,
        },
        ArbReply::Populations(p) => PublishReply::Populations(p),
        ArbReply::MapMigrated {
            records,
            record_bytes,
            preexisting,
            recomputed,
            freed: f,
            gen,
        } => PublishReply::MapMigrated {
            records,
            record_bytes,
            preexisting,
            recomputed,
            freed: freed(f),
            gen,
        },
    }
}

fuzz_target!(|data: &[u8]| {
    // --- arm 1: raw bytes at both decoders ----------------------------------
    if let Ok(frame) = decode_request_frame(data) {
        check_request(&frame);
    }
    if let Ok(frame) = decode_reply_frame(data) {
        check_reply(&frame);
    }

    // --- arm 2: constructive ------------------------------------------------
    let Ok(input) = ArbInput::arbitrary_take_rest(Unstructured::new(data)) else {
        return;
    };
    // The constructive input's own raw slice: a second, differently
    // aligned view of the same bytes at the decoders.
    if let Ok(frame) = decode_request_frame(&input.raw) {
        check_request(&frame);
    }
    let schema = if input.schema_is_current {
        PUBLISH_SCHEMA
    } else {
        input.schema
    };
    let request = PublishRequestFrame {
        schema,
        client: input.client,
        calls: input.calls.into_iter().map(Into::into).collect(),
    };
    // Past the CONTROL cap the encoder REFUSES (a layout that large rides
    // the indirect map blob, never the wire) — that refusal is the
    // contract, not a failure; below it the frame must round-trip.
    if let Ok(body) = encode_request_frame(&request) {
        let back = decode_request_frame(&body).expect("an encoded request frame decodes");
        assert_eq!(back, request, "request round-trip (constructive)");
    }
    let reply = PublishReplyFrame {
        schema,
        outcomes: input
            .outcomes
            .into_iter()
            .map(|o| match o {
                ArbOutcome::Done(r) => PublishCallOutcome::Done(Ok(reply_from(r))),
                ArbOutcome::Failed { errno, msg } => {
                    PublishCallOutcome::Done(Err(WireError { errno, msg }))
                }
                ArbOutcome::Refused { status, detail } => {
                    PublishCallOutcome::Refused { status, detail }
                }
            })
            .collect(),
    };
    if let Ok(body) = encode_reply_frame(&reply) {
        let back = decode_reply_frame(&body).expect("an encoded reply frame decodes");
        assert_eq!(back, reply, "reply round-trip (constructive)");
    }
});
