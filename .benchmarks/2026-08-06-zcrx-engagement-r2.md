# 2026-08-06 — zcrx engagement round 2: the volume gates named and closed (counted bypass, the no-harm economics arm)

Branch `perf/zcrx-cohort-volume` (worktree off `integrate/zcrx-wave` tip
`5f48634c`, **unmerged — the orchestrator merges**). Charter: the round-1
geometry deployed (pair `5f48634c`) and the field rows showed the lane
SAFE but ~0 engaged — A1 26.28 / A2 25.75 GB/s vs B 27.49/28.10 (a real
~5–7 % RSS rent at zero engagement), `fill ≡ gather ≡ dest ≡ 0.6 GB`,
waits 1045/2740, parks 5, failovers 10, fallbacks 401, poisoned 0,
armed 1, NIC pristine after. Decompose the interlock, fix by derivation.
Design: `docs/design-zcrx-read-lane.md` Rev 4a. Cluster READ-ONLY.

Commits: red `164818ec` (contracts, compile-red at base) · green
`50a92804` (the instruments + the economics arm + design Rev 4a +
this note).

## 1. THE VOLUME-PATH AUDIT (deliverable 1 — where 1500 GB routed)

Every device read class reaches the funnel (`NvmeBlockDev::
read_block_with_dest_inner` → `try_lane_read`); **nothing bypasses
structurally**:

| read class | path | funnel arm |
|---|---|---|
| demand COHORT fills (4 MiB block fetches) | `get_cached_or_fetch_block` → `fetch_block_from_remote` → `read_nvme_block` → `BackendRouter::read_block(key, window)` | pooled (dest-less) |
| R2 prefetch + read-lane ahead fetches | same `get_cached_or_fetch_block` funnel | pooled |
| dest-lease / R3 ranged windows | `get_block_range_for_index` → `BackendRouter::read_block_range(key, rel, len, dest)` | fused dest |
| EXA raw full-block dest serves | `BackendRouter::read_block_with_dest(key, size, Some(dest))` | fused dest |
| health probes | `probe_read_block` | pooled (uncounted in `get_obj`) |

On the composed row (lease armed) the ~1500 GB rode the **ranged dest
leg** — per the phase-1 lease acceptance, `read_dest_lease_bytes ≡
ranged_read_bytes ≡ the row's served bytes`, i.e. ~1.5 M lease-shaped
1 MiB dest reads reached `try_lane_read`. On a lease-off control the
same volume arrives as pooled 4 MiB cohort fills — the SAME funnel.
**The user's hypothesis 2 (cohort fills bypass the funnel) is FALSE**;
routing needs no change.

**The gate that ate the row**: the round-5 **degraded bypass** —
`try_lane_read` returns `None` when `refill_degraded()` (any queue's
`starved` latch), deliberately UNCOUNTED since round 5 ("ineligibility
class"). The park → failover → trickle-recovery cycle (parks 5 /
failovers 10 across 10 single-queue sessions) held that latch through
most of the row: ~1.5 M reads bypassed silently, ~4 k reached admission
during healthy windows (the waits), ~600 served (0.6 GB), 401 rode the
failover fallback. The interlock's hypothesis 1 (ahead-lane depth-0) is
REAL but IRRELEVANT to volume: the demand/lease stream alone is the
whole row.

## 2. THE WAITS DECOMPOSITION (deliverable 2)

From the admission code, exhaustively: `zcrx_area_admission_waits`
counts at exactly ONE site — `admit_whole_read`'s per-queue
`try_acquire_many_owned` refusal (the whole-read atomic WINDOW arm).
The CID gate (`take_cid`) AWAITS and never declines or counts; the
dest/pooled arms share the same admission. So waits = window-full
declines. The window (64 MiB = 64 concurrent 1 MiB windows/queue)
cannot be filled by the offered demand (~13 concurrent 1 MiB
windows/device at 16 streams × qd8 over 10 devices): it was full of
**permits held by PARKED fills** — admission permits ride pending fills
(MEM-3), and during a starvation episode those fills sit parked up to
`park_fail_bound()` ≈ 937 ms before the failover releases them. The
1045/2740 waits are the offered-rate × parked-time integral, not an
admission under-derivation. **The admission derivation stands; the pool
term underneath is the cause** — waits resolve when the pool does.

## 3. ROUND-8 STRUCTURAL NON-FIRING, ADJUDICATED (should it have fired?)

It COULD NOT fire, by construction: `structural()` requires
`REFILL_STRUCTURAL_FAILOVERS = 2` CONSECUTIVE failover windows with NO
payload progress, and `on_progress` (ANY payload byte) resets the
streak. The tape's 5 episodes / 10 windows with 0.6 GB of trickle means
progress ended episodes between windows — every streak reset. That is
the gray zone round 8 left open: a pool that trickles one fill per
window is "never structural" while paying full RSS rent at ~0
engagement. **Verdict: yes, it should have released — and the horizon
is extended, not by widening the zero-progress arm (trickle is real
payload; zeroing it would misfire on genuine transients) but by the
ECONOMICS arm below, whose ledger trickle cannot reset.**

## 4. THE FIXES (all derived; no routing change needed)

