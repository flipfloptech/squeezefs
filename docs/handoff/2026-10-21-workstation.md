# Handoff — 2026-10-21/22: the fourth wave (the two caches, the mainline UAPI), the mainline kernel booted

**Checkpoint (amended 2026-10-22):** `dev` at the commit that adds this amendment, on `948a324`;
everything pushed; the gate train **3/3 green on `84eacfe`** on the MAINLINE kernel, `948a324` gated
green once (gate 78), its train runs pending. **The host runs `7.3.0-1.7-mainline-sqz` since
2026-10-22 ~01:50 UTC** (mainline 7.3 + the 7 patches of `scripts/kernel/patches-mainline/`; the
FUSE transport takes its `mainline_pool` rung by probe — `bufpool present, kmbuf absent, zero-copy
true`; 116 tests skip by kernel feature, every one named). `linux-omarchy-sqz` 9.33 stays the Limine
default for a fallback boot. The 10-18/19/20 notes hold the release sequence, the bar, the rulings
and the venue plan. **The release window still waits for the owner's go.**

## 2026-10-22 on the mainline kernel (each through its own full gate, then the train)

| commit | what |
|---|---|
| `3b8f547` | **the gate on 7.3**: the §AF no-open rows pin the posture they test (`SQUEEZEFS_FUSE_ZC_OPEN=0`; under §BE's default a mainline zero-copy session SERVES opens, so zero-message opens and mainline zc exclude each other); the stock-stratum seam hides both carriers; fused poll and the write-cache data-plane row judge "zero-copy armed" on either stratum; **a product defect**: an `O_TRUNC` open of a foreign-slot file under served opens ran the local size path and wedged 48 s to EIO — it ships like `SETATTR(size 0)` now (new red row). `rung-10-fuse-mainline-uapi.md` §9 "live on 7.3" |
| `2b9659c` (create 7b) | wave 7 re-landed: BD.4/BD.5 unchanged (innocent) + the train's panic fixed at its root — **a key's records fold by SEQ across the overlay and the base** (§AW.4's either-order applies + a freeze between them put the later record in the base and the earlier in the overlay below it; `foldNewestFirst` asserted newest-first; latent since 10-20) — and what the honest fold exposed: **the manager's record-seq space is monotone across its incarnations** (the open took its seq offset from an empty replay window after a clean unmount). §BD.7–BD.9; red tests shown panicking on the old tree; train 3/3 |
| `00aed7e` | **the two release rows** (owner, 2026-10-22): `scripts/suites/mdtest.sh`, `scripts/suites/tarx.sh`, both in `zig build gate-release` (item 5 below has the laws and the first runs) |
| `84eacfe` | **the striped row's daemon death** (1 run in 85; the train's striped death): `Striping.persisted`, the per-directory "last persisted" stamp a fold reads and writes on every `stat`/`lookup` of a striped directory, had no lock — two folds past the cadence both committed D's times and both `put` at once (`std.hash_map` pointer_stability). It has an owner now (`persisted_lock`; the commit runs outside it) |
| `948a324` | **a joined appender's ring GROWS** — the joiner's half of GrowRing (spec §3.3 `joined_grow_ring_if_due`) was never ported: every wire joiner lived at the 512 KiB floor for its mount's life. New `src/meta/sym/ring_growth.zig` (the pure decision, the growth gate — a Dekker pair with the conveyor's passes, modelled), judged once per landed checkpoint cycle; the ring geometry swaps under a drained window, the tail memfd re-created and handed to the supervisor. The tarx row's wall law: **1.137× → 1.022×** (B's journal-full stalls 2,346 → 8, barriers 4,801 → 171, metadata bytes 313 → 126 MB). A clean leave whose pool fragmented past 16 runs refused itself — fixed in the same commit |

## Rulings taken today (owner, 2026-10-21)

- **Deployment shape:** production runs only on nvmet namespaces (Linux/NVMesh) — no local device,
  no local filesystem. Anything venue-only is out; **disk-based caching is gone** (the file-backed
  staging ring, `--disk-cache-paths`, the `staging` verb, the NVMe read tier).
