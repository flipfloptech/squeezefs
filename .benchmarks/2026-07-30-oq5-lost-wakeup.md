# OQ-5 lost-wakeup wedge — root cause, fix, and counted acceptance (2026-07-30)

Branch `fix/oq5-lost-wakeup` off dev `f16d5a9`. Correctness campaign — no
perf brackets owed. Closes **OQ-5** from
`.benchmarks/2026-07-29-probe-up-governor.md` §6b/§8.

## 1. Forensic inheritance

The probe-up-governor campaign's gate found
`staged_identity_visibility_tests::layout_transition_storm_reads_never_transient_zeros`
wedging ~1/40–1/200 runs when its 8-worker runtime is squeezed onto 2 CPUs
(`taskset -c 0-1`); never at full affinity. Always round-N **op 6** (the
staged→striped promotion) parked forever in `set_layout_and_size`'s 4a
`lock_inode_exclusive`. The inherited live-process forensics
(`~/squeezefs-evidence/pug-wedge-forensics/`): the stripe's RwLock
batch-semaphore with permits=0, zero live guards, one queued waiter with a
registered waker; the acquire/release tape showing the M6 **times-drain**
task's exclusive waiter popped fully-assigned (waker taken for `wake_all`)
and then **never re-polled** — its 536 M assigned permits dying with the
popped node; taskdumps showing the drain in the notified-but-never-run
empty-trace shape while other tasks keep running. Adjudicated there as "a
pre-existing tokio-runtime-layer lost task wakeup"; the standalone model
NOT reproducing was flagged as unexplained. **This campaign revises that
adjudication** (§3).

## 2. Reproduction (campaign currency)

Instrumented hunt: TEMP-DIAG stash applied to a scratch worktree
(`oq5-diag-scratch`, never merged) plus a **vendored instrumented tokio
1.53.1** (`[patch.crates-io]`, scratch-only) carrying an `oq5diag` event
tape: per-watched-task wake/schedule/poll transitions
(`transition_to_notified_by_{val,ref}` outcome, `schedule_local`
LIFO-vs-queue venue, `push_remote_task`, poll begin/end), unconditional
worker park/unpark, and batch-semaphore pop/registration events keyed by
waker data pointer. The times-drain task's header pointer is registered at
spawn.

8 lanes × 2 CPUs each (`taskset -c 2i,2i+1`, 32-CPU box, thermal governor
2.0–2.2 GHz): **4 catches within ≤12 runs/lane (~1/10 per run)** —
lane1_run12, lane3_run8, lane5_run10, lane7_run10 (archived at
`~/squeezefs-evidence/oq5-lost-wakeup/`). Unfixed baseline on the **stock
locked tokio 1.52.3** (pre-fix commit `1c62b04`, timeout-based detection):
**2 wedges / 200 runs (~1/100)** — consistent with the packet's 1/40–1/200
band.

## 3. Mechanism (revising the §6b adjudication)

All 4 instrumented catches show the **identical terminal tape signature**
for the times-drain task D:

```
SEM_ACQ_PENDING(D)            # D's exclusive acquire parks on the stripe (waker registered)
POLL_END_PENDING(D)           # D's poll ends mid-lock_many
SEM_POP_WAKE(D)   by tid R    # a reader's DLM shared-guard drop assigns D's final permits,
                              #   pops the waiter, takes its waker
WAKE_VAL_SUBMIT(D) by tid R   # transition IDLE→NOTIFIED succeeds — the wake is NOT lost
SCHED_LIFO(D)      by tid R   # schedule_local places D in R's worker's UNSTEALABLE LIFO slot
                              #   (LIFO placement never notifies another worker)
[silence forever]             # D is never polled again
```

