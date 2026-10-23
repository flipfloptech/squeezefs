# Rung 10 — a live member evicted at T_owner: the DEAD MANAGER row's second shape (2026-10-22)

The flakes ledger's row `A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY` hung the train once in 12
(`.gate-train/local-6a1f2fd402dc-20261022T000244Z.log` ~1184): the three kill arms passed, then
`Storm.deinit → child.kill → wait4` never returned — the storm child parked in a FUSE request on B past
SIGKILL; the hang watch had to abort B's connection. Venue: this workstation, `7.3.0-1.7-mainline-sqz`,
branch `fix/reclaim-twice-poison-fail-stop` off dev `f11cd41`. Scratch `~/tmp/reclaim2/`.

## The mechanism, pinned with a seam

`SQUEEZEFS_TEST_MEMBERSHIP_BEAT_HOLD` (a FIFO path, registered): the joiner's member beat parks ONCE, at
the first tick that finds the path present, until a writer opens it — no renewal and no reclaim beat
while every other thread of B serves the storm. It is what the gate's load did to B: the beat makes its
other calls inline (the census, the re-point's dial, the endpoint's re-publication, a set-config
learn), each bounded only by the dialer's 10 s reply bound, against the row's 3 s `T_owner`.

The trace (B's log, `~/tmp/reclaim2/dlogs/`): kill -9 A → `T_self reached … parked` (1) → A remounts
at term 2 → `the successor … admitted the reclaim (epoch 1)` → `BEAT_HOLD — the member's beat holds`
→ (A: `tick — 1 evicted at T_owner … 1 acted`, 3.9 s into the hold, B alive and shipping creates) →
`BEAT_HOLD released` → `T_self reached … parked` (2) → the reclaim meets the closed grace →
`appender_park_expiries: the successor answered UnknownLease` → `this joined mount is POISONED`.
Then, on dev: `move gate: getattr met a slot move that did not resolve in 48000 ms (last answer
MembershipPending) — answering EIO` — every foreign-object op of B (the root's getattr/lookup under
the token divert, refused `NotMember` by A) parked at the move gate for `T_park_max` PER op; a create
arriving after the poison was still unanswered 20 s later, the storm child sat in such a request
(uninterruptible), `wait4` hung. Four laws were broken on the way:

1. **The poison held ops.** The move gate's only early ends were the lattice's deadline and the mount's
   close. `MetaSet.poison` now latches every volume FAILED AND closes the mount's move epoch
   (`Moves.close`): every parked op answers EIO at once (`did not resolve in 0 ms`), no later one waits.
2. **`T_self` was the beat's alone.** The committers consulted the gate's word, which only the beat
   moved, so a stalled beat let a LIVE member commit for 3 s past `T_self` into a region its manager
   evicted and was recovering (the zombie writer): with fix 1 alone, A's `ls hot` lookup parked past the
   kernel's 51 s request timeout and the kernel ended A's connection; `xv_cross_owner_steps_rejected`
   fired on A (the ledger's FIRST shape); A's checkpoint tick failed `Corrupt`. The committers now judge
   `T_self` at the pre-admission gate (`park.SelfFence`, bound at the first grant; one monotonic read per
   batch admission) and park the gate themselves; the gate's CAS is the one latch between the beat and
   the committers (`park()` answers whose it was), so `membership_self_fences` counts one. In the green
   trace B's second `T_self reached` lands BEFORE `BEAT_HOLD released`.
3. **A fenced member's dismount was the clean leave.** The poisoned B's `umount` ran the release ladder:
   61 `ReleaseSlot`s for slots A had recovered and a page write onto the page A had set `Recovered`.
   `leaveAllowed` refuses past the poison as over a park, and `JoinedSet.leave` writes nothing then
   (the crash shape; the daemon exits 1 naming the FAILED volume).
4. **The reclaim was not idempotent against a lost grant reply.** `Owner.join` held a reclaim only at the
   exact epoch granted; a member that missed a re-point's reply named its last epoch and, once grace
   closed, answered `UnknownLease` — a poison of a live member. A stale epoch from the SAME incarnation
   (pid, boot) re-points; another incarnation or an epoch above the held one stays `UnknownLease`.

## Red tests

- smoke `A LIVE MEMBER EVICTED AT T_OWNER UNDER THE DEAD MANAGER'S STRIPED HOT DIRECTORY` (~8.5 s): the
  DEAD MANAGER shape, the hold, the eviction, the poison, then the fail-stop — an arriving create
  answers EIO within 20 s (dev: HELD; the row aborts B's connection and fails, never hangs the shard),
  the storm child ends when told, `umount B` completes with the crash shape, A lists `hot` with no
  tripwire, fsck clean.
- `shard.zig`: "T_self is the member's clock, never its beat's liveness" (a committer parks the gate with
  the beat stalled; one park, one self-fence; the beat's next tick re-points); "the reclaim is idempotent
  against the state held" (the stale-epoch re-point; the other-incarnation and above-held refusals).
- `park_reclaim.zig` step 5: the leave is refused past the poison.

## Counts

On `c3e53e2`: the row 20/20 beside the full smoke (A evicted B 3.9–4.2 s into the hold; the fresh create
answered EIO in 1–3 ms); the unit filters `membership` 100/100 (100 tests), `reclaim` 100/100 (99),
`lattice` 100/100 (58) beside the full smoke; the recovery and membership rows (N-DAEMON, KILL ROWS a/b/c,
KVDA ×2, SHARED HOT DIRECTORY ACROSS DAEMONS, DEAD MANAGER, `-o ro`, TWO MOUNTS OF ONE SET, ONLINE)
60/60; rails 32/32; `zig fmt --check` clean. The full smoke beside the loops: 117/121 (4 skipped), then
114/119 with THE KILL ROWS (c) red on a daemon panic in `exec/cpu_class.zig:101` (`index 170, len 13`
in `finishPass` under a `.stats` read at umount) — not this branch's code.

## Seen, not fixed

- `exec/cpu_class.zig:101 finishPass`: `self.retired[e.value_ptr.class]` with class 170 of 13 — a
  departed thread's ledger entry with an out-of-range class; killed THE KILL ROWS (c)'s A once under load.
- The manager accepts `ReleaseSlot` from an appender whose region it has recovered (fix 3 stops the zombie
  from sending them; the screen on the serve side is still owed — "every wire word is SCREENED against
  durable state").
- The DEAD MANAGER row runs at `smoke_t_owner_ms` (3 s): a renewal late by `T_owner − T_self` = 2.2 s
  under load now fails B LOUD and BOUNDED (poisoned, EIO, umount completes) instead of hanging, but the
  row still goes red then; the storming three-daemon rows run at 7 s for the same reason
  (`smoke_storm_t_owner_ms`).
