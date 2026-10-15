# Rung 10 — a ring FORGET's corpse destroy held the FUSE worker (the kamrui create wedge)

**The report.** On kamrui, under 8 mdtest ranks whose MPI TCP pairing failed, one create stayed
parked at `handler_entry` for over 75 s and the umount then hung. Nothing else was overdue. Nothing
from that run survived: no `.stats`, no stacks, no daemon log.

**Repro attempts (kamrui, 7.2.6-sqz, dev `83f43dc`, ReleaseSafe): none wedged.** Over about 2 h:
- 20 rounds of 8 creators killed with SIGKILL at random points, followed by a survivor create.
- 12 rounds of `mpirun -np 8 mdtest -n 2000` with the ranks killed mid-run.
- 8 full mdtest runs. kamrui prints the same `Unable to find reachable pairing` warning on every run,
  so that warning is normal there and not the trigger.
- 15 × 40 s of 12 processes creating and unlinking files in one directory (335 corpse drains per
  15 s).
- One cold-cache variant: fill a directory, remount, then churn it. It also stayed green
  (25 k files, remount, 12 × 30 s churn: 431,866 evictions, 6,882 drains, 0 overdue).

The watchdog never reported an overdue op, and every umount completed.

**What reading the code found.** The FUSE drain worker runs every ring FORGET inline. In
`worker.zig`'s forget arm, `fs.forget` calls `Dispatcher.forget`, which calls `kv_fs.forget` and
then `gatherCorpse`. When the 64th corpse fills the gather, `reclaimDrain` and `destroyCorpses` run
**on the worker thread**. That thread also drives the worker's handler lane (every non-truncating
CREATE, LOOKUP, UNLINK and MKDIR on that queue, 0003 §AB.3) and its ring. The destroy makes raw
thread parks: a single-flight node read, the layout lock (3.7), and the commit. Any of them parks
every fiber of the lane, and every request of the queue, where it stands.

This matches what was reported:
- A create already claimed at `handler_entry` (the station before `meta_backend_start`) stays there.
- The worker thread is no op, so it is never registered and nothing else shows as overdue.
- The umount waits on the worker.

A permanent wedge needs a cycle back through that worker, and none was observed here. One such
cycle: a SingleFlight leader fiber on the lane is woken through the lane while the worker waits as a
loser of the same flight. `onWorkerLane`'s own doc already excludes the forgets from the worker
("an eviction reclaims"), but the ring's forget arm bypassed that rule.

**Red** (`kv_fs`: "a FORGET that fills the corpse gather destroys nothing on the forgetting
thread"): after 64 evicting forgets, `expected 128, found 129` commits. The destroy ran in the
forget.

**Fix.**
- A full gather notifies `kv_fs.reclaim_wake`.
- The reclaim cadence parks on that wake with its 5 s tick as the deadline (register-then-check, as
  `TimesDrainCadence` does). A wake drains only full batches (`reclaimDrainFull`); the tick
  drains the tail and runs the rest of the cadence.
- Its stop notifies the wake.
- The forgetting thread destroys nothing. The one exception is the gather-cannot-grow fallback,
  which is unchanged.

**Instrumentation.** Each worker stamps its loop pass, and its post-session lane drain, in
`transport/pass_stamps`. The wedge census now logs `stalest_worker_pass_ms` and
`stalest_worker_group`. An overdue op's station shows where the victim stopped; the stalest stamp
shows which worker thread is holding its lane, and for how long. An idle worker passes at least once
per ring-park backstop.

**Loops (fix build).** The minimal shape, 50 runs (each: format, mount, three rounds of 8 creators
killed with SIGKILL, a survivor create, and an orderly umount): **50 ok, 0 bad**. Every umount
completed. Cold churn (25 k files, remount, 12 × 30 s): 585,781 evictions, 585,754 destroyed (the
remaining 27 drained at dismount), 0 gather overflows, 0 overdue.

That build drained the gather to empty on every wake: 22 corpses per destroy entry, against
62.7 on dev. A permit arriving mid-drain woke it again for a few corpses. The landed fix drains only
full batches on a wake, which restores one entry per 64. The red test pins this: 64 + 1 corpses give
one entry and leave the tail for the tick. The loops above were not re-run on that last change.

**Gate.** `GATE_RESULT`
