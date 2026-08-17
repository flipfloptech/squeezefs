//! DLM **S10 rung 11 — the RECALL LANE + THRASH VALVE** ("brake before
//! engine": `docs/design-full-multi-writer.md` PR row 11 lands this rung
//! BEFORE any delegation grant exists, so delegation can never ship
//! without its brake).
//!
//! The spec rows this suite pins:
//!
//! * **R5** (`docs/pre-rc-engineering-spec.md` §6.10): *"Revoke storms on
//!   hot shared objects at 15 k readers — `dlm_thrash_demotions` under a
//!   synthetic hot-object storm; verify the demotion valve engages before
//!   the fan-out hurts."* The storm here is in-process (N simulated
//!   clients hammering ONE object through the grant→recall cycle) — the
//!   population the delegation rows (12–14) will feed from real grants
//!   arrives today through the same API, so the pins transfer verbatim.
//! * **R3**'s law: the recall deadline derives from LIVE p99 evidence
//!   (`meta_ship_phase_ns.rtt` + `meta_ship_owner_phase_ns.total`),
//!   never a constant; the lease TTL is the ceiling because past it the
//!   S6/S7 fence arithmetic bounds the client anyway.
//! * The **Ceph recall pattern** (§6.6): batched per client (one frame
//!   carries many object recalls), rate-limited by DERIVED caps — one
//!   in-flight frame per client whose size derives from the CONTROL wire
//!   class, so the per-client rate is structurally `batch_max/deadline`
//!   with no free constant anywhere.
//!
//! Storm arms (the mission's five, plus the dark-posture and batching
//! pins):
//!
//! (a) valve-DISABLED seam ⇒ recall volume grows with the storm (the red
//!     half — the seam is a test-only constructor field, never a knob);
//! (b) valve ⇒ `dlm_thrash_demotions` engages and the volume FLATTENS;
//! (c) demoted objects re-promote after the derived cooldown;
//! (d) recalls batch (frames << recalls) under the derived rate cap;
//! (e) an un-acking client's grant is DEAD at the deadline and the object
//!     is grantable again (the S6/S7 composition: deadline ≤ the lease
//!     TTL, past which the client's own T_self self-fence has already
//!     fired — a timeout never waits on a zombie).
//!
//! Dark-by-default: nothing grants delegations yet, so every counter on
//! the GLOBAL lane is 0 BY CONSTRUCTION on every shipped mount, and the
//! wiring smoke proves the family is exported on a real filesystem's
//! `.stats` inode (in-process `SqueezefsFilesystem` — no root, no
//! `/dev/fuse`, the `metrics_tests.rs` fixture).

use squeezefs::meta_ship::{
    global_recall_lane, recall_batch_max_from, recall_cooldown_from, recall_deadline_from,
    recall_rate_cap_per_s, recall_stats_json, revoke_phase_json, GrantDecision, RecallConfig,
    RecallLane, RECALL_THRASH_CYCLES,
};
use std::time::{Duration, Instant};

/// A fixed test config: explicit values so every arm is deterministic
/// (time is INJECTED — no test sleeps; the house law).
fn cfg() -> RecallConfig {
    RecallConfig {
        deadline: Duration::from_secs(5),
        batch_max: 64,
        thrash_window: Duration::from_secs(60),
        thrash_cycles: RECALL_THRASH_CYCLES,
        cooldown: Duration::from_secs(120),
        valve: true,
    }
}

fn clients(n: usize) -> Vec<String> {
    // KD-MW-2 identity grammar: (node token, mount slot) pairs.
    (0..n).map(|i| format!("node_{i:016x}.m00000001")).collect()
}

/// One storm round: every client grabs the hot object, then a conflicting
/// mutation recalls every grant, the lane issues its frames, and every
/// framed client acks. Returns the number of grants that were ADMITTED
/// this round (0 once the valve has demoted).
fn storm_round(lane: &RecallLane, ino: u64, ids: &[String], now: Instant) -> usize {
    let mut granted = 0;
    for c in ids {
        if lane.try_grant(ino, c, now) == GrantDecision::Granted {
            granted += 1;
        }
    }
    lane.recall_object(ino, now);
    for f in lane.issue_pass(now) {
        lane.ack_frame(&f.client, f.frame_id, now);
    }
    granted
}

