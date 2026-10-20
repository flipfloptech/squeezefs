# Rung 10 — the fence's class: a fence on a FAILED volume is counted, a fence on a HEALTHY volume is the tripwire

**The ruling (owner, 2026-10-19).** A fence event taken while the volume is FAILED (or a device is
fenced) is an ordinary counted outcome; the tripwire fires only for a fence taken on a HEALTHY
volume. Before this every W5 arm fired its must-stay-0 on every drop, so under dm-error (xfstests
generic/475, 753, 441, 484: the metadata device answers EIO, the volume latches FAILED by design, §1.8)
the tripwires fired as a CONSEQUENCE of the latch and `scripts/suites/fstests.sh` exempted "tripwire(s)
on a device that answered EIO (for the record)". The exception is gone: every tripwire fails the run,
dm-error daemons included. Decision 0002 row 548; `docs/porting/data_write_path.md` §stats and
`data_alloc_reclaim.md` §7 carry the port note.

**The mechanism.** `seams.FenceProbe` (the reclaimer's type) carries the plane's `fencePoll` — the meta
set's `is_failed` or any device's `fenced()` — into `write_admit.Deps.fence`, `device_overlay.Deps.fence`,
`accumulate.UploadDeps.fence` and `Epochs.CloseSeams.fence`; `seams.classifyFence(probe, counter,
tripwire, detail)` is the ONE rule: FAILED/fenced ⇒ the counter and `.failed_volume` (one info line at
the site), else the tripwire fires with its "… on a HEALTHY volume" detail and `.healthy` (the site's
loud line). The latch is sticky (FAILED never clears before the remount), so a second poll after the
fence verdict reads the same answer. The meta side consults `FailureLatch.isFailed()` directly.

**The inventory** (`grep -rn "\.fire(" src`; 233 sites). Fence-class — reclassified:

| site | before | now |
|---|---|---|
| `device_overlay` ×4: the acked store's continuation, the settle's transient on a FAILED volume (`1fbb62d`), a device leg's `VolumeFailed`/`WriterGuardFenced` (gap seed, flush), the publish's `WriterGuardFenced` | `overlay_fence_drops` on every drop | `noteFenceDrop`: FAILED ⇒ `overlay_fence_drops_failed_volume` + info; HEALTHY ⇒ the tripwire + the loud line |
| `write_pipeline.countDisposition(.fence_drop)` (an upload's device write or merge fenced) | `write_pipeline_fence_drops` | takes `(ino, block, probe)`: `write_pipeline_fence_drops_failed_volume` / the tripwire |
| `write_epoch.closeEpoch`'s `WriterGuardFenced` | `rewrite_shadow_fence_drops` | `rewrite_shadow_fence_drops_failed_volume` / the tripwire |
| `reclaim.processEntries` on `fenceHalted()` | `block_free_reclaim_fence_halts` (tripwire) | a `ShardedCounter` of the same name: the halt fires only when the probe reads FAILED/fenced, so no healthy arm exists; the once-only halt line names "FAILED or fenced" |
| `conveyor.divergedFromReplay` (rollout/rollback/compensation) | the site's tripwire + `failStop` | latch already FAILED ⇒ `meta_conveyor_divergences_failed_volume` + info; else unchanged (tripwire, FAILED at once) |
| `conveyor.commitCompensation`: the entry's device write fails (`SmoJournal.commitEntry`'s `error.Corrupt`) | `conveyor_divergence_compensation` | `meta_conveyor_compensation_write_failures`: FAILED at once with one loud line (RAM ≠ replay until the remount), never a tripwire — a device refusing a write is the fail-stop lattice's own event, not a "cannot happen"; an entry that cannot be BUILT stays the divergence |
| `conveyor.abandonWindow` with `error.VolumeFailed` (the latch landed between A1's screen and the lane's submit) | `meta_conveyor_pass_panics` | `meta_conveyor_windows_abandoned_failed_volume`; every other abandon stays the tripwire |

Judged and left must-stay-0 (not fence events): `overlay_unpublished_at_fsync` (a fence-dropped record is
torn down, so a drain's `left` is 0 on a FAILED volume too), `overlay_ack_early_lost` (a dead ring with no
bytes to retry), the overlay's lifecycle tripwires, `block_free_reclaim_dismount_halts` (dismount
ordering), `custody_generation_poisoned` (the product installs no custody hooks; only saturation poisons),
`kvmap_resolve_failures` (a tree READ failing — the design says an unresolvable binding is loud, never a
fabricated hole; not seen in four dm-error runs), `parked_gate_wall_supply_visible` (a stall, not a fence;
a FAILED volume's parked blocks stay parked by the never-lossy law), `final_tick_open_reservations`,
`appender_flush_ceiling_overruns`, the sym plane's `xv_midplan_escalations` / `dead_member_write_deferrals`
(multi-writer paths a FAILED member reaches through its manager's death ledger, out of this ruling's
single-writer scope), and every accounting invariant (double frees, drift, refcounts, wire rejects).

**Red → green.** `write_seams`: "the fence's class" — a probe reading FAILED counts and leaves the
tripwire alone; healthy or absent fires it. `device_overlay`: "the fence's class on every W5 arm" — a gap
seed's `VolumeFailed`, a flush's `WriterGuardFenced` and a publish's `WriterGuardFenced` with the
`TestWorld.fenced` probe set each count `overlay_fence_drops_failed_volume` (+3, no error log, W5 unchanged:
nothing published, the dest not freed), the tripwire unmoved; the probe cleared, the same gap seed fires it
(the existing fence-immediate test, now also asserting the counter untouched); the `1fbb62d` transient-on-
FAILED test asserts the counter instead of the tripwire. `write_pipeline`: the disposition contract gains the
split. `write_epoch`: the fence test's FAILED twin (`CloseSeams.fence` latched). `reclaim`: the fenced batch
is a count and ONE error line (the halt latch's). `conveyor`: a rollback OOM on an ALREADY FAILED latch
counts `divergences_failed_volume` with the tripwire untouched; a compensation entry whose SMO-journal
device answers `error.Io` latches FAILED at once, counts `compensation_write_failures`, no tripwire.
Targeted runs green: `device_overlay` 64/64, `conveyor`/`meta.smo` 113/113, the data-plane set 334/361
(27 smoke-shard skips), `data.plane`/`meta.backend`/`write_e2e` 79/80 (1 skip), `zig build rails` green.

**The hand check** (`~/tmp/fence-class/hand-check.sh`: the suites' one-device slice layout over a
dm-linear target, 8 striped 8 MiB files synced, then 16 writers in flight — 1 MiB overwrites and fresh
256 KiB–4 MiB files — while the table flips to `error`; four runs). Before the compensation arm's fix (run
1): `invariant_tripwires` 1 = `conveyor_divergence_compensation` ("the compensation entry's write failed")
on a volume no rung had latched yet — the divergence itself raised the latch; `overlay_fence_drops_failed_
volume` 13, `overlay_fence_drops` 0. After (runs 2–4): `invariant_tripwires` **0**, `invariant_tripwire_
sites {}`, no `TRIPWIRE` line in any daemon log; `overlay_fence_drops_failed_volume` 8 / 14 / 14,
`rewrite_shadow_fence_drops_failed_volume` 2 / 1 / 0, `meta_conveyor_compensation_write_failures` 0 / 2 / 0
(run 3: the first latched the volume with its one loud line, the second was the info line on the FAILED
volume), `meta_conveyor_rollbacks` 3–8, `volume_failed true`; `squeezefs umount` returned in 0.21 s and the
daemon exited (`exiting 1`, the FAILED verdict) within 2.5 s. `.stats` stayed readable throughout.

**Seen beside, not this change's.** A FAILED volume's in-place drains log one error line per EIO write
(≈ 115 per writer ino in 6 s: "in-place dirty drain … DMA failed (InputOutput)"); the harness counts them
as alerts, never a failure. `write_pipeline_fence_drops_failed_volume` and `block_free_reclaim_fence_
halts` read 0 in these runs: on the one-device layout the data writes answer plain EIO (`stay_parked`)
before any merge can meet the FAILED meta set, and the reclaimer had nothing queued.
