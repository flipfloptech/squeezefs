# 2026-07-28 — Ingest economy: one sessions/threads derivation, spawn-on-bind, pooled write severs

Branch `perf/ingest-economy` off dev tip `88f2075` (the op-economy
merge). Commits: red `37b8711` (defaults-derivation + spawn-on-bind
contracts), green `fa13a0a` (the shared derivation + spawn-on-bind),
red `883487e` (severed-pool contract + counters), green `4692c34`
(SeveredPool + named fuse3 TPC lanes), plus the docs/evidence commit
carrying this note. Design: `docs/design-preload-interception.md`
Rev 13.

## 0. The field evidence (motivating data — user's cluster, binary `22c31ac`)

4-node 2×200GbE nvme-tcp cluster, 32-CPU client, nullblk data targets.
Large sequential writes over the shim:

- **Raw dual-target fio from the same client: 16.6 GB/s aggregate** —
  wire, kernel, servers, targets all exonerated. The wall is the
  client-side ingest path.
- SqueezeFS elbencho 64t×4MiB: **7.5 GB/s**; devices at `aqu-sz` ~2.2;
  write-pipeline governor engaged (depth_target 64 MiB) with
  `admission_waits` ≈ 0 — the pipe never fills; production-rate-limited
  at ~1,900 blocks/s.
- **pidstat -t: `sqz-ipc-svc0..3` at 94–99 % CPU (81–87 % %system),
  `svc4..7` IDLE** — the shim's flat `SQUEEZEFS_IL_SESSIONS=4` default
  left half the daemon's 8 service threads (`clamp(cpus/4,2,8)` on 32
  CPUs) with no ring to drain. 4 threads × ~1.9 GB/s = the 7.5 wall.
- Field experiment `SQUEEZEFS_IL_SESSIONS=8`: **10.8 GB/s (+44 %)**,
  all 8 svc threads at ~67–77 % (~10 % usr / ~60–68 % sys) — no longer
  thread-saturated; a deeper stage limits next.

