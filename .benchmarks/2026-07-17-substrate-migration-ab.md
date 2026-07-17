# Substrate migration A/B: file-backed-on-btrfs vs the virtual NVMe dev substrate (2026-07-17)

**Purpose**: the measured, current before/after table that QUICKSTART §2's
migration guidance owes — same binary, same shapes, same mount knobs
(defaults), only the substrate differs. These are **DOCS numbers
illustrating substrate distortion**, not product gates: neither side was
tuned, and rows where the file-backed side is *faster* are reported honestly
with attribution. No cross-system comparisons here (the vs-JuiceFS
scoreboard is a separate harness).

## Verdict up front

- **Barrier-bound metadata work is where file-backed-on-btrfs lies.**
  Strict-cadence (`SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`) small-file create is
  **10.4× faster on the substrate** (677 → 7,058 files/s) and delete
  **7.4×** (3,303 → 24,606 ops/s). Even at the *default* cadence, fsync-per-
  file create is **3.3×** (2,550 → 8,295 files/s).
- **The within-side strict/default collapse reproduces the md-baseline
  bracket**: on btrfs files strict create is 27 % of default (3.8× collapse)
  and strict delete 12 % (8.3×) — inside the 3.8–8.9× strict-cadence
  distortion documented in
  `.benchmarks/2026-07-14-metadata-throughput-baseline.md`. On the virtual
  substrate the same pair reads **85 %** (create) — the OQ-5 convergence
  constant from `.benchmarks/2026-07-15-m7-conveyor.md`, reproduced on a
  fresh box state.
- **Rand 4 KiB O_DIRECT**: write **5.0×** faster on the substrate — and, at
  least as important for anyone benchmarking, **stable** (B spread ±3 %; the
  btrfs-file side spans **5×** run-to-run, 4.8 k–24.4 k IOPS, from CoW
  extent churn under the W1 in-place patch DMA). Read 1.8× (hybrid posture).
- **Honest A-faster rows**: default-cadence delete is a wash (0.99×, ranges
  overlap — RAM-authoritative + amortized barriers ⇒ substrate barely
  matters), and **seq write 1 MiB is 0.86× (file-backed faster)**: A's
  writes land in the host page cache over btrfs onto a physical NVMe SSD,
  while B pays inline zstd compression in zram on the bench's
  incompressible fill. Stat is substrate-independent (1.02×/1.20×) — a
  control row showing the harness itself is not biased.

## Method

