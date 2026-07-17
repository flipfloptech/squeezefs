# RW4 — W2 extent-granular overlay, staged extent records, batched fold: G-RW6 CLOSED (2026-07-17)

**Charter**: PR RW4 of `docs/design-random-small-writes.md` (§5.2 W2, the
RW4 row — the program's only L-risk item, phase 2): the patch-INELIGIBLE
small-write shapes (compressed/encrypted volumes, refcount-shared/
decorated blocks, holes, unaligned) stop paying the whole-block RMW
pipeline. Extents park compactly in RAM, spill as staged `active_block_ext:`
records (no seed read at spill, ever), and fold into blocks lazily with
measured amortization. Includes the FIND-RW2-A fix (in-scope per the RW4
charter) and the ≤ block-size staged-layout rider.

## Provenance

| | |
|---|---|
| Tree | `perf/write-extent-overlay` off dev `a070836` (RED `4766886` → impl `76ee762` → this note + design annotations ride the closing commit); release binary md5 `5badc7cfba84469f616396afc4d99217`, built `taskset -c 0-15 CARGO_BUILD_JOBS=12` |
| Base comparator | dev `a070836` release md5 `1bc07a7fa185a6c78883af005574fbf0` (worktree build, same flags) |
| Box | the phase-1 box (25 CPUs, nvme0n1, kernel 7.1.3-2-cachyos), Tctl 51–58 °C at every quiet gate — zero thermal pauses |
| Rails | sandboxes `/var/tmp/squeezefs_rw4{,_kill9,_ab}` (never `/mnt/squeezefs`, `/mnt/juicefs`, `~/tmp/nvme`); daemons in `systemd-run --user --scope` **8 GiB** memcg cages, `taskset -c 0-15`; 3-poll quiet gate before every timed row; kills by PID; fresh volume per regime; sole box owner (no other 📊 branch active; fstests strictly serialized before the bench sessions) |
| Instruments | **fio 3.42** for the lz4 rows (`--buffer_compress_percentage=75` — see FIND-RW4-A: elbencho's incompressible payloads break compressed volumes on the BASE binary too); **elbencho 3.1-9** for the passthrough G-RW3 rows (the RW2/RW3b-comparable instrument) |
| Artifacts (preserved) | `/var/tmp/squeezefs_rw4/artifacts/base-20260717T051523Z` (G-RW6 before), `…/rw4-20260717T052456Z` (G-RW6 after + G-RW3), `/var/tmp/squeezefs_rw4_kill9/artifacts/20260717T053959Z` (kill-9 soak, 10 rounds), `/var/tmp/squeezefs_rw4_ab/` (FIND-RW4-A A/B logs) |

## TDD evidence

- **RED** (`4766886`): 20 contracts failed on their assertions against
  inert scaffolding (M1 convention — never compilation):
  `tests/extent_overlay_tests.rs` 13/13 red (`extent_parks` read 0, folds
  never ran, spills never happened, FIND-RW2-A fsync still errored
  `Invalid block offset`, rider counters 0) +
  `tests/extent_record_recovery_tests.rs` 7/7 red (no recovery sweep, no
  version gates, no clean-unmount drain).
- **GREEN** (`76ee762`): 20/20 + the full serial gate — clippy
  `-D warnings` clean, fmt clean, `cargo test --all-features --
  --test-threads=1` **916 passed / 0 failed**, doc 0 warnings, bench smoke
  ok, `tests/run_loom.sh` **27/27** (no new atomic protocol: the parked
  byte gauges ride the loom-modeled `gauge_core` saturating protocol;
  extent/coverage state is plain data under `BLOCK_FLUSH_LOCKS` — the
  RW3b precedent).

## 1. G-RW6 — the patch-ineligible floor: **CLOSED**

lz4 volume (4 MiB blocks, 4×12 GiB data slices), 16 GiB dataset over 16
files, fio randwrite 4 KiB t16 iodepth16, 30 s rows, n=3, compressible
payloads (75 %):

| row | BASE (a070836) IOPS | RW4 IOPS | × | BASE dev amp (R + W) | RW4 dev amp (R + W) |
|---|---:|---:|---:|---:|---:|
| r1 | 501 | **18,300** | 36.5× | 209× + 478× ≈ **687×** | 3.16× + 11.97× ≈ **15.1×** |
| r2 | 427 | **18,200** | 42.6× | 205× + 567× ≈ **773×** | 2.78× + 14.01× ≈ **16.8×** |
| r3 | 411 | **17,700** | 43.1× | 202× + 582× ≈ **785×** | 2.78× + 14.55× ≈ **17.3×** |

(диskstats-over-row ÷ fio user bytes; the base's ~700×-class is the §1.2
whole-block RMW pipeline in lz4 form — every write patch-ineligible by
transform.)

- **Amplification gate ≤ 150×: PASSED at 15–17× combined** (write leg
  ≤ 14.6×, read leg ≤ 3.2×) — ~10× inside the gate; the §4 arithmetic
  (amp ≈ 2048/k + spill legs) at the measured k ≈ 80 predicts ≈ 28× on
  4 MiB blocks before compression; lz4 (~4:1 on these payloads) lands it
  at the measured 15–17×.
- **`fold_fill` median ≥ 16: PASSED** — cumulative `fold_passes` 18,438,
  `fold_extents_folded` 1,478,862 ⇒ **mean fill 80.2**; histogram median
  bucket `<=128` (mid 96). `fold_seed_reads == fold_passes` exactly on
  every row (one seed per fold, never per extent); `get_obj` ≈
  `fold_passes` (the only reads left are fold seeds).
- Ledger truth per row: `extent_parks` ≈ 545–557 k (≈ ops), rand-4k
  overwrites never create block-size buffers; `extent_spills` ≈ 55 k
  (записи 4 KiB-class); `patch_writes = 0` (transform-ineligible, as
  designed); `extent_implicit_escalations = 0` (the designed-routes
  tripwire); zero `malformed transform frame` errors (compressible
  payloads).
- **Hole-write soak (sparse striped, fio randwrite into 4×1 GiB
  truncated holes): GREEN** — 10,900 IOPS (base 653), read amp **0.50×**
  vs base 3.87× with `get_obj` 4,209 seed reads in 19.6 k ops (**the
  seed storm**) → RW4 `get_obj` 1,474 ≈ its 1,442 fold seeds over 327 k
  ops (0.5 % — first-touch hole folds seed NOTHING; the residual seeds
  are re-dirtied previously-folded blocks, which are mapped and owe
  their old bytes).

## 2. G-RW3 protections — unmoved

Passthrough volume, elbencho (the RW2/RW3b instrument), same rails:

| row | RW4 | reference band | verdict |
|---|---:|---|---|
| rand_write_4k passthrough (t16 mb256) | **65,139 IOPS**, W amp 1.30×, `patch_writes` 1,954,591 ≈ ops, `extent_parks = 0` | RW2 61.5–66.7 k / RW3b 64.4 k | **in-family — W2 does not tax the default patch path** |
| rand_read_4k (t16) | **279,057 IOPS** | scoreboard 236–269 k | unmoved-or-better |
| seq_write_1m (t16) | **4,453 MiB/s** (dev W ≈ 1.08× user) | RW3b 3.7–4.2 GiB/s band | unmoved-or-better |
| seq_read_1m (t16, cold direct) | 3,729 MiB/s (dev R ≈ 1.08× dataset) | device-bound cold class | sane (no prior row on this exact instrument) |
| **mixed rand R/W** (8 readers f0–7 ∥ 8 writers f8–15, 30 s) | read **168,002 IOPS** + write **48,117 IOPS** concurrent | the R6/G-RW3 mixed clause | no starvation, no read-path tax |

`extent_records_recovered = 0`, `torn = 0`, `extent_implicit_escalations
= 0` across the passthrough session (the machinery is invisible where it
should be invisible).

## 3. Crash / torn / recovery evidence

- **Cargo crash suites**: crash_kill 8/8 with `SQUEEZEFS_CRASH_ROUNDS=60`
  deep churn; staged_crash_recovery 7/7; writeback_fencing_livelock 5/5;
  extent_record_recovery 7/7 (remount replay + loud orphan detection,
  stale-fencing discard, torn detected-and-ignored, future-version record
  refused-and-left, dir-level format gate per direction, clean-unmount
  zero-record drain, kill-9-around-the-fold old-block-intact).
- **Storm-shaped kill-9 loop, lz4 volume with parked extents + spilled
  records** (`/tmp/rw4_kill9_loop.sh`, SIGKILL at 3–6 s into each 8 s
  fio rand-write storm, 10 rounds): **10/10 GREEN** — every remount
  recovered its orphan population loudly (128–149 records/round,
  `EXTENT RECORDS AT MOUNT` stderr line), every file fully readable
  post-crash, drains clean; **2 genuinely crash-torn records detected
  and ignored loudly** (`EXTENT RECORD TORN … bad magic` — the ring
  write cut mid-record by the kill); final clean mount: **0 orphans**
  (the clean-unmount drain mandate holds live).
- **Downgrade matrix** (pinned per §5.2's honest mechanism, both
  directions): FUTURE dir-level staging format ⇒ loud unit refusal
  (mount construction fails naming the fence; custody never wiped);
  FUTURE record version ⇒ loud refusal, record left in place; pre-RW4
  dirs adopt-and-stamp v2; below-RW4 downgrade stays declared-unsupported
  with the forward-detection line asserted (the orphan sweep is that
  line, exercised 10×/10 in the soak).

## 4. fstests (root)

- Singles: **generic/075 pass** (14 s); **generic/213 fails with exactly
  the expected-table signature** (the standing ENOSPC-report class — the
  missing `fallocate: No space left on device` line, identical to the
  table's entry; recorded per the RW4 verify clause).
- `FSTESTS_QUICK=1`: **exact match with the expected table** — failures
  {generic/003, generic/213}, not-run {generic/009, generic/316}, all 15
  others pass (074/075/091/112/127/616/617/618 and the fsx/soak set
  green).

## 5. What landed (mechanism inventory)

- **`ExtentOverlay` repr** (`src/cache/active_block.rs`): payload slabs
  mirroring the RW3b coverage union EXACTLY (the union stays the single
  coverage/trigger source; debug-asserted lockstep); `extent()`
  constructors carry the item-B deferral; `escalate_to_full()` at ≥ 25 %
  coverage or large merges (RAM-only); `absorb_older_extent` fills
  coverage GAPS only (record custody absorbs under newer bytes); RAII
  byte gauges `parked_extent_bytes` / `parked_full_buffer_bytes` (the R5
  components; the extent side sheds via the parked drain, which FOLDS).
- **Byte budget** (`spill_parked_toward_cap`): the 256-count cap became
  `parked_cap_buffers() × block_size` BYTES (Red halves); extent victims
  spill as records (no seed, no image), full victims keep today's path;
  320 tiny overlays over an 8-buffer budget spill NOTHING (the convoy
  pin).
- **Staged extent records** (`src/cache/nvme.rs`): versioned +
  xxh3-checksummed `ExtentRecord` under `active_block_ext:` keys
  (4 KiB-padded block-family frames, occupancy-indexed, never
  budget-counted); dir-level `.squeezefs_staging_format` marker
  (`STAGING_FORMAT_VERSION = 2`) validated at construction — the
  forward-only fence; `patch_block_family_value` — the refusal-proof
  in-place record clip (truncate can never be refused by ring pressure —
  the FIND-VS-B class applied to the record kind).
- **Fold** (`fold_extent_block` + `fold_upload_block`,
  `src/fuse_client.rs`): seed once (staged-full sibling > item-B
  binding-validated `fetch_seed_image` > zeros for holes), apply record
  then RAM extents, one durable upload presenting the CURRENT generation
  (FIND-M11-A); triggers = fsync/close drain (mandate), count/byte
  thresholds (`SQUEEZEFS_FOLD_MAX_{EXTENTS,BYTES}`, default 64 / 1 MiB,
  lazy worker), R5 parked drain, teardown sweeps; never-lossy by
  construction (nothing removed until the merge published; overlay parked
  across every await — overlay never invisible).
- **Reads**: fuse single-block extent branch (covered ∩ range from slabs
  + complement from base/zeros); router single-/multi-block striped +
  staged ring/promoted legs compose records over every base; empty-map
  cost = one latch-free probe (R6).
- **Recovery** (`recover_extent_records`, init-time): generation-bound +
  fencing-stamped replay; stale fencing ⇒ discard (the remount law);
  torn ⇒ discard loudly; future ⇒ refuse-and-leave; orphan population ⇒
  the loud forward-detection stderr line.
- **FIND-RW2-A FIXED**: `read_nvme_block` decodes decorated `bk:off:len`
  mappings (exact LBA-rounded window + slice — the
  `read_promoted_staged_block` discipline) and `allocator_for_key`
  tracks the BASE offset's incarnation (`clean_block_key`), so
  deferred-seed materializes and folds over promoted-staged blocks work
  (pinned by `fold_seeds_decorated_promoted_mapping`).
- **Staged-layout rider** (`src/routing.rs` `write_file`): sub-image
  (≤ 25 %) non-extending overwrites of ring-resident staged files merge
  into the block-0 record (generation-bumped against racing promotions);
  every whole-image path folds-first and retires in-commit; promotions
  **defer** on record-bearing files (transient by construction);
  truncate clips in place; reads/clones compose. Two adjacent
  pre-existing races surfaced by the ring-pressure storm suite and fixed:
  `write_file` re-resolves the layout identity UNDER the block-0 guard
  (a stale pre-lock snapshot degraded the RMW seed), and the record clip
  is refusal-proof (above).
- **Patch predicate 2** gains the record probe (a record is an overlay —
  an in-place patch under it would be re-folded over).
- Stats families (§5.4): `parked_extent_bytes`,
  `parked_full_buffer_bytes`, `extent_parks`, `extent_escalations`,
  `extent_implicit_escalations` (must stay 0), `extent_spills`,
  `extent_spill_bytes`, `extent_record_absorbs`, `fold_passes`,
  `fold_seed_reads`, `fold_extents_folded`, `fold_fill` (histogram),
  `staged_rider_extent_writes`, `staged_rider_folds`,
  `extent_records_{recovered,stale_discarded,torn_discarded,
  future_refused}`.
- **Suite posture** (the RW2 §7 convention): rand_write_amp / rig_off /
  staged_rmw_alloc / mem_budget phases D+F / writeback flush pins moved
  their storms to ≥ 25 %-of-block (or ≥ 25 %-of-image) shapes — that
  machinery still owns every extent-ineligible shape;
  `tests/extent_overlay_tests.rs` + `tests/extent_record_recovery_tests.rs`
  own the extent twins. `tests/l1a_sweep.sh` gained
  `SQUEEZEFS_L1A_COMPRESSION` (harness-only).

## 6. Findings

- **FIND-RW4-A (pre-existing, NOT an RW4 regression — A/B-proven on base
  `a070836`)**: compressed (lz4) volumes CANNOT hold incompressible
  4 MiB blocks — the lz4 frame of incompressible data expands past the
  4 MiB allocator chunk (measured claim 4,210,758 B vs lz4 worst-case
  4,210,789 B for 4 MiB), the write lands anyway, and every read of such
  a block fails loud (`malformed transform frame: claims N image bytes,
  4194300 available`) — base and RW4 fail identically (24/24 frame
  errors on a 1 GiB elbencho seq create, zero kills involved). elbencho
  payloads are incompressible by construction, which is why the G-RW6
  instrument is fio with `--buffer_compress_percentage=75`. Fix belongs
  to the transform/allocator owner (store-uncompressed fallback or chunk
  slack — likely a format-visible decision); out of RW4's charter.
- **Named residual (crash-window stale-duplicate record)**: a kill-9
  inside the μs window between a whole-image commit and its record
  retire leaves a stale-duplicate record whose extents are already in
  the published image; re-application is idempotent unless the same
  range is overwritten post-remount BEFORE any fold — bounded by the
  checkout/absorb law (new writes absorb the record gaps-only, so newer
  bytes always win in RAM) and by the first fsync draining it.
- **Analyzer note**: `/tmp/rw4_analyze.py`'s `user` estimate for MiB/s
  rows is IOPS-based and wrong for the seq rows; the seq amp numbers in
  §2 are recomputed from the row's real byte totals (16 GiB dataset).

## 7. Gate adjudication

| Gate | Verdict | Evidence |
|---|---|---|
| **G-RW6** (compressed-volume rand_write_4k ≤ 150×; `fold_fill` median ≥ 16; hole-write soak green) | **CLOSED** — 15–17× combined amp (write ≤ 14.6×); fold_fill median bucket ≤128 (mid 96), mean 80.2; hole soak green with 0.5× read amp and no seed storms; IOPS 411–501 → 17.7–18.3 k (~40×) | §1 |
| G-RW3 (rand-read + mixed rand-R/W + seq rows + passthrough rand-write unmoved) | **PASSED** — 279 k rand-read; mixed 168 k R ∥ 48 k W; seq 4.45 GiB/s; passthrough rand-write 65.1 k in RW2's 61–67 k family with `extent_parks = 0` | §2 |
| Crash/torn/downgrade | **PASSED** — suites + 10/10 kill-9 storm rounds + live torn-record detections + per-direction downgrade pins | §3 |
| fstests | 075 pass; 213 = expected-table signature; QUICK == expected table exactly | §4 |
| Cargo gate + loom | clippy/fmt/doc/bench clean; 916/0 serial; loom 27/27 | TDD section |