Two independent constants — the shim session default (sized under L4
IOPS economics) and the daemon service-thread default — had drifted
into a topology where half the drain capacity was structurally idle.
(Post-hoc note: some of the field's per-thread attribution was itself
polluted — see §3's comm-inheritance finding.)

## 1. Item 1 — one derivation, spawn-on-bind (commits `37b8711` + `fa13a0a`)

- **`squeezefs_ipc::sizing::il_sessions_default(cpus) =
  clamp(cpus/4, 2, 16)`** now defaults BOTH the shim's per-mount
  fd-shard session count and the daemon's service-thread ceiling — one
  function, shared by the crate both halves already depend on. Paired
  tie tests (`session_sizing_tests` in `squeezefs-preload`,
  `service_ceiling_default_ties_to_shim_session_default` in the root
  suite) turn any future drift into a red test. `cpus/4` is the
  measured 2026-07-19 drain-saturation slope; floor 2 = the pre-L4-8
  single-consumer plateau; ceiling 16 = shim-registry (32 slots across
  mounts) + R5 arena math (16 × 64 MiB default arenas = 1 GiB inside
  the `min(budget/8, 2 GiB)` session-shm cap; past-cap admission keeps
  refusing honestly, counted, shards passthrough).
- **`SQUEEZEFS_IL_SESSIONS` / `SQUEEZEFS_IPC_SERVICE_THREADS` are
  override levers only** (shim clamp widened 1..=8 → 1..=16 to match
  the derivation ceiling).
- **Spawn-on-bind**: service threads spawn when their owner index first
  receives a session (`IpcHost::ensure_service_threads` — serialized on
  the `threads` mutex, shutdown-race-safe, spawn failure refuses the
  session loudly). A session-less host — every default mount's
  control-plane-only posture — now owns ZERO service threads
  (previously 2..8 permanently-parked spares per mount);
  `ipc_service_threads` gauges the SPAWNED count. On the field box the
  derivation lands exactly the +44 % experiment's topology: 8 sessions,
  8 threads, all fed.

## 2. Item 2 — the per-byte serve cost: profile + fix (commits `883487e` + `4692c34`)

### 2.1 The conviction (TCP devsub rig)

Venue: devsub **tcp** substrate (`SQZ_DEVSUB_TRANSPORT=tcp` — nvmet-tcp
on localhost; mds nvme1..4 = memory null_blk, oss nvme5..8 = zram,
service port 54129). Instrument: fio 3.42 psync `--zero_buffers`
`--direct=1` t16 b4m through the shim (zeros store ≈ free on zram — the
field's nullblk analog: the DEVICE is not the wall, the client path
is). 22-CPU box (derivation: 5 sessions / 5 service threads).

Pre-fix, at 10.2–12.7 GB/s sustained:

| thread | cpu% | usr% | sys% | minflt/s |
|---|---|---|---|---|
| sqz-ipc-svc0 | ~103 | ~29 | ~73 | ~460 k |
| svc1..svc4 | 67–97 | 20–28 | 48–69 | 0–420 k |
| (aggregate svc) | — | — | — | **~1.76 M faults/s** |

Process-wide syscall census (5 s, strace -c): 25.9 k `madvise` (the
jemalloc purge stream), 135 k futex, 37.9 k epoll_wait, 3.8 k
io_uring_enter. The svc threads' dominant kernel cost is **minor-fault
handling + page re-zeroing**, not syscalls.

**The engine**: `ArenaWindow::read_severed`'s per-op `vec![0u8; len]`.
A 1 MiB (`max_op_bytes`) slab-class allocation per ring write sits past
jemalloc's tcache ceiling → every op pays the arena mutex + extent
recycle + `MADV_FREE` purge + ~256 page re-faults, with the kernel
re-zeroing pages the sever memcpy immediately overwrites (the zeroing
was 100 % waste even pre-pool: `vec![0u8; n]` + full-length
`copy_nonoverlapping`). This is the field's 81–87 % %system signature.

### 2.2 The fix — `SeveredPool`

Ring-write severs copy into recycled `max_op_bytes`-class buffers
(`Bytes::from_owner` over a `PooledSevered` that returns the buffer on
last-clone drop). **The §5.5.2 severance law is untouched: same ONE
arena read, same single memcpy — now into page-warm memory.** Sizing
derives (no constants): buffer class = geometry `max_op_bytes`; queue
slots = `arena_cap_bytes / max_op_bytes` (the structural in-flight
bound — severed bytes in flight can never exceed the R5-admitted
session arenas), railed 1..=65536; retention gauged
(`ipc_severed_pool_bytes`), reuse-health counters
(`ipc_severed_pool_{hits,misses}`). `crossbeam::queue::ArrayQueue` is a
shipped dependency core, not a new house lock-free algorithm — no loom
model owed. Standing pin:
`production_write_sever_recycles_pooled_buffers`
(tests/ingest_economy_tests.rs — real host → service thread →
`DataPlaneSink::serve_write` path).

Post-fix, same venue/instrument:

| thread | cpu% | usr% | sys% | minflt/s |
|---|---|---|---|---|
| svc (busiest) | 72 | 70 | **1.6** | **88** |
| fuse3-tpc lanes | 12–14 each | ~12 | ~1 | 80–370 |

Pool health at 220 k ops: **hits 220,620 / misses 48 (99.98 % reuse)**,
retained 48 MiB. The svc-thread kernel term is gone; the remaining cost
is the sever memcpy itself (usr) plus the handler lanes.

### 2.3 Found while profiling: fuse3 TPC lanes inherited `sqz-ipc-svc0`'s comm

The lazily-spawned fuse3 per-core handler lanes were UNNAMED
`std::thread::spawn`s — a new thread inherits the comm of its creator,
and on interception mounts the first `tpc_spawn` comes from an ipc
service thread, so **all ~21 lanes showed in pidstat/perf as
`sqz-ipc-svc0`**. This polluted the field capture's per-thread
attribution (and this campaign's own first census + a test's OS-thread
assertions). Fixed: lanes are named `fuse3-tpcN` (crates/fuse3
session.rs). Field operators re-running pidstat will now see the lanes
separately.

## 3. Bracket (A-B-B-A vs dev tip `88f2075`, TCP devsub, quiet box)

Sides: CAMP = this branch tip, BASE = dev `88f2075` (worktree build) —
KD-7 same-commit daemon+shim pairs, clean identities, no dev override.
Order CAMP-BASE-BASE-CAMP; fresh blkdiscard + format + interception
mount per side; per-row engagement EXACT (`ipc_bytes_in` delta == row
bytes on every il cell, == 0 on every kernel cell); per-row
`/proc/diskstats` amplification columns on the data namespaces; reclaim
settle-to-idle between reps (the first, discarded bracket run showed
±15 % inter-row bleed from discard backlogs — rows-*.csv of both runs
preserved; the counted run is `bracket2`).

Medians (MiB/s; fio rows = 5 reps, elbencho rows = 3):

| row | CAMP-1 | BASE-1 | BASE-2 | CAMP-2 | camp/base |
|---|---|---|---|---|---|
| **fio-il-t16-b4m** (zero-buf, the wall venue) | 9,834 | 7,502 | 8,895 | 10,082 | **+21.5 %** |
| **fio-il-t64-b1m** | 9,900 | 7,706 | 9,352 | 9,930 | **+16.3 %** |
| fio-kern-t16-b4m (canary, no shim) | 11,686 | 11,093 | 11,686 | 11,612 | +2.3 % (box stable) |
| il-w-t16-b1m (elbencho) | 1,533 | 1,434 | 1,672 | 1,452 | 0.96× wash |
| il-w-t64-b1m | 1,628 | 1,637 | 1,589 | 1,763 | 1.05× wash |
| il-w-t16-b4m | 1,588 | 1,481 | 1,471 | 1,663 | 1.10× |
| il-w-t64-b4m | 1,624 | 1,545 | 1,684 | 1,589 | 1.00× wash |
| kern-w-t16-b1m (canary) | 1,520 | 1,571 | 1,578 | 1,652 | 1.01× ✓ |
| kern-w-t64-b4m (canary) | 1,622 | 1,486 | 1,724 | 1,765 | 1.06× |

- **The wall rows moved +16–22 %, order-independent, with the no-shim
  canary flat** — the item-1 topology (5 sessions/5 threads on this
  22-CPU box vs BASE's 4 sessions) plus the item-2 pool. elbencho rows
  are **zram-compression-bound by instrument** (random buffers: il ≈
  kernel ≈ the 1.4 GiB/s incompressible raw ceiling on both sides —
  stated, not hidden; they are the canary that the device-bound regime
  did not regress).
- **Achieved fraction of ceiling (this rig)**: raw fio `--zero_buffers`
  on the 4 namespaces = **21.0 GiB/s**; kernel-FUSE write path =
  11.7 GiB/s (56 %); **il path 9.9–10.1 GiB/s (47–48 %)**, up from
  BASE's 7.5–9.4 (36–45 %). Honest residual: on THIS box the kernel
  path out-streams the ring path by ~15 % at t16-b4m — the ring write
  still pays shim-copy + sever-copy + merge vs the kernel path's
  payload-lease + merge; the next ingest lever lives there (OQ: sever
  directly into the ActiveBlockBuf — an OQ-3-class write-fast-path
  question, NOT taken in this campaign).
- fio rows are **relaxed-ACK labeled** (RW6 convention: no fsync in the
  loop; identical law on both sides — the write pipeline drains behind
  the ack stream). elbencho `--direct` rows carry amp ≈ 0.97–0.99 with
  wareq ≈ 4 MiB (no request-size collapse); `write_path_seed_read_bytes`
  = 0 throughout.

## 4. Mystery reads — adjudicated

Field shape: ~1,800 reads/s × ~22 KB on EACH data namespace during pure
fresh 4 MiB writes (~40 MB/s, ~1 % of bandwidth), binary `22c31ac`.

**Reproduced on the rig** (fio rows: 424–650 read ops, avg ~18–20 KiB,
during the write window) and attributed with per-phase
`/proc/diskstats` + daemon `/proc/<pid>/io` + full stats-inode deltas:

1. **The reader is the daemon's flush/overwrite SEED machinery** —
   `fetch_seed_image` (whole-block reads via `read_block`) for active
   blocks that flush while their written-coverage union is still
   partial. Counter signature per 16 GiB fio row (16 × 1 GiB files):
   `overwrite_seed_deferred` +4,096 (every block arms), `_skipped`
   +4,080 (union completed → no read), **`_materialized` +16 (one block
   per file paid a whole-block seed read)**, `flush_seed_read_bytes`
   +64 MiB. The nvme-tcp initiator splits each 4 MiB seed read into
   MDTS-bounded sub-requests — **that is the small-rareq (~20 KiB)
   read stream iostat shows**; on the field's nullblk targets the same
   splitting yields the ~22 KB signature.
2. **The trigger is pre-sized files**: fio's default
   `--fallocate=native` (and any rewrite of an existing dataset) makes
   every write an INTERIOR write of a file whose size already extends
   past the block — the coverage machinery arms the seed deferral, and
   the handful of blocks that reach their flush boundary before the
   union completes materialize the seed. A/B on the rig: the seed
   counters march identically with `--fallocate=none` once files exist
   (rewrites), and `overwrite_seed_deferred` stays 0 only on genuinely
   fresh growing files (the elbencho rows: devR ≈ 4/row = the 4 KiB/5 s
   health probe only).
3. **Verdict: legitimate, bounded, documented — not a bug fix in this
   campaign.** The seed read is the crash-consistency complement fetch
   for a partially-covered flush (RW3b: `seed_deferred ⇒ union
   partial` — structural); the rate is ~0.4 % of blocks (1 per file
   here; ~1 % of bandwidth in the field). The remaining question the
   field can answer with counters (no blktrace needed):
   `overwrite_seed_materialized` / `flush_seed_read_bytes` deltas
   during a run — if they account the 40 MB/s, it is this machinery
   (and "why does a streaming file's block flush before its union
   completes at 1,900 blocks/s" becomes a targeted follow-up: the
   pipeline's flush boundary racing the 4-chunk ring reassembly);
   if they stay 0, the reads are kernel/initiator-side (our delete
   windows also show ~100 MiB of non-daemon kernel reads riding the
   discard storms — daemon `read_bytes` flat across that window).
   Secondary standing readers, quantified: the health probe
   (`probe_read_block`, 4 KiB per device per 5 s tick — 0.2/s/dev,
   deliberately uncounted in `get_obj`) and indirect block-map refetch
   (`fetch_metadata_from_backend`, `block_size`-sized, only on
   metadata-cache misses for >~6 GiB-map files — absent while streams
   keep entries merge-fresh; verified absent on 7 GiB-file rig runs).

### 3.1 write_matrix sweep (no-regression check, devsub-tcp substrate — stated: NOT the canonical fabric-latency venue)

`tests/write_matrix.sh` filtered to the protected odirect rows
(`SQZ_META_DEV=/dev/nvme1n1 SQZ_DATA_DEV=/dev/nvme5n1`, fio psync,
medians of 3, engagement checks internal to the script):

| row | CAMP | BASE | verdict |
|---|---|---|---|
| armed-shim-rand-4k (window 2, clean, B-then-A) | 94,023 | 94,772 | −0.8 % — noise; kernel canary −1.0 % same drift ✓ |
| armed-kernel-rand-4k (canary) | 94,187 | 95,136 | flat |
| armed-shim-rand-1m | 1,196 | 1,123 | +6.5 % |
| armed-kernel-rand-1m | 1,142 | 1,100 | +3.8 % (canary drift — wash) |

The FIRST (single-order) window showed shim-rand-4k CAMP 150.8 k vs
BASE 176.9 k — but its two sides ran in DIFFERENT substrate regimes
(both sides' 4k rows collapsed to the ~94 k device-bound level in the
clean matched window; the kernel canary pinned the drift) — the
standing A-B-B-A rule catching a single-order artifact again. Verdict:
**the 4 KiB rows are flat (protected); no matrix regression beyond
noise.** The seq-4k/1m rows EIO'd identically on BOTH sides mid-run —
zram capacity exhaustion by the matrix's incompressible datasets on the
8 GiB×4 substrate (a rig limitation, not a product row: the canonical
matrix venue is memory-backed null_blk).

## 5. Gates (campaign-side status at the blocking-finding handoff)

- clippy `-D warnings` clean (root all-features + fuse3 + preload
  [interposers] + ipc crates); fmt clean everywhere; fuse3 standalone
  suite 41/41.
- New contract tests green: ingest_economy ×4 (+ the severed-pool
  production-path pin), preload session_sizing ×2, ipc sizing ×1,
  transport_lease_overlong ×1; directly-affected suites green
  (ipc_host 21, preload_session 18, ipc_op_economy 3, ipc_direct_drive
  11, preload_parity 14, preload_lifecycle 5).
- statfs_tests ×10 under the stated load recipe + repro ×3 under load:
  green, 0 hangs (with the watchdog fix present on this branch).
- **The from-zero full suite + preload gate legs 1+2 rerun AFTER the
  rebase onto dev+`fix/opeconomy-writeback-hang`** (orchestrator's
  sequencing: the fix merges first; the campaign's first from-zero roll
  is what surfaced the blocking finding and was aborted by it). On the
  fix branch itself the full gate is complete from zero (148/148 test
  binaries, doc, bench smoke — its evidence note carries it).
- loom: not owed (no house lock-free core changed; crossbeam ArrayQueue
  is a shipped dependency; the fuse3 change removes a panic from a Drop
  without touching the release()/ordering protocol).

## 7. BLOCKING dev finding — the transport-lease watchdog daemon wedge (root-caused, fixed red-first)

**Orchestrator ruling applied verbatim: a load-dependent hang is a
first-class product bug with its repro condition already identified —
load selects the losing schedule, it does not cause it.**

### 7.1 The capture

During a from-zero full-suite gate roll on this branch,
`statfs_tests::test_statfs_free_tracks_write_then_delete_reclaim` hung:
the test thread sat 16+ minutes in **uninterruptible D-state** with
kernel stack `fuse_fsync → file_write_and_wait_range →
folio_wait_writeback`; its (debug-build) daemon's fusectl connection
showed **`waiting=28`** — 28 FUSE requests whose replies were never
sent. The daemon log: repeated `fuse3-tpcN` panics of
`crates/fuse3/.../fuse_over_uring.rs:342` —
`debug_assert!(lease age < 1 s)` in `EntPayloadLease::drop`. Even
`umount` recursed into the hang (it syncs the superblock behind the
same stuck writeback); only `echo 1 > /sys/fs/fuse/connections/<id>/abort`
released the D-state threads.

### 7.2 Mechanism (three links, all pinned)

1. Under saturated buffered writeback, a kernel-lane FUSE_WRITE
   **handler invocation legitimately exceeds 1 s** — write-pipeline
   admission waits are honest backpressure, and debug builds / slow
   substrates stretch them. The payload lease lives exactly as long as
   the invocation (§5.4's design bound).
2. The watchdog `debug_assert` fires **inside the handler task** (the
   lease drops mid-handler) — the panic unwinds the task, so **the FUSE
   reply is never sent**: the kernel's writeback folio never completes,
   `fsync(2)` parks in D-state forever, and every later sync of that
   superblock (umount included) joins the wedge.
3. The unwind also skips `state.release()` — that ring ent's deferred
   COMMIT_AND_FETCH re-arm parks forever: **permanent queue-depth
   loss** stacking with each firing.

### 7.3 Deterministic repro (schedule forcing, not statistics)

`SQUEEZEFS_TEST_WRITE_STALL_MS` (documented test seam in the write
handler — stalls the invocation while the payload lease is held, the
exact shape a parked admission produces under load) +
`tests/transport_lease_overlong_tests.rs`: real binary, real
unprivileged FUSE-over-io_uring mount, buffered 256 KiB write + fsync
against a 1.2 s stall. **Red every run pre-fix** (60 s deadline; the
harness unwedges itself — abort-waiting-connections BEFORE umount,
which this investigation learned the hard way). The first harness
iterations that omitted the abort-first order wedged their own test
processes in D-state — reproducing the gate's failure shape exactly.

### 7.4 Verdict: LATENT DEV BUG — branch WIP exonerated by evidence; CONFIRMED by orchestrator bisect

The identical seam + test applied to a pristine BASE worktree at dev
tip `88f2075` (evidence run, not committed there): **FAILED identically
(60 s deadline, same daemon panic signature)**. Nothing in this
branch's WIP touches the kernel write path; the assert and the >1 s
handler shapes both predate it. Reported as a blocking dev finding; the
orchestrator's independent bisect (loaded rolls ×6/commit) converged:
78b9498 / 7396ffd / 22c31ac all 6/6 green, dev tip 346be8d (delta = the
op-economy merge) 3 hangs. The merge is the perturbation that pushed
handler timing across the armed 1 s line (instrumented lease max-age
high-waters under load: 417–553 ms at 22c31ac vs 484–595 ms at 346be8d
— BOTH within ~2× of the trip line); the watchdog's kill-the-handler
action is the bug. The fix ships on the dedicated branch
`fix/opeconomy-writeback-hang` off dev 346be8d (red `7281b95`, green
`c2a713d`, evidence `.benchmarks/2026-07-28-opeconomy-writeback-hang.md`
there) for merge AHEAD of this campaign; the same pair exists here as
`7f66526` + `0421c9f` and will drop out on rebase.

### 7.5 The fix

`EntPayloadLease::drop` never panics: overlong leases count in the new
**`transport_lease_overlong`** tripwire (`transport_lease_stats`
5-tuple → stats inode) with a loud error log, and `release()` always
runs. A GENUINE §5.4 escape (payload parked toward a long-lived cache)
manifests as unbounded `transport_lease_max_age_ms` + growing
`transport_parked_commits` — adjudicated by counter, never by killing
the data plane. Green: the repro test (write+fsync completes in 2.8 s,
tripwire ≥ 1, mount stays serviceable — second write/fsync/read-back
pass).

### 7.6 Counted acceptance under load

Load recipe (stated): a looping fat-LTO `cargo build --release`
(`touch src/lib.rs` per iteration) in a second worktree on the same
22-CPU box — the same compile-storm load class the original gate roll
ran under. `statfs_tests` (whole file, `--test-threads=1`) ×10 under
that load: results in §5; 0 hangs required, plus a
zero-waiting-connections residue sweep after the runs.

## 6. Found while measuring (recorded)

- **fuse3 TPC lanes were pidstat-invisible** (comm inheritance) — fixed
  and load-bearing for every future svc-thread attribution (§2.3).
- **Debug-only transport-lease watchdog can wedge a debug daemon under
  saturated buffered writeback**: during one from-zero suite run,
  `statfs_tests::test_statfs_free_tracks_write_then_delete_reclaim`'s
  child daemon (debug build) hit the §5.4 lease-severance
  `debug_assert` ("payload lease held ≥ 1 s") repeatedly on fuse3-tpc
  lanes during the 1 GiB fsync writeback storm; each firing kills a
  lane thread, and the TPC round-robin then blackholes every future
  dispatched to the dead lane — the daemon wedges and the test hangs
  in `fuse_fsync` (uninterruptible). Attribution runs (×3 per side,
  matched quiet box, live daemon-log capture): **0 firings on BASE
  `88f2075` and 0 on this branch** — load-dependent, not
  branch-attributable; the >1 s lease lifetimes under a debug-build
  writeback storm are the write-pipeline era's admission waits
  reaching the kernel-lane handler (in BASE too). Recorded for the
  board: (a) a release build never carries the assert; (b) the
  lane-death → blackhole wedge shape deserves a panic-guard/respawn
  discussion independent of this campaign.
- The elbencho-vs-fio instrument split on zram (random buffers vs
  zero_buffers = device-bound vs client-bound regimes) is this rig's
  face of the standing instrument-alignment lesson — both stated on
  every row here.
