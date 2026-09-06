# Kernel COMMIT-lock split — per-queue FUSE background accounting (sqz patch 0026, 7.2 track)

**Status (2026-09-06): PATCH AUTHORED + COMPILE-PROVEN ON ALL THREE TRACKS, boot + A/B OWED.**
`docker/kernel-sqz/patches-7.2/0026-sqz-fuse-uring-per-queue-bg-accounting.patch`
— the first patch AUTHORED on the 7.2 track (every earlier 7.2 patch is a
rebase), landed on `dev` (`9edcf624`); compile-proven three ways on
linux-7.2.3 + 0001–0025 (§4), the 26-patch chain re-verified `patch -p1
--fuzz=0` on a fresh base. **Backported the same day as 0031 on the
6.19.14 FIELD track (`patches/`) and the 7.1 track (`patches-7.1/`)**,
each compile-proven the same three ways (§6). Not booted: the box that
boots it is the user's call, and the lever holds its slot on the field
A/B row (§5) — never on this document.
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

## 5. The A/B row — RUN 2026-09-06, verdict B SHIPS

**Result** (`.benchmarks/2026-09-06-kernel-bg-per-queue-ab.md`, the
A A B B on squeeze-test, 6.19.14 0001–0030 vs +0031, same 1.2.1 `dist`
daemon): every clause below met on both B boots — the `fuse3-ur`
worker **17.4 → 14.3 µs per transport op (−18 %)** with
`native_queued_spin_lock_slowpath` **12.8 % → 0.11 %** of its cycles
(`_raw_spin_lock` 3.3 → 2.3 %); kern rand-4k **+9.0…+9.4 % IOPS**
(508–510k → 555–557k; the 60 s sustained row +9.1 % and flat), p50
−9…−11 %, p99 −13…−17 %; seq 1 MiB par on throughput with p99 −8…−15 %;
the il control +1.0 % (par — the attribution pin: the shim never enters
the FUSE request path); box busy −3 points at +9 % delivered; zero
fuse/io_uring kernel-log lines, no WARN, no lockdep, every daemon
tripwire 0 on all 14 rows. The rule as written:

**Rig:** `.benchmarks/rigs/2026-09-06-kernel-bg-per-queue-ab.sh` (the
`2026-09-03-r4-field-abba.sh` / `-field-perf.sh` pattern; reuses
`2026-09-03-r4-row-delta.py` for the per-row table).

