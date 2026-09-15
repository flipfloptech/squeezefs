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
//!   equal too);
//! * **bounded EXECUTION** (review round 1, Issue 2) — a decoded integer
//!   is never an allocation authority at the SERVICE edge either: the
//!   `ReturnExtents` validators reject an overflowing or out-of-volume run
//!   without touching it, the record intersection materializes at most
//!   the RECORD's extents whatever the runs name, and an explicit
//!   `ExtentGrant { want }` is clamped to the derivation's cap
//!   (`appender::{validate_return_runs, coalesce_runs, intersect_coalesced_with_record,
//!   clamp_grant_want}` — the pure edge `KvMetaBackend::manager_return_runs`
//!   / `manager_extent_grant_class` run); and (review round 6, Issue 29)
//!   **no wire slot word reaches a RAM or durable effect unvalidated** — a
//!   `ReleaseSlot` frame's `seq_floor` / `cursor` / `root` /
//!   `slot_tree_extents` are screened against durable and derived bounds
//!   (`appender::{screen_release_words, release_seq_floor_bound,
//!   root_extent_of}`, the pure edge `manager_release_slot_wire` runs
//!   first); a poisoned word is `STATUS_REJECTED` with nothing written.
//!
//! Three arms: the raw bytes (the reject ladder), an `Arbitrary`-built
//! frame encoded then decoded — the constructive mirror that reaches
//! every variant — and the decoded call's integers driven through the
//! service-edge validators against a small arbitrary record. The mirrors
//! are local: the wire vocabulary must not grow a derive for the fuzzer's
//! sake, and a mirror that falls out of step fails to compile here (a new
//! verb lands with its fuzz arm or not at all).
#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::appender::{
    clamp_grant_want, coalesce_runs, intersect_coalesced_with_record, release_seq_floor_bound,
    root_extent_of, runs_extent_count, screen_release_words, validate_return_runs, GrantRun,
    ReleaseWordBounds, SEQ_FRONTIER_SANE_MAX,
};
use squeezefs::meta_backend::kv::slot_state::{
    ExtentGrantRecord, SlotState, SlotTails, SlotTailsRecord, TAILS_SPILLED,
};
use squeezefs::meta_backend::kv::tree::RootPtr;
use squeezefs::meta_ship::manager::{
    decode_reply, decode_request, encode_reply, encode_request, ManagerCall, ManagerReply,
    ManagerReplyFrame, ManagerRequestFrame, WireIdentity, WireSlotGrant, WireSlotWords,
    MANAGER_SCHEMA,
};

