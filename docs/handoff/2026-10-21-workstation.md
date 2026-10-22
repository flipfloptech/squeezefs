# Handoff — 2026-10-21: the fourth wave (the two caches, the mainline UAPI), create 7 reverted

**Checkpoint:** `dev` at the commit that adds this note, on `d0fcaf8` (the write cache); everything
pushed; the gate train **3/3 green on `d0fcaf8`** (403 / 215 / 213 s). Kernel `7.2.8-9.33-omarchy-sqz`
(series 0001–0033) — **the next step is the owner's reboot into `linux-mainline-sqz 7.3.0-1.7`**
(the Limine entry below the default; `fuse enable_uring=1` comes from `/etc/modprobe.d/squeezefs-fuse.conf`
so it holds on any kernel; the mainline module tree ships nvmet, nvme-loop, brd, null_blk). The
10-18/19/20 notes hold the release sequence, the bar, the rulings and the venue plan. **The release
window still waits for the owner's go.**

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

1. **Reboot into `linux-mainline-sqz 7.3.0-1.7`** (owner) → `scripts/venue status` →
   `provision-local-venue` check → the "e2e mainline pools" row, the smoke, one full gate → the
   matrix (pool vs user_ents; `SQUEEZEFS_FUSE_ZC_OPEN` 1 vs 0; 7.3 vs 9.33 numbers side by side).
   Rows that need the kmbuf/zc UAPI skip on mainline — expected. If mainline is red for the kernel's
   reasons, boot 9.33 again (still the default entry) and say what failed.
2. **Create wave 7 re-landed fixed** (agent `01a16101`): the fold-order law stated and modelled; then
   the remaining binders — the appliers' mutual leaf waits (62 of 109 µs per window on `venue`),
   `self` with reclaim destroy groups on removals, atomic-open patch 0034 (still not drafted after
   three waves).
3. The write cache's remainder (`rung-10-write-cache.md`): the FUSE transport is 52 % of the
   small-file profile (the mainline port's territory), per-grain checksums, clock stamps, the witness
   generation, per-lane regions named. The read cache's: the FUSE zc leg landing in the cache's own
   memfd slab (§BF; `SQUEEZEFS_READ_CACHE_FILL=lane` measured −20 % until then).
4. **The budget under the gate** (the write-cache agent's finding): a smoke daemon's RSS is 2.15 GB
   against a fleet-divided ≈ 2.5 GiB; every daemon reads Red every tick, so every residency-asserting
   row lives under Red (the write cache answers with its floor; the read cache skips judging). Worth
   an owner look at the budget's attribution — the cage's unreclaimable arm (16 test binaries +
   daemons) over 70 % of RAM is ≈ 100 % whatever the census divides by.
5. **Two more release rows (owner, 2026-10-22), beside LTP and xfstests:** (a) an `mpirun` of mdtest
   must pass (the perf shape — `mpirun --allow-run-as-root --oversubscribe -np 32`
   `/opt/sqz-tools/bin/mdtest -n 10000 -u`; pass = 0 errors, fsck clean, tripwires 0, no daemon
   alert); (b) the parallel `tar -x` the reference's acceptance ensured — its `sym-tarx` gate 2
   (`~/Source/squeezefs/tests/run_mw_matrix.sh` `leg_sym_tarx`; `tests/cloud_sym_rows.sh`): the linux
   `fs/` corpus (2,384–2,468 entries) extracted by a JOINED writer into a directory it created,
   A-B-B-A against the manager's own extract S0; law wall ≤ 1.10× S0, wire verbs/entry < 0.05,
   slot handovers 0, rpcs 0, fsck clean. Our tree has the single-mount concurrent form
   (`scripts/bracket/rows/tar-x.sh`) and the contract (`docs/porting/sym_tokens_intents_striping.md`,
   D1); the two need suite scripts under `scripts/suites/` (`mdtest.sh`, `tarx.sh`: fail early, the
   ledger discipline of `fstests.sh`) — class C, run in the release window.
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