// ---------------------------------------------------------------------------
// (a) The RED HALF: without the valve the recall volume grows with the storm.
// ---------------------------------------------------------------------------

#[test]
fn spec_r5_red_half_without_the_valve_recall_volume_grows_with_the_storm() {
    let mut c = cfg();
    c.valve = false; // the valve-disabled SEAM (test-only; never a knob)
    let lane = RecallLane::with_config(c);
    let ids = clients(32);
    let t0 = Instant::now();
    let ino = 0xA11CE;

    for round in 0..6u32 {
        let now = t0 + Duration::from_millis(u64::from(round) * 10);
        assert_eq!(
            storm_round(&lane, ino, &ids, now),
            32,
            "without the valve every round grants — nothing ever demotes"
        );
    }
    let half = lane.stats().issued;
    assert_eq!(half, 6 * 32, "6 rounds × 32 holders, one recall each");

    for round in 6..12u32 {
        let now = t0 + Duration::from_millis(u64::from(round) * 10);
        storm_round(&lane, ino, &ids, now);
    }
    let full = lane.stats().issued;
    assert_eq!(
        full,
        2 * half,
        "the un-valved recall volume grows LINEARLY with the storm — \
         this is the fan-out R5 says must be braked"
    );
    assert_eq!(
        lane.stats().thrash_demotions,
        0,
        "the disabled valve must never demote (the red half's control)"
    );
}

// ---------------------------------------------------------------------------
// (b) The valve: demotion engages BEFORE the fan-out hurts, volume flattens.
// ---------------------------------------------------------------------------

#[test]
fn spec_r5_the_valve_demotes_the_hot_object_and_the_recall_volume_flattens() {
    let lane = RecallLane::with_config(cfg());
    let ids = clients(32);
    let t0 = Instant::now();
    let ino = 0xB0B;

    for round in 0..12u32 {
        let now = t0 + Duration::from_millis(u64::from(round) * 10);
        storm_round(&lane, ino, &ids, now);
    }
    let s = lane.stats();
    assert_eq!(s.thrash_demotions, 1, "the valve engaged exactly once");
    // "Before the fan-out hurts": the volume is bounded by the threshold —
    // thrash_cycles grant→recall episodes × N holders — never by the
    // storm's length.
    assert_eq!(
        s.issued,
        u64::from(RECALL_THRASH_CYCLES) * 32,
        "recall volume is capped at thrash_cycles episodes × holders"
    );
    assert!(
        s.grant_refusals >= 32,
        "the demoted object refuses grants (owner-served) — got {}",
        s.grant_refusals
    );

    // FLATNESS: more storm moves nothing.
    let before = s.issued;
    for round in 12..48u32 {
        let now = t0 + Duration::from_millis(u64::from(round) * 10);
        assert_eq!(
            storm_round(&lane, ino, &ids, now),
            0,
            "a demoted object grants nothing during its cooldown"
        );
    }
    assert_eq!(
        lane.stats().issued,
        before,
        "recall volume is FLAT once demoted — the R5 verdict"
    );
    assert_eq!(
        lane.stats().demoted_objects,
        1,
        "the gauge names the demotion"
    );
}

// ---------------------------------------------------------------------------
// (c) Re-promotion after the cooldown.
// ---------------------------------------------------------------------------

