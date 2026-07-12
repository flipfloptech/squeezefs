# Read-path program baseline — PR 1 (dev @ 4f1c897)

**Purpose:** the committed measurement lineage every later read-path PR's deltas attribute
against (`docs/design-read-path.md`, PR Plan / Rollout step 2). PR 3/PR 4's "within 10 %"
warm-row comparisons and PR 5/6's headline gates all point back here. Docs + measurement
only — zero product-code change in this PR.

**Binary:** `dev @ 4f1c897` content (branch adds docs only). All five elbencho rows, the
single-stream row, the warm-re-read row, its interleaved-scan variant, raw-substrate
controls, `/proc/<pid>/io` device-byte ledgers, and `.stats` counter deltas. Raw artifacts
(per-row elbencho output, `.stats.before/after`, io snapshots): `~/tmp/sqperf/results/pr1_*`.

## Substrate & protocol (attribution-doc protocol, verbatim)

- AMD RYZEN AI MAX+ PRO 395 (32 CPUs), **CPU capped 3.5 GHz** (intentional, untouched),
  kernel 7.1.3-2-cachyos. Tctl 49–65 °C across all runs (rail: <80 °C), loadavg logged
  per row, `pgrep rustc|cargo` quiet-gate before every timed run.
- Volumes: 4 × 8 GiB `sqdata` + 1 GiB `sqmeta`, file-backed on the /home NVMe
  (`~/tmp/sqperf/*.img`); staging declared at format:

  ```
  squeezefs format sqmeta://~/tmp/sqperf/meta1.img sqdata://~/tmp/sqperf/oss{1..4}.img \
      --disk-cache-paths ~/tmp/sqperf/staging --force
  # disk cache default 10GB => 5 GiB NVMe read tier + 5 GiB staging (the user shape)
  squeezefs mount sqmeta://~/tmp/sqperf/meta1.img /tmp/sqperf_mount --daemon \
      --read-mem-cache-size 1G --write-mem-cache-size 1G
  # daemon caged: systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0
  ```

- Dataset 8 × 2 GiB (16 GiB ≫ 5 GiB tier; RAM `read_lru` holds only ≤ 256 KiB entries on
  this path, so cold rows are tier/device-bound). elbencho at `~/.local/bin/elbencho`,
  one file per thread, `--direct`; random rows `--timelimit 30`.
- Harness: `~/tmp/sqperf/run_row.sh` — snapshots `.stats` + `/proc/<daemon>/io` around each
  run. IOPS/MiB/s reported below are elbencho's **last-done** column.

## Elbencho rows (2 runs; set A then set B on a fresh-`rm` dataset, same mount)

| Row | Command (essence) | Run A | Run B | Baseline (for later deltas) |
|---|---|---|---|---|
| 1 fresh create | `-w -t 8 -s 2G -b 1M --direct f{1..8}` | **4282 MiB/s** | 3817 MiB/s | band 3817–4282 |
| 2 cold seq read | `-r -t 8 -b 1M --direct f{1..8}` | **787 MiB/s** | 839 MiB/s † | **787** (A = fully cold) |
| 3 rand-4k read | `-r --rand -t 8 -b 4k --iodepth 16 --direct --timelimit 30` | **326 IOPS** | 331 IOPS | 326–331 |
| 4 seq overwrite | `-w -t 8 -s 2G -b 1M --direct f{1..8}` (existing) | **869 MiB/s** | 800 MiB/s | 800–869 |
| 5 rand-4k write | `-w --rand -t 8 -b 4k --iodepth 16 --direct --timelimit 30` | 63 IOPS | **108 IOPS** | 63–108 (state-sensitive; writeback carryover) |
| single-stream cold | `-r -t 1 -s 2G -b 1M --direct f1` | 276 MiB/s ‡ | **1060 MiB/s** | **1060** (B = clean cold; the PR 5 row) |

† Run B's f1 (2 GiB of 16) was tier-resident from the preceding single-stream row —
device ledger 14.4 GiB vs A's 16.35 GiB. Run A is the canonical cold number.
‡ Run A ran after rows 2–3 (partial tier residency + loadavg 14 carryover) — kept for
honesty; run B ran immediately after a fresh write pass (write-through leaves read tiers
cold), i.e. genuinely cold and uncontended. Note the inversion: the *partially-warm* run is
~4× slower than the cold one — mmap-tier hits under the 8 GiB cage fault against the disk
while the cold path streams DMA + awaited publish; exactly the tier-tax structure
`docs/design-read-path.md` §5.3/§5.4 targets.