Live-process proof (gdb on a wedged catch): the waking worker's
`Core.lifo_slot == Some(D's header ptr)`, `Core.tick` frozen at 13,
`lifo_enabled == true`; D's task-state word `0xcc` = `NOTIFIED |
JOIN_INTEREST`, refcount 3 — a queued-but-never-polled Notified; the worker
thread eternally **inside one poll** of the storm test's `reader_loop`.

**Where the wakeup dies:** it doesn't — it is delivered, into the waking
worker's LIFO slot, exactly as tokio documents. The starvation has two
composed causes:

1. **tokio's unstealable LIFO slot** (the documented footgun — tokio
   #4323 / #4941): a task woken from within another task's poll is placed
   in the waking worker's LIFO slot with **no notify**; the slot is not
   stealable, and it drains only when the current poll ends. Upstream
   history: #7431 made the slot stealable in **1.51.0**; it was
   **reverted in 1.51.2 / 1.52.2 (#8100)** for its performance impact —
   both our locked 1.52.3 and 1.53.1 have the unstealable slot.
2. **Our warm read serve had ZERO coop-budget leaves**: a warm all-RAM
   read (fresh/dirty moka metadata + staging-ring mmap serve) completes
   with every await Ready-immediately and **no tokio leaf consulting the
   coop budget** — a task looping warm reads never ends its poll, so the
   LIFO slot never drains. (tokio's cooperative scheduling only bounds
   polls whose leaves consume budget.)

The wedge composition: reader R's iteration takes/drops the wedged
stripe's shared DLM guard; the drop's semaphore release fully assigns the
parked drain D and wakes it into R's LIFO slot. The wedged writer (mid
op-6) leaves the ino's cache metadata dirty, so R's subsequent iterations
serve entirely from RAM — R's poll never returns, D starves holding ALL
the stripe's permits, and every later acquirer (the op-6 promotion — same
stripe, ino collision) parks on a permit-less semaphore. Deadlock complete.

**Why the standalone model never reproduced:** its reader loops used tokio
primitives per iteration (budget leaves) — their polls always ended and the
LIFO slot always drained. The missing ingredient was our budget-free warm
serve, not runtime scale or thread census.

**Why the 2-CPU squeeze selects it:** the drain's mid-`lock_many` parked
residency stretches from µs to ms–s under an 8-workers-on-2-CPUs schedule,
widening the window in which a *reader's* guard drop (rather than the
drain's own retry or another venue) is the assigning release; at full
affinity the alignment essentially never occurs (40/40 + historical gates
clean). Load selects the schedule; the defect was ours.

**Cross-proof (decisive):** the identical instrumented tree on vendored
**tokio 1.52.1** — the last release with the stealable LIFO slot, stub
tape — ran **800/800 squeezed runs clean** on the same recipe that caught
4 wedges in ~40 runs on 1.53.1. The wedge exists exactly where the LIFO
slot is unstealable.

## 4. Fix — class (a): our composition

`8e623f6` — `tokio::task::coop::consume_budget().await` at the top of
`DataRouter::read_file_range_zero_copy_with_meta` (the shared entry for
both public zero-copy read wrappers). One budget unit per read op bounds
every warm-read loop to one budget window (≤128 iterations) before its
task yields — the poll ends, the LIFO slot drains, the woken task runs.
Cost: one thread-local read+write per read — no locks, no copies, no
allocation (zero-copy/latch-free rules intact). On foreign threads with no
runtime context (IPC sync-lane serves) it is a no-op (unconstrained
budget) — zero behavior change there.

Not chosen:
* **(b) tokio pin/bump** — no upstream fix exists to pin: 1.53.1 (latest)
  reproduces; the stealable-LIFO change was deliberately reverted upstream
  for perf (#8100), and pinning the yanked-behavior 1.51.0–1.52.1 window
  would trade a documented footgun for a known performance regression and
  a dead-end version island.
* **(c) watchdog liveness nudge** — unnecessary once the composition is
  fixed; a re-wake would also have been a symptom patch (the wake was
  never lost — a nudge re-delivering into the same LIFO slot would not
  even have helped).
* **`disable_lifo_slot`** — unstable API, unavailable to `#[tokio::test]`
  runtimes, and taxes every message-passing pattern to defend against one
  non-cooperative loop shape.

