# Design: the IL direct-drive WRITE lane (Rev 1, 2026-08-11)

**Status: design phase.** Owner branch ladder: `perf/il-direct-write-pr1…N`.
Evidence basis: `.benchmarks/2026-08-11-op-registry-shard.md` (the engaged-il
ledger + thread-class attribution), `.benchmarks/2026-08-11-write-iops-campaign-day1.md`
addendum 2 (the KD-7 passthrough correction that made the il picture honest).

## 1. The strategic ruling (user, 2026-08-11)

> "why are we chasing 1m write iops at the kernel level when our read IOPs
> are at the interposer IL/SHIM level?"

Every 1 M-class result this system has produced lives on the shim's
**direct-drive** lane (reads: 1.03–1.22 M sustained, shim-iops/drain-funnel
campaigns). Kernel FUSE warm floors sit ~600–650 k. Writes have **no
direct-drive lane at all**: an il write severs on the svc thread (§5.5.2)
and then hands off to the fuse3-tpc handler lanes to run the FULL kernel-venue
write handler — it pays the ring hop AND the handler. The road to 1 M writes
is the read lane's road: execute the eligible write shape ON the ipc lane,
device-true, with no handler handoff.

## 2. The measured prize (2026-08-11, squeeze-test, engaged rows)

Thread-class CPU on an engaged il rand-4k row (277.5 k IOPS, qd32 t32):

| class | cores | µs/op | share |
|---|---|---|---|
| `fuse3-tpcN` (the handoff venue) | 12.88 | 46.4 | 77.8 % |
| main + misc | 2.35 | 8.5 | 14.2 % |
| `sqz-ipc-svcN` (the lane reads run 1 M+ on) | 1.28 | 4.6 | 7.8 % |

tpc-lane flat profile (perf_il.data, dwarf, lane-aggregated):

| line | % of tpc cycles | class |
|---|---|---|
| `scc` bucket `Writer::lock_sync_wait` on `tiering::memory` `(Bytes,(Bytes,EntryState))` + `saa::wait_queue::poll_result_sync` | 22.1 | RAM-tier bucket contention — 32 lanes colliding on per-block keys |
| vdso clock (`gettimeofday` + `Timespec::{now,sub_timespec}` + anon vdso) | 23.5 | clock ceremony |
| `clear_page_erms` | 6.3 | page zeroing on buffer mints |
| `DataPlaneSink::serve_write` / `write_file_staged` glue | ~1.8 | the actual path |

Over half the handler-lane cost is contention + clocks + zeroing, not write
work. Reference: kern venue whole-daemon = 31.9 µs/op at 527 k; il = 60.1
µs/op at 272 k (op-registry bracket, CPU face).

## 3. The lane (target architecture)

**Eligible shape v1 = the W1 sole-owner patch predicate** (design-random-
small-writes; `is_whole_block_mapping()` + the 6-clause ledger): LBA-aligned
sub-block overwrites of exclusively-owned, passthrough, whole-block-mapped
striped blocks. On the standing rand-4k rows this is ~100 % of ops
(`patch_writes ≈ fuse_ops`). Everything ineligible rides today's
sever→handoff path unchanged — the lane is a fast path, never a fork of
write semantics.

Flow: ring dequeue (svc thread) → **eligibility probe** (lock-free, RAM-only)
→ **custody** via the §4.2 Shared admission probe + the §5.1 clone/patch
fence → **`data_custody::authorize_dma`** (the one door; epoch-carried form)
→ in-place sub-block DMA on the dd rings (the read lane's io_uring shards;
lane-scoped flush, COOP_TASKRUN — r3 machinery reused) → CQE → postlude
(coverage `record_write`, identical-WriteTimes publish elision, killpriv
clear law) → ring completion + doorbell.

What the lane must NOT skip (the correctness rails):
- the D0 fence: `data_dma_fence_refusals` / epoch refusals stay authoritative;
- the §5.1 fence pair on the clone/patch race (both sides);
- coverage-union bookkeeping (`record_write` — kernel-split OOO law);
- killpriv-v2 clearing on flagged writes (il-parity ring writes already
  carry it — reuse verbatim);
- KD-11 write-through semantics (the lane IS write-through by construction);
- the reserved-xattr/`.stats` screens (write path never touches them).

## 4. The three cost kills the profile mandates (lane-independent)

These pay on BOTH venues and land before or with the lane:

