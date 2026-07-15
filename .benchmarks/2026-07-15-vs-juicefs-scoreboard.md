# vs-JuiceFS scoreboard — inaugural baseline (2026-07-15)

**Charter**: USER GOAL (verbatim): *"My goal is to be faster than juicefs @
bare minimum. we kill it on write throughput. we need to kill it everywhere."*
This report is the **inaugural run** of the standing scoreboard harness
(`tests/run_vs_juicefs.sh`, landed this session): a re-runnable,
matched-conditions A/B that turns "faster than JuiceFS everywhere" into a
release gate. Protocol per the 2026-07-15 session (normative): matched
substrate, matched cache budgets, three regimes × the 6-shape workload grid,
win/loss table, **any loss fails the gate**.

## Provenance

| | |
|---|---|
| Harness | `tests/run_vs_juicefs.sh` @ `6bff615` (branch `test/vs-juicefs-scoreboard` off `abfde00`), defaults: `SQUEEZEFS_VS_CACHE_MB=4096`, dataset 16 GiB (16×1 GiB), tree 131,072×4 KiB files, timelimit 30 s, cages 16 G (R2 JuiceFS 2 G) |
| SqueezeFS | dev-lineage binary @ `6bff615` (md5 `7a59b3d0…`; identical product code to `abfde00` — the branch adds only the harness), 4 sqmeta + 4 sqdata file-backed volumes, `--mem-budget 4096M --disk-cache-size 4096MB`, staging declared at format |
| JuiceFS | `1.5.0-dev+2026-07-14.292f44cd` (the survey clone's lineage, the box's own binary), meta **sqlite3** (no redis-server on this box — harness auto-detects redis when present), `--storage file` objstore, `--cache-dir` on the substrate, `--cache-size 4096 --buffer-size 4096` (R1/R3), `--trash-days 0` (matched delete semantics) |
| Drivers | elbencho `3.1-9`, identical invocations both systems, incl. the user's exact line `-r --rand -t 16 -b 4k --iodepth 16 --direct` |
| Box | the phase-1 box: AMD RYZEN AI MAX+ PRO 395, 25 online CPUs, 109 GiB RAM, PC SN8000S 2TB NVMe, btrfs, kernel `7.1.3-2-cachyos` |
| Substrate | `/var/tmp/squeezefs_vs_juicefs` (btrfs on nvme0n1p2) — **both** systems' durable stores, caches, and meta in the same directory |
| Rails | `taskset -c 0-15` daemons + drivers; `systemd-run --user --scope` memcg cages; house 3-poll quiet gate per row (co-tenants + Tctl) + session-start load gate; per-row honesty lines (all 36 rows `quiet`, Tctl 55–65 °C); kills by PID; `/mnt/squeezefs` + `~/tmp/nvme` untouched |
| Artifacts | `/var/tmp/squeezefs_vs_juicefs/artifacts/20260715T225322Z/` (25 MiB: per-row elbencho outputs, `.stats` before/after snapshots, `/proc/<pid>/{io,stat}` deltas, diskstats evidence, honesty lines, mount logs) |

**Budget-mapping honesty**: one knob (`SQUEEZEFS_VS_CACHE_MB=4096`) drives
SqueezeFS's single `--mem-budget` (tiers + node cache + transport arenas, one
authority) and both JuiceFS knobs (`--buffer-size` RAM + `--cache-size` disk).
JuiceFS's meta engine and cache-index RAM are unbudgeted by their knobs, and
their `file://` objstore writes ride the **kernel page cache** (bounded only
by the cage) — where the mapping errs, it errs in JuiceFS's favor.

## The inaugural scoreboard (18 rows: 8 W · 3 TIE · 5 L · 2 INVALID — GATE: FAIL)

Verdict rule: W if SQZ > 1.05× JFS, L if < 0.95×, TIE inside ±5%. INVALID =
missing/unverified SqueezeFS row (counts as loss for the gate).

| Row | Workload | JFS | SQZ | SQZ/JFS | Verdict |
|---|---|---:|---:|---:|:--:|
| R1.seq_write_1m | seq write 1 MiB (MiB/s) | 11,541 | 4,431 | 0.38× | **L** — see Loss 1 |
| R1.seq_read_1m | seq read 1 MiB (MiB/s) | 6,565 | 6,634 | 1.01× | TIE |
| R1.rand_read_4k | rand read 4k (IOPS) | 106,068 | 219,740 | **2.07×** | **W** |
| R1.rand_write_4k | rand write 4k (IOPS) | 4,451 | 354 | 0.08× | **L** — see Loss 2 |
| R1.stat_storm | stat storm (files/s) | 116,408 | 332,309 | **2.85×** | **W** |
| R1.del_storm | del storm (files/s) | 3,357 | 34,606 | **10.31×** | **W** |
| R2.seq_write_1m | seq write 1 MiB (MiB/s) | 2,655 | 4,388 | **1.65×** | **W** |
| R2.seq_read_1m | seq read 1 MiB (MiB/s) | 6,591 | 6,594 | 1.00× | TIE |
| R2.rand_read_4k | rand read 4k (IOPS) | 83,603 | 237,401 | **2.84×** | **W** |
| R2.rand_write_4k | rand write 4k (IOPS) | 4,571 | 397 | 0.09× | **L** — see Loss 2 |
| R2.stat_storm | stat storm (files/s) | 117,628 | 317,505 | **2.70×** | **W** |
| R2.del_storm | del storm (files/s) | 3,696 | 34,431 | **9.32×** | **W** |
| R3.seq_write_1m | seq write 1 MiB (MiB/s) | 11,264 | 4,712 | 0.42× | **L** — see Loss 1 |
| R3.seq_read_1m | seq read 1 MiB (MiB/s) | 6,356 | 6,529 | 1.03× | TIE |
| R3.rand_read_4k | rand read 4k (IOPS) | 125,200 | 292,960 | **2.34×** | **W** |
| R3.rand_write_4k | rand write 4k (IOPS) | 4,423 | 378 | 0.09× | **L** — see Loss 2 |
| R3.stat_storm | stat storm (files/s) | 120,087 | NA | — | **INVALID** — see Loss 3 |
| R3.del_storm | del storm (files/s) | 3,324 | NA | — | **INVALID** — see Loss 3 |