#[test]
fn a_demoted_object_repromotes_after_the_cooldown_and_thrash_state_resets() {
    let c = cfg();
    let lane = RecallLane::with_config(c);
    let ids = clients(4);
    let t0 = Instant::now();
    let ino = 0xC001;

    // Drive to demotion.
    let mut demoted_at = None;
    for round in 0..8u32 {
        let now = t0 + Duration::from_millis(u64::from(round) * 10);
        if storm_round(&lane, ino, &ids, now) == 0 {
            demoted_at = Some(now);
            break;
        }
    }
    let demoted_at = demoted_at.expect("the storm must demote the object");
    assert_eq!(lane.stats().thrash_demotions, 1);

    // Still inside the cooldown: refused, and the refusal names the
    // remaining time (rows 12+ surface it to the grant decision).
    match lane.try_grant(ino, &ids[0], demoted_at + Duration::from_secs(1)) {
        GrantDecision::Demoted { remaining } => {
            assert!(
                remaining <= c.cooldown,
                "remaining ≤ the derived cooldown ({remaining:?})"
            );
        }
        GrantDecision::Granted => panic!("a grant inside the cooldown window"),
    }

    // Past the cooldown: the first attempt RE-PROMOTES.
    let after = demoted_at + c.cooldown + Duration::from_millis(1);
    assert_eq!(
        lane.try_grant(ino, &ids[0], after),
        GrantDecision::Granted,
        "the first grant attempt past the cooldown re-promotes"
    );
    assert_eq!(lane.stats().repromotions, 1);

    // Thrash state reset: one fresh recall→grant cycle does NOT
    // immediately re-demote (cycles restarted from zero).
    lane.recall_object(ino, after);
    for f in lane.issue_pass(after) {
        lane.ack_frame(&f.client, f.frame_id, after);
    }
    assert_eq!(
        lane.try_grant(ino, &ids[0], after + Duration::from_millis(1)),
        GrantDecision::Granted,
        "one post-repromotion cycle is not a verdict (cycles reset)"
    );
    assert_eq!(lane.stats().thrash_demotions, 1, "no second demotion yet");
}

// ---------------------------------------------------------------------------
// (d) Batching: one frame carries many recalls, under the derived rate cap.
// ---------------------------------------------------------------------------

#[test]
fn recalls_batch_one_frame_carries_many_recalls_under_the_rate_cap() {
    let lane = RecallLane::with_config(cfg()); // batch_max = 64
    let client = &clients(1)[0];
    let t0 = Instant::now();
    let total = 500u64;

    for ino in 0..total {
        assert_eq!(lane.try_grant(ino, client, t0), GrantDecision::Granted);
    }
    for ino in 0..total {
        assert_eq!(lane.recall_object(ino, t0), 1);
    }

    // The rate discipline: ONE in-flight frame per client. A second pass
    // while the first is un-acked issues NOTHING (and counts the deferral).
    let first = lane.issue_pass(t0);
    assert_eq!(first.len(), 1, "one client ⇒ one frame per pass");
    assert_eq!(
        first[0].inos.len(),
        64,
        "the frame carries a full batch (the derived cap)"
    );
    assert!(
        lane.issue_pass(t0 + Duration::from_millis(1)).is_empty(),
        "an un-acked client is never issued a second frame — the rate \
         limiter IS the one-in-flight-frame discipline"
    );
    assert!(
        lane.stats().rate_deferred > 0,
        "held-back recalls are counted (the limiter's engagement gauge)"
    );

    // Drain: ack → next frame, until done. Frames << recalls.
    lane.ack_frame(
        &first[0].client,
        first[0].frame_id,
        t0 + Duration::from_millis(2),
    );
    let mut now = t0 + Duration::from_millis(3);
    loop {
        let frames = lane.issue_pass(now);
        if frames.is_empty() {
            break;
        }
        for f in frames {
            assert!(f.inos.len() <= 64, "no frame exceeds the batch cap");
            lane.ack_frame(&f.client, f.frame_id, now);
        }
        now += Duration::from_millis(1);
    }
    let s = lane.stats();
    assert_eq!(s.issued, total, "every recall issued exactly once");
    assert_eq!(s.acked, total, "every recall acked exactly once");
    assert_eq!(
        s.frames,
        total.div_ceil(64),
        "frames = ceil(recalls / batch_max) — the batching LAW: \
         {} frames carried {} recalls",
        s.frames,
        s.issued
    );
    assert_eq!(s.outstanding, 0, "every grant surrendered");
    assert_eq!(s.pending, 0, "nothing left queued");

    // The phase table recorded every recall's issue + ack_wait + total.
    let phases = lane.phase_json();
    for arm in ["issue", "ack_wait", "total"] {
        let sum: u64 = phases[arm]
            .as_object()
            .expect("phase arm is a histogram object")
            .values()
            .map(|v| v.as_u64().unwrap_or(0))
            .sum();
        assert_eq!(sum, total, "dlm_revoke_phase_ns.{arm} saw every recall");
    }
}

