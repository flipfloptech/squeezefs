# Multi-volume bench-suite EIO — investigation + fix (urgent user regression)

**Branch:** `fix/multivolume-rand-write-eio` off dev@af10e9a.
**Commits:** `f5091ea` (red: never-lossy writeback dispositions) → `3636866`
(sticky map deleted + retry-forever ladder + capacity-bounded allocation) →
`aab8db1` (shutdown escape for the full-queue wait) → `52e84d4`
(published-but-unmerged block freed on merge failure + stale-token pin).

## The report and what it was

`sudo squeezefs bench /mnt/squeezefs/` (4 meta + 4 data volumes, allow-other,
uid/gid override, run as root, uncaged) passed write-seq/read-seq/read-rand
and died in the **write rand 4k** pass with `Error: Io(Os { code: 5 })`;
daemon alive.

**The user's durable state was never at risk.** Offline archaeology over
copies of their four meta volumes (`tests/mv_eio_archaeology.rs`, ignored,
`MV_EIO_META` env): 16 striped layouts, 8,192 block keys — bare/oss2/oss3/
oss4 split 2,055/2,024/2,050/2,063 (healthy round-robin, correct prefixed
persistence), **zero dupe-referenced offsets**.

## Repro + hypothesis eliminations

- **Prefixed-key hypothesis (the top suspect): CLEARED.** Multi-volume
  sandboxes (file-backed, loop devices, tmpfs loops; fresh AND
  remount-recovered; caged) ran the exact suite and 90-240 s rand-write
  hammers repeatedly: `stale_binding_rebinds` 0, all seeds materialized,
  frees routed per-volume (put/del deltas exact), leak probes flat across
  8 full overwrites and 6 rand-RMW passes, multi ≡ single volume.
- **Reproduced the failure CLASS** at user scale on bounded volumes: the
  suite died `Io(Os { code: 28 })` (my sandbox's leaf; EIO on real block
  devices) with **861→1069 sticky hard failures**: rand-4k RMW churn →
  upload failures → `WRITEBACK_MAX_ATTEMPTS=4` retries burned in ~0.75 s →
  sticky per-ino `WRITEBACK_HARD_FAILURES` → the pass's per-file `sync_all`
  surfaced the errno → the bench binary died. Same pass pattern, same
  surviving daemon.

## Taped defects and fixes (all red/pin-first)

1. **Sticky writeback poison (`src/fuse_client.rs` `requeue_or_hard_fail`,
   fsync at ~5884/5888): DELETED** (forward-only). The map turned transient
   upload/backpressure failures into app-visible fsync EIO for bytes sitting
   SAFE in staging; fsync's own error path even inserted into it, poisoning
   future fsyncs after conditions healed. The ladder now retries forever at
   capped (3.2 s) backoff — the only disposition consistent with never-lossy
   custody — counting wraps in `writeback_retry_exhaustions` (metric renamed
   from `writeback_hard_failures`); a FULL queue waits (with an `is_closed`
   escape to teardown — the first gate roll caught the bare `send().await`
   wedging daemon exit, storm test 5/5 after); fsync's honest surface is its
   own synchronous flush.
2. **Unbounded allocation (`src/block_allocator.rs`)**: `allocate_block`
   minted offsets past the device end — silent file growth on file-backed
   volumes, EIO/ENOSPC at DMA time on real devices. Allocators are now
   bounded at mount registration (`device_capacity_bytes`, seek-to-end) and
   refuse StorageFull at mint (CAS-clean; 0 = unbounded for tools/tests).
3. **Per-attempt block leak (`flush_one_active_block`)**: a failed merge
   propagated `?` after allocate+DMA+publish — leaking one device block per
   retry (unbounded under the new ladder for fenced units). Freed on error;
   pinned by `test_stale_token_writeback_adopts_current_epoch_no_leak`
   (release/reopen churn supersedes queued tokens; fsync drives the staged
   blocks durable; used == mapped).

## Red/pin tests

`writeback_requeue_on_full_queue_never_goes_sticky` (RED pre-fix) ·
`writeback_exhausted_retries_requeue_forever_not_sticky` (RED pre-fix) ·
`allocate_block_respects_device_capacity` (new API; red-by-absence) ·
`test_stale_token_writeback_adopts_current_epoch_no_leak` (pin) ·
`tests/mv_eio_archaeology.rs` (offline walker, ignored).

## Acceptance (final tip 52e84d4)

| Gate | Result |
|---|---|
| **User scenario** (4 meta + 4 data, allow-other, uid/gid, bench as root, suite ×3) | **rc=0 ×3** (physical returns to baseline after del — no leak) |
| Single-volume suite ×3 | rc=0 ×3 |
| Full serial cargo gate | **694 / 0** (first roll's storm-test failure = the send().await wedge, fixed `aab8db1`, count restarted) |
| clippy -D warnings / fmt / doc / bench smoke | clean / clean / 0 warnings / 122 ok |
| loom | 19/19 |
| Aged fsx ×3 | **3/3 CLEAN** (re-run on final tip) |
| QUICK ×3 | tip {003,069,074,213} ×3 — **base-binary A/B on the same box epoch: {003,069,074,213,617}** (tip ⊆ base). 069/074/008/617 wander identically across binaries (zeros signature, zero daemon errors, fresh-format volumes) after ~10 h of continuous fstests/repro soak on this box — environmental drift, ZERO fstests delta attributable to this diff; {003,213} remain the documented platform set. Release-gate results backed up at `~/tmp/mv_eio/release_gate_results_backup_1326`; a quiesced-box QUICK re-baseline is the standing follow-up |
| kill9 ×60 / unmount soak / LTP | ok / PASS 30/30 / 174-0-0 (re-run on final tip; first pass on aab8db1 identical) |

Cage note: 8G-caged daemons OOM-kill under the full auto-size (32 GiB)
suite's parked-buffer storm — sandbox rail vs the user's uncaged 109 GiB
box; journal-verified oom-kill, 0 panics. The write-rand crawl itself
(~50-130 ops/s, ~8 MiB I/O per 4 KiB op through the spill path) is the
long-standing row-5 shape, unchanged by this fix and out of its scope.
