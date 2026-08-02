# 2026-08-04 — Rewrite program P0: design + Ideas 17/4/2/1 landed

Branch `feat/rewrite-program-p0` (off dev tip `abc983f`, **unmerged —
the orchestrator merges**). Charter (leadership, 2026-08-02, verbatim
governing sentence): *"sequential overwrite of existing striped data
must match fresh ingest device-byte rate (±5%) and pay zero discards
during the row; loop-rewrite must be latest-wins (device writes ≈
unique blocks, not ops). Vehicles: shadow dual-map + data-plane
supersession + discard elision / substrate-probed inplace. Durability
is a named mount class, not an accident of CoW."*

Commits: design `a0f38a4` (`docs/design-rewrite-program.md`, Rev 1 —
keyed decisions + the W1–W6 crash-window table) · Idea 17 red `2dcc3a6`
/ green `af74ec0` · Idea 4 red `4508034` / green `464ea02` · Idea 2 red
`045f3c7` / green `e8fb912` · Idea 1 red `ece02fe` / green `4b9a4fc` ·
fmt `d3d0cb5` · this note. Ideas 6/7/8 (substrate probe /
intent-inplace / named durability classes) are DESIGN sections only
(§9 there) — implementation next campaign, per the charter. Out of
scope per the do-not-staff list: reclaim lane width,
inplace-default-on-zram, publish aggregation.

## 1. What landed (implemented in the charter's instrument-first order)

### Idea 17 — the `rewrite_amp` SLO (the acceptance instrument, first)

* Formula: `rewrite_amp = device_write_bytes / user_overwrite_bytes`
  per row, with `discard_bytes_during_row` surfaced alongside;
  per-row measurable via diskstats + stats-inode deltas (the standing
  amplification instrument, extended).
* Daemon attribution (vehicle-blind — CoW displacement, in-place
  replacement, shadow records all count): `rewrite_blocks`,
  `rewrite_user_bytes`, `rewrite_device_write_bytes`. A fresh row's
  deltas must stay 0 (pinned).
* `tests/write_amp_rig.sh` rows: `seq_overwrite_1m` (charter gates
  enforced under `SQZ_WA_REWRITE_GATE=1` default: REWRITE_AMP ≤ 1.05
  AND mid-row `d_ops == 0`) and `loop_rewrite` (device/unique ratio +
  coalesce factor + supersession engagement; report-only on the
  non-overlapping face until Idea 8 — design §2.2).
* Contracts: `tests/rewrite_amp_tests.rs` (3).

### Idea 4 — discard elision until pressure

* BdevDiscard-class terminal frees skip the reclaim queue entirely:
  `begin_free → tier purge → debt record → finish_free` — immediately
  reallocatable, **zero device commands during rows** (structural, not
  manners-lucky). FilePunch backings keep the queued reclaimer (the
  host-FS ENOSPC motivation, KD-4.1).
* Debt is RAM-only, cancelled at claim (`claim_block_idx` —
  claim-cancels-debt), drained at the trim venues: idle (drain to
  zero), pressure (the constant-free watermark `debt ≤ virgin tail`,
  KD-4.6 — paced drain past it, `block_free_debt_pressure_drains`),
  and `BackendRouter::trim_elided(full)` (the fstrim/defrag face;
  `full` trims the whole free list — the durable truth, so crash-lost
  debt is always recoverable hygiene, KD-4.9).
* Trim protocol (KD-4.4): claim OUT of the free list (the allocation
  claim + a live in-flight registration for the window) → issue →
  return. A discard can never race a new owner's DMA; a mid-trim
  offset is shielded from fsck C6.
