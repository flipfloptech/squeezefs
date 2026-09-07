# Finding 15 term 1 on the fleet — the four re-derivations landed; the hold fell 4×; the row still FAILS (2026-09-06)

**Verdict:** the four KD-FG-11 re-derivations (user decision 2026-09-06 —
`.benchmarks/2026-09-06-free-grace-ladder-rederivation.md` items 1–3,
`.benchmarks/2026-09-06-free-grace-checkpoint-composite.md` item 4,
composed at `ee49cf04`) did on the fleet what they predicted in-process:
the authority's reallocation bound age **9,382 → 1,836 ms**, the members'
acknowledgement lag **6.8–6.9 s → 0.65–0.83 s**, offsets held in the
grace ring at capture **537 → 68**, the `checkpointed → min_acked` stage
**7,059 → 4,024 ms** averaged over the row, closure exact, `forced =
laggard_fences = alloc_stalls = drain_overdue = 0`. **The s11-mpiio
sustained gate still FAILS** (1,649 → 649 MiB/s, baseline 1,551 → 646)
and the co-writers' lane-ENOSPC refusals fell only **50,485 → 35,007**.
The hold is no longer the binding term. The authority's harvest-serve
timeline shows the next one: a lane's recycled supply arrives in
**bursts with 9–31 s gaps**, aligned with the co-writers' rewrite-epoch
closes, and every ENOSPC storm sits inside a gap. Displaced blocks are
parked in the open rewrite epoch until it closes — a stage the free-grace
rate equation never included, because its clock starts at the free.

## 1. Venue and instrument

