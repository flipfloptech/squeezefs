# Design: the end-to-end performance audit — reads, writes, DLM (the finality program)

**Status: DRAFT Rev 1** (program opened 2026-09-02). Evidence base: the four
read-only ledgers the mapping agents produced on 2026-09-02 — hereafter
**the 2026-09-02 read / write / DLM / apparatus ledgers** — and the
**baseline of record** taken the same afternoon on squeeze-test
([`.benchmarks/2026-09-02-e2e-audit-baseline.md`](../.benchmarks/2026-09-02-e2e-audit-baseline.md)).
The ledgers' fat boards and gap lists are reproduced in Appendices A–D;
every mechanism claim below carries its `file:line` at dev tip `dafa82ca`;
every number carries its instrument and tier. Where a number does not
exist, this document says so rather than estimating one.

Companion: [the baseline note](../.benchmarks/2026-09-02-e2e-audit-baseline.md)
(the baseline table, per-row engagement/tripwire summary, first-day finding
evidence, the what-to-run-next list). Governing laws:
[`AGENTS.md`](../AGENTS.md) (§Non-Negotiables, §Benchmarks & Profiling);
evidence tiers: [`docs/rc-manifest.md`](rc-manifest.md).

---

## 0. Mandate — and what "finality" means operationally

User mandate (verbatim, 2026-09-02):

> I want to run an end to end performance audit on reads, writes, and the
> distributed lock manager. I want to get performance to the Nth degree of
> finality... everything should be benchmarked/checked/changed from end to
> end on all read/write paths.

This program is the organized form of `AGENTS.md`'s terminal requirement
("Performance is the only terminal requirement", ruling 2026-08-01): every
mechanism on the read, write and lock paths holds its slot on **measured
evidence**, and displacement happens by **counted A/B**, never by
convenience or by frustration.

**Finality, operationally**, is reached for a path when ONE of two states
holds for every stage of it:

1. **At the physical floor** — the stage's measured cost is within the
   instrument's resolution of the physical term it wraps (wire payload
   rate, device service time, one irreducible kernel copy, one DMA), with
   the floor itself stated and cited; or
2. **A counted adjudication** — the residual above the floor is named,
   measured, and declared load-bearing (the near-zero-copy census's
   precedent, `.benchmarks/2026-07-31-near-zero-copy.md`: the two remaining
   hot-path copies were *declared* load-bearing — the ring sever is the
   isolation boundary, the merge is what the lease-severance law needs —
   and their COST was cut instead).

Anything in neither state is an open lever, and this program's job is to
drive every open lever to one of the two states. The **campaign order**
(§3) is the merged fat board of the three plane ledgers, re-ranked across
planes by expected field gain × confidence.

**The landing law for every lever** (no exceptions; restated from
`AGENTS.md` §Benchmarks & Profiling so it cannot be argued per PR):

| Requirement | Source law |
|---|---|
| **A-B-B-A** alternating-order bracket, both orders cited | standing comparison rule, 2026-07-27 |
| on **both substrates** — loop for the latency-shaped decomposition, **tcp for every fabric-sensitive row** (writes, bandwidth, multi-connection) | the two-substrate rule, `tests/dev_substrate.sh` |
| a **sustained ≥ 60 s row**, flat across the window, as THE claim; bursts label-only | sustained-state rule, 2026-07-29 |
| **amplification columns** on every write/bandwidth row — device÷user bytes on the DATA namespace, `wareq-sz` vs block size, the `block_free_*` reclaim family | write-amplification instrument, 2026-07-27 |
| **engagement exact** — the row's stats-inode deltas account for the row's ops (the charter-rule-4 law), else the row is INVALID | L4 closing, `.benchmarks/2026-07-19-l4-interception-closing.md` |
| **instrument stated** with the row (fio / elbencho static or dynamic / `squeezefs bench`), because unaligned buffers split WRITEs and static elbencho cannot load the shim | instrument-alignment lesson |
| **evidence tier stated** — measured-real / measured-simulated / arithmetic-on-measured-constants | `docs/rc-manifest.md` (mandatory on every scale claim) |
| the **field row** on squeeze-test as the final verdict; **no cloud run without expressed approval for THAT run** | cloud mandate 2026-08-28 |

A lever that lands on a single-order delta, a burst, a loop-only fabric
row, an engagement-unverified row, or a midpoint-estimated mean (§1) has
not landed — it is scoping evidence and is labeled as such.

---

## 1. The honesty preconditions — the apparatus gaps

The apparatus ledger's structural finding governs everything else: **no
mean this repository has ever quoted from a `*_phase_ns` family is exact.**

- Every `*_phase_ns` family is a **26-bucket power-of-two µs histogram with
  no sum and no count** (`crates/squeezefs-ipc/src/latency_core.rs:19-37`;
  the root `LatencyHistogram` at `src/fuse_client.rs:4204-4266` is 26
  `AtomicU64` and `to_json` emits `{label: count}` only). A "mean" derived
  from it is a **bucket-midpoint estimate with up to 2× per-bucket error**,
  and a containment law such as `total ≈ Σ phases` cannot be checked
  exactly. Every phase number in Appendices B–D (3.25 ms of 10.5 ms;
  4.8 ms/block; 0.78 ms vs 0.17 ms; 23 ms publish total) is such an
  estimate and is marked ≈.
- **No end-to-end per-op stitch exists.** Every cross-layer attribution
  (fio clat − `read_transport_phase_ns.total` = kernel residue;
  `fio clat − ipc_direct_phase_ns.total` = client + ingress) is a
  **subtraction of independent histograms**, never a join on one op.
- The **write side has no `dev_queue`/`dev_service` split**
  (`write_pipeline.dma` is the whole `write_block`); the read funnel has
  one.
- **Metadata ops have no always-on phase family** — `fuse_op_phase_ns`
  exists only behind `SQUEEZEFS_OP_PROFILE=1`.
- **Lock instrumentation records WAITS for some classes and HOLD for none**
  (`LockClass` at `src/fuse_client.rs:2777-2794` has block / meta / device
  / commit / zc_extract and **no DLM 4a class**); the 3.5
  `INODE_META_LOCKS` wait is measured only inside the publish pass while
  **20+ acquisition sites are unmeasured**; 4b `pass_leaf_locks` lumps the
  pure wait with the locked window.
- **No uniform CPU column** — CPU/byte and CPU/op are per-campaign pidstat
  captures with no stats-inode face.
- The **DLM microbenches cover primitives** (`authorize_dma`, fencing
  reads, local acquire — all at floor, Appendix D) **but not the M7 pass,
  the shipped-verb RTT, or the custody grant end-to-end**.
- **`.benchmarks/criterion-baselines/reference.json` is a month stale**
  (stamped `66bb4775`, 2026-08-03 — before the rip-tokio-TOTAL sweep, the
  overlay program, the kvmap tree). The nightly compare has been measuring
  against a binary that no longer exists.

Two consequences bind the program:

1. **No lever is adjudicated on a midpoint mean.** A campaign PR's A/B is
   valid only once the phase families it cites carry exact `sum_ns`/`count`
   (PR A1) and — for any claim that attributes time ACROSS layers — once
   the per-op trace ring can join the stages on one op (PR A2). Until then,
   the ledger numbers rank levers; they do not accept them.
2. **The instrument PRs precede the campaign PRs** (§5). **PR A1 is
   already in flight** (branch `perf/audit-instruments`): exact
   `sum_ns`+`count` on every histogram (root + fuse3 sharded, fold exact
   across shards, bucket labels byte-identical, new keys beside old); the
   write `dev_queue`/`dev_service` split + the journal write split
   (`journal_ring_write` / `journal_prefix_wait` / `journal_barrier`, with
   `pass_journal_write` kept as their sum); the 3.5 wait+hold via ONE
   Drop-recording guard change in `src/stripe_locks.rs` covering every
   site, plus `leaf_lock_wait_ns` and `dlm_guard_hold_ns`; always-on
   `meta_op_phase_ns`; the `daemon_cpu_ns` + by-class CPU column; and the
   `reference.json` refresh (`tests/run_bench_baseline.sh save`). **PR A2**
   is the per-op trace ring + kernel tracepoint join + the client-side
   completion stamp + the end-to-end DLM benches (§5).

The read ledger's **governing caveat** is the sharpest instance of the
precondition: **every read number in the record predates the 2026-08-13
rip-tokio-TOTAL sweep** ([`docs/design-sqz-sync.md`](design-sqz-sync.md)
§Rip-tokio-TOTAL), which
put first-party primitives on every device read and every cohort wait, and
**no read row has priced them** (§4, finding candidate 48).

---

## 2. The baseline of record

**Venue:** squeeze-test (the EXA field box): 5 storage nodes over
**nvme-tcp**, memory-backed (nullblk) NVMe targets; client 32-core Xeon
6426Y, 251 GB RAM, 2×200 GbE; **cacheless mount with `--interception`**.
**Binary:** `aecf1561`. **Window:** 2026-09-02 14:53–15:10. **Instrument:**
fio, 24 jobs, **30 s + 10 s ramp** per row (the field job files at
`/scratch/tmp/fio_jobs/`), two modes per row — `kern` (kernel FUSE-over-
io_uring) and `il` (`LD_PRELOAD=libsqueezefs_il.so`). **Artifacts:**
`/scratch/tmp/e2e-baseline-20260902-145345/` — pre/post `.stats` snapshot
per row + fio bw logs. **Tier: measured-real.** **These are NOT sustained-
60 s rows** — every row below is labeled **baseline-30 s** and may not be
cited as a sustained claim; the sustained re-run is item 2 of the
what-to-run-next list.

### 2.1 The physical floors (with sources)