* Ledger identity: `queued + elided ≡ terminal frees`. Lever:
  `SQUEEZEFS_DISCARD_ELISION=0` = the queued path verbatim. Fenced
  daemons never trim (the reclaimer's fence-halt law).
* Counters: `block_free_reclaim_elided`,
  `block_free_elided_debt_bytes` (gauge), `block_free_trim_discards`,
  `block_free_trim_bytes`, `block_free_debt_pressure_drains`.
* Contracts: `tests/discard_elision_tests.rs` (7).

### Idea 2 — latest-wins supersession (the safe-today subset)

* The pipeline upload no longer holds `BLOCK_FLUSH_LOCKS` across the
  fabric RTT: snapshot + write-epoch stamp under the lock → device
  phase (crypto → allocate → DMA → incarnation publish) UNLOCKED
  against fresh unpublished state → revalidate-then-publish. A stale
  completion (newer merge / retired entry) publishes NOTHING: frees
  its never-map-named orphan and re-drives from the newest parked
  bytes (the retained dirty authority). Overlapping rewrites pay the
  device once per surviving generation, not per op.
* The CQE-supersession law (KD-2.4) = FIND-M11-A intra-mount: currency
  validated at publish time under the authority that orders publishes.
  `write_epoch` is process-globally unique (closes the retire/re-park
  ABA); stamp and check both run under the block lock —
  **lock-serialized by design, no fence protocol, NO LOOM OWED**
  (adjudicated in design §11; the deterministic schedule driver is the
  `SQUEEZEFS_TEST_UPLOAD_STALL_MS` seam + the stall-entry observable).
* KD-2.3: the in-place arms keep the serialized under-lock upload (a
  live-offset DMA never runs unlocked). Every supersession-path error
  degrades to the serialized upload — which owns the brim/staging/
  fencing ladders — so a one-shot transient device blip now heals with
  one retry before the staging detour (`write_through_tests` amended
  to pin the persistent-error posture with a 2-shot injection).
* The deferred arm (idle-window upload deferral — the non-overlapping
  loop-rewrite face) is classified `data=writeback` machinery (Idea 8)
  — **flagged for orchestrator review**, design §10.
* Counters: `write_pipeline_supersessions`,
  `write_pipeline_superseded_bytes`.
* Contracts: `tests/write_supersession_tests.rs` (3).

### Idea 1 — the shadow dual-map rewrite epoch

* ACK-path complete-block publishes on a striped ino record fresh
  (map B) bindings RAM-only (`layout_dirty` — the dirty-authority RYW;
  `fetch_metadata_from_backend` composes open-epoch bindings so an
  eviction can never lose them, KD-1.9); displaced A keys PARK in the
  epoch; ONE whole-tx save publishes the swap. Close triggers: full
  coverage (auto), fsync/flush (before the meta barrier), clone
  force-close, ENOSPC early-close (frees parked A supply + retries —
  `rewrite_shadow_fallbacks`), the idle sweeper (TTI/10 horizon).
* The §5.2 deferred-free law is the keystone: parked A keys free only
  after a durable save that no longer references them — intermediate
  dirty-persists are legitimate partial swaps (W6); flush legs never
  shadow (KD-1.10 — their custody families keep their durability).
* Crash windows W1–W6 per the design table: pre-swap crash ⇒ A intact
  + recovery reclaims B orphans (pinned by a real drop-and-remount
  contract); torn swap ⇒ v3 torn-write immunity; fenced close ⇒
  publish nothing, free NOTHING, loud (`rewrite_shadow_fence_drops`,
  must-stay-0). Mid-epoch B offsets hold live in-flight registrations
  — the fsck C2/C3 exemption hook (KD-1.3), pinned.
* Composition: bulk displaced frees at close ride Idea 4's elision
  (zero mid-row discards); shadow records attribute to Idea 17's
  family; gauges ride the epoch object's constructor/Drop so crashed
  epochs reconcile them.
* Lever: `SQUEEZEFS_REWRITE_SHADOW=0` restores per-block durable
  publishes verbatim. Per-block-machinery tests (CoW
  displace/free/reuse purge, conveyor economy, Lever A base
  provenance: `data_path_correctness_tests`, `publish_coalesce_tests`,
  `publish_drain_economy_tests` — 4 tests total) pin the lever off;
  the epoch venue is owned by `tests/rewrite_shadow_tests.rs`.
* Counters: `rewrite_shadow_{swaps,bytes,fallbacks,fence_drops}` +
  gauges `rewrite_shadow_{open_epochs,parked_bytes}`.
* Contracts: `tests/rewrite_shadow_tests.rs` (7).

## 2. Loom adjudication

No new lock-free protocol core: Idea 2's supersession decision is
block-lock-serialized (stamp and revalidation under the same
`BLOCK_FLUSH_LOCKS`; the only lock-free element is a globally-unique
counter, trivially monotonic); Idea 4's debt rides existing scc/DashSet
claim primitives whose protocols are already loom/field-pinned (the
free-list `remove` claim); Idea 1's epoch mutation serializes on
`INODE_META_LOCKS` with lock-free containers inside. Per the tier
table, no loom model owed; the deterministic-schedule seams
(`SQUEEZEFS_TEST_UPLOAD_STALL_MS`) are the weakening instrument.

## 3. Gates (the merge bar)

* `cargo clippy --all-targets --all-features -- -D warnings`: PASS.
* `cargo fmt --check`: PASS (after `d3d0cb5`).
* Full suite `cargo test --all-features -- --test-threads=1` from
  zero: RESULT-PLACEHOLDER.
* `cargo doc --no-deps`: RESULT-PLACEHOLDER.
* Bench smoke `cargo bench --benches -- --test`: RESULT-PLACEHOLDER.
* Targeted write-path family (re-run green during the campaign):
  `write_through{,_coverage}_tests`, `write_pipeline{,_phase}_tests`,
  `write_supersession_tests`, `rewrite_amp_tests`,
  `discard_elision_tests`, `inplace_overwrite_tests`,
  `block_free_reclaim_tests`, `async_block_reclaim_tests`,
  `crash_contract_tests`, `write_commit_crash_tests`,
  `striped_overwrite_lazy_seed_tests`, `data_path_correctness_tests`,
  `publish_coalesce_tests`, `publish_drain_economy_tests`,
  `extent_{patch,overlay}_tests`, `fsck_tests`, `refcount_clone_tests`,
  `rewrite_shadow_tests`, `transport_lease_overlong_tests` (in the
  full run), shim-parity battery (in the full run).
* External POSIX suites: per the tier table these are the release
  gate, not per-PR; not run here.

## 4. Local bracket (devsub-tcp — substrate law: loop is scoping-only)

RESULT-PLACEHOLDER

## 5. The field-window row manifest (the reformat window owes)

Stated per the cluster epoch lock (no deployment this campaign; local
file-backed sandboxes + devsub cover validation). The field acceptance
owes, on the 4-node NVMe-oF/TCP cluster after the reformat:

1. **Fresh-vs-rewrite A-B-B-A** (both orders — aging store) with the
   `rewrite_amp` columns: gate `±5 %` device-byte rate and
   `rewrite_amp ≤ 1.05` with **zero mid-row discards** (`d_ops == 0`
   during the row; `block_free_reclaim_elided` ≈ displaced blocks,
   `block_free_trim_*` quiet until idle). Lever A/B:
   `SQUEEZEFS_REWRITE_SHADOW=0 SQUEEZEFS_DISCARD_ELISION=0` is the
   pre-campaign posture on the same binary.
2. **Loop-rewrite latest-wins row** (hot set, deep qd, time-based)
   with the device-writes≈unique-blocks proof:
   `write_pipeline_supersessions` engagement + the rig's
   device/unique + coalesce columns; the non-overlapping face is
   measured and reported (gate arms with Idea 8).
3. **Zero-mid-row-discards assert on a ≥ 60 s sustained row**
   (flatness law: first-third vs last-third device-byte series).
4. **Loaded soak** (the rewrite-publish-drain §7 recipe: fio write +
   the 8-worker metadata storm + syncfs @10 s, 600 s) with the wedge
   indicator set all-zero (`fuse_op_watchdog_overdue`,
   `transport_lease_overlong`, `writer_guard_fenced`,
   `write_pipeline_fence_drops`, `rewrite_shadow_fence_drops`,
   `block_free_reclaim_fence_halts`, `ipc_sessions_poisoned`) and
   `rewrite_shadow_open_epochs` returning to 0 at quiesce.
5. fstests/LTP/pjdfstests remain the release gate (unchanged cadence);
   the epoch's fsync/close swap semantics ride the existing writeback
   contract, so no new adjudications are expected — any diff triggers
   the repro-port mandate.

## 6. Keyed decisions flagged for orchestrator review (design §10)

1. **Idea 2 retention scoping**: safe-today = supersession +
   retained-dirty-authority; the idle-window upload deferral (needed
   for the non-overlapping loop gate) is Idea 8 (`data=writeback`)
   machinery. If leadership wants the full loop gate in P0, Idea 8's
   class naming moves into this program.
2. **W6 legality** (intermediate saves as partial swaps) rests on the
   §5.2 deferred-free law; the alternative (force-close on every dirty
   persist) was rejected as needless serialization. Pinned by
   contracts.

## 7. Residuals / follow-ups

* A full in-process fsck DETECTION run against a mid-epoch volume
  (beyond the pinned registry-hook contract) — the fsck fixture shape
  is heavier than the write-path harness; owed with the next fsck
  campaign or the release-gate run.
* The `write_amp_rig` loop-rewrite row's non-overlapping gate arms
  with Idea 8.
* Idea 4's idle/pressure venue worker is deliberately paced
  single-lane; if a field profile shows trim-backlog pressure, the
  demand-derived lane fan-out (the reclaim worker's law) generalizes.
* AGENTS.md stats-surface listing for the new families rides the
  orchestrator's merge (single-writer doc discipline).
