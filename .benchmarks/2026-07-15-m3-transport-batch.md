# PR M3 acceptance — S2: batch COMMIT_AND_FETCH submits per drain (§5.3 D3.a)

| | |
|---|---|
| **Program** | metadata-throughput (`docs/design-metadata-throughput.md`), PR M3 — §5.3 D3.a (S2 transport submit economy) |
| **Branch** | `perf/fuse3-submit-batch` (off dev `ed481b5`): `c2deee4` (RED) → `2f5ec1a` (GREEN) → this report |
| **Box / rails** | same 3.5 GHz-capped box as baseline/M4/M7 (`scaling_max_freq` + performance governor verified, Tctl 51–68 °C all session); storms `taskset -c 0-15`; daemons caged `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0`; binaries `sqm3` (tip) / `sqm3pre` (dev @ `ed481b5`, fresh worktree build) — kill-pattern immunity; kills by PID only; unique sandbox `~/tmp/m3_transport_1784089147/` (artifacts preserved) |
| **Substrate** | file-backed sandbox on the btrfs-CoW home fs (baseline **A cow** class), default cadence — the sanctioned substrate for non-barrier throughput work (bracket flat ±9 %); syscall counts are substrate-independent by construction |
| **Session hygiene** | **every timed row is DIRTY-flagged** (M6/M7 precedent): the box carried the same steady co-tenant as M7 (22 root `juicefs` daemons, load 19.6–26.4 the whole window, Tctl ≤ 68 °C, zero decay over the session). The load-invariant evidence — strace syscall counts and `.stats` counters — is authoritative for this PR's gate, per the M3 mission spec |

## Verdicts up front