// ---------------------------------------------------------------------------
// (e) The timeout arm: an un-acking client's grant dies at the deadline.
// ---------------------------------------------------------------------------

#[test]
fn an_unacked_recall_dies_at_the_deadline_and_the_object_is_grantable_again() {
    let c = cfg();
    let lane = RecallLane::with_config(c);
    let ids = clients(2);
    let t0 = Instant::now();
    let ino = 0xDEAD;

    assert_eq!(lane.try_grant(ino, &ids[0], t0), GrantDecision::Granted);
    assert_eq!(lane.recall_object(ino, t0), 1);
    let frames = lane.issue_pass(t0);
    assert_eq!(frames.len(), 1);

    // Before the deadline: nothing expires (a slow drain is not a corpse).
    assert!(
        lane.expire_overdue(t0 + c.deadline - Duration::from_millis(1))
            .is_empty(),
        "a recall inside its deadline is never declared dead"
    );

    // At the deadline: the grant is DEAD. The S6/S7 composition that makes
    // this sound: deadline ≤ the membership lease TTL (the derivation's
    // ceiling), and the client's own T_self fires STRICTLY before the
    // owner's TTL — so by the time the lane declares the grant dead, a
    // live-but-partitioned holder has already self-fenced and a dead one
    // never answers. Nothing here waits on a zombie.
    let dead = lane.expire_overdue(t0 + c.deadline);
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].client, ids[0]);
    assert_eq!(dead[0].ino, ino);
    let s = lane.stats();
    assert_eq!(s.timed_out, 1, "dlm_revokes_timed_out counts the death");
    assert_eq!(s.acked, 0);
    assert_eq!(lane.holders(ino), 0, "the dead grant left the table");

    // The object is grantable again — to another client, immediately.
    assert_eq!(
        lane.try_grant(ino, &ids[1], t0 + c.deadline + Duration::from_millis(1)),
        GrantDecision::Granted,
        "a timed-out recall frees the object for the next grant"
    );

    // And the timed-out client is not wedged: its lane accepts new frames.
    lane.recall_object(ino, t0 + c.deadline + Duration::from_millis(2));
    let next = lane.issue_pass(t0 + c.deadline + Duration::from_millis(3));
    assert_eq!(
        next.len(),
        1,
        "the in-flight slot was cleared by the expiry"
    );
}

// ---------------------------------------------------------------------------
// Coalescing + protocol hygiene.
// ---------------------------------------------------------------------------

#[test]
fn a_second_recall_for_an_already_pending_object_coalesces() {
    let lane = RecallLane::with_config(cfg());
    let client = &clients(1)[0];
    let t0 = Instant::now();

    assert_eq!(lane.try_grant(7, client, t0), GrantDecision::Granted);
    assert_eq!(lane.recall_object(7, t0), 1, "first recall enqueues");
    assert_eq!(
        lane.recall_object(7, t0 + Duration::from_millis(1)),
        0,
        "a recall already pending for (object, client) is never re-queued"
    );
    assert_eq!(lane.stats().coalesced, 1);
    assert_eq!(lane.stats().pending, 1, "still exactly one queued recall");

    // Same law while the recall is IN FLIGHT.
    let frames = lane.issue_pass(t0 + Duration::from_millis(2));
    assert_eq!(frames.len(), 1);
    assert_eq!(
        lane.recall_object(7, t0 + Duration::from_millis(3)),
        0,
        "an in-flight recall coalesces too"
    );
    assert_eq!(lane.stats().coalesced, 2);
}

#[test]
fn an_ack_for_an_unknown_frame_is_counted_and_changes_nothing() {
    let lane = RecallLane::with_config(cfg());
    let client = &clients(1)[0];
    let t0 = Instant::now();

    assert_eq!(lane.try_grant(9, client, t0), GrantDecision::Granted);
    lane.recall_object(9, t0);
    let frames = lane.issue_pass(t0);
    assert_eq!(frames.len(), 1);

    lane.ack_frame(client, frames[0].frame_id + 1000, t0); // wrong id
    lane.ack_frame("node_ffffffffffffffff.m00000000", frames[0].frame_id, t0); // wrong client
    let s = lane.stats();
    assert_eq!(s.stale_acks, 2, "both bogus acks counted");
    assert_eq!(s.acked, 0, "neither resolved anything");
    assert_eq!(lane.holders(9), 1, "the grant is still outstanding");
}

