# Rung 10 — fresh blocks ride the rewrite epoch's save; the epoch closes at half the save's width (the lever after `rung-10-publish-churn.md`)

**Diagnosis** (this workstation, `scripts/perf/ramnvme.sh`'s RAM namespaces, dist builds, the whole venue
held per session; the fresh-file row = the matrix's lay-down shape run on its own: 4 jobs × 3 GiB fresh
files, 1 MiB O_DIRECT sequential, QD16, io_uring, no fallocate, no fsync; `~/tmp/ff/ff-run.sh`). A
file laid down for the first time published its inline map ONCE PER BLOCK: the overlay's settle fed the
rewrite epoch only on an overwrite (`old_binding`; the reference's `fresh_overlay_never_creates_a_shadow_
epoch_interaction`), so every fresh record took the durable merge — 3,068 merges for 3,068 blocks, 3,430
journal entries and 57.2 MB of journal per 12 GiB (18.6 KB per block: the record grows to 37 KiB), 54 MB
of node appends, the daemon at 0.9 cores of which the checkpoint's `CacheFlusher`/`smoReplace` 16 %, the
conveyor 8 %, allocation memsets and faults 19 %, the settle's merge 6 %. On the OVERWRITE rows the
timer's cadence was the only close of a streaming epoch, so a stream past the width per cadence paid the
per-block publish for the rest of the window: `rewrite_shadow_fallbacks` 25.5 % of the blocks on 1M seq
write ×1 (6,200 blocks/s, one ino), 7.2 % on ×4, 12.5 % on 4M ×4.

**What changed** (`337cddb`, `3dbca41`).
- **Every striped block FEEDS the epoch** (`device_overlay.settleLocked`, `accumulate`'s upload): an
  overwrite parks its displaced key, a fresh block parks nothing (`prev == null`), the save names both.
  The `rewrite_*` SLO terms stay the overwrite's; `overlay_epoch_feeds_fresh`, `rewrite_shadow_fresh_
  records` count the fresh feeds. The overlay stamps fsync's touched word at its settle (`Deps.touched`
  — the reference's overlay never did, so a fed block's namespace sat outside the data barrier before
  its save). Decision 0002 row 546; charter §6.2 items 1 and 3 amended; `data_write_path.md` §4, §5.4.
- **The close width** (`write_epoch.CLOSE_WIDTH` = `max_displaced / 2` = 106): the record that brings an
  epoch's shadow to the width answers the close-owed hint (`RecordVerdict.shadowed.close_owed`, once per
  epoch) and the guard-dropping caller closes it. Half the save's width, not the width: `record` bounds
  the open shadow united with the saving one at the width (a transient save folds the two), so an epoch
  closed at the full width would park every record behind its save; at half, the next epoch has the
  other half while the save lands (~ms against 40 ms of stream at 2,800 blocks/s). Tie test: 106; 2 × 106
  ≤ 213; the room under a saving epoch 107.
- **The ordered law's floor under an epoch** is its lowest shadowed block (`Epochs.lowestBlockOf`), not
  0: with fresh blocks riding the epoch an extending stream's epoch is open for the stream's life, and the
  whole-epoch 0 would have held the durable size at the first save.

**Red → green.** `write_epoch`: the width tie; a fresh stream of 2 × 106 + 5 records hints the close at
blocks 0 (its own coverage), 106 and 212 and nowhere else, ⌈N ÷ 106⌉ + 1 saves, nothing parked, nothing
freed, every block named; the hint fires once per epoch and a hint under a save in flight waits it out.
`device_overlay`: a fresh record FEEDS (0 publishes; 70 open records of one ino drain into ONE merge of
70). `accumulate`: a fresh write-through feeds; the fence at a fresh block's save is the epoch's W5.
`write_admit`: the floor under a fresh stream (blocks 1–2 fed ⇒ block 1's start; saved ⇒ lifts; a parked
partial below the epoch's lowest wins). Green: `write_epoch` 58, `data.*` 546, `fuse.*` 312, `ops.*` 328,
`meta.tx` 50, `kvmap` 53; smoke rows: the data plane WITH / WITHOUT a cache path, THE KILL MATRIX (every
acked byte exact or a hole, C8 0 drift, C2 0/0, fsck clean), kill -9 mid-tar (both cadences), THE ORDERED
LAW'S KILL ROW, `sync -f`, THE WRITE-BACK TIMER (three rows), THE BLOCK-MAP TREE's crossing kill.

**The fresh-file row** (A = `dev` `732a3da` in the main tree, B = this branch; MiB/s per pass; the
`.stats` deltas of a pass; the sessions `ff-{A,B}-r{1..4}` under `/build/perf/ff/`, A B B A).

| shape | A (per pass) | B (per pass) | merges/blk A → B | journal KB/blk | daemon cores |
|---|---|---|---|---|---|
| lay-down (extending, QD16) | 14,716 14,913 11,781 14,092 (+ matrix 11,725) | 13,170 13,199 12,203 14,059 (+ matrix 13,017) | 1.000 → 0.010 | 18.6 → 0.3 | 0.70–0.94 → 0.55–0.58 |
| the same at QD1 | 12,616 12,488 | 12,976 14,206 | 1.000 → 0.010 | 18.6 → 0.3 | 0.79–0.86 → 0.50–0.55 |
| fresh blocks, non-extending (files truncated to 3 GiB first) | 11,603 12,071 | 11,658 12,800 | 1.000 → 0.015 | 18.6 → 0.3 | 1.65–1.96 → 1.11–1.54 |

The metadata cost of a fresh block fell 98 % (3,430 → 125 journal entries and 57 → 0.9 MB per 12 GiB;
node appends 54 → 1.0 MB; `rewrite_shadow_fallbacks`, `save_waits`, tripwires 0) and the daemon's CPU 40 %,
**and the lay-down's bandwidth did not move: it was never bound by the publish.** QD1 lays down as fast as
QD16 (12.5–14.2 GB/s either way; the write's op latency 239–263 µs at QD1 in both legs), because the kernel
serializes an extending O_DIRECT write under the inode's exclusive lock — `fs/fuse/file.c:1431`
`fuse_dio_wr_exclusive_lock`: "Parallel dio beyond EOF is not supported, at least for now" — so a
lay-down is one 1 MiB round trip at a time per file (≈ 4 GB/s per file, four files ≈ 14 GB/s) while the
same shape over mapped blocks runs sixteen deep at 11.7 GB/s per file. The non-extending fresh row shows
the second bound: a cold write-pipeline depth governor (`write_pipeline_admission_waits` 78–200 k in the
one-second pass, op latency 4.5–5 ms, no probe-up) — the matrix's 20 s rows run after it has converged.
Both bounds are the transport's (a sqz series patch letting a server that orders its own sizes keep the
shared lock past EOF; the governor's cold start), not this lever's; B's per-write latency is A's or lower.

**The overwrite control** (`scripts/perf/quiet.sh ram-sqz`, `MX_ROWS='sw1m|sw4m'`, 20 s rows; MiB/s, the
daemon's cores with `sqz-ckpt`/`sqz-jrnl` in parentheses).

| row | A (`ffA-r1`) | B (`ffB-r1`) | B ÷ A | fallbacks A → B | journal KB/blk A → B | daemon cores A → B |
|---|---|---|---|---|---|---|
| 1M seq write QD16×1 | 24,858 | 28,906 | **1.16×** | 25.5 % → 0 | 9.8 → 0.7 | 1.85 (0.32, 0.22) → 1.57 (0.07, 0.10) |
| 1M seq write QD16×4 | 47,053 | 48,162 | 1.02× | 7.2 % → 0 | 3.1 → 0.6 | 5.32 (0.39, 0.29) → 5.18 (0.24, 0.17) |
| 4M seq write QD4×4 | 43,399 | 47,334 | **1.09×** | 12.5 % → 0 | 5.0 → 0.6 | 5.12 (0.44, 0.38) → 4.64 (0.22, 0.16) |

The width-close removes the fallback class (publishes per block 0.259 → 0.013 on ×1, 0.079 → 0.014 on ×4;
one save per 79–85 blocks) and the metadata device bytes with it; the ×1 row — one ino, the timer's cadence
its only close before — gains the most. The second matrix pair (`ffA-r2`, `ffB-r2`) is appended below
when it lands.

**What binds next.** (1) The lay-down's kernel lock (above): the sqz series could let the daemon opt out of
the exclusive lock for writes past EOF (the ordered law already keeps the size behind the data) — the
lay-down would then meet the governor and the overwrite's 47 GB/s. (2) The ×1 overwrite row's journal is
now the block-grant top-up under a drained free supply: 104 k grants of 1.4 blocks for 144 k blocks, each a
barriered structural write (`journal_lane_structural_writes` 107 k, `block_grant_topups` 104 k) — the
displaced keys sit in the free-grace ring while the 28 GiB set runs dry, so the grant size collapses to the
blocks the ring just returned. (3) The `.stats` document's `mincore` walk of the fill pool (14 % of the
daemon's samples under a 50 ms poll) is a measurement tax any harness that polls `.stats` pays.
