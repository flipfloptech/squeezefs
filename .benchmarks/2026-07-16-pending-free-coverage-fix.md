# Option A — pending-free coverage gate lands (Branch 3, 2026-07-16)

**Branch** `fix/kv-pending-free-coverage` off dev `f41a0eb`. **Charter**:
`docs/design-smo-replay-currency.md` **Option A + the PR 4 row in full**
(incl. review Issue 9's three at-cap clauses) = PR 1(b)/(c) test-commits +
the PR 4 fix-commit — the load-bearing fix for **sub-mechanism (ii)
recycled-extent stale routes** of the FIND-VS-A residual: the pending-free
gates compared checkpoint GENERATION only (`after_durable_barrier` live;
`alloc_ext::load` mount), certifying durability of the freeing checkpoint
RECORD — never coverage of the freeing FLIP/swap. The flush pass skips
flip-carrying interiors on reserve exhaustion and a root swap's dying
floor clamps the covering record's tail below the SMO entry, decoupling
the two: a freed extent could re-enter the pool while durable routing
still referenced it — the child-seq mount-refusal class, and for
root-swap SMOs (no journaled flip) the ONLY protection (C′ can't reach
them). §4.7's "any state replay can select references only
never-overwritten extents" is restored to the letter. The ×10 storm
acceptance soak stays **PR 5's** row.

Rails: unique sandboxes `~/tmp/smo_b3_*` (dev capture preserved, the rest
deleted post-run); kills by PID; systemd-run cages; storms on the full CPU
mask; builds on `taskset` subsets; Tctl peaked 72.9 °C (< 88 °C).

---

## RED (PR 1(b), commit `0377431`) — four contracts, ×3 deterministic on dev

`tests/kv_smo_crash_completeness_tests.rs`, all RED on dev `f41a0eb` with
exact signatures (3/3 identical runs):

- **`root_swap_freed_extent_stays_parked_until_tail_covers_free`** — the
  LIVE gate. A root swap's dying floor *provably* pins the covering
  record's tail below the SMO entry, yet dev released the old root's
  extent at that record's barrier: `left: 0, right: 1` (pending drained
  while `tail < free.seq`). Zero extra levers — the violation is
  structural.
- **`mount_side_replayed_free_parks_until_post_mount_checkpoint`** — the
  MOUNT gate. Kill with the swap's entry in-window; dev's reopen released
  the replayed free at load (`retire_tag ≤ mounted_seq`): `left: 0,
  right: 1`. The mounted record itself can be page-cache-only after a
  kill — releasing here is the reuse-vs-fallback §4.7 law violation.
