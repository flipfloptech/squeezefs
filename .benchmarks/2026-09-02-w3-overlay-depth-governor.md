# W-3 — the device overlay rides the write-pipeline depth governor

**Date:** 2026-09-02 (campaign W-3 of the e2e perf audit, write board #3)
**Design:** `docs/design-e2e-perf-audit.md` §3.2 rank 5 / Appendix C #3; the governor
`src/write_pipeline.rs` (2026-07-27 depth campaign + 2026-07-29 probe-up + finding 39's Red
clamp); the overlay arms `docs/design-device-overlay.md` / `docs/design-overlay-overwrite.md`.
**Branch:** `perf/w3-overlay-depth-governor` from `98346dab` (A1 exact histograms + A2 trace
ring + f47's overlay length floor in). Commits: red `2a741aa2`, fix `b8dbce80`, test-fix + rig
`d1ddee31`, note `1b10ad5b` + this field addendum.
**Verdict:** landed, default ON. Field `w_rewrite` (squeeze-test, A-B-B-A, 30 s rows): **par
within noise** (0.978× / 0.992×, drift-corrected ≈ −1 %) with **p99.9 −17 % / −33 %**; the
device-bound tcp-devsub adjudication (the venue that LOST 0.68–0.80× open-loop) is **owed** —
the substrate was held by a foreign mount for the whole time box.
**Time box:** the host reboot at ~17:20 — `task check` deliberately NOT run (batched later);
the named suites + shipped-config clippy ran and are green.

## The finding

The device-overlay ACK-early store issued **open-loop**: per-segment `WRITE_FIXED` + claim +
detached continuation with no admission against a BDP target. Its in-flight bytes rode no R5
component the admission sees, `write_pipeline_inflight_bytes` / `depth_target` /
`admission_waits` described only the accumulation vehicle, and the probe-up governor
(`ProbeCore`) never saw an overlay completion. On device-bound venues the ungoverned overlay
lost **0.68–0.80×** against the governed accumulation path (the overlay notes' own bracket rows:
`.benchmarks/2026-08-15-overlay-b4-overwrite.md` local zram-tcp devsub 783–940 vs 1149–1161
MiB/s at qd4 — the "device-bound substrates prefer `SQUEEZEFS_OVERLAY_OVERWRITE=0`" venue split
recorded in the knob's own registry text), while on the memory-backed field fabric it was
neutral-to-winning.

## The lever (landed)

ONE governor for both write vehicles:

* **Admission** — `try_device_overlay_store` takes a write-pipeline permit
  (`WritePipeline::admit_segment(len, block_size)`: the segment's bytes counted against the
  target at the volume block-size scale, so a 1 MiB segment and a 4 MiB block read the same
  target) **after the registry join and before the claim** — no 3.5 section is held there, and
  a parked writer holds no in-flight claim a settle would await. The permit is the segment's
  in-flight DMA custody on the shared gauge and on the R5 `write_pipeline_inflight` component
  (finding 39's Red clamp now sheds overlay queueing bytes too).
* **Release at the CQE, before the coverage publish** — the continuation
  (`finish_ack_early_store` / `finish_ack_early_bytes`) and the inline ACK-after-CQE arms call
  `overlay_store_complete` first: drop the permit, then (on a LANDED store) feed
  `record_completion_sized` on the destination lane — bytes = the segment, duration =
  admission→CQE, block scale = the volume block size — so the BDP estimate AND the probe-up
  governor learn from overlay completions. Ordering law: a parked admitter holding this block's
  `BLOCK_FLUSH_LOCKS` guard can never wait on a permit whose release needs that guard (the
  publish path takes the guard; the release does not).
* **ACK-early semantics unchanged** — the ACK still returns at store time; only ADMISSION
  waits when the pipe is over target: exactly the accumulation path's honest backpressure.
* **No copy, no per-segment allocation** — the permit is an `Arc` clone plus two words carried
  in the already-boxed continuation (`kernel_op_economy_tests` green, budgets unchanged).
* **Lever:** `SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR` (registered; default ON; `0` = the pre-W-3
  open-loop store — the A/B control). **Engagement:** `overlay_governed_stores` (≡ overlay
  stores admitted; 0 with the lever off) on the stats inode; write-phase census word `ov_admit`
  names a writer parked in overlay admission; the park itself lands in
  `write_pipeline_phase_ns.admit_wait` alongside the accumulation path's.

## Red → green (in-process contracts, `tests/overlay_ack_early_tests.rs`)

| Contract | Tip `98346dab` | `b8dbce80` |
|---|---|---|
| `overlay_store_burst_parks_in_pipeline_admission_beyond_depth_target` — target pinned at 2 blocks, 3 whole-block overlay stores with the mock device gate CLOSED: two admit + ACK early, the third must park (`admission_waits` moves, `inflight_bytes ≤ depth_target`, not ACKed); gate open ⇒ all three ACK, permits settle to 0, readback exact | **RED** — `admission_waits` never moved (open-loop) | green |
| `overlay_completions_feed_the_lane_bdp` — overlay-only stream (16 writers, 20 ms modeled service) must move `write_pipeline_depth_target_base` off the cold floor | **RED** — base pinned at the floor after 1,497 ack-early stores (the lane never learned) | green (learns within the first windows) |

Fixture note: the in-process fixtures never run the R5 sampler (budget 0 ⇒ the pipe caps at
ONE block), so the two contracts give the fixture's pipeline a real cap
(`WritePipeline::with_caps(never_red, 1 GiB)`); the mock slot gained a modeled service time.
The learn contract's loss assertion is delta-based (the dead-ring suite grows
`overlay_ack_early_lost` by design — an absolute `== 0` flaked by test order once).

## Suites (all green, `b8dbce80`+`d1ddee31`)

`write_pipeline_tests` 23, `overlay_ack_early_tests` 14 (×3 runs), `overlay_overwrite_tests` 32,
`device_overlay_tests` 10, `overlay_length_floor_tests` 7, `f44_overlay_rewrite_tests` 2,
`f48_warm_read_overlay_gap_tests` 3, `write_through_coverage_tests` 8, `kernel_op_economy_tests`
2, `env_knob_convention_tests` 21 (the new knob is registered). `cargo clippy --all-targets
-- -D warnings` (the shipped config) clean. `task check` **owed** (time box).

## Local A/B — OWED

The tcp devsub (`SQZ_DEVSUB_TRANSPORT=tcp`, the device-bound venue this finding is about) was
**in use by another agent's live mount** (`/mnt/squeezefs` on `/dev/nvme1n1..4` meta +
`/dev/nvme5n1..8` data — the f45 daemon) for the whole time box, and R-1's daemon held
`/mnt/sqz-r1` on the loop devsub. Per the campaign's own rule (never disturb a foreign mount;
the format owns the substrate's volumes) the local A-B-B-A was **skipped and is owed**:

* the 1 MiB seq-write (rewrite) row and the rand-4k write row, A-B-B-A
  `SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR=1` vs `0` on the tcp devsub, device bytes ÷ user bytes +
  `wareq-sz` columns, sustained 60 s — the adjudicating row of §3.2 rank 5 ("tcp devsub rewrite
  rows ≥ par (was 0.68–0.80×)"). Rig: `.benchmarks/rigs/2026-09-02-w3-field-abba.sh` is the
  on-box shape; the local form is the same five legs against `tests/dev_substrate.sh`'s tcp
  volumes.

## Field row (squeeze-test, `w_rewrite`) — MEASURED, baseline-30 s (NOT a sustained claim)

**Venue:** `memp-s3ds-aqs-37` (32 CPUs, 2×200 GbE nvme-tcp, 5 meta + 10 × 48 GiB data
namespaces, cache-less format — the memory-backed fabric the finding calls "field-neutral").
**Binary:** `dist/rocky8/squeezefs` at **`b8dbce80`** (the fix commit; the two later commits
are test/docs only) — ONE binary, the knob is the arm. **Instrument:** fio 3.36 libaio
O_DIRECT, the field job `/scratch/tmp/fio_jobs/write_BW.job` (24 jobs × 8 GiB, bs 1 MiB,
qd16, `time_based` 30 s + 10 s ramp) re-run over the files a fresh pre-pass minted — i.e. the
audit's `w_rewrite` row, kernel mode; fresh mount per leg; `.stats` snapshot deltas +
`/proc/diskstats` on the 10 data namespaces per leg. **Tier:** measured-real. Order
A-B-B-A = ON → OFF → OFF → ON (`SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR=1/0`). Artifacts:
`.benchmarks/rows-w3-field-20260902/` (fio json, pre/post `.stats`, per-job bw logs);
rig `.benchmarks/rigs/2026-09-02-w3-field-abba.sh`.

| Leg | Governor | GiB/s (30 s) | clat mean | p50 / p90 | **p99.9** | dev ÷ user | bw-log first→last third |
|---|---|---|---|---|---|---|---|
| fresh (label-only pre-pass) | ON | 31.90 | 11.61 ms | — | — | 1.215 | 0.970 |
| **A1** | **ON** | **31.30** | 11.87 ms | 8.45 / 25.0 ms | **149.9 ms** | 1.335 | 1.054 |
| B1 | OFF (pre-W-3 open-loop) | 32.00 | 11.55 ms | 8.22 / 23.7 ms | 181.4 ms | 1.334 | 1.069 |
| B2 | OFF | 32.40 | 11.47 ms | 7.70 / 24.3 ms | 223.3 ms | 1.331 | 1.094 |
| **A2** | **ON** | **32.14** | 11.56 ms | 8.36 / 24.0 ms | **149.9 ms** | 1.325 | 1.056 |

**Reading (both brackets):** throughput ON/OFF = **0.978×** (A1/B1) and **0.992×** (A2/B2)
— and the four legs drift monotonically upward with position (31.30 → 32.00 → 32.40 → 32.14,
the aging-store warm-up the A-B-B-A exists to expose; every leg's bw log still climbs
first→last third, so no leg is converged), which puts the drift-corrected delta at ≈ −1 %:
**par within noise on the field venue, not a win and not a loss** — exactly the "field-neutral"
prior, and the rank-5 acceptance condition ("squeeze-test must stay ≥ par") holds to within
the row's own resolution. The **tail is the measured gain**: p99.9 **149.9 ms on BOTH ON legs
vs 181.4 / 223.3 ms OFF (−17 % / −33 %)** — the governor's admission park replaces an
unbounded device queue with an honest writer-side wait, which is the queue-shaping the
depth campaign bought the accumulation path in 2026-07. dev ÷ user is identical across arms
(1.33 — the overlay's whole-block CoW dest + the row's own re-pass structure; no
amplification term moved). `write_pipeline_admission_tick_wakes` runs 5.9–11.2 k on EVERY leg
(OFF legs highest) — the PERF-13 tripwire was already moving on this venue before W-3 and is
not this change's; noted for the write board.

**Engagement (row-validity, exact):** `overlay_governed_stores ≡ overlay_stores ≡
overlay_ack_early_stores` on both ON legs (**1,266,046** and **1,290,697**; ≡ 0 governed on the
OFF legs); `overlay_overwrite_bytes ≡ overlay_store_bytes` (the B4 overwrite arm carried the
whole row: 1.33–1.37 TB per leg). `write_pipeline_admission_waits` **+66,923 / +56,922 on the
ON legs vs +526 / +775 OFF** — the governor parked ≈ 5 % of the overlay's stores (fio offers
24 × 16 × 1 MiB = 384 MiB; the ten lanes' governed target sat at the 10 × 8-block floor
= 320 MiB × the probe multiplier — at this venue's ~0.3 ms service floor the raw BDP term is
below the per-lane floor, so the floor + probe govern; `depth_probe_ups` 4–8 per leg both
arms). `invariant_tripwires`, `write_pipeline_fence_drops`, `overlay_fence_drops`,
`overlay_ack_early_retries` all 0 on every leg; `inflight_bytes`/`inflight_blocks` read 0 at
every post-snapshot (permits settle exactly). **Owed on this venue:** the sustained-60 s form
of the ON/OFF pair (the standing rule; these are baseline-30 s rows) and the `rw_4k` row
(W1-patch dominated — `rw_4k` never reaches the overlay above the f47 floor, so it is a
no-regression check, not an adjudication).

## Open question carried (audit §7 item 7)

Whether the governor's saturation signal for the overlay should be the write-side `dev_queue`
(post-A1) or the overlay's own residence is unchanged by this landing: the overlay now samples
admission→CQE as its service time, which on the retained-slot arm includes the ring store's
submit queue. If the tcp devsub row shows the lane's `lat_floor` inflated by that queue (BDP
chasing its own tail — the runaway guard's case), the sample should move to the device funnel's
`dev_service` once A1's write-side split lands.
