# 2026-08-21 — PR 0: volume scaling at 1 / 4 / 16 / 46 (the per-volume claim admission VIABILITY GATE)

**Branch** `perf/pv-volume-scaling-measurement` (off dev `495bd79d`).
**Design rows**: `docs/design-per-volume-claim-admission.md` — the **PR 0**
row of the PR plan, risk **R9** (the node-cache derivation at width), risk
**R15** (R-6 purge amplification / read-tier collapse vs K), §5.12 (usable
K), and open question **2** (does lowering the per-volume node-cache floor
need a user ruling). **No product code changed in this PR.**

---

## The question

The recipe's modeled load wants **~46 metadata volumes per set** (spec R4).
Each volume carries its own D0 claim + heartbeat, PR registration,
checkpoint task, pending-times drain task, journal ring and **node cache** —
and the node-cache budget derives **per volume** as
`max(budget/16, 512 MiB)` (`resolve_node_cache_budget`,
`src/meta_backend/kv/backend.rs:961`; knobs `SQUEEZEFS_META_NODE_CACHE_MB`
/ `_PCT`). Because the cache is a REGISTERED R5 component (`kv_node_cache`,
summed over volumes, `src/fuse_client.rs:20493`) whose shed lever is a
**checkpoint kick**, R9 predicts that 46 volumes means a ~23 GiB target and
therefore **permanent R5 Red with continuous shed kicks — checkpoint
thrash, not OOM**. PR 0 exists to confirm, refute or re-bound that before
any posture is built.

---

## Verdict

**R9 is bounded differently — one half CONFIRMED exactly, one half REFUTED
at the widths and working sets a 46-volume set reaches today.**

1. **CONFIRMED, to the number: the derivation authorizes more node cache
   than the whole memory budget.** On a mount whose R5 budget lands in the
   floor arm (measured at `--mem-budget 2G`), the per-volume target is the
   **512 MiB floor** and the aggregate at 46 volumes is **23.0 GiB —
   11.5× the entire budget**. On this box's derived budget (82.4 GiB) the
   pct arm gives 5,272 MiB per volume and **236.8 GiB — 2.87× the budget**.
   The authorization crosses the whole budget at **N = 16 by construction**
   (per-volume = budget/16), and at **N = budget/512 MiB** in the floor arm.
   (tier: arithmetic-on-measured-constants — the formula is the product's
   own, the budget is read live from `mem_budget_bytes` per row.)
2. **REFUTED as a mount-time or moderate-load failure.** 46 metadata
   volumes mount in **0.60 s**, cost **+110 MB anon RSS over N=1
   (2.4 MB/volume)**, **+1.95 % of one core** at idle, and on the derived
   82.4 GiB budget run R5 **Green — 0 red events, 0 hard backstops, 0
   node-cache evictions, level 0 at every width**. Squeezed onto a 2 GiB
   budget the same 46-volume mount settles at **Yellow with 0 hard
   backstops**, and the pressure is the daemon's own baseline (pools +
   transport arenas at 95 % of a 2 GiB budget), **not** the node cache,
   which is 54 MiB. The predicted thrash does not occur at these working
   sets; the width itself is not the problem. (tier: measured-real.)
3. **The gap between 1 and 2 is now an arithmetic anyone can evaluate**,
   because the fill constants are measured: node cache ≈ **N × 1.00 MiB
   (fixed, per volume) + ~173 B per resident inode** (mdstorm shape: empty
   files, one parent, no user xattrs). R9's Red therefore begins at
   ≈ `budget / 173 B` resident inodes — **≈ 12 M inodes at a 2 GiB budget,
   ≈ 49 M at 8 GiB, ≈ 500 M at this box's 82.4 GiB** (above the format's
   ≥ 100 M inode cap, i.e. unreachable there). Those are node-cache-alone
   numbers: the daemon's baseline pressure (arm B: 84–95 % of a 2 GiB
   budget before the cache is counted) brings Red forward, and the shape is
   an upper bound on how much metadata a set may hold resident, not a
   prediction of when a specific mount turns Red. The width does not cause
   it; the width **removes the brake**, which is item 4.