1. **The syscall gate passes with room: create-storm `io_uring_enter` 25.98 → 9.10 per create** (−65 % same-session pairing; **−71 % vs the baseline report's 31**, which was measured pre-M4/M5/M6/M7 — those PRs already shaved the op mix from 5.18 to ~5.0 fuse_ops/create). The design's target was ≤ 12–15. Unlink drops 32.96 → 12.57, stat 3.55 → 1.51 per op.
2. **The entire delta is the fuse3 queue workers**, per-thread attribution: `fuse-over-uring-*` threads 24.78 → 8.04 enters/create; the `squeezefs-uring-fs-*` pool reads **1.07 → 1.07** — see the uring_fs disposition below (already drain-batched; the design's "currently submit-per-message" premise was stale).
3. **R8 holds**: per-ent lease re-arm gating untouched (only the syscall is shared); `transport_parked_commits` delta **0** across every storm phase; `transport_leases_outstanding` returns to 0 at quiesce; the small-write zero-copy suites and the real-mount single-queue Q_DEPTH=4 storm (multi_queue suite) are green; `generic/074` (48 s write soak through the transport) **PASS**.
4. **No shape regressions**: journal entries/op byte-identical pre↔tip (create 1.005, rename 1.002–1.003, unlink 1.018–1.019 — the G4/M7 shape); fuse_ops/op unchanged; classical sideband alive (1.000/unlink — FORGETs still ride it by kernel mandate); `fuse_over_uring_cqe_errors` 0 everywhere.
5. **DIRTY timed rows are flat (±1.2 %, inside co-tenant noise)** — expected: the S2 win is CPU/syscall economy (~5–8 % op cost per the design's own estimate), invisible in wall-clock rows on a load-20+ box. The clean-pair debt rolls forward with M5/M6's (M9/M10 slot), unchanged in shape.

## Syscall table — `sudo strace -c -f` on the caged daemon, 30 k ops × 8 threads per phase

Fresh volume + mount per side; strace attached for the phase window only; counts
are the signal (strace slows the daemon ~4×, so the ops/s in these legs are not
rows). Per-op = total calls / 30 000.

| phase | syscall | dev @ ed481b5 | M3 tip | Δ |
|---|---|---:|---:|---:|
| **create** | **`io_uring_enter`** | **25.98** | **9.10** | **−16.88 (−65 %)** |
| create | `epoll_wait` | 15.53 | 15.49 | −0.04 |
| create | `futex` | 5.26 | 5.26 | 0.00 |
| create | `write` (eventfd wakes) | 19.22 | 19.21 | −0.01 |
| create | `read` (eventfd drains + sideband) | 15.94 | 12.09 | −3.85 |
| create | `fdatasync`/`fsync` | 0 | 0 | (all barriers via io_uring) |
| **stat** | **`io_uring_enter`** | **3.55** | **1.51** | **−2.03 (−57 %)** |
| **unlink** | **`io_uring_enter`** | **32.96** | **12.57** | **−20.40 (−62 %)** |
| unlink | `read` | 22.53 | 17.74 | −4.79 |

The `read` drop is the natural side effect of fewer worker wake cycles (each
cycle drains the eventfd); `write` (the session-side eventfd wake per reply,
one of the hardening invariants) is deliberately untouched.

**Cross-check (independent leg, 20 k creates, raw `-e trace=io_uring_enter`
event counting)**: totals 25.85 → 9.11 per create — agrees with the `-c` legs
to 0.5 %.

## Per-thread attribution — who owned the enters (create storm, 20 k ops)

`strace -f -e trace=io_uring_enter` with `/proc/<pid>/task/*/comm` mapping:

| thread family | dev @ ed481b5 (per create) | M3 tip (per create) |
|---|---:|---:|
| `fuse-over-uring-*` (32 queue workers) | **24.78** | **8.04** |
| `squeezefs-uring-fs-*` (8-worker pool) | 1.07 | 1.07 |
| main/other | 0.00 | 0.00 |
| **total** | **25.85** | **9.11** |

Worker economics per FUSE op: before — commit submit + poll re-arm submit +
`submit_and_wait` park ≈ 3 enters/op-side; after — everything pushed rides the
loop-bottom `submit_and_wait(1)` (one syscall = submission + wait) ≈ 1.6/op.

## `transport_commit_batch` — the new §9 histogram (M3 tip, 100 k-op storm sessions)

Whole-session (create→stat→rename→unlink→mfcreate→mfunlink), both runs:

| run | flushes | commits | mean | size 1 | 2 | 3 | 4 |
|---|---:|---:|---:|---:|---:|---:|---:|
| r1 | 2,100,928 | 2,299,143 | 1.094 | 1,905,989 | 191,836 | 2,930 | 173 |
| r2 | 2,101,911 | 2,299,432 | 1.094 | 1,907,894 | 190,703 | 3,124 | 190 |

Per phase (r1; r2 within 0.003): create **1.224**, mfcreate **1.275**,
stat 1.025, mfunlink 1.047, rename **1.000**, unlink **1.000**.
`commit_sqes/op`: create 5.00 (= fuse_ops — every reply over-uring),
unlink 4.02 (+1.00/op on the classical sideband: FORGETs), stat 1.00.

**Reading the histogram honestly (semantics note for the §9 alert row):** the
design table says "≈ 1 under load ⇒ batching regressed". That alert is about
the *submit-per-message* shape this PR removed, and the correct load signal is
the **syscall table above plus the phase means where coalescing is possible**:
with 32 kernel queues × Q_DEPTH 4 and the kernel spreading a one-dir storm
round-robin, per-queue concurrency is 1–2, so most flushes *can only* carry one
commit — while the *syscall* still amortizes commit + poll re-arm + wait into
one enter (the −65 % above). Batches > 1 form exactly where co-arrival exists
(create/mfcreate trailing FLUSH/RELEASE pairs: means 1.22–1.28, sizes up to 4);
rename/unlink are kernel-`i_rwsem`-serialized (§5.8) and read 1.000 by
construction. A future regression of this PR would show the syscall table
reverting and `flushes ≈ commits × 3`, not merely "mean ≈ 1".

## mdstorm rows — 8 threads × 100 k/phase, default cadence, interleaved pre/tip ×2 (ALL DIRTY)

Load 21.9–26.4 and Tctl 53–68 °C recorded per row in `results.tsv`; the M7
steady-co-tenant fallback applies (paired ratios minutes apart share their
contamination). ops/s:

| phase | pre r1 | m3 r1 | pre r2 | m3 r2 | Δ (medians) |
|---|---:|---:|---:|---:|---:|
| create (one-dir) | 7,068 | 7,056 | 7,126 | 7,042 | −0.7 % |
| stat | 265,224 | 267,740 | 260,859 | 268,990 | +2.0 % |
| rename | 5,480 | 5,438 | 5,478 | 5,438 | −0.8 % |
| unlink | 4,958 | 4,912 | 4,971 | 4,945 | −0.7 % |
| mfcreate | 30,492 | 30,044 | 30,161 | 30,148 | −0.8 % |
| mfunlink | 25,541 | 25,529 | 25,476 | 25,309 | −0.3 % |

Flat within DIRTY noise, as predicted for a syscall-economy PR under a
load-20 box (the design's own expectation was ~5–8 % *op cost*, i.e. CPU, with
"more under concurrency" — the concurrency here is throttled by the co-tenant).
Non-regression is the claim these rows support; the CPU-side win is banked by
the strace tables, which are load-invariant.

## Shape checks (`.stats` deltas per phase, pre vs tip — identical)

| metric | pre | M3 tip |
|---|---|---|
| `meta_kv_journal_entries`/op (create / rename / unlink) | 1.005 / 1.002–1.003 / 1.018–1.019 | same to the third decimal |
| journal B/op (create / rename / unlink) | 194 / 178–179 / 200 | same |
| `fuse_ops`/op (create / stat / unlink) | 4.992 / 0.998 / 5.011 | same |
| `transport_classical_sideband`/unlink | 1.000 | 1.000 (FORGET sideband alive) |
| `transport_parked_commits` delta | 0 | 0 |
| `fuse_over_uring_cqe_errors` | 0 | 0 |

## Verification

- **Red→green (TDD trail)**: `c2deee4` (RED) — `test_commit_pushes_defer_to_one_flush` failed at "pushes must stay queued" (SQ read 0: the per-message submit), `test_batched_push_sq_full_submits_and_continues` failed (tail flush empty), squeezefs `transport_commit_batch_stats_surface` failed (field absent). `2f5ec1a` (GREEN) — all pass. **Honesty note on unit-red scope**: ring-syscall *counts* aren't unit-observable, so the red tests pin the batching contract at the helper level (SQ occupancy, single-flush accounting, SQ-full flush-and-continue) against a real SQE128 ring + registered eventfd; the mount-level syscall claim is pinned by this report's strace evidence, and the re-arm gate ordering is deliberately *not* restated in new tests — it is already pinned by the `lease_core` unit tests, the loom `ent_lease` models, and the real-mount multi_queue storm (all green).
- **Full cargo gate at tip**: clippy `--all-targets --all-features -D warnings` clean; `fmt --check` clean; `cargo test --all-features -- --test-threads=1` exit 0 (all binaries green); `cargo doc --no-deps` 0 warnings; bench smoke 123 ok; loom 24/24 (lease protocol untouched; run as belt-and-braces for the wake-order change). fuse3 crate unit tests 14/14.
- **Zero-copy / small-write suites (explicit M3 verify-row item), run by name**: `small_write_zero_copy_tests` 3/3, `multi_queue_tests` 8/8 (includes the real-mount single-queue Q_DEPTH=4 small-file storm — the §5.4 parked/lease pin), `write_through_tests` 28/28, `data_path_correctness_tests` 27/27, `writeback_tests` 10/10.
- **Targeted fstests (root)**: `generic/074` **PASS** (48 s write/fsync soak through the batched transport).
- **Live smoke** (both pre-commit states): mixed create/write/read/unlink storms with concurrent 4–8 MiB writes (lease pressure): parked 0, leases outstanding returns to 0, clean unmount, daemon exit bounded.

## The uring_fs half of D3.a — disposition (deviation, with evidence)

§5.3 D3.a says the `uring_fs` process worker "currently submit[s]-per-message;
batch the drain the same way". **That premise is stale**: the worker loop at
`ed481b5` already admits in bursts (`try_recv` up to `ADMIT_CAP=256`), pushes
SQEs without submitting, and issues **one `submit_and_wait(1)` per drain** —
the `97d072e` "pipelined ring workers" lineage (module docs state it verbatim;
`push_slot` submits only as SQ-full backpressure). Measurement confirms:
**1.07 `io_uring_enter` per create on BOTH sides** (one journal-entry write per
create arriving as a lone message = one enter, irreducible request-response),
and the fuse3 workers own the entire 25.98→9.10 delta. Per the no-dead-code
rule, no mechanism was added to re-batch an already-batched drain; the
baseline's 1.28 % `crossbeam recv` share is parked-thread wake churn (futex),
not submit traffic, and is not S2 material. Recorded here so the design doc's
premise doesn't get re-chased.

## Wake-ordering hardening (required by the deferral; closes a pre-existing window)

Deferring the wake-fd PollAdd re-arm to the loop-bottom flush exposed an
ordering hazard: consuming an eventfd wake *after* scanning its producer, with
no armed poll, could strand that producer until an unrelated event. The eventfd
drain therefore moved to the **top** of the loop pass — sound because every
producer publishes state before writing the eventfd (submit_reply: channel send
→ write; lease drop: refs release → write; shutdown: active store → write), and
a wake landing after the drain leaves the counter nonzero, completing the
level-triggered PollAdd the moment `submit_and_wait` arms it. The same
consume-after-scan window existed in the old loop between its drain and
`submit_and_wait`; it is now closed rather than narrowed. Worker-exit flushes
any SQEs a truncated last pass left queued (teardown never drops an applied
reply); unmount storms + the 074 soak + suite teardown tests exercised it.

## Hand-offs

- **M10 (S3 SQPOLL)** now measures against the batched shape — its marginal
  value question ("does SQPOLL beat one enter per wake?") is honestly framed
  for the first time. The per-queue-ring builder remains SQPOLL-less.
- **The M4 vdso/clock residual (~2.2 %)**: NOT moved by this PR by design —
  the per-pull `pop_timeout` tokio timer is session-task-side, this PR is
  queue-worker-side. The only adjacent movement is the eventfd `read` count
  (15.94 → 12.09/create). Still M10-session material, as M4 recorded.
- **Clean quiet-slot mdstorm pairs** (M5/M6/M7 debt): the box never quieted
  this session either (22 juicefs daemons, load ~20 flat); the debt shape is
  unchanged and rolls to the next genuinely quiet slot.

## Artifacts

`~/tmp/m3_transport_1784089147/` (preserved): `results.tsv` (+ voided attempt-1
rows: harness bug — `mf` parent dir not pre-created; fixed, session restarted),
`stats/{pre,m3}_strace/` (per-phase `strace -c` + `.stats` snapshots + thread
maps), `stats/{pre,m3}_attrib/` (raw `io_uring_enter` event traces + summaries),
`stats/storm_{pre,m3}_r{1,2}/` (per-phase `.stats`), `logs/*.daemon.log` (all
mounts show FUSE-over-io_uring armed), harness (`lib.sh`, `run_strace_phase.sh`,
`run_thread_attrib.sh`, `run_storm_session.sh`, `sum_attrib.py`, `mdstorm.c`),
binaries `bin/sqm3` / `bin/sqm3pre`. Dev-control worktree
`~/tmp/m3_devtip_worktree/` removed after the session.
