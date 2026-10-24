# Rung 10 — the data plane answers, never refuses or holds (2026-10-24)

**The report.** §BO (`rung-10-no-eagain.md`) left the data plane's EAGAIN unchecked: a device lane refused past its
4,096 cells (`QueueFull`), and so did the read path's bounce acquire — EAGAIN to a regular file's write, read or
fsync. generic/347 (`rung-10-parked-gate-wall-supply.md`): a write parked behind a thin pool refusing every DMA sat
until the kernel's 135 s request timeout aborted the connection. generic/475 on a FAILED volume: reads served zeros
for a staged file's acked bytes. And arm G could ack a placed write with no entry at a full table.

**The laws (0003 §BQ; charter §6.2 amended).** A full lane PARKS its caller on a free-cell door (futex word +
SeqCst count, the device's wait route); a closing lane answers `WorkerShutdown` (EIO); the data plane's narrow error
sets drop `QueueFull`. An exhausted bounce pool overflows to a backing served once (the pool's own law). THE STALL
ANSWER: a write the gate holds behind refused flushes answers ENOSPC (the store's `StorageFull` or the device's own
ENOSPC inside its wait) or EIO at the applied request timeout less one 15 s check period, and the episode answers
every later held write at its first refused flush — what aborted at 135 s is `fs/fuse/req_timeout.c`
`fuse_check_timeout`: this mount's own INIT `request_timeout` (`T_owner + T_park_max`, §Z), capped by
`fs.fuse.max_request_timeout` (0 here), floored at 15, judged every 15 s from `req->create_time`. A joiner answers
its upload's refusal (one device wait). A failed same-identity re-stage keeps the record the committed layout names
(`Segment.put` had retired the only other copy); a stable lost staged identity on a FAILED volume answers EIO; the
promotion path was already RAM-consistent (release after a successful commit; `rollbackHook` keeps an acked window's
RAM). Arm G reserves its entry slot before sealing a placed assembly for adoption.

| red test | origin/dev | branch |
|---|---|---|
| nvme_dev "a lane past its depth" (4,176 ops, a 4,096-cell lane held mid-pass) | 64 `QueueFull` | 0; 64 parked |
| read_path "BOTH bounce pools checked out" | `QueueFull` | byte-exact |
| accumulate "a joiner whose upload the device HOLDS and then REFUSES" | 2 device attempts | 1 |
| kv_fs "a same-identity re-stage whose commit FAILS" | ring empty under the staged layout | record kept |
| write_admit "arm G at a full entry table" | acked, no entry (bytes lost) | lands via the copy path |
| smoke "A WRITE HELD BEHIND A DEVICE THAT REFUSES EVERY DMA" (2 thin volumes, 64 MiB pool, error_if_no_space, timeout 30 s) | ENOTCONN at 30.7 s, daemon fail-stopped | ENOSPC at 16.1–16.2 s (bound 15 s), 2nd writer 3–16 ms, mount serves, fsck 0 |
| model `io/lane_door_model.Door` | — | holds; 3 weakenings fail |

**Reachability through FUSE.** smoke "1,100 CONCURRENT WRITES, FSYNCS AND O_DIRECT READS … NEVER ANSWER EAGAIN"
(dm-delay 5 ms) is the regression guard — green on origin/dev too: 1,024 handler slots (32 × 32) < 4,096 lane cells;
ranged slab 2,048. Not on every host: the geometry admits 512 × 32 slots, and the fill pool's in-flight headroom
(`max(cores × 16, 64)` = 512) is below 1,024 here. The unit rows pin both doors.

**Numbers (local, 7.3.0-1.7-mainline-sqz).** Red/new unit tests 40/40 (20 × 2 side by side); the two new smoke rows
20/20 (every round: 0 failed, door waits 0, exhaustions 0). Unit filters (write_admit, accumulate, enospc, lane,
read_path, kv_fs, staging, nvme, fsync, geometry, plane, warm_ladder, overlay, patch, pack): 755/803, 48 skipped;
placed/ipc.host 148/151. `rails` 32/32, `fmt --check` clean. Smoke one pass (full store, fallocate, write-back
timer ×2, staged identity window, FAILED volume, orderly unmount, fsck/scrub, §BO, the new two): 51/51. xfstests
`generic/347 299 750`: passed 3/3 on f00e3d1 and again on the rebased head, fsck 0 findings on both sets, 0
`TRIPWIRE` lines, no kernel abort (ledger rc 3: the subset's stale-row check); 347's fsync waited one pool queue (~60 s), no gate stall this run; the filled scratch: 1,155
terminal-wall ENOSPC answers (915 on a4d5d24), 0 stall answers. The lane test's 4,176 threads cost ~1 GB RSS for <1 s.

**Seen, not fixed.** The interception host answers EAGAIN to an LD_PRELOAD'd read/write when every lane refuses
its handoff (`ipc/host/service.zig`). fsync's parked leg answers ENOSPC for any `stay_parked`, a device EIO included.
A multi-piece write answered at the gate after earlier pieces landed returns the error, not a short count. A failed
truncate/punch commit leaves its clipped re-stage under the old size (reads past the cut serve zeros). A device that
holds a DMA forever holds the gate's synchronous flush past the deadline (the watchdog's). At a full entry table the
copy path spins at the gate until an entry returns. The scratch's 6,349 `transport_lease_overlong` lines (5,980 on
a4d5d24). `block_alloc.coreError` and `staging.resolveStagedRead` are read only by their tests.
