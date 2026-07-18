# Multi-reference scoreboard — inaugural baseline (2026-07-18)

**Charter**: USER DIRECTIVE (2026-07-18, verbatim): *"if we are not one of the
top 3 fastest FUSE filesystems that exist, I don't care about any other
feature."* This report is the **inaugural run** of the standing multi-reference
scoreboard (`tests/run_scoreboard.sh`, landed this session) — the top-3 proof
surface. It absorbs and supersedes the vs-JuiceFS scoreboard
(`tests/run_vs_juicefs.sh`, retired forward-only; lineage:
`.benchmarks/2026-07-15-vs-juicefs-scoreboard.md` inaugural,
`.benchmarks/2026-07-17-rand-write-program-closing.md` §2 closing standing
13 W / 3 TIE). The **kernel-FUSE table is the primary claim surface**; a future
L4 LD_PRELOAD interception mode will add separately-labeled rows, never mixed
into this grid. This run captures the **post-L3 posture**
(`.benchmarks/2026-07-18-l3-transport-economy.md`: device-true rand-4k 604,313
IOPS / warm 644,726 on the devsub substrate).

**Headline**: SqueezeFS is **top-3 or better on all six row-families** and
**rank 1 of the five-system field on 13 of 18 primary rows** (rank 2 on the
other five). 59 W · 2 TIE · 3 L over 66 comparable cells; all three losses
adjudicated + attributed below (gate posture: allowlist of exactly those
three). RW6 did its job: the old JuiceFS seq-write ACK-artifact allowlist
rows flipped to outright wins at matched durability — **allowlist → ∅** for
that family.

## Reference field (user-confirmed; pinned releases)

| System | Column | Stack under test | Pin | Archive checksum |
|---|---|---|---|---|
| SqueezeFS | `sqz` | this repo — product code = dev `5bac2be` exactly (the branch adds harness/docs only; binary md5 `2e2775a0c63af876a5c1cf9817d59752`) | — | — |
| JuiceFS | `jfs` | `juicefs mount` FUSE daemon, sqlite3 meta + `file://` objstore on the substrate | **1.4.0** (+ upstream `ca2aef0` cherry-pick, source-built — see §Kernel note; binary sha256 `c20be65f…`) | release tarball sha256 `6dedd730…` |
| SeaweedFS | `swfs` | **native stack**: `weed server` (master+volume+filer, one node) + `weed mount` (FUSE) | **4.39** (`db42bb49`) | vendor md5 `5dc4acbf…` / sha256 `d25a3f5d…` |
| geesefs | `gee` | S3-backed FUSE client over the shared local RustFS store | **0.43.8** | binary sha256 `81dd5a90…` |
| mountpoint-s3 | `mps3` | S3-backed FUSE client (AWS) over the SAME RustFS store | **1.22.3** | tarball sha256 `54a7e2e2…` |
| RustFS | (backend for gee+mps3) | S3-compatible object store, Rust; **held constant** so the gee/mps3 rows differ only by client | **1.0.0-beta.10** (no stable RustFS release exists as of 2026-07 — newest beta, recorded as such) | zip sha256 `2bea1080…` |
| elbencho | (driver) | the only instrument, every row | **3.1-9** | static tarball sha256 `beda2921…` |

goofys was SKIPPED (unmaintained); MooseFS REJECTED (user: not interested).
Full checksums are pinned in the harness (`PIN_*` table,
`tests/run_scoreboard.sh`); binaries live in the non-repo tools dir
`/var/tmp/squeezefs-scoreboard-tools/`.

## Provenance

