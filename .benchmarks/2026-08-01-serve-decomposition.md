# 2026-08-01 — Serve-latency decomposition: both walls named (read 10.5 ms/op fully attributed; write wall Little-closed on the op-ACK chain)

Branch `perf/serve-decomposition` (off dev tip `da837ca`, **unmerged —
the orchestrator merges**). Charter: an instrumentation + decomposition
campaign — NAME the dominant term of the two remaining walls with
counted evidence; the one code deliverable is the read-side phase
instrument. Inputs: `.benchmarks/2026-08-01-read-lane.md` §7 (the
~4 ms/op read serve residual), `.benchmarks/2026-07-31-raw-write-ceiling-resweep.md`
(write 31.3 GB/s = 0.63× the 49.7 GB/s fabric),
`.benchmarks/2026-07-31-fio-gap-accounting.md` (venue + methodology).

Commits: red `d570a64` (contracts) · green `943206d` (instrument + the
in-place-reply rewire) · this note. Field window
2026-08-01T00:38Z–01:12Z, journaled SESSION START/END + every row in
`/scratch/tmp/agent_runs.log`; artifacts `/scratch/tmp/decomp_campaign/`.

## 1. The instrument (shipped, always-on, `tests/read_serve_phase_tests.rs` 5/5)

Three histogram families, the read twin of `write_pipeline_phase_ns`
(same cost contract: one `Instant` read + one relaxed `fetch_add` per
boundary actually crossed; ungated on the stats inode; buckets through
the NEW shared `#[path]` core `crates/squeezefs-ipc/src/latency_core.rs`
— root `LatencyHistogram` and fuse3 bucket identically by construction):

| Family | Unit | Phases |
|---|---|---|
| `read_serve_phase_ns` | per data-read op | prelude · meta_resolve · key_resolve · classify_probe · sf_wait · block_fetch · binding_check · slice_out · post_validate · total |
| `read_fill_phase_ns` | per block fill | dev_queue · dev_service (NvmeBlockDev worker enq→SQE, SQE→CQE) · fetch_dma · decode · admission · deposit · fill_total |
| `read_transport_phase_ns` | per FUSE_READ over-uring | queue_wait (CQE reap→dispatch pop) · dispatch_lag (pop→handler first poll) · reply_commit · transport_total |

Containment (the no-unexplained-residue law): `total ≈ prelude +
meta_resolve + key_resolve + classify_probe + (warm: slice_out +
binding_check | cold: block_fetch + slice_out) + post_validate`;
`block_fetch ⊇ {sf_wait | fill_total + binding_check}`; `fill_total ≈
fetch_dma + decode + admission + deposit`; `fetch_dma ⊇ dev_queue +
dev_service (+ oneshot-wake residue)`; `transport_total ≈ queue_wait +
dispatch_lag + handler total + reply_commit`; **fio clat −
transport_total = the kernel-side residue**, statable by subtraction.

**In-flight fix (mis-derived-value class, found BY the instrument
design):** `Session::dispatch_with_max_write` `take()`d
`self.fuse_connection`, so `handle_read`'s P2 in-place READ reply arm
(`be82794`) was **structurally disengaged since it landed** — every READ
reply silently paid the reply-channel + reply-task hop the P2 commit had
deleted. One-word fix (`take()` → `clone()`); the new
`fuse3_read_inplace_replies` gauge is the engagement pin (a pure unit
test cannot exercise an armed session — mount-level instrument per the
repro-port exception discipline). **Field-verified: 1,694,673 in-place
replies ≡ the row's 1,694,673 READ ops, exactly.** (µs-class per op —
priced in `reply_commit` = 0.016 ms; NOT one of the walls.)

## 2. Venue (labeled once; applies to every row)

Client squeeze-test (32 CPU / 2 NUMA / dual-200GbE), out of production;
substrate reset-v3 — nullblk 4-wide over **nvme-tcp** (8 × 48 GiB
namespaces `nvme{4,6,8,10,12,14,16,18}n1`), meta `nvme{0,2}n1`,
DATA_ON_MDS=1, cache-less format, 4 MiB blocks. Instrument fio-3.36 via
`tests/fio/run_fio_row.sh` (NUMA fan-out, amp columns); every row 60 s +
10 s ramp; settle discipline between write rows = reclaim queue AND
`meta_kv_pending_free` AND mem level 0 ×3 (the venue-hygiene law).
Pair under test: `.decomp` = `943206d` (dev tip + instrument; rocky8
container build, KD-7 identity verified). Fill: fresh 32×8 g (256 GiB)
written through the mount this session (29.04 GB/s pass); read rows are
cold (cache-less volume + remount = cold RAM). Flatness evidence: 60 s
rows agreeing across legs (25.48/25.24 armed qd8) + the prior session's
120 s flat anchor at this exact shape (26.12 GB/s, per-10 s device bytes
flat) — no per-10 s sampling was taken this window (stated limitation).

