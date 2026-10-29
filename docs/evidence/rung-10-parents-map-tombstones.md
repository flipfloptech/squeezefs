# Rung 10 — a reused mount slowed run over run: tombstones in the ino-keyed maps (2026-10-29; decision 0003 §CU)

**Symptom** (§CM.5). One mount reused for several `mpirun -np 32 mdtest -n 10000 -u -i 1` runs gets slower run over
run; the owner's profile showed `kv_fs.forgetParent` / `rememberParent`.

**Rig** (`~/tmp/parents-map/pmrig.sh`; a correctness rig, not a benchmark). 2 MDT × 16 GiB + 2 OST × 14 GiB sparse loop
images under `/build/tmp`, pid-tagged mountpoint, `--supervise`, `SQUEEZEFS_OP_PROFILE=1`, the shape 8 times back to
back on ONE mount (run *i* in `r<i>`); `.stats` before and after each run (the handler histograms), `perf record -g
-F 499 -p <daemon>` per run, and a measurement-only census (never landed) of the memos at each `.stats` lookup. Both
arms: 8/8 runs rc 0, 0 alert lines, `fsck --offline` clean.

**Cause.** `std.HashMapUnmanaged`'s removal leaves a tombstone and counts the slot available again, so the map never
grows or rehashes past it. Inos are never reused: every insert is a fresh key, which probes to the first FREE slot
(recycling the first tombstone on the way) and every miss walks to a free slot. Free slots are only consumed, so
under churn they drain geometrically (≈ e^−α per run at load α ≈ 0.61) and every miss lengthens with the mount's
history, not its live population. Three maps of this kind sit on the namespace path: the `..` memo and the depth memo
(kv_fs, one spin lock), the open table (16 striped spin locks, every entry reply and FORGET) and the rewrite epochs'
`table`/`closing` (one spin lock; `holds` probes a miss per no-open FLUSH). The `..` memo (524,288 slots) on dev:

| run | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|---|---|
| tombstones after the run | 320,034 | 444,720 | 493,211 | 512,168 | 519,609 | 522,432 | 523,581 | 523,996 |
| mean miss probe (slots) | 3.8 | 9.3 | 21.3 | 49.5 | 120 | 291 | 733 | 1,748 |
| profile: open table / memos / epochs (% daemon CPU) | 0.26 / 1.04 / 0.05 | 0.30 / 0.96 / 0.10 | 0.52 / 0.87 / 0.14 | 1.54 / 0.74 / 0.14 | 8.70 / 1.21 / 0.19 | 27.8 / 1.19 / 0.21 | 35.5 / 1.75 / 0.28 | 35.2 / 4.09 / 0.51 |

On this host the open table carries most of it (run 8: its `getOrPut` 21.9 %, `noteLookup` 7.9 % and `isDirty`
4.5 % — the stripes' spin locks held across the walks); the memos' four functions go 1.0 → 4.1 %.

**Red** (dev's maps): kv_fs "the `..` memo and the depth memo through a churn of fresh directories" — 12 rounds of
20,000 remembered then forgotten; fails at round 1 (after the removals a miss reads > 2 slots on the mean: 20,000
tombstones in 32,768 slots).

**Fix.** `core/u64_map.zig` `U64Map(V)`: open addressing, linear probing, backward-shift deletion (Knuth 6.4
Algorithm R) — a removal pulls the cluster behind the hole back, so the table holds live entries and empty slots only
and a probe's length follows the live load (≤ 3/4 by doubling; expected miss ≤ 8.5). Growth is the only allocation
(as before); the key 0 has a side slot; `std`'s room semantics (`ensureUnusedCapacity`, the `*AssumeCapacity` faces)
are kept, so the epochs' "a slot stays free for every closing epoch" law ports as is. Used by the two memos, the open
table's `map`/`owed` and the epochs' `table`/`closing`. Every map stays under the lock it had; no lock-free code is
touched. New gauges `fuse_parent_memo_{entries,slots,displacement}` and `fuse_dir_depth_memo_*`, published at each
`.stats` lookup (Σ distance from home: a hit reads `1 + displacement / entries` slots).

**After** (same rig, the fix + the census):

| run | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|---|---|
| mkdir mean µs (dev) | 94 (71) | 75 (81) | 90 (80) | 74 (95) | 89 (111) | 99 (195) | 92 (339) | 101 (370) |
| create mean µs (dev) | 92 (72) | 74 (82) | 94 (74) | 78 (102) | 94 (151) | 101 (320) | 96 (448) | 101 (466) |
| unlink mean µs (dev) | 103 (83) | 90 (89) | 99 (94) | 93 (100) | 105 (102) | 102 (112) | 103 (123) | 105 (140) |
| lookup mean µs (dev) | 4.7 (4.2) | 4.8 (5.0) | 5.7 (4.9) | 4.9 (5.8) | 5.7 (5.8) | 6.0 (6.4) | 5.8 (7.1) | 6.2 (7.0) |
| memo tombstones / end miss / peak miss | 0 / 1.00 / 3.77 | 0 / 1.00 / 2.10 | 0 / 1.00 / 3.77 | 0 / 1.00 / 1.77 | 0 / 1.00 / 3.81 | 0 / 1.00 / 3.78 | 0 / 1.00 / 3.81 | 0 / 1.00 / 3.77 |
| profile: open table / memos / epochs (%) | 0.38 / 0.60 / 0.01 | 0.30 / 0.50 / 0.05 | 0.28 / 0.51 / 0.01 | 0.26 / 0.66 / 0.02 | 0.27 / 0.58 / 0.02 | 0.23 / 0.69 / 0.02 | 0.20 / 0.64 / 0.03 | 0.20 / 0.62 / 0.03 |

The histograms are flat run over run (the arms ran at different times on a shared host: the level is not a
comparison, the trend is); the memo's peak miss is the load's (α ≈ 0.61 → 3.8) every run.

**Checks.** Units `core.u64_map` (the churn bound, the property test against `std`'s map through 4 × 40,000 random
ops with the probe law and the displacement checked, room, a refused growth), the kv_fs churn row (+ the gauges read
the scan's level); filters `fuse.handler` + `data.` + `core.` 940/940; `zig build fmt rails`. Smoke 51/51 under the
filters 300 CONCURRENT FSYNCS AND 300 CONCURRENT CREATES, THE UNLINK STORM'S INODE LAW, A STRIPED DIRECTORY ON ONE
DAEMON, A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY, RENAME_EXCHANGE AND THE PARENTS' NLINK, A RENAME THROUGH A
JOINED MOUNT, TWO MOUNTS OF ONE SET ON ONE HOST, FILE HANDLES SURVIVE A CACHE DROP, and the owed set's CUSTODY NO
CLOSE COVERS, A CLOSE BURST COALESCES, THE WRITE-BACK TIMER UNDER A WRITER STORM. xfstests generic/001, 013: 2/2,
0 alerts, fsck 0 findings.

**Seen, not changed.** Other `std` maps keyed by inos with removals (the read cache's `InoMap`, `economy`'s pending
times, `kv_fs_stripes`' `plain`) stay flat in this profile (≤ 0.05 %); each would degrade the same way under its own
churn. The reclaim cadence's tick drain (`drainGather` at batch 1) runs while corpses keep arriving — destroys lag a
removal phase (316 k of 640 k at run 1's end) — so the tick's other passes wait out a sustained storm (a census the
first rig published from the tick updated once in three minutes).
