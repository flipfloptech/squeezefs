# RW3b — coverage-based write-through trigger + covered flush-seed elision: FIND-L1-A FIXED, G-RW4 CLOSED (2026-07-17)

**Charter**: the fix PR that RW3's forensics earned
(`.benchmarks/2026-07-17-rw3-find-l1a-forensics.md` — the conviction this
note closes). PR `fix(write): coverage-based write-through trigger +
covered flush-seed elision`, branch `fix/write-through-coverage-trigger`
off dev `b1d7a28`. Commits: `9d17383` (red suite) → `2682dd0` (fix) →
this note + design-doc closure.

**Headline**: on the L1 report's own instrument (`squeezefs bench`, the
exact recipe that reproduced the convoy at **0.664×** on dev), the fixed
binary reads **t16 def/mb12 = 1.026 (off-rail) / 1.006 (on-rail)** with
the mechanism ledger zeroed — `get_obj = 0`, `write_path_seed_read_bytes
= 0`, `flush_seed_read_bytes = 0`, `staging_put_bytes_flush = 0`,
`write_through_blocks =` exactly the dataset block count, and the
FIND-L1-A `block_lock_wait`/same-key ms-class tail collapsed from the
951-sample class to 9–42/row. **Both cells rose** (def t16 on-rail
1,774 → 3,777 MiB/s, +113 %; mb12 2,671 → 3,756, +41 % — the flush-seed
waste taxed mb12 too, as predicted). Elbencho grid confirmation:
`not-convoy-shaped` on both masks, rows at/above RW3's dev band. G-RW4
**CLOSED**; G-RW3 spot rows unmoved; fstests singles pass.

## Provenance