4. **The mechanism half of R9 is CONFIRMED structurally and by the lever's
   own instrument: a Red driven by the node cache cannot be shed out of.**
   The R5 shed for `kv_node_cache` is `kick_checkpoint()`
   (`src/fuse_client.rs:20498-20502`) — it flushes DIRTY nodes; the only
   thing that lowers `cached_bytes` is `evict_to_budget()`
   (`src/meta_backend/kv/node_cache.rs:2663`), which fires against the
   **per-volume** budget. At width that budget is N/16 × the whole R5
   budget, so it never fires: **`meta_kv_node_cache_evictions` is 0 in all
   20 rows that ran the shipped derivation (arms A and B), at every N and
   both budget arms — including the rows where R5 entered Red.** (Arm D
   evicts precisely because its knob pins the per-volume budget below the
   working set — which is the point of that arm, and the demonstration that
   the eviction path itself works.) A set-wide over-budget node cache is
   therefore, today, un-shrinkable. (tier: measured-real for the zero, code
   for the mechanism.)
5. **R15's read-tier collapse cannot be produced by width under the SHIPPED
   derivation** — the caches are not divided, so the node-cache hit rate is
   100.00 % at every N with 4 cold misses, constant. **Under the DIVIDED
   derivation PR 9 would land, the collapse is real and large**: at a fixed
   64 MiB aggregate, cold metadata reads fall from **99.994 % / 27,057
   ops/s at N=1** to **93.14 % / 10,528 ops/s at N=16 (2.6×)** and to
   **85.98 % / 5,375 ops/s at N=46 (5.0×)**. Partitioning a shared cache
   costs, and the cost is worst exactly where the recipe wants to go.
   (tier: measured-real, modelling PR 9's derivation with the absolute
   knob.)

**What it bounds for the program's usable K.** Nothing in this measurement
caps K below the `MAX_APPENDERS = 16` ceiling §5.7 already imposes: 46
volumes is a mountable, workable, R5-Green width, and the per-volume costs
are linear and small (2.4 MB RSS, 0.043 % of a core, 0.2 checkpoints/s,
+9.6 ms of mount, +5.5 ms of re-open per volume). The bound the recipe
actually inherits is **not width, it is the working set**: a set whose
resident metadata exceeds the R5 budget enters a Red it cannot shed, and
the wider the set the further the per-volume brake is from ever engaging.
That is PR 9's problem statement, and it is now quantified rather than
predicted.

---

## Venue, instrument, tiers

**Box.** AMD Ryzen AI MAX+ PRO 395, 32 cores, 117 GiB RAM, NixOS, kernel
`7.1.8-cachyos-lto`, `fuse.enable_uring=Y`, memlock unlimited —
**unprivileged FUSE mounts work**, which is what makes this sweep runnable
without root. Thermally capped laptop: every row is paced (see below) and
all numbers are **medians of repeats**, never single shots.

**Build.** `cargo build --release`, default features (never
`--all-features`: dhat replaces jemalloc — ENG-8). Binary
`squeezefs 1.1.0 (495bd79dfeb0)` — the branch point, clean (no `-dirty`),
one binary for every arm.

**Substrate.** **file** — sparse 512 MiB images per metadata volume
(4 GiB data volume) under `/tmp` on btrfs/NVMe. This is stated on every row
and it is why the CPU and journal rows below are scoping evidence, not
acceptance: AGENTS.md's two-substrate rule forbids treating barrier-bound
work measured on file-backed-on-btrfs as final (the substrate bracket is
165× on the journal barrier). The **memory-plane rows are
substrate-independent by construction** — budget derivation, cache sizing
and the R5 registry are properties of the daemon, not of the device.

**Instrument.**
* `tests/pv_volume_set.sh` — the N-metadata-volume fixture (ONE daemon, N
  volumes; file-backed by default, device-backed through the same code path
  via `SQZ_PVSET_META_DEVS`). It asserts the width the daemon actually
  opened (`len(meta_kv_journal_entries_per_volume) == N`) and pins the four
  per-class kernel TTLs to 0 for the read rows.
* `tests/run_mw_matrix.sh pv-volume-scaling` — the sweep and row emitter.
  Phases per row: mount+settle → clean idle → create → warm stat →
  post-work idle → **remount** (new daemon, cold caches) → cold stat.
  Row validity: width asserted, every phase engaged, `dlm_rpcs == 0` (the
  solo re-gate), `invariant_tripwires` and `meta_kv_revalidate_dirty_skips`
  flat, no torn journal entries at the remount. **Every arm below exited
  `rows VALID`.**
