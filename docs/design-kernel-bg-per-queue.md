# Kernel COMMIT-lock split — per-queue FUSE background accounting (sqz patch 0026, 7.2 track)

**Status (2026-09-06): PATCH AUTHORED + COMPILE-PROVEN, boot + A/B OWED.**
`docker/kernel-sqz/patches-7.2/0026-sqz-fuse-uring-per-queue-bg-accounting.patch`
— the first patch AUTHORED on the 7.2 track (every earlier 7.2 patch is a
rebase). Compile-proven three ways on linux-7.2.3 + 0001–0025 (§4); the
26-patch chain re-verified `patch -p1 --fuzz=0` on a fresh base. Not
booted: the box that boots it is the user's call, and the lever holds its
slot on the field A/B row (§5) — never on this document.
**Authority:** the perf law (AGENTS.md — a lever lands on counted A/B
evidence; "the sqz kernel series is a first-class product surface",
ruling D13: portability governs CPU/topology, never kernel version) + the
R-4 board item (`.benchmarks/2026-09-03-r4-reap-thread-economy.md` §7
item 1: "per-CPU/per-queue background accounting in patches — expected
−1.5…−2 µs/op on the worker and on every fio thread's submit path").
**Companion documents:** `docker/kernel-sqz/SERIES.md` (the 0026 row +
paragraph, the compile-proof record), `docker/kernel-sqz/README.md` (the
7.2-track note), `docs/design-e2e-perf-audit.md` §3.3 row 8b / §5.3 row 20
(the ladder pointer), `.benchmarks/rigs/2026-09-06-kernel-bg-per-queue-ab.sh`
(the A/B rig), `docs/design-mw-multipath-kernel.md` (the campaign-shape
precedent: finding → mechanism → correctness → backport ledger → validation).

---

## 0. The finding (measured; not re-derived here)

R-4 step 1 (`.benchmarks/2026-09-03-r4-reap-thread-economy.md` §2 —
squeeze-test, **6.19.14-sqz**, kern rand-4k 24 jobs × qd8 libaio
`direct=1`, FUSE-over-io_uring 32 × 32, zc + kmbuf negotiated, `perf
record -e cpu-clock -F 4999` on the 32 `f3-ur*` worker threads mid-row):
the queue worker costs **17.7 µs per op, 63 % kernel**, and its largest
kernel class is **syscall entry + LOCK CONTENTION, 18.1 % = 3.20 µs/op**,
of which **1.9 µs/op is pure spinlock contention**:
`native_queued_spin_lock_slowpath` **7.8 %** + `_raw_spin_lock` 3.1 % of
the worker's cycles, callers **`fuse_uring_req_end` (3.8 %)** and
**`fuse_request_end` (3.1 %)** under `fuse_uring_commit_fetch` ←
`io_uring_cmd` ← `io_submit_sqes`. Every COMMIT_AND_FETCH ends a
background request under a lock the 24 submitting fio threads and the
other 31 workers also take.

The design read on `linux-7.2.3` (`fs/fuse/dev_uring.c`, `dev.c`,
`fuse_dev_i.h`) names the lock: **`struct fuse_chan` owns the background
accounting** — `max_background`, `num_background`, `active_background`,
`bg_queue`, `blocked`, all under ONE `spinlock_t bg_lock` — and
`fuse_chan` is 1:1 with the connection, so the ring's per-CPU queues all
share it:

* submit — `fuse_uring_queue_bq_req()`: `queue->lock`, append to
  `queue->fuse_req_bg_queue`, then **`spin_lock(&fch->bg_lock)`:
  `num_background++`, `blocked = (num == max)`, `fuse_uring_flush_bg()`**,
  nested inside `queue->lock`;
* end — `fuse_uring_req_end()`: `queue->lock`, and if `FR_BACKGROUND`:
  `queue->active_background--`, **`spin_lock(&fch->bg_lock)`:
  `fuse_request_bg_finish()` (clears `FR_BACKGROUND`, `num--`, `active--`,
  the `blocked` wake logic on `fch->blocked_waitq`) +
  `fuse_uring_flush_bg()`**; then `fuse_request_end()`;
* `fuse_uring_flush_bg()` already carved out "allow one bg request per
  queue, ignoring global fc limits" — the locality idea this patch makes
  general.

Why the ledger shows TWO callers: on **6.19.14** (the field kernel)
`fuse_request_end()` performs the background finish INLINE under
`fc->bg_lock` (the `fuse_request_bg_finish()` split is 7.x), so a uring
background end took `bg_lock` twice — once in `fuse_uring_req_end()` for
the flush, once in `fuse_request_end()` for the finish. On 7.2.3 the two
are one acquisition nested in `queue->lock`; the contention population is
the same 24 submitters + 31 workers.

## 1. Mechanism (patch 0026)

Each `struct fuse_ring_queue` gets its own background ledger under the
`queue->lock` it already holds on both paths, and a per-queue budget:

| Field / function | What |
|---|---|
| `queue->num_background` (new) | requests charged to this queue (`FR_BG_URING`) |
| `queue->active_background` (existed) | the subset moved `fuse_req_bg_queue` → `fuse_req_queue` |
| `queue->bg_blocked` (new) | this queue's admission gate |
| `queue->bg_waitq` (new) | this queue's EXCLUSIVE waiters |
| `fuse_uring_bg_max(queue)` | the budget: `max_background / nr_queues`, remainder to the lowest qids (the shares sum to `max_background` exactly whenever it is ≥ `nr_queues`), floor 1 — derived at each admission from `READ_ONCE(fch->max_background)`, no cache |
| `fuse_uring_queue_bq_req()` | charges the QUEUE (`num++`, `bg_blocked = num >= bg_max`), flushes against the queue's budget — **no `bg_lock`** |
| `fuse_uring_flush_bg()` | admits while `active_background < bg_max`; the `lockdep_assert_held(&fch->bg_lock)` is gone with the lock |
| `fuse_uring_bg_finish()` | on end: uncharge, re-open the gate when `num < bg_max`, wake ONE exclusive waiter (the classical chain verbatim, `waitqueue_active()` argument included), the drain-time `wake_up_all()` (§2.1), flush — **no `bg_lock`** |
| `fuse_uring_bg_uncharge()` | the credit alone (clears `FR_BG_URING` + `FR_BACKGROUND`, `num--`, `active--`) — the abort/teardown paths' half |
| `fuse_uring_bg_wait()` | `fuse_get_req()`'s per-queue gate: an exclusive `wait_event_state_exclusive` on the submitting task's queue (`fuse_uring_task_to_queue()`, the queue the submit would land on) for `!bg_blocked || !connected` |
| `fuse_uring_bg_kick()` | a slot the woken task will not use (allocation failure, request dropped unsent) is passed on — lockless, the waiter re-checks |
| `fuse_uring_bg_limit_changed()` | fusectl `max_background` writes re-gate every queue against its new share, outside `bg_lock` |
| `fuse_uring_num_background()` | `fuse_chan_num_background()` = the classical ledger + a lockless sum of the queues (fusectl, the readahead/writeback congestion checks — never the request path) |
| `FR_BG_URING` (new internal request flag) | marks the ledger a request was CHARGED to (§2.3) |

`fuse_block_alloc()` keeps the connection-level gates (INIT, ring
readiness) on `fch->blocked_waitq`; the classical `fch->blocked` governs
only while the ring is not ready (`for_background && blocked &&
!fuse_uring_ready()`), and a background allocation on a ready ring then
takes the per-queue gate. The classical `/dev/fuse` path (no ring) is
verbatim — `fuse_request_bg_finish()` becomes `static` to `dev.c`.
`fch->max_background` stores become `WRITE_ONCE` (fusectl, abort's
`UINT_MAX`) to pair with the queues' lockless reads. Zero uapi change,
zero Kconfig change; 5 files, +335/−44.

## 2. Correctness — the five points, preserved and reasoned

### 2.1 No lost wakeups

Waiters sleep EXCLUSIVELY on the queue's `bg_waitq`, so a `wake_up()`
passes exactly one slot on — the chain `fuse_request_bg_finish()` has
always used. Every gate transition set → cleared happens under
`queue->lock` and is followed by a full `wake_up()`; the not-blocked arm's
`waitqueue_active()` wake is safe for the reason the classical code
gives: a waiter only sleeps after seeing the gate set (its condition is
`!bg_blocked || !connected`), so a waiter the transition's `wake_up()` did
not see observes the clear instead and never sleeps.

**The one NEW hazard, closed explicitly:** a woken waiter is placed by the
CPU it runs on and may resubmit on ANOTHER queue, so a queue's wake chain
can run dry with waiters left behind (n in flight, w > n waiters: after n
completions nobody is left to hand a slot on). The last charged request
out of a queue therefore wakes them all —
`fuse_uring_bg_finish()`: `if (!num_background && waitqueue_active(&bg_waitq))
wake_up_all(&bg_waitq)`. At that instant `bg_blocked` is false (0 <
bg_max), so any late arrival sees the open gate; only waiters already on
the list are affected and `waitqueue_active()` sees them. Overshoot stays
bounded the way the original bounds it: one wake per completion.

### 2.2 Abort with requests queued per queue

`fuse_chan_abort()` raises `max_background` to `UINT_MAX` (now
`WRITE_ONCE`) before `fuse_uring_abort()`, so
`fuse_uring_abort_end_requests()`'s flush admits every queued background
request into `fuse_req_queue`; under the same `queue->lock` the queue is
stopped, its gate opened and its waiters released (`wake_up_all`). Every
path that ends a charged request WITHOUT `fuse_uring_req_end()` —
`fuse_uring_abort_end_queue_requests()` (`fuse_req_queue` →
`fuse_dev_end_requests()` → `fuse_request_end()`) and
`fuse_uring_entry_teardown()` (requests still attached to ents) — credits
the queue under `queue->lock` FIRST (`fuse_uring_bg_uncharge()`), so
`num_background` and `active_background` return to zero and the
connection ledger, which was never charged for them, is never debited.
(The naive split — charge the queue, let `fuse_request_end()` credit the
connection — would have UNDERFLOWED `fch->num_background` on every
aborted uring background request; `unsigned`, so the congestion checks
would then read "congested" for the connection's remaining life.) A
request submitted after `stopped` is set is refused before it is charged.

### 2.3 `FR_BACKGROUND` clear vs `fuse_request_end()`'s re-check

The per-queue credit clears `FR_BACKGROUND` under `queue->lock` in the
same task, before `fuse_request_end(req)` runs, so `fuse_request_end()`
never takes `bg_lock` for a queue-charged request — the same order the
7.x `fuse_request_bg_finish()` split used. `FR_BG_URING` exists because
`FR_BACKGROUND && FR_URING` is NOT that predicate: a background request
the classical `flush_bg_queue()` hands to the ring across the `fiq->ops`
switch-over (`fuse_send_one()` → `fuse_uring_queue_fuse_req()`, which
sets `FR_URING`) was charged to the CONNECTION. It stays there and is
credited by `fuse_request_end()` exactly as before — the one remaining
`bg_lock` take on a uring end, only for that transitional class. (Before
0026 that class also decremented a `queue->active_background` it never
incremented — a pre-existing underflow that silently disabled the
queue's one-guaranteed-slot clause; 0026 stops touching the queue ledger
for it.)

### 2.4 `max_background` shrink at runtime (fusectl)

`fuse_chan_max_background_set()` → `fuse_uring_bg_limit_changed()`:
every queue's gate is recomputed as `num_background >= bg_max`. Queues
now over budget close; they re-open as their requests end. Only charged
requests are ever credited, so nothing underflows; queued background
requests admit only when `active_background < the new share`, so the
queue drains to it. A grow opens gates and hands one slot on per queue,
like fusectl's classical write. INIT is too early for a ring to exist,
so the hook is a no-op there; the abort `UINT_MAX` write reaches the
queues through their next admission and the abort flush.

### 2.5 Semantics change, stated plainly

**A queue whose share is exhausted blocks ITS submitters even while other
queues have room** — the original's "one bg request per queue, ignoring
global limits" locality made general. For SqueezeFS's INIT reply
(`max_background = clamp(queues × q_depth, 64, u16::MAX)` — the
delivered ring capacity, AGENTS.md FUSE uring knobs) a queue's share is
**exactly its ent count**, so on this daemon the only behavior change is
the locking. A daemon whose `max_background` is below `nr_queues` gets
one slot per queue (`nr_queues` in flight, not `max_background`) — the
floor the flush already granted to `active_background`, now also to
admission. Aggregate in-flight is otherwise ≤ `max_background` + the
bounded per-queue overshoot the original also had.

Lock order after 0026: `queue->lock` alone on both hot paths;
`fuse_uring_bg_limit_changed()` takes `queue->lock` AFTER
`fuse_chan_max_background_set()` dropped `bg_lock`, so no path nests
`queue->lock` inside `bg_lock` (the historic nesting was the reverse).
The `lockdep_assert_held(&queue->lock)` lines stay on every helper.

## 3. Alternatives weighed

* **Per-CPU counters (`percpu_counter`) for the connection ledger** —
  reads are approximate by `nr_cpus × batch` (2,048 at 32 CPUs), which is
  larger than the congestion threshold the readers compare against.
  Rejected; the per-queue sum is exact and off the request path.
* **A shared `atomic_t` sum beside the per-queue ledgers** — one bouncing
  cache line per submit and per end across 32 workers + 24 submitters;
  the lock's cost in a cheaper coat. Rejected.
* **Global cap kept as a second gate** — reintroduces the shared word on
  the hot path; the per-queue shares sum to the cap anyway. Rejected.
* **Sticky queue (resubmit on the queue waited on)** — would close §2.1's
  hazard without the drain-time `wake_up_all()` but trades the per-CPU
  locality the ring exists for. Rejected; the backstop is one branch.

## 4. Compile proof (2026-09-06; logs beside the 2026-09-03 proof under `~/sqz-kernel-scratch/`)

| Leg | Tree | Command | Result |
|---|---|---|---|
| incremental | the 2026-09-03 patched linux-7.2.3 (0001–0025, pre-built) + 0026 via `patch -p1 --fuzz=0` | `make -j16 fs/fuse/ io_uring/` (gcc 15.3, the `7.1.8-cachyos-lto`-derived config) | **0 warnings / 0 errors** — `build-patched-0026.log` |
| `W=1` | the same tree vs the pristine 7.2.3 control | `make W=1` on the eight `fs/fuse/` TUs that include the touched headers (dev, dev_uring, inode, req_timeout, control, file, virtio_fs, cuse), both trees | **identical warning sets** (the only lines are cuse.c's pre-existing kernel-doc note in both) — `build-patched-0026-W1.log` / `build-pristine-W1-ctl.log` |
| lockdep | the clean 0001–0026 git tree, `O=build-0026-lockdep` | `scripts/config -e PROVE_LOCKING -e DEBUG_SPINLOCK -e DEBUG_LOCK_ALLOC -e LOCKDEP …` → `olddefconfig` → `make -j16 fs/fuse/ io_uring/` | `CONFIG_PROVE_LOCKING=y DEBUG_SPINLOCK=y DEBUG_LOCK_ALLOC=y LOCKDEP=y` confirmed in the built config; **0 warnings / 0 errors** — `build-patched-0026-lockdep.log` |
| chain | a fresh worktree at the pristine base | 0001–0025 from `patches-7.2/` + 0026, `patch -p1 --fuzz=0` sequentially | **26/26 applied**, resulting tree **byte-identical** to the git series tip (`git diff` empty) |

`git diff --stat` of 0026 alone: `Documentation/filesystems/fuse/fuse-io-uring.rst`
+26, `fs/fuse/dev.c` +49/−12, `fs/fuse/dev_uring.c` +217/−29,
`fs/fuse/dev_uring_i.h` +34, `fs/fuse/fuse_dev_i.h` +9/−3 — 5 files,
+335/−44. The runtime lockdep run (PROVE_LOCKING under load, abort while
queues are blocked, fusectl shrink under load) is the boot test's.

## 5. The A/B row (owed — the parent runs it once the user boots a patched kernel)

**Rig:** `.benchmarks/rigs/2026-09-06-kernel-bg-per-queue-ab.sh` (the
`2026-09-03-r4-field-abba.sh` / `-field-perf.sh` pattern; reuses
`2026-09-03-r4-row-delta.py` for the per-row table).

* **Arms:** the SAME daemon binary (a rocky8 `task build` of the dev tip,
  `release`, KD-7 shim pairing irrelevant — kern rows only + one il
  control), **kernel A** = the sqz series 0001–0025 (7.2 track; or the
  running 6.19.14-sqz if the row runs on the field box before the 7.2
  boot) vs **kernel B** = the same series + 0026. A-B-B-A across reboots
  is impossible, so **A A B B** (two boots), each boot: fresh cluster,
  one `write_BW` prep pass minting the file set, then the rows twice.
* **Rows per boot:** kern rand-4k 24 × qd8 30 s (+10 s ramp) × 2, the
  60 s sustained kern rand-4k, the 1 MiB seq `read_BW` qd16 × 2, one il
  rand-4k control (the shim does not take these locks — it should NOT
  move), one kern rand-4k row with `perf` on the `f3-ur*` threads mid-row
  (flat + a 2 s DWARF call graph).
* **Thermal state recorded per row:** `/sys/class/thermal/*/temp` +
  `/sys/class/hwmon/*/temp*_input` max, the box-wide `/proc/stat` busy,
  loadavg — the rig writes them beside every `.row`.
* **Columns:** `daemon_cpu_ns_by_class[fuse3-ur]` per op (the primary),
  `read_transport_phase_ns` (`queue_wait` / `dispatch_lag` /
  `reply_commit` / `transport_total`, exact means), the perf ledger's
  `native_queued_spin_lock_slowpath` + `_raw_spin_lock` share on the
  workers with their callers (`perf report -S … -g caller`), IOPS, clat
  p50/p99 (fio JSON), `zc_bridge_phase_ns` for context.
* **Verdict rule:** B ships only if the worker's µs/op falls with the
  lock class (expected −1.5…−2 µs/op → about −10 % of the worker's 17.7),
  IOPS ≥ par, p99 ≥ par, the seq rows par, the il control par, on BOTH B
  boots. A B boot that WARNs (`fuse_uring_task_to_queue`, `WARN_ON_ONCE`
  in abort) or trips lockdep is a red-first bug before any number counts.
* **Tier:** measured-real once run on squeeze-test; a dev-box run is
  scoping (its lock population is 32 workers + N local submitters on one
  target).

## 6. Backport ledger (7.1.6 and 6.19.14 — owed; deltas observed from `ref-7.1.6/` and the 6.19 series)

The patch was authored where the design read was done (7.2.3). The
touched functions differ per track as follows — every difference is a
mechanical re-expression, none changes the design:

| Site | 7.2.3 (authored) | 7.1.6 | 6.19.14 |
|---|---|---|---|
| The ledger's owner | `struct fuse_chan *fch` (`fch->bg_lock`, `fuse_dev_i.h`) | `struct fuse_conn *fc` (`fc->bg_lock`, `fuse_i.h` ~700–722) — every `fch->` becomes `fc->`, `ring->chan` → `ring->fc` | same as 7.1 |
| `fuse_uring_req_end()` | single nested `bg_lock` take: `queue->active_background--; bg_lock; fuse_request_bg_finish(fch); flush; unlock` | identical shape on `fc` (`dev_uring.c` ~86–96) | **no `fuse_request_bg_finish()`**: `req_end` takes `bg_lock` only for the flush; `fuse_request_end()` then takes it AGAIN for the inline finish (clear `FR_BACKGROUND`, `blocked` logic, `num--`, `active--`, `flush_bg_queue`). The per-queue credit clearing `FR_BACKGROUND` skips that whole block — verify the 6.19 `fuse_request_end()` gates on `FR_BACKGROUND` alone (it does upstream) |
| `fuse_request_bg_finish()` → `static` | `dev.c` ~601, declared in `fuse_dev_i.h` | `dev.c` ~451, declared in `fuse_i.h` — same hunk, different header | does not exist; nothing to make static |
| `fuse_chan_num_background()` sum | ONE accessor (`dev.c` ~388), callers `file.c` 933/2321 + `control.c` | **no accessor** — `file.c` 911/2306 read `fc->num_background` directly: add `fc->num_background + fuse_uring_num_background(fc)` at both sites (or introduce the accessor) | same as 7.1 |
| `fuse_chan_max_background_set()` + re-gate hook | ONE setter (`dev.c` ~398), callers `control.c` write + `inode.c` INIT | **no setter** — `control.c` 133 writes `fc->max_background` under `fc->bg_lock` inline (and `inode.c`'s INIT path likewise): `WRITE_ONCE` + call `fuse_uring_bg_limit_changed(fc)` after the unlock at the fusectl site | same as 7.1 |
| `fuse_block_alloc()` | three-clause form with `smp_rmb()` after `initialized` | one expression `!fc->initialized \|\| (for_background && fc->blocked) \|\| (fc->io_uring && fc->connected && !fuse_uring_ready(fc))` — add `&& !fuse_uring_ready(fc)` to the `blocked` clause | same as 7.1 |
| `fuse_get_req()` two-stage wait, `fuse_put_request()` kick, alloc-failure kick | as authored | same code on `fc`; `wait_event_state_exclusive` + `TASK_FREEZABLE` present | verify `wait_event_state_exclusive` exists (it does from 6.x); else `wait_event_killable_exclusive` |
| `fuse_uring_task_to_queue()` | exists | exists (`dev_uring.c` ~1276) | exists |
| `struct fuse_ring_queue::active_background` + `fuse_req_bg_queue` | exist | exist (`dev_uring_i.h` 95/99) | exist |
| `fuse_uring_abort_end_requests()` | `WARN_ON_ONCE(fch->max_background != UINT_MAX)` + nested `bg_lock` flush | same on `ring->fc` (`dev_uring.c` ~134–139) | same |
| Request timeout scan of `bg_queue` | `req_timeout.c` under `fch->bg_lock` — untouched | `dev.c` ~92 under `fc->bg_lock` — untouched | untouched |
| `fuse_uring_entry_teardown()` credit | added | same site | same site (verify the 6.19 teardown does `list_del_init(&req->list)` for `ent->fuse_req` — the credit goes beside it) |
| The rst paragraph | `Documentation/filesystems/fuse/fuse-io-uring.rst` end | same file (0021 docs exist on both tracks) | same |

Ordering law for the backports: 7.1 first (the locally-booted line), then
6.19.14 (the field), each with its own `make io_uring/ fs/fuse/` proof and
the `W=1` control — the 0030 precedent. The 6.19 backport is where the
field's two-caller ledger (§0) is actually measured away, so the A/B's
first real row is likely THAT track's.

## 7. What this document does not claim

No runtime behavior is verified: no boot, no lockdep run, no abort-under-
load or fusectl-shrink exercise, no number. The expected −1.5…−2 µs/op is
the R-4 ledger's arithmetic on the contention class, not a measurement of
this patch. The semantics change (§2.5) is stated for the record; its
only observable on the SqueezeFS daemon is `fuse_chan_num_background()`'s
sum and the absence of `bg_lock` in the uring profiles.
