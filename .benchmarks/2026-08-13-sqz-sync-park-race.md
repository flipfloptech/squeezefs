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

## Standing consequences

- `lock_ticked_reregisters` is restored to its design meaning: **0 on healthy schedules**.
  Growth is once again a loud anomaly, not an absorbed product-cadence tax.
- The dio-inval-latch bistability attribution must be re-run on top of this fix (the collapsed
  mode's stalls were this bug; the latch remains parked on `perf/dio-inval-latch` pending that
  re-test).