- **Caching design:** TWO caches, two knobs in percent of SYSTEM RAM, **default 25 each** (Lustre's
  `max_cached_mb` 50 % posture): `SQUEEZEFS_READ_CACHE_PCT` (the daemon's grain cache) and
  `SQUEEZEFS_WRITE_CACHE_PCT` (the staging ring, now a kernel-held sealed memfd, ON by default). Both
  purge by LRU. **ONE cache core** (`src/mem/cache.zig`, `Cache(Entry, Sink)`: striped exact LRU, a
  bound that follows the live budget, eviction through the Sink) — used forward for reading
  (`Cache(Grain, DropSink)`) and reversed for writing (`Cache(Resident, PromoteSink)`: eviction
  PROMOTES to the data volumes). Decision 0003 §BF (read), §BG (the write cache's member scope);
  0002 row 551.
- **Kernel:** no fork. The kmbuf/zero-copy UAPI the series 0001–0021 carries is dead upstream; 7.3
  took FUSE buffer pools. The daemon moves to mainline's UAPI; the owner's perf patches go upstream
  in posting form (`scripts/kernel/upstream/`, Signed-off-by placeholder to replace). The mainline
  config was checked against the running one: no driver loss (only `CONFIG_SCHED_BORE` differs).

## Landed today (each through its own full gate; then the train)

| commit | what | this box |
|---|---|---|
| `cc21b45` (create 6) | the create's leaf set named (two records, each taken twice); the heap admission's lock-free fast path (§BD) | neutral at 32 ranks (1.02×), 1.11× creates at 64 |
| `9361279` | the kernel series against mainline: `patches-mainline/` (7 on v7.3), `upstream/` postings, `build-host-kernel --mainline`; `linux-mainline-sqz` installed as a boot entry | `rung-10-kernel-mainline.md` |
| `6ab44eb` | **the transport's mainline UAPI port**: `mainline_pool` buffer mode by probe, the pooled held write, zero-copy-under-`no_open` RFC; charter §3/§4, §BE, 0002 row 550, `docs/porting/fuse_mainline_pools.md` | UNVALIDATED LIVE (needs the 7.3 boot) |
| `8e3d0d5` (+ `0677e2b`) | **the read cache**: the ONE core; `Cache(Grain, DropSink)`; the ranged read deposits after its DMA; a foreign write bumps the reader's lease epoch; `SQUEEZEFS_READ_CACHE_FILL=leg` ships (the zc direct leg deposits nothing — its DMA lands in the app's pages); the item table materializes lazily (an idle daemon paid 511 MB for a 27 GiB reservation's memset) | 4k randread ×32 warm **2.86 / 3.23 M** (2.7×; ext4 1.99 M O_DIRECT) |
| `ae5a974` | the supervisor's probe miss inside the grace window is silent (the lazy-umount row's law) | |
| `cb76e65` `6a1f2fd` | **the holder census walks fds through statx on the /proc links, never fdinfo** — a fdinfo read generates the fd's whole document (a desktop indexer's fanotify fd: 8.6 MB, 160–300 ms a read; three walks per `umount --lazy` = 6.9 s on the train); the verb walks once | `umount --lazy` 1.1 s → 0.2 s idle, ≤ 0.7 s under a gate |
| `d0fcaf8` (the write cache, 17 commits) | **the staging ring in a sealed memfd on every mount**, `SQUEEZEFS_WRITE_CACHE_PCT`; `Cache(Resident, PromoteSink)`; fsync PROMOTES (the ranged segment sync and `SQUEEZEFS_FSYNC_PROMOTE_STAGED` retired); RECALL — a foreign read promotes the holder's residents first; the kill -9 handover through the supervisor; the member scope `(mount slot, fabric host identity)` (§BG); a 64 MiB floor under a Red shed and the bound re-derived from the live budget every tick (the first gate's five reds: one Red tick had collapsed the ring to ONE record for the mount's life) | staged 4k rewrites j16 **350–400 k** (ext4 570 k; was 290 k file-backed); small files j16 56–58 k (ext4 562 k) |

## Reverted: create wave 7 (`9aec674..5b4e210` → `44e103a..993f9b5`)