Dev box (32 CPUs, kernel `7.2.3-cachyos-lto` — the venue CHANGED today,
so the baseline was re-taken on the same boot), `tests/mw_fleet.sh create
N=1 --cowriters=8` with `SQZ_MWFLEET_OSS_GB=32 SQZ_MWFLEET_RANGE_CUSTODY=1`
(two 32 GiB data volumes, W = 16 lanes ⇒ 512 blocks = 2 GiB per lane per
volume, 4 GiB per lane), `tests/run_mw_matrix.sh s11-mpiio` (32 ior ranks,
one shared 10 GiB file, 4 MiB block-cyclic, the sustained-window gate),
wrapped by `/tmp/five/d4/run-keep.sh`; rows reduced by
`.benchmarks/rigs/2026-09-06-free-grace-fleet-reduce.py`. Baseline =
`release` binary of `6cbbd848` (the tip before term 1); after = `ee49cf04`.
Both rows from zero, back to back, ~4 min apart. Artifacts:
`.benchmarks/rows-t1-s11-20260906/{baseline,all4}/` (every mount's
`.stats`, `matrix.log`, the ior iteration table, the authority's
harvest-serve timeline, the co-writer m50's refusal timeline).

## 2. The rows

| | baseline `6cbbd848` | all four `ee49cf04` |
|---|---|---|
| ior iterations (MiB/s) | 2,093 2,044 1,436 1,501 1,224 250 725 1,179 726 920 572 632 687 666 597 | 2,406 2,242 1,346 1,359 1,211 704 756 552 835 760 757 429 |
| sustained-window gate | **NOT SUSTAINED** 1,551 → 646 | **NOT SUSTAINED** 1,649 → 649 |
| `free_grace_bound_age_ms` (capture) | 9,382 | **1,836** |
| `free_grace_hold_ms` (capture) | 7,946 | **2,014** |
| hold stages, mean over the row (ms): defer→ckpt / ckpt→min_acked / min_acked→released / total | 465 / **7,059** / 53 / 7,577 | 245 / **4,024** / 38 / 4,306 |
| member ack lag ms (min / mean / max, 8 members) | 6,954 / 7,294 / 7,701 | **1,130 / 1,486 / 1,836** |
| member-side `free_grace_acked_lag_ms` (m50 / m55) | 6,790 / 6,899 | **652 / 827** |
| offsets held at capture | 537 (2.10 GiB) | **68** (0.27 GiB) |
| closure `deferrals ≡ releases + offsets` | 47,827 ≡ 47,290 + 537 ✓ | 38,469 ≡ 38,401 + 68 ✓ |
| tripwires `forced / laggard_fences / alloc_stalls` | 0 / 0 / 0 | 0 / 0 / 0 |
| `prod_renew_ms` / authority `checkpoint_ceiling_ms` | 1,000 / — | **500 / 500** (the composite in force) |
| members: qualify lag / drain lag / pass interval / advertised ceiling | 2,022 / 4,000 / 1,000 / — | **622 / 0 / 600 / 600** |
| members: `drain_observed` / `drain_overdue` / `serves_inflight` | — | 173–184 / **0** / 0 |
| `meta_kv_checkpoints` (authority, ~150 s) | 271 | 335 |
| Σ `alloc_lane_enospc_refusals` (8 co-writers) | 50,485 | **35,007** (−31 %) |
| per co-writer ENOSPC refusals | 968 … 14,069 | 21 … 7,469 |
| `block_claim_anomalies` (Σ) | 16 | 160 |
| fsync failures / `writeback_errors_latched` | 0 | 0 |

Every member gauge reads the four items engaged: qualify = the advertised
landing ceiling 600 + skew 22, drain 0 (observed — 173–184 promotions by
observation, none overdue), the pass floor at the advertised 600 ms, the
authority checkpointing at the 500 ms decision ceiling under the ask
(335 vs 271 cycles — the row's ask was in force ≈ 40 % of the time).

## 3. Why the row still fails — the serve timeline

The authority logs every non-empty harvest it serves. Lane 8 (co-writer
m50), after row:

```
00:55:48 64 64  00:55:49 64 64  00:55:50 16  00:55:52 64  00:55:53 64  00:55:55 62 …
00:56:06 64 64 64 64 64 64 64 22   (a burst of ~470 blocks in one second)
00:56:07 1  00:56:08 1 6  00:56:09 5  00:56:12 20  00:56:15 64 64
        ── 9 s, nothing ──
00:56:24 57  00:56:26 1
        ── m50's ENOSPC storm: 00:56:26–00:56:33, 184 refusals ──
00:56:30 64  00:56:31 64  00:56:32 64  00:56:33 64 64 6 58 …
00:56:43 64 64 3
        ── 13 s, nothing ──
00:56:56 64  00:57:00 57  00:57:01 64 64 64 64 …
```

Gaps ≥ 8 s between serves for lane 8: baseline 9, **31**, 8, 8 s; after
9, **13**, 9, 9, 9 s. m50 made **6,050 harvest calls** and received
3,858 blocks in **82 non-empty serves** (48 of them full 64-block
grants) — so the ~5,970 empty calls fell in windows where its lane had
NOTHING on the authority's list, and the supply arrived in bursts of
several hundred blocks at once. The `released → served` ages (mean
10 s, 48 % > 8 s) are the lowest-first pick's artefact on a standing
pool, not a throughput term: when a burst lands the co-writer takes 64
per call at 7 calls/s.

What a burst is: `rewrite_shadow_swaps` = 38 epoch closes on m50 over
the row, `rewrite_blocks` = 3,506 displaced → ≈ 92 blocks per close on
average, but the serve sizes say the closes are bimodal — a few hundred
blocks at an iteration boundary, a handful mid-iteration. The rewrite
program's epoch parks a displaced block's A key until the epoch closes
(design-rewrite-program §5.3, KD-1.6), and on the s11 shape none of the
routine triggers fires DURING an iteration: **full coverage** needs
`epoch blocks × block_size ≥ file size`, and a co-writer's 1.25 GiB slice
of a 10 GiB shared file never reaches it; **fsync / RELEASE** come at the
iteration's end; the **ENOSPC early-close** (KD-1.7) fires only AFTER a
`StorageFull`, then retries once — which is the refusal storm's shape.
So a co-writer's displaced blocks reach the free (and only then the
ring, the release, the harvest) in a burst at the epoch close, and its
lane must hold live data + the iteration's new blocks + the parked A
keys + the previous burst still in flight: per volume, 160 + 160 + up to
160 against a 512-block share. The margin is gone before the recycled
burst returns, whatever the ring's hold is.

The rate equation's stage list (design-free-grace-sustain §3.2) starts
at the deferral; `free_grace_hold_phase_ns.defer_checkpointed` is stamped
at `finish_free`. The displacement → free stage — the epoch's parking —
is unmeasured, and it is now the dominant one.

## 4. What is settled and what is next

* **Settled by this row:** the coherence windows are no longer the wall.
  Items 1–4 are worth their cost (2× checkpoints/renewals only while an
  ask is in force; `meta_kv_checkpoints` +24 % over the row) and every
  coherence contract held live (zero fences, zero overdue drains, zero
  fsync failures; `block_claim_anomalies` 160 is the standing term-3
  residue — its lineage note is on the board, it did not grow into a
  failure).
* **Not settled:** the sustained gate. The next term is the rewrite
  epoch's parking of displaced supply on a lane-partitioned co-writer.
  Two things are owed before any lever: (1) a **time-series instrument on
  the fleet row** — sample the authority's and one co-writer's `.stats`
  each second (bound age, ring offsets, per-lane list counts,
  `rewrite_shadow_parked_bytes`, `rewrite_shadow_open_epochs`, lane free
  count, ENOSPC refusals, harvests) against the ior iteration boundaries,
  so the parking stage is a measured column and not an inference from
  serve logs; (2) the lever's design as an adjudication: a **supply-coupled
  epoch close** on co-writers — close the shadow epoch when the lane's
  reachable supply falls below its parked A-key count (the KD-1.7
  early-close made AHEAD of the `StorageFull`, on the same watermark the
  ahead-refill task already samples), so the parked supply is bounded by
  the lane's headroom instead of the iteration's length. It changes an
  epoch-close trigger of the rewrite program, hence a design decision.
* The `read_settle_lost_serialized` invariant tripwire fired on the
  authority in BOTH rows (`invariant_tripwires` 420 / 326; 71 log lines
  "incarnation moved under a serialized settle") — a standing must-stay-0
  violation on this venue, unrelated to today's change, filed.
