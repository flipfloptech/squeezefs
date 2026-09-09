# Finding: two delivered LOOKUPs never answered for 23 minutes under `generic/795` (2026-09-08)

**Status: OPEN — captured live, not attributed, not fixed.** Found by the
1.2.2 release gate's fstests leg on the candidate `49964d7a`; the runner
scored the test **clean** because the test's own teardown aborted the FUSE
connection. Load-dependent hangs are first-class product bugs (repo law);
this note is the capture and the state of the attribution so the campaign
that closes it starts from evidence, not memory.

Raw capture: `/var/tmp/squeezefs_forensics/2026-09-08-generic-795-wedge/`
(the daemon log, `dmesg -T`, gdb stacks of all 205 daemon threads taken
while the wedge was live, the test's `795.full`). Times below are local
(UTC−4); the daemon log stamps UTC.

## 1. Venue

| | |
|---|---|
| box | the dev laptop (`strixhalo`, 32 CPUs, kernel `7.2.3-cachyos-lto`, sqz 7.2 track incl. the per-queue bg patch) |
| binary | `49964d7a` (1.2.2 candidate), `profile release`, FUSE-over-io_uring armed, 32 queues × depth 32, zc off (the fstests runner's default) |
| test | `generic/795`: "Race dropping file systems caches vs fsstress and repeated reading of files" (generic/579 without fsverity, more runs) — `echo 2 > drop_caches` in a loop while 4 fsstress processes + a reader hammer the scratch mount |
| mount | `/mnt/squeezefs_scratch`, meta on `/dev/shm`, `--disk-cache-size 500MB`, the fstests runner's standard shape (`tests/run_fstests.sh`) |
| segment | the resumed fstests pass (`--resume-from generic/651`) after two box resets inside `generic/650` — the box had been up 40 min, cold, idle otherwise |

## 2. Timeline (from the tape)

| when | what |
|---|---|
| 13:32:40–43 | staging-cache-full burst (`NVMe write staging cache full: direct synchronous backend block write … 156–167 similar spills suppressed`), promotions, one `promotion failed … FencingTokenExpired — entry stays resident`; 31 `Io(Os 61 ENODATA)` operational-error lines (fsstress's negative xattr probes — noise, not the wedge) |
| **13:32:43** | two requests DELIVERED on **queue 16** — `unique=1316798` (ent 20) and `unique=1316802` (ent 0) |
| 13:32:53 | first `transport_slots_overdue` line for both (9,681 ms); repeats every 5 s until the end |
| 13:36:40 | kernel `INFO: task fsstress:276239 blocked for more than 122 seconds` — a writer waiting on the directory's `i_rwsem` "likely owned by task fsstress:276238", whose stack is `fuse_chan_send ← fuse_dentry_revalidate ← lookup_one_qstr_excl ← filename_create ← mkdirat`: a **LOOKUP sent to the daemon, never answered**, holding the parent directory's lock |
| 13:40:46 | second hung-task report; "Future hung task reports are suppressed" |
| 13:51–13:56 | live inspection: `/sys/fs/fuse/connections/77/waiting = 12`; all 205 daemon threads idle (32 queue workers in `io_uring_enter`, 31 `fuse3-tpc` handler lanes parked, 2 `sqz-meta` lanes parked, journal lane parked in its ring); gdb stacks captured; `.stats` reads from any CPU hang (the stats read walks tables the wedge holds — not a per-queue probe) |
| 13:56:32 | the test's teardown aborts the connection (`/dev/fuse revents=0x8`); the daemon dismounts **"with unflushed data! Remaining local staged files: 210"**; the two requests were never committed (`transport_slots_overdue` was still counting at 13:56:31, age 1,336 s) |
| 13:58 | 795 finishes its remaining runs on a fresh mount; the runner records `Ran: generic/795` with no diff → **"clean"**; the resume pass ends `147 ran, 146 clean, 1 expected-shape, 0 unexpected` |

## 3. What the capture says

1. **The transport's view.** `transport_slots_overdue` names the two slots as
   *delivered and still unreplied*: the owed-watch is set at delivery and
   cleared when the worker PUSHES the COMMIT SQE (`submit_commit_retain`),
   so no COMMIT for either ent was ever pushed. Both were `not-fused`
   (classic handler-lane dispatch), `zc_bridge_pends=0`, `scan_orphans_seen=0`,
   `park_backstop_ticks=1`.
2. **The op watchdog's silence.** The per-op D1.b watchdog (30 s threshold,
   5 s tick, registers at `OpProf::begin` inside every handler) printed
   NOTHING for these two in 23 minutes, while the transport watchdog
   printed 563 lines. So either (a) the LOOKUP handlers never started, or
   (b) they ran, replied within 30 s, and the reply message sat unpumped
   in queue 16's `commit_rx`. **(b) is the reading that survives §4**: an
   op that completes is invisible to the op watchdog by design, and every
   path by which (a) could hold is tick-backstopped.
3. **Every thread idle.** The gdb dump shows no thread in a poll, no lane
   mid-task, queue 16's worker (`f3-ur16`, LWP 276077) in
   `submit_and_wait(want=1)` at `fuse_over_uring.rs:6083` with `wake_fd=248`.
4. **Nothing else on the connection was served either** — my `.stats` reads
   pinned to nine different CPUs all hung; the connection's `waiting`
   climbed from the 2 stranded requests to 12 as probes accumulated. The
   `.stats` LOOKUP handler takes no lock and awaits nothing before
   `OpProf::begin` (`generate_stats_json` is atomic reads + formatting), so
   those probes were never DISPATCHED; and since the transport watchdog
   names every delivered-unreplied slot on every queue and named only
   queue 16's two, they were never DELIVERED to the daemon either. The
   stall was connection-wide: **the queue workers stopped consuming their
   rings' completions** — reply-commit wakes and fetch deliveries alike —
   for the life of the mount, while parked in the healthy cq-wait posture.

## 4. Attribution: what is ruled out, what remains

Ruled out by construction (and read against the code, not from memory):

* **Handler lane lost-wake** — a task once enqueued on a `sqz_exec` lane
  cannot strand: the lane re-checks its queue every 2 s TICK
  (`TICK_RESCUES`) and the state word is loom-modeled (`exec_core`).
* **Inbound-channel lost-wake** — `InboundQueue::pop` parks in
  `ticked(RecvFut)`: a lost channel wake costs one 2 s tick
  (`TICKED_WAIT_RECOVERIES`), and the `SqzMutex` acquire in front of it
  ticks as well.
* **Mid-pass wake-poll consumption before the park** (FUSE-2 row 7's
  mid-pass face) — the pre-park re-arm at `fuse_over_uring.rs:5944` pushes
  the PollAdd before `submit_and_wait`, and the eventfd is level-triggered.
* **Distinct coalescers for lease drops vs commits** — one `Arc` (checked).

Remaining branches, both lost-wake shapes in tick-backstopped machinery:

* **(a) never dispatched**: the queue-16 session task (one per queue,
  `inner_mount_worker` → `dispatch_with_max_write` → `read_vectored` →
  `recv_inbound(16)`) was not pollable — a task-state inconsistency, or a
  timer registry that stopped ticking for it (`fuse3::sqz_time`'s own
  registry; `transport_timer_{arms,heap_entries,live_sleeps}` would say —
  unread, the stats inode hung). Every prior wedge of this family was a
  worker-side park, never a session task; there is no precedent.
* **(b) replied, never pumped**: the handlers replied within 30 s
  (`submit_reply_inner`: `commit_tx.send` → `arm()` → eventfd write) and
  queue 16's worker parked unbounded without a covering PollAdd — the
  FUSE-2 row 7 class with an unnamed window. The worker's pass-top order
  (drain → disarm → pump) is loom-verified; the field capture of 2026-08-13
  proved the cross-thread chain "CAN lose exactly one wake under load"
  and answered it with the bounded park — which engages ONLY while a
  bridge pend or a fused resident lives. With none live, the worker parks
  unbounded and one lost wake is a wedge until the next request on
  **that queue's CPU** — which never came, because the requests that would
  come were the ones blocked behind the hung `mkdir`'s `i_rwsem`.

The counters that would decide (a) vs (b) — `transport_wake_{writes,elided}`,
`TICKED_WAIT_RECOVERIES`, `TICK_RESCUES`, the timer faces, `fuse3` replies
vs commits — were on the stats inode, which hung. **Lesson for the next
capture: read `/proc/<pid>/fdinfo/<wake_fd>` (`eventfd-count`) the moment
a `transport_slots_overdue` line appears — a nonzero count with the
worker parked is branch (b) proven in one read; and read the counters
through a path that does not touch the FUSE mount.**

Since 1.2.1 (`stable-2026.09.1`) the only change in the queue worker /
session / connection is R-4's fd-source zc READ serve (`2f0fc361`,
default off, not on this mount). `generic/795` passed the 1.2.1 gate in
233 s — one green run is not evidence of absence for a race the drop_caches
storm selects.

## 4a. Reproduction attempts (2026-09-08, this laptop, the candidate binary)

| rig | shape | runs | wedges |
|---|---|---|---|
| `generic/795` alone (runner single-test mode, capture armed) | the test verbatim | 12 | 0 |
| the fstests slice `--resume-from generic/781` (the pass's own order to the end) | 795 in its pass context | 6 | 0 |
| fresh-mount storm cycler (`/tmp/release-1.2.2/wedge795/storm.sh`): the runner's mount recipe, 795's storm (4 × rm+cp 10 MB, 6 × cmp, drop_caches loop, 3 × fsstress), 40 s per FRESH mount | the onset window (15 s into a fresh mount, staging full at t+1 s) 30× | 30 | 0 |

48 storms, one wedge in the day's 50 exposures — the two release-gate
passes. Every run had the live capture armed (`capture.sh`: eventfd
counts + io_uring fdinfo PollList/heads + per-thread kernel stacks + gdb;
`probe.sh`: CPU-pinned LOOKUPs on other queues + a 10 s bpftrace census
of `fuse_uring_queue_fuse_req` / `add_req_to_ring_ent` / `send_in_task` /
`io_uring:local_work_run` / `cqring_wait`). The rigs stay under
`/tmp/release-1.2.2/wedge795/` for the next exposure; the census script
is `kern.bt`.

## 5. Consequences

* **The runner has a blind spot**: a test whose FUSE mount wedges and is
  rescued by the harness's own abort scores clean. `tests/run_fstests.sh`
  should refuse a test whose mount aborted (`revents=0x8` in the daemon
  log during the test) or whose wall exceeded a derived bound (the recorded
  time × k), the way the acceptance rigs treat a stalled row as INVALID.
* **The fix (landed with this note's follow-up): the bounded-outcome law
  for EVERY owed reply.** The 2026-08-07 zc campaign proved the worker's
  wake chain "CAN lose exactly one wake under load" and answered it by
  owning the clock — a 100 ms EXT_ARG-bounded park — but only while a zc
  bridge pend or a fused resident lives. An ordinary in-flight request had
  no bound, so one lost wake with the worker in cq-wait was a wedge until
  the next request on that queue's CPU — which the hung `mkdir`'s
  `i_rwsem` guaranteed would never come. Now the park is bounded while the
  worker's group owes ANY reply (`GroupHandle.owed`), a tick that finds
  work a wake should have delivered is counted
  (`transport_park_tick_{commit,cqe}_rescues` — ≈ 0 on a healthy mount,
  nonzero IS the lost-wake tripwire) and the first rescue per worker logs
  the attribution snapshot (coalescer armed?, `eventfd-count`, the ring's
  `Sq/CqHead/Tail` + PollList entries from `/proc/self/fdinfo`); the
  overdue-slot WARN carries the same snapshot at the 5 s mark. The
  deterministic red-first repro is the seam
  `SQUEEZEFS_TEST_DROP_COMMIT_WAKES` (the reply path skips the eventfd
  write after arming — the exact observed posture: armed coalescer,
  stranded commit) in `tests/commit_wake_loss_tests.rs`. What this does
  NOT do is name the wake's loser (kernel task-work wake vs daemon); the
  snapshot line names it at the next exposure.
* **Open beside it (P1):** four of the pass's 407 dismounts reported
  200+ unflushed staged files and 47 reported 1–2 ("dismounted with
  unflushed data") — orphans of deleted files or acked bytes lost at
  umount is unanswered; the runner now prints it as a NOTE (≥ 100), never
  a verdict.
* **Release posture**: the user's call — named in `RELEASE_NOTES.md` 1.2.2
  §Known limitations if 1.2.2 ships with it open.
