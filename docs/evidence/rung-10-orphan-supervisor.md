# Rung 10 — the orphaned supervisor: a supervisor never outlives its mount (decision 0003 §CV)

**The defect (2026-10-29).** A `squeezefs mount --daemon --supervise` supervisor (pid 372304) had
sat 6 h 21 min in `poll(2)` after its daemon was killed, its mount detached and its set's
directory deleted by a smoke rig's teardown under a load loop. It held the daemon's log (deleted),
the tail socket, the write cache's claim and `/memfd:sqz-write-cache (deleted)`: §BR.7's hold had
no deadline. The rig's escape: a row that mounts through `rig.squeezefs(mount --daemon --supervise
…)` never noted its supervisor, and a load loop's `pkill -x squeezefs` cannot match `sqz-supervise`.

**What changed.**
- *The successor grace* (`supervisor.zig`): a dead daemon's ring waits on its claim for
  `SUCCESSOR_GRACE_PROBES` = 6 probe intervals (30 s) from the death, attached or detached alike,
  then moves to its *held copy* and the supervisor exits. Nothing restarts the daemon.
- *The held copy* (`staging.Ring.Ram`): `<held dir>/<claim name>.wcache` under
  `/run/squeezefs/write-cache` — the memfd's data runs at their offsets, sparse, renamed in.
  `bind`, holding the free claim, adopts it; the copy goes only once the new ring is the
  supervisor's (`DataPlane.writeCacheHandedOver`). A ring handed over the claim supersedes a copy
  beside it; a holder that closes without a frame is gone, one silent past the wait is busy (six
  rounds). A cache-less mount refuses over a copy; `format` removes the copies of the set it
  overwrites.
- *A deleted set*: `O_PATH` witnesses of the writer guards' devices; every image with no name left
  ends every hold (ring, held tails, kept tails) at the next tick, said once.
- *SIGCHLD*: the intermediate restores its default before the fork; the reap loops also end on the
  pid's death.
- *The harness*: `Rig`/`Peer.deinit` kill every `sqz-supervise` whose argv names their mountpoint
  (off `/proc`); every rig holds its custody under `<work>/held`.

**Red** (`6415dc1` + the rows, `wt-orphan-red`): THE ORPHANED SUPERVISOR (a) `umount -l`, (b)
`fusermount3 -uz`, (c) left attached, (d) the escalation of a SIGSTOPped daemon, (g) the set
deleted, (h) a held tail against a deleted image — each `SupervisorOutlivedItsMount` with one
write-cache memfd left (h: one `.jtail` too); (f) SIGCHLD ignored — the death line never written
(the reap loop spun: no death replay, the guard and the ring held). (e) the fusectl abort under a
live daemon passes (its teardown drains; no orphan path). FORMAT REMOVES THE HELD WRITE CACHE: the
copy `LEFT`. The bash repro (`~/tmp/orphan-sup/repro.sh`) on `dea12cf`: the same per path, the
supervisor alive at +40 s holding the memfd.

**Numbers (the branch).**

| check | result |
|---|---|
| the eight ORPHANED SUPERVISOR rows and the two FORMAT REMOVES rows, on `6e9fbdf` (the final base) | ×10, 0 failures (load 5–84); the eight on `6415dc1` ×11, 0 failures (load 3–73) |
| per run | every supervisor exited, 0 write-cache memfds left to the mount's processes; (a)–(d), (f): one copy, the next mount adopted it, 8 of 8 files byte-exact, `staged_payload_lost_reads` 0, the copy gone, fsck clean; (e): no copy, 8 of 8; (g), (h): discarded, no copy left |
| the repro, by hand | the copy 4.8 MiB of a 9.4 GiB ring; the remount recovered 8 records, 16 of 16 files byte-exact |
| the supervised kill and custody rows (the kill matrix, the promotion kill, custody, the held tail ×2 arms, the replay proof, mid-tar, the killed manager's join, no write cache unsupervised, `--log-file`, mount races umount, two mounts, `umount --lazy` ×2, the failed volume, the kill rows, the write cache's rows, own connection, the `.stats` herd, busy umount) | 64/64 on `6e9fbdf` (66/66 on `6415dc1`) |
| unit filters `supervisor`, `staging`, `journal_tail`, `held`, `umount`, `daemon` | 272 pass, 100 skip, 0 fail |
| `zig build fmt rails` | green (the literal rail: one stale row removed) |
| xfstests `generic/001 013` | passed all 2, fsck 0 findings both sets, 0 alert lines |

**Seen, not fixed** (§CV.11). (a) A successor whose bring-up fails between the claim's handover
and its own `announceWriteCache` destroys the ring (the old supervisor dropped its memfd at the
send; pre-existing — an ack after the new supervisor holds it would close it). (b) A copy whose set
is deleted after it was written stays until reboot. (c) A held tail against a block device that
never returns is retried for good (§BR.4). (d) A death replay into a hung device parks the
supervisor in D state. (e) `/run/squeezefs` carries stale `sqz-il0-*.sock` names on this venue.