/// Arm 3: the service-edge law over a decoded call's integers. `record`
/// is small by construction (≤ 64 extents), so the intersection's bound
/// is the record's, never the frame's.
fn check_service_edge(call: &ManagerCall, total_extents: u64, record_seed: &[u8]) {
    let record = ExtentGrantRecord::from_extents(
        record_seed
            .iter()
            .take(64)
            .map(|b| u64::from(*b) % total_extents.max(1)),
    );
    match call {
        ManagerCall::ReturnExtents { runs, .. } => {
            let runs: Vec<GrantRun> = runs
                .iter()
                .map(|&(start, len)| GrantRun { start, len })
                .collect();
            match validate_return_runs(&runs, total_extents) {
                Ok(()) => {
                    for r in &runs {
                        assert!(r.start + u64::from(r.len) <= total_extents);
                    }
                }
                Err(offender) => {
                    assert!(
                        offender
                            .start
                            .checked_add(u64::from(offender.len))
                            .is_none_or(|end| end > total_extents),
                        "a rejected run is one outside the volume or overflowing"
                    );
                }
            }
            // The coalesce is bounded by the FRAME's own run count and
            // yields disjoint ascending runs naming no more than the
            // input; the intersection emits each record extent at most
            // once — strictly ascending by construction, no dedup step
            // exists to hide an over-allocation behind (Issue 2's
            // residual: the pre-dedup list was ∝ runs × record).
            let coalesced = coalesce_runs(&runs);
            assert!(coalesced.len() <= runs.len());
            assert!(coalesced
                .windows(2)
                .all(|w| { w[0].start + u64::from(w[0].len) < w[1].start }));
            assert!(runs_extent_count(&coalesced) <= runs_extent_count(&runs));
            let inside = intersect_coalesced_with_record(&coalesce_runs(&runs), &record);
            assert!(
                inside.len() as u64 <= record.len(),
                "the materialized list is bounded by the record"
            );
            assert!(inside.windows(2).all(|w| w[0] < w[1]), "strictly ascending");
            for e in &inside {
                assert!(record.contains(*e));
            }
        }
        ManagerCall::ExtentGrant { want, .. } => {
            for cap in [8u64, 1 << 20, u64::from(u32::MAX) + 1] {
                let w = clamp_grant_want(*want, cap);
                assert!(w <= cap && (w > 0 || cap == 0));
            }
        }
        // The slot-lease verbs (PR 4): their wire integers are a slot
        // (u16 — the routing namespace, total), an appender id, `g`, the
        // words and the tails; the ONE proportional input is the tails
        // list, bounded by the frame's class cap at the codec. The
        // release site records it inline below the KV value cap and
        // SPILLED above it (review round 3, Issue 21), so the fixed-size
        // `Unleased` record round-trips with the frame's words whatever
        // the count, and the tails record's inline arm encodes for every
        // count below the spill sentinel.
        ManagerCall::ReleaseSlot { words, tails, .. } => {
            let state = SlotState::Unleased {
                root: RootPtr {
                    addr: words.root.0,
                    seq: words.root.1,
                },
                cursor: words.cursor,
                g: 1,
                slot_tree_extents: words.slot_tree_extents,
                last_written: 0,
                seq_floor: words.seq_floor,
            };
            assert_eq!(SlotState::decode(&state.encode()).expect("decodes"), state);
            if tails.len() < usize::from(TAILS_SPILLED) {
                let rec = SlotTailsRecord {
                    g: 1,
                    tails: SlotTails::Inline(tails.clone()),
                };
                let img = rec
                    .encode()
                    .expect("a tails count below the spill sentinel encodes inline");
                assert_eq!(SlotTailsRecord::decode(&img).expect("decodes"), rec);
            }
            // The POISONED-FRAME arm (review round 6, Issue 29): no wire
            // slot word reaches a RAM or durable effect unvalidated. The
            // screen the release's wire face runs BEFORE any effect is
            // pure over the words and the durable bounds; against small
            // arbitrary bounds its verdict is exactly the predicate — a
            // word outside its bound is refused (the service answers
            // `STATUS_REJECTED`, nothing written), words inside pass — and
            // the derived frontier bound never leaves the sane seq space
            // whatever the page says.
            let seed = |i: usize| u64::from(record_seed.get(i).copied().unwrap_or(0));
            let node = 4096u64 << (seed(0) % 7); // 4 KiB .. 256 KiB
            let bounds = ReleaseWordBounds {
                seq_floor_recorded: seed(1) * 64,
                seq_floor_max: release_seq_floor_bound(
                    seed(2) * 64,
                    (seed(3) > 0).then_some(seed(1) * 64),
                    seed(4) * 16,
                    node * (1 + seed(5)),
                ),
                cursor_recorded: seed(6),
                cursor_max: 1 << 40,
                root_recorded: ((1 << 20) + node * (seed(7) % total_extents.max(1)), seed(8)),
                heap_base: 1 << 20,
                node_size: node,
                total_extents,
            };
            assert!(bounds.seq_floor_max <= SEQ_FRONTIER_SANE_MAX);
            let w: squeezefs::slot_lease_core::SlotWords = (*words).into();
            let root_ok = w.root == bounds.root_recorded
                || root_extent_of(w.root.0, &bounds).is_some_and(|e| record.contains(e));
            let inside = w.seq_floor > bounds.seq_floor_recorded
                && w.seq_floor <= bounds.seq_floor_max
                && w.cursor >= bounds.cursor_recorded
                && w.cursor <= bounds.cursor_max
                && u64::from(w.extents) <= bounds.total_extents
                && root_ok;
            let verdict = screen_release_words(&w, &bounds, &record);
            assert_eq!(
                verdict.is_ok(),
                inside,
                "the screen's verdict is the predicate: {w:?} against {bounds:?} → {verdict:?}"
            );
            if let Err(e) = verdict {
                assert!(!e.to_string().is_empty(), "every refusal names its class");
            }
            if let Some(e) = root_extent_of(w.root.0, &bounds) {
                assert!(e < total_extents);
                assert_eq!(bounds.heap_base + e * node, w.root.0);
            }
        }
        ManagerCall::JoinAppender { .. }
        | ManagerCall::AcquireSlots { .. }
        | ManagerCall::AcquireSlot { .. }
        | ManagerCall::OfferSlot { .. }
        | ManagerCall::ResolveSlot { .. } => {}
    }
}

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
    AcquireSlots {
        appender_id: u32,
        want: u16,
    },
    AcquireSlot {
        appender_id: u32,
        slot: u16,
    },
    OfferSlot {
        appender_id: u32,
        slot: u16,
        to: u32,
    },
    ReleaseSlot {
        appender_id: u32,
        slot: u16,
        g: u32,
        words: ArbWords,
        tails: Vec<(u64, u32)>,
    },
    ResolveSlot {
        slot: u16,
    },
}

