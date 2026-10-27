# Rung 10 — §BX's error paths: a rolled-back window replays nothing; a failed SMO leaves no reservation open (2026-10-26)

Decision 0003 §BX.8–9. Branch `fix/bx-error-paths` off `8c250a3`. Correctness only: no benchmark was run.

## §BX.8 — a window rolled back with nobody answered

`finishWindow` gave a window's journal-tail slots back only on success. A window rolled back with no member
answered kept its entries written. Two things cause such a rollback: its write failed (the lane's verdict), or
the lane never took it (a refused submit, an image refused). A death then replayed the op whenever the hole
checkpoint had not covered the window, even though the op had been answered as failed. The lane's hole
checkpoint is only a try-lock on the SMO lock, so a checkpoint cycle in flight leaves the hole uncovered. The
slots also leaked past the tail gate's count.

Now, before any member's error is answered, the window's range becomes PADs in the tail. The PADs stay until a
durable ledger lies past the window (`reusable_upto`), and are then swept at a later window's terminal outcome.
After that the window's own slots go back. The held PAD keeps one of the window's own `tail_members` units, so
the pass stays the gate count's only incrementer.

**Red** (backend "§BX.8 …", self and venue). The failed write comes from `fail_windows`, with the SMO lock held
so the cover fails. A later create, answered at its apply, sits in the same page on a buffered ring, grain 1.
After the supervisor's judged replay, the next mount of the device found the failed create. A fix that releases
the slots without the PAD loses the later create instead; I checked that it fails the row. With the fix: the
failed create is absent, the later one is present, and every slot and the count are back after a covering
checkpoint.

**Which windows roll back RAM.** A self-applied window rolls back only when no member was answered. A venue
window rolls back when the lane owns its answer. `b083526` skips undo for a member answered at its apply, and
that member sets `answered` before its share of `pending`, so such a window never meets a rollback. The guard
(conveyor "§BX.8 which windows roll back") pins both sides: a landing-only window rolls back whole, and a mixed
window whose write fails rolls nothing back. It was checked red when a window with an answered member rolls back,
and when a rollback lacks its undo.

## §BX.9 — an SMO failing past its in-window reservation

`smoReplace`, the leaf merge and the root collapse reserve their entry inside the lock window. Every error after
that returned with the reservation open. The maintenance pass only warns and retries, and nothing latches the
volume, so the reservation was never landed, never resident and never completed. Under §BX every later answer at
its apply parked behind it, uncancelable, and every landing, fsync and the checkpoint tail stopped with it: a
wedge.

Now an errdefer at all three sites writes a PAD over the reservation and completes it (`padAbandoned`). It runs
after the window's locks are given back and before the failed build's extents return. Nothing of the SMO is
journaled, which is the crash-equivalent hole a failed entry write already leaves.

**Red** (backend "§BX.9 …", seam `smo.test_fail_after_reserve`). B, committed after the failed replace, was not
answered within 10 s. With the fix, B is answered at its apply, fsync returns, and everything lands.
