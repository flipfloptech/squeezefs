# Finding 15 — the file-per-proc lane-ENOSPC residue on the passing tip: attribution and the per-volume fixes (2026-09-07)

**Verdict (re-stated after the D-C-C-D on squeeze-test, §8).** On
`886d4e31` the `s11-mpiio` fleet (authority + 8 co-writers, two 32 GiB
data volumes, W = 16 ⇒ 512 blocks per lane per volume) sustains every
phase, but the file-per-proc phases refuse 7,486–16,065 lane allocations
per phase and the aged shared-file phase A2 up to 16,065
(`.benchmarks/2026-09-07-f15-b1-squeeze-test-seq2.md` §2). **The refusals
are the capacity law of the venue, not code**: the matrix keeps the
previous phase's file (`ior -k`), so through B1/B2/A2 each mount's lane
holds 640 LIVE blocks (the 10 GiB shared file + the 10 GiB fpp files, 2.5
GiB per mount) of its 1,024-block share, and the recycle loop's transit
(parked → shipped free → the authority's grace ring ≈ 2.2 s → the lane list
≈ 0.7 s → harvest) is ≈ 3.4 s, so ≈ 215 more blocks are in flight at the
measured 63 blocks/s per mount; the ≈ 170 blocks left cannot carry an
iteration's 4.5 s front of 80 blocks/s for the 3.4 s before its first
displaced block comes back — the lane is dry for ≈ 1 s of every
iteration, on every co-writer, and every refusal is a 50 ms park slice
counted twice (both volumes tried). A1, the same workload with 320 live
blocks, refuses nothing (5–18 per row). Every refusal is a park slice
(never a synchronous `ENOSPC` — `write_enospc_refusals` 0 on every row),
92–97 % are single-flight DECLINES bounded by the 500 ms prodded grant
cadence, and the authority's list wait is 0.69 s mean on D and C alike.

Of the three landings the first pass made (§5), the **per-volume lane-supply
hint** (`Grant::lane_supply_volumes`, `CLUSTER_WIRE_SCHEMA` 3) engaged as
priced — harvest RPCs −30 % at +25 % blocks per RPC, `alloc_lane_volume_hint_skips`
205–434 per phase, the list wait unchanged — and stays; the
**`lane_allocators` dedup** is a defect fix and stays; the **per-volume
supply-coupled close** was FALSIFIED by the D row (its premise — a close's
yield must land on the exhausted volume — is wrong because the lane-aware
placement equalizes the volumes' stocks, and it stranded the covered
sibling's parked keys to the iteration boundary: 14 % of the displaced
keys released by the routine closes vs 0.3–1 % on both C rows) and is
RETIRED (`0bd03455` → this branch). The fleet row is the parent's on
`squeeze-test`; this note claims the attribution, the capacity statement
and the in-process contracts.

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

### 5.1 The supply-coupled close plans per volume — RETIRED (§8)

Landed as `SQUEEZEFS_REWRITE_SUPPLY_CLOSE_PER_VOLUME` (default on):
`RewriteEpoch` counted its parked A keys per `vol_tag`, the close sink
carried the asking allocator's tag, and the plan weighed each epoch by the
keys it parked ON THAT VOLUME. The D row measured the harm (§8.3) and the
premise was wrong (§8.4): the knob, the per-volume parked accounting and
the two faces (`rewrite_shadow_supply_close_volume_blocks`,
`…_declined_offvolume`) are deleted; the plan is mount-wide, as first
landed, and contract 7 of `tests/rewrite_shadow_supply_close_tests.rs`
now pins the mount-wide law on the two-volume rig.

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
| `tests/rewrite_shadow_supply_close_tests.rs` | 7 (as re-pinned after §8): a starving volume publishes the MOUNT's largest epoch whose keys live on its sibling (two-volume fs rig — B four short, F1 4 keys on A, F2 2 on B: B's tick publishes F1, `bounded` +1, A restocks 2 → 6 and the next placed allocation lands on A with no refusal; the next B tick publishes F2). The first pass's per-volume contract pair is gone with the mechanism | 9 passed |
| `tests/cowriter_lane_placement_tests.rs` | 7: the router names the aliased default allocator once | 12 passed |
| `tests/mw_data_alloc_lane_tests.rs` | §11: a decline on A ends when A's advertised supply moves (a B-only grant leaves A declined and skips A's pushed tick — `alloc_lane_volume_hint_skips` +1 — while B harvests; an A grant ends A's decline for one RPC; an unnamed volume keeps the every-grant law); the lever-off twin reads the sum verbatim | 32 passed |
| regression: `free_grace_lane_visible_tests` 10, `cluster_wire_tests` 23 (+1 ignored), `dlm_membership_tests` 51, `membership_renewal_isolation_tests` 5, `mw_cowriter_lane_tests` 26, `mw_cowriter_free_tests` 50, `mw_cowriter_free_leak_tests` 9, `dlm_cowriter_tests` 18, `dlm_multi_writer_tests` 16, `derivation_sweep_tests` 47, `env_knob_convention_tests` 21, `audit_instruments_tests` 26, `reader_free_grace_tests` 61 | | all green |