| | |
|---|---|
| Tree | `fix/write-through-coverage-trigger` @ `2682dd0` off dev `b1d7a28` (== RW3 forensics merge); release binary md5 `41c72c4d6b7aba1aa400277b596616d1`, built `taskset -c 0-15`, `CARGO_BUILD_JOBS=12`, pinned at `/var/tmp/rw3b_l1exact/squeezefs-rw3b` for every timed row |
| Box | the phase-1 box (25 online CPUs @ 3.5 GHz cap, 109 GiB RAM, nvme0n1 1.9T PC SN8000S, kernel 7.1.3-2-cachyos) — same box and cap as the forensics |
| Rails | sandboxes `/var/tmp/rw3b_l1exact` + `/var/tmp/squeezefs_rw3b_l1a` (never `/mnt/squeezefs`, `/mnt/juicefs`, `~/tmp/nvme`; RW3's tapes under `/var/tmp/rw3_l1exact` + `/var/tmp/squeezefs_l1a` + `/tmp/rw3-forensics` preserved untouched); daemons in `systemd-run --user --scope` 16 GiB memcg cages; 3-poll quiet gate + Tctl ≥ 86 °C hard pause (observed 52.4–61.6 °C, **every row `quiet`**, zero DIRTY); kills by PID only; elbencho 3.1-9 |
| Co-tenants | the user's standing `/mnt/juicefs` mounts + redis containers idle throughout; no rustc/cargo/foreign-elbencho during any timed row (the release build strictly preceded measurement) |
| Measurement serialization | sole box owner; no other 📊 branch active; dev == `b1d7a28` for the whole session |
| Harness | `/var/tmp/rw3b_l1exact/run_l1exact.sh` — RW3's `run_l1exact.sh` verbatim + a RAIL knob (on = `taskset -c 0-15` daemon+workload, the L1/RW3 posture; off = full 25-CPU mask) + per-row counter-delta extraction. Same volume/mount/cage/workload/interleave: 1×1G sqmeta + 4×8G data (32 GiB, 4 MiB blocks), staging dir, `--read/write-mem-cache-size 1G`, `SQUEEZEFS_OP_PROFILE=1`, `squeezefs bench <mnt> -w -t T -s 256m --direct`, def (mb256) vs `-o max_background=12,congestion_threshold=9` interleaved per rep, n=3, r1 = fresh-create pass, r2/r3 = overwrite passes |
| Artifacts (preserved) | `/var/tmp/rw3b_l1exact/artifacts-fix-{off,on}/` (bench rows: per-row bench output, `.stats` before/after, diskstats, honesty lines, mount logs, rows.tsv); `/var/tmp/squeezefs_rw3b_l1a/artifacts/20260717T013957Z` (seq grid) + `20260717T014530Z` (rand rows) |

## 1. The fix (what changed; contract in the red suite)

Convicted mechanism (forensics §3): `is_block_complete = write_end ==
b_end_offset` — one write's end as a proxy for block completeness. Under
kernel-split + `FOPEN_PARALLEL_DIRECT_WRITES` out-of-order segment
arrival the proxy (a) fired on PARTIAL coverage → inline 4 MiB seed fetch
inside a sequential write, and (b) never fired for the write that
COMPLETED coverage → fully-covered buffers parked and drained through the
fsync-flush/cap-spill seed + staging + writeback 12 MiB/op RMW pipeline.

Fix (all under the existing `BLOCK_FLUSH_LOCKS` discipline — no new
locks/atomics on the data path; loom 27/27 unchanged):

1. **Coverage-based trigger** (`src/cache/active_block.rs` +
   `src/fuse_client.rs` write path): `ActiveBlockBuf` tracks the
   overlap-safe UNION of written ranges — a primary run (the only state
   an in-order stream ever touches, no allocation) plus a rare sorted
   overflow of disjoint out-of-order runs (`active_block_ooo_runs`, new
   stats-inode counter). `record_write` returns `true` exactly at the
   union-complete transition — order-blind, once per covering stream —
   and that is the write-through trigger. Content-validity (zeros/seed
   safety) is tracked independently, so Seeded buffers still park on
   partial overwrites (no per-merge re-upload).
2. **Inline write-path seed fetches DELETED** (both faces): the
   gap-materialize arm and the partial-coverage trigger arm are gone —
   gap writes record a disjoint run and the item-B deferral holds.
   `write_path_seed_read_bytes` is now a **must-stay-0 tripwire** (the
   `patch_edge_rmw_reads` pattern).
3. **Covered flush-seed elision — structural**: `seed_deferred ⇒ union
   partial` (every full-coverage transition clears the deferral), so the
   flush/spill/self-flush/teardown seed fetch is reachable only for
   genuinely-partial blocks; a fully covered buffer can never pay one.
   The item-B law is intact both ways: partial coverage keeps deferring
   (nothing fetches at write time) and a truly-partial block's flush
   MUST fetch its seed (pinned).
4. **Coverage-aware read** extended to disjoint runs: zero-copy slice
   inside any covered run; Fresh gap reads compose zeros + runs; deferred
   gap reads materialize (overlay-never-invisible unchanged).

`FOPEN_PARALLEL_DIRECT_WRITES` stays enabled; W1's
`try_sole_owner_patch` predicate/fence untouched (suites green,
tripwires re-checked below). Behavior consequence pinned in the updated
`write_through_tests`: partial fills that merely END at the block
boundary now PARK (the old proxy's early upload of e.g. tail-first
`[2M,4M)` fresh fills is gone — they drain via the fsync exits with
multi-gap zero-complete), which is exactly the trigger movement the
out-of-order correctness requires.

**Red suite** (`tests/write_through_coverage_tests.rs`, committed RED at
`9d17383`, 6/8 failing on dev — the 2 passes are the designed
hold-the-line pins): end-first two-segment fills (dev: get_obj=12 per
6-block pass, early fires, flush pipeline), in-order control, 3-segment
shuffles (both shuffle classes incl. the disjoint-run bridge), partial
end-aligned segment never-fires + flush-must-seed (item-B both ways),
overlap/rewrite union exactness, fresh-create out-of-order (the bench r1
face), reads-during-disjoint-window (old bytes / zeros in gaps), and the
crash pin — kill mid-accumulation with out-of-order segments → remount →
old durable blocks byte-intact (dev FAILED it: the early-fired trigger
had half-uploaded the end-aligned segment durably pre-crash — got 0xe9
at 2·BS+HALF).

## 2. G-RW4 re-gate, bench instrument (the gate's own table)

Median [min–max] MiB/s, n=3 per cell, fresh volume per run, every row
quiet:

| binary | rail | cell | def (mb256) | mb12 | **def/mb12** | t16/t8 @def |
|---|---|---|---|---|---|---|
| dev `e0f9f39` (forensics) | on | t16 | 1,774 [1652–1875] | 2,671 [2517–2695] | **0.664** | 0.956 |
| dev `e0f9f39` (forensics) | on | t8 | 1,856 [1699–1880] | 1,749 [1734–1811] | 1.061 | — |
| **fix `2682dd0`** | **off** | t16 | **4,123** [3121–4207] | 4,020 [3892–4062] | **1.026** | **1.128** |
| **fix `2682dd0`** | **off** | t8 | 3,654 [3613–3671] | 3,724 [3692–3865] | 0.981 | — |
| **fix `2682dd0`** | **on** | t16 | **3,777** [3074–3804] | 3,756 [3704–3786] | **1.006** | **1.102** |
| **fix `2682dd0`** | **on** | t8 | 3,427 [3354–3432] | 3,428 [3413–3469] | 0.999 | — |

**GATE: PASS on both masks** — def/mb12 ∈ [0.95, 1.05] (1.026 / 1.006)
and t16 ≥ 0.95× t8 (1.128 / 1.102). Both cells rose vs the dev session
(on-rail t16: def +113 %, mb12 +41 % — the flush-seed waste taxed mb12
too). Honesty: absolute bands are session-relative on this box (RW3's
own note: elbencho bands moved ~50 % between RW1 and RW3 sessions);
in-grid ratios adjudicate. The min outliers (3121/3074) are each the r1
fresh-create pass (allocator/staging warmup — see §3 residue note);
medians sit on r2/r3, the L1 steady state.

## 3. The mechanism ledger (t16 r2 overwrite pass — dev vs fix)

Dev columns from the forensics note (§3 ledger); fix columns from
`artifacts-fix-off/rows/l1exact.fix-off.{def,mb12}.t16.r2.env`:

| counter | dev def (slow) | dev mb12 | **fix def (off-rail)** | **fix mb12** |
|---|---:|---:|---:|---:|
| `get_obj` (device block reads) | 1,008 | 435 | **0** | **0** |
| `write_path_seed_read_bytes` | 2.66 GiB | 1.13 GiB | **0** | **0** |
| `flush_seed_read_bytes` | 1.05 GiB | 0.46 GiB | **0** | **0** |
| `staging_put_bytes_flush` | 1.05 GiB | 0.46 GiB | **0** | **0** |
| `writeback_enqueued_flush` | 269 | 115 | **0** | **0** |
| `overwrite_seed_materialized` | 1,008 | 435 | **0** | **0** |
| `overwrite_seed_skipped` | — | — | **1,024** (= every dataset block) | **1,024** |
| `write_through_blocks` | — | — | **1,024** (exactly once/block) | **1,024** |
| `blw_ms_tail` (samples) | 951 | 363 | **22** | **15** |
| same-key ms-class tail | 942-class | — | **18** | **12** |
| `active_block_ooo_runs` | n/a | n/a | 643 | 235 |
| checkouts (`staging_sibling_probes`) | 8,192 (2×/write) | — | 7,936–8,192 (still ≈2×/write) | 8,192 |
| `uring_queue_full` | 0 | 0 | 0 | 0 |
| device read stream during row | 1.0–1.7 GiB/s | — | **0–11 MiB/s** | 2–4 MiB/s |

Reading: the kernel still splits every unaligned-buffer 1 MiB write into
2 FUSE WRITEs (checkouts stay ≈2×/write — expected; that is the
bench-instrument residual below), and the reorder is REAL and measured
(`active_block_ooo_runs` 235–891/row) — but the coverage trigger absorbs
it: zero seed reads anywhere, one write-through per block, no flush
pipeline, no read stream inside a pure sequential write, ms-tail gone.

r1 (fresh-create pass) residue, recorded honestly: one off-rail def r1
row drained 16 file-tail-class stragglers through the flush exit
(`flush_seedB` 16 MiB, `stg_flushB` 64 MiB, `seed_mat` 16, `wt` 992/1024)
— the item-B "16 file-tail partials" class on the creation pass;
r2/r3 (the L1 steady state) are 0 across the board on every row.

## 4. G-RW4 elbencho grid confirmation (aligned instrument — regression fence)

`tests/l1a_sweep.sh`, t{8,16} × mb{12,256} × both masks, n=3, 16 GiB
cages, rig armed, fresh volume per (rail, mb) — medians MiB/s:

| rail | mb | t8 | t16 | SIGNATURE |
|---|---|---|---|---|
| on | 12 | 3,640 | 4,038 | — |
| on | 256 | 3,531 | 4,058 | `t16/t8@mb256=1.15 mb256/mb12@t16=1.00 blw_ms_tail@t16=3 @t8=1 same_key@t16=0` → **not-convoy-shaped** |
| off | 12 | 3,676 | 3,992 | — |
| off | 256 | 3,792 | 4,096 | `t16/t8@mb256=1.08 mb256/mb12@t16=1.03 blw_ms_tail@t16=1 @t8=0 same_key@t16=0` → **not-convoy-shaped** |

Rows sit at/above RW3's dev band (3,444–3,984 med) — not regressed, not
convoy-shaped, `uring_queue_full = 0`, per-row blw ms-tails 0–4 (RW3 dev
grid: 1–7).

## 5. G-RW3 spot rows

- **seq_write_1m class**: the §4 grid IS the class (elbencho `-w -b 1m
  --direct`); mb256 t16 4,058–4,096 med vs RW3 dev 3,948/3,984 —
  unmoved-or-better.
- **rand_write_4k class** (rig ledger equivalent of the scoreboard row —
  the same elbencho shape RW2's G-RW1 adjudication rode): rail=on,
  mb256: **t16 = 64,438 IOPS med** [45,064–71,067], t8 = 64,798
  [62,714–65,111]; signature `t16/t8@mb256=0.99` → not-convoy-shaped.
  RW2's scoreboard evidence: 61,510 / 63,870 / 66,657 IOPS — in-family,
  unmoved.
- **RW2 ledger tripwires re-checked** on the t16 mb256 rand rows:
  `patch_writes` 1,933,578 / 1,356,207 / 2,132,203 vs row op counts
  1,933,312 / 1,355,776 / 2,131,968 (**≈ ops**, +0.01 % snapshot-edge
  skew); `patch_edge_rmw_reads = 0` on every row;
  `patch_write_bytes / patch_writes = 4096` exactly;
  `write_path_seed_read_bytes = 0`.

## 6. Correctness gates

| Gate | Result |
|---|---|
| Red suite (8 scenarios) | RED on dev: 6/8 fail (counters + early-fire + crash pin; 2 designed hold-the-line passes) → **8/8 green on the fix** |
| Updated write-through contracts | `write_through_tests` 29/29 (coverage-trigger matrix: tail-first parks, gap = disjoint run, middle-last single write-through, ooo union unit tests) |
| Full serial cargo gate | **896 passed / 0 failed** (`--all-features -- --test-threads=1`) |
| clippy -D warnings / fmt / doc | clean / clean / 0 warnings |
| bench smoke | ok (`cargo bench --benches -- --test`) |
| loom | **27/27** (no new atomics — coverage set is plain data under `BLOCK_FLUSH_LOCKS`, same as the old `covered` pair; run per the item-B precedent) |
| Targeted suites (explicit) | extent_patch 18, writeback 10, writeback_fencing_livelock 5, hole_read_zeros 7, sparse_write_bounded 5, small_write_zero_copy 3, staged_truncate_stale 6, write_visibility 14, striped_overwrite_lazy_seed 10 (item B), copy_file_range 5, staged_crash_recovery 7, crash_kill 8 — all green |
| fstests singles (root) | generic/013 (5s), 074 (52s), 075 (17s), 112 (18s), 616 (22s) — **"Passed all 5 tests"** against the pinned fixed binary (md5 verified); the design's RW3 rows (013/074) + the write-path quartet remainder. Quartet clean ⇒ the conditional aged-fsx ×3 not triggered |

## 7. Gate adjudication

- **G-RW4: CLOSED.** The acceptance sentence ("the mb256 write row joins
  the mb12 band" of the L1 report's `squeezefs bench` table) is
  delivered on that instrument, both masks, with the mechanism ledger
  zeroed — plus the elbencho fence not-convoy-shaped. Annotated in
  `docs/design-random-small-writes.md` (gate row + §5.3 CLOSED) and the
  normative trigger definition updated in
  `docs/design-zero-copy-write-path.md` §5.3.
- **G-RW1**: no re-adjudication owed (RW2 adjudicated FINAL,
  not-convoy-shaped on the rand shape; forensics §5 stated the deferral
  clause moot). The §5 rand rows here are a spot confirmation, not a
  re-adjudication.
- **G-RW3**: spot rows unmoved-or-better; full-scoreboard confirmation
  remains RW5's closing sweep as planned.

## 8. Named residuals (recorded, NOT implemented here)

1. **H1 sibling-hop** (real-but-secondary, forensics §4): the
   per-checkout `spawn_blocking` staged-sibling probe/remove
   (`sibling_remove` 145 ms-class/8,192 on the slow dev tapes; still
   ≈2 hops/write post-fix because the kernel still splits). Candidate
   fix: extend RW2's lock-free staged-existence probe to elide the hop
   on every striped write. Files behind its own measured row.
2. **Bench-instrument aligned-buffer charter** (tool honesty, forensics
   §5): `squeezefs bench`'s O_DIRECT write phase delivers tokio-copied
   UNALIGNED buffers (tokio `write_all` copies into an unaligned Vec,
   defeating bench's own `AlignedBuf`), so the kernel splits every 1 MiB
   write into 2 FUSE WRITEs and the instrument under-reads seq O_DIRECT
   throughput by the split tax even post-fix (checkouts ≈2×/write on
   every bench row above). Fix shape: aligned pwrite via
   `spawn_blocking` or `crate::uring_fs`. The daemon stays correct under
   split/reordered WRITEs regardless (that is what this PR pinned).

## 9. Tape inventory (preserved)

- `/var/tmp/rw3b_l1exact/artifacts-fix-off/` + `artifacts-fix-on/` —
  G-RW4 bench-instrument rows (rows.tsv; per-row bench/.stats/disk/env)
- `/var/tmp/rw3b_l1exact/run_l1exact.sh` + `squeezefs-rw3b` (md5 above)
- `/var/tmp/squeezefs_rw3b_l1a/artifacts/20260717T013957Z/` — seq grid
  (both masks); `…/20260717T014530Z/` — rand rows + tripwire stats
- RW3's tapes untouched: `/var/tmp/rw3_l1exact/`,
  `/var/tmp/squeezefs_l1a/artifacts/rw3-grid-dev/`,
  `/var/tmp/rw3_micro_dev/`, `/tmp/rw3-forensics/`