* Workload: `tests/mdstorm.c` (the house metadata storm), 8 threads, one
  shared parent directory, `create` then two `stat` sweeps.

**Instrument-alignment note (a trap, recorded).** The first read rows
measured *nothing*: at the shipped 1 s attr/entry TTLs the kernel serves an
entire re-stat sweep (4.6 M ops/s) and the daemon sees zero node-cache
probes. The fixture now pins `SQUEEZEFS_FUSE_{ATTR,ENTRY,DIR_ENTRY,
NEGATIVE}_TTL_MS=0` for these rows, so a stat sweep is 25–29 k ops/s and
**does** reach the metadata plane. Any future read row on this leg that
reports six-figure ops/s is measuring the kernel, not SqueezeFS.

**Quiet-box discipline.** Foreign `cargo`/`rustc`/`fio`/`elbencho` refuses a
row outright; the box's own heat and load decay are **waited out**
(resume < 68 °C, load1 < 4.0, bounded 1800 s) rather than aborting a sweep
half-way — the `run_bench_baseline.sh` pattern in paced form. The mdstorm
phases themselves take this box to ~90 °C, so most rows pace for minutes
before starting; that is the venue's honest cost, not a defect.
**Disclosure**: arm A ran with the gate in its refuse-on-hot form and
completed all 12 rows; the gate was then changed to the paced form (an
arm-B attempt had died between widths, which would have measured different
widths under different conditions — it emitted no rows and is not reported).
Arms B and D ran start-to-finish afterwards. Same binary throughout; the
change affects only when a row starts.

**Artifacts.** Every phase snapshot (the full stats JSON plus the `/proc`
CPU/RSS sample) is preserved by the leg under
`$SQZ_MWMATRIX_PV_ROOT/rows-<tag>-<epoch>/` — for this note
`/tmp/pvscale/rows-{A-derived,B-budget2G,D-divided-n*}-*`, 21 MB, ephemeral
(the tables here are the durable record). The fixture tears down to zero
residue: no daemon, no mount, no image survives a row.

**Evidence tiers** (`docs/rc-manifest.md` §2): every table cell is
**measured-real** unless the column is named in the arithmetic list
(`ncTarget/vol`, `ncTargetAgg`, `aggXbudget` — **arithmetic-on-measured-
constants**) or the row is flagged **scoping** (the file-substrate CPU and
journal-balance rows, which owe the devsub re-run). Nothing here is
measured-simulated: there is no fleet and no extrapolation to 15 k.

---

## Arm A — the shipped derived budget (82.4 GiB), 20 k files, medians of 3

```
== pv-volume-scaling (substrate=file, budget=derived, node_cache_mb=derived, files=20000, repeats=3, medians) ==
   R5 budget resolved: 82.4 GiB
N   mount_s reopen_s rssMnt_MB anonMnt_MB rssWrk_MB anonWrk_MB idleCPU_%core wIdleCPU_%core ckptIdle/s ckptWIdle/s jrnlIdle/s
1   0.17    1.01     1722      674        1764      715        0.50          0.75           0.20       0.30        0.20
4   0.20    1.03     1732      683        1776      727        0.85          1.10           0.80       1.20        0.50
16  0.34    1.09     1758      710        1809      760        1.30          1.55           3.20       4.65        1.70
46  0.60    1.26     1832      783        1887      838        2.45          2.25           9.19       11.89       4.70

N   createCPU_s create_ops/s statW_ops/s statC_ops/s nodeCache_MB nc_B/file ncEvict ncSheds coldMisses
1   6.20        5600         28954       29031       4.0          210       0       0       4
4   6.59        5015         28880       29048       7.0          367       0       0       4
16  6.89        4828         26058       26415       17.0         891       0       0       4
46  6.82        4934         25540       26433       47.0         2464      0       0       4

N   r5lvl r5yellow r5red r5backstop  ncTarget/vol_MB ncTargetAgg_GB aggXbudget  jrnlMax jrnlMean jrnlMax/Mean volsMoved ncHitCold_% ncHitWarm_%
1   0     0        0     0           5272            5.1            0.06        20102   20102    1.0          1         100.00      100.00
4   0     0        0     0           5272            20.6           0.25        20056   8770     2.3          4         100.00      100.00
16  0     0        0     0           5272            82.4           1.00        20033   2424     8.3          16        100.00      100.00
46  0     0        0     0           5272            236.8          2.87        20031   861      23.3         46        100.00      100.00
```