// ---------------------------------------------------------------------------
// Dark posture + the structural (never-operational) valve.
// ---------------------------------------------------------------------------

#[test]
fn the_dark_posture_exports_the_family_as_zeros_and_pays_nothing() {
    // The GLOBAL lane: nothing grants delegations yet (rows 12–14), so on
    // every shipped mount — and in this whole binary, whose storm tests
    // use PRIVATE lanes — the family is 0 BY CONSTRUCTION.
    let s = global_recall_lane().stats();
    assert_eq!(s.issued, 0);
    assert_eq!(s.acked, 0);
    assert_eq!(s.timed_out, 0);
    assert_eq!(s.frames, 0);
    assert_eq!(s.coalesced, 0);
    assert_eq!(s.rate_deferred, 0);
    assert_eq!(s.stale_acks, 0);
    assert_eq!(s.grants, 0);
    assert_eq!(s.grant_refusals, 0);
    assert_eq!(s.thrash_demotions, 0);
    assert_eq!(s.repromotions, 0);
    assert_eq!(s.outstanding, 0);
    assert_eq!(s.pending, 0);
    assert_eq!(s.demoted_objects, 0);

    // The stats family exports NOW (rows 12–14 inherit the instrument):
    // the spec's spellings, all zero, with the derived caps published as
    // gauges (the "published so the docs cannot drift" pattern).
    let j = recall_stats_json();
    for key in [
        "dlm_revokes_issued",
        "dlm_revokes_acked",
        "dlm_revokes_timed_out",
        "dlm_recall_frames",
        "dlm_recall_coalesced",
        "dlm_recall_rate_deferred",
        "dlm_recall_stale_acks",
        "dlm_recall_grants",
        "dlm_recall_grant_refusals",
        "dlm_thrash_demotions",
        "dlm_recall_repromotions",
        "dlm_recall_outstanding",
        "dlm_recall_pending",
        "dlm_recall_demoted_objects",
    ] {
        assert_eq!(
            j[key].as_u64(),
            Some(0),
            "dark posture: {key} must export and be 0"
        );
    }
    for gauge in [
        "dlm_recall_deadline_ms",
        "dlm_recall_batch_max",
        "dlm_recall_cooldown_ms",
        "dlm_recall_rate_cap_per_s",
    ] {
        assert!(
            j[gauge].as_u64().is_some_and(|v| v > 0),
            "derived gauge {gauge} publishes its arithmetic"
        );
    }

    // The phase table exports its three arms, all empty.
    let p = revoke_phase_json();
    for arm in ["issue", "ack_wait", "total"] {
        let sum: u64 = p[arm]
            .as_object()
            .expect("phase arm present")
            .values()
            .map(|v| v.as_u64().unwrap_or(0))
            .sum();
        assert_eq!(sum, 0, "dark posture: dlm_revoke_phase_ns.{arm} is empty");
    }
}

#[test]
fn the_valve_is_structural_never_operational() {
    // The brake cannot be removed by environment: no such knob EXISTS
    // (the valve-disabled seam is a test-only constructor field). The
    // name is assembled at runtime so the ENG-10 census scan never reads
    // a knob literal that must never be registered.
    let name = format!("SQUEEZEFS_DLM_RECALL_{}", "VALVE");
    assert!(
        squeezefs::env_knobs::lookup(&name).is_none(),
        "the thrash valve must not be operationally removable — \
         delegation can never ship without its brake"
    );
    assert!(
        RecallConfig::derived().valve,
        "the derived config always arms the valve"
    );

    // The three measurement levers ARE registered (ENG-10: adding a knob
    // means adding a registry entry).
    for knob in [
        "SQUEEZEFS_DLM_RECALL_BATCH_MAX",
        "SQUEEZEFS_DLM_RECALL_DEADLINE_MS",
        "SQUEEZEFS_DLM_RECALL_COOLDOWN_MS",
    ] {
        assert!(
            squeezefs::env_knobs::lookup(knob).is_some(),
            "{knob} must be in the ENG-10 registry"
        );
    }
}

