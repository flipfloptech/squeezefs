# Rung 10 — small files go through the write cache (2026-10-28)

Decision 0003 §CD; charter §6.1 "Inline"/"Staged", §6.2 the write cache's claim. The owner's ruling:
"small files should go through the write cache". Branch `feat/small-files-write-cache`, measured on `dev` `7693bab`, rebased onto `d80407c` and re-checked
(below).
**No benchmark was run**: counters from correctness runs (`~/tmp/small-files/sfrig.sh`: 2 MDT + 2 OST on
512-B loop files under `/build/tmp`, supervised, pid-tagged mountpoint, `SQUEEZEFS_MEM_BUDGET_MB=65536`
pinned, `SQUEEZEFS_OP_PROFILE=1`; device bytes from the loop devices' `/sys/block/*/stat`, the rest from
`.stats` around each phase; base and branch interleaved per shape; fsck `--offline` clean, 0 tripwires, 0
error lines after every run). Per file, mdtest `-n 10000 -u -i 1`, create phase unless named.

## What changed

- **The write** (`kv_fs.writeArm`): on a mount with a write cache an inline file's write takes the staged
  arm — rung in RAM, ONE commit naming the staged record, no payload journaled. Inline stays for a mount
  without a cache, a record-sized file (≤ `layout.inlineRecordMax` = 102 B, a packed tenant's own records),
  and a write the ring refuses (`fuse_write_staged_ring_full_inline`; it took a whole 4 MiB block before).
- **The placement** (`pack.dispatch`, `plane.promoteLocked`): packed above 102 B; inline ≤ 102 B, with
  packing off, and when no data volume has the tenant's block (`promotions_inline_fallbacks`: never
  ENOSPC for a small file; the full-store episode stays open).
- **The bulk passes** (`staging.runPass`, `Cache.evictShardsUntil`, `plane.drainTask`): syncfs, the
  dismount pass and the pressure drain run one promotion per CPU at once, so tenants share a flush.

## Red

kv_fs "a small write on a mount WITH a write cache…" (the 4 KiB write committed inline; then the 102 B
write rang); pack "the two-faced law…" (512 B / 4 KiB dispatched inline); layout "the promotion's inline
bound ties to the wire…"; staging "the bulk passes run `hooks.width` promotions…"; cache
"evictShardsUntil…"; plane "the bulk promotion passes' width…".

## Counters (base `7693bab` → branch)

| shape | metadata-device B | journal B (entries) | journal_full_stalls | CREATE / WRITE mean |
|---|---|---|---|---|
| -np 32 `-w 4096` | 10,255 → **1,901** | 4,432 → **346** (2.0) | 27–30 → **0** (5 runs each) | 197 / 315 → 67 / 87 µs |
| -np 64 `-w 4096` | 10,643 → **1,608** | 4,433 → **347** | 65–67 → **0, 0, 0, 1** | 431 / 791 → 213 / 276 µs |
| -np 32 / 64 `-w 512` | 2,980 / 2,821 → 1,845 / 1,606 | 847 → 346 | 1 / 14 → 0 / 1 | — |
| `-w 16384` (staged both) | 1,855 / 1,609 → 1,875 / 1,553 | 346 → 346 | 0 / 3 → 0 / 3 | 95 / 208 → 96 / 200 µs WRITE |
| `-w 64` (record-sized) | 1,830 → 1,833 | 398 → 398 | 0 → 0 | equal |
| plain create | 817 / 730 → 816 / 747 | 181 → 181 | 0 / 0 → 0 / 1 | equal |

WRITE p50 ≤ 64 µs, p99 ≤ 512 µs at -np 32 on the branch: the staged arm's (the `-w 16384` reference).
**Persisted** (128,000 files: create, `sync -f`, cold remount, mdtest `-E`, cold remount, `cat`): a 4 KiB
file's promotion costs 2,089 B of metadata + one 4,096 B slot and 0.18 data flushes (syncfs 2.2–2.7 s; one
promotion at a time it was 33–40 s, 1.00 flush per file, and the watchdog's syncfs > 5 s lines); promoted
inline instead (the old dispatch) 18,300 B, 8 journal-full stalls, a 10 s syncfs. 512 B: 1,590 + 2,053 B +
the slot against 2,732 B inline — a persisted sub-KiB file costs more, a short-lived one less (§CD.4).
**Cold reads**: a packed file is one 4 KiB data read (1.0 IO/file); an inline one was in the nodes the
mount loaded (7,024 B of metadata per 4 KiB file at mount against 1,432 B packed). **fsync per file**
(mdtest `-y`, 16,000 files): 611 → 610 µs mean; metadata 14,389 → 1,678 B (512 B: 4,169 → 1,672 B), +
4,096 B data and 0.40 data flushes per file.

## Checks

