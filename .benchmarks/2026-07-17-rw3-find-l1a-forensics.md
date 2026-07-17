# RW3 — FIND-L1-A forensics: the convoy is ALIVE, instrument-coupled, and convicted (2026-07-17)

**Charter**: PR RW3 of `docs/design-random-small-writes.md` (§5.3 W3, the
G-RW4 gate, the RW3 PR-plan row) — "FIND-L1-A convoy — closure (or
forensics + fix if it reproduces)". **It reproduces.** This note is the
forensics deliverable: the G-RW4 grid evidence (both masks, both
instruments), the historical-pair attribution, the conviction chain down to
the defective line, the H1–H4/H2b adjudication, preserved tapes, and the
recommended fix. **Per the RW3 charter the fix is NOT implemented here** —
it needs its own reviewed TDD PR. G-RW4 is **NOT closed**; the design doc's
§5.3/G-RW4 rows are deliberately left un-annotated.

**Headline**: FIND-L1-A never disappeared. It is invisible to the elbencho
harness (`tests/l1a_sweep.sh`) because elbencho aligns its O_DIRECT buffers;
it reproduces immediately under the L1 report's own instrument
(`squeezefs bench`), on current dev, at **−34 %** (t16 mb256 0.66× the mb12
band — wider than the original −25 %). The mechanism is a **write-path
completion-trigger defect** (`src/fuse_client.rs:4087`) exposed by any FUSE
WRITE stream whose per-block segments arrive split and out of order —
which is exactly what the kernel produces for unaligned-buffer O_DIRECT
writes under `FOPEN_PARALLEL_DIRECT_WRITES`. None of the design's H1–H4/H2b
is the mechanism; the tapes falsify each as primary.

## Provenance

