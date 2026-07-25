# 2026-07-25 — IPC miss-path concurrency: the ring-read collapse on fabric latency

Branch `perf/ipc-miss-path-concurrency` (off dev `207e1fe`). Commits: red
`e36ddf4` (tests/preload_parity_tests.rs O_DIRECT-class rows,
aio_core kernel-lane reroute, loom `ipc_park_snapshot_before_rescan_never_strands`),
green `49e7c87` (src/ipc_host.rs, src/ipc_service.rs, src/cache/pool.rs,
src/nvme_dev.rs, crates/squeezefs-preload/{aio_core,interpose}.rs).

## 1. The field report (2026-07-25, 6-node cluster)

Rocky 8 client, nvme-tcp fabric (~235 µs device RTT), 4k O_DIRECT randread,
16 × 8 GiB files (cold-dominated): **kernel FUSE 200–280k IOPS, the
LD_PRELOAD shim 90k** — the shim ~2× slower, with a less-is-more signature
(lower thread/iodepth outperformed higher). Their counters:
`ipc_service_threads=8`, `sessions_total=4`, `binds=16`, fast-path serves
~15 %, miss demotions ~85 %. The L4 closing reference (633k device-true
libaio) was LOCAL NVMe — the miss path had never been measured against real
device latency.

## 2. Substrate (labeled; same rig as `.benchmarks/2026-07-25-odirect-randread-concurrency.md` §2)

23-CPU box, 109 GiB RAM, kernel 7.1.4-1-cachyos. Fabric-latency substrate
still up from the prior campaign: configfs null_blk `sqzlat_oss0` (36 GiB,
memory-backed, **`completion_nsec=235000`, `irqmode=2`**, 8 squeues, hw QD
128) via nvmet-loop → `/dev/nvme1n1` (data); `sqzlat_mds0` (3 GiB) →
`/dev/nvme2n1` (meta). Raw ceiling re-verified: fio libaio 16×QD16 =
**282k** (prior campaign 298k).

Filesystem: cache-less format (`sqmeta:///dev/nvme2n1 sqdata:///dev/nvme1n1`),
4 MiB blocks, 1 GiB mem cache. Mount: `--daemon --allow-other --interception
-o direct_device_true` (both transports measured on the SAME mount; KD-11
write-through active). Dataset 16 × 1.5 GiB (24 GiB ≫ 1 GiB budget ⇒ misses
dominate; il rows are 100 % handoffs by ddt design). Instruments: elbencho
3.1-10 (dynamic — loads the shim), fio 3.42, a raw-libaio spin-poll probe
(`aioprobe`, nonblocking `io_getevents` loop — no shim sleep ladder in the
detection path). Engagement per §3 rule 4 on every il row:
`ipc_ops_read` delta == row ops == `ranged_reads` delta (printed per row by
the sweep driver; a row without it is INVALID).

## 3. Reproduction (dev `207e1fe`)

