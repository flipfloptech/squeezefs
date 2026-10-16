# Rung 10 — the hang watchdog: a hung test fails its step in minutes (2026-10-16)

**Why.** A hung test held the whole venue until the gate's outer `timeout 3900`: a kamrui gate
sat 63 min on `exec.lane` "a dead ring keeps a READ…" (shard 3/6, 199 % CPU), a VM gate 59 min
on the `nvme_dev` slow-read seam, the gate lock and every build slot blocked meanwhile. The
heartbeat saw it (`running "<test>" for 63m35s`); nothing acted on it.

**What changed.** `tests/test_runner.zig`'s heartbeat thread now ticks every second in both
modes on Linux and checks the running test against `shard_plan.hang.limitUs(row)`. Past it the
runner writes one line — live to the heartbeat's sink and to its own stderr, which the build
runner reports with the failure — dumps every other thread's name, state, CPU, `wchan` and
`/proc/self/task/<tid>/stack`, then each thread's user stack (SIGQUIT to one thread at a time,
the test's own thread first; the handler unwinds from the signal context; a handler stuck past
2 s ends the dump), and `exit_group(124)`. The build runner reports `'<test>' exited with code
124 with stderr:` and resumes the shard in a new process. `scripts/gate-train` takes the
`HUNG TEST:` line as the FAIL reason. No knob: `--hang-limit-ms=N` (a runner flag) exists for
the self-test only. Zig 0.17's `zig build --test-timeout` is one global bound with no dump; it
stays unused.

```
HUNG TEST: "hangwatch_selftest.test.hangwatch: a spinning test never returns" ran 2.5s (cost table: no row; limit: 2.0s) [squeezefs-hangwatch-selftest]
thread 1390601 "squeezefs-hangw" state R cpu 2.59s wchan 0
    [<0>] ktime_get+0xba/0x110
user stack of thread 1390601:
…/tests/hangwatch_selftest.zig:20:34: 0x12b5d40 in test.hangwatch: a spinning test never returns (hangwatch_selftest.zig)
…/tests/test_runner.zig:458:25: 0x12074b2 in mainTerminal (test_runner.zig)
```

**The limit**: `clamp(20 × row, 120 s, 900 s)`; no row (or no table: the filtered dev run,
bench/nvmet/shim smokes) → 900 s. The numbers come from every `--costs-out` file in kamrui's
gate cache — 762 shard outputs, 43,114 measured test runs, 2026-10-15 01:09 → 10-16 01:34 —
against the committed tables, plus the gate-train logs' heartbeat samples (vm + kamrui):

| | measured | so |
|---|---|---|
| runs past 10× their row and 30 s | 3 of 43,114 | k = 20 |
| runs past the formula | 2, one stalled run (04:51): 1019 s on a 3.9 s row with 5 s CPU, 1043 s on a 43 s row | both real stalls |
| a row under 6 s, worst green wall | 45 s (meta.backend D4 faces, 5.15 s row) | floor 120 s (2.7×) |
| slowest row | 241 s (the smoke's jobs matrix) | cap 900 s (3.7×) |
| longest no-row test, green | 104 s (a smoke row, heartbeat sample) | no row → cap |

No row gets the cap, not the floor: its cost is unknown, and `zig build costs`, which would
measure it, runs under the same watchdog — a floor would make a new long row unmeasurable. The
motivating `exec.lane` test had no row: it would have died at 15 min, not 63.

**Proof.** `zig build rails` (so the gate) runs `tests/hangwatch_selftest.zig` in terminal mode
with `--hang-limit-ms=2000`: two 1.2 s tests pass (together 2.4 s > the limit: the limit is per
test), the spinning third must produce the HUNG line naming it with `no row; limit: 2.0s`, a
`state R` thread, a user stack inside the spin, and exit 124 (kamrui: `run hangwatch self-test
success 5s`). `shard_plan`'s tie test restates the formula. A green full gate shows normal
shards untouched: kamrui, `-Dfresh=true`, `Build Summary: 55/55 steps succeeded; 2991/3074 tests
passed (83 skipped)`, no `HUNG TEST` line, every shard's heartbeat `done … 0 failed`; a
filtered dev run (`scripts/venue test "meta.leaf_merge"`, no table: every test at the cap)
49/49.
