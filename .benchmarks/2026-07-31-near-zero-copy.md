# 2026-07-31 — Near-zero-copy: the honest copy ledger, two cost levers, and the load-bearing declarations

Branch `perf/near-zero-copy` off dev `b4edafc` (**unmerged — do not
merge/push without orchestrator review**; the orchestrator runs the
field verdict). USER DIRECTIVE, verbatim: *"Ideally we would be near
zero copy...."*

Field facts driving the charter (user's 4-node cluster, dual-200GbE,
nullblk 4-wide ns=2, binary `b4edafc`, sustained 30–60 s rows): shim
writes **23,227 MiB/s**, kernel-path writes 19,654, reads ~19,900, raw
fio (no FS) 24.3 GiB/s; fabric exonerated end-to-end (all 8 NICs ~11 %
util); both FS paths plateau where their copy count predicts — the
ceiling is client memory traffic per payload byte.

Commits: red `d82c050` (NT + THP contracts), green `c274d9a` (the two
cost levers), rig `b88036a` (`tests/copy_census_rig.sh`), docs (this
note).

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

_(filled by the census runs below)_

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

## 7. Field projection (PROJECTIONS — the orchestrator runs the field verdict)

_(filled after rig rows)_

## 8. Gates

_(filled at the closing tip)_