* **The bypass is COUNTED** (`zcrx_degraded_bypasses`, stats inode +
  rig): the three volume gates are now mutually exclusive counters —
  bypasses (pool/degraded), waits (window admission), fallbacks
  (per-op errors). A field tape can never again lose ~1.5 M reads to an
  invisible arm (this exact blindness cost rounds Z3-8 AND engagement-1
  their diagnosis). Pinned: `test_degraded_bypass_is_counted_and_
  recovers` (bypass ≠ fill ≠ wait ≠ fallback; recovery stops the
  count), via the new contract seams `NvmeBlockDev::
  lane_session_for_test` / `LaneSession::set_refill_degraded_for_test`.
* **The no-harm ECONOMICS arm** (`RecvGovernor`): a cumulative
  **starved-time ledger** — accounted exactly at failover windows and
  episode-ending progress tails; monotone, so trickle progress resets
  nothing. `economic_structural(now, bound)` = starved_total × 2 ≥
  armed lifetime AND lifetime ≥ `REFILL_STRUCTURAL_FAILOVERS ×
  park_fail_bound()`. Derivations (no fresh constants): the horizon is
  the round-8 structural law's own time constant — this arm can never
  fire FASTER than the existing transient allowance; ½ is the majority
  boundary — the rent (RSS exclusion) is paid for the whole armed life
  while savings accrue only while serving, so a majority-starved queue
  is running at best half-rate savings against full rent with the
  episode trend established. It latches the SAME `starved_structural`
  → funnel-teardown path (RSS width restores; kernel path serves).
  At the field tape's shape it fires within ~2 windows (~1.9 s) of
  majority-starved life — the A rows degrade to lane-off parity
  instead of paying 60 s of rent. Pinned in-module:
  `recv_governor_economics_arm_trickle_cannot_reset` /
  `..._majority_serving_never_fires` (+ exact window/tail accounting).
* **Instruments**: `zcrx_starved_ms` (the ledger exported — starved
  share vs armed wall time is the live pool-term gauge) and
  `zcrx_structural_teardowns` (escalations latched, either arm — the
  "did no-harm fire?" adjudication is a number). Rig prints
  `bypasses/starved_ms/structural` per row.

## 5. ADJUDICATIONS (deliverable: build or defer honestly)

* **Ahead-governor feed (zcrx savings → delivery signal): DEFERRED.**
  (a) The demand/lease stream carries ~100 % of cold row bytes on the
  field shape — share ≥ 0.5 does not need ahead fills; (b) the
  governor measures DELIVERY response, so once the lane serves volume
  its CPU savings appear in the governor's own signal — the honest
  coupling already exists through measurement; forcing a cost-model
  hint into a delivery-measured governor would make its signal
  heterogeneous. Revisit only if a fixed-lane row shows demand-stream
  share < 0.5 with ahead-depth 0.
* **The pool term is FIELD-OWED**: why the provider pool starves under
  ~13 concurrent 1 MiB windows/device with a 184 MiB area (window 64 +
  slack 24 + ring-standing 96 MiB) is only measurable live — the prime
  suspect is the round-6 ring-standing model vs mlx5's REAL per-queue
  demand once restarted onto the zcrx provider (striding-RQ vs
  legacy-RQ geometry; a failed ring/MTU probe already logs loud).
  `zcrx_starved_ms` now names it directly: starved-share ≈ 0 ⇒ the
  area is sized and engagement follows; immediate majority-starvation
  ⇒ the ring-standing PROBE is the term (fix the probe's input, never
  a constant).

## 6. Verification

Red `164818ec` compile-red at base (`zcrx_degraded_bypasses`,
`economic_structural`, the seams — all absent). Green: in-module
`--lib zcrx` **39** (37 + the 2 economics contracts), `zcrx_lane_tests`
**64** (63 + the bypass contract) — ×10 consecutive; zcrx_steering 23;
derivation_sweep; read-path battery (nvme_dev, nvme_dest_ownership,
read_lane, read_dest_lease, read_copy_ledger, ranged_read, hybrid_io +
the prefetch/tier/governor/saturation/serve-phase/rebind set) green;
both clippys `-D warnings` clean; fmt clean; markdown links clean.

## 7. FIELD EXPECTATIONS (honest)

The same A-B-B-A instrument (`tests/fio/zcrx_field_rows.sh`, now with
bypasses/starved_ms/structural columns) discriminates THREE outcomes:

1. **The pool serves** (starved_ms share ≈ 0, bypasses ≈ 0): share
   ≥ 0.5 becomes reachable — the round-1 geometry already admits the
   offered row (§2), so engagement follows the pool. This is the only
   arm that reaches the 35.5 bar this round.
2. **The pool starves as in round 1**: the economics arm tears the
   majority-starved sessions down within ~2 windows; the row degrades
   to ≈ lane-off parity (the 5–7 % rent bounded to seconds, not the
   row) and the tape reads `bypasses ≫ fills`, `starved_ms ≈
   sessions × teardown latency`, `structural > 0` — **an honest
   verdict that the composed geometry cannot reach share ≥ 0.5 on this
   NIC until the ring-standing probe term is fixed in the field**, with
   the exact term named by instrument instead of by three more rounds
   of archaeology.
3. **Mixed** (some devices' pools serve): per-session teardowns release
   the starved queues' RSS width while serving sessions keep their
   share — net rent bounded, partial engagement visible per the
   bypass/fill split.

rand-4k stays inert-by-warmth (both sides serve from tiers; par is the
guard). Default-on adjudication unchanged: only on a counted win.
