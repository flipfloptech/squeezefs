# Rung 10 — nvmet preempt-all and drain barrier

Decision 0004 PR-2 (§2.2, §5.1, §8.2); decision 0003 DO.18–DO.25. Local x86_64 venue:
`7.3.1-1-omarchy-mainline`, Zig 0.17.0, `multipath=N`; Debug tests, ReleaseSafe daemon.
Device substrate only: metadata remains WERO; the WEAR switch is PR-5.

## Change and reds

`pr.Client` gains `preemptAllAndAbort`, three-valued `drainBarrier` and their counters;
`admittedHere` only renames `holdsHere`. The fake follows nvmet's registration order,
generation, acquire/preempt answers and per-victim kill → unregister → admitted-command
drain, including INTERNAL and conformant-target injections. Real loop/tcp legs cover the
preempt-all, KD22's acquire-conflict canary, the barrier, 50-preempt storms, a 1 s null_blk
drain and safe disconnect.

Runtime red `3ae3a52`: six fake laws fail (P4: expected 1 registrant, found 3).
Compile red `7b78d17`: missing drain/client APIs. Fixture lifetime fix `604dfa1`;
separate NQN lifetime red `8717a74` reads freed `UUUU…`, fixed by `7ff8ded`.

## Verification recovered from logs

| Check | Result |
|---|---|
| Final nvmet tier, idle + cpu/regression load | 12/12 steps, 77/79 passed each; fatal teardown and multipath-Y skipped |
| 27-row regression selection, idle + load companion | 12/12 steps, 71/71 harness tests each |
| Earlier broad guard units ×3 (`2754dfe`) | 7/7 steps, 256/257 each; 1 skipped |
| Earlier FULL `zig build test` (`2754dfe`) | 23/23 steps, 3,383/3,584; 201 skipped, 0 failed |
| Recovery (`d2c9ee1`) | Units 66/66; rails 18/18 steps, 34/34; fmt clean |

Final idle storm EIO counts: loop 32,061, tcp 11,597 over 50 preempt-alls, other outcomes 0.
Slow-drain EIO reads: 34,091 / 19,468, 0 late; admitted write landed, later write -EBADE;
barriers registered/unregistered after the drain. Loaded results and trace details are DO.20/25.
The final script recorded `6e88aca`; `d2c9ee1` differs only in module documentation.
Three earlier idle tier greens are not three final-head greens. Exact commands/logs:
`~/tmp/pr2-final-runs.sh`, DO.25 and `/tmp/sqz-pr2-recovered-summary.md`.
No full gate, timing benchmark or target rerun during recovery.

## Findings: gate before member re-admission (PR-4)

- **P9c write EIO is not rare.** The classification still holds; §8.2's estimate does not.
- **P17: source-reachable, fatal manifestation untested.** Safe loop/tcp disconnect passed
  without a splat, ordered by construction only for graceful host I/O draining, **not** ref
  resurrection before controller free. Keep-alive/fatal teardown was **not run**; the opt-in
  invokes the fatal callback, not actual expiry (DO.21).
- **Inferred fatal TCP deadlock, source-reachable; not reproduced.** PR payload execution
  blocks the worker needed by a victim's non-inline R2T/data; teardown waits on that worker
  before releasing refs. Linux permits W/PR/W-data ordering; separate userspace rings are
  not transport isolation. Report/read is not an equivalent global-drain barrier (DO.22).
  The durable two-sided audit follows DO.25. PR-4 is paused for premise resolution;
  fatal tests require separate approval and a disposable target. No redesign or kernel/configuration change.

Integration owed: PR-3's `bb3b480` duplicates the NQN fix; keep one implementation and this
branch's separate red. DO.18–DO.23 overlap PR-3: renumber both ledgers and references together.