Plus `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo clippy --all-targets -- -D warnings`, `RUSTDOCFLAGS="-D
warnings" cargo doc --no-deps` (see the report).

## 7. What is NOT claimed

* **A refusal-count effect from any landing.** The D row (§8) shows none
  of the three moved the fpp refusals, and the capacity statement (§8.5)
  says none could: the binding terms are the venue's live occupancy and
  the loop's hold. The per-volume hint's claim is the RPC economy it was
  built for (−30 % harvests, +25 % blocks/RPC, skips 205–434/phase);
  the dedup's is correctness; the per-volume close is retired.
* **Anything about the decline's stickiness.** Its window was bounded by
  the prodded grant cadence on every C phase and on D; per-volume made it
  honest (a sibling's grant no longer re-arms an RPC here), not shorter.
  The authority's list wait — `alloc_lane_visible_phase_ns.released_served`
  — is 692/693/710 ms mean (D/2C/3C, B1), p50 ≤ 512 ms, p90 ≤ 2 s on all
  three: the decline moved nothing the co-writer could have reached.
* **The counter's shape.** `alloc_lane_enospc_refusals` keeps counting
  each refused `allocate_block` — 2 per 50 ms slice per parked allocation
  on a two-volume mount; the note's arithmetic (÷ 40 per second) is the
  reading, not a new gauge.
* **The D-vs-C delta as a measured regression.** D's B1/A2 refusals per
  second (224 / 350) sit above every C row (79–150 / 160–288 across four),
  but the C rows themselves spread 2× and per-mount counts spread 2× within
  one row; with one D row the 15–20 % lower supply-close rate (§8.3) is the
  direction of the delta, not its measured size. The retirement stands on
  the falsified premise and the stranding instrument, which are exact.

## 8. The D-C-C-D re-attribution (squeeze-test, 2026-09-07 evening)

D = `0bd03455` (the three landings), C = `886d4e31`; rows 1D 2C 3C, row 4D
excluded (its sizing probe read 235 MiB/s). Snapshots
`/tmp/five/d4/box-seq3/keep3-{1D,2C,3C}/rows/s11mpiio-*/m{0,50..57}_p{0..4}.json`
(every mount), the authority + m50 logs, no per-second samples. Phase
lengths differ (D 57/91/92/75 s, 2C 70/107/107/83 s, 3C 56/79/80/64 s —
19/23/17 iterations), so the ledger below is per phase SECOND.

### 8.1 The Σ8 ledger, per phase second

| gauge (Σ8 co-writers, per s) | 1D A1 / B1 / B2 / A2 | 2C | 3C |
|---|---|---|---|
| `alloc_lane_enospc_refusals` | 0.1 / **224** / 103 / **350** | 0.3 / 115 / 127 / 160 | 0.1 / 150 / 133 / 288 |
| `alloc_lane_harvest_declined_stale` | 0.1 / 218 / 100 / 345 | 0.2 / 110 / 121 / 156 | 0.2 / 142 / 127 / 280 |
| `alloc_lane_harvests` (RPCs) | 18.9 / 25.2 / 25.7 / 24.6 | 23.9 / 33.0 / 33.1 / 31.4 | 20.6 / 32.4 / 31.0 / 31.3 |
| `alloc_lane_harvested_blocks` | 815 / 756 / 753 / 702 | 808 / 791 / 793 / 771 | 728 / 779 / 776 / 738 |
| blocks per harvest RPC | 43 / 30 / 29 / 29 | 34 / 24 / 24 / 25 | 35 / 24 / 25 / 24 |
| `alloc_lane_pushed_harvests` ÷ `free_grace_lane_push_wakes` | 1.00 / 1.25 / 1.26 / 1.20 | 1.73 / 2.29 / 2.27 / 2.28 | 1.62 / 2.41 / 2.38 / 2.21 |
| `alloc_lane_volume_hint_skips` | 3.6 / 4.8 / 4.5 / 4.5 | — | — |
| `rewrite_blocks` (the churn) | 817 / 502 / 525 / 644 | 803 / 521 / 545 / 709 | 737 / 515 / 543 / 682 |
| `rewrite_shadow_supply_closes` | 5.7 / 29.1 / 29.7 / 9.6 | 7.4 / 34.1 / 35.6 / 11.0 | 4.7 / 37.0 / 38.2 / 10.8 |
| `rewrite_shadow_supply_close_blocks` | 373 / **430** / 433 / **497** | 504 / 516 / 537 / 626 | 318 / 513 / 529 / 600 |
| `…_declined_covered` / `_no_parked` / `_offvolume` / `_bounded` (B1) | 10.7 / 10.2 / 0.5 / 2.9 | 17.2 / 15.4 / — / 8.1 | 14.0 / 15.0 / — / 7.7 |
| `backend_placement_lane_failovers` / `_exhausted_picks` (B1) | 1.3 / 0 | 2.0 / 0 | 2.4 / 0 |
| `write_enospc_refusals`, `free_grace_forced_releases`, `invariant_tripwires` | 0 | 0 | 0 |

Authority faces, all three rows alike: `free_grace_hold_ms` 1,947–2,292,
`free_grace_bound_age_ms` 2,080–2,737 at the phase ends,
`free_grace_pressure_pct` 97–100, `free_grace_prod_renew_ms` 500
throughout, `free_grace_offsets` 400–1,500 held, `alloc_lane_supply_blocks`
(the co-writers' released, unharvested supply) 620–2,450.

### 8.2 The lane-visible ledger — the decline moved nothing

`alloc_lane_visible_phase_ns` (Σ8, B1): `released_served` — the time a
released block sat on the authority's list before the co-writer's harvest
took it — mean **692 / 693 / 710 ms** (D / 2C / 3C), p50 ≤ 512 ms, p90 ≤ 2 s
on all three; `served_visible` (the harvest RTT to adoption) 6.0 / 7.8 /
7.8 ms. The per-volume decline witness and the per-volume pushed decision
changed WHEN the co-writer asks (fewer, better-aimed RPCs) and not how long
its supply waited: hypothesis (b) — the witness moving too rarely — is
falsified by the instrument built to test it. (The tail is by design: a
volume above its watermark leaves released blocks on the list until it
needs them.)

### 8.3 The per-volume close stranded the covered sibling's keys

Every displaced key leaves its epoch through exactly one close — a supply
close (the refill tick's) or a routine close (fsync / coverage / the idle
sweeper, the iteration boundary on fpp). `rewrite_shadow_bytes` counts
the blocks each close swapped; `rewrite_shadow_supply_close_blocks` the
keys the supply closes released:

| row (B1) | closes: supply / routine | supply-closed keys | of the ≈ displaced (`rewrite_blocks` − the 2,560 fresh) | closes/s per mount |
|---|---|---|---|---|
| 1D | 2,655 / 53 | 39,287 | **91 %** — 3,927 keys (9 %) waited for the boundary; on the B-swapped count, 14 % | 3.65 |
| 2C | 3,663 / 35 | 55,350 | ≈ 100 % (the count exceeds the estimate: same-epoch re-rewrites park their prior B key too) | 4.28 |
| 3C | 2,925 / 12 | 40,570 | ≈ 100 % | 4.63 |

On D the supply close released 430 blocks/s against 516 (2C) and 513 (3C)
— 17 % fewer at the same churn — and ~4–6 k keys per fpp phase sat parked
to the iteration boundary that both C rows released mid-iteration. The
mechanism: the per-volume plan's candidates were the keys parked on the
ASKING volume, and a volume whose stock sat above its own (lower)
watermark declined `covered` (1.3/s per mount on D) — so an epoch whose
keys lived mostly on the covered volume was neither the short volume's
candidate (low weight → `bounded`, 264 in B1) nor the covered volume's
(no tick) until the boundary. `offvolume` (0.5/s) is the visible corner
of it; the stranding itself was invisible to the new faces. A2's doubling
(350/s vs 160–288) is the same shape on the one shared-file epoch —
`offvolume` 90, its highest, and supply-closed blocks/s 497 vs 600–626 —
plus the row spread (§7).

### 8.4 Why the premise was wrong

The plan's premise — a close's yield must return to the EXHAUSTED volume
— assumed a volume's stock is its own. It is not: the lane-aware placement
(`.benchmarks/2026-09-07-cowriter-lane-aware-placement.md`) weighs each
volume by its lane-reachable fraction and keeps the 90 %-of-max band, so
the picks flow to whichever volume holds more stock until the stocks
match, and the failover tries the sibling in the same attempt. A parked
key returning to EITHER volume takes the mount's next write. The data
placement per volume IS skewed (m50's lane-2 supply came back 75 / 25
across the volumes on D, 71 / 29 on 2C, 53 / 47 on 3C; `data_write_lane_submits`
76 / 24 for m50's B1 on D) — that skews where LIVE blocks sit, not where
STOCK can be used. Stock is fungible across a co-writer's volumes; the
capacity law is therefore written per MOUNT (§8.5), and the per-volume
plan could only remove candidates. Retired: contract 7 now pins the
mount-wide law on the two-volume rig (a starving volume publishes the
mount's largest epoch whose keys live on its sibling; the yield restocks
the sibling and the next placed allocation reaches it with no refusal).

### 8.5 The capacity statement (why the remainder is not code)

Per co-writer mount, B1 on D (the other rows within ±5 %):

| term | value | source |
|---|---|---|
| lane share | **1,024 blocks** (4 GiB): 8,192 blocks per 32 GiB volume ÷ W = 16 = 512 per volume, two volumes | the venue |
| live, A1 | 320 (the 10 GiB shared file ÷ 8 mounts, 1.25 GiB) | `ior -b 4m -t 4m -s 80`, 32 ranks, 4 per mount |
| live, B1 / B2 / A2 | **640** — the shared file is KEPT (`-k`) under the fpp phase and the fpp files under A2 | `A1.out` / `B1.out` command lines (`-k`, distinct `-o` names) |
| churn | 63 blocks/s (`rewrite_blocks` 45,774 ÷ 91 s ÷ 8) | Σ8 snapshots |
| loop transit T | ≈ 3.4 s = parked ≈ 0.5 (1 ÷ (3.65 closes/s ÷ 4 epochs) ÷ 2) + ring hold 2.2 (`free_grace_hold_ms`) + list wait 0.69 (`released_served`) + RTT 0.006; the co-writer's own horizon reads 3,000–3,442 ms | authority + co-writer gauges |
| in flight | ≈ 215 = churn × T (the authority's faces: 90–185 per mount in the ring, 80–225 on the lists) | derived, bracketed by gauges |
| available (local + list) | ≈ **170** = 1,024 − 640 − 215 (observed local `alloc_lane_reachable_blocks` 101–384 at the phase ends) | derived |
| iteration | 4.55 s; 4 ranks × 80 blocks = 320 blocks per mount at ≈ 80/s, then the barrier | `B1.out` (20 iterations / 91 s) |
| the front | 80/s × 3.4 s ≈ **270 blocks** must come from stock before the iteration's first displaced block returns | derived |
| dry time | (270 − 170) ÷ 80 ≈ **1.2 s per iteration**, ≈ 24 s per phase per mount; at 40 counted refusals per parked allocation-second and a few allocations parked, the observed 100–300 refusals/s fleet-wide | matches the morning row's 19 refusal-seconds per phase on m50 |
| A1 check | 1,024 − 320 − 215 = 489 ≥ 270 → never dry | 5 / 18 / 6 refusals per row — ✓ |

The remedies are the venue's, not the co-writer's: a lane share ≥ live +
churn × T + the front ≈ 640 + 215 + 270 = **1,125 blocks** (the venue is
≈ 10 % under — a third data volume, W = 8, or not keeping the previous
phase's file all clear it), or a shorter ring hold (the free-grace
program's `hold_ms` 2.2 s — the largest transit term, owned by
`.benchmarks/2026-09-06-free-grace-hold-time.md`'s next rung). The parked
term (≈ 30–55 blocks per mount) is the only co-writer-side one, and it is
15–25 % of the available stock: closing every open epoch at every short
tick would cut it to ≈ 16 and buy ≈ 0.25 s of the 1.2 s dry front — a
candidate lever with a predicted magnitude, not landed (no evidence yet
that its publish cost is free on the authority at 15 k).

### 8.6 What changed on this branch

The per-volume close is retired (knob, accounting, two faces; the plan is
mount-wide as first landed); the per-volume hint and the `lane_allocators`
dedup stay. Contract 7 re-pinned (`a_starving_volume_publishes_the_mounts_largest_epoch_whose_keys_restock_the_sibling`,
RED against `0bd03455`). Not verified: any fleet row on the retired shape
(the mount-wide plan's C rows are its evidence; a D′ row would confirm the
close rate returns to ≈ 34–37/s).
