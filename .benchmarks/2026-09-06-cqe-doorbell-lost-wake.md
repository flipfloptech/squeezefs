# 2026-09-06 — the cqe doorbell's wake-collapse latch lost a wake: the era flag consumed by an already-reaped completion

| | |
|---|---|
| **Branch** | `fix/cqe-doorbell-lost-wake` off `dev` df938b0d |
| **Commits** | `4d8b0fe7` (red: loom model + the test's mask removal) · `d66e6a21` (fix: the mark-valued latch) · docs commit |
| **Trigger** | batch gate 2026-09-06 11:50 (pinned quiet worktree, `--all-features -- --test-threads=1`): `tests/ipc_op_economy_tests.rs::parked_reaper_is_woken_by_completion_cqe_wake` failed at line 788 — `ipc_cqe_wake_writes` did not move while `woke_in < 2 s` PASSED; 1 in ~5 gate runs, 20/20 in isolation |
| **Class** | lost wake in a lock-free protocol whose loom models claim "never stranded" (the sqz-sync lesson class, design-il-wake-economy §Risks row 1); masked in the field by the reap park's age bound — a latency tail, never a hang |
| **Protocol** | `crates/squeezefs-ipc/src/cqe_core.rs` — `CqeDoorbell` v3 (`ef8c3945`, design-il-wake-economy Lever 1), now mark-valued |

## 1. Two findings, not one

The gate failure and the protocol bug are DIFFERENT shapes. Both are closed.

### 1a. What the gate saw: the counter assertion over-specified WHEN

The test's reaper published its op, then looped `while seq == seq0 {
futex_wait(seq_word, seq0, 1 s) }`, then read `METRICS.ipc_cqe_wake_writes` and
asserted it had moved. The daemon's completion order is
(`SlotCompletion::complete`, `src/ipc_host.rs:445`):

```
slot.core.complete()          DONE publish
cqe.complete()                seq bump → fence → parked gate → mark → latch     ← the reaper can wake HERE
METRICS.ipc_cqe_wake_writes += 1
futex_wake(seq_word)
```

A reaper that observed the bump WITHOUT sleeping — the warm service thread was
mid-pass and served the op inside the reaper's `futex_wake(doorbell)` syscall +
two `Instant::now()` calls, or the kernel's `FUTEX_WAIT` admission returned
`EAGAIN` on the just-moved word — races the daemon's four cache-missing steps
between the bump and the increment (`parked.load` on the line the reaper's
`park_end` just dirtied, `wake_at.load`, the latch RMW, the `Align64` counter
line). A reaper that DID sleep is woken by the `futex_wake` that follows the
increment and always sees it. So the observed failure (`< 2 s` PASSED, counter
unmoved) is the not-slept leg reading the counter early: the wake WAS paid; the
law ("a completion toward a parked reaper pays and counts a wake") holds; the
assertion asserted the instant, not the law.

Fix (`4d8b0fe7`): the counter check waits bounded (≤ 2 s spin) for the
bookkeeping to land.

### 1b. What the test could never see: the era latch consumed by a reaped completion

Reading the protocol for the shape the task hypothesised found a REAL strand
the test's 1 s bound masked and its `wake_writes` assertion would have
accepted (a useless wake counts).

**The v3 protocol.** Parker: `parked += 1` → `fence` → **`wake_paid = 0`** →
`snap = seq` → `wake_at = snap + k`. Completer: `seq += 1` → `fence` →
`parked == 0 ⇒ Elided` → `wake_at ≤ seq` else `Elided` → **`CAS wake_paid
0→1`**: win ⇒ `Wake`, lose ⇒ `Collapsed`.

**The era argument's gap.** design-il-wake-economy Lever 1, strand case 1:
"*bump visible to the snapshot*: the completion's DONE publish precedes its
bump, so the parker's mandatory post-snapshot re-scan finds the DONE — the
parker never sleeps behind it." That is true only for ops in the parker's
PENDING SET. The daemon publishes DONE first and bumps the doorbell after
(§1a's order), so a reaper that consumed an op off its slot word IN THAT GAP
— the shim's `getevents` pass reaps `ticket_done` slots, then parks for the
rest — or a sync-lane `pread` on the same session (slot-word wait, never the
doorbell) leaves a completion whose bump has nothing for the scan to find.

**The interleaving** (single parker R, D1 = the already-reaped op's outstanding
doorbell completion, D2 = R's pending op):

```
R:  parked = 1; fence
D1: seq = 1; fence; parked == 1 → proceed; at = 0 (stale init mark) → reached
R:  wake_paid = 0 (the era clear)
R:  snap = 1                        ← D1's bump ABSORBED by the snapshot
R:  wake_at = 2
R:  re-scan pending: only D2's op, not done
R:  FUTEX_WAIT(seq, 1): seq == 1 → queued (asleep)
D1: CAS wake_paid 0→1 → Ok → Wake → FUTEX_WAKE           ← paid toward R; R re-checks seq == 1 → re-waits
                                                           (or: D1's wake lands BEFORE R queued → lost outright)
D2: DONE; seq = 2; parked == 1; at = 2 → reached; CAS 0→1 → Err → Collapsed
R:  asleep with seq = 2 ≠ 1 and no wake coming → sleeps to its age bound
```

Loom finds it at iteration 2702 with the futex modelled at kernel fidelity
(`ipc_cqe_latched_reaped_prior_completion_never_strands`, `loom-models/src/lib.rs`):

```
latched cqe strand: reaper queued on snapshot 1, both completions applied (seq 2),
and no wake reached it — the era's one wake was consumed toward a parker not yet
asleep (parked 1, wake_at 2, latch 1)
```

The `latch = false` control arm (the pre-campaign wake-per-mark-passed body)
has no such strand: D2 pays unconditionally. The bug is the latch's.

**Why the existing models passed.** `ipc_cqe_latched_parked_reaper_never_stranded`
has both ops in the reaper's pending set (a bump absorbed by the snapshot is
always found by the scan), and its era-scoped witness is RESET right after
`park_begin` — so D1's late `Wake` counts as the era's one wake and D2's
`Collapsed` reads as the latch engaging. The design's soundness note for that
reset ("a Wake whose seq bump precedes the parker's snapshot fails the parker's
admission") is backwards: a bump the snapshot absorbs PASSES the admission; it
is the scan that covers it — for pending ops only.

**Field consequence.** The shim's reap loop (`crates/squeezefs-preload/src/interpose.rs`)
parks with `wait_any(entries, bound)` where the bound is `RING_PARK_RECHECK`
5 ms (ring-only sparse), `KERNEL_LANE_SLICE` 1 ms (both lanes) or the reap
quantum 50 µs (deep / batch-marked). A stranded reaper sleeps that long, then
re-scans and finds its completion: a p99 tail, never a hang — exactly the
latency shape the task predicted. Window: the daemon thread descheduled
between a slot's DONE publish and its doorbell bump while the reaper reaps
that slot and parks for another (µs on a loaded box), or any sync-lane
completion landing during a libaio reaper's park.

## 2. The fix — a mark-valued latch (`d66e6a21`)

The 4th doorbell word records the **mark the pay satisfied**
(`wake_paid_mark`), not a per-era flag:

- **Completer**, latch arm: `fetch_update(|paid| if paid == at { None } else { Some(at) })`
  — `Ok ⇒ Wake`, `Err ⇒ Collapsed`.
- **Parker**: no write to the word at all (the v3 clear is gone). Compose gains
  one rule: an at-snapshot mark (`cur == snap`) whose pay is recorded is SPENT
  and the new parker's own mark replaces it; unpaid it stays (its completer may
  still be in flight before its mark read — replacing it would elide the wake
  its parker sleeps on).
- **Initial value** `NO_PAY_RECORDED = u32::MAX` — never the mark word's own
  initial 0: a completer reading the mark word before the first parker's store
  (stale-init, at-or-behind the seq ⇒ reads-as-reached) must over-pay one wake
  (the documented benign class), not collapse against a pay nobody made.

**Why it is strand-free.** A pre-snapshot completer paying late read a STALE
mark: the parker's mark store follows its snapshot, which follows that
completer's bump, so a fresh read sees a mark AHEAD of that completer's seq and
elides it as unreached; a stale read records a mark the parker never set. Either
way the record never equals the parker's mark, and the k-th post-snapshot
completion — which reads the parker's mark (its bump follows the admission the
mark store precedes) — finds `paid ≠ at` and pays. A new era is payable by
arithmetic: a pay for its mark needs the seq to have reached it, which the
snapshot precedes — no clear needed, hence no clear a late CAS can consume. Two
completers racing past one mark pay once (the record is the mark, so the second
finds it whichever seq it bumped — a seq-valued record was tried first and
re-paid when the later bump won the update; rejected).

**Control arm untouched.** `latch = false` returns before the record; the
compose reads a record that arm never writes (`NO_PAY_RECORDED ≠` any mark), so
the A/B control is the shipped body verbatim, compose included.

**The wake-economy law holds.** Wakes are paid only toward parked reapers
(the `parked` gate is unchanged); an era still pays ≤ 1 syscall
(`ipc_cqe_wake_collapsed` counts the rest); `writes/(writes+elided+collapsed)`
keeps its meaning.

## 3. Loom evidence

`tests/run_loom.sh ipc_cqe` (LOOM_MAX_PREEMPTIONS=3, release):

| model | v3 flag latch (`dev`) | mark-valued latch (fix) |
|---|---|---|
| `ipc_cqe_parked_reaper_never_stranded` | ok | ok |
| `ipc_cqe_batch_parked_reaper_never_stranded` | ok | ok |
| `ipc_cqe_latched_parked_reaper_never_stranded` | ok | ok |
| `ipc_cqe_latch_two_parkers_covered` | ok | ok |
| **`ipc_cqe_latched_reaped_prior_completion_never_strands`** (new) | **FAILED** (iteration 2702, the state above) | ok |

**Model fidelity.** The new model models `FUTEX_WAIT`/`FUTEX_WAKE` with the
kernel's hash-bucket lock (`FutexBucket`): admission reads the word under the
lock and queues; a wake delivers only to a queued sleeper; a delivered wake whose
seq still equals the sleeper's snapshot is spurious — the reap loop re-checks
and re-waits, modelled as the pessimistic immediate re-wait (a bump landing
between the wake and the re-wait could only free it sooner, so real strand ⇒
model strand). Strand = still queued after every completer finished.

**Weakening ledger** (each applied to the fixed body, models run, then
restored):

| weakening | result |
|---|---|
| (a) drop the daemon-side Dekker `fence(SeqCst)` in `complete` | `ipc_cqe_parked_reaper_never_stranded` + `…batch…` FAIL |
| (b) drop the parker-side fence in `park_begin_batch` | same two FAIL |
| (c) initialise the record to the mark word's 0 | same two FAIL — the plain-load admission those two models keep is what lets a completer read the mark word stale under loom's weaker-than-C++ SC; under C++ SC the sleeper's mark store precedes the admission the completer's bump follows, so the stale read is loom's, but it is exactly what gives (a), (b) and the sentinel their teeth |
| (d) keep the spent at-snapshot mark | all five models ok — no model covers the two-parker cross-pending-set shape; the unit test `compose_replaces_a_spent_at_snapshot_mark` is the pin |
| (e) restore the v3 flag latch | the new model FAILS (the red) |

The two-parker `payable_or_pending` predicate in `ipc_cqe_latch_two_parkers_covered`
became `wake_paid_mark != wake_at || !reached(seq, wake_at)` (payable = the
live mark's pay is not recorded, or the mark is ahead).

## 4. Stress rows

All-features debug test binary, one test filtered, `--test-threads=1`, N runs
with M `yes > /dev/null` loads on the 32-CPU dev box (`/tmp/cqe-stress/run.sh`).

| binary | test | loads | runs | failures |
|---|---|---|---|---|
| `dev` protocol, `dev` test (1 s mask) | parked_reaper | 8 | 300 | 0 |
| `dev` protocol, `dev` test | parked_reaper | 40 | 300 | 0 |
| `dev` protocol, NEW test (30 s bound, shim-shaped re-park, counter poll) | parked_reaper | 8 | 100 | 0 |
| fixed protocol, new test | parked_reaper | 8 | 300 | 0 |
| fixed protocol, new test | parked_reaper | 40 | 300 | 0 |
| fixed protocol, new test | whole suite, default `--test-threads` (parallel) | 0 | 50 | 0 |

The stress does NOT discriminate: neither §1a's counter race nor §1b's strand
reproduced in 1,000 runs here (the gate's 1-in-5 was on a different box
state). The red is the loom model; the stress rows are the "no regression, no
new flake" evidence, and the parallel-mode row confirms `c2f9c905`'s `serial()`
guard covers this test's global-counter window (all five tests in the file take
it).

## 5. Test change (`4d8b0fe7`)

`parked_reaper_is_woken_by_completion_cqe_wake`: the futex bound is **30 s**
(a lost wake now sleeps past the 2 s assert instead of hiding behind the 1 s
bound); the stand-in reaper takes the shim's exact shape — park → mandatory
pending re-scan → wait → `park_end` on EVERY return → a spurious return
re-scans and re-parks a NEW era (the protocol contract; the old loop re-waited
blindly without `park_end`, which the module docs forbid); the counter
assertion polls bounded for the daemon's bookkeeping (§1a).

## 6. Residual, named

The two-parker CROSS-pending-set shape (parker A asleep on mark `m`; its
completer `C_m` has bumped but not yet read the mark; parker B on another ctx /
sync lane parks with `snap == m`, keeps the unpaid at-snapshot mark; `C_m` pays
and its `FUTEX_WAKE` lands before B's admission): B sleeps and `C_{m+1}` collapses
against the pay for `m`. The v3 protocol had the identical window (the clear
before `C_m`'s CAS); the fix preserves it rather than trading it for A's delay
(replacing the unpaid mark would elide `C_m` toward A). Bounded by B's age
bound; the real client's re-park after any spurious wake sees the pay recorded
and replaces the spent mark. Not modelled (a kernel-fidelity two-parker model
goes red on exactly this pre-existing class); the "no permanent strand"
contract of `ipc_cqe_latch_two_parkers_covered` still holds for the shapes it
models. Closing it needs a second word (a per-parker snapshot the completer can
test the pay against) — an IPC_ABI-bumping follow-on if a field row ever
prices it in.

## 7. ABI

No layout change: the doorbell stays 16 bytes at header line 2, every offset
unchanged; the 4th word's MEANING and initial value changed. `IPC_ABI` stays 6
— the skew guard for a word's meaning is KD-7 build-commit equality at HELLO
(`ipc_bind_refused_version`), which already refuses a mixed daemon/shim pair.
The `layout.rs` version ledger records the revision under v6.