| | |
|---|---|
| Harness | `tests/run_scoreboard.sh` @ `fa892f1` (branch `feat/multi-ref-scoreboard` off dev `5bac2be`), defaults: `SQUEEZEFS_SB_CACHE_MB=4096`, dataset 16 GiB (16×1 GiB), tree 131,072×4 KiB files, timelimit 30 s, cages 16 G |
| Box | AMD RYZEN AI MAX+ PRO 395 (32 CPUs online, **capped 3.5 GHz**, performance governor), 109 GiB RAM, PC SN8000S 2TB NVMe, btrfs, kernel `7.1.3-2-cachyos` |
| Substrate | `/var/tmp/squeezefs_scoreboard` (btrfs on nvme0n1p2) — **every** system's durable stores, meta, objstores, and caches in the same directory (matched-substrate protocol; refused if tmpfs). NOTE: this is the vs-JuiceFS baseline's substrate class, NOT the zram devsub of the L3 report — absolute IOPS here are device-bound and not comparable to the L3 headline numbers |
| Instrument | **elbencho 3.1-9** for every timed row (page-aligns its O_DIRECT buffers — the AGENTS.md instrument-alignment lesson). The RW6 durability pass is the harness's own timed python step (see §RW6) — stated per row below |
| Rails | `taskset -c 0-15` daemons + drivers; `systemd-run --user --scope` memcg cages (16 G; R2 store-side 2 G); house quiet gate per row (co-tenants + Tctl) + session-start load gate; per-row honesty lines; kills by PID; `/mnt/squeezefs`, `/mnt/juicefs`, `~/tmp/nvme`, user containers, zram/nullb/nvmet state and `/usr/local/bin/squeezefs` untouched; dedicated port slice 53300–53399 (+ weed gRPC 63321–63323) preflighted free |
| Budgets | one knob → every system's documented cache surface: sqz `--mem-budget 4096M --disk-cache-size 4096MB`; jfs `--buffer-size 4096 --cache-size 4096`; swfs `-cacheCapacityMB=4096`; gee `--memory-limit 4096` (disk cache off = their default; they ship no disk-cache size cap); mps3 `--cache <dir> --max-cache-size 4096` (their documented caching config; bare default is cache-off). Where the mapping errs, it errs in the references' favor (their meta engines / index RAM are unbudgeted) |
| Fair-posture deviations (all recorded) | jfs `--trash-days 0` (matched delete semantics; their default trash renames instead of destroying); mps3 `--allow-delete --allow-overwrite` (documented opt-ins the grid requires) + `--force-path-style --region us-east-1` (local endpoint plumbing); gee/mps3 `--endpoint http://127.0.0.1:53331`; jfs `--metrics 127.0.0.1:0` (their default 9567 is a live user tenant on this box). Everything else: shipped defaults |
| JuiceFS meta engine | **sqlite3** (no redis-server binary on this box; the harness auto-prefers redis when present — same posture as the 2026-07-15 baseline). Re-run on a redis box before citing jfs metadata rows externally |
| Verification | R2 read rows verified per row: sqz via `.stats` (`direct_device_true=true`, `read_device_true_readsΔ`, device bytes ≥ 0.5× user bytes); jfs via its `.stats` counters (objGET bytes ≥ 0.9× user bytes + device bytes); swfs/gee/mps3 via diskstats device-byte evidence (≥ 0.5× user bytes). Failures flag the cell `!DEV` and gate |
| Artifacts | `/var/tmp/squeezefs_scoreboard/artifacts/<runstamp>/` — per-row elbencho stdout + CSV, `.stats` before/after, `/proc/<pid>/{io,stat}` deltas, diskstats deltas, durability-pass splits, honesty lines, mount/server logs, `capabilities.tsv`, `scoreboard.md`/`.tsv` |

## RW6 — durability-leveled timing (what exactly was timed)

Write-family rows (`seq_write_1m`, `rand_write_4k`) carry two modes:

* **relaxed** — the elbencho WRITE phase exactly as before: each system's
  native ACK semantics. Published labeled, never gating. Known ACK postures:
  geesefs ACKs before flush by default (`--fsync-on-close` off; fsync honored
  — its relaxed rows are the poster child for the label); JuiceFS's `file://`
  objstore ACKs ride the kernel page cache; mount-s3 `close()` blocks on
  upload completion (relaxed ≈ durable by construction); SqueezeFS
  write-through puts bytes on the device inside the row.