1. **The `tiering::memory` bucket storm (22 %)**: name the exact site (the
   write-tier update the patch path performs per op) and either elide it
   under value-idempotence (the publish-elision precedent), shard it, or
   move it off the per-op path. OPEN: why il hits it ~10× harder than kern
   (suspect: post-handoff burst alignment across 32 lanes).
2. **Clock ceremony (23.5 %)**: coarse-clock the postlude stamps on the
   handler lanes (the `COARSE_NOW_NS` precedent — the registry already
   proved the pattern) and audit `SystemTime::now` mints on the write path.
3. **`clear_page_erms` (6.3 %)**: buffer mints faulting fresh pages —
   severed-pool/assembly recycling should already own this; find the
   leak-to-fresh-alloc site.

## 5. Instruments (ship with PR 1)

`ipc_dd_write_{serves,bytes,ineligible_*,fence_refusals}` (the decision
ledger, `patch_ineligible_*` pattern); `ipc_dd_write_phase_ns`
(admit/probe/dma/postlude/total — `ipc_direct_phase_ns` pattern); the
engagement law: an il write row is INVALID unless `dd_write_serves +
async_handoffs` accounts for its ops. A/B lever `SQUEEZEFS_IL_DIRECT_WRITE=0`
(default ON once the bracket clears; measurement lever, never an escape).

## 6. PR ladder (red-first each)

- **PR 1**: the cost kills (§4) on the existing path + instruments — this
  alone should move the 60 µs/op materially and is venue-shared.
- **PR 2**: the eligibility probe + custody composition on the svc lane
  (no DMA yet — probe ledger + fallback proof; red tests pin the must-not
  rails above).
- **PR 3**: the DMA leg on the dd rings + postlude; bracket vs PR 1.
- **PR 4**: governor/width composition (drain-lane derivation unchanged;
  the lane rides existing `il_drain_lanes_default`).

### PR 2+3 landed (2026-08-11, `perf/il-direct-write`) — notes + residuals

Probe `SqueezefsFilesystem::ipc_direct_write_probe` + the WRITE leg of
the existing `DirectDriveEngine` (`submit_write`/`finish_write` — same
shards, lane routing, flush cadence, reapers, fusion; volumes now open
read+write with a loud reads-only degrade). Rails red-first in
`tests/il_direct_write_tests.rs`. Deviations from the sketch above, all
conservative (ineligible ⇒ fallback, never a weakened rail):

1. **Phase spans ride `ipc_direct_phase_ns`** (admit/inflight/finish/
   total — the read lane's family), not a separate `ipc_dd_write_phase_ns`:
   one table, both directions compose (implementation directive).
2. **Coverage (§3 rail 3)**: the eligible shape's coverage bookkeeping is
   the ino's stream word (`note_last_write_end`, swapped at the probe's
   commit point). The `record_write` coverage-union lives on
   ACCUMULATION buffers, which the overlay screen excludes structurally —
   a block with any overlay is ineligible, so there is never a union to
   feed (the handler's own patch arm records exactly nothing else).
3. **Killpriv (§3 rail 4)**: a `kill_priv`-flagged binding direct-serves
   only under a HELD `killpriv_clean` latch; a non-clean ino is
   custody-INELIGIBLE — one fallback lets the handler clear privs BEFORE
   its data lands (the VFS order; clearing in a CQE postlude would
   invert it), then steady state pays the same contains-check the
   kernel venue does.
4. **Custody window**: the probe holds the block's `BLOCK_FLUSH_LOCKS`
   stripe from ONE non-blocking `try_lock` (contended = custody
   fallback — the serve_read demotion posture) through the CQE
   postlude's purge — the same protection `try_sole_owner_patch` runs
   under, which is what makes probe-time mapping resolution
   authoritative across the DMA with no CQE revalidation ladder.
5. **The durable times park** (`park_write_times`) is the one async
   postlude member: dispatched exactly once per served op to the fuse3
   handler lanes AFTER `completion.complete` — the client ACK never
   waits on tokio. The RAM face (`publish_attr` WriteTimes, elision
   door, `size_claim: None` — non-extending by eligibility) runs
   synchronously on the reap thread.
6. **PR 1 (§4 cost kills) and PR 4 (governor/width) are NOT in this
   branch**; the counted A-B-B-A bracket on squeeze-test (leg.sh
   protocol, engagement enforced) remains owed before any public number
   (the §6 gate).

Gates: counted A-B-B-A on squeeze-test per leg (leg.sh protocol, CPU face,
engagement enforced); release battery before any public number (user ruling
2026-08-11).