## 3. READ wall — the 10.5 ms/op serve, FULLY attributed

Rows (gap_probe_read, libaio 1M nj32, kernel path, cold 256 GiB set):

| Row (order) | GB/s | clat | read_amp |
|---|---|---|---|
| qd8 armed (a1) | **25.48** | 10.497 ms | 0.709 |
| qd32 armed (a2) | 20.29 | 51.291 ms | 1.157 |
| qd8 lever-off `SQUEEZEFS_READ_LANE=0` (b1, reference) | 20.08 | 13.328 ms | 1.136 |
| qd8 armed re-check (a3) | 25.24 | 10.601 ms | 0.726 |

(armed legs bracket the off leg — order-independent; the off leg
reproduces the read-lane A0 posture on this fill.)

### 3.1 The qd8 phase table (bucket-midpoint means; 1,694,671 ops, 257,508 fills)

**Per op — sums CLOSE at every level (residue < 0.5 %):**

```
fio clat            10.50 ms
└─ kernel-side       1.86    (clat − transport_total: request formation/
                              queueing before ring delivery + completion wake)
└─ transport_total   8.64
   ├─ queue_wait     1.55    CQE reap → dispatch pop
   ├─ dispatch_lag   1.70    dispatch → handler-lane first poll
   ├─ handler total  5.37
   │  ├─ block_fetch 4.81/op (8,144 s ÷ all ops; the block-wait term)
   │  ├─ slice_out   0.39    (1 MiB memcpy into the uring payload dest)
   │  ├─ meta_resolve 0.07 · classify_probe 0.07 · prelude 0.01 · rest ≤ 0.01
   └─ reply_commit   0.02    (in-place COMMIT enqueue — the §1 fix live)
```

**Per fill (the chain block_fetch waits on; 4 MiB device reads):**

```
fill_total  7.77 ms
├─ dev_service 4.70   SQE → CQE (fabric/device service)
├─ dev_queue   1.37   worker channel + slot wait before SQE submit
├─ wake residue 1.63  (fetch_dma − dev_queue − dev_service: oneshot
│                      completion → tokio task poll)
└─ decode 0.001 · admission 0.02 · deposit 0.03
```

sf_wait (cohort waiters): 1,001,280 ops at 6.35 ms ⊂ block_fetch —
cohort dedupe working (ops/fill = 6.6; hot 405 k + hold 50 k serves).

### 3.2 THE READ TERM, NAMED

Against the raw same-shape control (6.2 ms at identical in-flight
bytes), the FS adds ~4.3 ms/op. The decomposition names it:

1. **Transport ingress queueing — 3.25 ms/op (queue_wait 1.55 +
   dispatch_lag 1.70): the dominant, previously-invisible term.** The
   op spends 31 % of its life between the ring CQE and the handler's
   first poll — per-queue serial dispatch loops (header reconstruction
   + spawn) plus tpc-lane scheduling under load. This is pure daemon
   CPU-side queueing, not fabric.
2. **Fill-issue overhead — 3.0 ms of the 7.77 ms fill (dev_queue 1.37 +
   oneshot-wake 1.63)**: 39 % of the fill RTT is client-side issue/wake
   economy, not device service (4.70 ms).
3. **Kernel-side residue — 1.86 ms/op** (by subtraction; the K1 request
   path + completion wake).
