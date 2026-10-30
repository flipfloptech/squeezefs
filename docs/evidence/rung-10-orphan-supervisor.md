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
  supervisor's — removed by that supervisor (§CV.13). A ring handed over the claim supersedes a copy
  beside it; a holder that closes without a frame is gone, one silent past the wait is busy (six
  rounds). A cache-less mount refuses over a copy; `format` removes the copies of the set it
  overwrites.
- *A deleted set*: `O_PATH` witnesses of the writer guards' devices; every image with no name left
  ends every hold (ring, held tails, kept tails) at the next tick, said once.
- *SIGCHLD*: the intermediate restores its default before the fork; the reap loops also end on the
  pid's death.
- *The harness*: `Rig`/`Peer.deinit` kill every `sqz-supervise` whose argv names their mountpoint
  (off `/proc`); every rig holds its custody under `<work>/held-custody` (§CV.12).

**Red** (`6415dc1` + the rows, `wt-orphan-red`): THE ORPHANED SUPERVISOR (a) `umount -l`, (b)
`fusermount3 -uz`, (c) left attached, (d) the escalation of a SIGSTOPped daemon, (g) the set
deleted, (h) a held tail against a deleted image — each `SupervisorOutlivedItsMount` with one
write-cache memfd left (h: one `.jtail` too); (f) SIGCHLD ignored — the death line never written
(the reap loop spun: no death replay, the guard and the ring held). (e) the fusectl abort under a
live daemon passes (its teardown drains; no orphan path). FORMAT REMOVES THE HELD WRITE CACHE: the
copy `LEFT`. The bash repro (`~/tmp/orphan-sup/repro.sh`) on `dea12cf`: the same per path, the
supervisor alive at +40 s holding the memfd.

**Numbers (the branch; session 1 on `6e9fbdf`, session 2 on `0262df2` = `90094f4` + the branch).**

| check | result |
|---|---|
| the eight ORPHANED SUPERVISOR rows + the two FORMAT REMOVES rows | ×10 on `6e9fbdf`, 0 failures (load 5–84); with HANDOVER IS ACKED, ×5 on `0262df2`, 55/55 (load 8–61) |
| per run | every supervisor exited, 0 write-cache memfds left; (a)–(d), (f): one copy, adopted, 8 of 8 files byte-exact, `staged_payload_lost_reads` 0, the copy gone, fsck clean; (e): no copy, 8 of 8; (g), (h): discarded; the handover: the first supervisor holds on past the killed successor, the next mount 8 of 8, lost 0 |
| the three evicted-member rows (§CV.12) | ×5 on `0262df2`, 15/15 (load 19–80) |
| the supervised kill and custody rows (21 filters, 67 tests: the kill matrix, the promotion kill, custody, the held tail, the replay proof, mid-tar, the killed manager's join, `--log-file`, races, two mounts, `umount --lazy`, the failed volume, the kill rows, the write cache's rows, …) | 64/64 on `6e9fbdf`; ×2 on `0262df2` 134/134; ×1 on `bc44e68` 66/67 — THE KILL ROWS (a), below |
| unit filters `supervisor staging journal_tail held umount daemon ENOTDIR frame` | 360 pass, 102 skip, 0 fail |
| nvmet fidelity `roundtrip-loop`; FALLOCATE — EVERY MODE'S LAW (solo) | 3/3; 3/3 (dev fails the latter 2 of 3: not this branch's) |
| `zig fmt --check`, `zig build rails`; xfstests `generic/001 013` (session 1) | green; passed 2/2, fsck 0 findings |

**Follow-ups (session 2).** *§CV.12*: the gate's stale cost table hid three rows red on this branch
(UN-LANDED FAIL-STOPS, NOTHING UN-LANDED RE-JOINS IN PLACE, EVICTED AT T_OWNER): they make a marker
FILE `<work>/held`, the rig-wide held dir was `<work>/held` — the open answered ENOTDIR, `consumeHeld`
`failed = StatFailed`, the volume refused. ENOTDIR now names no copy (red units on `25ff6df`:
`consumeHeld` not `none`, the bind `HeldCopyUnreadable`); the rig dir is `<work>/held-custody`.
*§CV.13 the handover ACK* (closes §CV.11 (a)): the holder keeps the memfd and the claim after the
send until the successor's supervisor acks holding the ring on the claim connection; that
supervisor then binds the claim. Found with it: the intermediate runs its supervisor loop only
after `ready`, so a daemon dying before `ready` took every fd it had announced — and it had already
removed an adopted copy; the supervisor holding the ring removes it now. Red: the unit (the claim
free after a successor died pre-ack) and THE WRITE CACHE'S HANDOVER IS ACKED (the successor held by
`SQUEEZEFS_TEST_WRITE_CACHE_ANNOUNCE_HOLD`, killed: the first supervisor EXITED, 0 memfds left).

**Seen, not this branch's.** THE KILL ROWS (a) (foreground A, foreground joined B: no supervisor,
no write cache — no path this branch changed) failed 3 of 29 runs here — two under heavy load (a
costs run's 16 shards; the venue at ~110), one at load ~6 beside a dev-tree run of the row that
passed: a file `tar` acked (`Cannot open`/`Cannot write` never said) read back empty on A after B's
kill -9. On `0a93a6a` 0 of 35 (10 of them side by side with 10 green here). Reported, not chased.

**Seen, not fixed** (§CV.11). (b) A copy whose set is deleted after it was written stays until
reboot. (c) A held tail against a block device that never returns is retried for good (§BR.4).
(d) A death replay into a hung device parks the supervisor in D state. (e) `/run/squeezefs` carries
stale `sqz-il0-*.sock` names on this venue.