Unit filters (`--summary all`): `kv_fs` 125, `fuse.handler` 195, `data.staging` 63, `data.plane` 50,
`data.pack` 54, `data.layout` 61, `data.fsync` 48, `data.writeback` 44, `write_admit` 90, `mem.cache` 51,
`cli.mount` 42, `e2e` 100 (20 skipped by kernel feature) — green; `zig build rails` 32/32, `zig fmt
--check` clean. Smoke rows (37 + the 39 always-run checks, 116/117): the data plane WITH and WITHOUT the
write cache, THE KILL MATRIX, KILL -9 BETWEEN A PROMOTION'S COMMIT AND ITS RING RELEASE, kill -9 mid-tar -x
(both cadences), THE KILL ROWS (a)–(c), THE REPLAY PROOF, UNDER THE WRITE-BACK TIMER fsync/fdatasync/syncfs,
`sync -f` IS A DURABILITY POINT, AN ORDERLY UNMOUNT LANDS EVERY ACKED WRITE (both), ALTERNATING APPENDS FROM
TWO MEMBERS (both), A JOINED MEMBER'S APPEND (both), TWO MOUNTS OF ONE SET, 300 CONCURRENT FSYNCS AND 300
CONCURRENT CREATES, THE WRITE CACHE'S RECALL, THE STAGED READ'S IDENTITY WINDOW, both SYNCFS-MEETS rows,
FSCK AND SCRUB, A JOINER'S CLEAN UNMOUNT AND REJOIN, A MEMBER JOINS A SET WHOSE MANAGER WAS KILLED, INODE
FLAGS, KERNEL AIO, NO WRITE CACHE WITHOUT A SUPERVISOR, THE ORDERED LAW'S KILL ROW, the guest-slot power-cut
row, THE TRANSFORMED APPEND, A MOUNT THAT RACES THE PREVIOUS UNMOUNT (both), A SUPERVISOR HOLDING CUSTODY —
green; A FULL DATA STORE ANSWERS ENOSPC green on its third run (below). xfstests generic/001 013 074 075 112
127 263 through `scripts/suites/fstests.sh`: 7 of 7 passed, fsck `--offline --scrub` 0 findings on both
sets, 0 alert lines (the subset's "did NOT pass" is the stale-ledger check alone). On the rebased head: the same
unit filters, rails and fmt green; the 37 smoke rows in three bounded runs 156/156 (THE FULL DATA STORE row
3,891 admitted); the xfstests subset 7 of 7, 0 findings.

## Seen, not changed (§CD.8, §CD.10)

An unsynced small file is lost to a power cut until promoted (its durable size reads zeros), where the
inline arm lost ≤ 50 ms. A small resident is charged 12 KiB of ring. `writeback.promoteStaged` has no
product caller. THE FULL DATA STORE row's fill bound fails under a host-wide `sync(2)` mid-fill (2 of 3 on
the branch; green on the third and on `7693bab`). The first measuring rounds ran capped by the venue's
cage and beside another agent's `sync(2)` loop (21:30–22:21): superseded by the pinned, interleaved round.

## The A-B-B-A (2026-10-29, quiet host on AC, the RAM-NVMe perf mount, 2 MDT + 2 OST, dist builds)

`dev` `0778bb4` against `e0c0596`, three rounds of legs A B B A, two mdtest shapes per leg
(`-n 10000 -u -i 1`, plain and `-w 4096 -e 4096`), gate lock and every build slot held, tctl ≤ 70 °C,
fsck clean after all 48 runs; k ops/s, the mean of all legs (`dev` 5–6, branch 6):

| row | plain -np 32 | plain -np 64 | 4 KiB/file -np 32 | 4 KiB/file -np 64 |
|---|---|---|---|---|
| file creation | 253 → 253 (0 %) | 258 → 262 (+2 %) | 64 → **98 (+53 %)** | 56 → **94 (+68 %)** |
| file read | 913 → 880 (−4 %) | 942 → 915 (−3 %) | 849 → 828 (−2 %) | 515 → **869 (+69 %)** |
| file removal | 322 → 321 (0 %) | 346 → 343 (−1 %) | 324 → 314 (−3 %) | 283 → **370 (+31 %)** |
| directory creation | 277 → 285 (+3 %) | 257 → 258 (0 %) | 293 → 278 (−5 %, one 250 k leg) | 255 → 263 (+3 %) |
| directory removal | 288 → 289 (0 %) | 298 → 295 (−1 %) | 291 → 285 (−2 %) | 289 → 291 (+1 %) |

The stat rows (kernel-cached, 1–13 M/s) swing ± 30 % between `dev`'s own legs and are not listed. One
`dev` run (round 2, 4 KiB/file -np 32) WEDGED: every in-flight op parked past `meta_backend_start` /
`write_locked`, the supervisor SIGABRTed the daemon — the inline arm's journal-full stall that never
cleared; that shape no longer journals payloads on this branch, and the wedge itself is fixed on its own
branch (`fix/journal-full-wedge`, decision 0003 §CM). Landed (gate 166 green, 75/75). Sessions:
`/build/perf/sf-abba/`, `sf-abba2/`, `sf-abba3/`.
