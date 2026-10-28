# Rung 10 — a new file's first 4 KiB write: the overlay chunk bounded by bytes (2026-10-28)

Decision 0003 §CA. Branch `perf/first-write` off `dev` `8b14376`. Finding 1 of `rung-10-perf-matrix-9811f84.md`:
mdtest `-w 4096 -e 4096` created 25 k files/s against 241 k plain, 23 k under the interposer — ≈ 1.3 ms per
file per rank, in the daemon. **No benchmark was run**: correctness runs only (`~/tmp/first-write/fwrig.sh`, the
create-path rig with `-w 4096 -e 4096`: 2 MDT + 2 OST on 512-B loop files under `/build/tmp`, supervised,
`mpirun mdtest -n 10000 -u -i 1`, `SQUEEZEFS_OP_PROFILE=1`, `.stats` around each phase, `perf record -g` and
`perf stat` of the daemon's `mmap`/`munmap`/`mremap` mid-phase; fsck `--offline` clean and 0 tripwires after
every run). The dev arm is `0dc163c`'s binary (code-identical to `8b14376`). The owner's A-B-B-A judges the rates.

## Where the 1.3 ms went

The sequence through FUSE is CREATE, WRITE, nothing else: under the write-back timer posture FLUSH answers
ENOSYS and RELEASE drains nothing, and a 4 KiB write at offset 0 of an empty file is the INLINE arm — one
commit carrying the layout record with the payload and the inode Δ(size, times). Per file (-np 32, dev): CREATE
529 µs, WRITE 776 µs mean; `leaf_lock_hold` **377 µs per tx** (6.7 µs on the plain create); daemon CPU 375 µs per
file; the host 65 % iowait. The op-handler histograms had no other phase: `handler_to_backend` 47–59 µs, the
rest `backend`. Hypotheses (a)–(e) died on counters: 0 barriers waited, FLUSH/RELEASE absent, 1.0 drained tx and
2.0 journal entries per file in both arms, no promotion.

`perf -g` named it. The daemon's profile was kernel: `osq_lock` 8.4 %, `rwsem_down_write_slowpath` 3.1 %,
`rwsem_spin_on_owner` 4.4 %, TLB-flush IPIs, `clear_highpages`, `__zap_vma_range` — the process's `mmap_lock`,
held for WRITE. Its callers: `writeInline → commit → applyMembers → applyLockedRefs → RunEdit.insert → copyChunk →
Chunk.copyRange → ensureTotalCapacityPrecise → PageAllocator.map → mmap`; `Chunk.insertRec →
ensureUnusedCapacity → PageAllocator.realloc → mremap`; `publishSnapshot → Arc(Chunk).destroy → munmap`; and the
same three under `createChild`'s commit. The node's open-delta overlay (`node_snapshot.Run`) chunks its records
by COUNT alone (`CHUNK_MAX` 32, sized for ≈ 100 B inode and dentry records); 32 inline layout records are a
≈ 134 KiB arena, past `smp_allocator`'s largest slab class (32 KiB), so every apply's chunk copy under the leaf
lock was an `mmap` (the lock, 33 page faults zeroing fresh pages), every in-place growth an `mremap`, every
publish a `munmap` with a shootdown to every CPU — and the 32 lanes, the journal threads and the checkpoint thread
all queued on one lock. Counted at -np 64 over 3 s: 1.68 mmap, 1.65 munmap, 0.62 mremap per file.

## The change (`src/meta/node_snapshot.zig`)

`CHUNK_BYTES_MAX = 16 KiB` of live arena bytes beside `CHUNK_MAX`. A chunk past either bound, with two records
to split, splits at its byte midpoint into two compacted copies, and a half still over full splits again; a
record larger than the bound sits alone and maps pages as before. `insertRec` grows the arena precisely (a growth
factor near the bound crossed the class). `Run.fromSorted` cuts by the same bound. 16 KiB keeps the copy —
`live + the record` — inside the 32 KiB class with two in-place inserts of slack; small records never reach it.

## Red

- `node_cache` "the RAM apply stays inside smp_allocator's slab classes on the inline layout's shape…": 320
  applies of 4 KiB xattr-keyed records on one leaf; the largest block the cache's allocator saw must stay
  < 32 KiB and every chunk's live bytes ≤ the bound + one record. With the count bound alone the largest block is
  ≈ 134 KiB: red. Green with the byte bound.
- `node_snapshot` "a chunk is bounded by bytes as well as by records…": 96 inserts of 4 KiB values — every chunk
  within the bound + one record, arena ≡ live bytes ≡ capacity after every edit, the run ≡ the flat array,
  `fromSorted`'s cut, a 48 KiB record alone in its chunk, never a one-record split.

## The counters

| | dev -np 32 | fix -np 32 | dev -np 64 | fix -np 64 | plain create (dev / fix) |
|---|---|---|---|---|---|
| CREATE mean | 529 µs | 247 µs | 1,124 µs | 490 µs | 85.7 / 80.5 µs |
| WRITE mean | 776 µs | 307 µs | 1,943 µs | 927 µs | — |
| `leaf_lock_hold` per tx | 377 µs | 11.4 µs | 549 µs | 12.4 µs | 7.2 / 6.7 µs |
| daemon CPU per file | 375 µs | 247 µs | 400 µs | 337 µs | 92 / 89 µs |
| mmap / munmap / mremap per file (3 s window) | — | — | 1.68 / 1.65 / 0.62 | 0.25 / 0.22 / 0.004 | — |
| fc phase wall (320 k / 640 k files) | 21.2 s | 13.6 s | 45.3 s | 22.7 s | 4.2 / 4.1 s |
| `journal_full_stalls` | 26 | 26 | 79 | 66 | 0 |