BD.4 (the admission on a volume with room takes no lock at all) + BD.5 (the appliers' turns fan out
as a binary tree) passed gate 73 and then went red on the train one run in two: the unit test "the
applier's resolve starts from the committer's heap-admission leaves … under a storm" **panicked** at
`record.zig:1310 assert(prev_seq >= r.seq)` in `foldNewestFirst` from the concurrent `applyMembers`
(records of one key folded out of sequence order), the striped-directory row failed, and the DEAD
MANAGER row hung with its storm child unkillable in a FUSE wait (wave 4's shape). The branch is
intact; an agent (`01a16101`) is making the interleaving deterministic red-first (a model of the
fan-out + apply order or a seam-pinned test), fixing, re-basing onto the reverts. **Lesson for the
method notes:** a lock-free change to the apply order needs the train's three runs BEFORE the
next landing on top of it, and its model must include the fold's ordering invariant.

## Where the rows stand (ext4 on the same RAM NVMe; `rung-10-*` notes hold each A/B)

| row | ext4 | FUSE path | interposer, direct arm |
|---|---|---|---|
| 4k randread ×32 cold / warm | 1.99 M O_DIRECT | 1.88–1.90 M / **3.23 M** (read cache) | 3.08 M = 101 % |
| 4k randwrite ×32 | 1.96 M | 1.40 M (71 %) | 3.03–3.07 M = 100 % |
| 1 MiB seq write / read ×32 | 85 / 98 GB/s | 75–81 % / 77–89 % | 99 % / 100 % |
| staged small-file 4k rewrites j16 | 570 k | **350–400 k** (write cache) | — |
| mdtest -u creates / removes -np 32 | 711 k / 573 k | 100–120 k / 97–111 k (wave 6; wave 7 reverted) | FUSE rows |
| stat | 9.8 M | 10.9 M | — |

## Owed / next (in order)

1. **DONE 2026-10-22:** the mainline boot, the gate green on it (`3b8f547`), create 7b (`2b9659c`).
   **Still owed on 7.3:** the matrix (pool vs user_ents; `SQUEEZEFS_FUSE_ZC_OPEN` 1 vs 0; 7.3 vs
   9.33 side by side — one benchmark at a time, cooled ≤ 70 °C) and §BE.3's default re-judged on
   those numbers; the 9.33 kernel has NOT run the day's landings (every change is conditional on the
   probed rung; boot it once before the release and run one gate).
2. The create path's remaining binders — the appliers' mutual leaf waits (62 of 109 µs per window
   on `venue`), `self` with reclaim destroy groups on removals, atomic-open patch 0034 (still not
   drafted). From create 7b's report, owed: the DEAD MANAGER row's hang on the train was B parking
   TWICE (the second reclaim → `UnknownLease` → poisoned, one cross-owner create parked forever at
   `[handler_entry]`) — a load sensitivity of `T_owner`; the replay gate drops a straggler Δ the node
   never saw (an exact-seq gate would keep it); the shipped setattr carries no `kill_suidgid`, so a
   foreign-slot truncate does not strip S_ISUID/S_ISGID at the holder; under doubled load `A
   MEMBER'S CACHED COPY…` and `THE ORDERED LAW'S KILL ROW` each failed once (`~/tmp/cp7b/runs/`).
