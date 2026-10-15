# rung 10 — owedstorm: the storm's settle read an empty owed set before the last RELEASE

**Symptom.** The unit test `fuse.transport.session` "e2e: a 10 s random-op storm from 4 threads …"
failed in one VM train run on `59349ee`: `expected 0, found 1` on `s.owedCount()` (`session.zig:2880`).
That check came after the storm and its settle loop (`while owedCount() > 0 and < 5 s`). Every law checked
before it held: abandoned and overflow deltas 0, the lost-wake tripwires 0, parked ≡ unparked, and
`leases_outstanding` 0. It was the first sighting. kamrui passed the same commit.

**Verdict: TEST (b), not a lost request.** The settle loop exits when the owed set is empty. An
empty owed set does not mean every request was answered. `close(2)` sends RELEASE as a background
request, and when the storm threads return the kernel may not have delivered the last RELEASEs to a
ring ent yet. The loop then reads 0 and exits at once. A RELEASE delivered a moment later is owed while
the worker → lane → worker round trip runs, and the final `owedCount()` (tens of µs later) can catch it
in that window. The 5 s cap is never involved: the loop exits early, not late.

**Evidence.** An instrumented build on kamrui counted the storm's OPENs and EchoFs's served RELEASEs at
three points: the join, the settle's exit, and afterwards.

| Run set | Settle exit | RELEASEs still to be served at the exit | Afterwards |
|---|---|---|---|
| 6 runs, idle host | 7–185 µs after the join | none | — |
| 16 runs, 4 side by side under 24 × `yes` | 9 runs exited after 10–26 µs with the owed set at 0 | 1–2 in each of those 9 | all served in all 22 runs |

In the runs that started the settle with 1–2 slots owed, those slots drained in 8–22 ms. Before
that, 296 runs (4 side by side, `taskset -c 0-3` plus 6 × `yes`) of the unchanged test had 0 failures
and 0 backstop ticks. No slot was ever seen staying owed. The race alone explains the `found 1`.

**Fix (`5fa7157`, test only).** The storm threads count their OPENs. The test waits until
`file_releases == opens` and only THEN for `owedCount() == 0`. Once every RELEASE has been served, no
further request can arrive, so the final `owedCount() == 0` is sound. A 60 s hang guard bounds the
wait. It is not a speed bound: a drain takes milliseconds. If the guard ever expires, the test fails
`TestRequestNeverAnswered` and `describeOwed` prints each owed slot: queue, ent, state, unique, commit
id, opcode, owed age, the reply cell's state (`free` = the handler has not replied; `published` = the
reply waits for the worker), lease-parked, and bridge. The storm line also reports the drain time.
`commit_queue.CellState` became `pub` so the report can name the cell state. No product code changed.

**Loops (kamrui, the fixed test).** 264 of 264 green: 66 rounds, 4 instances side by side, under 24 × `yes` on 12 threads. Backstop ticks were 0 in every run. Drain from join to quiet: median 6.1 ms, p95 13.8 ms, max 26.5 ms. That is three orders of magnitude inside the 60 s guard.
