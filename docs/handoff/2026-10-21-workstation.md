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
| `2093e40` | **a reader's projection follows a slot's LIVE home** (the ONLINE FSCK row's "roots stayed stale through 64 re-points", 1 train run in 6): tree 0's `Leased` record carries the GRANT-TIME root (spec §1.2's two homes); the read-only open folded tree 0 from appender 0's page only, so a slot B took from A by handover opened at the root A released — B's compactions moved it and recycled the image → `RootStale` forever. Now every live appender page is folded, a foreign lessee's moving tree is skipped (read under tokens; `reader_foreign_roots_skipped`), the manager's own roots re-point per slot, and the whole-projection re-point ends only after BOTH a landing ceiling and 64 attempts. Reader posture only; the writer's open is unchanged. Owed: the WRITER's open still projects a live foreign lessee's tree at the grant-time word (a manager remount beside a live B that recycled the image would refuse) — fixing it moves the leak census (the KVDA PAGE SINK row's inputs); `[C3] refcount 37 vs 36` from `scrub --online` under heavy load is pre-existing (4/10 on a dev baseline) |
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
   **The 7.3 matrix ran** (`rung-10-fuse-mainline-uapi.md` §10): bandwidth at parity with 9.33
   (64k 58.2 GB/s, seq read 74.6, seq write 52.7); 4k randread −31 % (1.30 vs 1.88 M, p50 64 → 90 µs),
   4k randwrite −12 % (1.17 vs 1.33 M) — the round trip, where 9.33's 0030/0031 live. §BE.3's
   zc-open default CONFIRMED (`ZC_OPEN=0` halves every bandwidth row and moves no 4k row); the pool
   rung is 1.8× / 4.5–6× over the stock `user` rung. Next lever on 7.3: 0031 re-expressed on
   mainline's `dev_uring.c` and posted; a per-request profile of the pool rung's 4k completion path.
   The 9.33 kernel has NOT run the day's landings (every change is conditional on the probed rung;
   boot it once before the release and run one gate).
2. The create path's remaining binders — the appliers' mutual leaf waits (62 of 109 µs per window
   on `venue`), `self` with reclaim destroy groups on removals, atomic-open patch 0034 (still not
   drafted). From create 7b's report, owed: the DEAD MANAGER row's hang on the train was B parking
   TWICE (the second reclaim → `UnknownLease` → poisoned, one cross-owner create parked forever at
   `[handler_entry]`) — a load sensitivity of `T_owner`; the replay gate drops a straggler Δ the node
   never saw (an exact-seq gate would keep it); ~~the shipped setattr carries no `kill_suidgid`~~ (DONE
   2026-10-23, decision 0003 §BK: `SETATTR_KILL_SUIDGID` on the wire, applied at the holder); under doubled load `A
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
     **The wall's bimodality (12:51–12:56: A 1.66 → 0.97 s between arms, MISS 1.54×) was the laptop
     ON BATTERY** (`rung-10-tarx-wall-validity.md`: every latency the daemon measures shifted
     ×1.5–2.5 uniformly, the budget Green, no stall counter moved; upower: discharging 12:29–13:05).
     The row now refuses battery, pins the clock (boost off, restored), records the host posture per
     arm and judges the phase VALID only with one posture and each mount's arms within 1.20× — else
     INVALID, re-run on a fresh set, never a pass. 3/3 PASSED on mains at 0.866× / 0.838× / 0.904×.
     Residual: a2 is always 1.05–1.18× a1 (the set's kv root grows 3 levels in b2/a2, SMO retries
     0 → 20, the journal thread +30–70 % CPU) — the A-spread bound sits close to it.
   - **The mdtest legs' heap finding is the designed posture, not a leak:** after mass removal the
     emptied, tombstone-heavy leaves (1,408 of them after np64, holding 1,485 of 4,030 extents) fold
     only under heap pressure (`posture.isFull()` / `merge_backlog`), a later write, or
     `defrag --meta`; a quiet-time sweep would be a new law.
6. The release window (owner's go), the 200 G venue (owner's inventory), the upstream postings
   (owner: Signed-off-by, 5 items), patches 0031–0033 still unmeasured, the smaller owed items of
   the 10-20 note (item 4 there) unchanged.

## The release window on 7.3 (opened 2026-10-22 ~07:50 local, owner's go: "run the rest of the things we need to run for a release here, circle back to 9.33 after")

Sequence run: gate 3/3 → pjdfstest ×3 → LTP → xfstests `-g auto` → mdtest ×3 → tarx ×3, fixing as
each suite surfaced an item, every fix through its own gate, every affected suite re-run on the
fixed head. **Every landing below went through a green gate (80–86) on this kernel.** The window's
state at the time of this amendment is in the table at the end.

| landed | finding → fix |
|---|---|
| `948ebbe` | **xfstests generic/029 — silent data loss across a remount.** Two mechanisms, neither the mainline posture: the staged-write shortcut judged "inside the image" by the INODE's size, so after a truncate-up the tail was re-staged under a record still saying the truncated-down length and `promoteInline` clipped it at promotion; and the dismount sweep flushed only inodes with a parked SIZE claim, so an overwrite under the write-back timer could leave RAM un-landed at an orderly unmount (charter §6.2: the unmount drains synchronously). Rows AN ORDERLY UNMOUNT LANDS EVERY ACKED WRITE ×2; `rung-10-unmount-owes.md` |
| `94c36d0` | **LTP fs — tripwire `pack_pin_refcount_drift`** (ftest01's small-file storm): the seal raised `sealed` OUTSIDE the pin lock and censused after; a tenant release observing the flag between the two used the sealed ladder. Census + flag are one critical section, census first; model + weakening; external-suites defect 28 |
| `b7e79a3` | **mdtest — one unlinked inode never destroyed** (`df` inodes 3 vs 2 for 120 s): the FORGET's try-only read met an SMO swap window; the store faces mapped the tree's `WouldBlock` to `Corrupt` and the handler judged any non-WouldBlock error "live" and dropped the corpse (fsck's own mount sweep reclaimed it, hence 0 findings). A try-only read answers `WouldBlock`; a FORGET defers on any refusal; row THE UNLINK STORM'S INODE LAW; mdtest 3/3 under load |
| `a08fce3` | **the `syncfs: a leg failed (InputOutput)` storm** (1,407 lines in one LTP fs run, EIO to `sync(2)` callers): (a) a promotion leased the census's record BEFORE the locks; a sibling's re-stage moved the generation → `.kept` → EIO — now promotes the CURRENT resident under the locks; (b) a `truncate -s 0`'s commit-to-release window left a record the layout no longer names → `LiveVerdict.unnamed → .absent` (`fsync_promote_superseded`). **Harness:** `suite_daemon_alerts` matched `^(error|warn)\(` — anchored under the `--log-file` epoch prefix and `warn(` never occurs (`warning(`): 1,407 error lines had counted as 0 alerts; `SUITE_ALERT_RE` fixed, every suite now reports its count. Defects 29/30 |
| `5e28397` | **LTP syscalls `kill11` broken (2 min)** — NOT the filesystem: `ltp.sh` launched kirk as a bash asynchronous list, which starts with SIGINT and SIGQUIT IGNORED; SIG_IGN survives exec down to every test, so the child never received the SIGQUIT (LTP restores SIGINT itself, hence `TPASS SIGINT`). Deterministic since the fail-early watcher (`eea8892`, 10-17); "passes by hand" because by hand is the foreground — and the harness's ext4 control ran in the foreground too, which mis-attributed it to us. `env --default-signal=INT,QUIT` + a SigIgn tripwire; `suite_umount` also detaches loop devices a killed test left. Kernel-buffer writes (coredump/sendfile/splice/loop) under zero-copy opens verified fine 10/10 × 3 postures |
| `4af7a01`…`448b498` | **`directory nlink underflow guard fired` ×4 during generic/025 (RENAME_EXCHANGE)**: every cross-directory swap of a directory with a non-directory shifted NO parent count (the local `economy.stageRename` EXCHANGE branch staged Δtimes only; the cross-owner planner already had the shift). Law table (8 cells) on the line; unit + row (the renameat2 matrix, `st_nlink` judged after setup/swap/cleanup/remount); the guard's text corrected (it masks a missed +1, creates no deficit). **fsck has NO directory-nlink arm** (`ops_fsck.md` §3.10 "directories are never a count finding") — a surviving inflated directory is invisible to fsck; owed below |
| `21a2308` `70ead87` | **generic/475's alert classes**: `the truncate's tail zero-complete failed (NoSpaceLeft) — bytes past the new end may read back` ×1,549 — the message was stale since §AL.1: the shrink's zero write was redundant (every grow lands its own zeros before the size moves), deleted (§AL.7); the set really was full (475's crash loop filled both volumes, the ring stopped draining). `staging ring full` ×5,790 — genuine refusals (first-fit at ~77 % fill), the latch cleared on ANY landing; now one warning per episode, closed by a quarter-ring drain (`staging_ring_full_episodes`); a 4,000-op placement-completeness oracle test says the hole search is sound |
| `d848fb5` | **xfstests generic/604 — `umount &` then `mount` 10 ms later refused BUSY**: the exiting daemon still held the writer guard. `guard/proc.zig flockExclusive` judges the holder each round on kernel facts (pid gone → replay wait; alive + detached → wait; alive + `squeezefs umount <mnt>` in flight → wait; alive + attached + no verb → BUSY at once), ceiling 36 s (`DAEMON_EXIT_WAIT_NS` + 2 ticks); the symmetric join door rides the same wait; `mount.zig`'s mountpoint pre-check is `statx(AT_STATX_DONT_SYNC)`. Row A MOUNT THAT RACES THE PREVIOUS UNMOUNT WAITS, THEN SERVES 20/20 under load; the helper's `dmsetup remove` race fixed with a per-set flock |
| `08a9aaa` `343a8d9` `7e0792a` | **xfstests generic/620 — the 17 TB dm-hugedisk**: the HARNESS refused any block device with no backing file (it keyed a set's log dir by the loop image's path) and carved fixed 1 + 2 × 8 GiB slices; also `check`'s bare `mount` could not type the device. Now any raw block device is a set device (the PRODUCTION shape — nvmet namespaces), slices follow the device, `mount.typed` types a bare mount. **The product at 17 TiB: format 0.29 s / 33.9 MB written, mount +107 MB RSS (24 B/block), 1 MiB write+fsync 23 ms, fsck 0.60 s over exactly the used blocks — everything O(structures)/O(used)**; tie test in `data_superblock.zig`; limits table in `rung-10-huge-volume.md` (volume_len/capacity u64; per-file EFBIG at 2³²−1 blocks = 16 PiB at 4 MiB; 2⁵⁶ inodes; the allocator table 6 MB RAM per TiB). Ten stale scsi_debug/loop-over-loop exclusions dropped: those tests judge the product now |
| `4a7401e` `27b8ceb` | **tarx's wall law** read 0.86–1.54× within an hour: the laptop was ON BATTERY 12:29–13:05 (every daemon latency shifted uniformly with the clock; budget Green, no shedding). Three extractions per arm; the row refuses a battery run, pins the clock (boost off, restored), records the host posture per arm, calls a phase INVALID (re-run, never a pass) when the posture moves or a mount's two arms disagree past 1.20× |

| `60b8242`…`f11cd41` (the audit), `d750a25` `c0da49d` | **The owner's rule (2026-10-22): never skip or adjudicate unless 100 % certain it is not a bug/defect/unsupported.** The audit re-ran every exclusion on this host (`rung-10-adjudication-audit.md`): ten container-era rows pass here (deleted); the exportfs five FAIL on the product (ESTALE: no `FUSE_EXPORT_SUPPORT`) and are now UNADJUDICATED; `proc01` deleted; the LTP ext4 control ran from the wrong cwd against modules built for 7.2.8 (fixed; "venue's" needs the SAME failing assertions); generic/460 had left the host at `dirty_ratio=100` (restored). The harness takes any device (raw or 64 MB: the layout scales to the product's floor — smallest set 36 MiB: 11.8 MB meta + 2 × 3 chunks; `rung-10-small-volume.md`). generic/563's cgroup charge is unsatisfiable under FUSE (the kernel charges a bio to the issuing task; the daemon is the issuer) — a proposed `by-design-fuse-io-charge` row awaits the owner |
| `c3e53e2` (F4) | **the DEAD MANAGER row's hang** (`rung-10-recovery-live-member-evicted.md`): the gate's load stalled B's membership beat past `T_owner`; A evicted the LIVE B; B kept committing past `T_self` (its committers never judged the deadline); its reclaim met the closed grace → POISONED; the poisoned mount HELD every foreign op at the move gate for `T_park_max` per op (the unkillable storm child); its unmount ran a clean leave over A's recovered regions. Committers judge `T_self` on the member's own clock; `poison` closes the move epoch (EIO at once); a fenced leave writes nothing; a same-incarnation stale epoch re-points. Seam `SQUEEZEFS_TEST_MEMBERSHIP_BEAT_HOLD`; row A LIVE MEMBER EVICTED AT T_OWNER… 20/20 |
| `4c7cf3e` (F8) | **a joiner's rename into the manager's directory EIO after 90 s** (generic/732 under the shipped symmetric posture): the intent of a rename whose every object the manager holds was written into "the mount's own rotor slot" — hard-wired to slot 0, the MANAGER's; the joiner's commit met its own door's foreign-lease refusal and the move gate re-issued it 513 times. The intent home is a slot the mount leases (`Resolver.ownHome`); row with 12 cross-owner renames ≤ 2 s each; generic/732 PASSES symmetric (`rung-10-joiner-rename-home.md`) |
| `feat/atime-posture` (branch; `rung-10-atime.md`) | **xfstests generic/003 and 192 — the atime posture** (the owner's 100 % rule: the `noatime` adjudication was true but incomplete — the kernel decides whether a read moves atime, never tells a FUSE daemon, and the next GETATTR overwrites its own stamp). Now the posture is the KERNEL MOUNT's, read from the daemon's mountinfo entry at mount and at every mount-table change (a meta fiber on `POLL_ADD(POLLPRI)` of mountinfo: `-o remount,strictatime` wakes it), `SQUEEZEFS_ATIME` overriding; `atime_needs_update` restated (`fuse/atime.zig`) on READ/READDIR/READLINK and noted by every record-less leg (the worker's probe serve and direct leg — the staged serve was the red the first hand probe found —, the IPC fast path, the direct arm's lease) as a relatime candidate the fold/drain judge; a due stamp is a parked refinement → `Δatime` by the times drain (never a ctime, never an entry per read); a foreign-slot stamp ships as `SETATTR_ATIME_TOUCH` (wire bit 9 — FULL tier); an atime-only Δ moves neither the read view's witness nor a direct lease's epoch (THE READ VIEW and THE DIRECT ARM rows caught both). Row ATIME — RELATIME, STRICTATIME, NOATIME ARE THE KERNEL MOUNT'S 10/10, red on `9d9e3ce`. **The gap that stays (a kernel one):** a read the kernel serves from its page cache under `FOPEN_KEEP_CACHE` reaches no daemon; its in-core stamp is overwritten by the next GETATTR — `fuse_flush_times` carrying `FATTR_ATIME` would close it (owner-call for the series) |
| `741615d` `b43167c` (F7) | **a full data store answered EIO** to fsync/syncfs and fired `alloc_trim_window_parks` (a counted regime misdeclared as a tripwire). Now: the park is a counter (+ a once-per-episode wall warning for a stalled device); **ENOSPC, never EIO**, at `write(2)` (the write cache's residents carry a CLAIM on the store; `admitWrite` judges reachable free bytes against it; `statfs` subtracts it) and at the sync point (`LiveVerdict.no_space`; bytes stay resident; one `data store FULL` line per episode; a `no_space` promotion retries once on another volume). Charter §6.1/§6.2 amended; `rung-10-full-store-enospc.md`; generic/750/475/476 pass with 0 tripwires |

**Verdicts on the fixed heads:** pjdfstest 3/3 (3492203); LTP fs + syscalls **PASSED, 0 alerts**
(d848fb5: 71/72 + 1 conf; 1,403 pass / 128 conf / 5 adjudicated + 15 venue); mdtest 3/3 (b7e79a3,
under load); tarx 3/3 (27b8ceb: 0.87× / 0.84× / 0.90×); xfstests: 598 ran on d848fb5 to generic/620 (harness), 546 on 7e0792a to
generic/563 (harness) — both fixed; 0 alerts on the test set in both; the run from the top on the
final head waits on the rulings above.

**OWNER RULINGS the window waits on (the next `-g auto` stops at generic/426 by the 100 % rule):**
1. exportfs / `open_by_handle_at` (generic/426, 467, 477, 756, 777): unsupported by ruling, or feature
   F3 (~1 day: advertise `FUSE_EXPORT_SUPPORT`, serve `LOOKUP(nodeid, ".")` for any inode, a durable
   parent for `..`). Recommendation: implement.
2. generic/563's cgroup io charge: approve the proposed `by-design-fuse-io-charge` row (certain).
3. ~~The suites' format posture~~ **RULED 2026-10-23: the shipped symmetric default.**
   `SUITE_SINGLE_WRITER=0` is every suite's default (`=1` keeps the `--single-writer` run possible);
   409/410/411/589/732 judge joins and left the ledger; the umount persona's bind/move gap (F6), the
   verb's bind behaviour, the 30 s residency wait behind a bind and a `.stats` render race that killed
   a daemon mid-teardown are fixed — `rung-10-suites-symmetric-posture.md`.
4. fallocate (F1): every mode EOPNOTSUPP; the porting spec says MUST; 31 xfstests `_notrun` + LTP
   `fallocate03`/`mremap06` behind it. In scope for the tag?
5. Inode flags `FS_IOC_SETFLAGS` (F2; LTP `setxattr03`): in scope?
6. Flake rows `-o ro READER` and `MOVE MATRIX` unseen in 61 train runs: retire? DEAD MANAGER leaves
   the ledger after three green trains on `c3e53e2`.

**Owed from the window (not blockers; in order of weight):**
1. **fsck C10 has no directory-nlink arm** — a directory's nlink too high or too low is never a
   finding (the ported spec's rule). Full tier: per-parent subdir census, the stripe fold, repair
   semantics, a decision-0002 ledger row (diverges from the reference). The RENAME_EXCHANGE defect
   was invisible to it.
2. The pending-free wedge under `HARD BACKSTOP` → `VolumeFailed` (mdtest -np 64 with a 2.75 GB
   fleet-divided budget under three agents' load): the checkpoint tail under memory starvation.
3. The WRITER's open still projects a live foreign lessee's tree at the grant-time word (the reader
   half landed in `2093e40`); fixing it moves the leak census (the KVDA row's inputs).
4. `PromoteSink.onEvict` judges by a cache-entry generation an in-place append does not bump: such
   an entry is dropped without promoting while the record stays resident and charged.
5. Under dm-error crash loops: `staged payload unreachable … serving zeros` (a promotion released its
   ring entry at commit, the journal window carrying the layout later lost); recovered unnamed
   residents (generation 0) never destroyed on a live mount; per-op `fsync/syncfs … a leg failed`
   lines on a FAILED volume want a once-per-episode latch; `TRIPWIRE alloc_trim_window_parks` in the
   dm-error daemons (baseline too).
6. Stripe migration vs cross-stripe moves of a migrated subdirectory (`parent_update = .none` then
   old −1 / new +1 on the stripes); the holder census is blind to kernel-held mount references (loop
   backing files, stacked mounts); a raw `kill -TERM` with the mount attached is still an at-once
   BUSY for a racing mount; first-fit ring fragmentation (5 MB refused from ~77 % fill, no
   compaction); a2 systematically slower than a1 in tarx (SMO retries grow with the set).
7. New since the audit: ~~`exec/cpu_class.zig:101 finishPass` panic (a departed thread's ledger entry
   with class 170 of 13) killed KILL ROWS (c)'s daemon once under load~~ (two `.stats` renders racing
   in ONE ledger — fixed 2026-10-23, `Sources.render_lock`); the manager accepts
   `ReleaseSlot` from an appender whose region it recovered (sender fixed, serve-side screen owed);
   a parked cross-directory rename holds the kernel's `s_vfs_rename_mutex`; ~~the umount helper's
   bind/move gap (F6)~~ (fixed 2026-10-23); `fresh supply EXHAUSTED` info line per re-latch; `MetaBarrierFailed` per fsync
   on a FAILED volume; a recovered-but-unnamed ring record judged only at a promotion point.
8. Harness: the fail-early stop SIGKILLs kirk (no JSON report); subset runs end "ledger rc 3" on
   stale rows by design; local-venue daemons inherit the desktop's cgroup (the budget's
   unreclaimable arm would read Red at 33 GB of desktop memory); `/sbin/*.squeezefs` helper symlinks
   and `/run/squeezefs-suites.env` are venue-global and repoint to whichever tree ran last.

## The release window continued, 2026-10-23 → 10-25 (on 7.3; checkpoint `dev` `78a4389`)

**The six owner rulings of 10-23** and **the six of 10-24** are all landed: export handles
(`380e4c1`), inode flags (`07d0e74`), fallocate (`2a079de` + follow-ups), the suites on the shipped
SYMMETRIC posture (`291ca31`), generic/563 by-design row, two flake rows retired; generic/741
by-design-symmetric-join, generic/424 statx row with NO kernel patches, FIEMAP UNSUPPORTED (the
kernel answers it for FUSE; the patch stays on the unmerged branch `feat/fiemap`), the supervisor
HOLDS a failed replay's journal tail (§BR), a live member evicted with nothing un-landed RE-JOINS IN
PLACE (§BS), no write cache without a supervisor (§BR.6). The owner's standing rules from the
window: never skip or adjudicate unless 100 % certain; a failed suite means fix now; no disk-size
limit; xfstests watched by log, fail fast, resume discovery after a failure (`FSTESTS_AFTER`).

**xfstests `-g auto` PASSED on `e5253be`** (794 ran, the 6 failures all adjudicated rows, 0
unadjudicated, fsck 0 findings on both sets; the alert census explained: ~100 dm-error FAILED
mounts × the once-per-class latch lines, the scsi_debug discard notice, generic/347's watchdog
during dm-thin's 60 s hold). The harness moved its sets to DISK (`/build/suites-disk`, no-CoW
btrfs — the zram /build/tmp filled under 299/302/303) and gained a run token (a hand-run `check`
had unmounted 366's scratch), scope cleanup after a fail-early stop, raw-device and 36 MiB–17 TiB
sets.

**What the suites and their follow-ups found and fixed (gates 87–138), by class:**
- **Silent data loss / wrong bytes (every one red-first):** a failed read freed its buffer twice
  (`b61d5cb`); a punch starting on a block boundary zeroed nothing (`8ad5340`); a freed block re-used
  before its free was durable — after a power cut one file read ANOTHER file's bytes and fsck was
  clean (`b23ed27`, §BT); a joiner's append to a holder's file read zeros (the holder ran the size
  claim through its truncate path), a stale grant size overwrote the holder's bytes (`a81c2b7`);
  alternating appends lost bytes (`1afede7`, register-then-check); a truncate racing a claim regrew
  over clipped blocks (`9e3b042`); a joiner read a holder's block-map file as zeros (`27740ec`); a
  joiner wrote into the manager's tree 0 on disk (`ac7986e`); a failed re-stage left zeros
  (`c9b0a4d`, `e56c1ee`); every unservable foreign binding now answers EIO, never zeros (`ab57acf`).
- **Hangs/wedges and killed mounts:** the times drain held a directory lock until its journal landed
  (`9075cbd`); a stalled device's parked write waited for the kernel's 135 s abort (now ENOSPC/EIO
  before it, `d5f06b1`); a coherence notify unmounted binds (`1d4c668`); the sideband locked a
  directory it owed a reply on (`b9e7e3d`); a live member evicted behind slow follow-ups (`13e3334`);
  a join after a manager remount dialled the dead port (`71374e9`).
- **No EAGAIN anywhere:** fsyncs coalesce (`c46daa5`), committers park (`5c9e261`), full device lanes
  park (`5898907`), the interposer's handoff waits (`216af19`); short counts are POSIX (`4599ba2`).
- **Memory and caches:** an idle daemon 2,035 → 262 MB at a 64 MiB budget (`3985727`…`702cce1`); the
  census storm (a writer wiped peers' read caches, `0fbd2c4`); inserts lost a doubling race
  (`33e9b2d`).
- **Tripwires reclassified as counted regimes:** `alloc_trim_window_parks`,
  `parked_gate_wall_supply_visible`, `upload_join_overlong`; the conveyor's window gate
  (`9744d22`, a real correctness fix behind `meta_conveyor_pass_panics`).
- **Log hygiene:** a FAILED volume logs each class once (`5d3e411`, `afd1649`: 12,110 → 1,802 lines).

**Perf A-B-B-A on 7.3 (`d46dd83`):** atime + idle RSS + the window gate cost nothing measurable
(rr4k +0.5 %, sw1m −0.6 %). **Owed on a quiet host:** fsync coalescing, the free hold, the resolve
fault's two loads per write, the append path, the census fix's read-while-write win.

**Next (in order):** the `writerFlockHeld` red (agent in flight) → the final head → the perf session
→ the verdict suites on the final head (xfstests, LTP, pjdfstest/mdtest/tarx ×3) → the final gate
with no fixes + the train 3/3 → the tag (owner confirms) → the 9.33 boot + one gate. **Owed, not
blocking:** cache-less small-file packing (a per-file exclusion with a model, §BR.11 (d)); the
watchdog's repeated census during a known device stall; small-block read-cache hits; the map
window's over-purge on a slide.

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
