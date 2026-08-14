# 2026-08-13 — sqz_sync park-racing-release lost wake (the 2 s-tick write stalls)

**Branch:** `fix/sqz-sync-park-race` (`661cfc19`) off dev `87b2b7ba`.
**Venue:** TCP devsub (`SQZ_DEVSUB_TRANSPORT=tcp`, nvmet-tcp localhost; meta = 4× nullb via
nvmet, data = 4× zram 8G). Instrument: fio; per-cell fresh substrate (zram ages across
`format`), mountpoint scrub + `mountpoint -q` assert, mount retry after kill -9. Every cell
reads `lock_ticked_reregisters` (the sqz_sync tick-rescue counter) before/after its row.

## Conviction

The dio-inval-latch campaign (`.benchmarks/2026-08-13-dio-inval-latch-campaign.md`) left one
open question: why the latch binary's pure-write stream bistably collapsed on the 1-file qd32
libaio row. Bucket decomposition of the collapsed window's `write_lock_wait_exclusive`
histogram answered it — the distribution is **stall-punctuated, not uniformly slow**: 98 % of
waits ≤ 256 µs, then 255 ops parked in the `<=2s` bucket, dominating the 23 ms mean. The same
saved windows showed the tick rail engaging on **both** binaries (latch: +143
`lock_ticked_reregisters`; dev tip A: +114, with 192 `<=2s`-bucket waits) — a real lost wake
absorbed by the 2 s backstop at product cadence, latch or no latch.

## Root cause

`sqz_sync::Acquire::poll` ran `LockCore::try_acquire` and `LockCore::register` as **two
separate interior critical sections**. A release landing between them saw an empty queue,
woke nobody, and flipped the lock free; the waiter then parked with no wake in flight. The
2026-08-13 FIFO-courtesy fairness fix amplified the cost: every fresh contender queues behind
the stranded front waiter, so the entire qd-deep convoy on one inode lock froze until the
front's 2 s tick. A second hole in the same class: `unregister` (acquire-future cancellation)
removed a woken front waiter without handing the wake to the new front.

The in-tree `sqz_semaphore` already used the correct single-hold shape (`AcquireFut::poll`
checks and parks under one lock) — only the rwlock/mutex core had the split.

## Fix

- `LockCore::poll_acquire` — grant-or-enqueue under ONE critical section, serialized against
  release by the interior mutex: either release ran first (the parker sees the freed lock and
  grants) or the park ran first (release sees the queued waiter and returns its waker).
  `register` survives only as the loom dead-waiter harness's raw-park surface.
- `LockCore::unregister` returns the new front's wakers when the cancelled entry was the
  front (a spurious wake costs one re-poll; a lost one cost a tick). `Acquire::Drop` wakes them.

## Proof

- Loom (red-first): `a_parking_waiter_racing_release_is_never_stranded` **fails the retired
  two-hold shape** (weakening-verified by re-expressing `poll_acquire` as try-then-register —
  loom finds the stranding) and passes the fix; `cancelling_the_front_waiter_hands_the_wake_
  to_the_next` pins the unregister handoff. Full loom suite 83/83 (`tests/run_loom.sh`).
- Wrapper suite `sqz_sync::tests` 7/7 (fairness, dead-waiter, tick backstop, cancel-unlink all
  green on the new core).

## Measured (A-B-B-A, fresh cell per leg, 25 s rows)

**rand-4k write, 1 file, libaio qd32** (the collapsed row):

| leg | IOPS | p99 (µs) | tick_delta |
|---|---|---|---|
| F1 (fix) | **25.2k** | 2180 | **0** |
| A1 (dev tip) | 17.5k | 2114 | 81 |
| A2 (dev tip) | 19.2k | 2245 | 61 |
| F2 (fix) | **25.4k** | 2180 | **0** |

**+37 % order-independent, engagement exact**: the tick-rescue counter collapsed 61–81 → 0.

**rand-4k write, 32 threads × separate files, psync** (regression check): F 70.4k/68.8k vs
A 68.7k/68.2k, p99 1811/1942 vs 1926/1909 — par-to-ahead, tick_delta 0 across the board.

## Follow-on: the fleet-residue recount (same day, governing il rows)

Doctrine (user, 2026-08-13): **IOPS verdicts govern on the IL shim; throughput on the kernel
zero-copy path.** The fleet attribution ledger (32×8 il randwrite, OP_PROFILE window + iostat)
split fio clat 2415 µs into 77 ingress + 1040 weighted-server + **~1300 µs client residue**,
with the device at 79/256 concurrency — the residue, not the device, was the wall.