**Reading it.**
* **Row 1 (RSS).** +110 MB anon from N=1 to N=46 = **2.4 MB per volume**,
  flat across the workload. `VmRSS` is dominated by the pools and the
  mapped binary; the per-volume term is the honest one.
* **Row 2 (checkpoint CPU).** Idle daemon CPU **0.50 % → 2.45 % of one
  core** = **0.043 %/volume** (≈ 433 µs of CPU per volume per second), with
  the checkpoint rate at **0.2/s/volume** (clean idle) and 0.26/s/volume in
  the post-workload window where there is dirty state to drain. The idle
  journal rate is **0.102/s/volume** — exactly the 10 s per-volume
  `writer_claim` heartbeat. *(scoping: file substrate.)*
* **Row 3 (R5).** Green at every width; `kv_node_cache` actual is
  **1.00 MiB per volume at mount** (four 256 KiB nodes — the pinned tree
  roots) plus the workload's fill, against an authorized
  `ncTargetAgg` of **236.8 GiB at N=46 = 2.87× the whole budget**. The
  component's own R5 **floor is 64 MiB**, which at these sizes is *above*
  the live gauge — so the R5 shed distributor skips it entirely and
  `ncSheds` is 0 even where Red fires (arm B).
* **Row 4 (journal balance).** **Every volume moves at every width**
  (`volsMoved == N`) — spec-R4's own row passes in the letter. The
  distribution does not: **max/mean = 23.3 at N=46**, because a create
  splits into a mint on the picked volume (spread) and a dentry +
  parent-times entry on the PARENT's volume (concentrated). One shared
  parent directory therefore puts 50.6 % of all entries on one volume
  (20,030 of 39,596 measured at N=46). Entries per create rise
  **1.01 (N=1) → 1.75 (N=4) → 1.94 (N=16) → 1.98 (N=46)** — the documented
  cross-volume create economy (`meta_backend/mod.rs:934`), visible for the
  first time as a measured curve. *(scoping: file substrate.)*
* **Row 5 (read tier).** 100.00 % node-cache hit rate warm and cold at
  every width; **cold misses are 4, constant in N** — the mount-time
  bootstrap already resident. Under the shipped derivation the read tier
  does not care about width.
* **Throughput side-note (not a PR 0 row, recorded).** create
  **5600 → 4934 ops/s (−12 %)** and stat **28954 → 25540 (−12 %)** from
  N=1 to N=46: the price of the second journal entry per create and of
  46 conveyors. *(scoping: file substrate — a devsub re-run owes this
  number.)*

---

## Arm B — the FLOOR regime (`--mem-budget 2G`), 100 k files, medians of 2

This is the arm that reproduces R9's own premise: a budget small enough
that `budget/16 < 512 MiB`, so the per-volume target IS the floor.

```
== pv-volume-scaling (substrate=file, budget=2G, node_cache_mb=derived, files=100000, repeats=2, medians) ==
   R5 budget resolved: 2.0 GiB
N   rssMnt_MB anonMnt_MB rssWrk_MB anonWrk_MB idleCPU_%core ckptIdle/s createCPU_s create_ops/s statW_ops/s
1   954       674        1085      804        0.62          0.20       32.86       5300         28013
4   964       683        1111      830        0.75          0.80       34.71       4794         28701
16  989       709        1144      863        1.40          3.20       36.20       4626         28316
46  1058      777        1209      928        1.97          9.19       36.36       4660         25616

N   nodeCache_MB nc_B/file ncEvict ncSheds coldMisses r5lvl r5yellow r5red r5backstop ncTarget/vol_MB ncTargetAgg_GB aggXbudget
1   17.5         184       0       0       32         1     0        1     0          512             0.5            0.25
4   21.0         220       0       0       32         1     0        1     0          512             2.0            1.00
16  32.0         336       0       0       32         1     0        1     0          512             8.0            4.00
46  54.0         566       0       0       32         1     0        0     0          512             23.0           11.50
```