| | |
|---|---|
| Tree / binary | dev @ `3621fe78acc071033b3448a9c3404cf8faa3fdc9` (clean, == origin/dev); `cargo build --release` built `taskset -c 0-15 CARGO_BUILD_JOBS=12`; binary md5 `cbd21959121ecea3132d7fdc700480f5` (single copy `sqzab` used for format/mount/bench on both sides) |
| Box | AMD RYZEN AI MAX+ PRO 395, 32 hw threads all online (`0-31`), CPU capped 3.5 GHz (performance governor, verified), 109 GiB RAM, kernel `7.1.3-2-cachyos`, **fresh boot** (measurement started 23 min after boot), box otherwise idle — the sole-owner slot |
| Substrate A (before) | 4× 1 GiB meta + 4× 8 GiB data **sparse files** (sizes match the dev-substrate defaults) on the `/home` btrfs (`rw,noatime,compress=zstd:1,ssd,discard=async` on the system NVMe) — the classic file-backed sandbox, recreated zeroed before the run. Daemon stats: `meta_volume_atomicity_physical = "file-backed"`, `writer_guard_mode = "flock+claim"` |
| Substrate B (after) | `sudo tests/dev_substrate.sh recreate` at defaults: 4× 1 GiB memory-backed null_blk (mds, 256 MiB write-back cache, fua=1) + 4× 8 GiB zram-zstd (oss) exposed as real `/dev/nvmeXnY` via nvmet-loop; format/mount lines taken from the script's printed hint. Daemon stats: `meta_volume_atomicity_physical = "atomic4k"`, `writer_guard_mode = "flock+pr"` (NVMe PR enforcement-grade) |
| Format / mount | `squeezefs format "sqmeta://…" "sqdata://…"` (no `--disk-cache-paths` ⇒ cache-less: every data byte hits the volumes under test, no third staging-dir path to confound the A/B); mount **defaults only** (`mount <sqmeta-uri> <mnt> --daemon --log-file …`), FUSE-over-io_uring armed every session (32 queues, depth 25); daemons root (substrate namespaces require it) in harness-identical `systemd-run --scope -p MemoryMax=8G -p MemorySwapMax=0` cages; `udevadm settle` between format and mount (QUICKSTART scripted-flow note); bench runs as the presented (sudo-invoking) user; measurement on the **full 32-CPU mask** (only the build was pinned) |
| Cadence pairs | metadata rows alternate default → strict per iteration (mount default, run, umount, mount strict, run, umount, ×3) to cancel monotonic warmth drift; each daemon's cadence proven from `/proc/<pid>/environ` (logged per mount — strict mounts carry `SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`, default mounts carry no override) |
| Rows / statistic | n=3 per row per side, **median [min–max]**; every bench invocation passed a 3-consecutive-poll (15 s apart) quiet gate: load1 < 2.0, comm-exact pgrep empty for rustc/cargo/fsstress/fsx/fio/elbencho/dbench/check, Tctl < 80 °C (≥ 86 °C ⇒ 120 s cool-down + streak reset — never fired); post-row co-tenant re-check: **all 36 rows clean**, Tctl 44.2–67.9 °C across the session |
| Instrument | `squeezefs bench` (the built-in tokio bench; per the RW-program instrument-alignment lesson every measurement states its instrument — elbencho rows would differ in buffer alignment) |
| Harness honesty | attempt 1 aborted per the multi-run discipline: a harness bug (umount waited for the mountpoint but not the daemon *process*; the old daemon's late teardown externally unmounted the next session ⇒ deterministic seqr.r1 failure) and an order-confounded cadence sequence were fixed, and **the count restarted from zero** on fresh volumes; the aborted attempt's rows are archived (`attempt1_aborted/`) and not blended |
| Artifacts | `~/sqzab_migration_20260717/` — per-row bench logs, mount logs, `gate.log` (QUIET lines), `rows.log` (per-row Tctl pre/post + co-tenant flags), `cadence.log` (per-daemon env proof), `.stats` snapshots, `provenance.txt`, runner scripts (sandbox is removed after the note lands; logs preserved off-tree by the operator only if wanted — the note carries the load-bearing numbers) |

### Shapes (exact bench invocations, identical both sides)

1. **Metadata (mfcreate/stat/del)** — `bench <mnt> -t 8 -n 3125 -s 4k -b 4k -w --stat --del`
   = create **25,000 files of 4 KiB** across 8 per-thread dirs with the
   bench's fsync-per-file durable-create contract (4 KiB ≤ inline threshold
   ⇒ pure metadata-volume traffic), then stat ×25 k, then del ×25 k. Run at
   the default journal cadence and at strict (`SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`).
   Note the shape is *create+write+fsync*, not mdstorm's bare `create(2)` —
   don't cross-compare absolute rates with the md-baseline storm tables.
2. **Seq 1 MiB O_DIRECT (t4 × 1 GiB)** — `-t 4 -s 1g -b 1m -w --direct`
   (write; overwrites in place on repeat runs) and `-t 4 -s 1g -b 1m -r
   --direct` (read, **fresh mount per run** so SqueezeFS's own tiers are
   cold; host-level backing caches stay as the substrate provides them —
   that asymmetry is part of what the row measures).
3. **Rand 4 KiB O_DIRECT (30 s boxes)** — `-t 4 -s 1g -b 4k -r --rand
   --direct --time 30` (fresh mount per run) and `… -w --rand …` (the
   scoreboard-adjacent shapes; W1 sole-owner patch path on writes). Default
   mounts ⇒ **hybrid** O_DIRECT read posture (reads may serve from the read
   tiers once warm; both sides identical) — for device-true amplification
   rows mount `-o direct_device_true`, which this A/B deliberately does not.

## Results — before/after (median [min–max], n=3)

