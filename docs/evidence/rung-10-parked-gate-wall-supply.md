# rung 10 — the parked gate's supply-visible wall and the joiner's 30 s bark are counts, not tripwires

**Finding** (release window, xfstests on `dev d46dd83`, kernel `7.3.0-1.7-mainline-sqz`). generic/299
(aio-dio writers + a falloc/truncate loop on the filled 2 × 8 GiB scratch) fired the must-stay-0
`parked_gate_wall_supply_visible` once: amid ~1,900 writes answered ENOSPC at the gate's wall on a
latched-exhausted set (614 parked, 1.29 GB; both volumes re-latching `fresh supply EXHAUSTED` dozens of
times as the truncates freed blocks), ONE wall in the same millisecond read supply in sight. generic/347
(a dm-thin pool queueing writes once out of space, `no_space_timeout` 60 s) fired it ×74, one per 2 s
tick, beside `upload_join_overlong` ×2: an fsync joined an upload whose DMA the pool held 60 s. Both had
been carried as owner-calls since `rung-10-fallocate.md`.

## Law 1 — the wall with supply in sight (`write_admit.awaitParkedCapacity`)

The wall answers ENOSPC iff `enospc.freshSupplyExhausted` over the candidates (unchanged, the charter's
terminal law); otherwise the write stays parked and the wall restarts (unchanged, never ENOSPC on a store
with supply — rung 9 F17). What changed is the CLASS: the census is one instant, and on a full store under
rewrite a free, a truncate, a trim return or a drain landing after the store refused the flushes shows
supply by design. So every such wall is the counter `parked_gate_wall_supply_visible`. Its stall sub-case:
a wall whose flushes met NO space refusal anywhere in the set (`GateWall.space_refused`, from the new
`BlockAllocator.last_storage_full_ns` read through `enospc.spaceRefusedSince(cands, flush start)`) had
supply in sight throughout and the plane refused the flushes anyway — a device failing its DMAs (dm-error),
a thin pool out of space (347), F17's grant ledger — counted `parked_gate_wall_plane_stalls` with ONE
warning per episode (`WritePath.gate_stall_warned`, closed when the gate next finds capacity). No "cannot
happen" remains at the site: a device refusing writes is the world's event. Not a stall of the gate's
making either: a supply-returned write retries its flush on its next tick (the flush's own mint parks on
visible supply); a plane stall is the op watchdog's station and, past the kernel's 135 s request timeout,
the fail-stop lattice's (347's connection was aborted at 145 s — seen, below).

## Law 2 — the joiner's verdict (`accumulate.uploadParkedBlock`)

A second flush of a block with an upload in flight JOINS it: its verdict is that upload's (a publish
retires the entry, `.joined`; a refusal leaves it parked and the joiner re-drives at once and answers the
plane's own refusal). It waits only on the upload's device phase, which a device may hold past the 30 s
bark (dm-thin's queue for 60 s; one that never answers ends at the kernel's request timeout, the fail-stop
lattice). So each bark is the counter `upload_join_overlong` and the episode says ONE warning
(`Table.join_overlong_warned`, closed when a barked joiner leaves); the per-bark error line is gone.

## Red → green

- `write_admit` "the wall on a FULL store whose supply RETURNS inside it (generic/299 …)": the refused
  station drives a full set — round 1's flush meets `StorageFull`, a block is freed and one DMA refused,
  round 2 crosses the wall with the block in sight, round 3 lands on it. Red on `d46dd83`:
  `INVARIANT TRIPWIRE 'parked_gate_wall_supply_visible'`, `invariant_tripwires` 0 → 1. Green: +1
  `parked_gate_wall_supply_visible`, +0 `plane_stalls`, no warning, no error line, no ENOSPC, the victim
  published at the freed block.
- `write_admit` "the wall with supply VISIBLE and no space refusal inside it" (was "… fires the tripwire"):
  every DMA refused on a set with free blocks; held at the wall round's station: +1 / +1, the episode flag
  set, no tripwire; healed: the write completes and the episode closes.
- `accumulate` "a joiner whose upload the device HOLDS past the bark (generic/347 …)": the DMA held by the
  device hook, the bark shortened (`Table.join_bark_ns`, 10 ms): ≥ 3 barks, ONE warning, the joiner answers
  `.joined` when the device answers. Red on `origin/dev f3e7262` with the real 30 s bark: the tripwire,
  `invariant_tripwires` 0 → 1.

Loops (100 each, the venue under two agents' builds, a gate and an fstests pass): the three wall tests
100/100 twice; the joiner 100/100. Unit: `write_admit` 83/83, `accumulate` 59/59, `enospc` 50/50,
`block_alloc` 68/68, `parked` 203/203; `zig build rails` green; `zig fmt --check` clean. Smoke: A FULL DATA
STORE ANSWERS ENOSPC (0 TRIPWIRE lines, 0 errors), FALLOCATE — EVERY MODE, the re-use row, THE WRITE-BACK
TIMER UNDER A WRITER STORM (gate waits 1,860, cold flushes 1,772) — green.

## The xfstests subset (`generic/299 750 475 027 347`, SUITE_RUNS=1, binary `a4d5d24`)

**Passed all 5 tests** (027 `[not run]`: `_scratch_mkfs_sized` unsupported); fsck `--offline --scrub` 0
findings on both sets; the ledger's rc 3 is the subset's stale-row check. Over the 56 scratch and thin-pool
daemon logs: **0 `TRIPWIRE` lines** (dev: 1 in 299; 74 + 2 in 347), 915 wall ENOSPC answers on the filled
scratch (the terminal law, unchanged), 0 plane-stall warnings, ONE joiner warning in 347 — its fsync
waited 55 s on the upload the pool held, the 60 s bark said nothing more (the episode's line), and this
time the pool's growth landed the writes before any gate wall.

## Seen, not fixed

generic/347's plane stall outlives the kernel: the write parked at the gate on a pool refusing every DMA
until the 135 s request timeout aborted the connection (the daemon fail-stopped, exit 1; the test passes —
it discards xfs_io's errors). Whether a plane stall should answer EIO before that deadline (the parked set
stays; the refused write was never acked) is an owner call; this change only classifies it. A barked joiner
re-drives once after a refused upload, doubling its wait on a device with a per-IO timeout. The subset's
347 daemon ended with `dismount promotion: … 10 FAILED` (the write cache's residents on the thin pool,
held by the supervisor's memfd for the next mount).