**Reading it.**
* **The design's number, reproduced exactly**: `ncTargetAgg = 23.0 GiB at
  N = 46`. Against this budget that is **11.5×**; the doc's "23 GiB" is the
  right number and its implicit budget is 8 GiB (where it is 2.875×).
  Crossover is at **N = 4** for a 2 GiB budget (`aggXbudget` = 1.00).
* **R5 settles at Yellow (level 1) at every width**, having passed through
  Red once on the way up at N ≤ 16 (`r5red` 1/1/1/0) with **0 hard
  backstops** — and that pressure is the DAEMON's baseline (pressure/budget
  84.4 % at N=1, 94.9 % at N=46; the pools and transport arenas, which size
  themselves down with the budget: `rssMnt` 1722 → 954 MB vs arm A).
  The node cache contributes 17.5–54 MiB of it.
* **`ncSheds` is 0 and `ncEvict` is 0 even in Red** — the two zeros that
  make verdict item 4 concrete: the node cache is below its 64 MiB R5 floor so
  the distributor never asks it to shrink, and even when it is asked, the
  lever it would pull (checkpoint kick) does not evict clean nodes.
* **Width inflates the resident cache 3× at a fixed working set**:
  100 k files cost **17.5 MiB at N=1 and 54.0 MiB at N=46**, of which
  **46 MiB is the fixed N × 1 MiB term**. The per-inode marginal is 173 B
  at N=1 and 84 B at N=46 (the same records spread over more, and more
  sparsely filled, 256 KiB node extents).

---

## Arm D — the DIVIDED derivation (PR 9's shape), the R15 read-tier row

Under the shipped derivation the read tier is indifferent to width (arm A,
row 5), so the interesting question is the one PR 9 asks: **if the
per-volume budget became `budget/(16·N)` — the aggregate held constant
instead of multiplied — what does the read tier do at width?** This arm
models it with the shipped knob (`SQUEEZEFS_META_NODE_CACHE_MB`, absolute,
wins verbatim), holding the aggregate at ≈ 64 MiB and the working set at
400 k files, so the caches are genuinely over-subscribed at every width.

```
files=400000, idle=5 s, medians of 2, kernel TTLs 0, derived R5 budget (82.4 GiB)
N   per-vol MB  aggregate MB   COLD stat (post-remount, the read-tier instrument)   evictions  create_ops/s
                               hit %        misses      ops/s      vs N=1
