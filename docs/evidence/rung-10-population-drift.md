# Rung 10 — `data_alloc_population_drift` under sequential writes: the check raced, nothing drifted

**The sighting.** The must-stay-0 `data_alloc_population_drift` fired 1× and 3× in two 20 s 1 MiB /
4 MiB sequential-write ×4 sessions on RAM NVMe (`rung-10-zero-copy-writes.md`, "seen beside"), never in
a smoke row. Any smoke row's must-stay-0 census would have gone red on it.

**Verdict: the CHECK, not the accounting.** `DataBitmap.populationDriftCheck` (called once per
checkpoint cycle from `plane_arm.writeKvdaPages`) read the maintained `population`, then popcounted the
words, then subtracted the allocator's `trim_windowed` — three reads, no exclusion. Every bit on an
armed holder moves inside ONE critical section of the bitmap's ordering lock (`carveRun`: 64
`holderTakeBit`s then `population += 64`; `noteFinishFree`: the bit clear then `−1`; `clearRun`,
`setRun`, the holding `takeOutOfList`), so a carve landing between the check's count read and its scan
showed `counted = maintained + 64`, and a free whose bit cleared before its word was scanned but
whose decrement came after the count read showed `maintained + 1` — the two torn pairs the check
called a drift, and the "re-sync" stored the torn count. A second pair sat in the trim window: the
window's bit was taken under the lock (`takeOutOfList(holds = false)`, the population untouched by
design) but `trim_windowed` moved after the unlock in `block_alloc.claimFreeForTrim` (and before the
lock in `returnFromTrim`), so a check between the two read `counted = maintained + 1` either way. No
path moves a bit without its count: every `tryTakeBit`/`clearBit` on an armed allocator runs through
the holder's faces under the lock (`holderTakeBit`, `holderPublishFree`, `holderTakeOutOfList`,
`holderReturnTrim`, `holderReturnBit`); the armed mint's own `tryTakeBit` is the carve's bit already
set; a page load re-syncs at its end. The reference had the same race (`data_alloc_bitmap.rs:604-619`)
and never saw a trim window in its holder bitmap (two bitmaps, 0002 row 238) — 0002 row 545.

**The fix** (`451bab6`; measured below as its pre-rebase twin `7122848`). The population ≡ the popcount of the words, every bit moving with it under
the ONE lock: a trim window's take adds one and its return subtracts one (`takeOutOfList`,
`returnTrim`), nothing reads `trim_windowed` any more, and the check holds the lock for its count read
and its scan — the one O(words) section the lock ever sees (4096 words per TiB at 4 MiB blocks,
microseconds; the spin lock yields every 64 spins). `blocks − population` is then the clear population
`HolderView` promises, and `reachableFreeBlocks` adds the windows back once instead of twice (the
armed path now agrees with the unarmed one, whose trim claim debits the budget). `population_scans`
counts the check's scan too (the reference's `population_scan` did): one per cycle per volume.

**Red → green** (`data_bitmap.zig`: "the maintained population IS the popcount …"). A 1 TiB-geometry
bitmap (2^18 blocks, 4096 words), blocks 0–1 minted and 1 freed to the list; single-threaded: a trim
window over block 1 raises `populationNow()` by one and the check holds (red on the old semantics:
"expected 2, found 1"); then a mutator thread runs 10 k laps of `carveRun(64, 64)` + `clearRun` + trim
claim/return while the test thread runs one `populationDriftCheck` per completed lap, over the next lap
in flight (never back to back: with the lock in place that starved the mutator — 100 s). Old code:
4005 of 4069 checks disagreed and fired (back to back: 8852 of 9019). Fixed: 0 of ~10 k, the tripwire
and `population_scans` deltas exact, 76 ms wall / 148 ms CPU (its cost row). `block_alloc`,
`checkpoint`, `data.plane`, `enospc`, `data_bitmap`, `sym_planes`, `block_grant`, `alloc_lease`,
`free_list` families: 233 passed, 3 skipped, 0 failed.

**On the mount** (RAM NVMe over nvme-loop, dist build, the venue held, `MX_RUNTIME=20`,
`sw1m-q16j4` then `sw4m-q4j4`; `.stats1.json` under `/build/perf/mx/ram-sqz-rpd-{A1,B1,B2}/`):

| binary | row | MiB/s | daemon cores | `data_alloc_population_drift` (cumulative) | `population_scans` |
|---|---|---|---|---|---|
| `ab75737` (before) | sw1m ×4 | 26 634 | 5.76 | **2** | 2 |
| `ab75737` (before) | sw4m ×4 | 28 619 | 5.52 | **3** | 2 |
| `7122848` (after) | sw1m ×4 | 26 715 | 5.79 | 0 | 308 |
| `7122848` (after, B2 re-run) | sw4m ×4 | 27 405 | 5.60 | 0 | 296 |

`invariant_tripwires` 0 after; fsck offline clean on every session. (B1's sw4m row read 16.4 GB/s with
20 cores of host `sys` time and the daemon at half its CPU — a third party on the host, not this lever;
B2 repeated the row alone.) The check's lock hold costs nothing visible: the checkpoint lane's cores are
unchanged (1.09 → 1.09, 1.05 → 1.02).

**Not verified here.** No full gate (the train's). The seqlock alternative (O(1) critical sections,
a retry on movement) was not built: it still needed the trim counter under the lock, and a PiB bitmap's
4 M-word scan at one lock hold per cycle (≈ 1–4 ms) was judged cheaper than a new lock-free core with
its model.