* **durable** — `value = phase bytes (or ops) ÷ (elbencho WRITE elapsed
  [csv "time ms [last]"] + durability-pass elapsed)`, where the pass runs
  immediately after the phase, identically for every system:
  1. `os.fdatasync()` on each of the 16 dataset files through the FUSE mount
     (O_RDWR reopen; O_RDONLY fallback where write-open is refused — mps3);
  2. `syncfs()` on the mount (FUSE_SYNCFS where the client honors it);
  3. `syncfs()` on the substrate directory — every system's local backing
     store (jfs `file://` objects, RustFS volumes, weed volume files, sqz
     image files) flushed to the device: the matched "bytes on stable
     storage" boundary.

**Why not elbencho's own `--sync`** (verified from elbencho v3.1-9 source,
`Coordinator::runSyncAndDropCaches` + `LocalWorker::anyModeSync`): `--sync`
runs as a *separate* SYNC benchmark phase — `syncfs()` on the bench paths —
whose elapsed is excluded from the WRITE row's number, and a FUSE client that
no-ops `FUSE_SYNCFS` would make it a no-op entirely; elbencho has no
fsync-at-close / O_SYNC option. The harness pass closes both gaps and its
component split (`fdatasync_s= syncfs_s=`) is recorded per row in the
artifacts.

Read/stat/del rows are unaffected (no durability semantics).

## Capability matrix (N/S cells — verified, not guessed)

| Ref | Workload | Declared reason | Empirical verification |
|---|---|---|---|
| mps3 | rand_write_4k (all regimes, both modes) | sequential-upload semantics: no random/out-of-order writes by design | **refused(EBADF)** on offset write to an existing object (recorded in `capabilities.tsv` by the mount-time probe) |

N/S cells are neutral: never "0 IOPS", never a LOSS, excluded from rank
denominators. Every other (ref, workload) pair ran.

<!-- grid inserted from the run's scoreboard.md verbatim -->

## Primary kernel-FUSE table (gating; write rows = durable mode)