| | |
|---|---|
| Tree | `perf/find-l1a-closure` off dev `e0f9f39` (harness knob `7b7a877`); dev release binary md5 `6d94e7c79d67dba11aa373f215d10995` (identical to RW2's acceptance binary — `e0f9f39` is docs-only over `71c4fba`); historical binary 15cc394 built in detached worktree `/var/tmp/rw3-prefix-worktree`, md5 `3501d7dd570ee896a2a68a7e8cddeae2`; both built `taskset -c 0-15`, `CARGO_BUILD_JOBS=12` |
| Box | the phase-1 box (25 online CPUs @ 3.5 GHz cap, 109 GiB RAM, nvme0n1 1.9T PC SN8000S, kernel 7.1.3-2-cachyos) |
| Rails | sandboxes under `/var/tmp/squeezefs_l1a` + `/var/tmp/rw3_l1exact` + `/var/tmp/rw3_micro_dev` (never `/mnt/squeezefs`, `/mnt/juicefs`, `~/tmp/nvme`); daemons in `systemd-run --user --scope` memcg cages (16 GiB — matching the L1 seq-isolation rows' registered depth=32 geometry); grid rows on-rail `taskset -c 0-15` AND off-rail full 25-CPU mask; 3-poll quiet gate + Tctl ≥ 86–88 °C hard pause (observed 49.2–57.8 °C, **every row `quiet`**, zero DIRTY); kills by PID only; elbencho 3.1-9 |
| Co-tenants | the user's standing `/mnt/juicefs` mounts + redis containers idle throughout (~0.2 % CPU, untouched); no rustc/cargo/foreign-elbencho during any timed row (builds strictly preceded measurement) |
| Measurement serialization | sole box owner; RW2 merged at `e0f9f39` before session start; no other 📊 branch active |
| Artifacts (preserved) | `/var/tmp/squeezefs_l1a/artifacts/rw3-grid-dev` (48-row elbencho grid, both masks); `/var/tmp/rw3_l1exact/artifacts-{dev,pre}` (L1-exact bench rows: per-row bench output, `.stats` before/after, diskstats, honesty lines, mount logs); `/var/tmp/rw3_micro_dev` (dd/bench/align-probe micro cells); `/tmp/rw3-forensics/` (probe scripts `micro_t1.sh`, `align_probe.py`, strace tape `bench_t1.strace`, copies) |

## 1. G-RW4 grid, elbencho instrument (`tests/l1a_sweep.sh`) — GREEN on both masks

Dev `e0f9f39`, seq shape (`elbencho -w -t $T -s 256m -b 1m --direct`, t
files), fresh volume per (rail, mb), 16 GiB cages, rig armed, n=3.
Median [min–max] MiB/s:

| rail | mb | t8 | t12 | t13 | t16 |
|---|---|---|---|---|---|
| on | 12 | 3,552 [3458–3584] | 3,912 [3823–3939] | 3,924 [3921–3965] | 3,873 [3761–3897] |
| on | 256 | 3,444 [3441–3559] | 3,893 [3778–3897] | 3,812 [3763–3815] | 3,948 [3888–4014] |
| off | 12 | 3,579 [3503–3779] | 3,828 [3753–4005] | 3,901 [3775–3935] | 3,837 [3811–3907] |
| off | 256 | 3,588 [3481–3674] | 3,924 [3815–4043] | 3,769 [3730–3846] | 3,984 [3834–4094] |

```
SIGNATURE rail=on  shape=seq t16/t8@mb256=1.15 mb256/mb12@t16=1.02 blw_ms_tail@t16=3 @t8=3 verdict=not-convoy-shaped
SIGNATURE rail=off shape=seq t16/t8@mb256=1.11 mb256/mb12@t16=1.04 blw_ms_tail@t16=5 @t8=1 verdict=not-convoy-shaped
```

`uring_queue_full = 0` across all 48 rows; `blw_ms_tail` 1–7 samples/row
(vs the L1 241-sample class). Read formally, the G-RW4 numeric clauses PASS
on this instrument (mb256@t16 +2 %/+4 % of mb12, both ≥ 0.95× t8) — **but
§3 shows this instrument cannot see the defect**, so the grid does NOT
adjudicate the gate's intent ("the mb256 write row joins the mb12 band" of
the **L1 report's table**, which is a `squeezefs bench` table). Honesty
note: this session's absolute band (3.4–4.1 GiB/s) sits ~50 % above RW1's
(2.37–2.59 GiB/s) on the same harness/box — RW2 landed in between; in-grid
ratios adjudicate, absolute bands are session-relative (not chased).

## 2. L1-exact recipe (the report's own instrument) — the convoy REPRODUCES on dev

Recipe reconstructed from the preserved phase-1 artifacts
(`~/tmp/iops_parity_3456308/l1_remount.sh` + `logs/l1w_*.log`): 1×1 GiB
sqmeta.img + 4×8 GiB data imgs (32 GiB volume, 4 MiB blocks), staging dir
(disk cache 10GB default), mount `--read-mem-cache-size 1G
--write-mem-cache-size 1G`, 16 GiB cage (the l1w rows registered depth=32 —
the 16 G-cage geometry), daemon+workload `taskset -c 0-15`,
`SQUEEZEFS_OP_PROFILE=1`, workload **`squeezefs bench <mnt> -w -t T -s 256m
--direct`** (fsync-inclusive timing — the L1 write-seq class), `def`
(mb256 policy) vs `-o max_background=12,congestion_threshold=9`
interleaved per rep, n=3. Harness: `/var/tmp/rw3_l1exact/run_l1exact.sh`.
fusectl verified per mount (mb=256/12 landed); r1 = fresh-create pass,
r2/r3 = overwrite passes (the interleaved steady state, as in the L1
isolation). Median [min–max] MiB/s:

| binary | cell | def (mb256) | mb12 | **mb256/mb12** |
|---|---|---|---|---|
| dev `e0f9f39` | t16 | **1,774** [1652–1875] | **2,671** [2517–2695] | **0.664 (−34 %)** |
| dev `e0f9f39` | t8 | 1,856 [1699–1880] | 1,749 [1734–1811] | 1.061 (noise) |
| pre-VS-B `15cc394` | t16 | **1,867** [1722–1972] | **2,673** [2533–2710] | **0.699 (−30 %)** |

Every FIND-L1-A boundary reproduces: `mb < writers` throttles it invisible
(mb12 band 2.5–2.7 GiB/s vs def 1.65–1.97), t8 collapses to noise,
`uring_queue_full = 0` everywhere, and the `block_lock_wait` ms-tail at
t16/def is 469–1,061 samples/row vs 323–378 at mb12 and ~356 at t8 — the
L1 "241-sample ≤16 ms tail" class, scaled by this box's faster device.
t16/t8@def = 0.956 (marginally at the gate line) while the mb boundary
fails massively — the mb pair is the load-bearing signature, as in L1.

**Historical-pair verdict**: the pre-FIND-VS-B binary shows the SAME convoy
(0.699 vs 0.664 — inside rep spread). **Nothing cured FIND-L1-A** — not
73b2654 (shard geometry), not FIND-VS-A, not SMO, not RW2 (whose patch
cannot fire on this shape: stream-adjacency guard + `PATCH_MAX_BYTES` =
512 KiB against ~1 MiB segments — confirmed live: `patch_ineligible_
{adjacent,oversize}` absorb every predicate evaluation, `patch_writes` = 0).
RW1's "does not reproduce" table was an **instrument artifact**, not a cure:
elbencho page-aligns its O_DIRECT buffers, `squeezefs bench` does not (§3).

## 3. The conviction chain (code anchors verified on `e0f9f39`)

1. **Instrument leg** — `squeezefs bench` writes through
   `tokio::fs::File::write_all`; tokio 1.52.3 copies every write into an
   **unaligned `Vec`** (`tokio/src/io/blocking.rs:198`,
   `Buf::copy_from`), defeating bench's own `AlignedBuf` before the
   O_DIRECT pwrite. strace (`/tmp/rw3-forensics/bench_t1.strace`): bench
   issues clean 1 MiB `write()` syscalls — the unalignment is the buffer
   address, not the size.
2. **Kernel leg** — an unaligned 1 MiB user buffer spans 257 pages; FUSE
   caps a WRITE at max_pages (256) and **splits the write into 2 FUSE
   WRITEs**; `FOPEN_PARALLEL_DIRECT_WRITES` (advertised,
   `fuse_client.rs:1460`) + mb256 dispatch both segments **concurrently**
   → per-block segments can arrive **out of order**. Measured: 8,192
   daemon checkouts for 4,096 user writes on every bench t16 row (2×);
   `patch_ineligible_oversize` ≈ interior-block-boundary count (segments
   straddle 4 MiB boundaries); dd/elbencho (aligned) = 1× checkouts.
3. **Daemon defect (THE CONVICT)** — `src/fuse_client.rs:4087`:
   `let is_block_complete = write_end == b_end_offset;` — the write-through
   trigger keys on **this write's own end**, not on the buffer's covered
   interval (the comment above it says "byte-identical to the old staging
   point" — that heritage is the bug). Under out-of-order segment arrival:
   - block-end segment arrives EARLY (coverage incomplete) → trigger fires
     on a `seed_deferred` buffer with partial coverage → **inline 4 MiB
     seed fetch** (`:4089-4119`, `write_path_seed_read_bytes`) on overwrite
     passes — device reads inside a sequential write;
   - block-end segment is NOT the last to arrive → **trigger never fires**:
     a fully-covered 4 MiB buffer parks forever and drains through
     fsync-flush or the `MAX_ACTIVE_BLOCK_BUFFERS=256` cap's inline
     victim spill — the 12 MiB/op RMW pipeline on a seq stream.
   - **Second face**: the fsync/close flush exit
     (`flush_memory_buffers_driven`, `:3542-3559`) fetches the 4 MiB seed
     image for EVERY `seed_deferred()` buffer **unconditionally — even
     when the buffer is fully covered** and `fill_complement_from` will
     apply zero of it. Measured ≈ 1.05–1.16 GiB/row of pure wasted device
     reads (`flush_seed_read_bytes` = `staging_put_bytes_flush` = 276–283
     × 4 MiB exactly).
4. **Scaling** — parked missed-trigger buffers × 16 writers convoy on
   `BLOCK_FLUSH_LOCKS` **same-key** waits: t16/def r2 tape shows
   `same_key_waits` 2,896 (942 ms-class) vs `cross_key_waits` 11;
   `write_checkout` site 951 ms-class; `seed_fetch` phase 1,008 samples
   (986 ms-class); `get_obj` 1,008 ≈ the whole 1,024-block dataset read
   back **during a pure sequential write**; `put_obj` 1,293 (~292 blocks
   uploaded twice); device read stream 1.0–1.7 GiB/s on r2/r3. At mb12 the
   kernel in-flight throttle shrinks the reorder window + parked
   population: `get_obj` 435, tail 323–378, +900 MiB/s. At t8 the parked
   population fits: collapse to noise. On-rail == off-rail (grid §1).

**Ledger deltas, t16 r2 (dev; slow vs fast)**:

| counter | def (slow) | mb12 (fast) | t8 def |
|---|---:|---:|---:|
| `overwrite_seed_materialized` | 1,008 | 435 | 446 |
| `get_obj` (device block reads) | 1,008 | 435 | 436 |
| `write_path_seed_read_bytes` | 2.66 GiB | 1.13 GiB | 1.18 GiB |
| `flush_seed_read_bytes` | 1.05 GiB | 0.46 GiB | 0.57 GiB |
| `staging_put_bytes_flush` | 1.05 GiB | 0.46 GiB | 0.57 GiB |
| `writeback_enqueued_flush` | 269 | 115 | 145 |
| `blw_ms_tail` (samples) | 951 | 363 | 360 |
| `uring_queue_full` | 0 | 0 | 0 |

**Isolation micro-probes** (t=1, rig-armed mount, tapes in
`/var/tmp/rw3_micro_dev` + `/tmp/rw3-forensics`):

| probe (64×1 MiB O_DIRECT, no fsync) | checkouts | seed materialized | flush enqueues | staging put |
|---|---:|---:|---:|---:|
| python, **page-aligned** buffer | 59 (≈1×) | 1 | 0 | 0 |
| python, **+512 B misaligned** buffer | 119 (≈2×) | 4 | 3 | 12 MiB |
| dd `oflag=direct` (aligned) | 59 | 1 | 0 | 0 |
| `squeezefs bench -w -t1` (tokio unaligned) | 119 | 3 | 2 | 8 MiB |

One variable (buffer alignment), no tokio/bench in the python pair — the
chain instrument→split→reorder→missed-trigger is proven end-to-end at t=1
and explodes combinatorially at t16.

## 4. Hypothesis adjudication (design §5.3)

| Hypothesis | Verdict | Evidence |
|---|---|---|
| H1 spawn_blocking sibling hop | **real but secondary** | `sibling_remove` 145 ms-class/8,192 on the slow row — present, not the −34 % |
| H2 upload hold-time under block lock | **adjacent, not as named** | the tail IS block-lock shadow, but the driver is missed-trigger parked buffers + inline seed fetches, not upload_full_block hold-time (`upload_dma` 1,024 samples in both fast and slow rows) |
| H2b stripe collisions | **falsified** | `cross_key_waits` 11 vs `same_key_waits` 2,896 — the 4096-stripe table is fine |
| H3 pool exhaustion | **falsified as primary** | `aligned_pool_misses` 61–248/row, `uring_queue_full` = 0 |
| H4 rail oversubscription | **falsified** | grid §1 identical on/off rail; bench rows all on-rail with 9 spare CPUs |
| **Actual mechanism (new)** | **convicted** | coverage-blind write-through trigger (`:4087`) + unconditional flush-exit seed fetch (`:3542`) under split/reordered parallel O_DIRECT WRITEs |

## 5. Gate + program adjudication

- **G-RW4: FAILED / NOT CLOSED.** The elbencho-instrument grid passes its
  numeric clauses but does not measure the L1 table's instrument; under
  that instrument the mb256 row sits at 0.66× the mb12 band on dev. The
  acceptance sentence ("the mb256 write row joins the mb12 band") is not
  achievable without the daemon fix below.
- **G-RW1: FINAL at RW2, unaffected — the deferral clause is MOOT** (stated
  per the RW3 charter): RW2 adjudicated G-RW1 FINAL with
  `verdict=not-convoy-shaped` on the rand shape, and this session's
  conviction does not touch that adjudication — the rand_write_4k rows ride
  the W1 patch path (no accumulation trigger involved), were measured with
  the aligned-buffer elbencho instrument, and their 13.3–15.0× W verdicts
  stand. No re-adjudication owed.
- **The L1 report's trade note updates**: the "−25 % on that one shape" is
  not an mb256-admission overhead — it is a standing write-path defect that
  mb12 happened to throttle invisible, and it taxes ANY unaligned-buffer /
  split-WRITE O_DIRECT writer regardless of mount options.
- **Instrument finding (separate, harness-tier)**: `squeezefs bench`'s
  O_DIRECT write phase does not deliver aligned buffers to the kernel
  (tokio copy). As a measurement instrument it under-reads seq O_DIRECT
  throughput by the split-write tax even after the daemon fix. Needs its
  own small charter (aligned pwrite via spawn_blocking or uring_fs) — but
  the daemon must stay correct under split/reordered WRITEs regardless:
  unaligned-buffer O_DIRECT is POSIX-legal on FUSE and the kernel may split
  WRITEs whenever it likes.

## 6. Recommended fix (NOT implemented — needs its own reviewed TDD PR)

1. **Coverage-complete trigger**: `is_block_complete` at
   `fuse_client.rs:4087` must consult the buffer's covered interval
   (`ActiveBlockBuf::covered()`, already maintained at `:4076
   record_write`) — fire write-through when coverage reaches the whole
   block (or `[0, min(block_end, i_size_end))` for the EOF tail), not when
   this write's end coincides with the block end. The EOF-tail and
   partial-coverage semantics must keep the item-B deferred-seed law
   (uncovered complement owes old bytes — never zeros).
2. **Coverage-aware flush-exit seed elision**: in
   `flush_memory_buffers_driven` (`:3542`), skip `fetch_seed_image` when
   the buffer is fully covered (the complement is empty) — kills the
   ~1 GiB/row unconditional flush-seed reads independently of (1).
3. **Tests-first (red suite)**: an out-of-order in-block segment delivery
   case (misaligned-buffer O_DIRECT writer at t≥2, or handler-level
   split-write injection) pinning `write_through_blocks == dataset blocks`,
   `write_path_seed_read_bytes == 0`, `flush_seed_read_bytes == 0` on a
   fresh seq stream; the §3 aligned/misaligned probe pair as a permanent
   regression pin; re-run BOTH instruments' grids for G-RW4 (the bench
   instrument is the gate's table; the elbencho grid is the regression
   fence).
4. **Watch items for the fix PR**: the trigger change interacts with
   `prev_write_end` stream-adjacency (`:3847`) and the zero-copy
   write-through law (complete-block DMA past staging) — G-RW3's seq rows
   + QUICK trio + kill-9 soaks are the fence; H1's sibling hop and the
   bench-instrument alignment are separate, smaller levers to file behind
   it.

## 7. Tape inventory (preserved)

- `/var/tmp/squeezefs_l1a/artifacts/rw3-grid-dev/` — 48-row G-RW4 elbencho
  grid (rows.tsv, per-row elbencho/.stats/disk/env), console log at
  `/var/tmp/rw3_l1exact/grid-dev.console.log`
- `/var/tmp/rw3_l1exact/artifacts-dev/` + `artifacts-pre/` — L1-exact bench
  rows (rows.tsv, per-row bench output/.stats/disk/env/del, mount logs)
- `/var/tmp/rw3_l1exact/run_l1exact.sh` — the reconstructed L1-exact
  harness (recipe documented in-file)
- `/var/tmp/rw3_micro_dev/` — dd/bench/align-probe micro sandbox (+ stats
  snapshots)
- `/tmp/rw3-forensics/` — `micro_t1.sh`, `align_probe.py`,
  `bench_t1.strace`, `bench_t1.out`, copy of the dev L1-exact artifacts
- Historical worktree removed after the pair (binary md5 recorded above);
  volumes/imgs under the sandboxes retained
