# 2026-09-02 — Finding 47: the device overlay's sub-cap length floor

**Branch** `fix/f47-overlay-length-floor` (worktree off dev `dafa82ca`).
RED `69786498` → fix `df2e8b9d`. Design: `docs/design-overlay-overwrite.md`
§5.1 (the length-floor clause) / §5.8 (the falsifier this closes);
contracts `tests/overlay_length_floor_tests.rs`.

## The conviction (read-only audit, dev tip dafa82ca)

`write_file_staged` (`src/fuse_client.rs`) runs the device-overlay arm
(`try_overlay` → `try_device_overlay_store`) BEFORE the W2 extent park
(`try_extent_park`), and the overlay shape screen had **no minimum
length**: single-block ∧ 4 KiB-aligned ∧ passthrough ∧ no verification.
So a sub-cap write (≤ `patch_max_bytes()` = block_size/8, the W1 class
boundary) that the W1 patch DECLINED at *state* time — hole
(`patch_ineligible_unmapped`), clone-shared (`shared`), decorated,
co-writer plane-gate refusal, stream-adjacent — never reached W2. It
took the overlay: a fresh CoW dest **minted and held per touched block**
across the drain interval, one `overlay_stores` DMA at the request size,
and at settle a whole-block old-image read + the whole complement seeded
as gap runs (`settle_overlay_block_locked`), then a rewrite-epoch feed
per record. Field row: `.benchmarks/2026-08-17-mw-shipped-free-c8-fix.md`
— six refused 4 KiB patches ⇒ `overlay_gap_seed_old_bytes` +25,141,248 =
6 × 4 MiB − 24,576, i.e. one 4 KiB write per record, 4 MiB read + 4 MiB
written each. The hold gate (`overlay_hold_eligible`) carved out only
MAPPED sub-cap writes; fresh-shape sub-cap slots were held for the
overlay.

Fat #2 from the same ledger: small-bs SEQUENTIAL O_DIRECT segments
(4–256 KiB, `patch_ineligible_adjacent`) rode the overlay **per op** —
one `WRITE_FIXED` + claim + continuation per segment, `wareq-sz`
collapsing to bs — instead of accumulating in the `ActiveBlockBuf` into
one whole-block write-through (the pre-overlay, request-size-preserving
behaviour).

## The fix — one derived predicate

`overlay_length_eligible(len) = cap == 0 || len > patch_max_bytes()`
(`src/fuse_client.rs`, beside `patch_max_bytes`). This is literally W1
predicate 5's oversize verdict, so the two fast paths **tile** the
sub-block population at the derived cap: `≤ cap` ⇒ W1 in place when
eligible, else the W2 extent park / accumulation; `> cap` ⇒ the
overlay/accumulation class (the 1 MiB+ segment A-leg the design was
built for). Applied at the handler's authoritative shape screen (counted
`overlay_ineligible_sub_cap`, BOTH shapes) and at the hold gate (a
sub-cap slot follows the patch ladder's hold rules or extracts at
delivery; the mapped-only carve-out is subsumed and deleted).

**Composition with `SQUEEZEFS_PATCH_MAX_BYTES=0`:** cap 0 empties the W1
class, so every length is overlay-eligible — exactly as it makes none
patch-eligible. The lever keeps meaning "no W1", never "no overlay"
(pinned: `patch_cap_zero_makes_every_length_overlay_eligible`).

**Why `>` and not `≥`:** the task's own definition of sub-cap is
`≤ patch_max_bytes`; `len == cap` is admitted by predicate 5 and is
therefore W1's. `>` is the exact complement, so no length sits in both
classes and none in neither.

## Red → green

| Contract (`tests/overlay_length_floor_tests.rs`) | dafa82ca | df2e8b9d |
|---|---|---|
| 4 KiB into a HOLE of a striped file rides W2, never the overlay (`overlay_installs` Δ0, `extent_parks` +1, `fold_passes` +1 at fsync, zero gap seeds) | **RED** (installs +1) | green |
| 4 KiB to a CLONE-SHARED block (refcount 2 via whole-file CFR) rides W2; `overlay_gap_seed_old_bytes` Δ0; clone snapshot untouched | **RED** (overwrite installs +1) | green |
| above-cap aligned overwrites (16 KiB, whole block) still take the B4 overlay (2 installs, 2 stores) | green | green |
| the floor tracks the block size: the SAME 16 KiB hole write is overlay-class at 64 KiB blocks and W2-class at 256 KiB blocks | **RED** | green |
| cap 0 ⇒ every length overlay-eligible; the predicate's tiling (`!eligible(cap)`, `eligible(cap+4K)`) | green | green |
| 64-op rand-4k hole burst (4 blocks × 16 pages, 1 MiB block): no mint per touched block, 64 parks, 4 folds, device write bytes 16× ≤ 30× | **RED** (4 installs) | green |
| fat #2: 16 × 4 KiB stream-adjacent segments ⇒ ONE `write_through_blocks`, zero overlay stores | **RED** (16 stores) | green |