| Row | Workload (unit) | SQZ | JFS | SQZ/jfs | v | SWFS | SQZ/swfs | v | GEE | SQZ/gee | v | MPS3 | SQZ/mps3 | v | Rank |
|---|---|---:|---:|---:|:--:|---:|---:|:--:|---:|---:|:--:|---:|---:|:--:|:--:|
| R1.seq_write_1m@durable | seq_write_1m (MiB/s) | 4,354 | 3,690 | 1.18x | **W** | 2,401 | 1.81x | **W** | 1,363 | 3.19x | **W** | 1,236 | 3.52x | **W** | 1/5 |
| R1.seq_read_1m | seq_read_1m (MiB/s) | 6,627 | 6,235 | 1.06x | **W** | 873 | 7.59x | **W** | 3,690 | 1.80x | **W** | 1,875 | 3.53x | **W** | 1/5 |
| R1.rand_read_4k | rand_read_4k (IOPS) | 164,976 | 84,053 | 1.96x | **W** | 44,631 | 3.70x | **W** | 9,147 | 18.04x | **W** | 1,601 | 103.05x | **W** | 1/5 |
| R1.rand_write_4k@durable | rand_write_4k (IOPS) | 35,450 | 3,429 | 10.34x | **W** | 23,667 | 1.50x | **W** | 37,086 | 0.96x | TIE | N/S | — | N/S | 2/4 |
| R1.stat_storm | stat_storm (files/s) | 377,460 | 109,462 | 3.45x | **W** | 92,746 | 4.07x | **W** | 186,435 | 2.02x | **W** | 22,512 | 16.77x | **W** | 1/5 |
| R1.del_storm | del_storm (files/s) | 37,160 | 3,704 | 10.03x | **W** | 12,264 | 3.03x | **W** | 79,738 | 0.47x | **L** | 5,554 | 6.69x | **W** | 2/5 |
| R2.seq_write_1m@durable | seq_write_1m (MiB/s) | 1,935 | 2,322 | 0.83x | **L** | 1,276 | 1.52x | **W** | 644 | 3.00x | **W** | 446 | 4.34x | **W** | 2/5 |
| R2.seq_read_1m | seq_read_1m (MiB/s) | 6,481 | 6,560 | 0.99x | TIE | 4,625 | 1.40x | **W** | 3,025 | 2.14x | **W** | 3,076 | 2.11x | **W** | 2/5 |
| R2.rand_read_4k | rand_read_4k (IOPS) | 348,136 | 83,782 | 4.16x | **W** | 87,747 | 3.97x | **W** | 1,671 | 208.34x | **W** | 1,124 | 309.73x | **W** | 1/5 |
| R2.rand_write_4k@durable | rand_write_4k (IOPS) | 30,433 | 3,085 | 9.86x | **W** | 18,243 | 1.67x | **W** | 276 | 110.26x | **W** | N/S | — | N/S | 1/4 |
| R2.stat_storm | stat_storm (files/s) | 353,355 | 112,153 | 3.15x | **W** | 93,654 | 3.77x | **W** | 226,239 | 1.56x | **W** | 12,673 | 27.88x | **W** | 1/5 |
| R2.del_storm | del_storm (files/s) | 36,870 | 3,391 | 10.87x | **W** | 9,459 | 3.90x | **W** | 86,037 | 0.43x | **L** | 2,165 | 17.03x | **W** | 2/5 |
| R3.seq_write_1m@durable | seq_write_1m (MiB/s) | 4,445 | 3,507 | 1.27x | **W** | 1,295 | 3.43x | **W** | 1,280 | 3.47x | **W** | 1,533 | 2.90x | **W** | 1/5 |
| R3.seq_read_1m | seq_read_1m (MiB/s) | 6,284 | 6,258 | 1.00x | TIE | 1,059 | 5.93x | **W** | 3,047 | 2.06x | **W** | 1,238 | 5.08x | **W** | 1/5 |
| R3.rand_read_4k | rand_read_4k (IOPS) | 313,442 | 130,180 | 2.41x | **W** | 63,160 | 4.96x | **W** | 8,758 | 35.79x | **W** | 1,883 | 166.46x | **W** | 1/5 |
| R3.rand_write_4k@durable | rand_write_4k (IOPS) | 44,241 | 3,309 | 13.37x | **W** | 21,328 | 2.07x | **W** | 35,796 | 1.24x | **W** | N/S | — | N/S | 1/4 |
| R3.stat_storm | stat_storm (files/s) | 373,572 | 112,827 | 3.31x | **W** | 90,610 | 4.12x | **W** | n/a | — | ref-n/a | 12,260 | 30.47x | **W** | 1/4 |
| R3.del_storm | del_storm (files/s) | 41,443 | 3,461 | 11.97x | **W** | 15,806 | 2.62x | **W** | n/a | — | ref-n/a | 4,304 | 9.63x | **W** | 1/4 |

## Relaxed write rows (native ACK semantics — labeled, non-gating)