| row | A: file-backed btrfs (before) | B: virtual NVMe substrate (after) | after/before |
|---|---|---|---|
| mfcreate 25 k × 4 KiB (fsync/file), default cadence | 2,550 [2,546–2,601] files/s | 8,295 [7,682–8,339] files/s | **3.25×** |
| stat 25 k, default cadence | 141,162 [133,250–150,526] ops/s | 143,400 [136,882–145,118] ops/s | 1.02× |
| del 25 k, default cadence | 27,283 [26,951–29,295] ops/s | 27,105 [24,495–27,611] ops/s | 0.99× (A faster) |
| mfcreate 25 k × 4 KiB (fsync/file), **strict** cadence | 677 [673–700] files/s | 7,058 [7,042–7,105] files/s | **10.43×** |
| stat 25 k, strict cadence | 136,587 [134,662–158,662] ops/s | 163,856 [129,799–165,735] ops/s | 1.20× |
| del 25 k, **strict** cadence | 3,303 [3,118–3,320] ops/s | 24,606 [24,016–25,074] ops/s | **7.45×** |
| seq write 1 MiB O_DIRECT (t4 × 1 GiB) | 2,784 [2,777–2,792] MiB/s | 2,407 [2,386–3,138] MiB/s | 0.86× (A faster) |
| seq read 1 MiB O_DIRECT (t4 × 1 GiB, fresh mount) | 3,284 [3,265–3,311] MiB/s | 7,718 [7,606–7,728] MiB/s | **2.35×** |
| rand read 4 KiB O_DIRECT (30 s, hybrid posture) | 27,930 [27,494–28,402] IOPS | 50,077 [49,463–51,010] IOPS | 1.79× |
| rand write 4 KiB O_DIRECT (30 s) | 7,155 [4,812–24,431] IOPS | 35,971 [34,239–36,575] IOPS | **5.03×** |

### Distortion ratios, read the other way (what file-backed *hides*)

| axis | file-backed-on-btrfs understates the substrate number by |
|---|---|
| strict-cadence create / delete | **10.4× / 7.4×** |
| default-cadence fsync-per-file create | 3.3× |
| within-side strict:default collapse (A) | create 27 % (3.8×), del 12 % (8.3×) — reproduces the md-baseline 3.8–8.9× bracket |
| within-side strict:default (B) | create **85 %** — the OQ-5 ≥ 85 % convergence constant, reproduced |
| rand-4k write IOPS (and stability) | 5.0× — and A's spread is **5×** run-to-run (4.8 k–24.4 k) vs B ±3 % |
| cold seq read | 2.35× |

## Honest anomalies (attribution, one line each)

1. **seq write 0.86× — file-backed FASTER.** A's 1 MiB writes are absorbed
   by the host page cache over btrfs and drain to a *physical* NVMe SSD
   (file-granularity fsync amortizes the barrier over 1 GiB); B pays inline
   zstd compression in zram on the bench's pseudorandom (incompressible)
   fill. Page-cache effects on sparse files were the anticipated case —
   reported, not tuned away. (B's own max run, 3,138 MiB/s, overlaps A.)
2. **del at default cadence 0.99× — a wash.** Ranges overlap; delete at
   the default cadence is RAM-authoritative with amortized barriers, so the
   meta substrate barely shows. The same row at strict cadence is 7.45× —
   the barrier is the whole story.
3. **stat 1.02×/1.20× — substrate-independent by design** (latch-free
   RAM-authoritative reads); serves as the control row for the harness.
4. **rand-write variance on A** (4,812 / 7,155 / 24,431 IOPS): btrfs CoW
   extent churn under repeated 4 KiB in-place patch DMA into a sparse file —
   run 3 landed on a friendlier extent layout. The median is honest; the
   spread itself is a reason not to benchmark this path on files.

## Why the substrate also *qualifies* more than it speeds up

Same session, from the daemons' `.stats`: A runs `writer_guard_mode =
"flock+claim"` (no PR substrate ⇒ detection-grade cross-host guard) and
classifies `meta_volume_atomicity_physical = "file-backed"`; B runs
**`flock+pr`** (NVMe Persistent Reservations, enforcement-grade) and
**`atomic4k`** — i.e. the substrate exercises the full kernel NVMe
target/host stack and the single-writer guard's production posture, which
no file can.

## Reproduce

```bash
# before (functional sandbox class): 4x1GiB meta + 4x8GiB data sparse files on btrfs
# after:
sudo tests/dev_substrate.sh create      # prints the format/mount lines
# both sides: format cache-less, mount defaults, then e.g.
squeezefs bench <mnt> -t 8 -n 3125 -s 4k -b 4k -w --stat --del   # metadata rows
squeezefs bench <mnt> -t 4 -s 1g -b 1m -w --direct               # seq write
squeezefs bench <mnt> -t 4 -s 1g -b 4k -w --rand --direct --time 30  # rand write
```

Cross-references: `.benchmarks/2026-07-14-metadata-throughput-baseline.md`
(the substrate-bracket methodology + the 165× barrier primitive this note's
end-to-end rows sit on top of), `.benchmarks/2026-07-15-m7-conveyor.md`
(OQ-5 strict/default constant), `docs/design-metadata-throughput.md`,
QUICKSTART §2 (the substrate itself).
