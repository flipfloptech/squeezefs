# Rung 10 — close is no durability point: the write-back timer (0003 §AJ)

The owner's ruling: **"Yes: drain on a bounded timer."** Closes become local; an acknowledged
unsynced write leaves daemon memory within `T_wb`, or at fsync / fdatasync / syncfs / unmount.

**What changed.** `SQUEEZEFS_WRITEBACK_POSTURE=timer|close` (default `timer`; `close` = §AF's drain,
its rows pinned to it). Under `timer`: FLUSH answers ENOSYS — the kernel latches `fc->no_flush`, the
daemon's own close of `.stats` at ready being the latch (a zero-message open carries the kernel's
`FOPEN_KEEP_CACHE` only, so no reply could add `FOPEN_NOFLUSH`; CREATE and the control arm's OPEN
answer it); RELEASE drains nothing. Every write notes its ino with its custody's birth in a per-stripe
min-heap (`open_table`); ONE meta fiber per flush cadence runs `timerPass`: every ino born by
`now − A` drains through `drainAged` — overlay records and the epoch whole, parked blocks born by the
cutoff (`flushParkedBlock`, §AK's in-place drain included), younger blocks re-owed at their birth.

**The bound.** `T_wb = A + τ + D`: `τ` = the flush cadence (50 ms), `A` = the checkpoint's cadence
ceiling (1 s), `D` the pass's drain — 1,050 ms + D, the 1,100 ms landing ceiling of a dirty metadata
leaf when `D ≤ τ`. Measured residency (birth → landed): mean 1,026 ms, max 1,046 ms (smoke);
max 1,050 ms under the 16-writer storm at a parked cap of 8. Memory: the write-custody components
(writers past the cap park at the gate or cold-flush: 263 / 263 in the storm row). `A = τ` was
tried first: IOR landed in-progress blocks partial under host load, 1.27 device bytes per user byte
(its first A leg on a loaded host; the B legs 1.00).

**Red rows** (run against dev's product): smoke "THE WRITE-BACK TIMER — CLOSE IS LOCAL" (40 closes:
8 FLUSH drains, 34 worker serves, 8 demotions; `expected 0, found 8`), "… fsync, fdatasync AND
syncfs ARE STILL DURABILITY POINTS" (`s4` drained at its close), "… UNDER A WRITER STORM" (closes
drained: the cap never met); session e2e "the write-back timer's close" (32 closes, 0 requests);
unit rows in `open_table`, `lifecycle`, `kv_fs`, `write_admit`. The crash oracle is at write grain:
each FUSE WRITE whole or absent (zeros, or past a length reverted to a write boundary — §AL's
`data=ordered`), never torn, never foreign bytes; a 1 MiB + 1000 B `pwrite` lands in part.

**The A-B-B-A** (perf container 4 vCPU / 12 GiB, dist build, 2 rounds = 8 legs, A = timer):

| row | A mean | B mean | A ÷ B |
|---|---|---|---|
| open/close, 1 / 4 threads | 1.81 M / 1.66 M | 38.3 k / 651 k | 47.4 / 2.54 |
| open + 4 KiB read + close, 1 / 4 threads | 1.29 M / 1.58 M | 38.3 k / 579 k | 33.8 / 2.73 |
| mdtest dir create / stat / rename / remove | 7,185 / 4.42 M / 7,521 / 6,627 | 5,764 / 3.67 M / 5,377 / 6,286 | 1.25 / 1.21 / 1.40 / 1.05 |
| mdtest file create / stat / read / remove | 6,286 / 3.23 M / 2.14 M / 7,005 | 5,904 / 3.61 M / 222 k / 6,789 | 1.06 / 0.89 / 9.66 / 1.03 |
| randread 4 KiB QD32 (cache / cache-less) | 141 k / 147 k | 145 k / 126 k | 0.97 / 1.16 |
| randwrite 4 KiB QD32 IOPS (cache / cache-less) | 63.1 k / 54.2 k | 96.1 k / 90.0 k | 0.66 / 0.60 |
| randwrite device ÷ user bytes | 1.32 / 1.36 | 0.13 / 0.14 | — |
| IOR write / read MiB/s (cache) | 1,580 / 2,864 | 2,248 / 3,063 | 0.70 / 0.93 |
| IOR write / read MiB/s (cache-less) | 2,236 / 3,790 | 2,280 / 3,167 | 0.98 / 1.20 |
| IOR device ÷ user bytes (both sets) | 1.000 | 1.000 | 1.00 |
| `echo >> f` × 500, cache-less: device ÷ user | 1.09 (41.0 KB) | 1.20 (45.1 KB) | 0.91 |

The host ran other agents' gates throughout (legs vary 2×). The cache set's IOR write (0.70) ran
right after randwrite, whose timer arm displaced ~5 GB of blocks into the reclaimer; the cache-less
IOR (0.98) did not reproduce it. **Randwrite** is the price: under `close` fio's close was the first
durability point — 15 s of O_DIRECT writes lived in RAM, coalesced 7:1; landing them within 1 s
re-parks each block every second (`overwrite_seed_materialized` 286 seeded 4 MiB uploads,
`write_pipeline_supersessions` 227 of them freed by the next write, 0.95 GB). Owed: a seeded
buffer's drain lands its dirty pages in place; a superseded upload publishes its snapshot.

**Findings attributed away** (`docs/gates/flakes.md`): `active_block_table_full` fires at
`SQUEEZEFS_PARKED_BUFFERS=8` under 16 `head -c` writers in BOTH postures (W2 entries hold table
slots without cells); `fsck --offline` after a kill refused once (`tree 0 … routing loop`, 1 of 11).
The timer never moves when a size commits (an extending write's size claim still commits per write).

**Gates.** One full venue gate through the lock on `3017dd8`: 49/49 steps, 2,930 / 3,012 tests
(82 skipped), after a 2 h wait on two hung `NEVER PARKS HOLDING` unit runs that held both build
slots at 0 CPU (killed by pid). Rebased onto `f15ec5e`: rails + the targeted rows 22/22 steps,
149 / 155. macOS gate (before 114af1d retired it) 29/29, 2,301 / 2,330.