* **Arms:** the SAME daemon binary (a rocky8 `task build` of the dev tip,
  `release`, KD-7 shim pairing irrelevant — kern rows only + one il
  control), **kernel A** = the track's sqz series WITHOUT the patch vs
  **kernel B** = the same series WITH it — on the field track (the
  finding's venue, `squeeze-test` on `6.19.14-sqz`) A = 0001–0030, B =
  +0031; on 7.1 A = 0001–0030, B = +0031; on 7.2 A = 0001–0025, B =
  +0026. The rig tells the arms apart by the loaded fuse module's
  `fuse_uring_bg_wait` symbol (the same name on all three tracks).
  A-B-B-A across reboots is impossible, so **A A B B** (two boots), each
  boot: fresh cluster, one `write_BW` prep pass minting the file set,
  then the rows twice.
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

### 5a. Runtime canary — the patch's first boot (2026-09-06, dev box, 7.2.3 + 0026)

The verdict rule's red-first clause ("a B boot that WARNs … before any
number counts") was checked the first time a patched kernel ran the
daemon: the dev box booted `7.2.3-cachyos-lto` carrying the 7.2 track
(`fuse_uring_bg*` symbols present in the loaded `fuse.ko`,
`enable_uring=Y`, `fuse: init (API version 7.45)`), and the box's own
kernel build for squeeze-test (`make -j16 binrpm-pkg`) was running
alongside — so nothing below is a number, only a NO-WARN + engagement
record. Artifacts: `.benchmarks/rows-kernel-bg-ab-20260906/canary-7.2.3-devbox/`.

* **zc-capability gate** (`sudo tests/run_zc_capability_gate.sh`,
  `REQUIRE_CAPABILITY=1`), both `SQUEEZEFS_READ_ZC_SERVE` postures on
  `dev` `aed8374d`: **149/149 each**, skip ledger EMPTY, rc 0 (the same
  five suites + zcrx lib probes the 1.2.1 gate counted).
* **Load canary** on the loop dev substrate (4 mds null_blk + 4 oss zram),
  `release` binary `a24b59a8`, cache-less format, kern path only, the
  house runner (`tests/fio/run_fio_row.sh`, fio-3.42, libaio, direct=1):
  the write_BW prefill (32 × 256 MiB, bs 1 MiB, qd8, 20 s: 2,799 MiB/s,
  clat p50 77 ms / p99 300 ms) minted the set, then rand-4k 32 jobs ×
  qd8 30 s (+5 s ramp): **779,395 IOPS**, clat p50 259 µs / p99 1.29 ms,
  27.96 M `fuse3_zc_replies` — every read rode the zc direct leg (the
  copy ledger's `read_copy_*`/`read_dest_*` terms all 0), with the
  transport at 32 queues × depth 32, `transport_max_background` 1024
  (fusectl `max_background=1024 congestion_threshold=768` — the
  per-queue share under 0026 is 32).
* **Tripwires after both rows:** `transport_cq_overflows` 0,
  `fuse_op_watchdog_overdue` 0, `invariant_tripwires` 0,
  `detached_task_panics` 0, `read_dest_overruns` 0, `open_count_stranded`
  0, `fuse3_zc_fd_body_fallbacks` 0; the §5.4 park ledger closed exactly
  (`transport_parked_commits` 59,844 ≡ `transport_unparked_commits`).
  `transport_lease_overlong` = 36, all inside the prefill's first second
  (leases held 1.07–1.17 s at 32 × qd8 × 1 MiB on a fresh zram set with a
  kernel compile on the box — the loud-never-fatal backpressure class,
  not the kernel's). Kernel log: **zero fuse/io_uring lines** across the
  gate and the rows; the one WARNING in the boot's dmesg is `amdgpu`
  display power management at 14:23, unrelated. Unmount 0.15 s via the
  verb, no daemon left, substrate torn down clean.
* **What it does not say:** nothing about the lock class — the dev box
  has no A arm on this kernel (7.1.8 → 7.2.3 changed underneath), and the
  IOPS figure is a laptop-substrate scoping row. The measured-real verdict
  is still §5's A A B B on squeeze-test. The `run_fio_row.sh` meta used to
  echo the runner's `--bs` default (`1M`) for a job whose shape FIXES
  `bs=4k`; the runner now records the effective knob (this commit), and
  the persisted canary meta was corrected to `4k` — the fio JSON beside it
  (`bw_bytes ÷ iops` = 4096) was always the authority.

## 6. Backport ledger (7.1.6 and 6.19.14 — DONE 2026-09-06, both compile-proven three ways)

The patch was authored where the design read was done (7.2.3). Both
backports landed the same day as **0031** on their tracks
(`docker/kernel-sqz/patches/0031-sqz-fuse-uring-per-queue-bg-accounting.patch`,
`docker/kernel-sqz/patches-7.1/0031-sqz-fuse-uring-per-queue-bg-accounting.patch`),
field track first. Every difference below was a mechanical
re-expression; the design (§1) and the five points (§2) transfer
verbatim. What the compile proof and the tree reads taught, beyond the
predicted table:

* **6.19's `fuse_uring_abort()` is gated on `queue_refs > 0`** (7.1+ walk
  the queues unconditionally — the upstream fix landed between the two).
  On the `queue_refs == 0` arm 6.19 does not walk the queues at all, so
  the 7.2 patch's gate-open-in-`abort_end_requests` would have left a
  per-queue sleeper stranded across an abort. 6.19's 0031 adds
  `fuse_uring_bg_abort_waiters(ring)` on that arm (open every gate,
  `wake_up_all`); the queued requests' own fate on that arm is unchanged
  from the tree (a pre-existing 6.19 gap — noted, not widened; adopting
  7.1's unconditional walk on 6.19 is a separate decision).
* **The verifications the ledger asked for hold on 6.19:**
  `fuse_request_end()` gates its inline block on `FR_BACKGROUND` alone
  (`dev.c` 480 `if (test_bit(FR_BACKGROUND, &req->flags))`, then
  `bg_lock`, `clear_bit`, the wake logic, `num--`, `active--`,
  `flush_bg_queue`), so the per-queue credit's clear skips it whole;
  `fuse_uring_entry_teardown()` `list_del_init(&req->list)`s
  `ent->fuse_req` under `queue->lock` — the credit sits beside it.
  `wait_event_state_exclusive` + `TASK_FREEZABLE` exist on both trees.
* **The accessor question resolved as an inline in `dev_uring_i.h`**:
  `fuse_num_background(fc)` (both CONFIG arms) rather than an edit to
  `fuse_i.h`'s include chain — `file.c` and `control.c` gain
  `#include "dev_uring_i.h"` exactly as `inode.c` already has it.
* 6.19's `fuse_uring_req_end()` keeps its unguarded
  `io_buffer_unregister(ent->cmd, …)` (no `7d87a5a284bb` path there) and
  its abort loop sets `stopped` outside the lock — both untouched.
* **gcc 8.5:** the kernel's own floor is gcc 8.1
  (`scripts/min-tool-version.sh`, `-std=gnu11`) on both 6.19 and 7.1, the
  EL8 image compiles with gcc-toolset-14, and "all gcc-8.5-clean" in the
  README is the field-box PROBES' requirement. The patch uses nothing past
  gnu11 anyway (no cleanup attributes, no `__auto_type`, no C23, kernel
  `max()`/`READ_ONCE`/`WRITE_ONCE` only). Not compiled with gcc 8.5 here
  (no such toolchain on the box); compiled with gcc 15.3 per track.

| Site | 7.2.3 (0026) | 7.1.6 (0031) — as landed | 6.19.14 (0031) — as landed |
|---|---|---|---|
| The ledger's owner | `struct fuse_chan *fch` (`fch->bg_lock`, `fuse_dev_i.h`) | `struct fuse_conn *fc` (`fc->bg_lock`, `fuse_i.h` ~700–722): `fch->` → `fc->`, `ring->chan` → `ring->fc`, `fuse_chan_abort()` → `fuse_abort_conn()` | same as 7.1 |
| `fuse_uring_req_end()` | single nested `bg_lock` take (`fuse_request_bg_finish(fch)` + flush) → per-queue `fuse_uring_bg_finish()` | identical shape on `fc` (`dev_uring.c` ~98–120) → same replacement | **no `fuse_request_bg_finish()`**: `req_end` took `bg_lock` for the flush; `fuse_request_end()` took it AGAIN for the inline finish (the ledger's two callers). Same replacement; the inline block stays verbatim and is skipped by the cleared `FR_BACKGROUND` |
| `fuse_request_bg_finish()` → `static` | `dev.c` ~601, declared in `fuse_dev_i.h` | `dev.c` ~451, declared in `fuse_dev_i.h` (86) — same hunk pair | does not exist; a comment above the inline block instead |
| `fuse_chan_num_background()` sum | ONE accessor (`dev.c` ~388) | **no accessor** — `file.c` 911/2306 read `fuse_num_background(fc)` (new inline, `dev_uring_i.h`) | same as 7.1 (`file.c` 899/2292) |
| `fuse_chan_max_background_set()` + re-gate hook | ONE setter (`dev.c` ~398) | **no setter** — `control.c` 133: `WRITE_ONCE` + `fuse_uring_bg_limit_changed(fc)` after the unlock; `inode.c`'s INIT write untouched (no ring can exist yet) | same as 7.1 |
| `fuse_block_alloc()` | three-clause form with `smp_rmb()` | one expression — `&& !fuse_uring_ready(fc)` added to the `blocked` clause | same as 7.1 |
| `fuse_get_req()` two-stage wait, `fuse_put_request()` kick, alloc-failure kick | as authored | same code on `fc` (`fuse_get_req(idmap, fm, …)`, `fc = fm->fc`; the per-queue wait sits after the tree's `smp_rmb()`) | same as 7.1 |
| `fuse_uring_abort()` / `fuse_uring_abort_end_requests()` | unconditional walk; gate-open + `wake_up_all` under the queue lock | unconditional walk — 0026's arm verbatim (`WARN` reads `fc->max_background`) | **gated on `queue_refs > 0`** — gate-open inside the walk + `fuse_uring_bg_abort_waiters()` on the `== 0` arm |
| `fuse_uring_num_background()` / `_limit_changed()` ring load | `smp_load_acquire(&fch->ring)` | `smp_load_acquire(&fc->ring)` (7.1 publishes with `smp_store_release`) | plain `fc->ring` (6.19's `fuse_uring_ready()` itself reads it plain) |
| `fuse_uring_task_to_queue()`, `queue->active_background`, `fuse_req_bg_queue` | exist | exist | exist |
| Request-timeout scan of `bg_queue` | `req_timeout.c` under `fch->bg_lock` — untouched | `dev.c` ~92 under `fc->bg_lock` — untouched | untouched |
| The rst paragraph | appended after *Payload retention* | same | same |
| Diffstat | 5 files, +335/−44 | 8 files, +340/−44 | 7 files, +376/−41 |

**Compile proofs (2026-09-06; `~/sqz-kernel-scratch/build-{6.19,7.1}-*.log`):**

| Track | Config | Control (0001–0030) | +0031 | +0031 lockdep (`PROVE_LOCKING` `DEBUG_SPINLOCK` `DEBUG_LOCK_ALLOC` `LOCKDEP` =y) | `W=1` all `fs/fuse/` (23 TUs) | Chain |
|---|---|---|---|---|---|---|
| 6.19.14 (field) | EL8 base + `config-fragment`, the build script's own `olddefconfig` assembly, gcc 15.3, `O=` | 0 compiler warnings / 0 errors | 0 / 0 | 0 / 0 | identical empty sets | 31/31 `--fuzz=0` from the pristine tarball (sha256 verified against the pin and `v6.x/sha256sums.asc`), byte-identical to the `git am` tree |
| 7.1.6 | `config-7.1.8-cachyos` → `olddefconfig`, gcc 15.3, `O=` | 0 / 0 | 0 / 0 | 0 / 0 | identical empty sets | 31/31 `--fuzz=0` from a fresh 7.1.6 (identical to `ref-7.1.6/`), byte-identical to the `git am` tree |

(The 6.19 logs each carry ONE `warning:` line — Kconfig's
`BOOTPARAM_SOFTLOCKUP_PANIC=0` note from the EL8 base config itself,
present on the control too; no compiler warning on any leg.) The 30
prior patches `git am` clean on both fresh bases. The 6.19 backport is
where the field's two-caller ledger (§0) is actually measured away, so
the A/B's first real row is that track's: kernel A = 0001–0030 vs B =
+0031, the rig's symbol check (`fuse_uring_bg_wait` in the loaded
module — the same name on all three tracks) tells the arms apart.

## 7. What this document does not claim

No runtime behavior is verified: no boot, no lockdep run, no abort-under-
load or fusectl-shrink exercise, no number. The expected −1.5…−2 µs/op is
the R-4 ledger's arithmetic on the contention class, not a measurement of
this patch. The semantics change (§2.5) is stated for the record; its
only observable on the SqueezeFS daemon is `fuse_chan_num_background()`'s
sum and the absence of `bg_lock` in the uring profiles.
