# W-3 — the device overlay rides the write-pipeline depth governor

**Date:** 2026-09-02 (campaign W-3 of the e2e perf audit, write board #3)
**Design:** `docs/design-e2e-perf-audit.md` §3.2 rank 5 / Appendix C #3; the governor
`src/write_pipeline.rs` (2026-07-27 depth campaign + 2026-07-29 probe-up + finding 39's Red
clamp); the overlay arms `docs/design-device-overlay.md` / `docs/design-overlay-overwrite.md`.
**Branch:** `perf/w3-overlay-depth-governor` from `98346dab` (A1 exact histograms + A2 trace
ring + f47's overlay length floor in). Commits: red `2a741aa2`, fix `b8dbce80`, test-fix + rig
`d1ddee31`.
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

## Field row (squeeze-test, `w_rewrite`)

FIELD_ROW_PLACEHOLDER

## Open question carried (audit §7 item 7)

Whether the governor's saturation signal for the overlay should be the write-side `dev_queue`
(post-A1) or the overlay's own residence is unchanged by this landing: the overlay now samples
admission→CQE as its service time, which on the retained-slot arm includes the ring store's
submit queue. If the tcp devsub row shows the lane's `lat_floor` inflated by that queue (BDP
chasing its own tail — the runaway guard's case), the sample should move to the device funnel's
`dev_service` once A1's write-side split lands.