**Program-gate framing (design Goals #1):** row 2 cold (787) vs same-session row 1
(4282) = **0.18×** — the inversion the program exists to flip. Row 3 (326 IOPS) vs the
raw-substrate 4k control below = **0.04–0.09 %**.

## Device-byte ledgers (`/proc/<daemon>/io` deltas, set A)

| Row | User I/O | Device READ | Device WRITE | Notes |
|---|---|---|---|---|
| 1 | 16 GiB w | 0.00 GiB | 16.20 GiB | write-through control: 1.01×, zero reads |
| 2 | 16 GiB r | **15.97 GiB** | **16.50 GiB** | 1.00× read amp (post-56a968b) + **the 1:1 tier-write tax** (§5.3's target: → ≈0) |
| 3 | 38.2 MiB r (9,790 ops × 4 KiB) | **27.39 GiB** | 27.68 GiB | ≈ **730× read amp** (4 MiB fetch per 4 KiB op, net of tier hits) + tier publish per fetch (R3 target: ≤ 2×) |
| 4 | 16 GiB w | **14.94 GiB** | 31.62 GiB | old-block RMW seed reads during a pure overwrite (write-side track, out of program scope) |
| 5 | ~7.6 MiB w (1,953 ops) | 11.29 GiB | 17.28 GiB | RMW + spill + writeback carousel (R5's liveness row) |

`.stats` deltas, set A (counter lineage for later PRs): row 2 `get_obj` +4,104 for 4,096
unique blocks (**get_obj/unique = 1.002** — the 56a968b churn contract holding at
baseline), cache_hits +18,078 / cache_misses +4,088, `bg_spawn_admitted` +885 (prefetch),
`uring_queue_full` 0, `stale_binding_rebinds` 0. Row 3 `get_obj` +7,037 ≈ one whole-block
fetch per user op net of tier hits (+3,028). Row 5 `put_obj` +653, `write_through_blocks`
+1. Full before/after JSON per row in the results dir.

**Daemon survival note:** both row-5 runs completed with no OOM kill this session (peak
under the 8 GiB cage; the attribution session's OOM landed after longer accumulated
state). The cage + row-5 reproduction remains PR 7's gate scenario.

## Warm-re-read row + interleaved-scan variant (fresh format/mount, then 16 GiB written)

Recipe (the lineage PR 3 / PR 4 gates re-run verbatim):

```
warm:        elbencho -r -t 8 -s 256M -b 1M --direct f{1..8}     # slice A = 2 GiB → tier
warm-hit:    same command again (x2)                             # the warm-re-read row
interleaved: (elbencho -r -t 8 -b 1M --direct f{1..8} &)         # 16 GiB cold scan, concurrent
             elbencho -r -t 8 -s 256M -b 1M --direct -i 3 f{1..8}  # 3 timed re-read iterations
             # re-warm slice A between variant runs
```

| Row | Result |
|---|---|
| warm-up (cold fill of slice A) | 557 MiB/s |
| **warm-re-read** (×2) | **16,588 / 16,915 MiB/s — zero device I/O** (tier mmap fully RAM-resident under the cage) |
| **interleaved-scan variant** run A (3 iterations, concurrent cold scan) | **9,098 → 9,341 → 758 MiB/s** |
| interleaved-scan variant run B | 7,098 → 778 → 451 MiB/s |
| concurrent scan's own throughput | 928 (A) / 811 (B) MiB/s |

Reading: iterations stay warm until the scan's tier flood evicts slice A, then collapse to
device speed (758→451). This collapse-under-pollution shape is the baseline the PR 4
ghost-table gate ("within 10 % of PR 3 incl. the interleaved-scan variant") compares
against; run-to-run iteration variance is eviction-timing, so later comparisons should
use the *shape* (warm plateau + collapse point) plus the device ledger, not single
iteration numbers.

## Raw-substrate controls (same /home NVMe, elbencho directly on files — no FUSE)

Substrate note: the sandbox's `sqdata` volumes are **files** on this same filesystem, so
this is the correct like-for-like control (design R-9/R-10 framing), not a raw block
device.

| Control | Result |
|---|---|
| seq write `-w -t 8 -s 2G -b 1M --direct ctrl/c{1..8}` | 5204 MiB/s |
| seq read `-r -t 8 -s 2G -b 1M --direct` (×2) | **6603 / 6605 MiB/s** |
| rand-4k read `-r --rand -t 8 -b 4k --iodepth 16 --direct --timelimit 30` (×2) | **814,788 / 363,081 IOPS** (large drive-state spread — both recorded; later gates must re-run the control same-session, per Goals #1) |

Headroom read: substrate seq-read 6.6 GiB/s vs FUSE row 2 at 787 MiB/s (8.4×); substrate
rand-4k ≥ 363 k IOPS vs row 3 at 326 (≥ 1100×). The program gate (reads > same-session
writes; rand-4k device-IOPS-bound) is physically available on this substrate.

## Rails compliance

Quiet-gate enforced by the harness (aborts on rustc/cargo); Tctl 49–65 °C (< 80 rail);
CPU frequency cap 3.5 GHz untouched and noted; daemon caged 8 GiB via systemd-run scope
(cgroup verified per mount); builds `taskset -c 0-15`, `CARGO_BUILD_JOBS=12`;
`/mnt/squeezefs` untouched; nothing pushed.