| Row (elbencho, 15–25 s, ddt mount) | IOPS |
|---|---|
| kernel t16 qd16 | **231k** |
| il t16 qd16, sessions=4 | **17.5k** (13× worse — field said 2×; fabric latency + this box's budget ratio amplify it) |
| il t8 qd8 | 35.9k — **less-is-more reproduced** (t32 qd32 = 13.4k) |
| il t16 qd16, sessions=8 | 12.7k — **more sessions = worse** reproduced |
| il t32 qd1 (sync lane) | 12.6k (2.55 ms/op); `SQUEEZEFS_IL_SPINS=64` → 38.7k; `=2000000` → 5.7k |

il p50 latency 13.8 ms against a 235 µs device; delivered device
concurrency ~10 of 256 offered; daemon CPU ~4.7 cores (not CPU-bound —
a latency/convoy collapse).

## 4. The chokes (three, compounding; profile evidence per item)

Candidate list from the mission: (a) slot budget, (b) completion wakes,
(c) service-thread serialization, (d) submission batching, (e) app-side
spin. Measured outcome: (a) ruled out (`ring_full_stalls` = 0,
`slot_wait_parks` = 0, slots 1024/session ≫ 64 in flight); (d) ruled out
(spawn-per-op is µs; fio through the same path reached 220k when its ops
spread thin). The real chokes:

### 4.1 Ring reads drop the fd's O_DIRECT read class (the structural asymmetry)

The kernel path hands the routing layer the description's O_DIRECT bit on
EVERY read (`fuse_read_in.flags` → `ReadClassHint`). The ring handoff
called `fs.read(…, flags=0)` — every O_DIRECT ring read was silently
reclassified **buffered**. On the ddt mount that re-enabled the hybrid
second-touch ghost machinery the kernel path escapes
(`get_block_range_for_index`: `device_true = ddt && hint.odirect`):
whole-4-MiB escalation fetches against a 24 GiB cold set, tier churn, and
the admission traffic saturating the 235 µs device. Evidence: il rows had
`read_odirect_requests` delta = 0 and `read_device_true_reads` = 0 on a
ddt mount; the kernel row's `ranged_reads` == `read_device_true_reads`.

**Fix**: `screen_fd` captures O_DIRECT → `BindingRights::odirect`; the
handoff passes `O_DIRECT` exactly as the kernel does; ddt+odirect bindings
skip the tier fast path by policy (kernel parity — not counted as miss
demotions); shim-side `F_SETFL` toggling O_DIRECT unbinds (class is
per-description, captured at bind; the kernel serves the live class after).
Pinned: `ring_read_carries_the_bindings_odirect_class`,
`ddt_mount_odirect_binding_skips_the_tier_fast_path`
(tests/preload_parity_tests.rs; red at `e36ddf4`). Fix alone: 17.5k → 42k.

### 4.2 Sub-block reads bounced through 4 MiB pooled buffers (the 6 ms/op convoy)

After 4.1 the il row sat at 42k with `read/backend` phase p50 ≈ 6 ms while
the device idled between bursts (10 ms-resolution inflight trace: 0 0 0 17
71 0 …). perf (dwarf): **40.6 % of daemon CPU in `kernel_init_pages`**
under `io_uring_enter → io_read → bio_iov_iter_get_pages → gup →
do_huge_pmd_anonymous_page` — first-touch THP zeroing of FRESH read
buffers, plus `smp_call_function_many`/`flush_tlb` (munmap IPI storms
across 23 CPUs).

Root: `read_block_with_dest(…, dest=None)` takes its bounce buffer from
`ALIGNED_BUF_POOL` — **4 MiB backings, capacity ≈ cores×16 = 368**. A 4 KiB
ranged read checks one out for its whole op; at miss-path concurrency
(256+ in flight, latency inflated by the zeroing itself) the pool drains
to 0 (sampled `mem_budget_components.aligned_buf_pool.current = 0` under
load, `sheds=0`) and ~0.4 of ops fresh-allocate 4 MiB: mmap + THP-zero +
gup fault + free per op — a self-sustaining convoy (slow ops ⇒ more in
flight ⇒ more misses). The kernel transport never touches this pool: its
reads land in registered payload buffers via `dest_addr`. That's why only
il collapsed. (fio round-robin across 16 fds/job partially passed through
to kernel FUSE — its 220k "il" row was a blend; the engagement check
caught it, and the 16-process spin-probe row at 38.7k total with zero
client sleep confirmed the daemon-side convoy.)

**Fix**: size-classed read bounce — new `RANGED_BUF_POOL` (64 KiB × 
max(cores×64, 512) ≈ 92 MiB; own R5 component `ranged_buf_pool`, floor
4 MiB, weight 2), `read_bounce_pool(size)` routes windows ≤ 64 KiB there;
`AlignedBufOwner` now recycles into its **home** pool (a cross-pool recycle
would hand a 64 KiB backing out as 4 MiB — heap overflow; pinned by
`read_bounce_pool_routing_and_home_recycle` in tests/nvme_dev_tests.rs);
`aligned_pool_hits` wired (the miss RATIO is the convoy instrument: ≈ 0.4
during the collapse, ≈ 0 healthy). Fix: 42k → **266k**.

### 4.3 Service-park lost wakes (chokes b/c — the 5 ms tails and the sessions inversion)

bpftrace on the service threads under load (pre-fix): **15 % of futex
parks expired at the 4–8 ms bound** with servable ops published. Two
protocol holes in `service_loop`:

1. The `FUTEX_WAIT(doorbell, observed)` admission value was loaded **after**
   the pre-park rescan — a submission racing the rescan's tail folds its
   bump INTO `observed`, the wait admits, and the op strands for the full
   `SERVICE_PARK_MAX` (5 ms). Fixed: snapshot **before** the rescan (loom
   `ipc_park_snapshot_before_rescan_never_strands`; weakening evidence:
   permuting to snapshot-after-rescan fails with "published entry
   stranded").
2. A thread owning multiple sessions parked on only the FIRST session's
   doorbell — wakes on every other owned session were lost entirely (the
   sessions=8 < sessions=4 inversion; sessions > service threads is the
   normal fleet posture). Fixed: `futex_waitv(2)` across ALL owned
   doorbells (≤ 128; ENOSYS degrades to the old single wait — same 5 ms
   bound).

Post-fix parks expiring by timeout: 2.6 % (residual = legitimate idle
expiry between bursts).

### 4.4 Design-board item: `io_submit` -EAGAIN under slot exhaustion

Not the root cause here (slots never exhausted), but fixed as chartered:
a ring-eligible iocb that gets no session slot (or a poisoned session) now
**reroutes to the kernel lane at its batch position** — the real call on
the bound fd is always correct (§5.4.2) — instead of surfacing `-EAGAIN` /
truncating the prefix. Ordering preserved (the rerouted op joins the open
kernel run, which any later ring op flushes first). Pinned:
`ring_refusal_reroutes_to_the_kernel_lane_never_eagain`
(crates/squeezefs-preload/tests/aio_core_tests.rs; red at `e36ddf4`).

## 5. A/B (fixed `49e7c87` vs dev `207e1fe`; quiet box, engagement exact per il row)

Headline (elbencho t16 qd16, 15 s rows, medians of 3 — 60 s rows are
impossible post-fix: 16 × 1.5 GiB / 4k = 6.29 M ops exhausts in ~20 s at
these speeds; the counted 15 s protocol keeps rows time-based on both
binaries):

| Row (elbencho, ddt mount) | dev `207e1fe` | fixed `49e7c87` | Δ |
|---|---|---|---|
| kernel t16 qd16 | **304.3k** (308/304/295) | **305.5k** (316/306/302) | — (unchanged) |
| il t16 qd16, sessions=4 | **38.2k** (38.2/41.6/37.7) | **265.6k** (265.6/267.6/265.6) | **+595 %** — 0.06× → **0.87× of kernel** |

Sweep (fixed binary, 10 s rows, sessions=4): monotone in both axes — the
inversion is gone:

| t\qd | 8 | 16 | 32 |
|---|---|---|---|
| 8 | 112.6k | 205.5k | 263.0k |
| 16 | 206.0k | 268.9k | 304.6k |
| 32 | 269.7k | 303.5k | **316.7k** (≈ kernel 320.6k) |

Sessions at t16 qd16 (fixed): 1 → 281.6k, 4 → 268.9k, 8 → 262.5k,
16 → 264.4k (note: `SQUEEZEFS_IL_SESSIONS` clamps 1..=8; "16" runs as 8) —
the sessions=8-worse inversion is gone (was 12.7k).

Other shapes (fixed vs dev):

| Row | dev | fixed |
|---|---|---|
| il sync t32 qd1 (default adaptive spins) | 12.6k | **68.4k** |
| 16 × spin-poll probe processes (16 sessions, qd16 each) | 38.7k | **120.2k** |
| single spin-poll probe (qd16, 1 fd) | 41.8k @ 382 µs | 34.1k (run variance band; see residuals) |
| il qd1 RTT (elbencho t1, libaio + sync) | — | **271 µs avg** (kernel 313 µs — the miss-path per-op cost is already better than the kernel round trip) |

**Warm fast path unregressed** (mission constraint): default interception
mount (no ddt), 4 × 200 MiB warm set, elbencho sync t8 rand-4k: dev 19.5k
vs fixed 18.5k, identical serve mix (fast-path serves + handoffs deltas
match) — inside run variance for this shape.

## 6. What this means for the field box

- The shim's miss path now delivers ~0.87× the kernel path at the same
  offered concurrency on 235 µs fabric (was ~0.06× on this rig, ~0.5× in
  the field), and more threads/iodepth once again help.
- Their per-run tells after upgrading: `aligned_pool_misses` per-op ratio
  ≈ 0 (vs ~0.4), `read_odirect_requests` ≈ ring reads (class carried),
  service futex timeout rate ~0.
- **Separate finding, out of scope here**: the DEFAULT (hybrid) mount
  collapses to ~12k on this cold-dominated shape **on the kernel path
  too** (ghost-escalation whole-block admissions churning against a
  budget ≪ working set: `ranged_read_ghost_escalations` +6.3k/12 s, each
  a 4 MiB fetch + eviction). `-o direct_device_true` is the correct
  posture for beyond-budget random O_DIRECT working sets today; the
  escalation-vs-budget interaction belongs to the read-path program
  (R1b/R5).

## 7. Residuals (recorded, not chased)

- **Multi-process/multi-session ceiling**: 16 single-fd processes reach
  120k vs 266k for 16 threads in one process — per-session drain overhead
  and the 5 ms park quantum still shape many-session fleets. Next lever if
  needed: per-thread drain batching or a sessions-per-thread admission cap.
- **Client reap granularity (libaio lane)**: `aio_reap_served` detects
  completions by poll ladder (2 yields then 200 µs sleeps), and
  `AioCtxState::getevents` accounts pass time nominally
  (`waited += kwait.max(1)` against non-blocking probes). Post-fix this is
  no longer the binding term (spin-poll probes and elbencho now agree
  within variance); a futex_waitv-over-pending-tickets park would delete
  the residual sleep quantum if the many-session fleet posture needs it.
- **Single spin-probe regression band** (41.8k → 34.1k, one run each,
  different mount sessions): inside the burst-variance band of this rig;
  worth a counted median if anyone leans on single-ctx qd16 shapes.
- The dev-baseline il rows varied 17.5k–42k run-to-run (the convoy's
  burst dynamics); the table's dev medians are the counted quiet-box runs.
- `fuse_op_phase_ns.read.backend` p50 for il misses post-fix sits in the
  ≤1024 µs bucket (was ≤16 ms), matching the kernel path on the same mount.

## 8. Substrate teardown

Same as the prior campaign (§8 there): disconnect the two nvmet-loop
subsystems, unlink port 52126, rmdir nvmet objects, power-off + rmdir the
`sqzlat_*` configfs null_blk items. Left up while this branch is under
review (RAM-backed, reboot-ephemeral).