After the fix the latency MODE is the plain create's (WRITE ≤ 64 µs bucket 18 k → 121 k of 640 k at -np 64;
CREATE 53 k → 185 k); the mean is a tail of ring-full stalls, each parking a volume's committers for up to
~150 ms (700 queue waits in the ≤ 256 ms bucket). The remaining mappings are all the checkpoint thread's (the
frozen bset images, the O_DIRECT read buffers, SMO successor images — one per node per cycle). fsck clean and 0
tripwires on every run; the plain phase unchanged.

## Checks

Unit filters `node_snapshot` 56, `node_cache` 78 (in-process), `record` 466, `meta.tree` 56, `conveyor` 116,
`smo` 216, `leaf_merge` 49, `economy` 71, `meta.backend` 87, `checkpoint` 121, `kv_fs` 123, `meta.sym` 357,
`journal` 196 — green; `zig build rails` and `zig build fmt` clean. Smoke rows green (status `w`, fsck clean,
0 tripwires): THE KILL MATRIX, KILL -9 BETWEEN A PROMOTION'S COMMIT AND ITS RING RELEASE, THE ORDERED LAW'S
KILL ROW, kill -9 mid-tar -x (both cadences), THE REPLAY PROOF, ALTERNATING APPENDS FROM TWO MEMBERS (both),
300 CONCURRENT FSYNCS AND 300 CONCURRENT CREATES, TWO MOUNTS OF ONE SET ON ONE HOST, UNDER THE WRITE-BACK
TIMER fsync/fdatasync/syncfs, `sync -f` IS A DURABILITY POINT, THE UNLINK STORM'S INODE LAW, THE MINT WINDOWS
UNDER A 32-STREAM CREATE STORM, AN ORDERLY UNMOUNT LANDS EVERY ACKED WRITE (both), FSCK AND SCRUB, THE KILL
ROWS (a) and (b). xfstests generic/001 013 074 075 112 127 263 through `scripts/suites/fstests.sh`: 7 of 7
passed, fsck `--offline --scrub` 0 findings on both sets, 0 alert lines (the subset run's "did NOT pass" is
the stale-ledger check alone).

## Seen, not changed (§CA.5)

The journal ring's economy under inline payloads — ≈ 13 KiB of metadata-device writes per 4 KiB file (the
entry, the leaf append, the leaf's CoW split share) against a staged file's ≈ 300 B record and a packed slot;
the 32 MiB ring turning every ~75 ms under 64 ranks against an 83 ms checkpoint flush on the loop files — is what
parks committers now; `--meta-journal-mb`, a bounded flush pass (§BY.5 (a)) and a write-cache posture for a fresh
file's first write ≤ `inline_max` are the owner's levers. The checkpoint thread's own per-node page mappings
(`fastMemset` 6.8 % of the profile after) want a pooled, pre-faulted buffer set. `__vdso_clock_gettime` is 10.6 %
of the profile (the turn spin).

## The A-B-B-A (2026-10-28, quiet host on AC, the RAM-NVMe perf mount, 2 MDT + 2 OST, dist builds)

`dev` `8b14376` against `6d00fc0`, legs A B B A, two mdtest shapes per leg (`-n 10000 -u -i 1`, plain and
`-w 4096 -e 4096`), gate lock and every build slot held, tctl ≤ 55 °C, fsck clean after all 32 runs;
k ops/s (leg 1 / leg 2). A first round that began on battery is discarded (its AC pair read the same shape).

| shape, row | -np 32 dev | -np 32 fix | Δ | -np 64 dev | -np 64 fix | Δ |
|---|---|---|---|---|---|---|
| 4 KiB per file, **file creation** | 24 / 23 | **64 / 64** | **+173 %** | 23 / 22 | **56 / 54** | **+147 %** |
| 4 KiB per file, file read | 833 / 758 | 868 / 819 | +6.1 % | 524 / 459 | 521 / 505 | +4.3 % |
| 4 KiB per file, file removal | 324 / 323 | 329 / 323 | +0.7 % | 261 / 234 | 296 / 274 | +14.9 % |
| 4 KiB per file, directory creation | 287 / 252 | 289 / 247 | −0.5 % | 260 / 256 | 244 / 266 | −1.0 % |
| 4 KiB per file, directory removal | 282 / 279 | 284 / 291 | +2.4 % | 299 / 292 | 293 / 257 | −6.9 % |
| plain, file creation | 245 / 250 | 254 / 254 | +2.6 % | 251 / 236 | 250 / 262 | +5.1 % |
| plain, file removal | 315 / 313 | 326 / 318 | +2.4 % | 331 / 355 | 344 / 356 | +1.9 % |
| plain, directory creation | 287 / 281 | 267 / 292 | −1.8 % | 267 / 241 | 286 / 274 | +10.2 % |
| plain, directory removal | 274 / 277 | 287 / 276 | +2.0 % | 287 / 295 | 293 / 290 | +0.2 % |

The stat rows (file and directory, 1–12 M/s, served mostly by the kernel's caches) swung −18 … +9 % in
both directions across the four shapes, inside the spread of `dev`'s own legs (7,845 / 8,736), and are
not listed. Landed (gate 158 green, 75/75). Sessions: `/build/perf/fw2-abba/` (and `fw-abba/`, the
discarded round).
