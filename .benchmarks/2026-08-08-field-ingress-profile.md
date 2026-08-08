# 2026-08-08 — THE FIELD INGRESS PROFILE (r5 §6 executed)

| | |
|---|---|
| **Charter** | The owed measurement from `.benchmarks/2026-08-08-iops-internal-time-r5.md` §6 (the a5d47bc6 companion directive): deploy the dev tip on the field box, run the 32×32 rand-4k il row at default posture, and profile WHERE the field's ingress microseconds actually go — same svc-cost ladder as the calibrated local venue (then the residual ladder rules) or a field-only term no local venue manufactures. Adjudicate the row against the 663 k / 1,545 µs / 1.25 ms pre-fix equilibrium and the r5 §6 composition projection (0.93–1.14 M). Record-don't-fix. |
| **Binary** | dev tip `4d033c2f` (r5 L1 single-read clock law + L2 zero-heap probe, r3 lane-scoped flush + COOP_TASKRUN, fused-predicate fix `eb403ec8`, fusion default ON). `task build:rocky8`, KD-7 pair rsync'd + md5-verified both ends (`85f50515…` bin / `55828b00…` shim), `build_commit 4d033c2fd2a5` confirmed on the mount. |
| **Venue** | squeeze-test (memp-s3ds-aqs-37): **2-socket Xeon Gold 6426Y** (16+16 cores, no SMT, **2 NUMA nodes**, node0 = even CPUs / node1 = odd), kernel 6.19.14-sqz, real NVMe-oF/TCP fabric via 2×mlx5 (990 established nvme-tcp conns, ss RTT mean 40 µs, unacked ≤ 3). Local calibrated venue for contrast: **1-socket / 1-NUMA-node** AMD (Ryzen AI MAX+ PRO 395), nvmet-tcp localhost + 240 µs null_blk timer. |
| **Posture** | Fresh default mount, NO env vars: `mount sqmeta:///dev/nvme{0,2,4,6,8}n1 /scratch/tmp/test --daemon --interception --allow-other`. Arm line present; zc + fusion defaults in force. |
| **Artifacts** | squeeze-test `/scratch/tmp/profile-0808/` (pre/post stats JSON, fio JSON, daemon.perf 13 GB dwarf + svc/dd relative reports, client.perf, mpstat/pidstat/ss captures, leg scripts). |
| **Verdict** | Row **706 k / clat 1,450 µs / ingress 1,165 µs** — +6.5 % IOPS / −6 % clat vs the equilibrium; **projection NOT met**, and the profile says exactly why: the field's svc service time is **55 % moka metadata-cache bookkeeping** — the r5 ladder's L3 term at a **field-only 5–8× magnitude** (cross-socket coherence on the 2-node box). L1+L2 engaged exactly (clock 8.9 % → 0.38 %, probe strings gone) but were ~8–9 % of the FIELD's svc cycles, not the local 17 %. |

## 1. The row (32 jobs × qd32, libaio, direct=1, norandommap, 60 s + 10 s ramp, il lane)

Engagement FATAL gates: **PASS, exact** — `ipc_ops_read` Δ 49,354,051 ≡ ingress n 49,354,051 (the count law);
`ipc_direct_drive_serves` 48,818,012 + `ipc_fast_path_serves` 530,307 + `ipc_async_handoffs` 5,732 = ops exactly;
tripwires (`ipc_sessions_poisoned`, `ipc_descriptor_rejects`, `ipc_direct_reap_stalls`, `transport_lease_overlong`) all 0; 32 binds, 12 svc threads, 12 dd lanes.

| metric | field pre-fix equilibrium (r2/r3 reads) | THIS ROW (4d033c2f) | Δ |
|---|---|---|---|
| IOPS | 663 k | **706,046** (supplemental 30 s re-run: 709,930) | **+6.5 %** |
| clat mean | 1,545 µs | **1,449.8** (p50 528, p99 18,481) | −6.2 % |
| `ipc_ingress_ns` mean | ~1,250 µs (~80 % of clat) | **1,165.4** (p50 512, p99 32,000) | −6.8 % |
| device `inflight` mean | ~330 µs | **307.6** (total 333.5; admit 15.6, finish 8.3) | par |
| drain pass | — | mean 7,774 µs, 432.9 ops/pass → **svc 17.96 µs/op** | r5 local: 13.5 µs/op |
| projection (r5 §6) | 0.93–1.14 M | **NOT MET** | see §3 |

