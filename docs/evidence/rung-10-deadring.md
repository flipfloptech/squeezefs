# rung 10 — deadring: a dead ring's flush submitted a window the lane had already failed

**Symptom.** One kamrui train run on `baf17ef` failed the unit test "a dead ring latches the volume
FAILED at once …". The final replay found `[1, 2]`. Ino 2 is the entry appended after
`test_seams.kill_ring`. Its ticket answered `RingFailed`, and the lane logged "rolling back 1
entries" for its window.

**Verdict: (b), a product defect.** The seam models a failed `io_uring_enter` faithfully. The fiber
prepares b's WRITE SQE in `admit`, and no enter happens before `failAll` in the same poll. `failAll`
marks the ring dead, releases the slot (and any bounce buffer), and b's ticket answers RingFailed.
The SQE was never submitted at that point. Afterwards, `exec.Lane.parkOnce` ran
`ParkedRing.flush()` on its work-in-hand and inbox branches. Both run before the park that checks
the verdict, and `flush` entered the kernel whether or not the ring was dead. On kamrui the shutdown
kick could reach the inbox between the fiber's poll and that branch, so the dead window was
submitted and landed. The same path exists in production. A failed park submit leaves its SQEs
published in the SQ. `onRingDead` then fails their owners and readies their fibers, the next
`parkOnce` takes the work-in-hand flush, and after a transient errno (EAGAIN/EBUSY) the kernel takes
SQEs whose owners already answered EIO and may have freed their buffers. For a fiber's READ, that
memory may already belong to something else.

**What can be in flight at a ring death, and what replay does.**
- Prepared but not yet submitted (the seam's window, or a failed enter's SQ): after the fix these
  never reach the kernel, so the window's entry is never on the device.
- Already taken by the kernel (submitted, CQE not yet reaped): these are journal WRITE runs, the
  FSYNC barrier and the wake READ. An enter failure does not cancel them, so a WRITE may still land.
  Its window was never acked, because acks follow landing and come in journal order. `failAll`
  answers RingFailed at once (waiting on a ring we can no longer reap would wedge, and §1.8 says
  "loud, never a wedge"). The device may therefore hold the entry, and the next mount's replay
  applies it. That makes RingFailed an **indeterminate EIO**, the classic EIO contract. The volume is
  FAILED, so no later mutation is acked over it. A WRITE only reads the buffer that `failAll`
  released, so the worst a reused bounce buffer can do is put garbage in an un-acked window, and
  replay's checksum drops that.

**Fix (`97c81b3`).** `ParkedRing.flush` returns on a dead ring. This one guard covers every owner: the
exec lane, the journal and NVMe lanes, and the SQE128 transport face. `pumpDriven`'s own `isDead`
check is deleted. `journal_lane.failAll` states the contract. Decision 0003 §AR records it.

**Red (`6191aed`).** The test holds the poll that admits b (`hold_before_scan`), kills the ring during
that poll, and posts one foreign message (`postCancelOp` of the lane fiber, a no-op cancel) so the
lane loop takes its pre-park flush. That is the shutdown kick's shape, now deterministic. A
`ring_park` assertion checks that an SQE prepared before `markDead` stays unsubmitted through a
flush.

| run (kamrui) | result |
|---|---|
| pre-fix, `dead ring` filter | journal test `[1, 2]` (`journal_lane.zig:2524`); ring_park `sq_ready` expected 1, found 0 |
| pre-fix binary × 30 | journal test red 30 of 30 |
| fixed, `a dead ring latches` × 300 | green 300 of 300 |
| fixed, `journal_lane` / `dead ring` / `ring_park` filters | green |
| full gate (`-Dfresh`) on `97c81b3` | 53/53 steps, 2981/3064 tests passed (83 skipped). The run before it was red on one unrelated row, `io.uring_fs` "fd cache churn" (`uring_fs_cell_return_refused`; `uring_fs` imports neither `ring_park` nor `exec`), now a new row in `flakes.md` |

**Not fixed here, a noted risk.** `exec.Lane.onRingDead` wakes a fiber whose ring READ the kernel
already took. That READ can still complete into the fiber's buffer after the fiber has moved on. The
fix closes only the never-submitted half of that window.
