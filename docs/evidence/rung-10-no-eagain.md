# Rung 10 — no metadata operation answers EAGAIN under load (2026-10-24)

**The report.** The window gate (0003 §BN, `rung-10-conveyor-window-gate.md`) left three items open.
(1) A barrier request took a cell from the lane's request half. Past 64 on a floor-ring lane, `requestBarrier`
refused `LaneQueueFull`, and the backend mapped that to `ConveyorFull`: EAGAIN for fsync(2). (2) A committer queues
before ring admission, so `queueCapacity(pages)` bounds nothing upstream. The 129th simultaneous committer got
`ConveyorFull`: EAGAIN for a create, an mkdir or an unlink. (3) The lever pair
`SQUEEZEFS_META_PASS_VENUE=committer` + `SQUEEZEFS_META_APPLY=self` can wedge (§BN.6). POSIX gives none of those
calls EAGAIN, and applications do not retry it.

**The laws (0003 §BO).**

- **BO.1:** An fsync is a waiter on the lane's barrier sequence (`BarrierWaiter`, a node on its caller's frame, pushed
  on an unbounded Treiber inbox). The scan takes the inbox whole before it drains the inbound windows and admits it
  after them, so the cover (§BB.4) names the submitter's earlier window. Every fsync admitted between two barrier
  starts shares one barrier. Nothing bounded is taken, so nothing refuses.
- **BO.2:** A committer that finds the queue full parks for room (`enqueueParking`: count, register on `room`,
  re-try, park). The pass notifies after a drain. The wait is the pass's progress, which already bounds every queued
  committer.
- **BO.3:** The startup gate refuses the lever pair (`knobs.exclusions`, naming both settings and why).

| red test | origin/dev d46dd83 | branch |
|---|---|---|
| journal_lane "THE FSYNC STORM" (192 fsyncs from 192 threads behind an un-landed window, 256-cell lane) | fsync 128 `LaneQueueFull`; 64 refused | 192 Ok; the lane held all 192 at once; ONE coalesced barrier answered them |
| conveyor "THE COMMITTER BURST" (pass parked at the window gate, 128 queue cells taken, 16 more committers) | committer 130 `ConveyorFull`; 16 refused | 16 room parks; all 146 acked and present |
| knobs "gate: … committer … self" | no report | refused, naming both |
| smoke "300 CONCURRENT FSYNCS AND 300 CONCURRENT CREATES ON ONE MOUNT NEVER ANSWER EAGAIN" (B joined, floor ring, dm-delay 5 ms) | dev `517121d`: 236 fsync(dir) EAGAIN (300 − 64) and 236 fsync(file) **EIO** (`MetaBarrierFailed`: the data leg's meta barrier met the same refusal) | 0 failed; B's lane held 300 fsyncs at once |
| models `RoomPark` (55 states, depth 16), `FsyncInbox` (172 states, depth 14) | — | hold. Weakenings fail: count-after-re-try, re-try-before-register and read-before-drain strand the committer; a load-then-store push loses an fsync; a scan that drains windows before it takes the inbox admits an fsync uncovered |

**Numbers (local venue, kernel 7.3.0-1.7-mainline-sqz).**

- The three red tests and the two models, rebased on dev `517121d`: 100/100 sequential, then 200/200 4-wide (four
  processes at once).
- Unit filters `journal_lane`, `conveyor`, `backend`, `knobs`: 183/186 (3 skipped: the kill matrix's child body and
  two smoke rows of the smoke step). `zig build rails` 32/32; `zig fmt --check` clean.
- Smoke, one run: the new row, THE TWO-DAEMON MATRIX, THE REPLAY PROOF, kill -9 mid-tar -x (coalesced and strict),
  UNDER THE WRITE-BACK TIMER fsync/fdatasync/syncfs, THE UNLINK STORM'S INODE LAW, the POSIX pass: 47/47.
- Smoke row: at 5, 15 (×2) and 25 ms, every op answered 0 and B's `journal_lane_fsync_waiters_hwm` was 300 (301 once). At 25 ms,
  B's floor ring also parked its pass at ring admission and 94 committers parked for room (`meta_conveyor_room_parks`).
  But a checkpoint cycle held a lock past the 2 s tick (`lock_ticked_reregisters` 1), so the row runs at 5 ms and
  reports the committer crossing without asserting it. The unit row pins that crossing.
- The fsync hot path changed: before, an mpmc pop, an inbound push and a cell give-back; now one CAS push and the
  scan's swap. The A/B of the fsync rows (`scripts/perf/matrix.sh`) is **owed**: the host was never quiet (load
  5–35, an fstests run and other agents' gates and smokes).

**Seen, not fixed.**

- On dev, a file fsync whose metadata barrier was refused returned **EIO** (`MetaBarrierFailed`, "the touched bits
  stay set"), not EAGAIN. BO.1 removes the refusal. The data leg's mapping of a retryable meta-barrier refusal to EIO
  stays as it is.
- The data plane's device lanes still map a full lane to `QueueFull` (EAGAIN). Whether a write or an fsync can surface
  that was not checked (out of this change's scope).
- `smoWriteHook` and `bind.zig` retry a full structural pool with `std.Thread.yield()`. A lane-thread caller would spin
  there. Barriers no longer share that pool, so it now drains only structural entries.
