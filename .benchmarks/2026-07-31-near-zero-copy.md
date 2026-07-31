# 2026-07-31 — Near-zero-copy: the honest copy ledger, two cost levers, and the load-bearing declarations

Branch `perf/near-zero-copy` off dev `b4edafc` (**unmerged — do not
merge/push without orchestrator review**; the field verdict is RUN —
§7, 2026-07-31, A-B-B-A-B-A + A0 on the user's cluster). USER
DIRECTIVE, verbatim: *"Ideally we would be near zero copy...."*

Field facts driving the charter (user's 4-node cluster, dual-200GbE,
nullblk 4-wide ns=2, binary `b4edafc`, sustained 30–60 s rows): shim
writes **23,227 MiB/s**, kernel-path writes 19,654, reads ~19,900, raw
fio (no FS) 24.3 GiB/s; fabric exonerated end-to-end (all 8 NICs ~11 %
util); both FS paths plateau where their copy count predicts — the
ceiling is client memory traffic per payload byte.

Commits: red `d82c050` (NT + THP contracts), green `c274d9a` (the two
cost levers), rig `b88036a` (`tests/copy_census_rig.sh`), docs (this
note; the §7 field verdict + §8 gates landed in the closing docs
commit).

## 1. The copy ledger (the campaign's baseline instrument)

**Counting convention (stated):** one "copy" = one CPU pass that reads
the payload byte and writes it somewhere else. Each cached-`memcpy`
copy moves ≈ 3 bytes across the memory fabric per payload byte (source
read + destination read-for-ownership + destination writeback) when the
working set outruns the LLC; an NT-store copy moves ≈ 2 (no RFO); a
device/NIC DMA moves ≈ 1 (read or write). These per-copy weights are
architectural, not measured — the MEASURED cross-checks are §4's rig
rows (per-process LLC-miss proxies + throughput ratios; this host
exposes no uncore DRAM counters, stated honestly in the rig header).

### 1.1 Write paths (per payload byte, code-derived, b4edafc)

| # | Move | Who | Kernel path | Shim path | Disposition |
|---|---|---|---|---|---|
| K1 | app buffer → FUSE-over-io_uring registered payload ent | kernel (`copy_from_user` class), client CPU | 1 copy | — | **irreducible on this transport** (zero-copy design non-goal: no registered-user-buffer FUSE payloads in any shipped kernel; re-check if FUSE grows splice/zc receive) |
| S1 | app buffer → session arena slab | app thread (`session.rs::slab_write`) | — | 1 copy | **load-bearing — DECLARED**: the arena copy IS the §5.2/§5.3.1 isolation boundary (daemon never dereferences client pointers; a registered-buffer shortcut over app memory would be the GDS `/proc/pid/mem` precedent the design explicitly does not extend). True zero needs the app writing into the arena = API change, out of scope. Cost levers: arena THP (§3), NUMA note (§6) |
| M1 | transport lease → `ActiveBlockBuf` (merge) | daemon handler lane | 1 copy | — (placed-sever elided: the sever IS the merge) | **load-bearing — DECLARED**: (a) §5.4 lease-severance law — the ent's payload buffer is kernel-registered; a lease outliving its handler parks COMMIT_AND_FETCH (queue starvation, the R1 hang class); (b) the ACK-before-DMA pipeline detach (write-pipeline-depth campaign) — the handler returns before the device write, so the bytes must leave the transport buffer inside the invocation. Sub-block DMA-from-lease (Alt C) would pin ents across ~ms device writes AND kill pipeline depth. Cost lever: NT stores (§2) |
| S2 | arena → placed-sever assembly (= the future ABB backing) | ipc service thread (`SharedBlock::write_at`) | — | 1 copy | **load-bearing — DECLARED**: the §5.5.2 sever (single arena read, at dequeue, on the service thread) is the same §5.2 boundary as S1 — the daemon must sever before it can trust a byte. The merge copy is already elided (pointer proof, shim-parity 2026-07-28). Cost lever: NT stores (§2) |
| D1 | ABB → NVMe-oF | block layer + NIC DMA | 1 DMA read | 1 DMA read | already zero-CPU on the field kernel: nvme-tcp TX splices GUP-pinned O_DIRECT pages (§5); digests off, csum offloaded |
| — | **total CPU copies** | | **2** (K1 + M1) | **2** (S1 + S2) | |
| — | **fabric-traffic weight** (cached copies ×3, DMA ×1) | | ≈ 7 B/B | ≈ 7 B/B → **≈ 5 B/B with both NT sites** (S2+M1 → ×2) | |

Why the shim leads the kernel path at equal copy count (field +18 %):
S1 runs on the 32 app cores (parallel, overlappable with op pipelining)
while K1 runs in the write(2) syscall path with FUSE protocol overhead
around it; the shim path also skips the kernel round-trip per op. The
copy COUNT parity is why the two plateau near each other and well below
raw fio (0 copies before DMA).

### 1.2 Read paths (per payload byte, code-derived, b4edafc)

| # | Move | Kernel path | Shim path | Disposition |
|---|---|---|---|---|
| R-RX | NVMe-oF RX: skb → destination pages | 1 kernel copy (softirq) | 1 kernel copy | **irreducible on nvme-tcp** (§5): no zero-copy TCP RX without devmem/TLS-offload-class machinery; bounds every cold-read economy from below |
| R1 | device pages → tier/pooled buffer | (is the DMA destination) | (same) | already right: DMA lands in the pooled/tier buffer |
| R2 | tier/buffer → ent payload / arena | 1 copy (reply serve into registered ent payload — already zero-alloc) | 1 copy (serve-into-arena `PayloadSink`, op-economy 2026-07-28; **direct-drive small shapes DMA straight into the arena — 0 copies**) | keep cached stores: destination is CPU-read immediately (kernel `copy_to_user` / app) — NT here would trade an LLC hit for a DRAM read on the consumer |
| R3 | ent payload → app / arena → app | 1 kernel `copy_to_user` | 1 app copy (`slab_read`) | structural: POSIX `read(2)` hands the app ITS buffer; same count both paths |
| — | **total** | DMA + RX copy + 2 copies | DMA + RX copy + 2 copies (1 on direct-drive shapes) | parity by construction; warm (tier-hit) rows drop the DMA+RX leg |

**Eliminations available in this table: none that survive their laws.**
The campaign therefore ships COST reductions (§2, §3) and the
instrument (§4), and prices what remains.

## 2. Lever 1 — NT-store DMA-destined copies (`src/nt_copy.rs`)

M1 and S2 are the only hot copies whose destination's next consumer is
**device DMA, not a CPU** — so they may use non-temporal stores:
destination RFO deleted (≈ 3 B/B → ≈ 2 B/B for that copy) and multi-GiB
streams stop sweeping the LLC the warm-read tiers and the §5.5.1 sync
fast path live in. Everything CPU-read stays cached (pooled sever, read
serves, CoW/zero paths, the shim's S1 — its destination is read by the
service thread's sever within ~one op).

- SSE2 body (x86_64 baseline, no dispatch): scalar head to 16 B dst
  alignment, 64 B `movntdq` unroll, scalar tail, **trailing `sfence` —
  LOAD-BEARING** (NT stores are weakly ordered; the fence is what makes
  the existing publication edges — `BLOCK_FLUSH_LOCKS` unlock, placed
  `end_write` — sufficient). No loom model is possible for NT stores;
  the fence is pinned by comment, the publication-edge smoke test, and
  review.
- Pure policy core: default ON, floor 256 KiB (merge chunks are
  1 MiB-class; sub-floor small writes are latency-bound and re-read
  soon). `SQUEEZEFS_NT_COPY=0` kill switch / `SQUEEZEFS_NT_COPY_MIN`
  floor — census-rig A/B levers.
- Engagement: `nt_copy_bytes` (stats inode) — a lever row is INVALID
  unless its delta accounts for the row's merge/sever bytes.
- Contracts (`tests/nt_copy_tests.rs`, red-first): byte exactness at
  every alignment with guard bytes, policy purity, truthful engagement,
  counter exactness, publication-edge smoke.

## 3. Lever 2 — session-arena THP (`crates/squeezefs-ipc/src/thp.rs`)

The session shm is **shmem**, policy-gated separately from anon THP:
`shmem_enabled` = `advise` (dev rig) / **`never` (field, Rocky 8
default)** — so while every anon pool already rides 2 MiB pages
(`enabled=always` fleet-wide), the arena both S1 and S2 sweep was
4 KiB-paged everywhere. Levers (best-effort, refusal-tolerant —
`SQUEEZEFS_IPC_ARENA_THP=0` disables):

- Daemon, at session admission: `MADV_HUGEPAGE` +
  `MADV_POPULATE_WRITE` + `MADV_COLLAPSE` — **collapse operates
  independent of the sysfs policy** (madvise(2)), so it is THE field
  lever; populate-first makes it deterministic. One-time, off the data
  path; bytes already budget-charged at full geometry
  (`ipc_arena_bytes`).
- Shim, at map time: `MADV_HUGEPAGE` only — once the daemon's collapse
  makes the memfd's page-cache pages PMD-sized, the client mapping can
  map them huge.
- Canonical file lives in the `squeezefs-ipc` tree, `#[path]`-shared
  into the root crate and the shim (the `wake_core` production-sharing
  precedent — the ipc LIBRARY stays dependency-free).
- Contracts (`tests/arena_thp_tests.rs`, red-first): anon `hg` VmFlag,
  refusal tolerance (shim-safe), shmem collapse visible as
  `ShmemPmdMapped` when granted (verified granted on the dev rig).

## 4. Rig measurement (TCP devsub, the fabric-sensitive venue)

**Substrate (stated):** TCP devsub (`SQZ_DEVSUB_TRANSPORT=tcp`,
nvmet-tcp on localhost; meta = 4× memory null_blk, data = 4× 8 GiB
zram, ports 54100-slice), coexisting with the box's idle loop devsub.
**Instrument (stated):** `tests/copy_census_rig.sh` — fio psync
`--direct=1 --zero_buffers` t8 × 1 MiB time_based 30 s (sustained rows
60 s), medians of 3, order **A-B-B-A** (A = tip `3d3cc8f` pair, B = dev
`b4edafc` pair; KD-7 same-commit daemon+shim pairs; the one
post-binary source commit is whitespace-only fmt) + an **A0 pass**
(tip binary, `SQUEEZEFS_NT_COPY=0 SQUEEZEFS_IPC_ARENA_THP=0`) for
lever isolation. **Box honesty:** 32-CPU AMD Strix Halo under the
external 2.0–2.4 GHz thermal governor, with the main tree's fstests
release gate running throughout (stated background load — constant,
not bursty); no uncore DRAM counters exist on this platform, so the
traffic instrument is the per-process core-PMU proxy
`cache-misses × 64 / user_bytes` (prefetch traffic undercounted; the
proxy's VALUE is relative A-vs-B on identical rows). zram stores age
across reps (rep-1 fresh-format vs rep-2/3 — the write-wall
hysteresis shape), so bw verdicts are read per-rep-position and from
the sustained rows, never from pooled medians. Raw CSV + fio/perf/THP
snapshots: `/tmp/nzc_census/` (log `/tmp/nzc_census_log.txt`).

### 4.1 The traffic proxies (the stable columns on this venue)

Daemon LLC-miss bytes per payload byte (median of 3, per pass):

| Row | A1 | A2 | B1 | B2 | A0 (levers off) | Δ A vs B |
|---|---|---|---|---|---|---|
| wr-il | 1.11 | 0.97 | 1.30 | 1.31 | **1.28** | **−20…−25 %** |
| wr-kern | 1.98 | 1.65 | 2.23 | 2.15 | 1.91 | **−15…−23 %** |
| rd-il | 0.72 | 0.74 | 0.89 | 0.74 | — | ≈ par (no NT on reads by design) |
| rd-kern | 0.86 | 0.79 | 0.85 | 0.79 | — | par (untouched path) |

- **A0 ≈ B on every proxy** (1.28 vs 1.30/1.31 il; 1.91 vs 2.23/2.15
  kern) — the deltas are THE LEVERS, not binary drift.
- Daemon dTLB misses/MiB on wr-il: A 678–1131 vs B 872–957 vs
  sustained A 597 vs B 862 (**−31 % sustained**) — the arena-THP face.
- `nt_copy_bytes` ≡ user bytes on every A write row (engagement
  exact); `ShmemPmdMapped` 448 MiB sampled live on every A il row
  (7 sessions × 64 MiB); `write_path_seed_read_bytes` 0 throughout;
  amp ≈ 1.00 on every write row (dev_w ≈ user).

### 4.2 Throughput (per-rep-position brackets + sustained)

| Row | rep1 A/B | rep2 A/B | rep3 A/B | sustained 60 s A vs B |
|---|---|---|---|---|
| wr-il | **1.28** | 0.96 | 1.04 | **8,268 vs 7,510 (+10.1 %)** |
| wr-kern | 1.11 | 1.14 | 1.00 | **6,337 vs 6,123 (+3.5 %)** |
| rd-il | 1.13 | 1.20 | 1.10 | — |
| rd-kern | 1.01 | 1.07 | 1.05 | — |

The sustained rows are the verdict (standing sustained-state rule);
the single-pass rep spreads carry the zram aging + busy-box noise
(both directions present in the brackets, stated). Reads do not
regress (il reads trend ahead — consistent with the LLC no longer
being swept by write streams on this shared-workload venue, but not
claimed as a proven mechanism).

**Lever defaults adjudicated: both stay ON** (`SQUEEZEFS_NT_COPY=1`
floor 256 KiB, `SQUEEZEFS_IPC_ARENA_THP=1`) — the traffic proxies move
exactly where the ledger predicts, the sustained rows gain, no row
regresses beyond the venue's own spread, and A0 pins attribution.

## 5. Kernel TX/RX posture (charter item 2 — verified on the field, read-only)

Field client fleet (probed over ssh, journaled): Rocky 8.10, kernel
**7.1.2-1.el8.elrepo**, 2× Xeon Gold 6426Y (2 NUMA nodes), dual
ConnectX (mlx5) 200GbE; `nvme-tcp` connections carry **no header/data
digests** (our initiator never passes digest flags — nvme-cli defaults
off; verified `src/nvmeof/initiator.rs` + module params), NIC
`tx-checksumming`/`scatter-gather`/`TSO` on.

- **TX (block writes): zero-CPU-copy.** Since the 6.5-era
  `MSG_SPLICE_PAGES` conversion (present in this 7.1 lineage), nvme-tcp
  transmits request data by splicing the bio pages into the socket —
  for our io_uring O_DIRECT writes those are the GUP-pinned
  `ActiveBlockBuf` pages themselves. With data digest off there is no
  CRC read pass; with csum offload the NIC computes TCP checksums. The
  CPU never touches payload bytes on TX: **D1 ≈ 1 NIC DMA read of the
  ABB, full stop.** Consequence: userspace copy elimination is the
  whole write-side game on this fleet — there is no hidden kernel TX
  copy to chase.
- **RX (block reads): one kernel copy, irreducible here.**
  `nvme_tcp_recv_data` copies skb payload into the destination pages
  via the datagram-iter path in softirq context — ≈ 1 CPU copy per read
  byte before any userspace economics start. Zero-copy TCP RX
  (devmem-TCP / page-flipping) is not available to nvme-tcp on this
  kernel lineage. This bounds read-path elimination: the read floor is
  RX-copy + R2 + R3.
- Digest posture is a standing operational tripwire: enabling data
  digest would add a full CPU read pass per byte in both directions —
  never turn it on for perf fleets without re-running the ledger.

## 6. What was eliminated vs priced vs declared load-bearing

| Item | Verdict |
|---|---|
| K1 kernel FUSE payload copy | irreducible (transport non-goal, re-check on kernel FUSE zc-receive) |
| S1 app→arena | **load-bearing (security boundary)** — priced; cost cut by THP; NUMA placement noted below |
| M1 lease→ABB merge | **load-bearing (§5.4 + ACK-before-DMA)** — cost cut by NT stores |
| S2 arena→assembly sever | **load-bearing (§5.2 boundary)** — cost cut by NT stores; merge already elided (placed-sever) |
| R2/R3 read serves | priced (destinations CPU-read — NT would pessimize); direct-drive already 0-copy daemon-side on its shapes |
| nvme-tcp TX | verified already zero-CPU on the field kernel |
| nvme-tcp RX | irreducible kernel copy (documented floor) |
| NUMA (field: 2-socket clients) | noted, unpursued: cross-socket S1/S2 and NIC-remote DMA are a real term on 2-socket clients; session-to-thread affinity would need its own campaign |

## 7. Field verdict (MEASURED, 2026-07-31 — supersedes the projection draft)

**Venue (stated):** the user's 4-node cluster — client
`memp-s3ds-aqs-37` (32 CPU, 2× Xeon Gold 6426Y, 2 NUMA nodes, dual
ConnectX 200GbE), storage mds0/mds1/oss0/oss1, **cluster_reset v3**
(2026-07-31 08:13Z): nullblk 4-wide ns=2 data plane, cache-less
format, meta-slots 8 — the throughput-ceiling substrate. That reset's
own b4edafc bar was journaled "15–17 writes / reads floor" (GiB/s
class) — the pre-reset charter figures (shim 23,227 / kernel 19,654 /
reads ~19,900 MiB/s) belong to the previous substrate build + fill
state, so the governing floor here is the in-bracket B, not the
historical row. **Instrument (stated):** elbencho 3.1-11 (dynamic),
`t32 -b 4m`, 16 × 8 GiB set (128 GiB; fresh rows at 9 % fill,
sustained rows at 42 % fill), O_DIRECT, sustained rows
`--timelimit 60 --infloop`; il rows via `LD_PRELOAD` of the mounted
daemon's KD-7 shim. **Discipline:** order **A-B-B-A-B-A** (medians of
3, per-rep order-labeled) + one **A0** attribution leg (tip binary,
`SQUEEZEFS_NT_COPY=0 SQUEEZEFS_IPC_ARENA_THP=0` both sides); settle
hygiene between rows (reclaim `queue_bytes == 0` ×3); dev pair
restored + store settled at end. A = rocky8 pair `e03b65f` (KD-7
same-build, glibc ≤ 2.28 asserted in-container); B = deployed dev
pair `b4edafc`. Raw artifacts: client `/scratch/tmp/nzc_verdict/`
(CSV + per-row elbencho/stats/PMD samples), journal
`/scratch/tmp/agent_runs.log`.

### 7.1 Throughput (MiB/s, medians of 3, per-rep ratios order-labeled)

| Row | A (e03b65f) | B (b4edafc) | A/B | per-rep A/B | A0 (levers off) |
|---|---|---|---|---|---|
| fresh 128 GiB (kern, 9 % fill) | **22,348** | 20,343 | **+9.9 %** | 1.099 / 1.079 / 1.156 | 20,274 (≈ B) |
| wr-kern sustained 60 s | **16,666** | 15,184 | **+9.8 %** | 1.085 / 1.106 / 1.098 | 15,298 (≈ B) |
| wr-il sustained 60 s | **18,045** | 16,878 | **+6.9 %** | 1.056 / 1.069 / 1.066 | 16,945 (≈ B) |
| rd-kern sustained 60 s | 16,955 | 16,881 | +0.4 % (par) | 1.009 / 1.005 / 1.004 | 16,939 |
| rd-il sustained 60 s | 17,436 | 17,578 | −0.8 % (par, in spread) | 1.005 / 0.997 / 0.970 | 17,642 |

- **A0 ≈ B on every row** (wr-kern 15,298 vs 15,184; wr-il 16,945 vs
  16,878; fresh 20,274 vs 20,343) — binary drift is nil; **the deltas
  are THE LEVERS**, attribution pinned on the field exactly as on the
  dev rig.
- The field verdict lands inside the §1.1 prediction band (projected
  +10–20 % on the copy-bound write plateau): **+9.8 % kern / +6.9 %
  il sustained**, +9.9 % fresh — the write plateau moved where the
  ledger said the deleted RFO would put it.
- Reads: no regression (kern +0.4 %; il −0.8 % median with per-rep
  spread both directions 0.970–1.005) — reads sit at/above the
  in-bracket B floor, which reproduces the reset-v3 journal bar.

### 7.2 Engagement (every check exact, per row)

- `nt_copy_bytes` / row bytes = **1.000–1.001** on every sustained A
  write row; 0.972–0.987 on fresh rows (sub-floor tails + inline
  births — expected); **0 bytes on every A0 row** (lever-off proof).
- `ShmemPmdMapped` = **512 MiB live during every A il row** (8
  sessions × 64 MiB; sampled from the daemon's smaps_rollup DURING the
  row) vs **0 on every B and A0 row** — the daemon-side
  `MADV_COLLAPSE` + PMD-aligned map works on the Rocky-8-class field
  client exactly as designed (`shmem_enabled` policy bypassed).
- `placed_merge_elides ≈ ipc_placed_severs` on every il write row
  (e.g. 1,068,969 vs 1,069,125); `ipc_bytes_in/out` account for every
  il row's bytes (charter rule 4); `write_path_seed_read_bytes` **0
  throughout**; `ipc_descriptor_rejects`/`ipc_sessions_poisoned` 0.

### 7.3 Parity-law adjudication (stated honestly)

Shim stays **strictly ahead of kernel on every write row** (A: 18,045
vs 16,666 = +8.3 %; B: 16,878 vs 15,184 = +11.2 %) and gained
absolutely (+6.9 %) — but the shim/kernel **ratio narrowed**
(1.112 → 1.083). Mechanism, per the ledger: NT stores cheapen M1
(kernel-only) AND S2 (shim), but the shim's other copy S1 (app→arena)
is untouched by NT (declared load-bearing, CPU-read next), so the
kernel path gains proportionally more. The shim-parity campaign's
governing verdict (kernel and IPC at minimum par, IPC ahead) holds on
every row; the ratio narrowing is the cost of fixing a kernel-only
waste term, not a shim tax — flagged for the orchestrator explicitly.
The widening lever the ledger names is S1-side (NUMA/session
placement, §6) — its own campaign.

### 7.4 Field notes verified

- **NUMA (2-socket client):** still unmeasured/unpursued — named in
  the ledger (§6), the natural next lever for the il lead (§7.3).
- **NT on Sapphire Rapids:** the Zen-5-rig result transfers — the
  fill-buffer economics question is answered by the A/B itself
  (+9.8 % kern with `nt_copy_bytes` exact).

## 8. Gates

Closing tip `e03b65f` (docs) / binary-relevant tip `01b662e`; long
gates skipped by user directive (release-tag gates run later,
elsewhere) — the merge-readiness bar for this campaign:

- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --check` — clean.
- Targeted test binaries covering every touched surface
  (`--test-threads=1`, all green): `nt_copy_tests` 6/6,
  `arena_thp_tests` 3/3, `ipc_host_tests` 21/21, `shim_parity_tests`
  3/3, `write_through_coverage_tests` 8/8, `ipc_op_economy_tests` 3/3.
- Red-first contracts: `d82c050` (red) precedes `c274d9a` (green) in
  the branch history.
- rocky8 container build (`docker/build-in-container.sh`): KD-7
  same-commit daemon+shim pair, `--version` non-unknown, glibc ceiling
  2.28 asserted in-container — passed (the deployed field pair).
- Field verdict (§7): A-B-B-A-B-A + A0, zero aborts, engagement exact
  on every row, dev pair restored, store settled.


## 9. Open questions

1. **App→arena NT?** Deliberately not attempted (destination is
   CPU-read by the sever within ~one op). If a future field profile
   shows the arena copy RFO-bound on 2-socket clients, re-run the §4
   lever bracket with an NT variant of `slab_write` — expected to lose
   while the sever reads hot lines, stated so the negative is cheap to
   re-check.
2. **Kernel FUSE zc-receive:** K1 stays irreducible until the kernel
   grows registered-user-buffer FUSE payloads; re-open the M1
   discussion only then (the lease law would need a re-design, not a
   tweak).
3. **fuse3 PayloadArena THP:** individual 1 MiB payload buffers are
   sub-PMD and jemalloc-backed (anon THP `always` fleet-wide covers
   them opportunistically); a dedicated 2 MiB-aligned arena per queue
   was left unpursued — bounded win, measurable via the same dTLB
   column if revisited.