| Row | Workload (unit) | SQZ | JFS | SQZ/jfs | SWFS | SQZ/swfs | GEE | SQZ/gee | MPS3 | SQZ/mps3 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| R1.seq_write_1m@relaxed | seq_write_1m (MiB/s) | 4,440 | 3,785 | 1.17x | 2,474 | 1.79x | 2,105 | 2.11x | 1,236 | 3.59x |
| R1.rand_write_4k@relaxed | rand_write_4k (IOPS) | 35,967 | 3,490 | 10.31x | 24,353 | 1.48x | 72,148 | 0.50x | N/S | — |
| R2.seq_write_1m@relaxed | seq_write_1m (MiB/s) | 4,776 | 2,370 | 2.02x | 1,287 | 3.71x | 3,253 | 1.47x | 446 | 10.71x |
| R2.rand_write_4k@relaxed | rand_write_4k (IOPS) | 30,540 | 3,137 | 9.74x | 18,348 | 1.66x | 485 | 62.97x | N/S | — |
| R3.seq_write_1m@relaxed | seq_write_1m (MiB/s) | 4,714 | 3,609 | 1.31x | 1,306 | 3.61x | 2,601 | 1.81x | 1,534 | 3.07x |
| R3.rand_write_4k@relaxed | rand_write_4k (IOPS) | 44,834 | 3,376 | 13.28x | 21,596 | 2.08x | 74,816 | 0.60x | N/S | — |

## Top-3 adjudication (per row-family, primary table)

| Workload family | Rows | Worst rank | Top-3? |
|---|---|:--:|:--:|
| del_storm | R1.del_storm:2/5, R2.del_storm:2/5, R3.del_storm:1/4 | 2/5 | YES |
| rand_read_4k | R1.rand_read_4k:1/5, R2.rand_read_4k:1/5, R3.rand_read_4k:1/5 | 1/5 | YES |
| rand_write_4k@durable | R1.rand_write_4k:2/4, R2.rand_write_4k:1/4, R3.rand_write_4k:1/4 | 2/4 | YES |
| seq_read_1m | R1.seq_read_1m:1/5, R2.seq_read_1m:2/5, R3.seq_read_1m:1/5 | 2/5 | YES |
| seq_write_1m@durable | R1.seq_write_1m:1/5, R2.seq_write_1m:2/5, R3.seq_write_1m:1/5 | 2/5 | YES |
| stat_storm | R1.stat_storm:1/5, R2.stat_storm:1/5, R3.stat_storm:1/4 | 1/5 | YES |

**Overall: SqueezeFS is TOP-3 OR BETTER on every adjudicated row-family.**

Reading the ranks: SqueezeFS is **rank 1 of the field on 13 of 18 primary
rows** and rank 2 on the other five; no row-family drops below rank 2. The
run itself (18 primary rows × 4 references = 66 comparable cells after N/S
and ref-n/a) scored **59 W · 2 TIE · 3 L · 2 ref-n/a**, gate verdict below.

## RW6 outcome — the old allowlist is retired; one new attributed row

The two standing `R1/R3.seq_write_1m` allowlist entries (JuiceFS's
page-cache-ACK artifact, tracked since the 2026-07-15 baseline) are **gone as
predicted**: at matched durability SqueezeFS wins those rows outright
(R1 1.18×, R3 1.27× vs JuiceFS; 1.81–3.52× vs the rest). geesefs's relaxed
write rows show exactly the inflation the labels exist for (R1 rand-write
72,148 relaxed → 37,086 durable; R3 74,816 → 35,796) — durable mode is where
its cells became comparable, as designed.

## Loss adjudication (3 rows, all attributed; gate posture below)

### Loss 1 — `R2.seq_write_1m.jfs@durable` (0.83×): durable timing surfaces a real R2 write-path finding (substrate-conditional)

The relaxed R2 row is a SqueezeFS **W 2.02×** (4,776 vs 2,370 MiB/s). The
durable row inverts it (1,935 vs 2,322) because the durability pass costs
SqueezeFS 5.04 s of substrate `syncfs` while JuiceFS pays 0.15 s — its 2 G R2
cage already forced its objstore drain inline. Device evidence pins the
mechanism honestly: during the sqz row the device wrote **1,956 MiB/s across
the whole 8.6 s window** with the daemon at **1.8 cores — versus 4,291/4,413
MiB/s at 3.7/3.9 cores for the identical R1/R3 rows** (whose durability passes
cost 0.07–0.21 s, i.e. nothing was left dirty). The `-o direct_device_true`
mount (the only R2 delta on the sqz side) halves the write-phase drain rate on
file-backed volumes, leaving ~9.7 GiB in the substrate page cache for the
pass to drain. Two honest components, both chartered: (a) product — why does
`direct_device_true` throttle the *write* drain (read-path flag by contract);
(b) posture — file-backed `.img` volumes buffer block writes in the substrate
page cache at all (raw-namespace production mounts drive O_DIRECT io_uring
straight to the device, where this accounting class cannot arise; see
`tests/dev_substrate.sh`'s 165× barrier-bracket warning about file-backed
btrfs measurement generally). Allowlisted as `R2.seq_write_1m.jfs` with this
attribution; delete the entry when either component lands.

