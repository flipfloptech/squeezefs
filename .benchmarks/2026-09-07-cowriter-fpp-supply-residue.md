# Finding 15 — the file-per-proc lane-ENOSPC residue on the passing tip: attribution and the per-volume fixes (2026-09-07)

**Verdict.** On `886d4e31` the `s11-mpiio` fleet (authority + 8 co-writers,
two 32 GiB data volumes, W = 16 ⇒ 512 blocks per lane per volume) sustains
every phase, but the file-per-proc phases refuse 7,486–16,065 lane
allocations per phase and A2 (the aged shared-file phase) up to 16,065
(`.benchmarks/2026-09-07-f15-b1-squeeze-test-seq2.md` §2). The time series
attributes them to **a lane at its capacity edge whose supply returns PER
VOLUME while three co-writer mechanisms still planned MOUNT-wide**: every
refusal is a park slice on a lane whose stock ran dry (never a synchronous
`ENOSPC` — `write_enospc_refusals` 0 on every row), the counter reads 2 per
50 ms slice per parked allocation (both volumes are tried and each refusal
is counted), 92–97 % of the refusals are single-flight DECLINES (the
authority was not asked because nothing it had advertised had moved), and
at the burst instants the lane's own displaced keys were sitting (a) in the
authority's grace ring, (b) parked in this mount's open rewrite epochs
(A2: every refusal; fpp: ~20 %), or (c) on the authority's list for the
SIBLING volume. Landed: the supply-coupled epoch close plans **per volume**
(`SQUEEZEFS_REWRITE_SUPPLY_CLOSE_PER_VOLUME`), the renewal grant carries the
lane-supply hint **per volume** (`Grant::lane_supply_volumes`,
`CLUSTER_WIRE_SCHEMA` 3, `SQUEEZEFS_ALLOC_LANE_VOLUME_HINT`) so the pushed
refill, the ahead witness and the single-flight decline are per volume, and
`BackendRouter::lane_allocators` names the default slot's alias of the first
volume once (the fleet was running TWO refill tasks on volume 1). The fleet
row is the parent's on `squeeze-test`; this note claims the attribution and
the in-process contracts only.

## 1. Evidence

