# rung 10 — uringfsdead: a dead uring-fs ring handed back READ buffers the kernel still held

**Defect.** When a worker's `io_uring_enter` or CQ read failed, `uring_fs.Reactor.failAll`
released every slot. It freed the READ's buffer, dropped the file reference and answered the
request cell with RingFailed. An enter failure cancels nothing the kernel already took (§AR.4).
So a READ of a pipe, a FIFO, or a regular file whose page was not cached still copied into
the freed buffer when it completed, through task work on the worker or io-wq. By then the
heap had handed that memory to someone else. `ringdeadread` closed this for the exec and NVMe
lanes. This file belonged to another change at the time (§AR.7).

**Fix (decision 0003 §AR.8).** The pool now follows the NVMe lanes' contract:
- `failAll` answers at once only the SQEs the kernel never took. It uses
  `ring_park.untakenIn`, which is now the body `Parked.untaken` calls, so there is one
  implementation. Every taken slot (READ, WRITE or FSYNC) keeps its buffer, its file and its
  cell.
- On a dead ring, `reap` enters through `ring_park.enterDeadFd`, the body of
  `Parked.enterDead`. It answers each CQE with RingFailed and never continues the op.
- `drainDead` replaces the old drain loop. It answers new requests with RingFailed and reaps on
  the 1 ms dead tick (`ParkedQueue.recvBlocking(tick)`). It exits once the queue is closed and
  nothing is held. A CQE that never comes holds the join, as on the NVMe lanes (§AR.5).
- The `uring_fs_ring_dead_held_ops` gauge is in the stats JSON.

A held cell's caller has not been answered, so the cell never reaches `freeCell`
(`pushSpinUnlessFull`).

**Red test.** The test runs a reactor on the test thread. A READ of an empty pipe is
submitted, so the kernel took it and armed a poll. A second READ is prepared but never
submitted. Then the test calls `failAll`. Only the untaken READ may answer at the verdict. The
taken READ's cell must stay unanswered, and the gauge must read +1. Then `LATEDATA` is written
to the pipe, the queue is closed, and `drainDead` runs. The held READ consumes the bytes (the
pipe ends at 0 bytes) and only then answers RingFailed. The gauge returns to its baseline.

| run (kamrui) | result |
|---|---|
| red commit, the new test | red at `uring_fs.zig:2122`, `expect(!pool.cells[taken].done.isDone())`: the cell was answered and the buffer freed with the READ still in the kernel |
| red commit × 30 (`-Dfresh`) | red 30 of 30 |
| fix, the new test × 120 (`-Dfresh`) | green 120 of 120 |
| fix, filters `uring_fs`, `ring_park`, `dead ring`, `mpmc`, `session` (`-Dfresh`) | 7/7 steps, 170/172 tests passed (2 skipped) |
| full gate (`-Dfresh`) on `0bb78ef` | 53/53 steps, 2988/3071 tests passed (83 skipped) |

**Sideband (§AR.9): the window is not real.** `Sideband.deinit` frees `buf` only after the
join. It asserts `self.thread == null` (`session.zig:393`) before `ring.deinit` (`:394`) and
`gpa.free(self.buf)` (`:399`). `Session.deinit` runs `close()` (`:1784`), which joins the
sideband (`:1553`), before `sideband.deinit` (`:1797`). The sideband thread itself arms the
READ (`armRead`, `:662`) and submits it (`park`, `:647`). On a dead exit, `finish` skips the
cancel (`:785`), so the READ stays in the kernel past the loop.

The kernel ends that READ when the thread exits, not when the ring closes. `do_exit` runs
`io_uring_files_cancel` before `exit_mm`, and `exit_mm` is what wakes the joiner. That cancel
tears down the thread's io-wq (`io_uring_clean_tctx` → `io_wq_put_and_exit`). The teardown
wakes a blocked worker with a notify signal, which ends `fuse_dev_do_read`'s interruptible
wait. It cancels queued work and waits for every worker to exit. Any copy therefore lands
before the join returns, while `buf` is still allocated. (Checked against the venues' 7.2.6-sqz
source: `kernel/exit.c:949` `io_uring_files_cancel()` precedes `:997` `exit_mm()`, whose
`mm_release` clears `clear_child_tid` and wakes the joiner, `kernel/fork.c:1483`;
`io_uring/tctx.c:190,258` `io_wq_put_and_exit`; `io_uring/tw.h:22` `io_should_terminate_tw`.)

A poll-armed READ that fires after the exit queues task work on a dead task. That work falls
back to the ring's kworker, where `io_should_terminate_tw` fails the READ without issuing it.

The INIT read (`readOnce`, `:412`) runs on the mount thread, and the errdefer at `:1193` would
free `buf` if `awaitOne` failed. That is not a dead-ring exit, and mount(2) queues INIT before
it returns, so that READ completes inside its own submit. It is left as is.