4. slice_out 0.39 ms (the 1 MiB copy — matches the near-zero-copy
   census's load-bearing-copy pricing; not a wall).

qd32 corroboration: clat 51.3 ms vs transport_total 11.5 —
**39.8 ms/op sits kernel-side** at qd32 (1,024 offered ops vs the
transport's ~256-slot appetite): deeper client qd just moves queueing
into the kernel, exactly the fio-gap "more depth makes it worse" face
(amp 1.16 here vs 1.41 pre-hold — the hold still absorbing cohort
stragglers).

## 4. WRITE wall — Little-closed on the op-ACK chain; every pipeline lever exonerated by counted A/B

Rows (exa_write_bw, libaio 1M qd8 nj32, kernel path; rm + settle + fresh
dir before every leg; the 60 s time_based row = pass-1 fresh + ~6
rewrite passes, so it measures the SUSTAINED mixed face):

| Leg (order) | Lever | GB/s | clat | in-pipe residence (total−admit) |
|---|---|---|---|---|
| w1 | governed default | 32.68 | 8.019 | 20.8 ms/block |
| w2-B | `-o max_background=1024` | 32.33 | 8.096 | 16.8 |
| w3-D | `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS=512` (2 GiB pin) | 33.23 | 7.887 | 20.0 |
| w4-A | governed default (bracket close) | 32.36 | 8.089 | 16.4 |

**Every leg Little-closes on the OP chain, not the pipeline:** 256
in-flight 1 MiB ops ÷ clat ≈ delivery on all four rows (8.02 ms → 32.7;
7.89 → 33.2; 8.09 → 32.4 GB/s). The A/Bs are decisive negatives:

* **Depth pin (w3): admission gate wide open** — admit_wait 10.3 ms →
  **0.004 ms/block** (admission_waits 115 k → 345) — and delivery moved
  +1.7 % (noise-adjacent) while **inflight bytes SELF-LIMITED at median
  507 MB** (max 1.74 GB available). `write_pipeline_depth_target` is NOT
  under-derived: the pipe cannot fill because intake = op rate.
* **max_background=1024 (w2): flat** — queueing conservation observed:
  admit_wait 3.4 → 10.2 ms (kernel-side wait moved into the daemon's
  admission park), clat identical, delivery identical.
* **Handler lanes NOT saturated**: fuse3-tpcN at 35–38 % CPU each
  (pidstat, w4); mpstat 30 usr / 43 sys / 20 iowait — the 43 % sys is
  the kernel's FUSE-ingress copy + nvme-tcp TX machinery, no core pegged.

### 4.1 The op-ACK chain (fuse_op_phase_ns[write], OP_PROFILE armed)

clat 8.02–8.10 ms = **pre-handler ~5.3 ms** (clat − handler total: the
kernel WRITE path + transport ingress — the same dispatch machinery
reads measured at 3.25 ms daemon-side + 1.86 kernel-side; the write
split is not instrumented this campaign, stated inference) **+ handler
2.7 ms** (backend 2.69: lease merge/copy + amortized admission park
0.85 ms/op at w1) + reply ≈ 0.02.

### 4.2 In-pipe residence (write_pipeline_phase_ns; runs BEHIND the ACK)

| phase (ms/block) | fresh pass (29.0 GB/s, pass-bound) | sustained mixed w1 | w4 |
|---|---|---|---|
| admit_wait | 0.001 | 3.44 | 10.30 |
| detach_lag | 0.06 | 1.46 | 1.14 |
| dma | 1.75 | 5.93 | 5.81 |
| **publish** | **0.83** | **11.04** | **7.72** |
| displaced_free | 0.00 | 1.95 | 1.38 |
| inval_tail | 0.03 | 0.22 | 0.19 |
| total | 3.03 | 24.29 | 26.73 |

The FRESH pipe is clean (3.03 ms/block). Under sustained rewrite the
residence inflates — publish 13× (0.83 → 8–11 ms; coalesce factor
collapses to 2.4 blocks/batch vs the 64 cap; journal 3.75 k entries/s,
conveyor 1.75 k passes/s), dma 3× (5.8–5.9 ms with 6.8 k reclaim
discard cmds/s contending on the same namespaces), displaced_free
1.4–2.6 ms (at-cap parks — the manners law's designed sustained-rewrite
posture: queue rides at cap, `cap_parks` 40 k/row, `cap_overflow` 0,
`sync_drains` 0). **But none of this gates delivery at this shape** —
it prices the in-flight bytes (~500–680 MB), not the op rate.

### 4.3 THE WRITE TERM, NAMED

**The wall is the WRITE op ingest ceiling: ~31–33 k × 1 MiB ops/s,
set by the per-op ACK RTT (~8 ms at 256 offered ops), whose dominant
component is the pre-handler leg (~5.3 of 8 ms — kernel FUSE ingress
incl. the K1 payload copy at 32 GB/s + the same transport
dispatch/lane queueing reads measured directly).** Offered-concurrency
scaling is already exhausted (fio-gap: plateau ≈ 30–34 for inflight
≳ 32 MiB at ANY qd/njobs mix; deeper qd inflates clat
proportionally). The fabric has 17 GB/s of headroom that only a
cheaper ACK chain (or a deeper *effective* ingest width) can buy.

## 5. Attribution quality (honest)

* Read qd8: every level closes — transport_total − Σ(parts) = 3 µs;
  handler − Σ(parts) = 12 µs (0.2 %); fill − Σ(parts) = 45 µs (0.6 %).
  The only subtraction-derived terms are the kernel-side 1.86 ms and the
  1.63 ms oneshot-wake residue — both bounded by closed sums on either
  side. **No unexplained residue above 1 %.**
* Write: the op chain closes by Little's law on all four legs (±2 %);
  the pre-handler 5.3 ms is clat − handler (measured both), but its
  kernel-vs-transport split is INFERRED from the read side's measured
  3.25 ms on shared machinery — extending `read_transport_phase_ns` to
  WRITE is the named follow-on instrument (trivial: the stamps exist,
  the family is READ-gated by one opcode check).
* Ramp-inclusive counts (60 s + 10 s ramp) — means unaffected, totals
  ≈ 1.17× a 60 s window (the standing amp-column caveat).
* One-rep legs (this is a decomposition campaign, not an acceptance
  bracket); the read armed/off sandwich and the write A-B-B-A ordering
  guard order effects, and every headline agrees with the prior
  campaigns' anchors (25.5 ≈ read-lane 25.6; 32.7 ≈ fio-gap 31.3).
* Pre-existing failure note: `read_tier_admission_tests` has 3 failures
  at dev tip `da837ca` on the dev box (reproduced with this branch's
  diff stashed — not branch-attributed; reported to the orchestrator).

## 6. Ranked targeting recommendation (the follow-on build campaigns)

1. **Transport ingress economy (BOTH walls' shared dominant term).**
   Mechanism: per-queue dispatch loop (pop → header reconstruction →
   per-op `spawn`) + tpc-lane scheduling put 3.25 ms/op on reads
   (queue_wait 1.55 + dispatch_lag 1.70) and are the prime suspect
   inside the write side's 5.3 ms pre-handler. Candidates: dispatch the
   handler future directly on the queue's lane without the
   spawn/channel hop (the ipc handoff-economy precedent), multiple
   dispatch pullers per inbound queue, opcode-specialized fast dispatch
   for READ/WRITE. Expected win: reads 10.5 → ~7.3 ms/op ≈ **+40 %
   cold-read BW (25.5 → ~34 GB/s, 0.77× raw)**; writes each −1 ms op
   RTT ≈ +4 GB/s. Acceptance instrument: `read_transport_phase_ns`
   (queue_wait + dispatch_lag < 0.5 ms at the R1 shape) + the same
   family extended to WRITE.
2. **Fill-issue economy (read-only).** dev_queue 1.37 + oneshot-wake
   1.63 = 3.0 ms of the 7.77 ms fill RTT is client-side issue/wake, not
   device. Candidates: NvmeBlockDev completion wake batching/doorbell
   (the ipc cqe_core precedent), wider worker submission, waking the
   fill's cohort directly on the reaping thread. Expected win: fill
   7.8 → ~5.0 ms ⇒ block_fetch −~1.8 ms/op ≈ +15–20 % on top of (1).
   Acceptance: `read_fill_phase_ns` (fetch_dma − dev_service < 0.7 ms).
3. **WRITE transport instrument, then the kernel-ingress question.**
   Extend the transport family to WRITE (one opcode check) and re-run
   W1: if the daemon-side share of the 5.3 ms mirrors reads (~3.3 ms),
   campaign (1) covers it; the remainder is the kernel K1 copy path
   (max_pages/payload geometry — kernel-interface work, price before
   pursuing).
4. **Publish-leg residence (write, secondary).** 8–11 ms/block under
   sustained rewrite (coalesce 2.4 vs cap 64) does not gate this shape
   but taxes in-flight bytes and any fsync-bound/deeper-pipe shape;
   worth a bounded look at per-ino conveyor batching under mixed
   rewrite load once (1)/(3) land. Not the wall today.

## 7. Client state

Standing mount (`b4edafc` pair) restored + verified; `.decomp` pair
(`943206d`) retained at `/scratch/tmp/{squeezefs,libsqueezefs_il.so}.decomp`;
artifacts (per-row job/json/stats-before-after-delta/gauges/pidstat/
mpstat/netdev) under `/scratch/tmp/decomp_campaign/`; analyzer at
`/scratch/tmp/fio_decomp/phase_means.py`; `wr` fileset (32×1 g) left in
place (normal bench artifact); store settled at SESSION END. No resets,
no reformats, no raw-device writes, no storage-node changes.