The shape is unchanged from r3's field read: ingress (publish→dequeue queue wait) is ~80 % of clat, device
inflight is a fifth of it. mpstat: box 93 % busy (44 usr / 24 sys / 13 iowait / 9.5 soft / 3.2 irq); pidstat:
svc threads **87 %** CPU each, dd 55 % (21 % wait). Arrival 706 k/s × svc 17.96 µs/op ÷ 12 threads = **ρ ≈ 1.06
on the svc population** (dd finish-side serves absorb the excess) — the row is **svc-CPU-bound**, and the
ingress term IS that queue.

## 2. The field hotspot ledger vs the local calibrated ledger

Same units both sides: **within-population relative %** (perf dwarf, cycles; field sample 1.12 T cycles over
18 s mid-window across all 24 threads; svc = 60.2 % of daemon sample, dd = 39.8 %). Local column =
r5 note §3 (calibrated venue, same binary lineage pre-L1/L2 for ranks 1–2, post-fix otherwise).

| # | field svc symbol | field svc % | local svc % | field dd % | local dd % |
|---|---|---|---|---|---|
| 1 | **moka `BaseCache<u64,CachedMetadata,ahash>::get_with_hash::{closure#0}`** | **31.89** | (in #3's ~7.0) | **25.44** | (in ~9.4) |
| 2 | `NvmeCache::get_static` | 5.76 | 1.7 | 2.90 | 2.0 |
| 3 | moka `StreamLanes` `record_read_op` (String-keyed) | 4.63 | (family) | — | — |
| 4 | `DirectDriveEngine::submit` / `drain_cq_locked` | 3.66 | 2.8 | 5.51 | 3.6 |
| 5 | moka `Cache::get` (CachedMetadata) | 3.31 | (family) | 2.52 | (family) |
| 6 | `__memmove_avx512_unaligned_erms` | 3.29 | — | 2.05 | — |
| 7 | moka `do_run_pending_tasks` (CachedMetadata) | 2.35 | (family) | 3.24 | (family) |
| 8 | moka `StreamLanes` `get` | 2.16 | (family) | — | — |
| 9 | moka FileAttr cache `get_with_hash` closure | 1.75 | (family) | — | — |
| 10 | `IpcHost::drain_pass` | 1.56 | 2.0 | — | — |
| — | `__vdso_clock_gettime` + `Timespec::now` | **0.38** | **8.9 (pre-L1)** | ~0 | 7.1 (pre-L1) |
| — | probe strings (`fmt::write`/`TwoWaySearcher`) | **gone** | **~8.0 (pre-L2)** | — | — |
| — | `mutex_spin_on_owner` (dd uring_lock residue) | <0.4 | 2.9 | (k locks 4.77 total) | 8.1 |
| — | kernel net (mlx5/skb/nvme_tcp TX+RX) | 4.37 | 0 (loopback venue) | **15.49** | ~0 |
| — | kernel blk/nvme | 0.29 | ~2.8 (mq-deadline+wbt, venue) | 6.39 | ~3.8 |

**Family sums** (field): svc — moka+hash **54.63 %**, tier probes 6.92, engine 6.60, kernel 9.12, memmove 3.29,
clock 0.38, malloc 0.43. dd — moka+hash **33.43 %**, kernel 40.73 (net 15.49, blk/nvme 6.39, locks 4.77),
engine 6.65. Client (32 fio workers + shim, ~2 CPUs total): `aio_reap_served` scan 15.5 %, arena→user
`memmove` 8.9 %, ring poll 2.7 %, `is_done_for` 2.3 %, `io_submit` 1.9 % — **no field-only client term**; the
client is not the constraint (r3's verdict re-confirmed with a proper worker-population sample).

**L1/L2 field engagement confirmed**: the two symbols the levers deleted are gone from the field profile
(clock 8.9 → 0.38 svc; string machinery below the 0.05 % floor). The r3 residue (`mutex_spin_on_owner`) is
also gone — lane-scoped flush + COOP_TASKRUN hold on the field.

## 3. The adjudication — the named term

**The residual ladder ruled, but re-priced.** The field's ingress microseconds go to svc queueing whose
service time is dominated by the ladder's **L3 term (moka + hashing)** — at a magnitude the local venue
structurally underprices: local 7.0 % svc / 9.4 % dd → field **54.6 % / 33.4 %**. Arithmetic:
moka+hash ≈ 46 % of the 1.12 T-cycle sample ≈ 28.8 G cycles/s ÷ 706 k ops/s ≈ **40 k cycles ≈ 12 µs/op**
spent in metadata-cache bookkeeping — two-thirds of the 17.96 µs svc per-op cost.

**The field-only amplifier is cross-socket coherence, not a new symbol.** The mechanism: the serve prelude +
completion revalidate pay ~2 gets/op on ONE process-global moka `CachedMetadata` cache (plus one String-keyed
`StreamLanes` get + `record_read_op`, plus the FileAttr cache), and every moka get performs shared-line atomic
RMWs (read-recording ring push, TinyLFU frequency-sketch updates, entry-timestamp CAS, LRU deque maintenance
in `do_run_pending_tasks`) against ~32 hot inos hammered by 24 threads. On the 1-node local venue those RMWs
resolve in-LLC (tens of ns); on the 2-socket field box (threads interleaved across both nodes — pidstat shows
svc/dd on both CPU parities) each is a UPI cacheline ping-pong (~300–600 ns) with CAS-retry inflation. The
NUMA campaign's placement instruments show its own scope is healthy — `numa_local_bytes` 2.17 GB vs
`numa_remote_bytes` **0** (copy passes 1.000 local) — but the moka caches are outside that scope: one global
instance, structurally cross-socket, no gauge covers them.

Consistency check on the projection: r5 §6 projected 0.93–1.14 M by composing the L1+L2 ~17 %-of-svc-cycles
cut at field ρ. The field profile shows clock+strings were only ~8–9 % of the FIELD's svc cycles (the moka
term compresses everything else's share), so the delivered cut was ~8 % of service time → measured
svc/op 17.96 µs and +6.5 % IOPS. **The levers engaged exactly as designed; the projection's error was
assuming the local cycle SHARES transplant to the field.** The r5 lesson ("the emulator matches the
network; nothing emulates the CPU topology") now has its counted face.

### The chartered fix (NOT executed this session — record-don't-fix)

**L3, field-priced: make the hot-cache READ path bookkeeping-free or node-local.**
* **Code**: `src/routing.rs` — the `CachedMetadata` moka cache (serve-prelude single get + completion
  revalidate = 2 gets/op) and the `StreamLanes` String-keyed moka cache (get + `record_read_op` per op);
  `crates/fuse3` FileAttr moka cache (1.75 % svc); second order: `NvmeCache::get_static` (5.76 %).
* **Resource**: UPI cross-socket coherence on moka's shared bookkeeping lines (read ring, frequency sketch,
  entry timestamps, LRU deques) — invisible on any 1-node venue; the derivation for any replacement must be
  topology-general (`numa_core`, arbitrary domain counts) per the portability law.
* **Candidate mechanisms** (measure, in order of blast radius): (a) arc-swap'd read-mostly snapshot for the
  serve-prelude metadata probe (the D7 fold-overlay precedent — reads pay zero shared-line writes, invalidation
  rides the existing write-path publish); (b) sampled read-recording (record 1-in-N reads, N derived from
  core/topology count — never a constant) if moka stays; (c) per-NUMA-node cache instances with W1-notify-
  riding invalidation; NOT (d) a hasher swap — the u64 caches already ride ahash and the term is coherence,
  not hashing. Each lever ships with an A/B env lever + engagement gauge per convention; loom on any new
  lock-free core; A-B-B-A on BOTH venues (the local venue can only falsify regressions, not price the win —
  the acceptance row is a field row).
* **Ceiling arithmetic**: deleting ~10 of the 12 µs/op moka term takes svc/op to ~8 µs → 12-thread service
  ceiling ~1.5 M; at the measured device inflight (308 µs) and ρ ≈ 0.85 the row lands in the **1.0–1.3 M**
  class — the r5 §6 projection's band, now with the correct term named.

## 4. Bonus row 1 — fusion field confirmation (kernel lane, rand-4k WRITE 32×qd8, 60 s + 5 s ramp)

Vehicle ledger engagement exact on every leg (`fusions`/`directs` ≡ row ops on ON legs; `extractions` = the
non-fused residue; `patch_writes` carries every op — the W1 sole-owner path; `ipc_ops_write` 0 = kernel lane;
amp columns from per-leg `/proc/diskstats` data-namespace deltas, user bytes = ios × 4 KiB).

| leg | mount | IOPS | clat mean | p99 | fusions | extractions | directs | amp(data) |
|---|---|---|---|---|---|---|---|---|
| on | default — but AGED mount (post-100M-op read rows; excluded from the verdict, see below) | 176,419 | 1,448.8 | 5,276 | 11.45 M | 15.0 k | 11.45 M | 2.170 |
| off | `SQUEEZEFS_FUSE_ZC_WRITE_FUSION=0`, fresh remount | 237,561 | 1,073.8 | 8,028 | 0 | 42.1 k | 15.36 M | 2.163 |
| on2 | default, fresh remount (order-parity leg) | **458,114** | **555.1** | 1,729 | 29.59 M | 173.4 k | 29.59 M | 2.160 |
| off2 | `SQUEEZEFS_FUSE_ZC_WRITE_FUSION=0`, fresh remount | 241,189 | 1,057.6 | 8,028 | 0 | 47.3 k | 15.58 M | 2.160 |

**Verdict: fusion ON confirmed on the field — ≈ 1.9× IOPS (458.1 k vs 237.6/241.2 k) at −48 % clat and
−78 % p99 at venue parity (fresh remounts, adjacent legs both orders around on2: off → on2 → off2).** The
store-aging confound the single pair carried is FALSIFIED as the driver: the two OFF legs are flat (237.6 →
241.2 k) with a full randwrite pass of aging between them, while on2 sits between them in time at 1.9×. The
first `on` leg is DISCARDED WITH ATTRIBUTION (not fusion, not aging: it ran on the aged mount whose read
tiers held the 100M-op read rows' admissions — every write paid tier invalidation churn; the three fresh-
remount legs are the venue-parity set). Amp flat 2.16–2.17 on every leg (the fusion axis does not touch
device bytes — as designed; vehicle only). The 0.45× falsification row (D16, pre-`eb403ec8`) is RESOLVED on
the field: the fused predicate + default-ON ships as the measured-faster posture on kernel-lane small writes.

## 5. Bonus row 2 — cold seq-read 1M sentinel (60 s)

Fresh default-posture remount (tiers cold), kernel lane, 32 jobs × qd8, 1 MiB, direct=1, 60 s time_based:
**42.52 GB/s (40,549 IOPS, clat 6.27 ms)** — near the 2×200 GbE fabric ceiling; the headline sentinel holds
on this binary.

## 6. Close-out

Host left mounted at **default posture** (no env vars), `build_commit 4d033c2fd2a5`, P0 smoke
(`cp` 32 MiB + `sync` + md5, ×3) **OK ×3**, all tripwires 0 (`ipc_sessions_poisoned`,
`ipc_descriptor_rejects`, `transport_lease_overlong`, `invariant_tripwires`, `detached_task_panics`,
`writer_guard_fenced`, `fsck_findings`), no errors/panics in the daemon log. Artifacts preserved
under `/scratch/tmp/profile-0808/`. Cross-reference appended to the r5 note §6. No product code changed this
session; branch `perf/field-ingress-profile` is docs/rig-only. The carried debts stand:
`tests/run_bench_baseline.sh save` on a quiet local window, and the L3 charter above is the next fix.