3. The write cache's remainder (`rung-10-write-cache.md`): the FUSE transport is 52 % of the
   small-file profile (the mainline port's territory), per-grain checksums, clock stamps, the witness
   generation, per-lane regions named. The read cache's: the FUSE zc leg landing in the cache's own
   memfd slab (§BF; `SQUEEZEFS_READ_CACHE_FILL=lane` measured −20 % until then).
4. **The budget under the gate** (the write-cache agent's finding): a smoke daemon's RSS is 2.15 GB
   against a fleet-divided ≈ 2.5 GiB; every daemon reads Red every tick, so every residency-asserting
   row lives under Red (the write cache answers with its floor; the read cache skips judging). Worth
   an owner look at the budget's attribution — the cage's unreclaimable arm (16 test binaries +
   daemons) over 70 % of RAM is ≈ 100 % whatever the census divides by.
5. **Two more release rows (owner, 2026-10-22), beside LTP and xfstests** — class C suites under
   `scripts/suites/` (the discipline of `fstests.sh`: the installed binary, fail early at the first
   unadjudicated check, a ledger `docs/gates/<suite>-exceptions.md` if a failure must be carried,
   `fsck --offline --scrub` clean after every run, every tripwire judged, "N consecutive clean
   run(s)"), both in `zig build gate-release`, run in the release window one at a time:
   - `scripts/suites/mdtest.sh` — `mpirun --allow-run-as-root --oversubscribe -np 32` (then 64)
     `/opt/sqz-tools/bin/mdtest -n 10000 -u -d <mnt>/mdtest`; pass = exit 0, the header names every
     rank and item, every phase in the SUMMARY table at a positive rate, the tree empty after, `df`
     (bytes and inodes) back to its baseline, `invariant_tripwires` 0, the FUSE op watchdog never
     overdue, fsck clean; the rates go to `rates-run<n>-np<N>.tsv` (evidence, not the law).
   - `scripts/suites/tarx.sh` — phase 1 the reference's `sym-tarx` gate 2 in our venue: a
     pristine linux `fs/` (2,452 entries here: `/build/kernel/linux-mainline-v7.3/fs`; `/usr/src/
     linux-mainline-sqz/fs` is the 143-entry headers tree, not the corpus) tarred once; two mounts
     of one symmetric set on this host (`suite_mount_pair`: A the manager, B joined over loopback);
     A-B-B-A arms (S0 on A, B twice into directories IT created, A again); law B's mean wall ≤ 1.10 ×
     S0, B's wire verbs (`joined_wire_verbs + xv_* + slot_* + dir_stripe_ships + dlm_token_grants +
     free_shipped_blocks`) per entry < 0.05, `slot_handovers` 0 on both, every tree byte-exact
     (sha manifest + type/mode/path listing) through BOTH mounts, both daemons healthy, fsck clean
     after both unmount. Phase 2 the parallel single-mount form: nproc/4 concurrent extractions
     into fresh directories on one mount, all byte-exact, fsck clean.
   - **First scoping runs (2026-10-22, `2b9659c` + the scripts, this box on 7.3.0-1.7):**
     `mdtest.sh` PASSED (np32: file creation 97–100 k/s, removal 95 k/s, directory creation
     102–104 k/s; np64: 94 k / 91 k / 85 k; `df` back to baseline in 5 s / 10 s; fsck clean).
     `tarx.sh` phase 2 PASSED (8 extractions in 1.84 s, 10.7 k entries/s aggregate, all byte-exact);
     phase 1's census is clean (B 0.0003–0.0057 wire verbs/entry, handovers 0, every tree
     byte-exact through both mounts, tripwires 0, fsck clean) but **the wall law MISSES: B 1.156×
     S0 at one extraction per arm, 1.137× at three** (A ≈ 0.80 s, B ≈ 0.92 s per 2,452 entries).
     Localized, not fixed: the joiner's appender ring stays at its 512 KiB floor for the mount's life
     (`joined_wire_ring_grows` 0, `appender_ring_grows` 0 — the manager had 49 MB of ring budget
     unspent) against the manager's 16 MiB journal, so B's commits hit `meta_kv_journal_full_stalls`
     2,268 times in six extractions (A: once), 4,632 checkpoint barriers (A: 171), its pass
     admission 128 ms vs 10 ms per extraction, twice the metadata device bytes. The row is a
     release blocker until the ring grows under a sustained commit stream (or the law is re-judged).
     **Fixed the same day (`948a324`): the ring grows (3 growths inside B's first extraction,
     524288 → 16 MiB); the row passed 3/3 at 1.022× / 1.094× / 0.856×.** Residual: b1 (the first B
     arm) pays the growths' drains; B's checkpoint flush 13 vs A's 7 cycles per extraction (≤ 20 ms).
     The joined conveyor's queue cells and the lane's inbound cap stay sized by the floor ring (128)
     across growth — a throughput cap under many committers, not a correctness matter.
   - **The mdtest legs' heap finding is the designed posture, not a leak:** after mass removal the
     emptied, tombstone-heavy leaves (1,408 of them after np64, holding 1,485 of 4,030 extents) fold
     only under heap pressure (`posture.isFull()` / `merge_backlog`), a later write, or
     `defrag --meta`; a quiet-time sweep would be a new law.
6. The release window (owner's go), the 200 G venue (owner's inventory), the upstream postings
   (owner: Signed-off-by, 5 items), patches 0031–0033 still unmeasured, the smaller owed items of
   the 10-20 note (item 4 there) unchanged.

## Method notes (added today)

- **The gate is fast now** (4–7 min sharded; the 65 min figure is stale). A branch gets ONE gate
  before landing; the train's three runs under `-Dfresh=true` catch what one run misses (create 7:
  gate green, train 1 in 2 red). A revert is cheap and keeps `dev` green; the branch keeps its work.
- A **stale cost table** (`tests/costs/*.tsv`) fails EVERY shard of its binary; a branch that renames
  or removes > 1/10 of a binary's tests regenerates from scratch: delete the table, `zig build costs`,
  commit (`91e0e09`, `b39b904` are the shape).
- `--summary all` prints `failed command:` for every step with stderr — read the step's status word
  (`w` passed with warnings, `failure`), not that line.
- A monitor's timeout kills what it runs: the train died with a 2 h monitor (its last run completed
  without a ledger row). Run the train under a 10 h monitor.
- Agents' clones are on named branches; their fetched commits land through the main tree (`git fetch
  <clone> <branch>`, `--ff-only`). A root-owned worktree needs `sudo rm -rf` + `git worktree prune`.