* **`REAP_EVENT_PARK_MAX` 2 → 24 (shipped default retune, commit `047783a0`)**: the
  2026-07-28 wake herd that priced 24 out predates the r2 batch-wake threshold; re-counted
  with no losing shape (write 32×8 +12–14 %, 16×16 +7 %, 32×32 wash, qd1 RTT preserved, reads
  +1.4–2.8 %). KD-7 lesson re-learned: a dirty-stamped shim against a clean daemon runs
  PASSTHROUGH — the engagement columns caught it (`ipc_w_delta=0`), rows discarded.
* Post-retune ledger: 143k IOPS, residue 1298 → **759 µs**, device concurrency 79 → 110,
  dd admit 315 µs (svc-thread intra-pass queueing, not CPU — see below).
* **Width re-sweep FALSIFIED (again)**: lanes 8/12/16/24 → 126/136/132/129k — the derived
  `3×cpus/8` (= 12 here) is still the interior optimum on the fixed binary; the 2026-08-06
  slope stands unchanged (user law re-affirmed: tuning lands as derivations of cores/memory,
  never box constants).
* Scout losers (single legs, not counted): `REAP_QUANTUM_US=10` −13 %, `IPC_SPIN_US=50` −24 %.

**Sustained-state rows (90 s, retuned pair, thirds-flat per the 2026-07-29 rule):**
il 1-file qd32 (1 GiB) = **126k flat (−2.4 % drift)**; il 32×8 fleet = **144k flat (+4.1 %)**.
Engagement exact on both (13.1 M / 11.4 M ring writes accounted). Cumulative vs the dev tip's
governing rows: 1-file 14–68k bistable → 126k; fleet ~96–115k → 144k. One real finding from
the sustained discipline: a 128 MiB single hot file DECAYS 45k → 32k (−28 %) — the
whole-file-rewrite churn shape (reclaim/displacement backlog), a separate wall from this
campaign's, left named for the rewrite program.

## The 759 µs residue attribution (same day, part 3)

The residue is **worker-count scheduling, amplified by the handler-handoff share** — counted:

1. **Submit is clean**: fio slat 2.7 µs at 32×8 — the ring publish never blocks.
2. **Process-count sweep (same offered depth where noted)**: 32×8 = 154k (clat 1656 µs,
   CPU PSI-some 32 % of the window) / 16×8 = 186k / 8×8 = 218k / **8×32 = 228k** /
   **4×64 = 242k** (PSI 5 %). Depth exonerated, worker count convicted: at 256 offered
   in-flights, 4 processes beat 32 processes by **57 %**. fio `--thread` (32 workers, one
   process) is WORSE (111k) — it is scheduling/wake fan-out, not process overhead.
3. **The wake fan-out multiplier is the handler-lane share.** Steady-state (128 MiB files)
   write-lane ledger closes exactly: `patch_writes` 1.585M (55 % — dd in-place) +
   `patch_ineligible_unmapped` 1.015M (35 %) + `overlay` 274k (9.5 %) ≈ all 2.876M ring
   writes. On FULLY-WRITTEN inline-mapped files (`layout_indirect_map_reads` = 0), 35 %
   unmapped means the per-block mapping is probed MID-CYCLE: ring write → active buffer →
   flush → publish → mapped again, revisited every ~7 ms at this rate — the probe lands
   before the publish restores the map. Every such op pays the handler handoff (an extra
   cross-thread wake + lane dispatch), and at 32 client processes those wakes are what the
   PSI shows as runqueue starvation.

The prize named for the next campaign: **dd-eligibility across the publish cycle** (serve the
W1 patch against the block's current custody even while the active/publish window is open —
needs the §5.1 fence-protocol treatment, a design item, not a lever), worth an estimated
+60 % on the 32-process fleet shape (its 4-process ceiling is 242k on this venue).

## Standing consequences

- `lock_ticked_reregisters` is restored to its design meaning: **0 on healthy schedules**.
  Growth is once again a loud anomaly, not an absorbed product-cadence tax.
- The dio-inval-latch bistability attribution must be re-run on top of this fix (the collapsed
  mode's stalls were this bug; the latch remains parked on `perf/dio-inval-latch` pending that
  re-test).