// ---------------------------------------------------------------------------
// Derivations: drift-is-red ties, explicit-wins-verbatim, the p99 servant.
// ---------------------------------------------------------------------------

#[test]
fn derivations_tie_to_their_inputs_and_explicit_levers_win_verbatim() {
    // batch_max = frame_cap / headroom(2) / entry_bytes(32): the CONTROL
    // class cap is the wire's own bound, so at the shipped 1 MiB cap one
    // frame carries 16,384 recalls — R6's whole-token-set-per-frame law
    // fits 16× over at the spec's 1 k tokens/client shape.
    assert_eq!(recall_batch_max_from(None, 1024 * 1024), 16_384);
    assert_eq!(
        recall_batch_max_from(None, squeezefs::cluster_wire::CONTROL_MAX_FRAME_BYTES),
        16_384,
        "the live wire cap derives the shipped batch"
    );
    assert_eq!(
        recall_batch_max_from(Some(8), 1024 * 1024),
        8,
        "explicit wins"
    );
    assert_eq!(recall_batch_max_from(None, 64), 1, "floor: never a 0 batch");

    // deadline: no live evidence ⇒ the lease TTL (the fence bound is the
    // only derivable bound with zero samples); live evidence ⇒ 4 × p99
    // PLUS the wire's structural delivery term (rung-12 live finding #3:
    // recalls travel on the holder's STANDING POLL, whose turnaround —
    // deliver round + ack round, one park floor each — exists at zero
    // load; the un-termed derivation read 1 ms on a loopback fleet and a
    // HEALTHY holder was timed out and membership-evicted before its
    // poll could possibly answer), ceilinged at the TTL.
    let ttl = Duration::from_secs(45);
    let delivery = Duration::from_millis(200); // 2 × the 100 ms park floor
    assert_eq!(recall_deadline_from(None, None, ttl), ttl);
    assert_eq!(
        recall_deadline_from(None, Some(360), ttl),
        Duration::from_micros(4 * 360) + delivery,
        "4 × (rtt_p99 + owner_total_p99) + the poll delivery term"
    );
    assert_eq!(
        recall_deadline_from(None, Some(10), ttl),
        Duration::from_micros(40) + delivery,
        "the delivery term is STRUCTURAL: even negligible load evidence \
         must never price the deadline below one deliver+ack poll \
         turnaround (the healthy-holder-evicted class)"
    );
    assert_eq!(
        recall_deadline_from(None, Some(20_000_000), ttl),
        ttl,
        "the lease-TTL ceiling (the fence arithmetic takes over there)"
    );
    assert_eq!(
        recall_deadline_from(Some(250), Some(360), ttl),
        Duration::from_millis(250),
        "explicit wins verbatim"
    );

    // cooldown = 8 × thrash window (bounds worst-case residual thrash duty
    // at cycles/(cycles+8) of the un-valved volume).
    assert_eq!(
        recall_cooldown_from(None, Duration::from_secs(5)),
        Duration::from_secs(40)
    );
    assert_eq!(
        recall_cooldown_from(Some(7000), Duration::from_secs(5)),
        Duration::from_secs(7),
        "explicit wins verbatim"
    );

    // The published aggregate rate cap: roster × batch / deadline — wire
    // budget × lease arithmetic × roster size, no free constant.
    assert_eq!(
        recall_rate_cap_per_s(64, Duration::from_secs(1), 4),
        256,
        "4 clients × 64/s each"
    );
    assert_eq!(
        recall_rate_cap_per_s(64, Duration::from_secs(1), 0),
        64,
        "roster floors at 1 (a lane with no roster still has its holder)"
    );

    // The derived config composes the pieces: window = deadline (the
    // lane's own completion horizon), cycles = the settle-law constant.
    let d = RecallConfig::derived();
    assert_eq!(d.thrash_window, d.deadline);
    assert_eq!(d.thrash_cycles, RECALL_THRASH_CYCLES);
    assert!(d.batch_max >= 1);
}

