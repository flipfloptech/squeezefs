# Rung 10 — nvmet preempt-all and drain barrier

Decision 0004 PR-2 (§2.2, §5.1, §8.2); decision 0003 DO.18–DO.26. Local x86_64:
`7.3.1-1-omarchy-mainline`, Zig 0.17.0, `multipath=N`; Debug tests, ReleaseSafe daemon.
Device substrate only: metadata remains WERO; PR-4/5 are paused.

## Change and reds

`pr.Client` gains preempt-all and three-valued drain-barrier APIs; `admittedHere` only
renames `holdsHere`. Fake registration/generation/preempt/drain laws follow nvmet.
Loop/tcp legs cover the acquire-conflict canary, 50-preempt storm, 1 s null_blk drain
and graceful disconnect. Initial runtime red `3ae3a52`, compile red `7b78d17`;
NQN lifetime red `8717a74` → fix `7ff8ded`.

Review fixes and separate reds are detailed in DO.26:
- **C1:** interrupted native waits resume; rejected SQEs retract; accepted commands retain
  payload/lock through errors and reap only their own tags. Completed CQE EINTR is distinct.
- **C2:** local barrier answers leave sequence diagnostics untouched; counters are sharded
  and exported. **C3:** reserve fake bookkeeping before any hold/ref/semaphore mutation.
- **C4/C6/S1:** failure cleanup owns tickets, workers, write handles, kmsg fds and Client
  arenas; allocation-safe NQN transfer; existing manifest helper reused.
- **C5/C7:** native-issued overlap replaces false done flags; return endpoints check the
  retained write. Missing/errored kernel-log observation fails, not clean.

## Verification

| Check | Result |
|---|---|
| Final focused native/fake/client/fixture/export units (`91cd3d4` source) | 7/7 steps, 113/113 |
| Final rails and fmt | 18/18 steps, 34/34; fmt clean |
| Safe loop/tcp matrix, slow drain, graceful disconnect (`00a3693`) | 12/12 steps, 42/42 each selection |

Storm EIO: loop 36,352, tcp 12,017 over 50 preempt-alls; other 0. Slow-drain EIO reads:
24,743 / 17,549, late 0; both native overlap witnesses and both after-write endpoints true.
The subsequent changes are exports/tests and SQ-poller retry wake-state handling; target
logs retain their exact earlier code identity. No full gate, benchmark/load, real-target
signal/corruption experiment, fatal case, module/configuration install/load or sysctl change.

Recovered historical counts remain DO.25 (77/79 tier and 71/71 regression passes).
A broader combined run passed 126/127 but failed the unchanged CPU-render stress; its
isolated run passed 41/41 with a stdlib ESRCH diagnostic. No cause/closure or release greens
are claimed. Exact commands/logs and responses: `/tmp/sqz-pr2-fix-summary.md`.

## Kernel findings: gate before member re-admission

**P9c EIO is not rare. P17 remains source-reachable, fatal manifestation untested.**
Passing graceful disconnect is ordered only for host I/O draining, not resurrection before
controller free. Keep-alive/fatal teardown was not run (DO.21).

**Inferred fatal TCP deadlock, source-reachable, not reproduced:** a pr_sem-blocked worker
cannot deliver a victim's non-inline R2T/data; teardown waits on it before dropping refs.
Linux permits W/PR/W-data ordering. Separate userspace rings are not transport isolation;
Report/read is not an equivalent global-drain barrier (DO.22; durable two-sided audit).
Resolve these premises on a separately approved disposable target before PR-4/5 proceed.

Integration owes one allocation-safe NQN implementation shared with PR-3 and retained reds;
DO.18–DO.23 overlap PR-3 and must be renumbered together, not overwritten.