Regimes: **R1** as-deployed (all cache layers live, 4 G budgets, 16 G dataset
= 4× cache) · **R2** device-true (JuiceFS `--cache-size 0` + 2 G cage
[decomposition-report mechanism], SqueezeFS `-o direct_device_true`; **both
verified by counters per row**) · **R3** cold-cache (page-cache drop + JFS
cache wipe + remounts, first pass).

**Where we already kill it**: every metadata row (stat 2.7–2.9×, del
9.3–10.3× — their delete path pays per-object objstore removes), every
rand-read row (2.1–2.8× — and the R2 cell is the purpose-built one:
**237k IOPS at 1.00× read amplification vs their 83.6k at 2.6× GET / 11.7×
device-byte amplification** — the decomposition report measured this exact
11.7× on their chunk model; counter proof `get_objΔ=4,194,320 ≈
device_true_readsΔ=4,194,304 = ops`), and **durable** seq write (R2 1.65×).
Seq read is a dead-heat TIE in all three regimes — both systems saturate the
same substrate class (~6.5 GiB/s).

## Loss rows — attribution + follow-ups

### Loss 1 — R1/R3 `seq_write_1m` (0.38× / 0.42×): page-cache-ACK vs durable write-through, NOT a device-throughput deficit

The device evidence (diskstats over each row) inverts the elbencho numbers:

| Row | elbencho MiB/s | actual device MiB/s during row |
|---|---:|---:|
| R1 jfs | 11,541 | **2,640** (0.23× of claimed) |
| R1 sqz | 4,431 | **4,453** (1.00× — device-true) |
| R3 jfs | 11,264 | 2,427 |
| R3 sqz | 4,712 | 4,728 (1.00×) |

JuiceFS's `file://` objstore writes ride the **kernel page cache** — elbencho
gets ACKs at RAM speed and the OS drains ~4.4× slower after the phase ends;
their two budget knobs cannot bound this (only the cage can). SqueezeFS
write-through puts **1.7–1.9× more bytes on the device per second** in every
regime. The R2 cage forces JuiceFS's dirty pages to drain inline and the same
driver flips the verdict: **SQZ W 1.65×** — that is the durable-throughput
truth, consistent with the user's "we kill it on write throughput."
**Follow-up (harness, P2)**: add an opt-in durability-leveled seq-write mode
(`--sync`-inclusive timing: value = bytes / (write+sync elapsed)) so R1/R3
seq-write cells compare acked-durable numbers instead of ACK semantics; until
then these two cells are expected-loss under
`SQUEEZEFS_VS_ALLOW_LOSS=R1.seq_write_1m,R3.seq_write_1m` with this paragraph
as the standing attribution.

### Loss 2 — `rand_write_4k` all regimes (0.08–0.09×): the real product gap — whole-block RMW on small random O_DIRECT writes