1   64          64             99.994       332         27,057     1.00×            275        5,104
4   16          64             94.63        300,582     13,392     0.49×            300,556    4,558
16  4           64             93.14        383,978     10,528     0.39×            383,974    4,451
46  1           46             85.98        785,362      5,375     0.20×            785,585    4,441
```

**Reading it.**
* **A uniformly divided budget is lossy, and the loss is large.** At the
  SAME 64 MiB aggregate, moving from one cache to sixteen takes the cold
  metadata-read hit rate from **99.994 % to 93.14 %** and the sweep from
  **27,057 to 10,528 ops/s — 2.6×**. At N=46 (where the integer-MiB knob
  can only express 1 MiB per volume, so the aggregate is 46 MB, itself part
  of the story) it is **86.0 % and 5,375 ops/s — 5.0×**.
* **This is the classic partitioned-cache result, sharpened by two
  SqueezeFS-specific terms**: the 256 KiB node granularity (a 1 MiB slice
  is FOUR nodes) and the parent-volume concentration of finding 3 — the
  volume hosting the workload's parent directory holds the whole dentry
  stream (401,160 of 793,654 create-phase entries at N=46) and therefore
  needs far more than `1/N` of the cache, which a uniform division refuses
  to give it.
* **The warm pass is NOT comparable across these widths and is excluded
  from the verdict**: it composes with the routing-level `metadata_cache`
  (390.6 MiB after the create phase in EVERY row) and the fold memo, and it
  behaves non-monotonically (0 node-cache misses at N=46 against ~291 k at
  N=4). The post-remount cold pass is the honest read-tier instrument here,
  which is why the leg takes one.
* **Tier**: measured-real, but it is a MODEL of PR 9's derivation (the
  aggregate is held at ~64 MiB by the absolute knob rather than by a
  set-aware formula), so it prices the SHAPE of that change, not a shipped
  configuration. No shipped mount divides its node cache today.

---

## What needs the root + devsub venue (owed, explicitly)

Nothing in the verdict rests on these, but they are not acceptance evidence
until they are re-run under root on `tests/dev_substrate.sh` (the fixture
takes `SQZ_PVSET_META_DEVS=/dev/nvmeXn1,...` and runs the same code path,
so the re-run is a device list, not a new harness):

1. **Row 2, checkpoint CPU** — the per-volume 0.043 %/core and the
   0.2/s/volume checkpoint rate are barrier-sensitive: on a file substrate
   a checkpoint's journal barrier is an `fdatasync` to page cache. The
   *shape* (linear in N) will hold; the *slope* is a file-substrate number.
   **`SQZ_DEVSUB_TRANSPORT=tcp`** is the mandatory venue if the row is ever
   quoted for a write-side claim.
2. **Row 4, journal balance** — `volsMoved == N` and `max/mean = 23.3` are
   structural (routing, not devices) and will reproduce, but the
   *entries/op* curve and any throughput reading beside it need the devsub.
3. **The sustained-state row PR 0 does not have** — every arm here is a
   burst (create sweep + stat sweeps). AGENTS.md's sustained rule wants a
   ≥ 60 s flat window at 46 volumes before any throughput number from this
   leg is a claim. The leg supports it (`--files` large, `--repeats`); the
   box's thermal cap and the file substrate made it worthless to run here.
4. **The R9 Red-in-the-field row** — driving a real over-budget node cache
   (≥ 12 M resident inodes at a 2 GiB budget) is a devsub/field-scale run,
   not a laptop one. The arithmetic above is the prediction it would test;
   the two zeros (`ncEvict`, `ncSheds`) are the mechanism it would confirm.

---

## PR 9's input (the shape of the fix — NOT landed here)

Open question 2 asks whether making the derivation set-aware needs a user
ruling. The measurement says the change is warranted and says what its
floor must be:

* **The defect is the multiplication, not the value.** `max(budget/16,
  512 MiB)` is a sound per-volume answer and a broken per-SET one: at N
  volumes it authorizes `N/16 × budget` (pct arm) or `N × 512 MiB` (floor
  arm) against ONE budget. A set-aware form —
  `per_volume = max(budget/16/N, physical_minimum)` — keeps the shipped
  1-volume answer byte-identical and makes the aggregate scale-free.
* **But arm D says a naive uniform division is NOT free**, and PR 9 must
  land knowing its price: 2.6× on cold metadata reads at N=16 with the
  aggregate held constant, 5.0× at N=46. Three postures are therefore on
  the table and this note deliberately does not choose between them —
  (a) **divide uniformly** with a physical minimum, and accept the
  measured read-tier cost at width; (b) **share the budget set-wide** —
  one admission authority over N caches, per-volume borrowing instead of
  per-volume partitioning, which is the only form that both bounds the
  aggregate AND keeps the parent-hosting volume's larger share (the arm-D
  mechanism), at the cost of a real change to `NodeCache`'s budget owner;
  (c) **leave the derivation and cap K** (a 2 GiB-budget mount is honest
  at N ≤ 4, an 82 GiB one at N ≤ 16), which is the zero-code answer and
  the one the §5.12 usable-K sentence would then have to state.
* **The per-volume physical minimum is measured: 1.00 MiB** — four 256 KiB
  nodes (the pinned tree roots), invariant across every width and both
  budget arms in this sweep. Expressed as `4 × node_size` it tracks the
  `--meta-node-kib` format knob (64 KiB–1 MiB ⇒ 256 KiB–4 MiB), which is
  what a floor with a *physical* justification looks like under
  AGENTS.md's rule. Note the practical corollary: the existing knob is
  integer MiB, so a set-aware derivation at large N and small budgets can
  ask for less than the knob can express — the physical minimum is not
  optional.
* **The never-regress-below-shipped posture is preserved where it is
  actually deployed** if the divisor is `max(N, 1)` and the floor stays
  512 MiB for `N ≤ 4`: the shipped field widths (1–4 volumes) then keep
  their exact current budget, and only N ≥ 5 — a width no shipped set uses
  — sees a smaller per-volume cache. That is the A/B PR 9 owes, at 2–4
  volumes, plus the tie row in `tests/derivation_sweep_tests.rs`
  (`node_cache_budget_derives_from_memory_budget`, which currently pins the
  per-volume form and must gain the per-set one, and
  `fleet_share_quarters_every_divisible_derived_cap`, which composes the
  fleet-share divisor with it).
* **The ruling is still owed.** This note is the input open question 2
  asked for; it does not settle it. What it removes is the guesswork: the
  change costs nothing at shipped widths, buys a bounded aggregate at
  recipe widths, and its floor has a physical derivation rather than a
  historical one.

---

## Findings convicted along the way (no product change; all recorded)

1. **The R5 shed for `kv_node_cache` cannot lower the gauge it is
   registered against.** Structural (kick vs evict, both anchored above),
   and measured as `ncEvict == 0` / `ncSheds == 0` across all 20
   shipped-derivation rows including the Red ones. Anyone who reads a Red
   with a large `kv_node_cache` component in the field should not expect it
   to converge.
2. **The component's 64 MiB R5 floor is above its live size at every width
   measured here** (max 54 MiB at N=46 / 100 k files), so the shed
   distributor skips it entirely. The node cache only becomes *sheddable*
   at ≈ 390 k resident inodes on a 1-volume set — sooner at width, where
   the fixed `N × 1 MiB` term is already 46 MiB of the 64 — after which
   finding 1 applies and the shed still cannot converge.
3. **A create on a multi-volume set costs ~2 journal entries, on two
   volumes** (1.01 → 1.98 measured). Documented in the code's economy note
   since 2026-07-30; this is its first measured curve, and it is why
   `jrnlMax/Mean` at N=46 is 23.3 rather than 1.0 for a single-parent
   workload: mints spread, dentries do not.
4. **Kernel TTLs make a naive metadata-read row measure the kernel.** See
   the instrument-alignment note; the fixture now pins them to 0 and the
   leg documents why.
5. **A thermally-capped box will abort a sweep between widths** if the
   thermal gate refuses instead of waiting — which silently measures
   different widths under different conditions. The leg waits (paced) and
   refuses only on foreign work.
6. **Partitioning a shared cache costs even at a constant aggregate**
   (arm D) — 2.6× on cold metadata reads at N=16. Any future "divide the
   budget by the number of X" derivation in this tree inherits this
   result; the shared-budget-with-borrowing form is the one that does not.
7. **The R15 purge-amplification row PR 0 was asked for cannot be taken
   yet, and this note does not claim it.** Sweep row 16's mechanism is
   `ro_coherence::purge_reader_block_keys` firing per revalidation epoch on
   a partial authority holding K−1 peer-owned volumes — a posture PR 4
   builds. What is measurable today is the cache-division half (arm D),
   and it is measured. The purge half stays owed to PR 8 with the posture
   armed; `meta_kv_revalidate_{epochs,keys_purged}` are its instruments and
   are 0 in every row here (correctly: no mount in this sweep arms
   revalidation).

---

## Reproduce

```bash
cargo build --release                       # default features (ENG-8)

# Arm A — shipped derived budget
bash tests/run_mw_matrix.sh pv-volume-scaling \
     --ns=1,4,16,46 --files=20000 --idle-secs=20 --repeats=3 --tag=A-derived

# Arm B — the floor regime R9 assumes
bash tests/run_mw_matrix.sh pv-volume-scaling \
     --ns=1,4,16,46 --files=100000 --idle-secs=20 --repeats=2 \
     --budget=2G --tag=B-budget2G

# Arm D — the divided (set-aware) derivation, one invocation per width
for pair in "1 64" "4 16" "16 4" "46 1"; do set -- $pair
  bash tests/run_mw_matrix.sh pv-volume-scaling \
       --ns=$1 --files=400000 --idle-secs=5 --repeats=2 \
       --node-cache-mb=$2 --tag=D-divided-n$1
done

# The same sweep on the nvmet devsub (root; the owed re-run)
sudo SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh create
sudo SQZ_PVSET_META_DEVS=/dev/nvmeXn1,... SQZ_PVSET_DATA_DEVS=/dev/nvmeYn1 \
     bash tests/run_mw_matrix.sh pv-volume-scaling --ns=1,4,16,46 ...
```