No upstream issue filed: the mechanism is tokio's **documented** open
footgun (#4323/#4941, revert #8100); our forensics add no new upstream
information — the defect adjudicates to our non-cooperative task shape.

**Deterministic repro-port** (`1c62b04`, red-first):
`warm_read_loop_yields_to_peer_tasks_on_one_worker` — current-thread
runtime, task A loops warm zero-copy reads (cap 100 k), task B (spawned
second) sets a flag. RED without the fix: A completes all 100 k iterations
in ONE poll and B never runs. GREEN with it: A yields within one budget
window (observed ~130 iterations). This is the OQ-5 starvation property as
a per-commit cargo test.

## 5. Counted acceptance (from zero, final binary)

Recipe: the UNMODIFIED storm test, 2-CPU-squeezed lanes
(`taskset -c 2i,2i+1`, 8 lanes), wedge = run exceeding 120 s (nominal
completion 2–5 s squeezed; the wedge is permanent). All counts from zero
on the final commit's binaries:

| Binary | tokio | Runs | Wedges |
|---|---|---|---|
| pre-fix `1c62b04` (baseline) | stock 1.52.3 | 200 | **2** (~1/100) |
| **fix, final tip** | stock 1.52.3 | **200** | **0** |
| fix, final tip (extension) | stock 1.52.3 | 600 | 0 |
| fix applied to instrumented scratch | instrumented 1.53.1 (catch rate ~1/10 unfixed) | 200 | 0 |
| unfixed scratch (cross-proof) | vendored 1.52.1 (stealable LIFO) | 800 | 0 |

Honest rate math: at the packet's 1/40 the required ×200 alone gives
(1−1/40)^200 ≈ 0.6 % false-pass; at today's measured stock rate (~1/100)
×200 alone would leave ≈13 % — which is why the count was extended: 800
stock runs ⇒ (1−1/100)^800 ≈ 0.03 %, plus 200 runs on the instrumented
build whose unfixed rate was ~1/10 ⇒ (1−1/10)^200 ≈ 7×10⁻¹⁰, plus the
deterministic red→green pin of the exact starvation property. Combined
false-pass probability is negligible.

## 6. Gates (final tip, from zero)

Code-class gates ran from zero on the fix tree (`1467e52`); the one
commit after them (`6b416e8`) is a manifest-requirement-only edit (tokio
floor 1.36→1.48 — the lock stays pinned at 1.52.3, resolved graph
byte-identical) and carried its own manifest-class gate
(check + clippy + fmt, all clean).

* `cargo clippy --all-targets --all-features -- -D warnings` — clean.
* `cargo fmt --check` — clean.
* `cargo test --all-features -- --test-threads=1` — **exit 0, 0
  failures** (full suite incl. the write/read campaign contracts —
  write_pipeline, write_through_coverage, transport-lease, read-path —
  and the new OQ-5 pin; ~28 min wall).
* `cargo doc --no-deps` — clean.
* `cargo bench --benches -- --test` — exit 0 (criterion smoke).
* Loom (`tests/run_loom.sh`, `LOOM_MAX_PREEMPTIONS=3`) — **52/52**.
* **statfs ×10 loaded soak — 30/30 green, 0 hangs** (release build;
  looping fat release build in a second worktree as load, loadavg 8–35
  across rolls; 63–74 s/roll); fusectl residue sweep clean (`waiting=0`
  on every connection; one scratch mount left by the final roll's
  process exit was unmounted by hand and is not a request residue).

## 7. Open questions

* The class defense is per-entry: other future all-RAM hot loops (not
  demonstrated to exist today) would need their own budget leaf. A
  repo-convention note rides the fix comment; no speculative leaves were
  added (charter: narrow).
* The diagnostic apparatus (instrumented-tokio tape + stash) lives on the
  never-merged `oq5-diag-scratch` worktree branch and
  `~/squeezefs-evidence/oq5-lost-wakeup/`; the original stash@{0} is
  preserved untouched.