### Loss 2+3 — `R1/R2.del_storm.gee` (0.47×/0.43×): geesefs async-ACK deletes — the del-family twin of the artifact RW6 just retired for writes

geesefs "deletes" 131,072 files at 79,738/86,037 files/s — 1.7 s wall for
131 k S3 DeleteObjects through a localhost store, i.e. **unlinks acked from
RAM** (geesefs's documented posture: modifications are asynchronous by
design; only fsync forces persistence). Device evidence during the rows shows
the store's delete churn (~390–450 MiB/s of metadata writes) still trailing
when the clock stops. SqueezeFS deletes are real destroys through the commit
conveyor (journal-durable). This is an ACK-semantics measurement artifact,
not a product loss — the exact class RW6 closed for the write family, one
family over; del rows carry no durable mode in this harness revision.
**Corroborating evidence from the same run**: in R3 (cold protocol = create
tree → unmount → remount → timed row) geesefs's *own* stat/del rows failed
with `No such file or directory` on ~0.4 % of the tree — acked creates that
never became objects were lost across its own remount (`ref-n/a` cells, rows
recorded rc=1). Allowlisted as `R1.del_storm.gee,R2.del_storm.gee` with this
attribution; the chartered follow-up is a durable-del harness mode
(fsync/syncfs-inclusive delete timing, the RW6 pattern applied to the del
family).

## Gate posture for releases

```bash
SQUEEZEFS_SB_ALLOW_LOSS="R1.del_storm.gee,R2.del_storm.gee,R2.seq_write_1m.jfs" \
  tests/run_scoreboard.sh
```

Verified on this run's rows (re-score of `rawrows.tsv` with the allowlist):
**GATE GREEN — no unattributed loss rows**. Every W/TIE row and the N/S
ledger are hard gate; each allowlisted row must be deleted the day its
follow-up lands. The historical `R1.seq_write_1m,R3.seq_write_1m` allowlist
is retired (rows now W at matched durability).

## Honest anomalies & residuals

1. **The gate did its job on first contact**: the raw run exits 1 on the
   three rows above; the allowlist above is the *adjudicated* posture, not a
   default. Anyone re-running bare `tests/run_scoreboard.sh` will see the
   same three rows until the follow-ups land.
2. **R3 `gee` stat/del = ref-n/a** (geesefs lost ~0.4 % of acked creates
   across its own remount — evidence under Loss 2+3). Not a SqueezeFS gate
   event; recorded as the reference's own behavior with rc=1 rows kept.
3. **`R2.stat_storm.swfs` carries `DIRTY(remounted-dead-daemon)`** — the
   weed mount died once mid-regime (its server stayed up) and the harness's
   one-flagged-remount rule kept the grid filling. Value recorded with the
   flag; treat that single cell as lower-confidence.
4. **`del_storm.mps3` rows are rc=1** (value recorded + flagged): mount-s3
   deletes all 131 k files (the timed RMFILES phase completes) but the
   follow-up `-D` rmdir pass hits `Directory not empty` — S3-consistency
   listing lag on freshly-emptied prefixes. The files/s numbers stand;
   flags preserved.