- **`pending_free_at_cap_forced_cycle_completes_and_conserves_extents`** —
  the §4.7 at-cap law ("pressure forces a checkpoint rather than unsafe
  reuse") was intent, not mechanism: at cap `free_pending` errors
  POST-swap (`smo_replace` step 3 `?`) and the extent leaks from the live
  FIFO. Measured on dev: **4 extents leaked** across 4 at-cap compactions
  (conservation `3958 != 3962`), volume "healthy".
- **`pending_free_wedged_tail_fails_volume_loud_never_livelocks`** — a
  genuinely wedged tail (ancient un-flushable XATTRS floor below every
  parked retirement, FIFO saturated at the `TEST_PENDING_FREE_CAP = 2`
  seam) must present as a LOUD volume failure; dev leaked silently and
  kept acking (`is_failed()` never latched).

## Fix (PR 4, commit `3cf096d`) — the row verbatim

**The gate** (zero format change — free-record VALUES keep the historical
retire tag byte-for-byte; gating keys off `rec.seq`):

- **Live**: `free_pending` carries the SMO entry's **free-record journal
  seq** (the entry's highest seq — pushed last; per-SMO-entry floor
  pinning makes `tail > free.seq ⇔` every flip of the entry is
  materialized; tails are entry boundaries post-PR 3, so the wrapper's
  `tail ≥ gate` is the design's strict inequality).
  `after_durable_barrier` advances the allocator by the record's **tail**
  — the same §4.6 pt 3 clock as `reusable_upto` and the cache durable
  tail (`pending_reclaim` slimmed to tails; the ledger-seq half was
  dead).
- **Mount**: replayed allocator deltas fold **per-key LWW first**, then
  every checkpoint-referenced final **parks** gated on its `rec.seq`
  until the first post-mount durable checkpoint; tag-0 sentinels stay
  immediate. LWW-first is load-bearing: a free superseded by a later
  in-window re-alloc of the same extent must fold away, or its parked
  drain would clear a LIVE extent's bit — caught by the standing R3
  crash test (`test_kv_alloc_torn_newest_root_after_churn_…`), which
  failed the first blanket-park cut and now passes with the tail-domain
  arithmetic.

**At-cap force-cycle, three clauses (Issue 9)**:

- **(a)** pending-headroom check at SMO admission, beside the
  `try_admit`: `PendingFreeFull` becomes a clean PRE-swap abort (claims
  released, floor restored by the caller). The post-swap `?` stays
  defense-in-depth — structurally unreachable under the serialized
  single-producer argument, now loom-modeled.
- **(b)** both `run_maintenance` arms treat `PendingFreeFull` like
  reserve exhaustion — `force_pending_free_cycle`: one forced
  `checkpoint_cycle(…, true)` + retry, progress-audited **on the
  backend** (spans passes; any `pending_count` decrease resets), with the
  bounded-retry-then-loud terminal at **8** stalled cycles
  (`checkpoint_past` precedent): `fail_stop_loud` latches the volume
  FAILED. Healthy convergence needs ≤ 2 cycles (cycle 1 discharges dying
  floors, cycle 2's tail covers the parked frees) — the at-cap test
  exercises exactly that.
- **(c)** skip-and-defer on `PendingFreeFull` in the flush-pass match
  inside `checkpoint_cycle` (restore-floor semantics identical to the
  reserve skip), so a forced cycle under a cap-saturated storm completes
  its non-SMO flushes, writes the ledger, barriers, and drains via
  `after_durable_barrier` — without it the forced cycle itself would
  return `PendingFreeFull` before its barrier and the remedy livelocks.

**Core hardening the loom model forced**: `free_pending`'s full-detection
now distinguishes genuinely-full (cursor distance ≥ cap) from a
mid-vacate consumer (stamp store pending) — the new headroom invariant
caught the transiently-conservative refusal breaking clause (a)'s
soundness.

**Stats**: `meta_kv_pending_free_parked` / `meta_kv_pending_free_released`
(process-global counters, stats-JSON wired; `parked − released` tracks
the authoritative per-volume `meta_kv_pending_free` gauge — note the
counters also accumulate preflight/verify opens' mount parks, so the
gauge is the wedge detector, the counters the rate surface).

## GREEN + gate (fix commit `3cf096d`)

- RED→GREEN ×3 on `taskset -c 16-23` AND ×3 on the full mask (suite 9/9
  each run, both masks).
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --check` — clean. `cargo doc --no-deps` — clean (0 warnings).
- `cargo bench --benches -- --test` — smoke green.
- `cargo test --all-features -- --test-threads=1` — **856 passed / 0
  failed** on the merged SHA (851 inherited + 4 PR 1(b) + 1 PR 1(c)).
  Named suites: crash_contract 25 (incl. the R3 reuse-vs-fallback case,
  updated to tail-domain arithmetic), crash_kill 8, conveyor 10,
  kv_smo_crash_completeness 9, writeback_fencing_livelock 5,
  staging_budget 7, mem_budget 16, kv_alloc 14 (mount rule updated to
  §2-A parking).
- **loom** (`tests/run_loom.sh`) — **25/25**: the `alloc_ext_core` gate
  model re-clocked to coverage tails
  (`alloc_ext_pending_free_never_claimable_before_covering_tail`, incl.
  the record-durable-but-uncovered-tail refusal shape) and the new
  single-producer headroom invariant
  (`alloc_ext_headroom_observed_at_admission_holds_at_push`).

## PR 1(c) — fixture over captured images (REFUSED-hunt outcome documented)

The chartered fixture source was a REFUSED round. **The hunt on dev
`f41a0eb` produced none: 8/8 recapture rounds CLEAN** (67 k acked/round,
`MISSING-ACKED=0`, no refusals) — consistent with C′+PR 3 having closed
the walks that *detected* the reuse (Branch-1 evidence: the refusal was
reproduced on pre-C′ `44d14d6`); no REFUSED round was preserved from
earlier branches (verdicts on disk: LOSS + CLEAN only). Fallback, per the
same kvparse discipline: `.agents/findvsa/extract_pending_free_fixture.py`
(imports kvparse.py's checksummed parsers) adjudicates the §2-A gate
decision over the round-8 post-kill images —

| metric (4 volumes) | value |
|---|---:|
| in-window checkpoint-referenced frees | **231** |
| released-at-load by the OLD generation gate | **71 (31 %)** |
| parked by the NEW coverage gate | **231 (100 %)** |
| referenced by mounted/fallback roots | 0 / 0 |

— 71 real-storm extents the old mount gate handed back to the pool while
their freeing entries rode the replay window: the §2-A violation face,
quantified from real bytes. Committed:
`tests/fixtures/findvsa3_pending_free_window.jsonl` (55 KB) re-asserted by
`recaptured_window_pins_generation_gate_release`; full dump
`.agents/findvsa/capture-2026-07-16-pendingfree-expectations.txt`; raw
images preserved at `~/tmp/smo_b3_1493772/capture_round8/` (regeneration:
`recapture.sh`).

## Storm rate-gathering (declared rate-gathering, NOT acceptance)

Three `recapture.sh` rounds on the **fixed** release binary (16-worker
acked-create storm, kill -9 at peak, full mask, in-place remount). The
×10 acceptance count remains PR 5's gate on the final program binary.

| round | acked | missing | refusals | dropped_torn | replay entries/vol |
|---|---:|---:|---:|---|---|
| 1 | 67,265 | **0** | 0 | **[0,0,0,0]** | 6305/8078/4279/3486 |
| 2 | 63,585 | **0** | 0 | **[0,0,0,0]** | 2119/2685/5926/3594 |
| 3 | 67,664 | **0** | 0 | **[0,0,0,0]** | 3053/2895/4320/2580 |

198,514 acked creates, zero lost, zero refusals, torn-drops all-zero;
post-remount `meta_kv_pending_free = [0,0,0,0]` every round (parked
window frees drained by the first post-mount checkpoints). (Round 3 ran
in a fresh sandbox after a round-2→3 stale-mountpoint harness hiccup —
`Transport endpoint is not connected` on the *storm mount* of the reused
mountpoint, before any storm: harness plumbing, not adjudication.)

**Backlog gauge sanity (the PR 4 verification row's ≈ 1.4 % arithmetic)**:
a dedicated no-kill storm mount sampled `.stats` at 100 ms for 9 s:
aggregate SMO rate ≈ **760/s** (6,763 SMOs / 8.9 s, 4 volumes), peak
`meta_kv_pending_free` sum = **419 extents ≈ 0.64 % of the 65,536 cap**
(per-volume peak 170 ≈ 0.26 %), steady-state 300–420 — the same order as
the design's ≈ 939-extent / 1.4 % projection at its measured 939 SMO/s,
comfortably ≪ cap, and it drains toward zero the moment the storm stops
(147 at t=8.9 s). The at-cap machinery stays cold in production shapes.

## What PR 5 (acceptance) still owes

- The **×10 consecutive** storm soak on the final program binary:
  acked-loss == 0 AND mount-refusals == 0 AND `dropped_torn == 0` jointly
  (multi-run discipline: any failure restarts the count).
- `churn_unmount_soak.sh` 10/10; `FSTESTS_QUICK=1` tier;
  `SQUEEZEFS_VS_REGIMES=R3` scoreboard re-run (rows stay W).
- Closing evidence note `.benchmarks/…-smo-currency-closing.md`; §4.6/§4.7
  + FIND-SMO-TAIL deltas in `docs/design-cow-kv-metadata.md`; AGENTS
  stats rows (`meta_kv_pending_free_parked/_released` belong in the
  stats-surface list there).
