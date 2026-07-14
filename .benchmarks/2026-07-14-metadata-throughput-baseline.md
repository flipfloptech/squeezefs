# Metadata-throughput baseline — substrate bracket, cadence curve, per-op attribution

Measurement + attribution + inventory baseline for the upcoming metadata design
program. No code changes on this branch — the deliverable is this report and
its ranked lever list.

## Provenance

| | |
|---|---|
| Tree | `docs/metadata-baseline` off dev @ `c0dab9c` |
| Date | 2026-07-13/14 |
| Binary | `cargo build --release` from this tree (release keeps `debug = true`) |
| Machine | AMD RYZEN AI MAX+ PRO 395, 32 hw threads — **24 online** (offline: 4,8,12,14,16,18,24,28), **3.5 GHz cap** (`scaling_max_freq`, performance governor, boost on — absolute numbers are capped-era; **ratios are the signal**) |
| Kernel | 7.1.3-2-cachyos |
| Rails | `taskset -c 0-15` (⇒ **12 online CPUs** in-mask), `CARGO_BUILD_JOBS=12`, daemons caged `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0`, Tctl < 80 °C gate at run start |
| Quiet gating | the fstests re-gate (`check -g auto`) + a residue-EIO agent + an LTP cycle owned the box this session; **every timed run** waited for comm-exact `pgrep -x` on check/rustc/cargo/fsstress/fsx/fio all empty AND 1-min load < 6 AND Tctl < 80 °C, **held for 3 consecutive polls 15 s apart** (the re-gate loop has short between-iteration lulls that fooled single checks; `-f` pattern matching false-positived on the poller itself — rails log) — plus a per-phase post-run quiet re-check that marks rows DIRTY if load returned mid-phase |
| Harness | `~/tmp/mdbase_20260714/` — `mdstorm.c` (8-thread barrier-start storm driver, K7 storm shape: thread w takes names `i = w, w+T, …` in one shared dir; plus many-dirs mode), `syncprobe.c`/`syncprobe2.c` (raw 4 KiB barrier floors: fdatasync, O_DIRECT RWF_DSYNC=FUA), the release `kv_scale_tests` binary for trait-path storms, per-phase `.stats` snapshots |
| Volumes | fresh `format --force` per run: 2 GiB meta on the substrate under test (⇒ 32 MiB journal ring by the default clamp), **data volume constant** (8 GiB file, btrfs `chattr +C`) + staging dir on the same nocow dir — data path held constant to isolate metadata |
| Storm counts | scale 100 % = 20 k mkdir / 100 k create / 100 k stat / 100 k rename / 100 k unlink / 100 k many-dirs create+unlink / 20 k rmdir; strict-cadence runs scaled down (20 %, CoW-file 10 %) — rows report **ops/s**, not totals |
| Statistic | bracket cells: median over 2–3 clean (quiet-flagged) runs; cadence/attribution cells: 1 clean run (wall-budget triage under the shared box — flagged in the tables) |

## The question being answered

The user's only option is file-backed meta volumes on `/home` — **btrfs, CoW,
`compress=zstd:1`** (recorded mount opts:
`rw,noatime,compress=zstd:1,ssd,discard=async,space_cache=v2,subvol=/@home`).
v3 commits = RAM apply + one checksummed journal-ring entry; durability =
`fdatasync` of the volume file on the flush cadence
(`SQUEEZEFS_META_FLUSH_INTERVAL_MS`, default 50 ms; `0` = coalesced fdatasync
per commit). So the substrate's **fdatasync cost** is the entire substrate
dependence of the commit path. The bracket: btrfs-CoW file → btrfs nocow file →
null_blk (real block device, real FLUSH/FUA, memory-backed) → nvmet-loop NVMe
(the full kernel NVMe target/host stack).

## Substrates

| ID | Meta volume | Notes |
|---|---|---|
| A `cow` | 2 GiB file, btrfs CoW+zstd (default `/home`) | what the user runs today |
| A2 `nocow` | 2 GiB file, btrfs `chattr +C` | the K7 reference method |
| B1 `nullb` | `/dev/mdb_fast` null_blk configfs: 3 GiB, `memory_backed=1`, `cache_size=1024` MiB, `completion_nsec=0`, `irqmode=0` | **fua=1, write_cache=write back** — real FLUSH semantics |
| B2 `nullb10us` | same + `completion_nsec=10000`, `irqmode=2` (timer) | ~10 µs device latency model |
| C `nvmet` | `/dev/nvme1n1` = kernel nvmet **loop** target (configfs) over a third null_blk, `nvme connect -t loop -i 4` | **fua=1, write back**, full NVMe stack |