5. **mps3 rand-read is S3-GET-latency-bound** (1.1–1.9 k IOPS): each 4 KiB
   read is a ranged GET against RustFS; mountpoint ships no data cache by
   default and its documented `--cache` mode (R1) caches whole parts. Honest
   defaults posture, recorded — not tuned around.
6. **gee R2 rand-read at 208× device-read amplification** (1,671 IOPS,
   193 GiB device reads for 197 MiB of user reads): geesefs's 5 MiB default
   readahead against a reclaim-caged store. Their design trade, verified
   device-true, recorded as-is.
7. **JuiceFS meta engine is sqlite3 on this box** (no redis-server binary;
   the user's redis lives in containers the harness must not touch). Same
   posture as the 2026-07-15 baseline; JuiceFS's stat/del/rand-write cells
   may improve on a redis box — re-run there before citing those margins
   externally.
8. **Substrate compression**: `/var/tmp` is btrfs `compress=zstd:1`; every
   system's durable stores share it (matched), and elbencho's fill pattern
   is identical for all five systems. Absolute MiB/s numbers are
   substrate-conditional; ratios are the claim surface.
9. **This box's rand-write ceiling differs from the devsub records**: sqz
   rand-write 30–44 k IOPS here (file-backed btrfs volumes) vs 59–67 k on
   the RW-program's zram namespaces — substrate class, not regression
   (`.benchmarks/2026-07-17-rand-write-program-closing.md` measured on
   devsub).

## Reproduction

```bash
tests/run_scoreboard.sh                       # full grid (~85 min on this box)
tests/run_scoreboard.sh teardown              # owned daemons + stores to zero
SQUEEZEFS_SB_SMOKE=1 tests/run_scoreboard.sh  # ~11 min micro-grid plumbing proof
# release-gate posture (adjudicated allowlist above):
SQUEEZEFS_SB_ALLOW_LOSS="R1.del_storm.gee,R2.del_storm.gee,R2.seq_write_1m.jfs" \
  tests/run_scoreboard.sh
```

Artifacts for this run: `/var/tmp/squeezefs_scoreboard/artifacts/20260718T190017Z/`
(per-row elbencho stdout+CSV, durability-pass splits, `.stats`/metrics
snapshots, diskstats deltas, honesty lines, `capabilities.tsv`,
`scoreboard.md`/`.tsv`). Multi-run discipline note: two earlier full-run
starts were aborted by harness defects found mid-run (R2 store-cage OOM +
unbounded durability pass; both fixed in `fa892f1`) — their partial grids
were discarded as rate-gathering, and this report's numbers come exclusively
from the clean end-to-end run of the fixed harness (single run, n=1 per row
per the standing scoreboard discipline).

## Kernel note — JuiceFS release binaries vs this kernel (named residual)

JuiceFS **release** binaries (1.4.0 AND 1.3.0, user or root, caged or bare)
wedge at mount time on this box: their `ensureFuseDev` probes `/dev/fuse` with
`os.Open` and the leaked fd drives a Go-netpoller `epoll_ctl` that this
kernel (`7.1.3-2-cachyos`, FUSE-over-io_uring patched) parks in
**uninterruptible D-state** — the mount never becomes ready and the process
survives `kill -9`. Upstream fixed it on main as `ca2aef0` ("replace open()
with stat()", juicedata/juicefs#7252), released after 1.4.0. The scoreboard's
jfs pin is therefore **v1.4.0 + that one-line cherry-pick, source-built**
(binary sha256 `c20be65f…`, patch recorded alongside as
`juicefs-1.4.0-p1/ca2aef0.patch`); the harness falls back to the release
tarball on unaffected kernels. Residual: **4 wedged juicefs processes**
(2 from the user's own pre-session `run_vs_juicefs.sh` attempt this morning,
2 from this session's bisect) are kernel-stuck in `epoll_ctl` and cannot be
reaped without a reboot — PIDs 1428555/1428575/2050894/2050918, zero mounts
held, zero CPU. Everything else this session spawned was torn down to zero
residue.
