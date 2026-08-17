# 2026-08-17 — Rung 11: the S10 RECALL LANE + THRASH VALVE ("brake before engine")

**Branch** `feat/s10-recall-valve` (worktree off dev `d926e45b`). Charter:
`docs/design-full-multi-writer.md` §8 (the recall law) + PR row 11 —
*"Ceph-pattern recall lane + `dlm_thrash_demotions` valve; red-first
hot-object storm test (spec R5) — lands BEFORE grants exist so delegation
can never ship without its brake"*; `docs/pre-rc-engineering-spec.md`
§6.10 **R5** (*"verify the demotion valve engages before the fan-out
hurts"*) and **R3** (deadlines derive from LIVE p99, never a constant).
Deliberately sequenced ahead of rows 12–14: no delegation grant exists
yet, so the population arrives only through the test seams and every
shipped mount is structurally unchanged (the dark-posture pin).

**Red-first discipline.** The whole suite
(`tests/mw_recall_valve_tests.rs`) landed first, failing against the
missing API (commit `5bb76188` — the suite does not compile at that
commit; the red state was captured before a line of implementation was
written). The spec-R5 red HALF is permanent: arm (a) runs the identical
storm against the **valve-disabled seam** (`RecallConfig { valve: false }`,
a test-only constructor field — deliberately NOT a knob) and proves the
un-braked fan-out grows linearly forever.

---

## What landed

* **The recall lane** (`src/meta_ship/tokens.rs` — the module the design's
  §8 table names): owner-side grant bookkeeping (`try_grant`), owner-
  initiated recall of every outstanding grant on an object
  (`recall_object`, per-(object, client) coalescing), the **batched,
  rate-limited issue pass** (`issue_pass` — one frame per client, ≤
  `batch_max` entries, at most ONE in-flight frame per client; the caller
  puts frames on the wire, which is exactly where row 12's `DelegRecall`
  verb plugs in), ack correlation (`ack_frame`, whole-frame acks, stale
  acks counted and ignored — the S8-dedup posture), and the **deadline
  sweep** (`expire_overdue` — a timed-out recall's grant is DEAD, loudly,
  and the object grantable again; the returned list is rows 12+'s
  eviction-escalation input).
* **The thrash valve** (Ceph pattern, spec R5): one cycle is counted per
  grant-after-recall EPISODE (the first re-grant inside the thrash window
  after a recall — never per holder, so a 32-holder re-grant wave is ONE
  cycle). At `thrash_cycles` consecutive cycles the object **demotes to
  owner-served** for the derived cooldown (`dlm_thrash_demotions`);
  grants during the cooldown refuse with the remaining time; the first
  attempt past the cooldown **re-promotes** with the evidence reset.
* **The stats family** (`dlm_recall` object + `dlm_revoke_phase_ns` on
  the stats inode, emitted beside the `meta_ship` tables): registered NOW
  so rows 12–14 inherit the instrument; all-zero on every shipped mount
  BY CONSTRUCTION.
* **Three ENG-10 measurement levers** (`SQUEEZEFS_DLM_RECALL_BATCH_MAX`,
  `SQUEEZEFS_DLM_RECALL_DEADLINE_MS`, `SQUEEZEFS_DLM_RECALL_COOLDOWN_MS`)
  with derived defaults; **the valve itself has no knob** — pinned
  (`the_valve_is_structural_never_operational`): the brake must not be
  operationally removable, which is the entire reason this rung precedes
  the engine.
* `LatencyHistogram::p99_micros` (`src/fuse_client.rs`) — the nearest-rank
  bucket-bound p99 read the deadline derivation consumes (bucket-quantized
  up to 2×, which the deadline's ×4 margin documents and absorbs).

## The derivation arithmetic (no free-floating constants)

| Quantity | Derivation | Shipped value | Reason on the line |
|---|---|---|---|
| `batch_max` | `CONTROL_MAX_FRAME_BYTES / 2 / 32 B` | **16,384 recalls/frame** | the wire's own CONTROL-class cap; ÷2 header/MAC headroom (the S8 encode check's split); 32 B = measured ≤ 20 B bincode entry, power-of-two ceiling. Spec R6's "one frame per client carrying its whole token set" fits the 1 k-token shape 16× over |
| `deadline` | `clamp(4 × (rtt_p99 + owner_total_p99), 1 ms, lease TTL)`; **no samples ⇒ the TTL** | 45 s dark (no evidence), ~1.4 ms at the S8 note's 250 µs RTT + ~110 µs owner-frame shape | spec R3: LIVE `meta_ship_phase_ns.rtt` + `meta_ship_owner_phase_ns.total` p99s (the two terms a drain-and-ack traverses). ×4 = two binary octaves: the histogram's power-of-two buckets make a p99 read up to 2× coarse, one more octave covers the drain the p99 predates; a deadline AT p99 would falsely kill ~1 % of healthy recalls. Floor = the 1 ms timer grain (unmeasurable below). Ceiling = the membership lease TTL — past it the S6/S7 fence arithmetic bounds the client anyway |
| `thrash_window` | `= deadline` | — | the lane's own completion horizon: an object re-granted before its recall round could even complete is cycling faster than the mechanism serves |
| `thrash_cycles` | **3** (constant, documented) | 3 | the smallest run separating a PATTERN from a coincidence — one cycle is any legitimate writer conflict, two can be one conflict's retry (the fsck verify-before-report settle posture: single evidence is never a verdict) |
| `cooldown` | `8 × thrash_window` | 360 s dark | bounds the worst-case residual thrash duty at `cycles/(cycles+8)` ≈ 27 % of the un-valved volume for a permanently hot object; a cooled object re-promotes within one decade of the detection horizon (the AIMD-retreat class) |
| rate cap (published gauge) | `roster × batch_max / deadline` | `dlm_recall_rate_cap_per_s` | ENFORCED structurally (one in-flight frame of ≤ batch_max per client per ack/deadline round — `batch_max/deadline` per client, derived at both ends); the gauge is the free_grace-style publish-the-derivation so the docs cannot drift. Roster = the armed membership census (`MembershipOwner::len`), floored at the live holder population |

Lease TTL source: the armed membership plane's own `T_owner` when
installed, else the same `SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS`/45 s default
`LeaseClocks::derive` reads — the two planes cannot disagree about what
"the lease" means.

**The timeout composition (S6/S7), stated:** deadline ≤ `T_owner`
(ceiling), and a member's own `T_self = T_owner − 2·skew_max − D_purge`
fires STRICTLY before the owner's TTL — so a holder that could not ack
inside the deadline has either died or self-fenced before the owner may
act as if the grant were gone; its in-flight DMA is S7's dead-epoch
quarantine's problem, never this table's. Every expiry logs loud (the
`transport_lease_overlong` precedent — never a silent wait); rows 12+
escalate the returned `TimedOutRecall`s to membership eviction.

## The storm table (spec R5's verdict — in-process, 32 simulated clients × one hot object)

Instrument: `tests/mw_recall_valve_tests.rs`, injected time (no sleeps),
fixed config (deadline 5 s / batch 64 / window 60 s / cycles 3 / cooldown
120 s). One round = every client grants, a conflicting mutation recalls
every grant, the lane issues, every client acks.

| Rounds | valve OFF (the red half): recalls issued | valve ON: recalls issued | valve ON: demotions |
|---|---|---|---|
| 6 | 192 (= 6 × 32, linear) | 96 (= **3 × 32**, capped) | 1 (engaged round 3) |
| 12 | 384 (= 12 × 32, linear) | 96 (**FLAT**) | 1 |
| 48 | (extrapolates 1,536) | 96 (**FLAT** — pinned through round 48) | 1 |

The valve engages at exactly `thrash_cycles` grant→recall episodes —
**before the fan-out hurts**: the recall volume is bounded by
`thrash_cycles × holders`, never by the storm's length. Re-promotion
after the cooldown is pinned (arm c), with the thrash evidence reset so
one post-repromotion cycle is not a verdict. Batching law pinned (arm d):
500 recalls to one client at batch 64 ⇒ **8 frames** (`ceil(500/64)`),
every phase arm (`issue`/`ack_wait`/`total`) accounting all 500; a second
pass against an un-acked client issues NOTHING (`rate_deferred` counts the
holdback). Timeout arm (e): dead at the deadline, grantable again, the
timed-out client's rate slot freed.

## The API contract rows 12–14 consume (frozen here)

```text
meta_ship::global_recall_lane() -> &'static RecallLane   // the lane the stats inode exports
RecallLane::try_grant(ino, client, now) -> Granted | Demoted{remaining}
    // call FIRST at grant issuance; Demoted = owner-served, do not issue.
    // Roster admission (only an ADMITTED member holds a delegation) is the
    // CALLER's — the lane bookkeeps what the grant path admitted.
RecallLane::recall_object(ino, now) -> usize   // owner-initiated, coalescing
RecallLane::issue_pass(now) -> Vec<RecallFrame{client, frame_id, inos}>
    // row 12's DelegRecall wire half drains this; one frame per client
RecallLane::ack_frame(client, frame_id, now)   // whole-frame ack correlation
RecallLane::expire_overdue(now) -> Vec<TimedOutRecall>
    // cadence sweep; the return is the eviction-escalation input
RecallConfig::derived()                        // live re-derivation (spec R3)
RecallLane::with_config(cfg)                   // pinned (tests / measurement)
```

Stats: `dlm_recall.{dlm_revokes_issued, dlm_revokes_acked,
dlm_revokes_timed_out, dlm_recall_frames, dlm_recall_coalesced,
dlm_recall_rate_deferred, dlm_recall_stale_acks, dlm_recall_grants,
dlm_recall_grant_refusals, dlm_thrash_demotions, dlm_recall_repromotions,
dlm_recall_outstanding, dlm_recall_pending, dlm_recall_demoted_objects}`
plus the published derivations `{dlm_recall_deadline_ms,
dlm_recall_batch_max, dlm_recall_cooldown_ms, dlm_recall_rate_cap_per_s}`,
and `dlm_revoke_phase_ns.{issue, ack_wait, total}` on the shared 26-bucket
latency core.

**Spelling adjudication (charter: reuse/extend, never fork):** the spec
§6.9 family `dlm_revokes_{issued,acked,timed_out}` was PARTIALLY landed by
S9 as `dlm_custody.dlm_revokes_{issued,expired}` — the custody plane's
PULL-model face (no ack channel; expiry is the terminal). This rung lands
the PUSH-model recall lane's face under its own `dlm_recall` object with
the same family spelling: same spelling, scoped by their objects, never a
second name for one thing (`data_grant.rs` untouched). The design doc's
§Observability spellings (`dlm_delegation_recall_*`) belong to rows 12–14's
delegation-scoped counters, which will COMPOSE with this lane's.

## Dark-by-default (the solo/no-delegation posture pays nothing)

* Nothing populates the global lane — no production call site exists; the
  lane is reachable only through rows 12–14's future grant path and the
  test seams. Pins: `the_dark_posture_exports_the_family_as_zeros_and_
  pays_nothing` (global lane all-zero + the family exports with the
  derived gauges published) and `the_stats_inode_carries_the_recall_
  family_on_a_real_filesystem` — the wiring smoke: an in-process
  `SqueezefsFilesystem` (real stats inode, no root, no `/dev/fuse` — the
  `metrics_tests.rs` fixture) serves `/metrics/dlm_recall/*` = 0 and
  `/metrics/dlm_revoke_phase_ns/issue` present. A heavier live leg was
  deliberately NOT added: the machinery is dark until delegation, so a
  mounted leg would prove the same wiring for more rig cost.
* Cost on a shipped mount: one `Lazy` global, one stats-JSON read per
  `.stats` render. No hot-path touch, no lock any production path takes.

## Gates run

* `cargo test --test mw_recall_valve_tests` — 12/12 green (red first:
  `5bb76188` fails to compile by construction).
* Touched suites serial (`--test-threads=1`): `mw_recall_valve_tests`,
  `meta_ship_tests`, `mw_arm_s8_tests`, `env_knob_convention_tests`,
  `derivation_sweep_tests`, `metrics_tests`, `skip_ledger_tests` — green
  (table in the closing commit; run log below).
* `cargo clippy --all-targets --all-features -- -D warnings` AND
  `cargo clippy --all-targets -- -D warnings` (the shipped config) — clean.
* `cargo fmt --check` — clean. Markdown link check (`task check:docs`
  script) — clean. No shell scripts touched. No new lock-free core (the
  lane composes `parking_lot::Mutex` + existing atomics/histograms —
  control-plane, lease-transaction class), so no new loom model per the
  charter's preference.
* Full `task check` DEFERRED per the standing user ruling for this ladder.

## Residuals (stated, not hidden)

1. **The wire half does not exist** (by design): `issue_pass` returns
   frames and `ack_frame` consumes correlations — row 12's `DelegGrant`/
   `DelegRecall`/`DelegReassert` verbs (schema bump) are the transport.
   The lane is the arbiter only; nothing here serializes onto
   `cluster_wire` yet.
2. **Eviction escalation is a return value, not an act**: `expire_overdue`
   hands back the dead recalls; wiring them to `MembershipOwner::evict`
   (minting the S7 dead epoch) belongs to the row that arms delegation —
   the lane must not evict members no delegation plane admitted.
3. **Whole-frame acks**: partial acks (a client draining object-by-object)
   are a row-12 refinement if the storm row prices them in; the S8 dedup
   posture covers resends today.
4. **Demotion does not proactively recall the remaining holders** — the
   next conflicting mutation does. If rows 12–14's storm pricing wants
   demote-recalls-now, it is one call at the demote site.
5. The `RECALL_THRASH_CYCLES = 3` and `RECALL_COOLDOWN_WINDOWS = 8`
   constants are documented-reason clamps (settle-law / duty-cycle-bound
   classes), not measured wins; the PR-13 storm row is where a counted
   retune would land, with `SQUEEZEFS_DLM_RECALL_COOLDOWN_MS` as the
   measurement lever.
