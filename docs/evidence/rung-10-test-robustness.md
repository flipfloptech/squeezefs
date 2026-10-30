# Rung 10 — rows that assert their law, not the host's pace (2026-10-29; decision 0003 §CO)

A row may judge only what holds under any host load and may count the kernel's caches only while it holds
what it counts. Branch `test/host-noise-robustness` off `74d419c`, `7.3.0-1.7-mainline-sqz`. Every proof
ran its noise inside one `SQZ_VENUE_SLOT=1 scripts/venue` command (`~/tmp/tn/loop.sh`, trap EXIT).

**1. The `.stats` herd row (§CC.4).** It bounded the slowest read by 2 s — a fresh render (§CC.6) plus a
`cat | grep | cut` pipeline, paced by the host: 3 of 10 full-smoke runs at load ≈ 56 failed. Now: 32 readers
each complete ≥ 8 whole documents beside 8 creators for ≥ 8 s, the probe never misses, no drain worker is
inside one pass for the probe's answer window, renders are shared, the watchdog is silent; reads are bash
builtins; walls and renders per read are printed only (the ratio is load-paced: a descheduled reader
revalidates against every render that ran while it slept — 2.0 idle, 5.0 at load 115). **20/20** beside the
full smoke binary's 16 shards and 16 busy loops (load 88–156): slowest read 3,967 ms (14 runs past the old
bound), lane hold ≤ 24 ms, 0 misses (those 20 also judged renders ≤ shared lookups and < readers / 4 a read; the shipped `shares > 0` ran 5/5 more at load 111–140). **Red on `bc569d4`** (+ the row and the gauge): 171 of 256 reads failed
when the supervisor SIGABRTed the daemon (1 probe miss), walls p50 9.8 s / max 35.7 s; the lane hold read
58 ms — the reads and the probe tell the shapes apart (the failing assertion is the shipped row's).

**2. Counting the kernel's caches (§CE.10's class).** An evicting FORGET empties the daemon's read view
and directory memo; `echo 2 > drop_caches` (the export-handles row) or reclaim evicts every unreferenced
inode. Each red below came from a drop placed in the window; each fix ran 20/20 beside a drop every 2 s.

| row | exposure | fix | red → fixed |
|---|---|---|---|
| THE READ VIEW | passes open and close the file: an eviction between them reads as a miss | `O_PATH` pin for the row | misses 1 → 0 (all 20) |
| a TWO-MEMBER metadata set | `depth_reads == 1`: evicting `r$d` before `mkdir r$d/s` re-reads it; no pin can cover that window | mkdirs a phase between SIGUSR1 dumps; `hits + reads == 16`, `1 ≤ reads ≤ 1 + evictions` | hits 12/15 → 7 reads ≤ 1 + 20; the eviction arm taken in 2 of 20 |
| THE TWO-DAEMON MATRIX | A's second read must take no grant: evicted, it is a daemon read under A's token entry, a grant if the budget shed it | `b1/f7` pinned to the count | a drop alone was absorbed (equal grants) |

Audited, not exposed: INTERCEPTION TTL (§CE.10's pins), FILE HANDLES (the dropper; asserts hold either
way), UNLINK STORM, the EchoFs e2e rows, ATIME (relatime judged on every read that reaches the daemon),
the access-stamp, busy-writer and kvmap-peer rows (O_DIRECT), VIRTUAL INODES WITHOUT AN OPEN.

**3. Wall bounds that were no product law,** each replaced by the mechanism it stood for (walls printed):
the bind row's exits (no `residency backstop lapsed`), busy/lazy `umount` (the host's fd census →
`transport_umount_busy_refusals`; a waiting verb never returns), the rename matrix and sideband create (the
move gate's `T_park_max` EIO), the stalled plane's second writer (no second wall), four notifier waits
(half the 60 s TTL), the atime and interception-death rows; units — `dutyPark` in served time, try-only
probes whose lock this thread holds, the ring park and lease recall by their verdicts, the pool's burst by
rendezvous, the lane's drop-wake by `tick_rescues`, the poison rows by cause or op deadline, the mint's
lower bound only. `direct_lease`'s one test never compiled into the unit binary: referenced, 20/20 beside the busy loops.
Units **20/20** beside 16 busy loops; the ten smoke rows **20/20** beside the full smoke's shards and 16 busy loops (load 55–185). Kept as product laws: tick-, `T_owner`-, quantum-,
recovery-, request-deadline-, `DIAL_TIMEOUT`- and exiting-holder-derived bounds.

**Seen, not fixed.** `expectSupervisorSilent` matches `missed a probe` but the supervisor logs `answers
again after a missed probe` (a recovered miss is invisible to its rows); the retention teardown's
`close < 2 s` and the seam-armed looped stage row's ≤ 1 s op latency; `enospc`'s freer racing the 1 s wall
floor; the nvme-dev close ceilings and two 30 s poll caps (anti-hang).

**4. A job's mid-run act meets a held job (§CO.26–CO.30; branch `test/defrag-job-hold` off `1df4bdd`).**
Gate 181 (a branch touching neither defrag nor jobs) went red in THE DEFRAG row: `awaitJobMidRun` met
`JobCompletedEarly` — `defrag --meta` at 1 % finished between `job resume` and the row's 50 ms poll. The jobs
matrix's drains (phase 3's `throttle 25`/`pause`, phase 5's kill) and the rekey row's B → C kill polled the
same way, and phase 3 had already tolerated "the job is terminal". New seam `SQUEEZEFS_TEST_JOB_TASK_HOLD`
(`off` | a job type): a job of that type parks in `fabric.taskDone` after each task — Running, the task
counted, where the duty park sits — until a pause, a cancel or the fabric's stop ends the step (the hold
stands again on the next run) or `job throttle … 100` releases it for good. Each row arms the one type it
acts on (`defrag_meta`, `evacuate_volume`, `rekey`), awaits the hold's log line, acts, and sets `off` before
the remount that adopts the job; the polls and the terminal tolerance are deleted.