#[derive(Arbitrary, Debug, Clone, Copy)]
struct ArbWords {
    root: (u64, u64),
    cursor: u64,
    slot_tree_extents: u32,
    seq_floor: u64,
}

impl From<ArbWords> for WireSlotWords {
    fn from(w: ArbWords) -> Self {
        WireSlotWords {
            root: w.root,
            cursor: w.cursor,
            slot_tree_extents: w.slot_tree_extents,
            seq_floor: w.seq_floor,
        }
    }
}

impl From<ArbCall> for ManagerCall {
    fn from(c: ArbCall) -> Self {
        match c {
            ArbCall::AcquireSlots { appender_id, want } => {
                ManagerCall::AcquireSlots { appender_id, want }
            }
            ArbCall::AcquireSlot { appender_id, slot } => {
                ManagerCall::AcquireSlot { appender_id, slot }
            }
            ArbCall::OfferSlot {
                appender_id,
                slot,
                to,
            } => ManagerCall::OfferSlot {
                appender_id,
                slot,
                to,
            },
            ArbCall::ReleaseSlot {
                appender_id,
                slot,
                g,
                words,
                tails,
            } => ManagerCall::ReleaseSlot {
                appender_id,
                slot,
                g,
                words: words.into(),
                tails,
            },
            ArbCall::ResolveSlot { slot } => ManagerCall::ResolveSlot { slot },
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
    SlotsGranted {
        slots: Vec<(u16, u32, ArbWords)>,
        already: bool,
    },
    SlotRefused {
        slot: u16,
        holder: u32,
        g: u32,
    },
    Offered,
    Released {
        already: bool,
    },
    Holder {
        appender_id: u32,
        g: u32,
    },
    Unleased {
        g: u32,
    },
}

impl From<ArbReply> for ManagerReply {
    fn from(r: ArbReply) -> Self {
        match r {
            ArbReply::SlotsGranted { slots, already } => ManagerReply::SlotsGranted {
                slots: slots
                    .into_iter()
                    .map(|(slot, g, words)| WireSlotGrant {
                        slot,
                        g,
                        words: words.into(),
                    })
                    .collect(),
                already,
            },
            ArbReply::SlotRefused { slot, holder, g } => {
                ManagerReply::SlotRefused { slot, holder, g }
            }
            ArbReply::Offered => ManagerReply::Offered,
            ArbReply::Released { already } => ManagerReply::Released { already },
            ArbReply::Holder { appender_id, g } => ManagerReply::Holder { appender_id, g },
            ArbReply::Unleased { g } => ManagerReply::Unleased { g },
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
    volume: u16,
    call: ArbCall,
    reply: ArbReply,
    // The service-edge arm: the volume's extent count and a record seed.
    total_extents: u64,
    record_seed: Vec<u8>,
}

fuzz_target!(|data: &[u8]| {
    // --- arm 1: raw bytes at both decoders ----------------------------------
    if let Ok(frame) = decode_request(data) {
        check_request(&frame);
        check_service_edge(&frame.call, 1 << 20, &data[..data.len().min(64)]);
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
        volume: input.volume,
        call: input.call.into(),
    };
    // --- arm 3: the service edge over the call's integers -------------------
    check_service_edge(&request.call, input.total_extents, &input.record_seed);
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