| Floor | Value | Source | Caveat |
|---|---|---|---|
| **Wire payload** (client 2×200 GbE, nvme-tcp practical line rate ~90 % ifutil) | **44.7 GB/s = 41.6 GiB/s** payload | [`.benchmarks/2026-08-15-overlay-b4-overwrite.md`](../.benchmarks/2026-08-15-overlay-b4-overwrite.md) line 119 (sar: 22.05–22.14 GB/s TX per port, both ports 90.3–90.7 % ifutil) | measured in the TX (write) direction; the wire is full-duplex so the RX (read) direction has the same capacity, but RX pays one irreducible kernel copy per byte (a CPU term, not a wire term — `AGENTS.md` §Zero-copy) |
| **Raw 4 KiB IOPS** (fio direct against the fabric namespaces) | **2.486 M** (p99 4.5 ms) … **3.36 M @ 372 µs** | [`.benchmarks/2026-08-08-shim-reap-fanin.md`](../.benchmarks/2026-08-08-shim-reap-fanin.md) line 272; [`.benchmarks/2026-08-05-ingress-queue-spread.md`](../.benchmarks/2026-08-05-ingress-queue-spread.md) line 59 | two same-fleet re-grades a week apart; the range is the honest floor until re-measured on today's target posture |
| **Device RTT** (4 KiB, qd1) | **~235 µs** | [`.benchmarks/2026-07-25-ipc-miss-path.md`](../.benchmarks/2026-07-25-ipc-miss-path.md) line 11 | a 2026-07-25 measurement on a Rocky 8 client — **stale**: the il `rr_4k` row's 0.22 ms mean clat is BELOW it, so today's unloaded RTT is unknown (open question §7.1) |
| Standing raw-fio seq ceilings (read 41.8 GB/s, write ~42–44 GB/s) | — | [`.benchmarks/2026-08-05-post-wave-numbers.md`](../.benchmarks/2026-08-05-post-wave-numbers.md) | **superseded**: the il `r_cold` row (40.35 GiB/s = 43.3 GB/s) exceeds the standing read ceiling; the wire payload is the only floor used below |

The **% of floor** column divides GiB/s rows by **41.6 GiB/s** and IOPS
rows by the **2.486–3.36 M** range. (The mandate's working figures of
"83–90 % of wire" for reads and "≈ 70 %" for rewrites divided GiB/s rows by
the 44.7 GB figure; the unit-consistent values are given here.) Mixed rows
get no single floor: the two directions are duplex and the fio mix ratio,
not the wire, bounds each half.

### 2.2 The rows

| Row (job) | kern | il | % of floor (kern / il) | Note |
|---|---|---|---|---|
| **w_fresh** (24 × 8 GiB fresh seq write) | **0.36 GiB/s**, clat 986 ms, p99 2.4 s | **0.36 GiB/s**, clat 988 ms | 0.9 % / 0.9 % | **FINDING 46** — stream collapse at the kvmap crossing (§4.1) |
| **r_cold** (seq read, cold) | 36.88 GiB/s, 10.13 ms, p99 38.0 ms | 40.35 GiB/s, 9.21 ms, p99 43.8 ms | **88.6 % / 96.9 %** | il within 3 % of the wire payload |
| **r_repeat** (seq read, repeat) | 36.15 GiB/s, 10.34 ms | 40.34 GiB/s, 9.25 ms | 86.8 % / 96.9 % | cacheless mount — repeat ≡ cold by construction |
| **rr_4k** (rand read 4 KiB) | 441.5 k IOPS, 0.43 ms, p99 4.95 ms | 873.8 k IOPS, 0.22 ms, p99 0.54 ms | 13.1–17.8 % / 26.0–35.1 % | of the raw range |
| **w_rewrite** (seq overwrite of the kvmap files) | 32.26 GiB/s, 11.52 ms, p99 49.6 ms | 30.48 GiB/s, 12.08 ms, p99 50.6 ms | 77.5 % / 73.2 % | the same files that collapsed fresh |
| **rw_4k** (rand write 4 KiB) | 476.2 k IOPS, 0.40 ms, p99 1.55 ms | 704.0 k IOPS, 0.27 ms, p99 1.32 ms | 14.2–19.2 % / 21.0–28.3 % | W1 patch path (`patch_writes` 55.3 M end-of-run) |
| **mix_bw** (mixed seq) | r 23.45 + w 10.05 GiB/s @ 5.0 ms | see artifacts | n/c (mix) | per direction: r 56.3 % / w 24.1 % of one-direction wire |
| **mix_4k** (mixed rand 4 KiB) | r 212.7 k + w 91.1 k IOPS | r 536.0 k + w 229.6 k IOPS | 9.0–12.2 % / 22.8–30.8 % | aggregate IOPS vs the raw range |
| **w_durable** (seq write, `end_fsync`) | 31.00 GiB/s, 11.88 ms | 31.01 GiB/s | 74.5 % / 74.5 % | kern ≡ il |

**Arithmetic readings** (tier: arithmetic-on-measured-constants — flagged,
to be confirmed against the job files):

- **Every row is closed-loop at ≈ 190 ops / ≈ 380 MiB in flight.** Little's
  law on the table: `rr_4k` 441.5 k × 0.43 ms ≈ 190, 873.8 k × 0.22 ms
  ≈ 192; `rw_4k` 476.2 k × 0.40 ≈ 190, 704.0 k × 0.27 ≈ 190 (consistent
  with 24 jobs × iodepth 8); `r_cold` 36.88 GiB/s × 10.13 ms ≈ 382 MiB, il
  40.35 × 9.21 ≈ 380 MiB; `w_rewrite` 32.26 × 11.52 ≈ 380 MiB (consistent
  with 24 × 16 × 1 MiB — the block size and depth are INFERRED here and
  must be confirmed against the job files, not restated from this table).
  **At fixed concurrency, throughput is 1/latency: every GiB/s and every
  IOPS this program wins is a latency term removed**, which is why the
  phase families — not throughput — are the levers' instruments.
- The **il-vs-kern 4 KiB gap is entirely latency at equal depth**: 0.43 →
  0.22 ms (read), 0.40 → 0.27 ms (write). The kernel-path residue
  (`fio clat − read_transport_phase_ns.total`) is the read ledger's fat #2
  (Appendix B).
- **The 4 KiB rows sit at 13–35 % of the raw device range** while the
  bandwidth rows sit at 73–97 % of the wire: **the IOPS plane is where the
  distance to the floor is**, and both the read #1 mechanism (per-op
  process-global mutex + tombstone, §4.3) and the write W1/overlay
  per-op economies (Appendix C #7) are per-op costs.

### 2.3 End-of-run gauges (cumulative over the whole run; per-row deltas are in the artifacts)

| Gauge | Value | Reading |
|---|---|---|
| `map_migrate_inos` | 144 | 6 × 24 — six 24-file sets crossed into the kvmap tree (which six: per-row deltas) |
| `kvmap_partial_inos` | 0 | no partial crossing left behind |
| `meta_kv_block_refs_drift` | 0 | the C8 must-stay-0 held |
| `invariant_tripwires` | 0 | held |
| `transport_lease_overlong` | **1** | fired ONCE — on the f46 row: a write-handler invocation exceeded 1 s under the collapse (the loud-never-fatal tripwire behaving as designed) |
| `fuse_op_watchdog_overdue` | 0 | held |
| `*_fence_drops` (the write-pipeline / rewrite-shadow must-stay-0 pair) | 0 | held |
| `ipc_ops_read` / `ipc_ops_write` | 57.4 M / 38.6 M | the il rows' engagement instrument in aggregate — per-row accounting against fio's op counts is the artifacts' job (charter rule 4) |
| `overlay_ack_early_stores` | 6.98 M | the B4 overwrite arm engaged on the rewrite/mixed rows |
| `patch_writes` | 55.3 M | W1 sole-owner patch — the `rw_4k` + `mix_4k` write rows |
| `write_through_blocks` | 201.8 k | complete-block write-through on the fresh/rewrite streams |

Tripwire verdict: **clean except `transport_lease_overlong = 1`, which is
attributed** (f46 row). No fence drop, no drift, no invariant tripwire, no
watchdog.

---

## 3. The merged fat board — one campaign order across planes

The three plane ledgers each ranked their own findings. This section merges
them and re-ranks **across planes by expected field gain × confidence**,
where *gain* is the ledger's stated number where one exists (else the
mechanism's reach), and *confidence* is HIGH when the mechanism is cited in
code and the number is measured, MEDIUM when the mechanism is cited but the
number is a midpoint estimate, LOW when the number does not yet exist.
Every item names its **adjudicating A/B** — the row that accepts or
refuses it under the §0 landing law.

**Cross-plane merges made here** (the ledgers name them separately; they
are one mechanism):

- **Write #5 ≡ DLM #2 + DLM #3.** "Publish conveyor commit-rate coupling
  (4.8 ms/block saturated, 0.68 ms wake residue)" and "the conveyor pass is
  one serialized server incl. the device write (ρ ≈ 0.92, 0.78 ms vs
  ~0.17 ms leaf-lock floor)" + "wake-hop inflation (spawn-per-leadership
  pass task, ~0.68 ms residue)" are the SAME M7 conveyor
  (`src/meta_backend/kv/backend.rs:7498-7512` spawns the pass on
  `try_lead`; `conveyor_pass_task` at :7547). One campaign (**C-1/C-2**)
  serves the write plane's publish tax, the authority's verb ceiling AND
  f46's post-crossing publish train.
- **Write #2 ⊂ Write #1 (f47).** Small-bs sequential O_DIRECT riding the
  overlay per-op (the `wareq-sz` collapse) is cured by the same derived
  length floor.
- **Read #4 and Read #5 are one lever** (the zc serve — only zc can delete
  the warm-serve pass that the 2026-08-04 "A1 warm-serve split" decision
  declined to build a tier-buffer handoff for; that "A1" is the read-audit
  board item, not PR A1 of this program).

### 3.1 Tier 0 — findings with fixes in flight (bug class; no A/B needed to land, field row needed to close)

| # | Finding | Plane | Branch | Closing row |
|---|---|---|---|---|
| F46 | fresh-stream collapse at the kvmap crossing (§4.1) | W / M | `fix/f46-kvmap-stream-publish` | `w_fresh` both modes ≥ the `w_rewrite` row of the same files (32 GiB/s), sustained 60 s; `layout_publish_batched_blocks / batches` ≫ 1.0 post-crossing |
| F47 | overlay B4 arm before `try_extent_park` with no length floor — sub-cap patch-ineligible writes at ~1,024× each way (§4.2) | W | `fix/f47-overlay-length-floor` | rand-4k on hole / clone-shared / decorated files: `overlay_gap_seed_old_bytes` ≈ 0, `extent_parks` accounts for the row, amp columns ≤ the RW-program's 15–26× regime |

### 3.2 Tier 1 — structural limits and the highest gain × confidence