**Red on `1df4bdd`**, each row's window given the host's worst pace (it waits ≤ 180 s for the job to complete
before it looks): 5/5 red — DEFRAG 2a `JobStateNotReached`, 2b `JobCompletedEarly` (gate 181's), matrix
phase 3 `job throttle 25` refused (terminal), phase 5 and rekey `expect(mid)`. The fix with the same windows:
5/5 green, each job still held after 180 s. Unit (fabric): retune keeps the hold, pause/resume re-holds,
unthrottle completes, cancel and the fabric's stop free a held worker, other types never hold.

| the three rows' binary | beside | runs | load |
|---|---|---|---|
| `bb8362d` (the code commit before its rebase) | the full smoke's 16 shards in passes + 16 busy loops | **23/23** | 54–185 |
| `bb8362d` | nothing of this work's (other worktrees' builds) | **20/20** | 4–29 |
| `b1e69b4`, `d8fb922` (the seam's first name) | shards + busy loops | 43/43 | 30–146 |
| `d8fb922` | nothing of this work's | 20/20 | 19–66 |

The first name, `SQUEEZEFS_TEST_JOB_HOLD`, strictly prefixed `…_HOLD_SUBMISSION`: the knob registry's
collision law refused it. On `bb8362d`: units (`ops.jobs`, `ops.defrag`, `ops.lifecycle`, `dutyPark`,
`core.knobs`) 149 + 3 skipped, fmt, rails, the full smoke 169/175 (6 skipped). Rebased onto `90094f4`: the
units 149/149, fmt, rails 29, the three rows ×3, the full smoke 170/176 (6 skipped).

**5. The no-EAGAIN row's storm meets the old bound on a held barrier (§CO.31–CO.33; branch
`test/bo-hwm-precondition` off `72f125f`).** A full gate on 2026-10-30 went red on the row's precondition, not
its law. B's `journal_lane_fsync_waiters_hwm` read 54 against `> 64` (the 64 request cells a barrier took before
§BO.1), with 0 EAGAIN and 0 failed. Idle runs read 73, 88, 122 or 300. The host's pace defeats the precondition
in two ways. If the 50 ms cadence barrier lands every create before the fsyncs arrive, each fsync's cover is
already durable and none is asked. If the fsyncs arrive spread out, each barrier answers one wave. New seam
`SQUEEZEFS_TEST_BARRIER_HOLD_FSYNCS=N`, armed by the mount's first FUSE CREATE before its commit: from then on no
journal lane starts a coalesced barrier until N fsyncs wait on one lane at once. The first lane to hold N spends
the seam for good. A stopping lane, a dead ring and the strict group barrier are never held. The creates stay
un-durable, so every fsyncdir is admitted needing that one barrier. B mounts with N = 65, the row awaits the arm
line before the fsync gate, and it asserts hwm ≥ 65.

**Red on `72f125f`** (a temporary row change, not committed): 1 s between the creates and the fsync gate gave hwm
1 and 1, and the shipped assertion failed. A stagger of 2·i ms on thread i's fsyncdir gave 5 (red) and 116. On the
fix, the same windows gave 300 and 300 (pause) and 257 and 67 (stagger), all green, with the seam spent at 65–66.
Unit (`journal_lane`): two fsyncs and a landed window's cadence barrier start no barrier, the third fsync
spends the seam and one barrier answers all three, and a stopping lane is never held. A seam that never holds
fails that test.

| the row's binary (`f0ed112`) | beside | runs | load | hwm |
|---|---|---|---|---|
| the row alone | the full smoke's 16 shards + 16 busy loops | **10/10** | 128–371 | 89–290 |
| the row alone | nothing of this work's (other worktrees' builds) | **20/20** | 22–183 | 104–300 |

Every run answered 0 to all 1,200 ops (0 EAGAIN). On `f0ed112`: units `journal_lane` 67/67 and `knobs` 57/57,
fmt, rails 29, the full smoke 183 + 6 skipped of 189.
