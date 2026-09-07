# The co-writer refill gate: arm on the ADVERTISED supply, not the owed ledger (2026-09-07)

Finding 15's second co-writer starvation case, the gate half. Branch
`fix/lane-refill-hint-gate` off `dev` `73a133c8`. The sibling half — a
co-writer's placement/harvest reaching the VOLUME that holds its lane's
supply — is `fix/cowriter-lane-aware-placement`; this note claims nothing
about it, and the fleet row is the parent's.

## 1. The defect

A co-writer's lane refill has three arms:

| Arm | Where | Gate (as landed) |
|---|---|---|
| **ENOSPC-path harvest** | `BlockAllocator::allocate_block_inner` → `harvest_lane_supply` (inside the finding-29 bounded park) | none — runs at lane exhaustion |
| **AHEAD refill** | `should_harvest_ahead` / `ahead_refill_tick` (the 1 s watermark tick) | `lane_owed > 0 ∧ reachable < watermark` |
| **PUSHED refill** | `pushed_refill_tick` → `free_grace::lane_push_wants_harvest(hint, owed)` (on the renewal grant's wake) | `hint > 0 ∧ lane_owed > 0` |

`lane_owed` is the per-allocator "blocks this mount is OWED on the
authority's list" word, incremented ONLY by `note_owed_freed`, whose one
call site is `cowriter::ship_displaced_frees`' `Freed` verdicts — the
EXPLICIT-ship arm. On the s11-mpiio fleet (8 co-writers rewriting one
shared 10 GiB file under S11 range custody) most displaced blocks do not
travel that arm: the authority RECOMPUTES them when it serves the
co-writer's layout publish, and finding 36's local retire notes nothing
owed. So `lane_owed ≈ 0`, BOTH proactive arms stayed dark, and every
refill ran from inside an ENOSPC park — each one a refused-write storm.

### The numbers (the sampler-shakedown row, `.benchmarks/2026-09-07-fleet-sampler-shakedown.md`; binary `ee49cf04`, samples at `/tmp/five/d4/keep-sampler-shakedown/`)

End snapshots (`m*.stats.json`):

| Mount | `free_recomputed_blocks` | `free_shipped_blocks` | `rewrite_blocks` | `alloc_lane_owed_blocks` | `alloc_lane_harvests` | `harvested_blocks` | `pushed` | `ahead` | `enospc_refusals` | `lane_supply_hint` (end) |
|---|---|---|---|---|---|---|---|---|---|---|
| m0 (authority) | **39,279** | — | — | — | — | — | — | — | — | — |
| m50 | — | 580 | 4,383 | **0** | 6,930 | 5,078 | 158 | 35 | **6,704** | 407 |
| m51 | — | 498 | 4,404 | 0 | 3,165 | 5,015 | 226 | 19 | 2,883 | 335 |
| m52 | — | 585 | 4,426 | 0 | 10,472 | 5,073 | 185 | 18 | 10,256 | 426 |
| m53 | — | 656 | 4,216 | 0 | 5,492 | 5,160 | 185 | 46 | 5,185 | 291 |
| m54 | — | 505 | 4,500 | 0 | 2,496 | 5,127 | 212 | 15 | 2,226 | 311 |
| m55 | — | 488 | 4,497 | 0 | 5,523 | 5,058 | 223 | 34 | 5,232 | 323 |
| m56 | — | 481 | 4,504 | 2 | 1,811 | 4,981 | 210 | 28 | 1,548 | 511 |
| m57 | — | 504 | 4,483 | 0 | 6,125 | 5,190 | 266 | 11 | 5,812 | 231 |

Σ shipped over the co-writers 4,297 against the authority's 39,279
recomputed: **≈ 90 % of the fleet's displaced blocks return through the
recompute arm** (per co-writer, shipped ÷ rewritten ≈ 11–16 %). The
proactive arms produced 3–8 % of the harvests (m50: 193 of 6,930); the
rest ran inside ENOSPC parks, and `alloc_lane_enospc_refusals` tracks
`alloc_lane_harvests` to within a few percent on every co-writer — the
refill IS the refusal storm. (The parent's reading of the same shape on
its row — `free_recomputed_blocks` 30,865 vs `free_shipped_blocks` 399 on
m50, 89 % recomputed — is the same conviction from a different run.)

The per-second series (`samples/m50.jsonl`) makes the gate visible.
Columns: `t`, `free_grace_lane_supply_hint`, `alloc_lane_reachable_blocks`,
`alloc_lane_harvest_watermark`, `alloc_lane_harvests`,
`alloc_lane_pushed_harvests`, `alloc_lane_ahead_harvests`,
`alloc_lane_enospc_refusals`, `free_grace_lane_push_wakes`:

```
t   hint reach  wm  harv  push ahead enospc wakes
74   410   162 128   152   79   22     26   100
75   374   138 128   433   79   22    307   103
76   374   138 128  1152   79   22   1026   105
77   374   138  84  1821   79   22   1695   108
78   374    30 128  2058   79   22   1932   111
87   332   135 128  2075   96   22   1932   133
88   333    72 128  2075   96   22   1932   136
89   332    23 125  2081  102   22   1932   139
90   332    17  53  2087  108   22   1932   142
```

At 74–78 s the authority advertises **374** blocks of m50's lane the whole
time, the co-writer's reachable stock falls 162 → 30 under a watermark of
128, **eleven renewal wakes arrive and every one is declined** (pushed flat
at 79), the ahead tick never fires (22 flat), and 1,906 ENOSPC-path
harvests run beside 1,906 refusals. At 87–90 s: reachable 135 → 17 under
a watermark of 125–128 with the hint at 332 — the ahead tick still dark.
Across the row m50 received 248 wakes and pushed 158 times (90 declined on
`owed == 0`); the maximum per-5 s deltas were `alloc_lane_harvests`
**2,264** / `enospc_refusals` **2,261** against `pushed` 22 / `ahead` 11.

The just-landed supply-coupled epoch close
(`perf/rewrite-epoch-supply-close`, integrated by the parent) injects the
parked keys into the loop, but they come back through the recompute arm
too — so without this fix its supply is also reachable only from inside
an ENOSPC park.

## 2. The fix

The ground truth of "the authority holds N blocks of my lane" is the
authority's OWN count, carried on every renewal grant
(`Grant::lane_supply_blocks` → `free_grace::note_lane_supply_hint` →
`lane_supply_hint()`; the per-lane counters of the lane-counted free set,
summed over the mount's data volumes by `multi_writer::lane_supply_source`;
fresh within one renewal cadence — 500 ms while the valve asks). The owed
ledger is a strict SUBSET of it. Both proactive gates are re-derived on it:

| Predicate | Was | Is |
|---|---|---|
| `free_grace::lane_supply_witnessed(hint, owed)` (new — the ONE witness) | — | `owed > 0 ∨ (REFILL_HINT ∧ hint > 0)` |
| `BlockAllocator::should_harvest_ahead` | `HARVEST_AHEAD ∧ laned ∧ owed > 0 ∧ 0 < watermark ∧ reachable < watermark ⇒ Some(grain)` | `HARVEST_AHEAD ∧ laned ∧ lane_supply_witnessed(hint, owed) ∧ 0 < watermark ∧ reachable < watermark ⇒ Some(grain)` |
| `free_grace::lane_push_wants_harvest(hint, owed)` | `LANE_PUSH ∧ hint > 0 ∧ owed > 0` | `LANE_PUSH ∧ hint > 0 ∧ lane_supply_witnessed(hint, owed)` — the hint alone suffices |
| the ENOSPC-path harvest | unchanged | unchanged |

* **The watermark law is untouched** — the hint is a supply WITNESS, never
  a demand: stocked above the watermark, a nonzero hint fires nothing.
* **The harvest ask stays the grain** (`lanes.grain.max(1)`), never fewer.
  The audit found the ask was never sized by `owed`; it is NOT shrunk to
  `min(hint, grain)`: the authority's serve (`take_lane_free_blocks`) scans
  its whole free set regardless of `max` and returns at most what it
  holds, so a smaller ask against a one-cadence-stale, mount-summed hint
  could only under-harvest and saves nothing on either side.
* **Per mount vs per volume**: the hint is summed over the data volumes;
  the refill runs per allocator. A nonzero hint arms every laned allocator
  whose reachable stock is below its watermark; a volume without the
  supply pays one empty, counted RPC (`alloc_lane_harvests` vs
  `alloc_lane_harvested_blocks` is the ledger). No per-volume hint was
  added to the wire here (the sibling may).
* **The owed ledger stays** as the explicit-ship arm's instrument
  (`alloc_lane_owed_blocks` still exported; `note_owed_freed` /
  `adopt_lane_free_grant`'s pay-down unchanged; the capacity law's
  `live = share − reachable − owed` unchanged — the mount-summed hint
  cannot enter a per-volume law, noted in the code). Its docs now say what
  it is.
* **Lever** `SQUEEZEFS_ALLOC_LANE_REFILL_HINT` (bool, default on;
  registered, `docs/operations.md` row): `0` = the retired owed-only gate
  verbatim, the A/B control. Read once and cached (`free_grace::REFILL_HINT`,
  the `LANE_PUSH` latch shape); with `SQUEEZEFS_FREE_GRACE_LANE_PUSH=0` no
  hint is ever stored, so the ahead arm degrades to the owed gate there by
  construction.
* **Gauge** `alloc_lane_hint_refills`: proactive harvests (ahead + pushed)
  that fired with the owed word at 0 — the ones the owed gate would have
  declined; ⊆ `alloc_lane_ahead_harvests + alloc_lane_pushed_harvests`,
  0 under `REFILL_HINT=0` by construction, never the ENOSPC arm. Added to
  the fleet sampler's key list beside `alloc_lane_owed_blocks`.

Sites audited for an `owed` gate (grep `lane_owed` / `owed_blocks` /
`note_owed_freed`): `should_harvest_ahead` (re-derived), `pushed_refill_tick`
→ `lane_push_wants_harvest` (re-derived), `adopt_lane_free_grant` (the
pay-down — untouched), `sample_alloc_rate` (the capacity law's `live` —
untouched, a gauge), `harvest_lane_supply`'s ask (never owed-sized), the
`alloc_lane_grant.rs` wake loop (it dispatches to the two ticks — its
comment re-stated), the bounded park's wake in
`allocate_block_grace_bounded` (re-runs the ENOSPC harvest — unchanged),
`cowriter::ship_displaced_frees` (the increment — unchanged).

## 3. Contracts (red-first; `a4514a01` red, the implementation green)

`tests/free_grace_lane_visible_tests.rs`:

* `the_pushed_refill_decision_is_pure_and_lever_gated` (re-stated): the
  hint alone pushes; hint 0 ∧ owed 0 pushes nothing; `REFILL_HINT=0` is the
  owed-only gate; the lane-push lever off pushes nothing either way.
* `the_pushed_refill_fires_on_the_hint_alone_for_a_recomputed_free` (new,
  real allocators): a lane-1 block the authority freed WITHOUT the
  co-writer shipping it is adopted by `pushed_refill_tick` on the grant's
  hint with owed 0 — `alloc_lane_pushed_harvests` +1,
  `alloc_lane_hint_refills` +1, the owed word and its sum gauge untouched,
  the block mints; the same shape under `REFILL_HINT=0` declines.

`tests/mw_data_alloc_lane_tests.rs` §9 (new):

* `the_ahead_refill_fires_on_the_hint_alone_below_the_watermark`: hint 0 ∧
  owed 0 ⇒ dark, no RPC; hint > 0 ∧ owed 0 ∧ reachable < watermark ⇒ one
  RPC asking the full grain, 10 adopted, ahead +1, hint_refills +1, owed
  0; stocked above the watermark ⇒ dark, hint or not; `REFILL_HINT=0`
  declines the hint and one owed block re-arms; `HARVEST_AHEAD=0` is the
  ENOSPC-only shape.
* `the_enospc_path_harvest_is_unchanged_and_never_a_hint_refill`: an
  exhausted lane's allocation still harvests inline before its verdict
  with hint 0 ∧ owed 0 — one RPC, adopt, mint, zero refusals — and moves
  neither the ahead, pushed nor hint-refill counters.

`tests/mw_cowriter_free_tests.rs`: the owed pin
`the_ahead_decision_harvests_only_the_owing_starved_volume` is re-stated —
its fixture learns no grant (asserted), so the owed word is its only
supply witness.

Suites run (`--all-features`, `--test-threads=1`, this worktree's target):
`mw_data_alloc_lane_tests` 25, `free_grace_lane_visible_tests` 10,
`mw_cowriter_lane_tests` 26, `mw_cowriter_free_tests` 50,
`reader_free_grace_tests` 61, `cowriter_enospc_wedge_tests` 9,
`derivation_sweep_tests` 47, `env_knob_convention_tests` 21 — all green;
plus `mw_cowriter_free_leak_tests`, `dlm_cowriter_tests`,
`dlm_multi_writer_tests`, `audit_instruments_tests` (see the branch's
report). `cargo fmt --check` and `cargo clippy --all-targets --all-features
-- -D warnings` clean.

## 4. What is NOT claimed

* **No fleet row.** Nothing here was measured on the s11-mpiio venue; the
  before/after pair (`SQUEEZEFS_ALLOC_LANE_REFILL_HINT=0` vs default, same
  boot, from zero, the sampler running) is the parent's. The expected
  shape on the fix: `alloc_lane_hint_refills` growing ≈ `ahead + pushed`
  on every co-writer, `alloc_lane_enospc_refusals` and the ENOSPC-path
  share of `alloc_lane_harvests` falling, `alloc_lane_pushed_harvests` ≈
  `free_grace_lane_push_wakes`.
* **The per-volume half is the sibling's.** With two data volumes a
  nonzero hint arms both laned allocators; the one whose lane list is
  empty pays an empty RPC per tick until the sibling's lane-aware
  placement/harvest routes the demand. That RPC is bounded by the tick and
  renewal cadences (≤ 2/s per volume) and counted.
* **Case (a) of the shakedown note (hint 0 — the parked-epoch shortage)**
  is untouched by this gate: with nothing advertised and nothing owed the
  proactive arms stay dark by design (the quiet-lane posture — no RPC
  storms on an idle lane); the supply-coupled epoch close is that lever.
* The hint is one renewal cadence stale; a harvest that empties the
  authority's list can leave a stale nonzero word until the next grant,
  costing at most one empty ahead RPC per volume in that window. A
  harvest reply carrying the remaining lane supply would close it and is
  a wire change deliberately not made here.
