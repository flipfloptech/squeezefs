# Rung 10 — the metadata WEAR and preempt-all model (decision 0003 §DO, PR-1 of decision 0004)

PR-1 of decision 0004 lands the model only, with no product code. It has three parts:

- the explorer's opt-in quiescence and cycle checks and `replayError`;
- `src/meta/guard/meta_fence_model.zig`, with the families `Death`, `Succession`, `Window` and `Debt`;
- the metadata half of `src/meta/guard/data_fence_model.zig`, re-based on the axis
  `.wero_holder` / `.wear_preempt_all`.

The model went through three design revisions:

1. **Revision 15.** The model was built from it and found four as-built failures, three
   specification errors and one inconsistency (D1–D10, N1).
2. **Revision 16** ruled those findings. Applying it found modelling artifacts, which were fixed,
   one new spec defect (D11), and the `Debt{.ja}` family over the bound.
3. **Revision 17** (17.1 adds no model change) answered the review of revision 16 and G1. Applying
   it found one more modelling artifact and one new spec defect (D12).
4. **Revision 18** ruled D11, D12 and the `Debt{.ja}` bound, and added G4 and
   `Succession{.lapse, .split_register}`. Applying it found one new spec defect (D13) and one more
   modelling artifact: row 1, `KilledCurrent`, was not evaluated in `Succession`.
5. **Revision 19** ruled D13 and put the re-judge's stale path in the product's order: the SIGKILL
   and the release of the registration lock in one step, then the withdrawal. W57 shows that order
   is load-bearing. Applying it found nothing new.
6. **Revisions 20–27** changed no mechanism the model mirrors, and the design converged with 0 open
   issues (see "The converged design" below).

The model at `d0fae07` mirrors revision 19, whose mechanisms revision 27 keeps.

The counts and seconds come from the Debug test build under `tests/test_runner.zig` on the local
venue (kernel.org 7.3.1, 32 threads), one process. A state count is the same in ReleaseFast. In the
tables, "len" is a failing schedule's length, and "states" is the number explored up to the first
error.

## The explorer (`src/model/explorer.zig`)

| Check | What it does |
|---|---|
| Quiescence (opt-in: the model declares `stuck`) | A step that leaves the state key unchanged is a blocked step and is not recursed into. At a state with an unfinished thread where no step changes the key, the explorer calls `stuck()`, and its error fails the search as `error.StuckViolated` with the schedule. |
| Cycle (same opt-in) | The memo value is a mark: on-path when the search enters a state, done when it leaves. The back-edge test runs before the seen-state return, so a step onto the current path fails as `error.CycleViolated`. |
| `replayError(Model, schedule)` | Replays a schedule the way the search does. A revisit of the replayed path gives `CycleViolated`, and invariant errors are reported along the path. At the end it calls `final()` if every thread is done, `stuck()` only at a quiescent end, and otherwise returns `NoInvariantNamed`. |
| Models that do not opt in | Explored exactly as before. The four existing self-tests are unchanged. |
| New units | A circular wait gives `StuckViolated` with `stuck` and holds without it. A waiter released by a bounded environment thread holds. A blocked waiter beside a ping-pong gives `CycleViolated` with `stuck` and holds without it. A bounded retry loop holds. The replay names `CycleViolated` for the ping-pong and the model's `stuck()` error for the circular wait. |

## Revision 15's findings, closed by revisions 16–19

