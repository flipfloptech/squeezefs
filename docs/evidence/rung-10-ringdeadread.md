# rung 10 — ringdeadread: a dead ring handed back buffers the kernel could still write

**Defect.** `exec.Lane.onRingDead` answered EIO for every fiber with a ring op and readied it. An
`io_uring_enter` failure cancels nothing the kernel already took, though. A READ the kernel holds
still lands whenever it completes: an io-wq copy, a device DMA, or task work. By then the woken
fiber has its buffer back and its caller is reusing it. The NVMe lane's `failInFlight` had the same
shape. It sent the cells of in-ring requests home, so a late READ DMA could land in the next
request's pool slot or in `readInto`'s caller buffer. `rung-10-deadring.md` closed only the half
the kernel never took (§AR.1).

**Audit.** The table lists each ring user that can see a death: the ops the kernel may hold, and who
frees or reuses their memory.

| ring user | in-kernel ops at the verdict | memory the kernel may write; owner | before | after |
|---|---|---|---|---|
| exec lane fibers (`exec/lane.zig`, threaded + driven) | READV/WRITEV (`sqz_io`), POLL_ADD (`fdwatch`) | READV target: the fiber's caller | readied at once, buffer reused (**UAF**) | fiber parked until CQE (§AR.4) |
| journal lane (resident on an exec lane) | WRITE runs, FSYNC | none (a WRITE reads its bounce or window) | answered at once (§AR.2) | unchanged; late CQE dropped by the lane |
| NVMe lanes (`io/nvme_dev.zig`) | READ / READ_FIXED, WRITE, FSYNC, discard | READ DMA into the pool slot (`readBlock`) or the caller's `dest` (`readInto`) | cell answered, slot home (**UAF/DMA**) | cell + buffer held until CQE (§AR.4) |
| `ParkedRing` wake READ (every owner) | eventfd READ | `wake_buf` (8 B, heap) | freed after the ring's close | unchanged (§AR.6) |
| transport SQE128 worker (`fuse/transport/worker.zig`) | FUSE uring cmds, lane-wake READ | headers, payload arena, kmbuf, `lane_wake_buf` | freed in `Group.deinit` after the ring's close | unchanged (§AR.6) |
| sideband (`fuse/transport/session.zig`) | `/dev/fuse` READ, reply WRITEs | `buf` | healthy exit cancels and reaps; dead exit frees after close | noted risk (§AR.7) |
| zcrx lane (`ops/zcrx/lane.zig`) | RECV_ZC, sends | refill region | unmapped after the ring's close | unchanged (§AR.6) |
| uring-fs pool (`io/uring_fs.zig`) | READ/WRITE/FSYNC per slot | caller cells' buffers | `failAll` releases the slots (**same defect**) | open: its own change, because the file was `uringcell`'s while this landed (§AR.7) |

**Contract (decision 0003 §AR.4–AR.7).** An op the kernel took stays its owner's (buffer, cell,
waiter) until its CQE, and answers RingFailed then. `Parked.untaken` tells the SQEs the kernel
never took (between its SQ head and the owner's tail); those are answered at once. The dead ring
keeps reaping on the owner's backstop tick through `Parked.enterDead`: one enter with
`to_submit = 0` (it submits nothing, and runs `DEFER_TASKRUN`'s deferred completions), plus the
CQ memory. A cancel request on a dead lane goes through `IORING_REGISTER_SYNC_CANCEL`, which is no
SQE. A CQE that never comes leaks its op for good, and the `exec_ring_dead_held_ops` and
`dev_ring_dead_held_ops` gauges count it. No timer gives up, because no buffer could be returned
anyway.

**Red test.** A READ of an empty pipe goes through io-wq (`IOSQE_ASYNC`), so the worker copies
without any enter. The ring is killed once the READ is in the kernel. A writer fiber, woken after
the verdict, writes `LATEDATA`; one ready FIFO means it runs after every fiber the verdict readied.
The test waits until the pipe has drained and the reader has finished. Then the buffer must hold
the reader's own `REUSED!!`.

| run (kamrui) | result |
|---|---|
| pre-fix (red commit), the new test | red: `expected 0, found 8` (`lane.zig:1715`). The fiber had its buffer back while its READ still sat in the kernel; under `DEFER_TASKRUN` it never ran |
| pre-fix binary × 30 | red 30 of 30 |
| fixed, the new test × 120 (`-Dfresh`) | green 120 of 120 |
| fixed, filters `dead ring`, `ring_park`, `journal_lane`, `exec.`, `nvme_dev` | 7/7 steps, 192/192 tests passed. This includes the new `ring_park` row (`untaken` / `cancelSync` / `enterDead`) and the NVMe dead-ring row, whose slow READ is now held for its 300 ms and then answered |
| full gate (`-Dfresh`) on `63b8296` | 53/53 steps, 2987/3070 tests passed (83 skipped) |