#[test]
fn latency_histogram_p99_serves_the_deadline_derivation() {
    use squeezefs::fuse_client::LatencyHistogram;
    let h = LatencyHistogram::default();
    assert_eq!(h.p99_micros(), None, "an empty histogram derives nothing");
    for _ in 0..99 {
        h.record(Duration::from_micros(100));
    }
    assert_eq!(
        h.p99_micros(),
        Some(128),
        "single-population p99 = its own bucket bound (≤128 µs)"
    );
    // Nearest-rank law: a 1-in-100 outlier sits at rank 100, ABOVE the
    // p99 rank (ceil(0.99 × 100) = 99) — the p99 stays in the fast bucket
    // (the deadline's ×4 margin is what absorbs sub-1 % tails) …
    h.record(Duration::from_millis(4));
    assert_eq!(
        h.p99_micros(),
        Some(128),
        "a sub-1 % tail never moves the p99 (nearest-rank)"
    );
    // … while a ≥1 % tail DOES move it into the tail bucket.
    h.record(Duration::from_millis(4));
    h.record(Duration::from_millis(4));
    assert_eq!(
        h.p99_micros(),
        Some(4096),
        "a ≥1 % tail is the p99 bucket (rank 101 of 102 lands in ≤4 ms)"
    );
}

// ---------------------------------------------------------------------------
// The wiring smoke: a REAL filesystem's `.stats` inode carries the family
// (in-process SqueezefsFilesystem — the machinery is dark until delegation,
// so the live leg proves WIRING, not behavior).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_stats_inode_carries_the_recall_family_on_a_real_filesystem() {
    use fuse3::raw::prelude::Filesystem;
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::fuse_client::{SqueezefsFilesystem, STATS_INODE};
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    async fn open_v3_meta(
        path: &std::path::Path,
    ) -> Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
        squeezefs::meta_backend::kv::builder::format_v3(
            path,
            256 * 1024 * 1024,
            &squeezefs::meta_backend::kv::builder::FormatV3Options {
                node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
                journal_len_override: None,
                force: true,
                full_wipe: false,
                format_config_xattr: None,
            },
        )
        .await
        .expect("format v3 meta volume");
        squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
            .await
            .expect("open v3 meta volume")
    }

    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("recall_valve_wiring").await.unwrap());
    let s = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path()).await,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());

    let req = fuse3::raw::Request {
        unique: 0,
        uid: 1000,
        gid: 1000,
        pid: 1,
        ..Default::default()
    };
    let _entry = fs
        .lookup(req, 1, std::ffi::OsStr::new(".stats"))
        .await
        .expect("lookup .stats");
    let opened = fs
        .open(req, STATS_INODE, libc::O_RDONLY as u32, 0)
        .await
        .expect("open .stats");
    let data = fs
        .read(req, STATS_INODE, opened.fh, 0, 16 * 1024 * 1024, 0)
        .await
        .expect("read .stats");
    let parsed: serde_json::Value =
        serde_json::from_slice(&data.data).expect(".stats parses as JSON");

    // The dark posture on a REAL filesystem: the family is present, 0.
    for (ptr, why) in [
        (
            "/metrics/dlm_recall/dlm_revokes_issued",
            "recall issue counter",
        ),
        (
            "/metrics/dlm_recall/dlm_revokes_acked",
            "recall ack counter",
        ),
        (
            "/metrics/dlm_recall/dlm_revokes_timed_out",
            "recall timeout counter",
        ),
        (
            "/metrics/dlm_recall/dlm_thrash_demotions",
            "the valve's engagement counter (spec R5's instrument)",
        ),
    ] {
        assert_eq!(
            parsed.pointer(ptr).and_then(|v| v.as_u64()),
            Some(0),
            "{why} ({ptr}) must export as 0 on a shipped (no-delegation) mount"
        );
    }
    assert!(
        parsed
            .pointer("/metrics/dlm_revoke_phase_ns/issue")
            .is_some_and(|v| v.is_object()),
        "dlm_revoke_phase_ns rides the stats inode beside the meta_ship tables"
    );
    fs.release(req, STATS_INODE, opened.fh, 0, 0, false)
        .await
        .expect("release .stats");
}