| # | Revision 15's witness | The ruling | The model now |
|---|---|---|---|
| D1 | W2 first failed `WroteOverLiveClaim` (len 32), outside {`OverwroteAcked`} | W2's set gains `WroteOverLiveClaim` | W2 fails `WroteOverLiveClaim` (32), inside its set |
| D2 | the literal budget, one retry per change of `mgr_writer[v]`, failed `LiveFenced` (63) | §4.5.3's budget is M7; the old reading is W48 | W48 fails `LiveFenced` (31) |
| D3 | `Succession{.m0_unlisted}` `TwoManagers` (29); `{.lapse}` `UnreservedUnderManager` (30) | DC20; revision 17 narrows it to a registration lock, from a rule's register through its re-judge, and adds DC24, the register's re-read, which kills a stale M0 (G1); revision 18 releases the lock on a stale verdict and adds `.split_register` | both hold (15,275 and 161,640); W54 (no re-read) fails `TwoManagers` (30); W19 discriminates (`TwoManagers`, 29); A9 is reachable (`BlindWindow`, 29); W49 holds alone in the atomic configurations (D12, ruled defense in depth) and fails `TwoManagers` (30) in `.split_register` |
| D7 | `Succession{.lapse, .kept_law}` `TwoManagers` (25) | DC21 and the holder pointer | holds (1,960); W50 fails `TwoManagers` (25, D7's witness); W18 on `.lapse, .kept_law` fails `OverwroteAcked` (25) |
| D8 | `Debt{.ja}` and its companions `LiveFenced` (48) | DC23; revision 17: a failed metadata re-check keeps the data registration, stores `cleared` and retries; revision 18 splits the family by operation | the split forms hold under the bound (below); W51 (revision 15's decline) fails `LiveFenced` on op 2 alone (35) |
| D9 | W38's kick-only half alone failed `RegisteredButPending` (44) | W38a holds, W38b fails | on op 1 alone: W38a holds (611,063), and W38b fails `RegisteredButPending` (35) |
| D10 | `Window{.late_clear}` and W36 `FencedMemberWrote` (25) | DC22; revision 17 bounds the wait, and at the bound the registration stays | both hold (3,643 and 3,837); W52 (plain unregister) and W53 (unregister at the bound) fail `FencedMemberWrote` (25, D10's schedule) |
| N1 | a write EIO with a "registered" barrier: §7.1 retried it, §4.5.3 latched it | §4.5.3 re-issues once | no model change |

## What applying revisions 16–19 found

**Modelling artifacts, fixed in the model.** Each was hidden behind an earlier failure, and each
diverged from the design or from the product's code.

1. **A rule's adoption is decided at its read**, as `host_fence.registerUnderRule` does: it
   returns at its first report when the host is registered. The model re-tested registration at
   its register step. So a rule that read host A registered, before a preempt-all unregistered it,
   re-admitted a superseded M0 (`Succession{.lapse}` `TwoManagers`, len 34, under revision 16).
2. **`WindowLateClear` gains the design's checks**: R-1 at the held verdict, M5 at the post-check,
   R-1's not-Live proof at every tick whatever the registration, and C2 before the register.
   Without them the run stranded at quiescence (len 28), and an evicted J's write landed after the
   scan (len 19).
3. **`Debt`'s dead values are canonical**: a refused successor's program counter and working
   debt, and the probes' predecessor after use. `Debt{}` goes from 893 to 813 states, and
   `{.bind_race}` from 4,661 to 3,233.
4. **(Revision 17's run) A killed M0's registration lock is released with it.** Once a rule takes
   the lock only at its register, its read can kill M0 inside M0's section. M0's lock then never
   came free, and the rule's register stranded (`Succession{.lapse}` `StrandedAtQuiescence`, len
   29). This matches revision 16's §7.6 ("as do M0's fail-stop and death").
5. **(Revision 18's run) Row 1, `KilledCurrent`, is evaluated in `Succession`.** At `4a0dec9` it was
   not, although rows 1 and 15 read "current" through M3 and DC24 kills M0. A rule's SIGKILL of M0
   now sets row 1 when M0 is current: bound, alive, its claim, and no takeover reached host A. No
   run trips it.
6. **Withdrawn with revision 17:** revision 16's run had added the data rule's metadata pre-check
   (`registerDataUnderRule`'s `HostFenced`) to stop its held arm livelocking. Revision 17's op 2
   keeps its registration and has no held path, so op 2 is back in revision 15's shape. Its
   re-judge's re-check covers the same ground.

**D11, ruled in revision 18: the claim re-read is load-bearing in `.lapse` too.** W18 fails there
with `OverwroteAcked`, len 27, schedule {0×3, 1×14, 3, 3, 5, 0, 0, 1, 0×4}: S1 binds inside A3 with
no holder, and M0's lapse acquire succeeds. The Holds row was wrong.

**D12, ruled in revision 18: the lock is defense in depth behind the atomic re-read.** W49 alone holds
in every atomic configuration:

| Configuration | States |
|---|---|
| `.lapse` | 166,442 |
| `{}` | 354,005 |
| `.fresh_ja` | 8,651 |
| `.lapse, .kept_law` | 1,960 |
| `.page_unreadable` | 28,458 |

W49 with W54 fails `TwoManagers` (29). In `Succession{.lapse, .split_register}`, which models the
product's two commands, W49 fails `TwoManagers` (30).

**D13, ruled in revision 19: W39 runs on the full form.** W39's witness spans both operations: only
op 1's held verdict makes the data word `pending`, and op 2's arm then completes without clearing
it. On op 2 alone W39 held (506,059 states). On `Debt{.ja}` with both operations it fails
`ClearedAfterEstablish` at len 33 (33 states), schedule {0×3, 1×12, 2, 3×8, 5×5, 4×4}. A failing
search stops at its first error, so W39 runs within the bound, and a lost witness would surface as
`StateSpaceExceeded`. Its as-built companion is the full form's one-off measurement (19,972,582).

**Revision 19's re-judge order (W57)**, ruled by decision 0004 revision 20 with no model change. Every re-judge kill path now takes the product's order: the
SIGKILL and the release of the registration lock in one step, then the withdrawal of the fresh
registration as a later step, then the register again after the drain. The kill path exists in
JA's re-judge only, because JA.beat and JA2 run where M0 is off (`.drain_reads`,
`.chain_after_reregister`, `.ja2`). DC22's wait is vacuous at that withdrawal, because the unsplit
JA issues its write on the same thread.

W57 (`rejudge_kill = .after_release`: the release first, the kill a step later) fails
`UnreservedUnderManager` in `.split_register` (len 33, 13,872 states), schedule
{0×6, 1, 3, 3, 5, 1×3, 3×4, 1×8, 3, 3, 0, 0, 0, 3, 3, 1}:

1. M0's section takes the released lock.
2. Its fence of host B lands before S1's bind.
3. JA's late kill and withdrawal leave the namespace without registrants.
4. S1 binds into the unreserved namespace.

W57 holds in the atomic configurations: 166,478 states in `.lapse` and 354,005 in `{}`.

**The `Debt{.ja}` family, ruled in revision 18: split by operation.** Revision 19 drops the merged
S1 open from the `.all_elapsed` form. Every split form holds under the bound:

| Form | States | Depth |
|---|---|---|
| op 2 alone | 589,249 | 47 |
| op 1 alone, M's data write off | 780,616 | 50 |
| `.unreadable`, op 1 alone, M's data write off | 685,518 | 50 |
| `.all_elapsed`, op 1 alone, M's data write off | 1,436,868 | 48 |

None of the forms needs the ledger's other trims, except the `.all_elapsed` form. With N_tick = 2 it
had 4,958,121 states, and with N_tick = 1 2,354,349. It fits with S1's data write off as well, a
ledger trim, at 1,436,868. The merge of S1's open (revision 18) saved 48 states and hid
interleavings, so revision 19 removed it, and the knob went with it.

The full forms with both operations are one-off measurements, all of which hold:

| Full form (revision 17) | States |
|---|---|
| `Debt{.ja}`, ledger trims | 19,972,582 |
| `Debt{.ja}`, untrimmed | 32,465,658 |
| `{.ja, .unreadable}`, ledger trims | 17,411,527 |
| W38a, ledger trims | 19,387,358 |
| `{.ja, .all_elapsed}`, ledger trims | 150,918,967 |

Revision 18's new knobs are off by default, so these counts stand.

## As-built configurations

"Holds" includes `stuck()` and the cycle check. Revision 15's counts are §7.4's measured column, and
seconds are the Debug runtime of the test that runs the row.

| Configuration | Revision 15 | Revision 19 states / depth | Test s |
|---|---|---|---|
| `Death{}` / `{.colocated}` / `{.joiner_key}` | 47,223 / 181,340 / 51,864 | the same (48 / 51 / 49) | 0.69 |
| `Succession{}` | 333,950 | 354,005 / 61 | 0.97 |
| `Succession{.drain_reads}` | 578,858 | 592,242 / 67 | 2.25 |
| `Succession{.drain_reads, .all_elapsed}` | 1,862,984 | **1,881,600 / 68** (5.9 % under the bound; S2 off is its named trim) | 7.74 |
| `Succession{.chain_after_reregister}` | 556,098 | 567,630 / 66 | 2.24 |
| `Succession{.kept_law}` / `{.fresh_ja}` / `{.page_unreadable}` | 193 / 8,651 / 28,458 | the same (25 / 36 / 42) | 0.50 (with the next row) |
| `Succession{.drain_reads, .barrier_unknown}` / `{+ .ja2}` | 6,924 / 107,567 | 5,669 / 83,958 (37 / 41) | (above) |
| `Succession{.m0_unlisted}` / `{.lapse}` | failed (D3) | **15,275 / 36; 163,312 / 44** | 0.54 (with the next row) |
| `Succession{.lapse, .kept_law}` | failed (D7) | **1,960 / 29** | (above) |
| `Succession{.lapse, .split_register}` (revision 18; estimate 0.3–0.7 M) | — | **194,251 / 48** | 0.89 |
| `Window{}`, `{.row_missing}`, `{.row_missing_frozen}`, `{.stale_recovering}` | 206,868 each | the same (62) | 2.92 |
| `Window{.volumes = 2}` / `{.all_elapsed}` | 520,764 / 536,516 | the same (79 / 63) | 2.08 / 2.17 |
| `Window{.volumes = 2, .vk_open_fails}` / `{.fair}` / `{.evicted_after_sibling}` / `{.lapse_evicted}` / `{.data_stall}` | 18,956 / 28,572 / 1,760 / 18 / 23 | the same | 0.18 |
| `Window{.colocated_x}`, trimmed (X's metadata write off) | over the bound | **184,453 / 70** | 1.76 (with the next row) |
| `Window{.lapse}`, trimmed (X's metadata write off, the ledger's data merge) | over the bound | **335,604 / 68** | (above) |
| `Window{.late_clear}` | failed (D10) | **3,643 / 30** | 0.02 |
| `Debt{}` / `{.bind_race}` | 893 / 4,661 | 813 / 3,233 (26 / 26) | 0.01 |
| `Debt{.ja, .op = .two}` / `{.op = .one}` / `{.unreadable, .op = .one}` / `{.all_elapsed, .op = .one}` | failed (D8) | **589,249 / 47; 780,616 / 50; 685,518 / 50; 1,436,868 / 48** | 15.00 |
| data model, both planes: `Death` / co-located / transient-absent | 15,364 / 95,277 / 32,145 | the same | ≤ 0.9 |
| data `Succession` / `+ leaver` | 3,209 / 17,491 (`.wero_holder` 2,973 / 25,890) | the same | ≤ 0.2 |
| data `Lapse` (and `.unlisted_joiner`) / data only (PTPL-less test) | 1,124,580 / 734,976 | the same (48 / 44) | 9.31 `.wero_holder` / 12.72 `.wear_preempt_all`; §DO's test 7.91 |
| data `Lapse`: slow / slow, data only / grace only (§DL test) | 1,917,758 / 1,572,834 / — | the same | 19.32 / 26.72 |
| data `Lapse{.meta_volumes = 2}` (§DN test) | 1,793,960 … 107,544 | the same | 31.72 / 43.11 |

`Succession{}` grows ×1.01 over revision 15. Revision 17's shorter lock holds block fewer
interleavings, so the count rises from revision 16's 300,414.

The data model is unchanged by revisions 16–18. Its `stuck()` and cycle check found nothing under
`.wear_preempt_all`: nothing needed triage, and no product defect was found.

**Holds rows:**

| Row | States |
|---|---|
| `reassert_gate = false`, `.page_unreadable` / `.drain_reads, .barrier_unknown` | 51,393 / 28,227 |
| W3 in `Succession{}` | 351,535 |
| W36 | 3,837 |
| the W23 halves | 202,556 / 256,732 |
| W43's companions | 1,530 / 1,674 |
| W38a, op 1 alone | 611,063 |
| W49 alone, atomic configurations | 168,520 / 354,005 / 8,651 / 1,960 / 28,458 (D12) |

**Full forms of the accepted trims** (one-off measurements, re-run by hand when the model family
changes; the `Window` family has not changed since):

- `Window{.colocated_x}`: 2,821,934 (depth 78);
- `Window{.lapse}` with the ledger's merge: 3,983,908 (depth 74; 4,299,151 before the review's P6/P8 fix);
- `reassert_gate = false` with two successors: 2,821,453 / 5,908,962 (re-measured after the review's fixes).

All hold. W9, W45 and A5 run on the full `.colocated_x`, and W17 and A3 on the full `.lapse`.

## Weakenings: observed first error against revision 19's acceptable sets

Every observed first error is inside its set.

| W | Configuration | Acceptable | Observed (len, states) |
|---|---|---|---|
| W1 | `Succession{}` | TwoManagers, WroteOverLiveClaim | WroteOverLiveClaim (36, 1,958) |
| W2 | `Succession{}` | OverwroteAcked, WroteOverLiveClaim | WroteOverLiveClaim (32, 4,087) |
| W3 | `Debt{.bind_race}` | ReallocatedUnfenced | ReallocatedUnfenced (23, 194) |
| W4 | `Window{}` | FencedMemberWrote, StaleDataWrite, ReallocatedUnfenced | FencedMemberWrote (46, 11,502) |
| W5 | `Succession{}`; `Death{}` | FencedWrote, TwoManagers; FencedHostRegistered | TwoManagers (30); FencedHostRegistered (32) |
| W6 / W10 | `Window{}` | ReconcileFencedLive | ReconcileFencedLive (8 / 10) |
| W7 / W8 | `Debt{}` | ReallocatedUnfenced | ReallocatedUnfenced (22 / 19) |
| W9 | `Window{.colocated_x}` | ReallocatedUnfenced, FencedMemberWrote | ReallocatedUnfenced (37, 52) |
| W11 | `Succession{}` | TwoManagers | TwoManagers (19) |
| W12 | `Succession{.kept_law}` | UnreservedUnderManager, OverwroteAcked | OverwroteAcked (22, 117) |
| W13 / W14 | `Window{}` / `Death{}` | FencedMemberWrote | FencedMemberWrote (46 / 16) |
| W15 | `Death{}` | ManagerFailStoppedUnproven | ManagerFailStoppedUnproven (31) |
| W16 | `Death{}`: refence, rejudge | FencedMemberWrote | FencedMemberWrote (19, 33) |
| W17 | `Window{.lapse}` | LiveFenced | LiveFenced (40, 1,213,572) |
| W18 | `Succession{.lapse}`; `{.lapse, .kept_law}` | OverwroteAcked | OverwroteAcked (27, 37,247; 25, 532) |
| W19 | `Succession{.m0_unlisted}` | TwoManagers | TwoManagers (29, 815) |
| W20 | `Succession{}` | PreemptedLive | PreemptedLive (32, 150) |
| W21 | `Succession{.fresh_ja}` | FencedWrote, TwoManagers | TwoManagers (30, 155) |
| W22 | `Succession{.chain_after_reregister}` | LiveFenced | LiveFenced (62, 17,217) |
| W23 | `Window{.row_missing}` | ReconcileFencedLive | ReconcileFencedLive (21, 128,633) |
| W24 | `Window{.lapse_evicted}`, `Strict.lapse` | LapseWindow | LapseWindow (12) |
| W25 | `Succession{.lapse, .kept_law}` | UnreservedUnderManager, OverwroteAcked | OverwroteAcked (26, 1,159) |
| W26 | `Window{.volumes = 2}` | LiveFenced | LiveFenced (32) |
| W27 | `Succession{.drain_reads}` | LiveFenced | LiveFenced (30, 100,373) |
| W28 | `Window{.row_missing_frozen}` | RecoveryStuck | RecoveryStuck (37) |
| W29 | `Window{.stale_recovering}` | LiveFenced | LiveFenced (12) |
| W30 | `Death{.joiner_key}` | FencedMemberWrote | FencedMemberWrote (38) |
| W31 | `Succession{.page_unreadable}` | ManagerFailStoppedUnproven | ManagerFailStoppedUnproven (39, 98) |
| W32 | `Succession{.drain_reads, .barrier_unknown}` | LiveFenced | LiveFenced (25, 3,949) |
| W33 | data `Lapse{.wear_preempt_all, .unlisted_joiner}` | LiveFenced | LiveFenced (26) |
| W34 | `Debt{.ja, .op = .one}` | LiveFenced | LiveFenced (17, 17) |
| W35 | `Succession{.chain_after_reregister}` | RegisteredButPending | RegisteredButPending (30) |
| W36 | `Window{.late_clear}` | holds | holds (3,837) |
| W37 | `Succession{.chain_after_reregister}`; `Debt{.ja, .op = .one}` | RegisteredButPending | RegisteredButPending (34, 319; 35, 1,063) |
| W38 | `Debt{.ja, .op = .one}` | RegisteredButPending | RegisteredButPending (33, 569) |
| W38a | `Debt{.ja, .op = .one}` | holds | holds (611,063) |
| W38b | `Debt{.ja, .op = .one}` | RegisteredButPending | RegisteredButPending (35, 1,271) |
| W39 | `Debt{.ja}`, both operations | ClearedAfterEstablish | ClearedAfterEstablish (33, 33): {0×3, 1×12, 2, 3×8, 5×5, 4×4} |
| W40 | `Debt{.ja, .unreadable, .op = .one}` | RegisteredButPending | RegisteredButPending (35, 1,271) |
| W41 | `Window{.data_stall}` | PendingPastDeadline | PendingPastDeadline (7) |
| W42 | `Succession{.drain_reads, .barrier_unknown, .ja2}` | RegisteredButPending | RegisteredButPending (31, 57,719) |
| W43 | `Window{.evicted_after_sibling}`, `rejudge = false` | FencedMemberWrote | FencedMemberWrote (21, 325) |
| W44 | `Succession{.drain_reads}` | LiveFenced | LiveFenced (32, 186) |
| W45 | `Window{.colocated_x}` | KilledCurrent | KilledCurrent (12) |
| W46 | `Succession{}` | WroteOverLiveClaim | WroteOverLiveClaim (28) |
| W47 | `Succession{}`; `Window{.fair}` | StrandedAtQuiescence | StrandedAtQuiescence (30; 22) |
| W48 | `Succession{.chain_after_reregister}` | LiveFenced | LiveFenced (31, 46): {1×15, 2, 3×7, 4×5, 8, 3, 3} |
| W49 | `Succession{.lapse, .split_register}`; with W54 `Succession{.lapse}`; alone in the atomic configurations | TwoManagers, UnreservedUnderManager; TwoManagers; holds | TwoManagers (30, 7,665): {0×8, 1, 3, 3, 5, 1×3, 3×4, 1×9, 3, 0}; TwoManagers (29, 6,226); holds (D12) |
| W50 | `Succession{.lapse, .kept_law}` | TwoManagers | TwoManagers (25, 1,285): {0, 0, 0, 1, 5, 0, 1×6, 0×7, 1×6} |
| W51 | `Debt{.ja, .op = .two}` | LiveFenced | LiveFenced (35, 974): {0×3, 1×12, 5×5, 4×3, 7, 2×10, 4} |
| W52 | `Window{.late_clear}` | FencedMemberWrote | FencedMemberWrote (25, 691): {0×6, 1, 1, 1, 2, 3, 0, 0, 0, 2, 4, 0, 1, 1, 2, 0×4, 1} |
| W53 | `Window{.late_clear}` | FencedMemberWrote | FencedMemberWrote (25, 691): {0×6, 1, 1, 1, 2, 3, 0, 0, 0, 2, 4, 0, 1, 1, 6, 0×4, 1} |
| W54 | `Succession{.lapse}` | TwoManagers | TwoManagers (30, 11,524): {0×6, 1, 3, 3, 5, 1×3, 3×3, 1×8, 3, 3, 0, 0, 1, 0} |
| W55 | `Succession{.lapse, .split_register}` | TwoManagers | TwoManagers (31, 12,736): {0×6, 1, 3, 3, 5, 1×3, 3×4, 1×8, 3, 3, 0, 0, 1, 0} |
| W56 | `Succession{.lapse, .split_register}` | TwoManagers | TwoManagers (27, 78,752): {0, 0, 0, 1, 3, 3, 5, 0, 1×6, 0×6, 1×6, 0} |
| W57 | `Succession{.lapse, .split_register}` | TwoManagers, UnreservedUnderManager | UnreservedUnderManager (33, 13,872): {0×6, 1, 3, 3, 5, 1×3, 3×4, 1×8, 3, 3, 0, 0, 0, 3, 3, 1} |

Accepted windows under `Strict`:

| Window | Configuration | First error (len) |
|---|---|---|
| A2 | `Debt{}` | DebtWindow (5) |
| A3 | `Window{.lapse}` | LapseWindow (52) |
| A4 | `Death{}`; `Window{.evicted_after_sibling}` | ReadmitWindow (14; 16) |
| A5 | `Death{}`; `Window{.colocated_x}` | RegisterWindow (32; 45) |
| A6 | `Succession{}` | ProbeInstant (32) |
| A9 | `Succession{.m0_unlisted}` | BlindWindow (29) |

`Window{.lapse_evicted}` holds under `Strict.lapse` (18 states).

**Where the bounded withdrawal applies (DC22, revision 17).** The wait and its `WITHDRAW_TIMEOUT`
alternative apply where a withdrawal can meet its own process's admitted write on another thread:
J.op beside J.beat in `WindowLateClear`. Elsewhere no withdrawal can meet such a write:

- the unsplit JA, J and J_D withdraw on the same thread that issues the write;
- JA.beat's only withdrawal follows a refused late `cleared`, and the beat's own terminal check
  latches first;
- JA2 issues no writes;
- the data arms never withdraw (DC23).

## Bound, trims, gate cost

**Trims applied.**

- **From the ledger:** `Window{.lapse}` merges X's data admit and land; `elapsed` is limited to the
  targeted words in the weakening runs, with the `.all_elapsed` companions as the as-built check; JA
  is split only where rule L's steps interleave; at most one unreadable read per word; the register's
  re-read is atomic (revision 17), except in `.split_register` (revision 18); one restart per run; M0's lapse grace as one report, in `.split_register` only.
- **Accepted by revision 16:** X's metadata write off in the as-built `Window{.colocated_x}` and
  `Window{.lapse}`; `reassert_gate = false` on its single-successor configurations.

**Ruled by revisions 18–19: `Debt{.ja}` split by operation** (the forms and counts above). Applied:

- op 2 alone;
- op 1 alone with M's data write off, and its `.unreadable` form;
- the `.all_elapsed` op-1 form, with N_tick = 1 (the fallback) and S1's data write off (a ledger
  trim).

The split hides two things. One is op 1 and op 2 interleaving on one data word, which is W39's
witness; W39 therefore runs on the full form. The other is M's data write beside JA's, which
`Debt{}` and `.bind_race` cover.

**Gate cost.** Revision 16 ruled no build option. Instead:

- The three heaviest data-model tests run as one test per plane.
- `tests/costs/squeezefs-unit.linux.tsv` was regenerated at revision 17 from that FULL run's 16
  unit shards, merged as `zig build costs` merges the unit binary's (`LC_ALL=C sort`), with the smoke
  table unchanged. The runners of revisions 18 and 19 accepted it, so it was not regenerated again; the new tests
  take the median guess until the next regeneration.

| Measurement | Value |
|---|---|
| Debug, sequential: `meta_fence_model` | 53.1 s over 31 tests; largest 15.00 s (the four `Debt` split forms) |
| Debug, sequential: `data_fence_model` | 154.7 s over 34 tests |
| largest single model test | 43.11 s (§DN, `.wear_preempt_all`, revision 18's run) |
| FULL `zig build test` wall | 1 m 25 s; 1 m 15 s at revision 17 with the table fresh |
| FULL wall in the first round, both planes in one test and no table rows | 2 m 28 s |

Every test is under the 900 s hang limit.

## The converged design (revisions 20–27)

Decision 0004 §7.6.4–§7.6.11 each record "no model change relative to `d0fae07`". The rounds
specified product mechanisms that later PRs implement with §8.1 units: the rule markers, the hold
check, the frozen-joiner kill, the takeover verbs, and their counters and tripwires. None of these
is in the model's device semantics.

The model mirrors revision 27's mechanisms, so the configuration and weakening tables above stand as
of `d0fae07`.

§7.4's trim ledger now records what the model over-approximates:

- **The re-judge gap** (revision 20). The model releases the registration lock at the re-judge's
  kill-and-release and does not model the registry flock, which the product's rule keeps through
  its withdrawal. So the model admits another rule adopting host A's registration before the
  withdrawal. No configuration reaches that state: JA.beat and JA2 run only where M0 is off, and M0
  is dead after the kill. A failure there would be a model artefact, fixed by modelling the flock
  across the gap.
- **MG's sections during a hold check after every co-located kill.** Revision 21 recorded this, and
  revision 22 withdrew it once the check stopped following a kill.
- **The model's U is the forced unmount's shape in mid-lapse** (revision 20). The plain unmount's
  wait is a §8.1 unit.

Revision 27 is approved by the review of revision 26 and has 0 open issues.

## The model's safety review (after revision 27)

The independent review of the model code, `~/tmp/pr1-model-review.md` at `d0fae07`, found no BLOCKER
and no MAJOR. Six MINORs and four NITs; each is fixed or recorded here.

**Fixed:**

1. **Minor 1: the holder after a PRKEY≠0 preempt.** On nvmet under All-Registrants, a key preempt
   runs `unreg_all_host_by_prkey` and sets no holder; a hold the victim had passes to the first
   remaining registrant (`pr.c:486-507`, `:261-291`; P5, P10). The model gave the sender the hold.
   `fenceKey` and W1's key preempt now leave the holder to `dropReg`'s hand-off. This was inert:
   `rec_fence` is M0's last step and nothing later reads the holder.
2. **Minor 2: Window's preempt-all lacked P8 and P6.** In `Window{.lapse}`, L could precede S's
   preempt-all, and S then took over an unreserved namespace from an unregistered host. The open
   now refuses there (`s_alive` false), as the product's preempt-all conflicts. The trimmed
   `Window{.lapse}` drops from 371,387 to **335,604** states, and the full form from 4,299,151 to
   3,983,908. Both still hold, and W17 and A3 are unchanged (40, 52).
3. **Minor 3: row 4's PR-mutation clause keyed on `sup_a`, which M0's own re-acquire clears.** "M0
   superseded" is now `sup_a` or a successor's bind after M0's own (`view[M0] < epoch`), a fact the
   re-acquire does not undo. This covers D11's shape: a successor bound with no hold, then M0's
   successful re-acquire. No result changed; the case stays unreachable, because S1 has no lapse
   path and never registers host B again.
4. **NITs: the generation bump and the victim order.** The preempt-all's generation now bumps at the
   command (`inc_gen`, `pr.c:664-666`), before the abort work, instead of at the drain's
   completion. Victims are now taken in registration order (`pr.c:530-547`) instead of host index.
   The `Succession` counts grow by up to ×1.05: `{}` 354,005, `.drain_reads` 592,242,
   `.drain_reads, .all_elapsed` 1,881,600 (5.9 % under the bound), `.chain_after_reregister`
   567,630. No first error changed. W22's and W27's states moved to 17,217 and 100,373.
5. **NIT: `replayError` skipped the root state's invariant,** which `explore` checks. It now
   checks it.

**Recorded in the trim ledger:**

- **Minor 4: M0's claim refresh is not modelled, and the bind is one step.** The product's lapse
  beat refreshes the claim by a blind put after DC6's check (`gate.zig:804-818`, `:847-869`). A
  successor's commit in that instant is overwritten, and the claim re-read then reads M0's claim.
  D11 then rests on the page check, the successor's step 11. The residue needs the successor frozen
  between its commit and its page write, a double fault of the class A9 already names.
- **Minor 5: the restart waits for every admitted write.** The model's L is a clean namespace
  disable. A target crash that loses admitted writes, which the host then requeues and re-admits
  after the restart, is A8's requeue class and is not modelled.
- **Minor 6: M0's `fence(K_B)` is one atomic step,** with no per-victim kill and unregister and no
  `pr_sem`. It cannot hide a row 4, because any landing after the bind already is one. Host B's
  P9b and P9c answers during that fence are unexplored; the successor's preempt-all explores them
  for every victim.
- **NIT: row 14 (`ClearedAfterEstablish`) is vacuous outside `Debt`'s synchronous arm.** Every other
  establishing site checks `pending` right after its own successful `clear()`, so only W39's site
  can fire. That matches §7.5.2; "checks L(c) directly" overstated it.
- **NIT: the data model's WEAR takeover is atomic.** P9's timing is the metadata model's job.

## Checks

These ran on kernel.org 7.3.1 (multipath=N), on the local venue, at the final code:

- FULL `zig build test`: 23/23 steps, 3,363 / 3,558 tests passed (195 skipped), 0 failed, in
  1 m 25 s. The runner accepted the cost table.
- `zig build rails`: 18/18 steps, 34/34 tests.
- `zig fmt --check`: clean.

Docs-only round (revision 27): decision 0004 copied verbatim; `zig fmt --check` clean;
`zig build rails` (fresh) 18/18 steps, 34/34 tests. No model or product code changed.

Review round (the model's safety review): FULL `zig build test` 23/23 steps, 3,363 / 3,558 passed (195 skipped), 0 failed, 1 m 28 s, the cost table accepted; `zig build rails`
(fresh) 18/18 steps, 34/34 tests; `zig fmt --check` clean.

## PR-3: deaths carry the host key from the appender page (decision 0003 §DO.18–§DO.23)

**What changed.** On today's WERO code a member a successor reaps at its grace deadline was shipped
with key 0 (`wire_plane.rosterKey`), so recovery skipped the metadata preempt and the data fence and
released the region with the member's host still registered; a member whose row was lost was never
expected and never recovered (decision 0004 §2.4, G-b). Now the appender page (layout 2) carries the
host's registration key (`host_key` @296, `FIXED_LEN` 304, the budget still 108), written by the
manager at region 0's bind and at `JoinAppender` from the joiner's claim, which is verified against
the volume's report (`JoinAppenderReq` 48 B). The successor's grace expects rows ∪ Live pages with
the pages' keys and reaps with them; same-host pages whose writer is proven dead ship through the
death sink at its bind; recovery takes the record's key, else the page's.

**The reds**, each failing first (on `134be36`, and before the rebase on `d807a60`):

| Red | On the base | At the head |
|---|---|---|
| F9 (loop): X reaped at M2's deadline | `appender_recoveries` 1, `appender_recovery_preempts` 0, `pr_fencer_preempts` 0, `data_pr_preempts` 0 (`fidelity.zig:2584`) | 1 / 1 / 1 / 1; host A's key gone; X's released windows refused (EBADE) |
| F9b: X's row dropped (`SQUEEZEFS_TEST_MEMBER_ROW_DROP`) | `appender_recoveries` 0 after 30 s: X's page stays Live (`:2647`) | 1, both fences, X's acked file read back through M2 |
| THE LOST ROW (wire plane) | no grace opens (`wire_plane.zig:2880`) | the page opens the grace; the reap's record stands; the region recovered |
| A SAME-HOST PAGE PROVEN DEAD | no death recorded (`:2917`) | recorded once, before the listener serves |
| 12 units (page v2/v1, 48 B, screen, `joinKeyVerdict`, `JoinAppender`, `arm`/reap/pre-dead, `deathKey`, recovery key and mismatch, region 0's key) | compile-red, 12 errors (`1d7d3b3` on `58e726a`) | green |

F9 and F9b read `appender_recovery_key_from_page` 0: the reap's record already carries the page's
key; the record-0 fallback is the recovery unit's.

**The harness.** The tcp restart legs crashed in the rig, not the product: `Rig.unshare` freed the
NQN the leg then handed to `expectNoResidue` (a fault in `Walk.find`). On `4179803` (no PR-3 code)
3 of the 5 legs crashed in one run; fixed in `213aa1c`, the five ×2 45/45.

**Checks** (kernel.org 7.3.1, multipath=N; Omarchy, the local venue; other agents' nvmet legs ran
beside these): before the rebase onto `bb75ca8` — at `a3a28d2`, the tier and the full smoke with
the rig fix `9665629` — the reds ×5, the 27 eviction / re-join / KILL rows ×3 (71/71 each), the
nvmet tier ×3 idle (74 passed / 1 skipped (multipath-tcp) / 0 failed each), the full smoke 186 /
199 (13 skipped). At the head: the
reds ×5 (units 55 / 57, 2 skipped — F9 and F9b, the nvmet tier's —; F9 and F9b 5/5, every run
`appender_recovery_preempts` 1, `pr_fencer_preempts` 1, `data_pr_preempts` 1); FULL `zig build
test` 23/23 steps, 3,384 / 3,581 passed (197 skipped), 0 failed; the nvmet tier ×1 idle and ×1
under `cpu+smoke` load (load ≈ 78) 74 / 1 / 0 each; the 27 rows ×1 71/71; `zig build rails` 18/18,
34/34; `zig fmt --check` clean. No hot path touched; no timing benchmark run.