Rows `.benchmarks/rows-f15-day2-20260907/box-seq2-{1C,4C,9L4,7L3}/` — the
matrix's per-phase snapshots `m{0,50,53}_p{0..4}.json.gz` (p0 before the
probe, p1..p4 after A1/B1/B2/A2), the 1 Hz samples `samples/m{0,50}.jsonl.gz`
(1C, 4C), the ior phase outputs (the phase boundaries: 4C's A1 12–75 s, B1
78–173 s, B2 173–274 s, A2 274–350 s on the sampler's clock), and the
daemon logs of the same run (`/tmp/five/d4/box-seq2/keep2-4C/m{0,50}.log`,
matrix `s11mpiio-1788803158`): the authority's `S9: lane free harvest
served N block(s) of lane L/16 on vol_tag T` line per non-empty harvest,
the co-writer's rate-limited refusal line naming the volume. `vol_tag`
`0xf4b8e4a505a2d52b` = `nvme32n1`, `0xf9dd6ee406d70793` = `nvme33n1`
(`xxh3_64` of the volume id). m50 is lane 2, m53 lane 5.

## 2. The per-phase ledger (m50 / m53, deltas per phase)

| row | gauge | A1 | B1 (fpp) | B2 (fpp) | A2 |
|---|---|---|---|---|---|
| 4C m50 | `alloc_lane_enospc_refusals` | 0 | 733 | 801 | 2,734 |
| 4C m50 | `alloc_lane_harvest_declined_stale` | 1 | 677 | 755 | 2,635 |
| 4C m50 | `alloc_lane_harvests` (RPCs) | 196 | 395 | 394 | 299 |
| 4C m50 | non-empty harvests served to lane 2 (authority log) | 151 | 316 | 277 | 231 |
| 4C m50 | `alloc_lane_harvest_coalesced` | 81 | 249 | 242 | 304 |
| 4C m50 | `alloc_lane_pushed_harvests` / `free_grace_lane_push_wakes` | 275 / 165 | 508 / 211 | 516 / 234 | 361 / 159 |
| 4C m50 | `backend_placement_lane_failovers` / `_exhausted_picks` | 0 / 0 | 19 / 1 | 21 / 0 | 64 / 0 |
| 4C m50 | `rewrite_shadow_supply_closes` / `_blocks` / `_bounded` | 69 / 4,647 / 0 | 436 / 6,362 / 96 | 423 / 6,555 / 101 | 111 / 5,980 / 0 |
| 4C m50 | `write_enospc_refusals` | 0 | 0 | 0 | 0 |
| 1C m50 | refusals / declined / harvests | 2 / 2 / 180 | 1,577 / 1,518 / 396 | 373 / 347 / 386 | 680 / 666 / 300 |
| 1C m53 | refusals / declined / harvests | 1 / 0 / 193 | 1,525 / 1,473 / 406 | 390 / 339 / 393 | 5,289 / 5,074 / 328 |
| 9L4 m50 (single-flight OFF) | refusals / declined / harvests | 122 / 0 / 399 | 680 / 0 / 1,248 | 1,017 / 0 / 1,582 | 1,386 / 0 / 1,838 |
| 7L3 m50 (refill hint OFF) | refusals / declined / harvests | 32 / 11 / 188 | 3,149 / 2,868 / 369 | 3,876 / 3,612 / 429 | 5,905 / 5,646 / 325 |

Three structural facts fall out before any time series:

* **`declined_stale` ≈ refusals on every C phase** (92–97 %). A refusal is
  one `allocate_block()` refused; under the single-flight lever a refused
  call asked the authority only when the witness had moved since the last
  empty reply, so nearly every refusal was a slice that did NOT ask.
* **The counter is 2 per slice per parked allocation.** The placed
  allocation tries the picked volume, then the sibling, then parks one
  slice (50 ms on a co-writer — no plane, `pressure_park_slice_ms` = 50);
  both `allocate_block` refusals count. A parked allocation therefore
  reads 40 refusals/s; 733 refusals in B1 ≈ 18 allocation-seconds parked
  over 19 refusal-seconds, i.e. the parks were short and bursty, never a
  sustained stall (`write_enospc_refusals` 0: no park reached the 1 s wall).
* **9L4's lower fpp count is not a cheaper decline.** With the single
  flight off every slice issues two RPCs (each a three-pass serve on the
  authority), so a slice is longer and a second of parking counts fewer
  refusals; its harvests are 3–5×. Within the C rows' own spread (B1: 733
  vs 1,577 on m50 across 1C/4C) the leg is not distinguishable, and its
  throughput was par. The decline is NOT convicted of costing parked time
  on this evidence: its window is bounded by the prodded 500 ms grant
  cadence (`free_grace_prod_renew_ms` 500 throughout every load phase).

## 3. The time series at the refusal instants (4C, m50 = lane 2)

The 1 Hz join of the co-writer's samples with the authority's serve log
(`/tmp/f15_join.py`, the reduce script's columns plus the authority's
per-volume serves to lane 2 and the refusal log's volume):

```
  t  dRef dHarv dHrvBlk reach hint epochs parkMiB | auth→lane2: v32 v33 | refusal log
  83     3     7     115    11   43      3   128  |  47  21 | nvme33n1 exhausted (0 foreign)
  84    97     8      68     0    0      0     0  | 124  70 |
  98    88     6      66    16   66      0     0  | 169  70 |
 113    65    10     256    53   22      1     4  | 107  22 | nvme33n1 exhausted
 115   104     6     156   124   92      0     0  | 115  41 |
 144    24     2       0     0    0      4   340  |   0   0 | nvme33n1 exhausted
 145   112     8     228   171  118      0     0  | 128 100 |
 149    16     4      82     0   38      0     0  |  65  33 |
 150    48     7      98    77  178      0     0  | 153  70 |
 --- A2 ---
 308   234    12     281    13    0      1   380  |  36  21 |
 311   837     9     152    78  187      1     4  |  84 111 |
 332   732     9     205    95  173      1   380  |  81 106 |
 345   712    11     275    95  108      1   228  |  13  29 |
```

Per phase: B1 — 733 refusals in 19 seconds, burst p50 24 / max 112 per
second; at the burst-second sample the hint (the authority's SUMMED
advertisement) was nonzero for 514 of them and 0 for 219, parked keys sat
in this mount's open epochs for 153, and the authority served lane 2
2,333 blocks within those same seconds. A2 — 2,734 refusals in 9 seconds,
bursts of 234–837 per second (the shared file's 32 ranks park together),
the hint nonzero for 2,450 of them, and **parked keys present for 2,733 of
2,734** — the one shared-file epoch held 57–95 blocks (228–380 MiB) at
every burst. The authority's grace ring held 1,000–3,000 offsets
throughout with `bound_age` ≈ 2 s and released 500–2,600/s.

Where the lane's supply was per volume: the authority served lane 2 on
**v32 6,344 / v33 3,512 blocks in B1** (166 / 150 non-empty RPCs, 38 vs 23
blocks per RPC), 6,270 / 3,327 in B2, and the other lanes show the mirror
skew (lane 1: 2,414 / 7,213) — file-per-proc places each rank's file
mostly on one volume and its displaced blocks return there, so a lane's
two shares run out of step. One burst in full (17:48:20Z, t ≈ 142–144):
three RPCs served 64 blocks each on v32 while two served ONE block each on
v33; the refusal log named `nvme33n1` exhausted; the co-writer harvested
228 blocks within the second and the burst ended.

## 4. The attribution table

| cause (what the refused slice was waiting for) | share of refusals | evidence | fix |
|---|---|---|---|
| **The counter's multiplicity** — one parked allocation refuses on BOTH volumes per 50 ms slice | structural: refusals = 2 × slices; ≈ 40/s per parked allocation | `allocate_placed_block`'s loop; 733 refusals ≈ 18 allocation-seconds; `write_enospc_refusals` 0 | none — the gauge stays the per-volume refusal count; read `refusals ÷ (2 × 20)` as parked allocation-seconds |
| **Single-flight declines** — the slice did not ask the authority because nothing it had advertised had moved | 92–97 % of the count | `declined_stale`/refusals on every C phase; the 500 ms prodded cadence bounds every window | the witness is now PER VOLUME (§5.2): a grant that moved only the sibling's share re-arms nothing, a grant advertising this volume's supply ends the decline — the decline was bounded, not sticky; the fix is its honesty and RPC economy |
| **Supply on the authority for the SIBLING volume while this volume asked** | 20–30 % of harvest RPCs came back empty (B1 395 sent vs 316 served, B2 394 vs 277, A2 299 vs 231, A1 196 vs 151); every pushed wake fired on every low volume | the summed hint (`free_grace_lane_supply_hint`) cannot name the volume; `alloc_lane_pushed_harvests` ≈ 2.4 × wakes | the grant's per-volume vector (§5.2): the pushed decision and the ahead witness read the volume's own entry; `alloc_lane_volume_hint_skips` counts the empty RPCs saved |
| **Supply parked in this mount's own open epochs** | fpp: ~20 % of refusals had parked keys at the sample (32–112 blocks in 3–4 epochs); A2: 100 % (57–95 blocks in the one shared-file epoch) | `rewrite_shadow_parked_bytes` at the burst seconds; `rewrite_shadow_supply_close_bounded` 96–101 per fpp phase; the close's yield returns to whichever volume the largest epoch's keys live on | the close plans PER VOLUME (§5.1): the short volume's tick closes the epochs whose keys live on it |
| **Supply in the authority's grace ring** (the loop's hold, `bound_age` ≈ 2 s) and a lane at its capacity edge (`alloc_lane_headroom_pct` 2 %, `alloc_lane_share_needed_blocks` 497 of 512) | the remainder — the honest wait | `free_grace_offsets` 1,000–3,000 held, `bound_age` ≈ 2 s; 320 live blocks + 200–300 in transit + 50–100 parked + the reclaim queue ≈ the 1,024-block share | not this note's: the hold time is the free-grace program's (`.benchmarks/2026-09-06-free-grace-hold-time.md`); a larger share is the operator's |
| **Supply reachable locally on the sibling** | ≤ 19–64 refusals per phase (the pick's refusal, then the sibling succeeds) | `backend_placement_lane_failovers`; `_exhausted_picks` 0–1 | none — the failover works; the pick is right |
| **The aliased default allocator engaged twice** | every wake ran 3 pushed decisions instead of 2 on volume 1 (one coalesced or declined); the claim-rate EWMA sampled twice per tick | the co-writer log's doubled `lane ENGAGED on volume 'nvme32n1'` line per mount lifetime (3 per lifetime for 2 volumes); `pushed ≈ 3 × wakes` in the samples | `lane_allocators` dedup (§5.3) |

What the table does NOT name: the placement pick (`_exhausted_picks` 0–1
per phase) and the ahead watermark (at its `share/4` cap of 128 on every
sample; the ahead tick never fired in the fpp phases because the pushed
wake covers it at 2–4/s).

## 5. What landed (this branch, `perf/cowriter-fpp-supply-residue`)

### 5.1 The supply-coupled close plans per volume

`SQUEEZEFS_REWRITE_SUPPLY_CLOSE_PER_VOLUME` (default on). `RewriteEpoch`
counts its parked A keys per `vol_tag` at the park (the key's allocator is
resolved once — `parked_by_volume`); the close sink captures the asking
allocator's tag; `DataRouter::supply_close_epochs(vol_tag, deficit)` weighs
each epoch by the keys it parks ON THAT VOLUME and runs the same
`supply_close_plan` (largest first until the yield on that volume covers
the deficit). An epoch parking only elsewhere is no candidate (never
`bounded`); a tick with parked keys only elsewhere declines
`rewrite_shadow_supply_close_declined_offvolume`. Face:
`rewrite_shadow_supply_close_volume_blocks` (⊆ `_blocks`). `0` = the
mount-wide plan verbatim. Every §5.6 crash window of the rewrite program
holds: the close is `close_rewrite_epoch` unchanged.

### 5.2 The grant's lane-supply hint per volume

`Grant::lane_supply_volumes: Vec<(vol_tag, blocks)>` beside the summed
`lane_supply_blocks` (`Grant` gives up `Copy`); `CLUSTER_WIRE_SCHEMA` 2 → 3
(KD-7: a mixed fleet refuses loud at the handshake — a 2-speaker's member
would silently read the sum for every volume). The authority's
`LaneSupplySource` answers the vector (one O(1) per-lane counter per
allocator, KD-FG-4 stands); the member lands the vector BEFORE the sum so
the wake's pushed ticks read it. Under `SQUEEZEFS_ALLOC_LANE_VOLUME_HINT`
(default on) each allocator reads `lane_supply_hint_for(vol_tag)` for the
pushed decision and the ahead witness, and its single-flight witness is
`lane_supply_hint_gen_for(vol_tag)`: +1 per grant advertising NONZERO
supply for that volume, +1 per grant whose vector did not name it (the
every-grant law survives for an un-advertised volume; a volume no vector
ever named reads the mount-wide generation, and the two scales are seeded
together so a stamp never aliases). Face: `alloc_lane_volume_hint_skips`
— pushed decisions the vector declined that the sum would have fired (the
empty RPCs saved). `0` = the sum and the mount-wide generation for every
volume.

### 5.3 One allocator, one engagement

`BackendRouter::lane_allocators` deduplicates the default slot's alias of
the first registered volume, so `engage_co_writer_lanes` spawns one
ahead-refill task per allocator and the two sum gauges and the authority's
lane-supply source share the one list. No knob: it is a defect, and a
single-writer / authority mount is byte-identical (the alias IS the same
allocator; the second engagement was idempotent on its `OnceLock`s and
wasteful in its task and its OPEN round trip).

## 6. Contracts (red-first, all green — `--test-threads=1`)

| suite | new contracts | result |
|---|---|---|
| `tests/rewrite_shadow_supply_close_tests.rs` | 7: a starving volume closes the epoch whose keys live on it first (two-volume fs rig — F1 on A, F2 on B, B's tick publishes F2 and leaves F1 open, `volume_blocks` +2, a second B tick declines `offvolume`, A's tick then publishes F1); the lever-off twin (B's tick publishes the largest epoch, F1, restocks A, `bounded` +1, no volume gauge moves) | 10 passed |
| `tests/cowriter_lane_placement_tests.rs` | 7: the router names the aliased default allocator once | 12 passed |
| `tests/mw_data_alloc_lane_tests.rs` | §11: a decline on A ends when A's advertised supply moves (a B-only grant leaves A declined and skips A's pushed tick — `alloc_lane_volume_hint_skips` +1 — while B harvests; an A grant ends A's decline for one RPC; an unnamed volume keeps the every-grant law); the lever-off twin reads the sum verbatim | 32 passed |
| regression: `free_grace_lane_visible_tests` 10, `cluster_wire_tests` 23 (+1 ignored), `dlm_membership_tests` 51, `membership_renewal_isolation_tests` 5, `mw_cowriter_lane_tests` 26, `mw_cowriter_free_tests` 50, `mw_cowriter_free_leak_tests` 9, `dlm_cowriter_tests` 18, `dlm_multi_writer_tests` 16, `derivation_sweep_tests` 47, `env_knob_convention_tests` 21, `audit_instruments_tests` 26, `reader_free_grace_tests` 61 | | all green |

Plus `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo clippy --all-targets -- -D warnings`, `RUSTDOCFLAGS="-D
warnings" cargo doc --no-deps` (see the report).

## 7. What is NOT claimed

* **The fleet row.** No `s11-mpiio` row was run on this branch; the
  refusal, RPC and throughput effect of the three landings is the parent's
  A-B-B-A on `squeeze-test`. The expected faces: `alloc_lane_volume_hint_skips`
  ≈ the empty-RPC share (20–30 % of the pre-landing harvests),
  `rewrite_shadow_supply_close_volume_blocks` ≈ `_blocks` (the yield lands
  where the deficit is), `alloc_lane_pushed_harvests` ≈ 2 × wakes on a
  two-volume mount.
* **A refusal-count target.** The remainder row of §4 — the lane at its
  capacity edge behind a ≈ 2 s grace hold — is the binding term on this
  geometry and none of the three landings moves it; a lower count needs
  the hold-time program's next rung or a larger share. The three landings
  make the co-writer's supply decisions honest per volume and cut the
  authority's empty serves; the parked-time effect is owed to the row.
* **Anything about the decline's stickiness.** Its window was bounded by
  the prodded grant cadence on every C phase; per-volume makes it honest
  (a sibling's grant no longer re-arms an RPC here), not shorter.
* **The counter's shape.** `alloc_lane_enospc_refusals` keeps counting
  each refused `allocate_block` — 2 per slice per parked allocation on a
  two-volume mount; the note's arithmetic (÷ 40 per second) is the reading,
  not a new gauge.