FUA support measured: **yes on all three block substrates**
(`/sys/block/*/queue/fua` = 1, write_cache = "write back") — null_blk exposes
FUA when `cache_size` is set (module default `fua=1`), and nvmet loop
namespaces inherit write-back+FUA. An FUA-capable path is therefore testable
on this box without raw hardware — relevant to the FUA-instead-of-fdatasync
lever below.

Setup notes (repro): null_blk via configfs (`/sys/kernel/config/nullb/<name>`;
this kernel names the disk after the config item, e.g. `/dev/mdb_fast`);
nvmet-loop namespaces backed by null_blk need a `device_uuid` stamped before
enable (duplicate/absent NGUID ⇒ host connect fails) and `nvme connect -i 4`
(default per-CPU I/O queues hit `blk_mq_alloc_request_hctx` **EXDEV** with
this box's offlined CPUs). Teardown verified (`teardown_substrates.sh`):
disconnect → port unlink → ns disable/rmdir → subsystem/port rmdir → nullb
`power=0` + rmdir → `modprobe -r nvme_loop nvmet null_blk`.

## Raw substrate floor — 4 KiB pwrite + fdatasync (the journal's barrier primitive)

`syncprobe` (buffered 4 KiB pwrite @0 + `fdatasync`, 2000 iters, quiet-gated,
scratch files — never the volumes):

| Substrate | mean µs | p50 µs | p99 µs | vs B1 |
|---|---:|---:|---:|---:|
| A `cow` (btrfs CoW file) | 525.0 | 495.4 | 807.1 | **165×** |
| A2 `nocow` (btrfs +C file) | 503.9 | 465.4 | 679.9 | **155×** |
| B1 `nullb` | 3.2 | 3.0 | 5.4 | 1× |
| B2 `nullb10us` | 27.0 | 26.2 | 36.3 | 8.7× |
| C `nvmet` (loop NVMe stack) | 9.4 | 8.9 | 16.0 | 3.0× |

Two facts fall out immediately:

1. **`chattr +C` does NOT fix the fdatasync floor on btrfs** (465 vs 495 µs
   p50): nocow avoids extent CoW but `fdatasync` still runs the btrfs
   log-tree/transaction machinery. The K7 method's `+C` protected *data-path
   O_DIRECT* behavior, not the metadata barrier.
2. The **kernel NVMe fabric stack costs ~6 µs over raw null_blk**
   (8.9 vs 3.0 µs p50) — the loop target is a faithful "real NVMe stack"
   substrate at ~9 µs/barrier, i.e. **~50× cheaper than any file on the
   host's btrfs**.

A real consumer NVMe barrier (FLUSH on write-back cache) is typically
O(100 µs–1 ms class) — the bracket's B2 row (10 µs device latency model) and
the A rows bound it from both sides; none of the substrates here model a
spinning-rust-class FLUSH.

## Substrate bracket — FUSE metadata storms @ default cadence (50 ms)

ops/s, median of 2 runs (single values where a companion run was lost to the
box-sharing incidents in the rails log; every reported row ran quiet-gated and
carries a clean post-phase quiet flag unless noted):

| Phase (8 threads) | A cow | A2 nocow | B1 nullb | B2 nullb-10µs | C nvmet-loop | spread |
|---|---:|---:|---:|---:|---:|---:|
| mkdir ×20 k, one dir | 10 516 | 10 294 | 9 212 | 9 980 | 9 658 | ±7 % |
| **create ×100 k, one dir** | **6 490** | **5 868** | **5 812** | **5 812** | **5 873** | **±6 %** |
| stat ×100 k | 209 042 | 202 116 | 204 394 | 202 958 | 202 006 | ±2 % |
| rename ×100 k, same dir | 6 321 | 6 220¹ | 5 715 | 5 472 | 5 814 | ±7 % |
| unlink ×100 k | 4 816 | 4 989¹ | 4 170 | 4 774 | 4 782 | ±9 % |
| mfcreate ×100 k, 8 dirs | 25 548 | 27 763¹ | 28 348 | 28 200 | 27 488 | ±5 % |
| mfunlink ×100 k, 8 dirs | 20 760 | 21 365¹ | 21 060 | 21 264 | 20 996 | ±1 % |
| rmdir ×20 k | 8 750 | 9 231¹ | 8 928 | 8 686 | 8 430 | ±5 % |

Medians over all **clean** (quiet-flagged) runs — 2–3 per cell; ¹ = single
clean run (the companion was lost to a box-sharing incident; rails log).
DIRTY-flagged rows (collision with returning background load) are excluded;
the full per-run table with flags is in the session `results.tsv`.

**The bracket answer: at the default 50 ms deferred cadence, file-backed
volumes on btrfs are NOT lying about metadata throughput.** Across a substrate
bracket whose barrier primitive spans **165×** (495 µs → 3 µs), every FUSE
metadata storm lands within ±9 % — indistinguishable from run noise. This is
the design working as specified: the commit path is RAM apply + journal-ring
pwrite (via io_uring), and the fdatasync lives on the background cadence, so
substrate barrier cost never appears inline. Corroborated by the journal
counters: identical per-op journal shape on every substrate (below), and by
the strict-mode rows (next section) where the substrate suddenly matters
enormously.

Two structural signals *inside* the bracket, both substrate-independent:

- **Single-dir vs many-dirs: 3.9–4.9×.** mfcreate (per-thread private dirs)
  runs 25.5–28.3 k/s while the same 100 k creates into ONE shared directory
  run 5.8–6.5 k/s. Same fuse_ops/op, same journal/op — the delta is
  contention on the shared parent (per-inode FUSE op lock on the parent +
  same-leaf commit locking + dentry-chain probes on one hot leaf).
- **stat ≈ 200 k op/s at exactly 1.00 fuse_ops/op** — pure GETATTR round
  trips at ~5 µs/op through the armed over-uring transport (the kernel dcache
  absorbs the lookups). This is the practical transport ceiling for this box
  (lineage control: 294 k IOPS at 12-online-CPU cap era ~3.4 µs/op).

## Cadence curve — `SQUEEZEFS_META_FLUSH_INTERVAL_MS` ∈ {0, 50 (default), 1000}

create/rename/unlink ops/s (one dir, 8 threads; strict runs at reduced counts
— rates are the statistic):

| Substrate | op | **0 (strict)** | 50 (default) | 1000 |
|---|---|---:|---:|---:|
| A cow | create | **1 291** | 6 490 | — |
| A cow | rename | **709** | 6 321 | — |
| A cow | unlink | **679** | 4 816 | — |
| A2 nocow | create | **1 525** | 5 868 | 5 870 |
| A2 nocow | rename | **866** | 6 220 | 5 807 |
| A2 nocow | unlink | **837** | 4 989 | 4 911 |
| B1 nullb | create | **5 380** | 5 812 | 5 829 |
| B1 nullb | rename | **4 835** | 5 715 | 5 858 |
| B1 nullb | unlink | **3 863** | 4 170 | 4 662 |
| C nvmet | create | **5 329** | 5 873 | 5 769 |
| C nvmet | rename | **4 141** | 5 814 | 5 632 |
| C nvmet | unlink | **3 788** | 4 782 | 4 518 |

The curve is a step function, and the step is entirely at strict-0 on file
substrates (B1 numbers below are the clean medians from the bracket):

- **Default → 1000 ms: flat everywhere** (±2 %). There is nothing to win by
  lengthening the cadence — 50 ms already takes the barrier fully off the
  throughput path. (The cadence knob is a durability-window knob, not a
  throughput knob, from 50 ms upward.)
- **Strict-0 on real block devices barely costs anything**: create −7 %
  (B1) / −9 % (C), i.e. per-commit coalesced fdatasync at a 3–9 µs barrier
  is absorbed. **Strict-0 on btrfs files collapses 3.8–8.9×**: create
  6.5 k → 1.3 k/s (5.0×), rename 6.3 k → 0.7 k/s (8.9×) — barrier 465–495 µs. *This* is
  where file-backed testing lies: any experiment about strict-durability
  commit latency, group-commit design, or barrier batching run on a
  btrfs-file volume measures btrfs's fdatasync, not squeezefs.
- Barrier accounting (strict, A cow): rename = 2.004 journal entries/op
  (two `commit_tx` per rename — dentry move + inode mtime as separate txs)
  ⇒ ≈ 1 420 entry commits/s at 709 renames/s, each barriering (coalesced)
  before ack ⇒ **up to ~70 % of wall inside `fdatasync`**, effective group size ≈ 1 commit/barrier under 8 writers —
  the existing jbd2-style `SyncCoalescer` (`src/meta_backend/sync_coalescer.rs`)
  coalesces *concurrent* waiters but upstream per-leaf commit locking spreads
  arrivals so groups rarely form. Headroom for real group commit: ~8× at this
  concurrency on file substrates.

## Per-create cost decomposition (transport vs engine vs journal)

`.stats` deltas per op (identical across ALL substrates and cadences —
measured on B1 and A cow, r1 runs; journal shape is cadence-invariant by
design and the counters agree):

| op | fuse_ops/op | journal entries/op | journal B/op | DLM leases/op |
|---|---:|---:|---:|---:|
| create (open O_CREAT\|O_EXCL + close) | **5.18** | 1.006 | 194 | **0** |
| mkdir | 3.45 | 1.006 | 233 | 0 |
| rename | 5.01 | **2.002** | 203 | 0 |
| unlink | 5.70 | **2.020** | 301 | 0 |
| stat | 1.00 | 0 | 0 | 0 |

(fuse_ops is thread-batched ±128/thread ⇒ <0.3 % error at these counts.
`lease_acquire_*` stayed 0 through every storm: **cluster DLM leases are not
on the metadata path** — only per-inode/dentry in-process locks are.)

Same-substrate engine-only comparison (trait-path storm,
`kv_scale_tests::million_entry_directory_storm…`, TMPDIR on A2 nocow, default
cadence, 8 writers — same storm shape as mdstorm):

| Path | creates/s (100 k) | µs/create | creates/s (1 M) | unlinks/s |
|---|---:|---:|---:|---:|
| KV engine direct (trait) | **26 894 / 26 865** (2 runs) | 37 | **22 193** | 21.3–21.7 k (100 k) / 18.1 k (1 M) |
| Through FUSE (same substrate, same shape) | 5 868–6 490 | 154–170 | — | 4 816–4 989 |
| **FUSE multiple** | **4.4×** | +117–133 µs | — | ~4.3× |

The 1 M-dir trait row sits at the K7 lineage band edge (23.4–26.6 K/s at the
3.2–3.5 GHz-cap era — 22.2 K/s here at 3.5 GHz cap with a desktop-ambient
box), so the engine has not regressed; **the entire gap between "engine
23–27 k" and "mount ~6 k" is the FUSE layer**, and of the +117–133 µs/create
only ~18 µs is the raw transport floor (5.18 round trips × ~3.4 µs lineage
floor). The remaining ~100–115 µs/create is FUSE-layer software: handler glue,
per-op tokio task + timeout wrapping, parent-inode op locks, attr/entry cache
maintenance, and the FORGET sideband servicing that trails every storm.

## Flamegraph / syscall attribution (create storm, quiet window)

perf on the caged daemon (dwarf call graphs, 997 Hz, 6–12 s mid-storm
windows; `perf.data` artifacts under `~/tmp/mdbase_20260714/stats/attrib_*/`):

**A cow, default cadence, create storm** (equally true of B1 — profiles are
substrate-identical at default cadence):

| Share (self cycles) | What |
|---:|---|
| **26.3 %** | KV engine (`meta_backend::kv::*`) — dominated by `record::InodeDelta::decode` **7.9 %**, `NodeSnapshot::lookup` 3.8 %, `record::Reader::finish` 3.3 %, `NodeSnapshot::next_live` 2.3 %, `bset::MergeIter::run_end` 2.2 %, `RecordIndex::group_bounds` 1.4 % — i.e. **the per-lookup/create record-fold machinery, not the commit pipeline** (`commit_tx` self ≈ 0.3 %) |
| ~36 % | kernel — `epoll_wait` ~10 %, `io_uring_enter` ~9.5 % (over-uring commit+fetch and the uring fs worker), scheduler (dequeue/pick/psi) ~8 %, locking/fput/misc the rest |
| 3.5 % | vendored fuse3 over-uring code (`fuse3::*`) |
| 2.75 % | `__vdso_clock_gettime` (tokio timeout wrappers + metrics stamps ×5 ops/create) |
| ~2–3 % | jemalloc (`_rjem_malloc` 2.2 % + tcache) |
| 1.3 % | `arc_swap::Debt::pay_all` (snapshot publishes) |
| 1.28 % | `crossbeam_channel::Receiver::recv` on the `squeezefs-uring` worker thread (uring fs-worker handoff) |

**fdatasync/flush is invisible in BOTH default and strict profiles on
null_blk** (`blkdev_issue_flush` < 0.05 %) — and there are **zero fdatasync
syscalls** in strace because every barrier rides `crate::uring_fs::fdatasync`
(io_uring). Strict-mode wall time on file substrates is barrier-bound
(§cadence) but that cost is *off-CPU* (waiting on btrfs), so a CPU flamegraph
under-reports it — the cadence table and the journal-entry × barrier-latency
arithmetic above are the honest accounting.

Whole-daemon syscall profile during the create storm (sudo strace -c -f,
60 k creates, default cadence, A cow; strace itself slows the daemon ~3.2× —
counts are the signal, not the times):

| syscall | calls/create | % of strace'd time |
|---|---:|---:|
| `io_uring_enter` | **31** | 29.2 % |
| `epoll_wait` | 16.4 | 31.7 % |
| `futex` | 5.2 | 33.5 % |
| `write`+`read` (eventfd/pipe wakes) | 41 | 2.5 % |
| `sched_yield` | 4.4 | 0.2 % |
| `fdatasync`/`fsync` | **0** | 0 (all barriers via io_uring) |

**~31 `io_uring_enter` per create** (≈ 6 per FUSE op) is the transport-side
smoking gun: one submit per COMMIT_AND_FETCH plus eventfd-driven wake churn,
exactly the S2 lever from the read-path closing report (drain `commit_rx`
fully → one `ring.submit()` per drain), plus SQPOLL (S3) as the knob-only
variant.

## FUA-vs-fdatasync barrier probe (the FUA lever, measured)

`syncprobe2`: buffered pwrite+fdatasync vs O_DIRECT `pwritev2(RWF_DSYNC)`
(= REQ_FUA on fua-capable queues) vs O_DIRECT|O_DSYNC, 4 KiB @0, 2000 iters:

| Substrate | fsync p50 µs | **FUA p50 µs** | O_DSYNC p50 µs |
|---|---:|---:|---:|
| B1 nullb (fua=1, wb cache) | 3.8 | **3.4** (−11 %) | 3.9 |
| B2 nullb-10µs | 26.8 | 26.1 | 26.1 |
| C nvmet-loop | 9.5 | **7.6** (−20 %) | 7.6 |
| A2 nocow (btrfs file) | 417.5 | 556.5 (**+33 %**) | 552.6 |
| A cow (btrfs file) | 492.1 | 493.0 | 494.9 |

All three block substrates advertise `queue/fua=1` + write-back cache, so the
FUA path is exercisable on this box without raw hardware. Verdict: **real but
small here** (1–2 µs/barrier, −11/−20 %) because null_blk's FLUSH is nearly
free; on real NVMe with a populated write cache the FLUSH-vs-FUA gap is
typically much larger, and FUA also composes with group commit (one FUA
journal write replaces write+flush). On btrfs *files* the FUA form is
counterproductive (+33 %) — another way file-backed testing inverts a real
lever's sign.

## src/nvmeof.rs inventory + "SPDK mandatory for targets" scoping

**What target setup uses today — both paths exist, selected per-share by a CLI
flag** (`squeezefs storage nvmeof share [--spdk]`, `src/main.rs` ~2631):

1. **Kernel nvmet via configfs** (`share_target`, default): `modprobe nvmet` +
   `nvmet-tcp`, mounts configfs if absent, regular-file backings are wrapped in
   a **loop device** (`losetup`, association recorded in the subsystem dir),
   subsystem + `namespaces/1/device_path` + `attr_allow_any_host=1`, TCP ports
   (`addr_trtype=tcp`, ipv4) with subsystem symlinks. Teardown
   (`unshare_target`) unwinds symlinks → namespace → subsystem → port and
   detaches the recorded loop device. **Transport: TCP only** (no RDMA/loop in
   the CLI surface).
2. **SPDK via JSON-RPC** (`share_target_spdk`, opt-in): talks to a running
   `nvmf_tgt` over `SQUEEZEFS_SPDK_SOCK` (default `/var/tmp/spdk.sock`):
   `nvmf_create_transport(TCP)` → **`bdev_aio_create`** (block_size 4096) →
   `nvmf_create_subsystem` (allow_any_host) → `nvmf_subsystem_add_ns` →
   `nvmf_subsystem_add_listener` per IP. Teardown by RPC
   (`unshare_target_spdk`, also reached from `unshare_target` via the share
   registry's `is_spdk` flag). Lifecycle helpers: `spdk_install` (clone+build
   to `/opt/spdk`), `spdk_setup` (hugepages), `spdk_bind`/`spdk_unbind`
   (PCI vfio), `spdk_start` (spawns `nvmf_tgt -i 0 -m 0x1` — single-core mask,
   detached, no supervision).

Shared plumbing: a JSON **share registry** (`register_share[_ext]` /
`load_shares` / `deregister_share`) with `is_spdk` per share; `restore_shares`
replays registered shares after reboot (both flavors). **Host/initiator side is
kernel-only** in both cases: `connect_target` shells out to `nvme connect`
(nvme-cli), device discovery via sysfs — SPDK is target-side only. Both target
paths share the **btrfs CoW guard** (`ensure_nocow_backing`: statfs magic +
`FS_IOC_GETFLAGS`; refuses non-empty CoW backing files, sets `+C` on empty
ones) because O_DIRECT on btrfs-CoW silently degrades to buffered and wedges
the fabric — independent evidence for this report's substrate thesis.

**What "force SPDK for targets" (recorded user decision, future program)
replaces:**

- Deleted/replaced: `share_target`'s configfs walk, the losetup file-backing
  path (SPDK `bdev_aio` opens files directly, O_DIRECT), the nvmet branch of
  `unshare_target` + `restore_shares`, the `--spdk` flag (becomes the only
  path), `modprobe nvmet/nvmet-tcp` requirements.
- Becomes mandatory: SPDK daemon lifecycle — install/build (`/opt/spdk`),
  hugepage setup, `nvmf_tgt` supervision (today: unsupervised single-core
  spawn with no restart policy, no persistent config — `restore_shares` is the
  only replay). A real program needs: pinned SPDK version/packaging, tgt
  config file or RPC-replay on boot (systemd unit), core-mask policy sized to
  fabric load (the `-m 0x1` default single reactor is a scaling cliff),
  socket permissions (root-only RPC today), and observability (RPC health
  probe; today failures surface as connect-refused).
- Unchanged: initiator side (`nvme connect`, kernel host stack), the share
  registry shape (already carries `is_spdk`), `ensure_nocow_backing`.
- Open scoping questions for that program: (a) `bdev_aio` vs `bdev_nvme`
  (PCI-passthrough via existing `spdk_bind`) — aio keeps kernel fs backing
  possible, nvme bypasses the kernel entirely; (b) TCP-only vs adding RDMA
  listeners (nvmet loop-style local serving has no SPDK equivalent — local
  consumers would connect over TCP to 127.0.0.1); (c) whether **this
  report's loop-substrate method survives** — it does not: nvmet-loop is a
  kernel-target construct, so the local-bracket methodology stays on kernel
  nvmet even after targets go SPDK (measurement-only use is compatible with
  "SPDK for *serving*").

## Ranked lever list

Ranked by (expected win on the FUSE metadata-storm numbers above) ×
(confidence from this session's evidence). The mount-path default-cadence
create number to beat is **~6 k/s**; the engine proves **22–27 k/s** on the
same substrate.

| # | Lever | Evidence from this baseline | Expected win | Cost/risk |
|---|---|---|---|---|
| 1 | **FUSE-layer per-op cost teardown** (the 4.4× engine→mount gap): profile-guided slimming of the create/rename/unlink handler path — parent-inode op-lock hold shapes, per-op tokio task/timeout wrapping (2.75 % vdso clock alone), attr/entry-cache maintenance, allocation churn (jemalloc ~3 %) | trait 26.9 k vs FUSE 6.1 k creates/s **on the same substrate**; only ~18 of the +117–133 µs/create is transport floor; profile shows the cost is smeared (sched 8 %, epoll 10 %, glue) — a family of 5–15 µs cuts, not one cliff | stepwise toward 2–3× on creates; every µs cut ≈ +40 creates/s at current shape | M per item; needs a per-op wall breakdown first (tokio-tracing span timing), then targeted PRs |
| 2 | **Reduce FUSE round trips per create: 5.18 → ~3–4** — the K7-era bench Mkdir row (32 k/s) shows lighter op shapes go faster. Concretely: kernel-side negative-dentry caching for the pre-create LOOKUP (`entry_timeout` on ENOENT), and evaluate skipping the post-create GETATTR/FLUSH pair (FOPEN_NOFLUSH on 0-write handles) | measured 5.18 fuse_ops/create, 1.00 for stat; each round trip ≈ 3.4–5 µs floor + handler cost ≫ floor | −2 round trips ≈ −25–40 µs/create at current handler cost ⇒ +20–35 % creates | M — kernel-negotiated flags, semantics review (POSIX close-to-open); no on-disk change |
| 3 | **S2: over-uring commit batching** (one `ring.submit()` per commit_rx drain) + **S3: SQPOLL knob session** | **31 io_uring_enter/create** measured; `io_uring_enter` = 9.5 % daemon CPU + syscall entry/sched share on top | −30–50 % of ring submits ⇒ ~5–8 % op cost; more under higher concurrency | S–M (vendored fuse3); S3 is knob-only, burns a core |
| 4 | **Single-tx rename/unlink** (merge the 2 commits/op into one journal entry) | measured 2.002/2.020 journal entries per rename/unlink vs 1.006 for create; at strict-0 this is literally 2 barriers per op (rename 709/s vs create 1 291/s on A cow) | strict-mode rename/unlink ×2; deferred-mode: −50 % journal entries + one commit-pipeline pass saved ⇒ single-digit % | M — tx-boundary refactor in the two-key ops; crash-contract review (both keys already CoW-atomic per entry, merging *strengthens* atomicity) |
| 5 | **Group commit v2 for strict/short cadences**: leader-based *batch admission* ahead of the leaf locks (queue commits, apply batch under one lock pass, ONE journal entry chain + ONE barrier per batch) — today's `SyncCoalescer` only merges barriers of *already-written* entries and measures ≈ 1 commit/group under storm | strict A cow: 67 % of wall in barriers at group size ≈ 1; 8 writers available to batch; B1 strict shows the non-barrier ceiling (5.4 k/s) | file-substrate strict: up to ~6× (barrier-bound → engine-bound); real-NVMe strict: latency win per op | M–L — commit-pipeline change inside §4.4; the design doc's lock-order rules (4b) already accommodate batch leaf-locking (ascending, deduped) |
| 6 | **Directory-shard concurrency for hot parents** (or finer-grain parent-lock scoping on create/unlink) | single-dir 5.5–6.5 k/s vs many-dirs 25.5–28.3 k/s = **4.6–5.1×**, substrate-independent, both storm shapes 8-thread | multi-writer single-dir workloads up to ~4×; no effect on 1-writer | L — semantics-sensitive (readdir cookies, lock order 1→4); design-program material, not a quick PR |
| 7 | **FUA journal barriers on fua-capable block volumes** (`RWF_DSYNC` on the ring write, via io_uring `IORING_FSYNC_DATASYNC`-equivalent write flags) | FUA measured −11 % (null_blk) / −20 % (nvmet-loop) per barrier; `queue/fua` visible at mount-probe time (atomicity.rs precedent) | small here; potentially large on real NVMe (FLUSH of a full cache vs one FUA write); only strict/short cadences | S–M — gate on `queue/fua`, fall back to fdatasync; needs real-hardware validation |
| 8 | **KV read-side record-fold slimming**: `InodeDelta::decode` 7.9 % + `Reader::finish` 3.3 % + fold walk (`next_live`/`MergeIter`) ~4.5 % = **~16 % of daemon CPU** re-decoding deltas on every lookup/create probe | top self-cycles symbol in BOTH default and strict profiles; independent of substrate | ~10 % daemon CPU (≈ +5–8 % storm throughput), and it compounds with lever 1 | M — e.g. memoized folded-inode cache keyed on (node, seq), or decoded-delta memo in the node cache; RAM-authority interplay (R5) |
| 9 | **DLM lease batching** — *demoted by evidence*: `lease_acquire_*` = 0 across every metadata storm; cluster leases are not on this path | measured zero | ~0 for metadata storms (keep for the data-write lease path where it was originally proposed) | — |

**Explicit non-levers (measured dead ends):** raising
`SQUEEZEFS_META_FLUSH_INTERVAL_MS` above 50 ms (flat curve — pure durability
loss); substrate/O_DIRECT tuning for *deferred* metadata throughput (bracket
flat ±8 %); btrfs `chattr +C` for barrier cost (465 vs 495 µs — noise).

## Testing-methodology consequence (the user's question, answered)

For **throughput** work at the default cadence, file-backed volumes on
`/home` (even CoW+zstd btrfs) are a *valid* substrate — the bracket is flat.
For **strict-durability latency, group-commit, FUA, or barrier-order** work
(levers 4/5/7), file-backed volumes are **off by 4–7× and can invert lever
signs**; use the null_blk/nvmet-loop recipe from this report (10 minutes of
root setup, no spare hardware needed, FUA-capable, real FLUSH). The
`~/tmp/mdbase_20260714/{setup,teardown}_substrates.sh` scripts in the session
workspace are the working recipe; their quirks (device naming, `device_uuid`,
`-i 4`) are recorded under Substrates above.

## Rails & incidents log

- The box ran the fstests re-gate + residue-EIO agent + an LTP cycle
  throughout; all timed rows quiet-gated as stated. Setup/reads proceeded
  under load (untimed).
- **Harness incidents (all fixed before the reported rows; discarded rows
  archived in the session workspace):**
  1. The first batch attempt (a) trusted a single quiet poll and raced a
     re-gate loop lull — quiet thereafter required a 3-poll 45 s streak; (b)
     pointed the raw-floor `syncprobe` (which writes at offset 0) at the meta
     volume files, clobbering their superblocks — floors moved to dedicated
     scratch files, and `fmt_vol` zeroes the volume head before
     `format --force` because **dev-tip format refuses corrupt-magic
     volumes** (the in-flight `fix/format-force-unsupported` branch fixes
     exactly this class; this baseline stands on dev alone and does not
     depend on it).
  2. `pgrep -f 'check -g auto'` (the prescribed gate probe) false-positives
     on the *poller's own cmdline*; switched to comm-exact `pgrep -x check`.
  3. **Silent daemon deaths mid-storm** (attempt 2/3: three `Transport
     endpoint is not connected` kills, all on runs whose binary path
     contained `squeezefs`): no panic, no OOM (scope peak 664 MB of 8 G), no
     coredump; the daemonized child's stderr goes to the log only after the
     panic hook, and nothing was logged. Foreground repro of the same storm:
     clean. After renaming the harness binary to `~/…/bin/sqmd` (cmdline no
     longer matches `squeezefs` patterns), **zero deaths in 20+ subsequent
     runs** — consistent with an external pattern-kill (a co-tenant harness's
     cleanup `pkill`), not a product bug. Flagged for awareness: kill-by-PID
     discipline matters when multiple agents share a box.
  4. One `MOUNT FAILED` timeout left a **late-arming daemon** behind, which
     then double-mounted the *next* run's freshly reformatted volume (two
     daemons, one meta file ⇒ one run's rename storm saw ENOENT on names it
     had created). Harness now reaps the daemon on readiness timeout.
     Product observation for the design program: **nothing refused the
     second concurrent mount of the same meta volume** — worth a loud
     single-writer guard (lease/flock on the volume file/device).
- nvme-cli EXDEV + null_blk naming + nvmet `device_uuid` quirks recorded
  under Substrates (repro cost for the next agent: zero).
- Artifacts: `~/tmp/mdbase_20260714/` — `results.tsv` (every row +
  quiet/DIRTY flag + per-run Tctl/load provenance), per-phase `.stats`
  snapshots (`stats/<tag>/<phase>_{pre,post}.json`), perf/strace captures
  (`stats/attrib_*/`), harness sources (`mdstorm.c`, `syncprobe*.c`,
  `run_*.sh`, `lib.sh`, `analyze.py`).
