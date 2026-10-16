# Rung 10 — where the sequential write's ≈ 13 ms go (seqwrite; rangelock's next)

**Method.** `scripts/perf/seqdiag.sh` (kamrui, RAM-backed NVMe, dist builds, every venue lock held): one fresh
mount, 4 × 2 GiB lay-down, 1M write rows at jobs 1/4/8/12 × QD16 (and QD1/QD4), each with `.stats` deltas,
`/proc` thread CPU, the handler lanes' current syscall sampled every 10 ms, and `offcputime`;
`scripts/perf/seqdiag.py` prints the table. `SQUEEZEFS_OP_PROFILE` cannot split a striped write (it never
stamps `meta_backend_start`, so the whole span lands in `backend_to_reply`).

**The write's 14.6–16 ms (1M ×4, per write; A = dev, B = this branch).**

| | A | B |
|---|---|---|
| daemon write latency | 14.6 ms | 16.5 ms |
| parked on the block guard (3) at `write_checkout` | 3.9 ms (54 % of writes, 6.9 ms each) | 0.5 ms (9 %) |
| device queue + service | 0.09 + 1.19 ms | 0.08 + 1.13 ms |
| the rest: ops parked off-lane or behind a lane blocked in a DMA | ≈ 9.4 ms | ≈ 14.8 ms |
| MiB/s · daemon cores · lanes in `futex` / idle | 3351 · 5.0 · 39 % / 48 % | 3249 · 4.6 · 34 % / 57 % |

**Fixed: (3) across the overlay's DMA.** The overlay arm held the block guard through its store's DMA, so a
stream's four writes into one 4 MiB block took turns. The charter's range claims exclude overlapping DMAs
(§6.2 item 1; porting spec §3.3), so the arm now claims under (3) and lands holding none. The coverage union
gets the reference's own lock, and a completing CQE freezes the record before it drops its claim. Red
(`32dcdb7`): with every DMA held, a write into the block's other half parks on the stripe ("expected
both_in_flight"). Result: the waits fall to 0.5 ms per write, the settle's to 0.06 ms; throughput does not
move, because the queue only moves (B's latency above).

**Not a daemon depth limit.** Jobs 4 → 8 → 12 at QD16 give 3351 → 3060 → 3155 MiB/s (A) and
3249 → 3275 → 3085 (B). Latency grows 15 → 23 → 39 ms with the ops held (Little), and the rate stays flat.
The depth governor never throttles (target 64 MiB, 0 admission waits); the nvme rings have 1,024 entries
and 0 were refused. No thread saturates (the busiest is `sqz-nvme` at 24 %).

**What binds (design questions, unchanged here).**
1. *A DMA blocks a lane thread.* `nvme_dev.Device.writeBlock` waits on `sync/completion.zig`'s raw futex
   (decision 0001 item 2). The handler fiber therefore holds its lane thread for the whole DMA, and the
   fibers homed there wait with it. The lanes' `futex` share equals the DMAs in flight (39 % of 11 lanes =
   4.3 = 3351/s × 1.28 ms). Device service grows 1.13 → 1.62 ms as jobs rise while the in-flight count
   stays at 4–5.
2. *Two copies per written byte.* Every write larger than block/8 is extracted (`zc.w1Shape`, W1's hold
   shape) into the slab with `WRITE_FIXED` (io-wq 2.4–2.7 cores), then the RAM device copies it again.
   Charter §6.2 item 1 says `WRITE_FIXED(device fd ← slot)`, zero daemon copies. Probe: raw 1M ×4 runs
   6913 MiB/s alone and 5616 beside a shmem copy load (which runs 4575 meanwhile), about 10 GB/s of copies
   combined. So two copies cap at about 5 GB/s.

*Options.* (a) Fiber-aware device waits: the completer wakes through SqzIo's futex table (fiber or raw
waiter), revising decision 0001 item 2 (the journal lane's own note asks for this "Io route"). The in-flight
count is then bounded by the governor (64 × 1 MiB, not ≈ 4.5 lanes). The device gives 6.4–6.9 GB/s at
QD16–64, so the ceiling becomes the two copies, ≈ 5 GB/s (+50 %). (b) Hold overlay-shaped big writes and
store from the slot. Its wait is already an `Io` event on the transport ring, and it saves one copy (io-wq
0.77 ms/MiB). With (a), the expected ceiling is ≈ 6 GB/s (≈ 2×). It needs a hold-policy ruling: the 0.45× of
held W1-ineligible shapes was rand-4k at fabric RTT. Neither is a bound or a serialization the charter leaves
open.

**A/B** (`baseline.sh ram-sqz`, legs A B B A, A = `68b8585`, B = the fix on it; mean B ÷ mean A). The
seq-write rows: 1M ×4 3288, 3193 → 3153, 3207 (−1.9 %); 1M ×1 1896, 1952 → 1925, 1943 (+0.5 %); 4M ×4
3275, 3239 → 3253, 3289 (+0.4 %). The small-block rows: 4k randwrite QD1 / QD32×1 / QD32×4 +1.0 / −1.1 /
−2.1 %, 64k randwrite −2.9 %, 64k randread −1.8 %. Every pair overlaps. The rows the change cannot reach
(4k and 64k writes never take the overlay; the reads) move the same ±2–3 %. Every session ended with a clean
`fsck --offline`.

**Gate** (kamrui, `-Dfresh=true`, this branch): 55/55 steps succeeded; 3003/3086 tests passed (83 skipped).