| Rank | Item (ledger id) | Mechanism (cited) | Gain / confidence | Adjudicating A/B |
|---|---|---|---|---|
| **1** | **DLM F-A — the owner executes a shipped frame SERIALLY** (DLM #1) | `src/meta_ship/service.rs:673-690` `run_batch`: `for op in ops { out.push(self.run_op(..).await) }` — one conveyor commit per mutating verb. The **9,473 verbs/s authority ceiling = 1 / (owner per-verb serial latency)**, and it is the root of the **≈ 2.6 GiB/s co-writer ingest wall** the S9-a note published as writer-count-independent | **2–10× on fan-out** (ledger) / HIGH mechanism, MEDIUM number (midpoint) | K-co-writer fan-out fleet (`tests/mw_fleet.sh`, `tests/run_mw_matrix.sh`), A-B-B-A on tcp devsub then squeeze-test; `meta_ship_owner_phase_ns` (sum/count) + `meta_txpass_phase_ns`; verdict = verbs/s per authority and aggregate ingest GiB/s, engagement `shipped ≡ served` |
| ~~**2**~~ | **The conveyor is one serialized server incl. the device write** (DLM #2 ≡ W #5) — **MECHANISM LANDED 2026-09-03 as D-2** (`.benchmarks/2026-09-03-d2-two-stage-conveyor.md`): the pass is the APPLY stage (submits, never waits), a per-volume durability lane awaits / prefix-waits / barriers per group and acks in journal order. Fleet A-B-B-A (D-1b's rig): ρ(apply) 1.00 / 0.93 → **0.125 / 0.140**, `pass_total` 725 → 105 µs, `tx_queue_wait` −55 %, hwm 22–24 windows, crash contracts green — **aggregate ingest PAR** (B2/A2 +0.3 %; B1/A1 −33 % at the session's highest load, unreproduced by the reversed bracket) | the binding term on the co-located fleet was never the device: `journal_ring_write` per window is a ~1.0–1.3 ms MEAN with a 50–200 µs MODE — the `uring_fs` completion hop chain under CPU saturation, HOL-blocking the in-order lane; batches also shrank (size-1 78 → 86 %, +18 % submissions) once the device wait stopped pacing the pass | the "~4×" was the pass's headroom, and the pass HAS it (ρ 0.13); converting it is #3's | in-process (`uring_fs::arm_device_latency`): 16-committer closed loop 10.9k → 21.4k tx/s at D = 500 µs (queue 670 → 56 µs), 3.2k → 7.0k at 2 ms, strict 4.2k → 6.3k; `pass_journal_write` split shows the device/hop term alone in the lane. The "`meta_commit_group_size` up" expectation was wrong for this lever (it removes the wait arrivals accumulated in) |
| ~~**3**~~ | **Wake-hop inflation on the conveyor** (DLM #3, the W #5 residue) — **MECHANISM LANDED 2026-09-03 as C-2** (`.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md`): `uring_fs_write_phase_ns` split the journal write's round trip at its two thread boundaries and the same-binary fleet bracket measured the shared-lane `wake_hop` at **746 / 664 µs of 1,209 / 1,130 (62 % / 59 %)**; every writable volume now runs both conveyor stages on its OWN lane (`sqz-jrnl{N}`, derived one-per-volume) that owns the volume's journal io_uring and parks in it (`LanePark`, `OwnedReactor`) — `wake_hop` → 53–107 µs, `journal_ring_write` −34…−57 %, its ≥ 4 ms tail 15–18 % → 6–8.5 %, `window_lane_wait` −50…−66 %, commit latency −7…−49 %, four brackets agreeing; `SQUEEZEFS_JOURNAL_LANE=0` is the A/B control | the residue is the **io-wq punt** a buffered block-device write takes (`blkdev_write_iter` refuses NOWAIT buffered writes — no inline completion on any venue): `device` 380–650 µs mean / 32 µs mode, two kernel wakes per window under a box at load 85 | **aggregate ingest PAR** (−3.0 / −2.3 / +1.6 / −4.9 %): the conveyor stopped binding the row when D-2 landed — commit latency (1.4 ms) is ≈ 5 % of the co-writer's 25–30 ms per-block publish, whose terms are its own layout-conveyor `queue_wait` 7–9 ms, `save_encode` 7–9 ms (CPU starvation) and the owner's `dispatch` hop 2.0–2.3 ms per verb (**DLM #7 — the same wake-hop class one plane over, now the owner's largest term**) | in-process (release, lever on / off): quiet 86.2 k / 74.6 k tx/s; 4 × 2 ms serve bursts on `sqz-meta` 35.7 k / 2.45 k, `journal_ring_write` 63 / 3,187 µs; D-2's rows on the fix 88.3 k / 26.9 k / 7.5 k / 6.8 k vs 50.5 k / 21.4 k / 7.0 k / 6.3 k. Lever (c) batch shaping NOT landed (the ledger does not convict it: size-1 groups 84 → 87–89 %, passes par) |
| ~~**4**~~ | **Read #1 — the `sqz_time`/`sqz_channel` mutex class on every device read** (finding candidate 48, §4.3) — **MEASURED 2026-09-02, NOT FAT at the field posture; retired from Tier 1** (`.benchmarks/2026-09-02-r1-device-read-executor.md`) | `src/nvme_dev.rs:2426` wraps every `NvmeBlockDev::read_block` in `sqz_time::timeout` — but on the sqz kernel **neither field mode reaches it** (kern → the FUSE-zc direct leg, il → direct-drive; **0 arms per 27.9 M field kernel reads**). The per-op arm is `InboundQueue::pop`'s ticked recv on the fuse3 fork's OWN `sqz_time` registry (a second lock/heap/thread): 0.35–0.45 arms/kernel READ. The lane mpsc is crossbeam (lock-free, 12 ns); the oneshot's mutex is uncontended and a lock-free oneshot is not faster | **measured**: ≤ 3 % daemon CPU kern / ≈ 0.5 % il, < 0.2 % of per-op latency; the registry lock's knee (0.8–1.0 M arms/s, microbench) sits at ≈ 1.8–2.2 M kernel IOPS ≥ the device ceiling | adjudicated load-bearing-at-cost; the residual (timer-thread wakeup coalescing ≈ 1.1–1.5 % CPU; a timer-less inbound park) joins the Tier-3 economy batch (R-5 / the transport-economy PR). On the stock-kernel `SQUEEZEFS_FUSE_ZC=0` posture the named site engages (1.14 arms/op, ≈ 5–6 % CPU) but `dev_queue` = 500 µs (read #3) dwarfs it |
| **5** | **Write #3 — the overlay has no depth governor** | the B4 overlay arm admits stores open-loop; the ledger measured **0.68–0.80× on device-bound venues**, field-neutral | field-neutral today, so the gain is venue protection + the governor pattern the write pipeline already owns (`ProbeCore`) / HIGH mechanism, HIGH number on the losing venue | tcp devsub (the device-bound venue) A-B-B-A rewrite rows, then squeeze-test must stay ≥ par; `overlay_*` residence family (a gap — Appendix C) lands with it |
| **6** | **DLM F-B — the cluster wire is thread-per-connection** | `src/cluster_wire.rs:1464` `max_connections_from(cpus) = (cpus×16).clamp(64, 1024)`; one OS thread per accepted connection (`:2060`) → **≤ 1,024 readers or ≤ ~340 co-writers PER AUTHORITY by construction** (3–4 planes per node); 15 k members = 15 k OS threads | a **capability limit**, not a throughput number: 15 k requires ≥ 15 authorities (readers) / ≥ 45 (co-writers) by arithmetic on the ledger's constants; gain at today's fleet sizes = 0 / HIGH mechanism | membership fleet at N ≫ 1,024 on the SIM-1 harness (measured-simulated) + a real-mount N = 512/1,024/2,048 ladder on one box: connection count, RSS, renewal latency; lever = multiplex planes per node (×3–4) then a poll/uring venue that removes the thread term |

Sequencing inside Tier 1: **#1 first** (it moves a published number the
program is already accountable for — the S9-a wall), **#2/#3 as one
campaign** immediately after (the same conveyor serves f46's shape, so the
f46 fix's field row doubles as its baseline), **#4 as measure-first in
parallel with A1** (its instrument IS A1), **#5** on the tcp venue, **#6
last in the tier** because it buys headroom no current fleet uses.

### 3.3 Tier 2 — large measured terms with a named lever

| Rank | Item | Cited term | Lever | Adjudicating A/B |
|---|---|---|---|---|
| 7 | **Read #2 — transport ingress** — **LANDED 2026-09-03 (R-2, `850ce8b0`; `.benchmarks/2026-09-03-r2-read-fast-dispatch.md`)**: kern rand-4k 24×8 +14.8 % IOPS A-B-B-A (446 → 512 k), 514 k sustained 60 s flat, `queue_wait` exactly 0, ingress 141 → 51 µs per op, 1 MiB seq par, p99.9 par; the K1 tail attributed to the parked worker's wake path (`transport_reap_gap_ns`), not attacked | `read_transport_phase_ns` `queue_wait + dispatch_lag` = 158 µs of 439 at 4 KiB 24×8 (exact; the 2026-09-03 attribution) | READ fast-dispatch from the reap thread: sync probe + inline commit (warm) / direct lane mint homed on the queue's CPU (cold) | `rr_4k` 24×8 kern A-B-B-A + same-binary lever control; `transport_fast_dispatch_demotes ≡ fuse3_read_inplace_replies`; `queue_wait` Σ = 0 |
| ~~8~~ | **Read #3 — fill-issue economy** — **LANDED 2026-09-03 as R-3** (`.benchmarks/2026-09-03-r3-fill-issue-economy.md`) | the zc-leg instrument (`zc_bridge_phase_ns` msg_hop / sq_wait / device_cq / wake_hop / total) named the kern rand-4k bridge's ≈ 126 µs software half as THREE cross-thread wakes around a 40 µs DMA; the funnel's `dev_queue` term was the lane worker parking on `submit_and_wait(1)` with the request channel outside the ring | (a) the NvmeBlockDev arrival wake IN the ring (red-first: a fast fill beside a 400 ms slow fill took 378.7 ms → device speed; `dev_enters`/`dev_fills`/`dev_wake_batches`); (b) armed READs fused onto the queue worker's lane (`SQUEEZEFS_FUSE_ZC_READ_FUSION`, default on; the D16 machinery) — msg_hop 32 → 0.6 µs, K2+K3 158 → 83, `transport_wake_writes` 1.5 → 0.48/op | field A-B-B-A kern rand-4k 24×8 **+15.5 % / +16.0 % sustained** (441 → 510 k, p99 −66 %, CPU/op −27 %), il par, 1 MiB seq par; the next term is the fused pass's run-queue wait (`queue_wait` 83 + `wake_hop` 101 of 247 µs) |
| 8b | **The reap thread's per-op serialization (R-3 §9.5's wall) — MEASURED 2026-09-03 as R-4** (`.benchmarks/2026-09-03-r4-reap-thread-economy.md`) | perf on `f3-ur*` at the field: 17.7 µs/op, 63 % kernel (the COMMIT's own work incl. 1.9 µs of fuse lock contention, the fetch's nvme-tcp submit 2.5, two lane wakes 1.7), 37 % daemon (pass loop 2.1, the R-2 probe ladder 1.8 for 0 serves, instruments 1.0); 0.76 parks/op, 84 % ≤ 32 µs | the worker CPU diet (sharded per-op counters, one net-delta pend RMW per pass, a period-clocked deadline scan, a single-read eventfd drain) **landed**: kern rand-4k +0.7…+2.4 % (four brackets), CPU/op −3…−4 %, pass work −25 %; the adaptive spin-before-park **measured NOT fat and ships off**: 69 % of parks deleted, `msg_hop`/`device_cq`/`wake_hop` unmoved, +5.5 % CPU/op — the worker's ingress terms are its kernel residence per enter + run-queue standing at 75–80 % box busy, not its park | next levers named: the sqz-kernel COMMIT lock split, the worker's scheduling class, the probe population gate (R-5) |
| 9 | **Write #4 — zc O_DIRECT extraction** | ≈ 100 % of O_DIRECT bytes pay the extraction pass; CPU 26.7 → 39.5 (ledger unit: j/GiB) | one kernel pass (sqz-kernel `FUSE_URING_ZERO_COPY` v2 fusion) | `w_rewrite` O_DIRECT A-B-B-A + `daemon_cpu_ns` per GiB; sqz-kernel box only |
| 10 | **DLM #4 — 4a stripe collisions** | the 4a guard is **held across the whole commit park** by design (D5: queue entries co-own their tx's `DlmGuard`s until terminal outcome), so every collision on a stripe is a full-commit wait. The ledger says "4096 stripes"; at HEAD the DLM's OWN stripe tables are **1024-way** (`LAST_GRANT_FLOOR` `src/dlm.rs:1033`, `LOCK_WAITERS` `:1079` — a release wakes its whole stripe) while the 4096-way tables are the 3.5 `INODE_META_LOCKS` (`src/routing.rs:35`) and `BLOCK_LOCK_STRIPES` (`src/fuse_client.rs:2192`) — which table the collisions live in is exactly what the missing histogram must convict | derive the convicted table's width from `possible_cpus × q_depth`; **land the `LockClass` DLM histogram first** (A1's `dlm_guard_hold_ns` + the 4a wait) | mdstorm create/rename/unlink at 24–64 writers; `dlm_guard_hold_ns` + 4a wait sums |
| 11 | **Write #6 — exclusive inode guard on fresh/append streams** — **LANDED 2026-09-05 (W-2, `.benchmarks/2026-09-05-w2-write-stream-guard.md` §7: tcp devsub A-B-B-A par-or-up, exclusive stream waits 16,384 → 241 per leg)** | the P1-8 half is ACQUITTED by the new hold instrument: the stream's guard drops before block I/O (in-process release: Σ hold over K = 8 concurrent extends under a 200 ms block-window stall < 1 stall; hold per write ≈ µs). The measured term is the MODE — the exclusive meta-prep of a qd-N stream on one ino serializes N writers through a FIFO wake chain for a RAM-only snapshot | the Shared class widened to every cache-resident striped write (`SQUEEZEFS_WRITE_GUARD_NARROW`, default on; `0` = the v1 mapped-within-EOF class) | `w_fresh` (post-f46) qd16 A-B-B-A, same binary via the knob; columns `write_lock_wait_{exclusive,shared}`, `write_lock_hold_*`, `write_pipeline_phase_ns.lock_wait` |
| 12 | **Read #4/#5 — the whole-box CPU wall + the warm-serve pass** | **2.69 passes/byte** cold whole-block (Appendix B table); the warm-serve pass (`read_copy_warm_serve_bytes`) has no software lever left — only zc deletes it | zc serve (`READ_FIXED` into folios) on the sqz kernel | `r_cold` CPU/byte via `daemon_cpu_ns`; `read_copy_*` closure |
| 13 | **DLM #6 — free-grace ack cadence** | recycle **65 MiB/s vs 2.2 GiB/s** churn (`docs/design-free-grace-sustain.md` — campaign ON but unaccepted) | accept the sustain campaign's rows | the free-grace sustain rig, `free_grace_reader_acks` moving, `forced_releases = 0` |
| ~~14~~ | **Write #8 — reclaim constants at fleet rates** — **MECHANISM LANDED 2026-09-05 as W-4 (derivation + instrument); FIELD MEASURED — §5 of the note: on the shipped posture (discard elision on) the reclaim queue is OFF the rewrite path, so the premise "the 1 s park quantum IS the tail" is WITHDRAWN and the 2026-09-01 field tail is UNATTRIBUTED (open)** (`.benchmarks/2026-09-05-w4-reclaim-derivation.md`): the at-cap park is event-driven (woken by the drain's per-range `finish_free`; the cap counts queued + in-flight) and both constants derive — `bound = clamp(4 × batch ÷ measured drain rate, 50, 1000 ms)` (fleet under load ≈ 140 ms; the shipped 1 s is the ceiling), `cap = clamp(displacement rate × room latency, 4096, budget/1024 ÷ entry RAM)`; in-process release rows A-B-B-A (32 producers vs a 20 ms-stalled 400 blocks/s serial drain, cap 64): park p99.9 **1,000 ms → 80.6–80.8 ms** (= the bound in both legs; 12.4×), max 1,000 → 80.8 ms, `cap_overflow` 5 → 618–626 (the pinned regime's designed tradeoff), `drain_rate` gauge 399–405 vs the seam's 400 | `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS` default 4096 fills in ≈ 0.2 s at fleet rewrite rates; the 1,000 ms `CAP_PARK_MS` quantum IS the p99.9 tail — a parked producer holds its pipeline permit | the cap is NOT the fleet lever (the drain cannot keep pace there at any finite cap — the floor governs at 4 MiB blocks); the bound + the room edge are | **OWED**: `w_rewrite` A-B-B-A on tcp devsub, same binary, knobs pinned `4096`/`1000` vs derived: GiB/s, p99.9, `block_free_reclaim_{cap_parks,cap_overflow,drain_rate,park_bound_ms}`, `write_pipeline_phase_ns.displaced_free` |
| 15 | **Write #9 — fsync economy** | `fsync` (`src/fuse_client.rs:24474`) flushes ALL data namespaces + 3 serialized meta legs; **no `fsync_phase_ns`** | instrument first (gap), then flush only touched namespaces, parallel meta legs | `w_durable` + a small-file fsync storm; new family lands with it |

### 3.4 Tier 3 — per-op economies (low each; batched per plane)

| Item | Term | Lever |
|---|---|---|
| Read #6 | kernel READ handler **8 allocs/op + ~20 global atomics + ~9 clock reads** | `block_key_in` by-ref at `src/routing.rs:16145` (`load_striped_block_keys`), inline epochs, per-lane counter shards |
| Read #7 | ranged read per-op string re-parse | parse once per binding |
| Read #8 | kvmap partial resolve: `parse_kvmap_head` per call, `String` clones, **O(4096) allocs per window fill** | typed head, borrowed keys, arena per window |
| Read #9 | R2 residency: 5 probes per decision | one probe |
| **Read #10** | **deposit under the shard write lock — the ledger flags it CORRECTNESS-class.** The finding-17 fix (`.benchmarks/2026-08-26-read-tier-deposit-window.md`) deliberately moved deposit validation UNDER that lock; whether the ledger's item is a residual of that class or the lock's hold-time cost is the first thing to establish | triage as bug class first: red-first repro if a window exists; a perf PR only if it is hold time |
| Write #7 | per-WRITE **4 Box + 4 Arc slot closures, per-block `String`s, ~30 atomics** | stack closures, `StackKey`, sharded counters (the op-economy suite's law) |
| Write #10 | gap seeding re-reads the whole old block | ranged seed of the uncovered span only |
| DLM #7 | owner `spawn_meta_join` hop (`src/meta_exec.rs:147`) ≤ 32 µs vs ≤ 8 µs read execute | execute on the accepting lane |
| ~~DLM #8~~ | stop-and-wait frame depth 1 — **LANDED 2026-09-02 as D-1b** (`.benchmarks/2026-09-02-d1b-publish-plane-batching.md`): the publish plane frames concurrent publishes (N calls per `PublishRequestFrame`, schema 13) and pipelines `SQUEEZEFS_PUBLISH_SHIP_DEPTH` frames per authority on a session pool; the owner co-queues a frame's independent inos (F-A's chains). In-process 24 concurrent publishes: 24 frames / 24 passes / 122 µs each → 1–5 / 3–5 / 26 µs (release); local fleet A-B-B-A (8 co-writers × 24 streams): frames/publish 1.00 → 0.61–0.63, owner passes −39 %, journal entries −24 %, per-block publish latency −74 %, aggregate PAR with the authority's conveyor at ρ ≈ 0.97 — the wall moved onto #2 | the SINGLE-CONNECTION half (frames multiplexed on one socket, owner read-ahead) + the wire's 100 ms `ACCEPT_POLL_TICK` stay with D-5 |
| DLM #9/#10 | wire copies / allocs (LOW) | after #1–#8 |

### 3.5 At the floor — leave alone (the DLM ledger's floor table)

| Primitive | Measured | Verdict |
|---|---|---|
| `authorize_dma` | 1.3 ns | floor |
| fencing reads | 12–22 ns | floor |
| local `acquire` | 340 ns | floor |
| membership renew | 30–38 ns | floor |
| alloc-lane admission | 0 delta vs unpartitioned | floor |
| journal entry framing | 34 ns | floor |

---

## 4. Findings opened on the audit's first day

### 4.1 Finding 46 — the fresh-stream collapse at the kvmap crossing (fix in flight: `fix/f46-kvmap-stream-publish`)

**Row:** `w_fresh`, both modes, squeeze-test baseline (24 × 8 GiB, 30 s +
10 s ramp). **Observed** (bw logs + per-row `.stats` deltas +
`publish_phase_ns` dump in the artifacts): the write ran **28.7 GB/s for
≈ 7 s**, then **all 24 files crossed into the kvmap tree at 14:54:22**
(`map_migrate_inos` +24), and per-job bandwidth fell to **8–12 MB/s for the
remaining 33 s**. The reported row mean (0.36 GiB/s, clat 986 ms, p99 2.4 s)
is the post-collapse regime — the burst sat inside the 10 s ramp.
**38,587 publishes = one per 4 MiB block**; `publish_phase_ns.total ≈ 23 ms`
with `meta_commit ≈ 17.8 ms` of which **≈ 16 ms is unattributed inside the
commit** (midpoint estimates — and this is precisely a §1 case: most of the
38,587 publishes happened pre-crossing at burst rate, so the mean hides a
bimodal distribution); `layout_publish_batched_blocks / batches = 1.0`;
`publish_commit_groups` ≈ all size 1. **The REWRITE of the same kvmap
files runs 32 GiB/s** (`w_rewrite`), so the collapse is specific to the
post-crossing EXTEND shape, not to kvmap resolution.

**Arithmetic readings** (flagged): 8–12 MB/s per job is **≈ 0.3–0.5 s per
4 MiB block per stream**; ×24 ≈ 0.2–0.3 GiB/s aggregate, consistent with
the 0.36 GiB/s row mean carrying a tail of the burst. 38,587 blocks × 4 MiB
≈ 151 GiB, ≈ 5.6 s at the burst rate — the trickle contributed almost
nothing.

**Hypothesis under test:** post-crossing extend publishes run the
**whole-map diff train O(map) per block** — the kvmap design's own §3
promise ("a 64-block window ≈ 4 KiB of journal") is not what the extend
path executes ([`docs/design-kvmap-block-map-tree.md`](design-kvmap-block-map-tree.md#3-mechanics),
the Publish bullet).
`transport_lease_overlong = 1` on this row is the same event seen from the
transport (a write-handler invocation > 1 s).

**Closing evidence required:** `w_fresh` both modes ≥ the same files'
`w_rewrite` (32 GiB/s), **sustained 60 s** across the crossing;
`publish_phase_ns` with exact sums showing `meta_commit` attributed;
`layout_publish_batched_blocks / batches ≫ 1` post-crossing; the
red-first cargo contract (the crossing venue in
`tests/`, the finding-45 pins' sibling) pinning per-block publish cost
O(batch) after the crossing.

### 4.2 Finding 47 — the overlay B4 arm reopens the small-write amplification regime (fix in flight: `fix/f47-overlay-length-floor`)

**Mechanism** (write ledger fat #1, HIGH): the device-overlay B4 overwrite
arm (`src/fuse_client.rs:14704` — the `overwrite_shape` screen, gated only
by `overlay_overwrite_enabled()`) runs **before** the W2 extent-overlay
park (`try_extent_park`, called at `src/fuse_client.rs:16659`, defined at
:14440) **with no length floor**. A **patch-INELIGIBLE sub-cap write** —
hole / clone-shared / decorated shapes, which W1 refuses by its 6-clause
ledger — therefore **mints a fresh 4 MiB CoW destination and reads the
4 MiB old image per 4 KiB write: ≈ 1,024× each way.** Field-observed in
`.benchmarks/2026-08-17-mw-shipped-free-c8-fix.md:30`:
`overlay_gap_seed_old_bytes +25,141,248 = 6 × 4 MiB − 24,576 user bytes` for
six sub-block writes. **This is the ~2,500× regime the random-small-write
program closed** ([`docs/design-random-small-writes.md`](design-random-small-writes.md),
closing [`.benchmarks/2026-07-17-rand-write-program-closing.md`](../.benchmarks/2026-07-17-rand-write-program-closing.md))
**reopened for every shape W1 does not take.** Write #2 (small-bs sequential O_DIRECT
riding the overlay per-op — the `wareq-sz` collapse) is the same hole.

**Fix (in flight):** a **derived** length floor on the overlay arm —
`len ≥ patch_max_bytes` (= `block_size/8`, the `SQUEEZEFS_PATCH_MAX_BYTES`
derivation, `src/env_knobs.rs:146`) — so sub-cap shapes fall through to
the W2 park exactly as the RW program designed. **Closing evidence:**
rand-4k on hole / clone-shared / decorated files, both substrates,
A-B-B-A: `overlay_gap_seed_old_bytes` ≈ 0 and `extent_parks` accounting for
the row's ops; amplification columns back in the 15–26× fold regime (the
RW program's measured number); the red-first contract in the
write-through/overlay suites. The baseline's `rw_4k` rows are NOT this
shape (fresh sole-owner files → W1, `patch_writes` 55.3 M) — the f47 venue
must be built deliberately.

### 4.3 Finding candidate 48 — the per-read timer + channel mutex class (MEASURED — NOT FAT, closed)

**Adjudicated 2026-09-02 (R-1, `perf/r1-device-read-executor`,
[`.benchmarks/2026-09-02-r1-device-read-executor.md`](../.benchmarks/2026-09-02-r1-device-read-executor.md)):
the candidate does NOT promote.** The measurement (uprobes on both
`Sleep::new` symbols + `cpu-clock` profiles + the new `timer_*` /
`transport_timer_*` gauges and `sqz-timer` / `fuse3-ur` CPU classes, tcp
devsub then squeeze-test) found: (1) the named site `nvme_dev.rs:2426` is
**cold on the sqz-kernel field posture in both modes** — kern rand-4k
rides the FUSE-zc direct leg (`zc_device_fetch`, a worker-side deadline
and a plain oneshot, no per-op timer), il rides the direct-drive engine;
the field kern row ran `ranged_reads = 0` over 27.9 M reads; (2) the
per-op timer arm that does exist is **`InboundQueue::pop`'s ticked
`mpsc::recv` park** (`fuse_over_uring.rs:1412-1435`) on the **fuse3 fork's
own `#[path]`-shared `sqz_time` registry** — a second global lock / heap /
`sqz-timer` thread — at 0.35–0.45 arms per kernel READ (1.12 per 1 MiB
READ): the exact "per-pull timer registration" L3 lever C retired, brought
back by the rip-tokio-TOTAL sweep through `ticked()`; (3) the lane channel
is crossbeam (lock-free, 12 ns/op) and the oneshot's per-channel mutex is
uncontended (a lock-free oneshot measures slower) — both "mutex" items
retire; (4) cost: `sqz-timer` class 1.14 % (kern) / 0.44 % (il) of daemon
CPU on the field, the whole class ≤ 3 % / ≈ 0.5 %, and < 0.2 % of per-op
latency; the registry lock's contention knee (0.8–1.0 M arm cycles/s per
registry, `read_fill_executor_prims`) sits at ≈ 1.8–2.2 M kernel IOPS —
at/above the field device ceiling. Below the ≥ ~5 % bar ⇒
load-bearing-at-cost. Residual levers (Tier 3, not landed): timer-thread
wakeup coalescing (the thread's cost is a ≈ 200 k wakeups/s stream, not
the pops), a timer-less inbound park. The stock-kernel `SQUEEZEFS_FUSE_ZC=0`
posture does pay the named site (1.14 arms/op, ≈ 5–6 % CPU) but its
`dev_queue` = 500 µs is read board #3's term and dwarfs it. The original
hypothesis text follows for the record.

**Not yet a finding**: a finding needs a number, and this one has a
mechanism and an arithmetic estimate only. **Mechanism** (read ledger fat
#1): since the 2026-08-13 rip-tokio-TOTAL sweep, **every device read** goes
through `squeezefs_ipc::sqz_time::timeout(read_timeout_ms, rx_oneshot)`
(`src/nvme_dev.rs:2426`; 30 s default at `:91-97`) — which `Box::pin`s the
future (`crates/squeezefs-ipc/src/sqz_time.rs:240`), registers a deadline
in a **process-global `Mutex<Registry>`** (`:69`) over a `BinaryHeap`
(`:55`), and on the normal completion path leaves a **tombstone** the
service thread later skips (`:192`) — plus a per-channel `Mutex` on every
fill's lane mpsc (`nvme_dev.rs:2404`) and per oneshot (`:2395`), and the
same `timeout` per 50 ms cohort-wait slice (`src/routing.rs:9299`,
`WAIT_SLICE` at `:9167`). **Arithmetic** (ledger): at field rand-4k rates
≈ 1.2 M process-global mutex ops/s and ≈ 190 MB of tombstones. **Unpriced
by any read row** — every read number predates the sweep.

**Why it ranks Tier 1 without a number:** a process-global lock on the
IOPS plane's hottest per-op path is the exact class `AGENTS.md`'s
latch-free law forbids on the hot path, and the 4 KiB rows are the rows
farthest from their floor (§2.2). **Why it is not yet a finding:** the
measured 4 KiB IOPS today (441 k / 874 k) are ABOVE the pre-sweep record
(364–393 k / 438–531 k, `.benchmarks/2026-08-05-post-wave-numbers.md`),
so the sweep's cost is hidden inside a net gain and can only be priced by
instrument, not by history.

**The measurement** (the A/B that promotes or retires it): A1's
`dev_queue` exact sums + `daemon_cpu_ns_by_class`; `perf record` on the
`fuse3-tpc*` / `sqz-ipc-svc*` lanes during `rr_4k` (kern and il) with the
`sqz_time`/`sqz_channel` symbols' share; then the lever — a timer-less
per-lane deadline watchdog (the lane already owns its in-flight set; a 30 s
deadline needs no per-op heap entry) and SPSC completion rings — A-B-B-A
on loop (latency-shaped) and tcp, verdict = `rr_4k` clat at fixed depth +
CPU/op. If the share is < the instrument's resolution, the item is
adjudicated load-bearing-at-cost and closed.

---

## 5. The campaign ladder (PRs)

Each rung is one PR: **red contract or counted A/B + fix + field row**, in
the merged order of §3. Branch names are proposals; the plane letters are
R (read), W (write), D (DLM/metadata), C (the shared conveyor).

### 5.1 Instruments first

| PR | Branch | Content | Status |
|---|---|---|---|
| **A1** | `perf/audit-instruments` | exact `sum_ns`+`count` on every histogram (root + fuse3 sharded, fold exact, labels byte-identical); write `dev_queue`/`dev_service` + journal write split; 3.5 wait+hold via the one `stripe_locks.rs` guard change, `leaf_lock_wait_ns`, `dlm_guard_hold_ns`; always-on `meta_op_phase_ns`; `daemon_cpu_ns` + by-class; **`tests/run_bench_baseline.sh save` refreshes `reference.json`** (apparatus items 1, 3, 4, 5, 6, 9). Contracts are export/histogram LAWS, never perf numbers; the alloc-economy suites prove zero hot-path allocations added; per-record cost of A and D measured in ns | **IN FLIGHT** |
| **A2** | `perf/audit-trace-ring` | per-op trace ring (op id stamped at ingress, stage stamps appended lock-free, dumped on the stats inode under a census-class knob) + kernel tracepoint join (`fuse:*`, `io_uring:*`, `nvme:*` by op id/tag); the client completion stamp (IPC slot, the `stamp_ingress` v5 precedent); end-to-end DLM Criterion benches (M7 pass at the D4 shape, shipped-verb RTT, custody grant) (apparatus items 2, 7, 8) | **trace ring LANDED** (item 2): `crates/squeezefs-ipc/src/op_trace_core.rs` (the `Stage` vocabulary — every stage is the END boundary of the phase beside it — the Lamport SPSC ring, the stride-independent sampling law), `fuse3::op_trace` (the one ring set, the task-scoped current op the session binds at every handler spawn), `squeezefs::op_trace` (derived geometry, the il ticket law, the `.trace` drain); stamps sit INSIDE every `*_phase_record`, on the NvmeBlockDev request, on `QueuedTx`/`QueuedPublish`/the pipeline upload/`DataOp`; `SQUEEZEFS_OP_TRACE` + the `op-trace` admin verb; `tests/op_trace_stitch.py` stitches a dump against the exact histograms (containment ratio 1.00 on `pass_total`/`tx_queue_wait`) and joins `fuse:fuse_request_send/end` by `unique`; hook cost 0.3–0.6 ns disarmed / ~5 ns armed (`op_trace_hook` rows); contracts `tests/op_trace_tests.rs`. The client completion stamp (item 7) and the E2E DLM benches (item 8) remain open. |

### 5.2 Fixes in flight (bug class)

| PR | Branch | Closes |
|---|---|---|
| F-47 | `fix/f47-overlay-length-floor` | §4.2 |
| F-46 | `fix/f46-kvmap-stream-publish` | §4.1 |

### 5.3 The merged campaign order

| Order | PR | Plane | Item | Acceptance (beyond the §0 law) |
|---|---|---|---|---|
| 1 | `perf/owner-concurrent-verbs` — **concurrent-dispatch half landed** (D-1, `.benchmarks/2026-09-02-d1-owner-concurrent-dispatch.md`; publish plane D-1b); **group-per-frame rung LANDED, field A-B-B-A RUN on both venues — +6 % full-save ingest on squeeze-test, par elsewhere, no loss; `.benchmarks/2026-09-04-d1c-fleet-squeeze-test.md`** (D-1c, `perf/d1c-conveyor-group-per-frame`, `.benchmarks/2026-09-04-d1c-conveyor-group-per-frame.md`) | D-1 | F-A: dispatch a frame's mutating verbs concurrently so they co-queue into ONE M7 pass. **Residual rung (measured 2026-09-03/04, closed in-process 2026-09-04):** since C-2's instant-drain apply pass the co-queue was arrival-spread ÷ pass-latency — 4–8 passes per 64-verb frame (D-1 note), 3–14 per 24 publishes (C-2 note item 8). D-1c makes **one conveyor group per shipped frame** structural on the PUBLISH plane: `ConveyorCore::enqueue_many` (one queue-lock acquisition — a drain never sees a partial group, loom-modeled), `KvMetaBackend::commit_tx_group`, `RoutedMetaBackend::set_layout_and_size_group` (one canonical `lock_many` per volume over the members' inos), and the owner's ROUND dispatch (a frame's `SetLayoutAndSize` calls prepared concurrently under the round's serve stripes, committed as one group; other verbs served beside them; chain order kept). The natural-row contract is assertable again (`a_framed_burst_is_one_owner_conveyor_pass_by_construction`: passes ≤ frames, no held pass). Lever `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP` (`0` = D-1b's dispatch); gauges `meta_conveyor_group_{commits,txs}`, `meta_ship_publish.frame_groups`. **Next rung**: the S8 verb plane's `run_batch` and the `MergeLayoutAndSize` (Lever-B) path stay per-call | K-writer fan-out: verbs/s per authority and ingest GiB/s vs the S9-a wall; `owner_phase_ns` sums; `shipped ≡ served`; fsck + C8 clean. Rung: `META_CONVEYOR_LEADER_PASSES` per served frame → 1 on the fleet rig (in-process release: 1.00 with the lever on vs the venue ratio off), journal entries unchanged (one tx = one entry — pinned by replay equality), A-B-B-A on tcp devsub (`.benchmarks/rigs/2026-09-04-d1c-fleet-lever-abba.sh`) then squeeze-test |
| ~~2~~ | `perf/d2-two-stage-conveyor` — **landed 2026-09-03** (`.benchmarks/2026-09-03-d2-two-stage-conveyor.md`) | C-1 | DLM #2 ≡ W #5: apply stage / durability stage, N journal writes in flight, in-order acks; one tx = one entry unchanged | fleet A-B-B-A: ρ(apply) 1.0 → 0.13, queue wait −55 %, aggregate PAR (the hop term binds — C-2); the handoff reuses the loom-modeled `ConveyorCore` verbatim (`loom-models/` did not build at the D-1b tip for a foreign reason — the re-run is owed) |
| 3 | `perf/conveyor-resident-pass` | C-2 | DLM #3: resident pass task on `sqz_notify`, fan-out on the committer's lane | `tx_queue_wait` sums; the 0.68 ms residue gone |
| 4 | `perf/r1-device-read-executor` (was `perf/read-fill-timerless`) | R-1 | candidate 48: **measured → NOT FAT, closed** (§4.3; `.benchmarks/2026-09-02-r1-device-read-executor.md`) — instruments + microbench landed, no lever; the residual joins R-5 / the transport-economy PR | done: `sqz_time` share in perf ≤ 3 % kern / ≈ 0.5 % il on the field, < 0.2 % of clat |
| 5 | `perf/overlay-depth-governor` | W-1 | W #3: `ProbeCore`-governed overlay admission + the `overlay_phase_ns` residence family | tcp devsub rewrite rows ≥ par (was 0.68–0.80×); field ≥ par |
| 6 | `perf/wire-multiplex` | D-2 | F-B: multiplex planes per node, then the poll/uring accept venue | N-ladder 512/1,024/2,048 on one box; SIM-1 at 15 k (measured-simulated, labeled) |
| 7 | `perf/read-fast-dispatch` (run as `perf/r2-read-fast-dispatch`) | R-2 | R #2: READ dispatch from the reap thread — **LANDED** (`850ce8b0`; `.benchmarks/2026-09-03-r2-read-fast-dispatch.md`): +14.8 % kern rand-4k A-B-B-A, 514 k sustained, ingress 141 → 51 µs; `transport_reap_gap_ns` landed first (the K1 instrument) | done: `queue_wait` Σ = 0, `dispatch_lag` 79 → 40 µs, `transport_total` 319 → 215 µs exact; the tail (K1) is the next campaign's, attributed to the parked worker's wake path |
| 8 | `perf/fill-poll-cohort` (run as `perf/r3-fill-issue-economy`) | R-3 | R #3: the zc-leg instrument first, then READ fusion onto the queue worker + the NvmeBlockDev arrival wake in the ring | `zc_bridge_phase_ns` sums + `dev_enters`/`dev_fills`; field kern rand-4k +15.5 % — **LANDED** `.benchmarks/2026-09-03-r3-fill-issue-economy.md` |
| 9 | `perf/dlm-stripe-derivation` — **LANDED 2026-09-05 (mdstorm A-B-B-A: 4a collisions −76 %, 4a wait −58 %/op, throughput par)** (`.benchmarks/2026-09-05-d3-dlm-stripe-derivation.md`) | D-3 | DLM #4: the stripe-collision CENSUS first (`<table>_stripe_collisions` vs `<table>_key_waits` on all six striped tables + `lock_phase_ns.dlm_guard_wait`) — it acquitted the board's suspects (the 1024-way `LOCK_WAITERS`/`LAST_GRANT_FLOOR` pair: spurious wakes, never waits; `INODE_META_LOCKS`/`BLOCK_FLUSH_LOCKS`: 0 on a metadata storm) and convicted the 4a `DlmLockManager` tables (4096-way, held across the commit park): in-process 32-writer many-dirs rename 2.9 % of ops pay a false-sharing commit wait at 4096, 11.2 % at 1024, 0.78 % at 16,384 — exactly 1/W; the DLM-class widths now derive `next_pow2(max(shipped, 16 × possible_cpus × q_depth))` (`SQUEEZEFS_DLM_STRIPES` explicit; 16,384 on the 32-CPU field box), and the one 4a scoping the D5 law permits landed (guards drop BEFORE the ack — the self-wait a committer paid on its own previous tx, 11 % of many-dirs renames) | **OWED**: `tests/run_mdstorm.sh` A-B-B-A same binary, `SQUEEZEFS_DLM_STRIPES=4096` (shipped control) vs derived; columns ops/s per phase, `dlm_guard_wait` Σ/p99, `dlm_guard_hold` p99, the 4a census |
| 10 | `perf/write-stream-guard` — **LANDED 2026-09-05 (tcp devsub w_fresh A-B-B-A: par-or-up both aged brackets, exclusive stream waits 16,384 → 241 per leg)** (`.benchmarks/2026-09-05-w2-write-stream-guard.md`) | W-2 | W #6: the hold instrument (`write_lock_hold_{shared,metaprep,entire}`) ACQUITTED the premise — the stream's order-1 guard was already dropped before block I/O (Σ hold over K concurrent extends < one stall window; the only EntireOp per fresh file is its promotion); what remained was the MODE: a qd-N stream hands one exclusive guard down an N-deep wake chain to serialize a RAM-only snapshot. Landed: the Shared class widened to every cache-resident striped write (extends + hole-fills; `SQUEEZEFS_WRITE_GUARD_NARROW`, `0` = the v1 class) — the convoy design's §4.4 row-15 (a) proof delivered | `w_fresh` qd16 A-B-B-A on tcp devsub, same binary via the knob — OWED (columns in the note) |
| 11 | `perf/read-zc-serve` | R-4 | R #4/#5: `READ_FIXED` into folios (sqz kernel) | CPU/byte; `read_copy_*` closure; zc-capability gate |
| 12 | `perf/zc-odirect-extraction` | W-3 | W #4 | `w_rewrite` O_DIRECT CPU/GiB |
| 13 | `perf/free-grace-accept` | D-4 | DLM #6: accept the sustain campaign — **LANDED, FIELD OWED** (`.benchmarks/2026-09-05-d4-free-grace-sustain.md`): the campaign's five levers were already in the tree (PRs 1–4, 2026-08-25); D-4 closed the §3 rate equation IN-PROCESS on one deterministic clock — the shipped levers unbind a recycle-bound stream (80.0 MiB/s flat, 0 stalls vs 72.1 / 179; `bound_age` 8.6 s vs 11.8 s; forced = fences = 0), Little's law `rate ≈ spare ÷ bound_age` holds on live gauges, and the 2026-08-30 GREEN cloud row's 23–29 s `bound_age` is the UNCOUPLED routine composite (demand dark by design), not a campaign miss; finding D4-1 (the re-based runway's fleet-rate divisor asks the floor on mid-supply lanes — inventory, not throughput) priced and pinned | owed: the from-zero s11-mpiio row on the finding-15 venue with the sustain columns (the parent runs it; command + verdict columns in the note) |
| 14 | `perf/reclaim-derivation` — **LANDED 2026-09-05 as derivation + instrument (`.benchmarks/2026-09-05-w4-reclaim-derivation.md` §5); the tail claim is withdrawn: with discard elision on the reclaim queue is off the rewrite path, the 2026-09-01 field tail is UNATTRIBUTED (open)** | W-4 | W #8: cap + park quantum derived | `w_rewrite` p99.9, `cap_parks` |
| 15 | `perf/fsync-economy` | W-5 | W #9: `fsync_phase_ns` first, then touched-namespace flush + parallel meta legs | `w_durable` + fsync storm |
| 16 | `perf/read-handler-economy` | R-5 | R #6/#7/#8/#9 batched | `kernel_op_economy_tests` alloc law; `rr_4k` CPU/op |
| 17 | `perf/write-handler-economy` | W-6 | W #7/#10 batched | op-economy suite; `rw_4k` CPU/op |
| 18 | `perf/owner-hop-and-depth` | D-5 | DLM #7 + #8's single-connection half (#8's framing + session-pool depth landed as D-1b, `perf/d1b-publish-plane-batching`) + the accept-tick | `owner_phase_ns.dispatch`; frames multiplexed per socket |
| 19 | `perf/wire-economy` | D-6 | DLM #9/#10 | after the above |
| — | `fix/read-deposit-shard-lock` | R | **R #10 — correctness-class triage** | if a reader-visible window exists: red-first repro (the finding-17 pattern), lands as a bug fix outside the perf order; if it is hold time only, it joins R-5 |

### 5.4 The gate law

- **Every perf PR runs the full `task check`** (the code-class gate:
  clippy both feature configs, fmt, `test --test-threads=1`, doc, bench
  smoke, the fuse3 fork's gate, docs links, audit) **plus
  `tests/run_bench_baseline.sh` compare** against the A1-refreshed
  `reference.json` (nightly/perf-PR tier — pinned quiet cores, thermal
  tripwire, same-box only) **plus its field row** under the §0 landing law.
  A lock-free core change adds loom.
- **Waived gates are for bug fixes only.** A bug-class PR (F-46, F-47, R
  #10) lands on its red-first repro + the change-class gate without a field
  row; its field row is owed to CLOSE the finding, not to land the fix. **A
  perf PR never lands on a waiver**: no field row, no landing.
- **Counted-run discipline** applies verbatim (a fix mid-count restarts
  the count from zero; pre-fix rolls are rate/signature gathering only).
- **Cloud**: none of the above implies cloud approval; the cheap-first
  pipeline (tcp devsub → squeeze-test) is the venue ladder.

---

## 6. The matrix of record and the artifact discipline

The matrix is the union of the three ledgers' row sets (read R1–R12, write
R1–R13, DLM M-1–M-12 — their ids are carried verbatim in the per-campaign
notes) expressed as **path × shape × mode × substrate**. A campaign PR
measures the rows its lever touches; the **full matrix runs at program
milestones** (after A1; after Tier 1; at close).

### 6.1 Rows

| Id | Path | Shape (fio unless stated; "field shape" = the `/scratch/tmp/fio_jobs/*.job` file verbatim, 24 jobs) | Modes | Substrates |
|---|---|---|---|---|
| B-1 | fresh seq write | field shape (24 × 8 GiB), across the kvmap crossing | kern, il | tcp, field |
| B-2 | seq read cold / repeat | field shape; plus a 1 MiB qd8 / qd16 pair for the transport decomposition (the read ledger's row shape) | kern, il | loop (decomp), tcp, field |
| B-3 | rand read 4 KiB | field shape; plus qd1 (the RTT row) and qd32 (the saturation row) | kern, il | loop, tcp, field |
| B-4 | seq rewrite (kvmap files) | field shape, O_DIRECT and buffered | kern, il | tcp, field |
| B-5 | rand write 4 KiB — sole-owner (W1) | field shape | kern, il | tcp, field |
| B-6 | rand write 4 KiB — **patch-ineligible** (hole / clone-shared / decorated; the f47 venue) | field shape on a prepared file set | kern, il | tcp, field |
| B-7 | small-bs seq O_DIRECT (W #2) | 4–64 KiB | kern, il | tcp |
| B-8 | mixed seq bw / mixed rand 4 KiB | the field job files | kern, il | field |
| B-9 | durable write (`end_fsync`) + small-file fsync storm | — | kern, il | tcp, field |
| B-10 | raw fabric ceilings | fio direct on the namespaces: seq read/write 1 MiB, rand 4 KiB qd1/qd8/qd32 | — | field (per posture) |
| M-1 | authority-local metadata storm | mdstorm create/stat/rename/unlink, 1–64 writers | — | loop, tcp |
| M-2 | serial `tar -x` (real linux `fs/`) | authority-local; co-writer at RTT {floor, +250 µs, +1 ms} | — | tcp (netem) |
| M-3 | K-writer fan-out ingest (the S9-a shape) | K = 1..8 co-writers, iodepth 16 | — | tcp fleet, field |
| M-4 | membership at N | N = 32 / 512 / 1,024 / 2,048 real; 15 k SIM-1 | — | one box; SIM-1 |
| M-5 | free-grace recycle under churn | the sustain rig | — | tcp |
| M-6 | custody grant / renew / revoke RTT | the A2 E2E benches + a live fleet row | — | tcp |

### 6.2 Per-row artifacts (the rig's contract; `.benchmarks/rigs/2026-09-02-e2e-audit-baseline.sh` is the packaged form to be landed with A2)

1. reset → mount → **pre `.stats` snapshot** → row → **post snapshot** →
   delta; every `*_phase_ns` family dumped with exact `sum_ns`/`count`
   (post-A1) — midpoint tables labeled as such until then;
2. fio JSON + per-job bw/clat logs; **≥ 60 s window, first-third vs
   last-third flatness stated**;
3. `/proc/diskstats` deltas on the DATA namespaces → **device ÷ user bytes,
   `wareq-sz` vs block size, `block_free_*`** on every write row;
4. **engagement**: `ipc_ops_*` / `fuse3_*_replies` / `patch_writes` /
   `extent_parks` / `overlay_*` / `meta_ship_*` deltas vs the row's op
   count — a non-accounting row is INVALID, exit nonzero;
5. tripwires: `invariant_tripwires`, `transport_lease_overlong`,
   `*_fence_drops`, `meta_kv_block_refs_drift`, `fuse_op_watchdog_overdue`,
   `fsck_findings` (post-row fsck on write rows) — any movement is
   attributed in the note or the row is red;
6. CPU: `daemon_cpu_ns` (+ by class) delta ÷ bytes or ops (post-A1);
   pidstat per lane class as the cross-check;
7. A-B-B-A order + substrate + instrument + binary + kernel + tier in the
   row header; artifacts under `/scratch/tmp/e2e-<campaign>-<epoch>/` on
   the field box and `.benchmarks/rows-<campaign>-<epoch>/` in the repo for
   the accepted rows.

---

## 7. Open questions

1. **Today's unloaded fabric RTT** is unknown (the 235 µs constant is a
   2026-07-25 posture; the il `rr_4k` mean clat of 0.22 ms is below it).
   B-10's qd1 row answers it; until then no latency "% of floor" is
   computed.
2. **The raw 4 KiB IOPS floor** is a range (2.486–3.36 M); B-10 on today's
   memory-backed targets collapses it to one number per posture.
3. **Is the kvmap post-crossing publish O(map) or O(batch)?** (f46's
   hypothesis — the fix branch's red contract decides.)
4. **What share of `rr_4k` CPU is the `sqz_time`/`sqz_channel` class?**
   (candidate 48 — A1 + perf decide whether it is a finding.)
5. ~~**Two-stage conveyor vs one-tx-one-entry**~~ — answered by D-2
   (`.benchmarks/2026-09-03-d2-two-stage-conveyor.md`; the module doc in
   `src/meta_backend/kv/backend.rs`, "The two-stage commit conveyor"):
   the window's entries are the same N ordinary entries in the same
   contiguous reservation, atomicity/torn-write immunity are per entry,
   acks wait for the landed entry (deferred) / the barrier (strict) and
   never leave journal order (chain reachability); the crash matrix is
   pinned by `tests/conveyor_two_stage_tests.rs` (kill -9 with windows
   overlapping, parked predecessor write, parked barrier, fenced barrier).
6. **F-B's target shape**: multiplexed planes (×3–4, cheap) vs a
   poll/uring accept venue (removes the thread term) — is the first enough
   for the fleets this program can measure, and does the second need the
   sqz kernel?
7. **The overlay depth governor's signal**: device-bound venues lose
   0.68–0.80× — is the governor's saturation signal `dev_queue` (post-A1)
   or the overlay's own residence?
8. **The `mix_bw` il row** is "see artifacts" in the baseline — the table
   is incomplete until it is transcribed.
9. **`map_migrate_inos = 144`** — which six 24-file sets crossed (per-row
   deltas) — matters for which baseline rows ran on kvmap heads.

---

## Appendix A — the 2026-09-02 apparatus ledger (reproduced)

**Structural finding:** every `*_phase_ns` family is a 26-bucket µs
histogram with NO sum/count (`crates/squeezefs-ipc/src/latency_core.rs:19-37`;
`src/fuse_client.rs:4204-4266`), so every mean ever quoted is a ≤ 2×
midpoint estimate; no end-to-end per-op stitch exists (all joins by
subtraction); the write side has no `dev_queue`/`dev_service` split;
metadata ops have no always-on phase family; lock instrumentation has waits
for some classes, HOLD for none, 3.5 waits at 20+ sites unmeasured; no
uniform CPU column; the DLM microbenches cover primitives but not the M7
pass / shipped-verb RTT / custody grant end-to-end; criterion
`reference.json` is a month stale (`66bb4775`).

**Build-first ranking (by un-attributed time hidden):**

| # | Item | PR |
|---|---|---|
| 1 | `sum_ns` + `count` on every histogram | A1 |
| 2 | per-op trace ring + kernel tracepoint join | A2 |
| 3 | write `dev_queue` / `dev_service` split | A1 |
| 4 | 3.5 wait + hold; 4b wait split | A1 |
| 5 | always-on `meta_op_phase_ns` | A1 |
| 6 | uniform CPU column | A1 |
| 7 | client completion stamp | A2 |
| 8 | end-to-end DLM benches | A2 |
| 9 | refresh `reference.json` | A1 |

## Appendix B — the 2026-09-02 read ledger (reproduced)

**Governing caveat:** every read number predates the 2026-08-13
rip-tokio-TOTAL sweep, which put a process-global `std::sync::Mutex` +
`Box::pin` + `BinaryHeap` tombstone (`sqz_time::timeout`) on EVERY device
read (`src/nvme_dev.rs:2426`, 30 s deadline) and every cohort-wait slice
(`src/routing.rs:9299`), plus a per-channel `Mutex` on every fill's lane
mpsc (`nvme_dev.rs:2404`) and per oneshot (`:2395`) — unpriced by any read
row. *(Priced 2026-09-02 by R-1, §4.3: the named site is cold on the
sqz-kernel field posture in both modes, the lane mpsc is crossbeam, the
oneshot mutex is uncontended; the real per-op arm is the transport's
inbound-pop ticked park on the fuse3 registry at ≤ 3 % daemon CPU —
fat #1 below is retired.)*

**Fat board:**

| # | Item | Term | Lever |
|---|---|---|---|
| ~~1~~ | the `sqz_time`/`sqz_channel` mutex class — **retired 2026-09-02 (R-1, not fat; §4.3)** | measured: ≤ 3 % daemon CPU kern / ≈ 0.5 % il, < 0.2 % of clat; named site cold on the field | residual → Tier 3 (timer-thread wakeup coalescing; timer-less inbound park) |
| 2 | transport ingress | `queue_wait + dispatch_lag` ≈ 3.25 ms of 10.5 ms at 1 MiB qd8 | READ fast-dispatch from the reap thread |
| ~~3~~ | fill-issue economy — **landed 2026-09-03 (R-3)** | zc leg: three thread hops ≈ 126 µs around a 40 µs DMA (`zc_bridge_phase_ns`); funnel: `dev_queue` = the park outside the request channel | READ fusion onto the queue worker + the lane's arrival wake in the ring (`.benchmarks/2026-09-03-r3-fill-issue-economy.md`); residual = the fused pass's run-queue wait |
| 4 | whole-box CPU wall | 2.7 passes/byte | zc serve (`READ_FIXED` into folios, sqz kernel) |
| 5 | warm-serve pass (the 2026-08-04 "A1 warm-serve split" board item) | — | only zc can delete it |
| 6 | kernel READ handler | 8 allocs/op + ~20 global atomics + ~9 clock reads | `block_key_in` by-ref at `src/routing.rs:16145`, inline epochs, per-lane counter shards |
| 7 | ranged per-op string re-parse | — | parse once |
| 8 | kvmap partial resolve allocs | `parse_kvmap_head` per call, `String` clones, O(4096) allocs per window fill | typed head / borrowed keys |
| 9 | R2 residency | 5 probes | one probe |
| 10 | deposit under shard write lock | **correctness** | bug class |

**Copy-count table (passes/byte):**

| Path | Passes/byte |
|---|---|
| cold whole-block (kernel) | 2.69 |
| dest-lease | 2.0 |
| warm serve | 2.0 |
| il | 2.0 / 3.0 (arm-dependent) |
| zc | 1.0 |

**Rows:** R1–R12 (the ledger's matrix; carried into §6 as B-2/B-3/B-8/B-10
by shape). **NOT-fat adjudications** (measured, left alone): B1, B3, C1,
scatter-DMA, R2 ahead.

## Appendix C — the 2026-09-02 write ledger (reproduced)

**Fat board:**

| # | Item | Term | Status / lever |
|---|---|---|---|
| 1 | **the device-overlay B4 arm precedes `try_extent_park` with no length floor** — patch-INELIGIBLE sub-cap writes (hole / clone-shared / decorated) mint a 4 MiB CoW dest + read the 4 MiB old image per 4 KiB | ~1,024× each way, field-observed `.benchmarks/2026-08-17-mw-shipped-free-c8-fix.md:30` — the 2,500× regime the RW program closed, REOPENED | **FINDING 47**, fix in flight `fix/f47-overlay-length-floor` (derived floor `len ≥ patch_max_bytes`) |
| 2 | small-bs seq O_DIRECT rides the overlay per-op | `wareq-sz` collapse | same floor |
| 3 | overlay has no depth governor | 0.68–0.80× on device-bound venues; field neutral | `ProbeCore` governor |
| 4 | zc O_DIRECT extraction | ≈ 100 % of bytes; one kernel pass; CPU 26.7 → 39.5 j/GiB (ledger unit) | zc v2 fusion |
| 5 | publish conveyor commit-rate coupling | 4.8 ms/block saturated, 0.68 ms wake residue | ≡ DLM #2/#3 (C-1/C-2) |
| 6 | exclusive inode guard on fresh/append streams | — | narrow to meta-prep |
| 7 | per-WRITE allocs | 4 Box + 4 Arc slot closures, per-block `String`s, ~30 atomics | op-economy |
| ~~8~~ | reclaim constants at fleet rates — **LANDED 2026-09-05 (W-4)**, field owed | 4096-block cap fills in 0.2 s; 1 s park quantum = p99.9 tails | derived: event-driven park + `clamp(4 × batch ÷ drain rate, 50, 1000 ms)` bound + `clamp(rate × room, 4096, RAM)` cap |
| 9 | fsync flushes ALL data namespaces + 3 serialized meta legs | no `fsync_phase_ns` | instrument, then narrow |
| 10 | gap seeding re-reads whole old block | — | ranged seed |

**Copy law:** met or exceeded on every path (the ledger's per-path table
is the record; the 1-userspace-copy + 1-DMA law of
`docs/design-zero-copy-write-path.md` holds on the lease → merge → DMA path,
the placed-sever path elides the merge, zc extraction is one kernel pass).
**Amplification table:** the ledger's; the two cells quoted in this audit
are f47's ~1,024× each way and the baseline's must-stay-0 tripwires
(`write_path_seed_read_bytes`, `patch_edge_rmw_reads`) — per-row
amplification columns for the baseline rows were NOT captured (diskstats
deltas are owed by the rig, §6.2 item 3).

**Rows:** R1–R13 (carried into §6 as B-1/B-4/B-5/B-6/B-7/B-9). **Gaps:**
no overlay residence family, no fsync family, no IL write family, no
teardown instrument.

## Appendix D — the 2026-09-02 DLM ledger (reproduced)

**Two structural findings:**

- **F-A** — the owner executes a shipped frame SERIALLY, one conveyor
  commit per mutating verb (`src/meta_ship/service.rs:673-690`) — the
  9,473 verbs/s authority ceiling = 1 / (owner per-verb serial latency),
  the 2.6 GiB/s co-writer ingest wall's root.
- **F-B** — the cluster wire is thread-per-connection capped
  `clamp(cpus × 16, 64, 1024)` (`src/cluster_wire.rs:1464`) → ≤ 1,024
  readers or ≤ ~340 co-writers PER AUTHORITY by construction; 15 k members
  = 15 k OS threads.

**Fat board:**

| # | Item | Term | Lever |
|---|---|---|---|
| 1 | F-A | 2–10× on fan-out | dispatch a frame's mutating verbs concurrently so they co-queue into one M7 pass |
| ~~2~~ | the conveyor pass is one serialized server incl. the device write — **LANDED 2026-09-03 (D-2)**: ρ(apply) 1.00 → 0.13 on the fleet, aggregate PAR (the hop term binds — #3) | ρ ≈ 0.92; 0.78 ms vs ~0.17 ms leaf-lock floor | two-stage apply/durability conveyor, N writes in flight, in-order acks → ~4× headroom (the pass has it; converting it is #3's) |
| ~~3~~ | wake-hop inflation — **LANDED 2026-09-03 (C-2)**: the shared-lane `wake_hop` measured at 59–62 % of the journal write's round trip on the fleet, removed by the per-volume journal lane (`sqz-jrnl{N}` owning the volume's ring); `journal_ring_write` −34…−57 %, ingest PAR (the row's terms are the co-writer's and #7's) | the residue is the io-wq punt (two kernel wakes per window, 380–650 µs mean / 32 µs mode under saturation) | resident pass task on Notify and fan-out on the committer's lane stay unbuilt (not convicted); the io-wq residue's candidates are in the note's Owed |
| 4 | 4a stripe collisions — **mechanism LANDED 2026-09-05 (D-3), field owed** | 4096 stripes; guard held across the whole commit park. The census convicted the 4a `DlmLockManager` tables (collisions ≡ in-flight ÷ width: 2.9 % of 32-writer many-dirs renames at 4096) and acquitted the board's 1024-way suspects (waiter stripes = spurious wakes) | DONE: `<table>_stripe_collisions`/`_key_waits` + `dlm_guard_wait`; width `next_pow2(max(shipped, 16 × possible_cpus × q_depth))` (`SQUEEZEFS_DLM_STRIPES`); guards drop before the ack. OWED: mdstorm A-B-B-A 4096 vs derived |
| 5 | F-B connections | see above | multiplex planes per node ×3–4; poll/uring venue removes the thread term |
| ~~6~~ | free-grace ack cadence — **LANDED, FIELD OWED (D-4, 2026-09-05)** | recycle 65 MiB/s vs 2.2 GiB/s was the PRE-campaign row; the levers landed 2026-08-25 and the in-process closed loop shows them unbinding a recycle-bound stream at zero fences (`.benchmarks/2026-09-05-d4-free-grace-sustain.md`) | the from-zero s11-mpiio row with the sustain columns is the parent's |
| 7 | owner `spawn_meta_join` hop | ≤ 32 µs vs ≤ 8 µs read execute | execute on the accepting lane |
| 8 | stop-and-wait frame depth 1 | — | pipeline |
| 9–10 | copies / allocs | low | later |

**At floor (leave alone):** `authorize_dma` 1.3 ns; fencing reads
12–22 ns; local acquire 340 ns; membership renew 30–38 ns; alloc-lane
0 delta; journal framing 34 ns.

**Distributed floor arithmetic (tier: arithmetic-on-measured-constants):**

| Quantity | Value | Basis |
|---|---|---|
| members per authority | ≤ 1,024 readers; ≤ ~340 co-writers | F-B's cap ÷ planes per node |
| authorities for 15 k members | ≥ 15 (readers) / ≥ 45 (co-writers) | 15,000 ÷ the above |
| verbs/s per authority | 9,473 | F-A: 1 ÷ owner serial per-verb latency (≈ 106 µs) |
| co-writer aggregate ingest | ≈ 2.6 GiB/s per authority, writer-count-independent | the S9-a publish-plane wall (`docs/design-full-multi-writer.md`) |
| free-grace recycle | 65 MiB/s vs 2.2 GiB/s churn (pre-campaign); post-campaign the loop's ceiling is `per-lane spare ÷ L_lag` with `L_lag` ≈ 8.6 s coupled (in-process, D-4) | the sustain campaign's rows; the field row is owed |
| liveness | renew primitive at floor (30–38 ns); the fleet beat rate is bounded by F-B, not by the primitive | — |

**Matrix:** M-1 … M-12 (carried into §6 as M-1 … M-6 by shape).