SqueezeFS: 354–397 IOPS, flat across R1/R2/R3 — the house-known
device-latency-bound number (L1 report 6-pass table: 346–362 ops/s, "equal,
device-latency-bound"). Device evidence pins the mechanism: **~2,478–2,859
device reads/s at 1.2–1.3 GiB/s + 2.5 GiB/s device writes for ~1.4–1.6 MiB/s
of user bytes** — every 4 KiB random write pays a 4 MiB-class block
read-modify-write plus durable write-through, ~600× write amplification at
this shape. JuiceFS: 4.4–4.6k IOPS via log-structured slice appends (no RMW;
compaction deferred; page-cache-cushioned — their device evidence shows
~1.5–1.9 GiB/s writes + reads for ~18 MiB/s of user bytes, so their
amplification is ~100×, better but not honest-free either). This is a
**genuine loss family**, not a semantics artifact.
**Follow-up (product, P1)**: the small-random-overwrite charter —
`.benchmarks/2026-07-13-overwrite-lazy-rmw-seed.md` is the existing seed
(lazy RMW / partial-block overwrite path); FIND-L1-A's >12-writer convoy
charter is adjacent. Expected-loss rows until it lands:
`SQUEEZEFS_VS_ALLOW_LOSS=R1.rand_write_4k,R2.rand_write_4k,R3.rand_write_4k`.

### Loss 3 — R3 `stat_storm`/`del_storm` INVALID: teardown-SIGBUS class crashed the pre-row unmount; post-crash remount lost the un-drained tail of acked creates (FIND-VS-A)

The R3 cold protocol (create tree → unmount → drop → remount → timed row) hit
the **documented pre-existing teardown-SIGBUS class** (L1 report: "every
daemon that ran the bench suite then unmounted dumped SIGBUS … needs its own
charter"; reproduced there on baseline `c56ec6a`): the unmount's drain died
(8 SIGBUS coredumps across the session, e.g. `coredumpctl` 19:11:54 /
19:12:20 / 19:12:49, up to 948 MB), the harness lazy-detached the ENOTCONN
mountpoint and remounted (by design — the grid kept filling), and the timed
rows then took ENOENT on ~3–4 % of the tree (elbencho rc=1 ⇒ INVALID).
**New evidence this run adds to that charter**: journal replay on the
post-crash remount was clean (`meta_kv_replay_dropped_torn=[0,0,0,0]`,
replay 4,324+488+21+3,209 entries in ≤53 ms) — the missing files are **not
torn writes**; they are the **un-drained tail of acked creates** (missing
paths cluster in `d7`, every thread's last-created directory: creates acked
to elbencho whose journal entries never reached the device because the
SIGBUS killed the daemon mid-drain, upstream of the final cadence flush).
Un-fsynced creates carry no POSIX durability promise, but ~4k files ≫ one
50 ms cadence window (~1.5k at the observed create rate) — the crash lands
**before** the final flush completes, so the blast radius exceeds the
deferred-durability contract's intent. **Follow-up (product, P1)**: the
teardown-SIGBUS charter the L1 report already demanded, now carrying this
acked-create-loss evidence (drain-then-detach ordering + the mmap-region
teardown class). These two rows are NOT allow-listed: they must go green when
the SIGBUS charter lands — that is the gate doing its job.

## FIND-VS-B — standing dev cargo-gate failure surfaced by this branch's merge gate (NOT a scoreboard row)

Running the required cargo gate for this (Rust-free) branch surfaced
`staged_rmw_storm_is_pool_backed_recycling_and_byte_exact`
(`tests/staged_rmw_alloc_tests.rs:167`) failing **deterministically** —
`staged_rmw_pooled_seeds` Δ = **100 of 200 ops** (exactly half the storm's
RMW seeds took the ring-miss leg instead of the pooled ring-hit leg; ×4
repeats, with and without `--all-features`). **Reproduced bit-for-bit on a
pristine `abfde00` worktree** — a dev-baseline deviation from the pooled-seed
contract (`0ca1e9a`/`e4a8434` lineage), pre-dating this branch (which
contains zero Rust). Every other suite green under `--no-fail-fast`
(single-failure inventory), bench smoke green, clippy/fmt/doc green.
Environmental note for the re-bisect: the failure emerged in a session whose
shell ran with `ulimit -n 2048` initially (fd-starved runs fail earlier in
`data_path_correctness` with EMFILE — raise to ≥ 65536 before gating; the
staged-RMW failure persists either way). Filed like the L1 report's
generic/074 row: **standing dev regression to re-bisect**, treatment
precedent "identical failure on baseline ⇒ deviation pre-dates the branch."

## Gate posture for releases (until the follow-ups land)

```bash
SQUEEZEFS_VS_ALLOW_LOSS="R1.seq_write_1m,R3.seq_write_1m,R1.rand_write_4k,R2.rand_write_4k,R3.rand_write_4k" \
  tests/run_vs_juicefs.sh
```

— i.e. the five attributed losses above are tracked known-losses; the two R3
INVALID rows and every current W/TIE row are **hard gate**: any regression
of a W row, any new loss, or a recurrence of the R3 INVALIDs fails the
release. Each allow-listed row must be deleted from the list the day its
follow-up lands (rand-write rows with the RMW charter; seq-write rows with
either the durability-leveled harness mode or a product write-path win).

## Reproduction

```bash
tests/run_vs_juicefs.sh                       # full scoreboard (~20 min on this box)
SQUEEZEFS_VS_SMOKE=1 tests/run_vs_juicefs.sh  # ~4 min micro-grid plumbing proof (per-commit tier)
```

Knobs: `SQUEEZEFS_VS_{SUBSTRATE_DIR,CACHE_MB,DATASET_GB,REGIMES,WORKLOADS,
ALLOW_LOSS,SMOKE,KEEP,…}` — full list in the script header. Output:
`scoreboard.md` + `scoreboard.tsv` + per-row raw evidence under
`<substrate>/artifacts/<timestamp>/`. JuiceFS meta engine is auto-detected
(redis preferred, sqlite3 fallback — this box: sqlite3) and pinned in the
provenance line; re-run on a redis box before citing metadata rows
externally (sqlite3 is JuiceFS's weakest meta engine; our stat/del margins
may narrow against redis).
