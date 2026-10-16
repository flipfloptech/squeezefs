# rung 10 — deadreadhang: the dead-read rig's killer starved the flush it waited for

**Symptom.** The unit test `exec.lane` "a dead ring keeps a READ the kernel took: its fiber stays
parked until the CQE …" (from `rung-10-ringdeadread.md`) hung one kamrui train gate on `bb94bf1`
for 63 minutes, until the gate's 3900 s timeout. Its shard process ran at ~199 % CPU.

**Verdict: TEST (b). No held op was involved.** The ring was never killed. The killer fiber
yield-loops until the reader's READ is armed *and* the SQ is empty (`sq_ready() == 0`). Only the
pass's loop-bottom flush in `parkOnce` empties the SQ. But a yielded fiber goes back to the tail of
the ready FIFO, and with `poll_budget` 0 `runReadyBudget` runs up to 2^32 polls before it reaches
`parkOnce`. So when the reader armed its READ in the same pass as the killer's first poll, the
killer re-ran itself for 2^32 polls and the SQE stayed unflushed. That happens when all three
spawns land in one inbox drain, which load makes likely. At the measured ~360 k polls/s
(Debug), 2^32 polls take ~3.3 h. The two spinning threads were the lane thread (running the
killer) and the test thread (`while (l.alive) yield`): 199 %. No product fiber yields (`yieldNow`
has only test callers), so the lane's unbounded pass is not a product hang.

**Stacks (kamrui, the dev test binary, 8 copies under 24 × `yes`, 600 s).** 16 runs hung past
30 s, and every copy hung at least twice in its first 9 runs. All 16 stacks matched:
`sqz-meta0` was in `Lane.runReadyBudget` (perf: `clock.monotonicNs` per poll,
`DeadRead.kill` → `yieldNow` → `switchToLane`), the main thread was in `do_sched_yield`, and none
was in the dead park (`parkDead`, `io_cqring_wait`, poll).

**Red (`2bc1a35`, deterministic).** A blocker fiber holds the lane thread on a futex until the
test has spawned the reader, writer and killer, so they land in one drain. Every wait is now
bounded by a 60 s hang guard (not a speed bound). When it expires, the guard stops the killer and
reports the state, so a broken contract fails instead of hanging. Pre-fix:
`the verdict not reached in 60 s — reader op_pending=true, sq_ready=1, killer polls=21624983,
lane alive=true, in_flight=1, held=0`.

**Fix (`ababe63`, test-only).** The killer flushes the SQ itself (`ring.flush()`, on the lane
thread) before it checks. Decision 0003 §AR is unchanged.

| run (kamrui) | result |
|---|---|
| red commit, the test | red at the 60 s guard (above) |
| dev binary, 8 copies × 600 s under 24 × `yes` | 16 hangs, all the runReady spin |
| fixed binary, 8 copies × 420 s under 24 × `yes` (every run takes the blocker-ordered path) | 9,480 of 9,480 green |
| fixed, filters `exec.`, `dead ring`, `ring_park`, `journal_lane`, `nvme_dev`, `uring_fs` (`-Dfresh`) | 7/7 steps, 220/220 tests passed |
