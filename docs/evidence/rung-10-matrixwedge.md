# Rung 10 — THE TWO-DAEMON MATRIX on the VM: B's teardown tripwire (fixed) and A's teardown wedge (open)

**The red.** The VM gate on `715450e` and again on `e540b77` (the first two heads with the clock cuts) failed
the row at `umount A`: `daemon N has not exited 32 s after the signal`. Both logs also show B's
`REFUSED begin_free of block 129: no RAM refcount` (`block_untracked_free_refusals`). kamrui passed both heads.

**(2) B's tripwire — every run, not a race.** A stack dump at the refusal and a log of the refcount at ship time
(diagnostic builds, not landed) show the path: B's leave → `wp.stop()` → `WriterPlane.destroy` → the shipper's
last flush → A answers `Freed` → `trackRetire` → B's local `beginFree`. Block 129 shipped with `rc null, state virgin`:
A minted it for its 8 MiB write into B's file, a later served merge displaced it, and B's reclaimer shipped it as an
untracked block, which it does by design (porting spec §4.1). A freed it correctly. B's retire then read the missing count
as a double release. It fired in 52 of 52 VM runs and in every kamrui run. Nobody saw it because the row judges B's
tripwires before B leaves, and B's alerts print only when the row fails. It does not come from the clock cuts.
- Red (`d977a1b`): unit `plane_arm` "…never tracked touches no RAM count" fails on the VM with `expected 0, found 2`
  (retire + release). The row now also judges B's log after B exits.
- Fix (`821793c`, 0003 §AU): the retire and release steps touch B's allocator only for a count B holds. `freed_since_word`
  still counts every `Freed`. Green: the unit row; `sym_planes` 116/116 ×3 fresh on the VM.

**(1) A's teardown wedge — open.** It reproduces only inside a full VM gate. It failed 2 of the 4 train gates since the
clock cuts and 0 of the ~88 VM train gates in which the row ran before them. The four commits between the last green
(`68b8585`) and the first red are only the clock cuts (`57d9090`, `fa3fe60`) and two docs commits. So the history
points at the cuts. The mechanism is unproven. Attempts that did not reproduce it:

| attempt | runs | wedges |
|---|---:|---:|
| row alone, VM, idle / 4 × `yes` (dev `e540b77`, diag build) | 54 | 0 |
| row alone, kamrui, 6 × `yes` | 14 | 0 |
| full smoke (4 shards), VM | 3 | 0 (one unrelated JOBS MATRIX `job pause` failure) |
| diagnostic full gate, VM, with a /proc watcher | 1 | 0 (51/51 steps, 3,001/3,086 tests) |

An A/B row loop (`68b8585` vs `715450e`) would read 0 vs 0, so the train's history is the only A/B. The VM's
coarse clock lags the fine clock by at most 9.0 ms idle and 9.8 ms under 12 × `yes`, against the 12 ms
`coarseLagNs` bound (HZ=250, `CONFIG_NO_HZ_FULL=y`, no `nohz_full=` CPUs). A lane timer that misses that bound
spins or re-parks until the coarse clock catches up (`expireTimers`, `parkOnce`, `drivenBoundNs`). It is never
32 s late, so no lane-timer path explains the wedge on its own.
- Capture (`12eeae1`): when `squeezefs umount` gives up and A is still alive, `Rig.umount` prints every thread's name,
  state, wchan and kernel stack, then the last 6 KiB of A's log (exercised once on the VM: 48 threads). The rig used
  to SIGKILL A and delete its log before anyone could look. The next sighting will carry the step and the thread that
  never finished. Row in `docs/gates/flakes.md`.

**Loops after the fix** (the row with B's new assertion): VM 34/34; kamrui 32/32 under 6 × `yes`.
