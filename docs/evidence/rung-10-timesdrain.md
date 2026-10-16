# Rung 10 — the times drain committed both members' refinements as one entry (timesdrain)

**Seen.** `rung-10-umountclaim.md` ("Seen, not this change"): every kamrui perf session logged the
tripwire `routing_split_refused` about 67 times and "times drain cadence: drain failed
(InputOutput)" about 1,100 times.

**Root cause.** `kv_fs.drainPendingTimes` cut the parked snapshot into windows of
`dlm.max_requests` inos without looking at their homes. On a multi-member set, the root's children
spread over both members. So a window held inos of both members, and it reached `meta_set.commit`
as one entry. The facade refuses such an entry, correctly: a transaction never spans members. It
fired the tripwire and the drain failed with EIO. The batch stayed parked and was retried on the
next tick, forever. A refinement is one inode record, so it never needs two members. Every fire in
the before session named the drain. The tripwire's detail now names the committer site
(`committer drainTimesBatch:<line>`), and that is how the venue confirmed it.

**Consequences.** On a multi-member set, the cadence and the dismount never landed a parked stamp
or size claim while both members had refinements parked. `syncfs` and `fsyncdir` answered EIO
(the smoke row's `sync -f .`). Only a per-file `fsync` landed anything (`drainTimesOf`, one ino).
In the perf session, fio's `--end_fsync` landed each file's times that way, so the remounted mtimes
matched (no loss observed there). An unfsynced file's mtime and its size claim stayed RAM-only until
an fsync. Another latent effect: a window that mixed another holder's ino with local ones dropped
the local refinements at that holder's `SlotMoved`.

**Fix.** The drain moves each home's inos to the front of what is left of the snapshot. It asks
the xv plane's `spansHomes`, the same question the destroy cut asks. Each home then drains in its
own `dlm.max_requests` windows. The partition is in place on the snapshot's stack buffer
(≤ `PENDING_TIMES_DRAIN_BATCH` = 128 inos), so it allocates nothing. The slot drain
(`drainPendingTimesOfSlot`, one slot = one home) and the fsync drain (one ino) were already safe.

**Why the gate missed it.** The only two-member smoke row formatted metadata only. With no data
volume, nothing writes striped, so no write stamp is ever parked. The row also asserted neither
`routing_split_refused` nor `invariant_tripwires`.

**Red → green.**
- Unit test, `set_bind` "the times drain commits each member's parked refinements on that member".
  40 files over both members, every stamp parked, then a drain. Red: the tripwire
  (`drainTimesBatch`) and EIO. Green: tripwire unchanged, exactly ⌈n₀/16⌉ + ⌈n₁/16⌉ entries,
  0 parked, and a reopened set reads every stamp.
- Smoke row "a TWO-MEMBER metadata set through the binary". It now formats two data volumes, does a
  32 KiB write plus a 512 B overwrite in all 8 directories, then `sync -f .`. It asserts "synced",
  `routing_split_refused`, `invariant_tripwires` and `meta_kv_times_echo_pending` all 0, and no
  "drain failed" in the log. Red: `sync: error syncing '.': Input/output error`. Green on kamrui.
- Green on kamrui with the `meta_set`, `times`, `routing`, `kv_fs` and `set_bind` filters.

**Perf session** (kamrui, `perf_mount.sh` native, dist build, 2 MDT (4 GiB) + 2 OST (32 GiB)
scratch LVs, `--fresh`, 4 × 2 GiB layout in the root, 64 KiB randwrite qd16 nj4 60 s
`--end_fsync=1`, umount + `fsck`, remount + stat, gate lock and every slot held):

| | tripwire log lines / fires | cadence drain failures | drain entries / refinements landed | fsck |
|---|---|---|---|---|
| before (the red commit) | 67 / 1018 (all `drainTimesBatch`) | 1018 | 4 / 4 (the fsyncs) | clean |
| after (the fix) | 0 / 0 | 0 | 2259 / 4506 | clean |

The before session also logged 64 `transport_lease_overlong` error lines. The after session logged
none. That is one session each, so it was not investigated.