Neighbour suites: `device_overlay_tests`, `overlay_ack_early_tests`,
`write_visibility_tests` ran on the 512 KiB cell default at 64 KiB
blocks (every write sub-cap by that number); their harnesses now apply
the mount's own derivation (`set_patch_max_bytes(derived_patch_max_bytes
(BS))`) and their overlay-path fixtures use overlay-class segments
(BS/4 or 16 KiB — the 8 KiB storm chunk is the W1/W2 program's by
definition now). The S11 rung-16 composed range pin
(`range_shared_span_refuses_both_patch_and_overlay`) is split per
class: sub-cap ⇒ clause 7 + the floor; above-cap ⇒ the overlay range
clause + predicate-5 oversize. Green (serial): overlay_length_floor 7,
device_overlay 10, overlay_ack_early 12, overlay_overwrite 32,
write_visibility 28 (7 s wall on a quiet box), f44_overlay_rewrite 2,
overlay_settle_wait 2, parked_overlay_gate 3, parked_overlay_reclaim 2,
striped_overwrite_lazy_seed 10, extent_overlay 14, extent_patch 21,
write_through_coverage 8, overlay_core 19, rand_write_amp 7,
inplace_overwrite 3, mw_ranged_lease_ladder 15, dlm_range_custody 41,
mw_arbiter_fold 3, mw_authority_assembler 21, durable_block_refs 17,
write_pipeline 23, fsync_writeback_tail_loss 3, env_knob_convention 21,
skip_ledger 11, derivation_sweep 37. `cargo clippy --all-targets
[--all-features] -- -D warnings` clean, `cargo fmt --check` clean,
markdown links PASS.

## Live A/B — loop devsub (the tcp devsub was held by a sibling agent's live mount)

Rig `.benchmarks/rigs/2026-09-02-f47-overlay-length-floor-rig.sh`. A =
dafa82ca, B = df2e8b9d, release builds; fresh `format --force` + `mount
--daemon --allow-other` per row (cache-less volume, 4 MiB blocks ⇒ cap
512 KiB); **instrument fio 3.42 psync O_DIRECT**; amplification =
`/proc/diskstats` sectors written on the two data namespaces ÷ fio
`io_bytes`; `wareq-sz` = device bytes ÷ writes completed (the loop
devsub's max request is 512 KiB — the A-leg's exact `wareq-sz`).
**Substrate: nvmet-loop over zram** (`SQZ_DEVSUB_OSS_COUNT=2`, 8 GiB
each, 1 null_blk mds) — per the two-substrate rule these write rows are
scoping evidence, not acceptance; the ledger and request-count columns
are structural and exact. Box NOT quiet: a sibling agent's `cargo test
--all-features` + containerized `cargo build --release` ran throughout
(loadavg 5–15 on 32 CPUs) — two passes, both reported; latency/IOPS
deltas are directional, bytes/requests are exact.

### rand-4k O_DIRECT, 4 jobs, 20 s, `--end_fsync=1`, 1 GiB `truncate`d sparse file (256 hole blocks)

| pass | bin | user MB | dev W MB | amp_w | dev writes | wareq-sz | IOPS | clat p50 / mean / p99 µs | ledger |
|---|---|---|---|---|---|---|---|---|---|
| 1 (load 14.5) | A | 1044.6 | 1399.6 | 1.34× | 308,271 | 4.4 KiB | 12,406 | 78 / 315 / 2,703 | `overlay_installs` 255, `overlay_stores` 254,004, `overlay_gap_seeds` 53,235 (351 MB), `patch_writes` 1,027 |
| 1 | B | 1348.1 | 2347.8 | 1.74× | 309,528 | 7.4 KiB | 16,433 | 40 / 243 / 4,424 | `overlay_ineligible_sub_cap` 329,122, `extent_parks` 18,804, `fold_passes` 255, `patch_writes` 307,979 |
| 2 (load 5.0) | A | 1013.8 | 1382.3 | 1.36× | 301,820 | 4.5 KiB | 12,045 | 72 / 323 / 4,293 | installs 255, stores 246,537, gap seeds 54,296 (364 MB), patches 982 |
| 2 | B | 2101.9 | 3098.4 | 1.47× | 490,488 | 6.2 KiB | **25,617** | **34** / 156 / 4,293 | sub_cap 513,150, parks 18,121, folds 255, patches 489,178 |

Reading: on B every hole block takes ONE W2 park chain (≈ 71 extents,
then the 64-extent fold trigger materializes it — `fold_passes` = 255 =
the touched blocks) and from then on every write is a **W1 in-place
patch** (1×): 489k of 513k ops. On A the same blocks stayed OPEN
overlay records for the whole run (255 dests held), every op a 4 KiB
device store, the complement seeded at the end-fsync (54k gap runs);
`patch_writes` ≈ 1k on A are blocks that settled mid-run through
claim conflicts between the four jobs. Bytes: same class (1.36× vs
1.47× — B's extra is 255 whole-block materializations at the 64-extent
trigger instead of at the drain), **IOPS +32 % / +113 %, p50 −49 % /
−53 %** across the two passes (directional under load). Device reads 0
on both (a hole seeds nothing on either path).

### rand-4k, `--fsync=1` (the field's fill-1 drain shape), 8 s

| pass | bin | user MB | dev W MB | amp_w | IOPS | clat p50 / mean µs | ledger |
|---|---|---|---|---|---|---|---|
| 1 | A | 195.8 | 1268.5 | 6.48× | 5,975 | 44 / 360 | installs 255, stores 255, gap seeds 508 (**1,068.5 MB**), patches 47,546 |
| 1 | B | 137.2 | 1209.9 | 8.82× | 4,186 | 46 / 484 | sub_cap 33,491, parks 255, folds 255, patches 33,236 |
| 2 | A | 191.3 | 1264.0 | 6.61× | 5,839 | 40 / 368 | installs 255, stores 255, gap seeds 508 (1,068.5 MB), patches 46,459 |
| 2 | B | 229.7 | 1302.4 | 5.67× | **7,010** | 36 / 327 | sub_cap 56,090, parks 256, folds 255, patches 55,834 |

Reading: bytes are structurally identical — a fixed **1,068.5 MB hole
fill (255 × 4 MiB, one per touched block)** plus 1× W1 patches
(A 1264 − 1068 = 196 ≈ 191 user; B 1302 − 1068 = 234 ≈ 230 user); the
`amp_w` column moves only with the user-byte denominator, and the two
passes disagree on the IOPS sign (−30 % under loadavg 14.5, +20 % under
5.0) — this row is latency-bound psync and reads box load. The fill
vehicle is the only difference: an overlay settle (4 KiB store + two
zero gap runs) vs a W2 fold (one whole-block upload).

**The honest amplification statement.** Per drain event the overlay's
settle and the W2 fold pay the same device bytes (one whole-block
write, plus one whole-block read on the mapped shape); on a
passthrough hole/clone fileset both paths converge to W1 (1×) once a
block is materialized. What the floor changes is *structural*: no
fresh dest minted-and-held per touched block across the drain interval
(the A rows held 255 open records = 1 GiB of unpublished device space
for the run; `overlay_open` returned to 0 only at the end-fsync), no
rewrite-epoch feed per sub-cap record, the first touch's fill lands as
ONE whole-block request instead of a sub-block store + gap runs, and
the W1/W2 class boundary the Random-small-write program measured
(`fold_fill` ≥ 16; the byte-budgeted RAM park with 4 KiB-class staging
spills) is restored for every sub-cap shape, including the co-writer
plane-gate refusal the field row came from (where W2 parks and the
unlink DISCARDS, while the overlay drains-and-publishes on unlink: the
six 8 MiB I/Os of the C8 row vs zero). The "1,024× each way" arithmetic
is the fill-1 settle of a mapped block — W2's fold pays it identically
when it is forced to fold at fill 1; the floor's win there is the
mint/hold/feed/request-count face, not bytes.

### The A-leg — 1 MiB aligned sequential O_DIRECT overwrite of a mapped 1 GiB file, 3 loops, both orders

| pass | row | user MB | dev W MB | amp_w | writes | wareq-sz | IOPS | clat mean µs | ledger |
|---|---|---|---|---|---|---|---|---|---|
| 1 | A1 | 3221.2 | 3221.2 | 1.00× | 6,144 | 512.0 KiB | 195 | 5,121 | `overlay_overwrite_installs` 768, `overlay_stores` 3,072, `patch_ineligible_oversize` 3 |
| 1 | B1 | 3221.2 | 3221.2 | 1.00× | 6,144 | 512.0 KiB | 195 | 5,110 | identical |
| 1 | B2 | 3221.2 | 3221.2 | 1.00× | 6,144 | 512.0 KiB | 190 | 5,241 | identical |
| 1 | A2 | 3221.2 | 3221.2 | 1.00× | 6,144 | 512.0 KiB | 174 | 5,729 | identical |
| 2 | A1 / B1 / B2 / A2 | 3221.2 | 3221.2 | 1.00× | 6,144 | 512.0 KiB | 214 / 213 / 213 / 215 | 4,660 / 4,691 / 4,672 / 4,637 | identical (768 / 3,072 / 3) |

The win does not regress: byte-identical, ledger-identical (768
overwrite installs = 3 loops × 256 blocks; 3,072 stores = one per 1 MiB
segment), IOPS within noise in both orders; `overlay_ineligible_sub_cap`
0 on every A-leg row (the floor is silent above the cap).

### Fat #2 — small-bs sequential O_DIRECT into a fresh 512 MiB file (the `wareq-sz` face)

| pass | row | dev W MB | amp_w | **dev writes** | **wareq-sz** | IOPS | MB/s | clat p50 / mean µs | ledger |
|---|---|---|---|---|---|---|---|---|---|
| 2 | seq-4k A | 541.1 | 1.01× | **131,077** | **4.0 KiB** | 12,750 | 52.2 | 66 / 78 | `overlay_stores` 131,070 (one per op), installs 128 |
| 2 | seq-4k B | 545.3 | 1.02× | **651** | **817.9 KiB** | **30,826** | **126.3** | 28 / 32 | `overlay_ineligible_sub_cap` 131,071, `write_through_blocks` 127 |
| 2 | seq-64k A | 541.1 | 1.01× | 8,197 | 64.5 KiB | 2,635 | 172.7 | — / 378 | stores 8,190 (one per op) |
| 2 | seq-64k B | 545.3 | 1.02× | 651 | 818.0 KiB | 19,366 | **1,269.2** | — / 49 | sub_cap 8,191, write-throughs 127 |
| 1 | seq-4k A / B | 541.1 / 545.3 | 1.01× / 1.02× | 131,077 / 681 | 4.0 / 781.9 KiB | 11,944 / 25,842 | 48.9 / 105.8 | 83 / 38 mean | as above |
| 1 | seq-64k A / B | 541.1 / 549.5 | 1.01× / 1.02× | 8,197 / 656 | 64.5 / 818.0 KiB | 2,153 / 16,094 | 141.1 / 1,054.8 | 463 / 59 mean | as above |

Confirmed: with the floor the sequential sub-cap stream accumulates
into 127 whole-block write-throughs (the file's 128th block is the
partial tail), so the device sees **201× fewer write requests at the
device's max request size** (817.9 KiB ≈ the 512 KiB/1 MiB split of a
4 MiB upload on this substrate) instead of one request per op at bs;
throughput 2.4× at 4 KiB and 7.3× at 64 KiB, p50 −58 %. Bytes are 1×
on both (a sequential fill has no complement to seed).

## Verdict

Root cause confirmed at the anchors; the floor lands as ONE derived
predicate on the W1 cap with the stated `=0` composition; RED → green
on all seven contracts, neighbour suites green; the A-leg is
byte/ledger-identical in both orders; fat #2's request-size collapse is
closed (201× fewer device requests, 2.4–7.3× throughput). The
rand-4k byte-amplification claim is adjudicated honestly above: per
drain the two vehicles pay the same bytes and both converge to W1; the
floor's rand-4k wins are the held-dest/epoch-feed/request-count faces
and +32–113 % IOPS at −49–53 % p50 (directional, loaded box, loop
substrate). A quiet tcp-devsub re-run is the acceptance venue for the
IOPS/latency rows; the ledger rows need no re-run.
