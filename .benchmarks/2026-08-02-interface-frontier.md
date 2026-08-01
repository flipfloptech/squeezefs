# 2026-08-02 — Interface frontier: capability census, counted copy-term pricing, and the ranking that IS the deliverable

Branch `perf/interface-frontier` (off dev tip `8fb42f5`, **unmerged — the
orchestrator merges**). Charter: the last named performance frontier —
the two interface-class copy terms the READ copy ledger priced and
stopped on (`.benchmarks/2026-08-02-read-copy-count.md` §3.1: nvme-tcp
RX ≈ 0.7 CPU passes/user byte, fuse-uring ent→user commit = 1.0
pass/byte). **Binding user constraint (2026-08-02, non-negotiable):**
the client NICs are RoCE-capable but *no fabric transport conversion,
no network reconfiguration* — the data plane stays NVMe/TCP on the
existing links; host-side software arrangements only; NIC feature
toggles approval-gated (censused read-only here, nothing enabled).

**Outcome up front: the Phase-1 STOP-clause is invoked.** Every
candidate above the ~1 GB/s bar is either already shipped (the il
lane), driver-blocked on this venue (io_uring zcrx — the mlx5 build
exposes no `tcp-data-split` control), kernel-absent (nvme-tcp DDP,
devmem-TCP), or banned by the constraint (TCP_ZEROCOPY_RECEIVE needs an
MSS/MTU change). The only buildable-now candidate (daemon DRAM/alloc
slimming) counted **below** the bar. No code shipped; the capability
matrix + counted ranking below is the deliverable, per charter.

## 1. Venue (labeled once)

* Field client squeeze-test / memp-s3ds-aqs-37 (32 CPU / 2× Xeon Gold
  6426Y / 2 NUMA), Rocky 8.10, **ELRepo kernel-ml 7.1.2-1.el8** —
  VENUE EPOCH **reset-v4** (canonical table
  `.benchmarks/2026-08-02-post-reset-baseline.md`; standing pair
  `1b936bf`), store UNAGED by this window: every field row was
  **read-only** (reads of the fixed `base/` 16×8 GiB set + raw
  read-only device sweeps; zero data-plane writes, no resets, no
  reformats, no storage-node changes, no NIC/network changes).
* Instrument: fio-3.36 via the retained row machinery
  (`/scratch/tmp/ifr/` — the rcc wrapper relabeled reset-v4), rows
  60 s + 10 s ramp, cold = fresh remount, settle hygiene ×3 @ 1 Hz;
  perf 7.1.5 whole-system dwarf captures (30 s mid-window, `-F 199`;
  il row `-F 99`/25 s) analyzed locally per `tests/perf_remote.sh`
  round-trip (same-version local perf + on-box kallsyms; daemon DSO via
  build-id cache). **Perf-carrying rows are labeled w/perf** — the
  capture costs ~1–6 % of row throughput; canonical numbers stay the
  baseline table's.
* Dev box (7.1.5-cachyos, 18 CPU): dhat pass venue (loop devsub,
  zram-backed). No wired NIC (wifi only) ⇒ **zcrx/RX-placement
  prototyping is impossible off the field client** — stated as a
  matrix precondition.

## 2. Phase 0 — the capability matrix (READ-ONLY census; nothing was changed)

NIC inventory: 2× **Mellanox ConnectX-7 (MT2910)**, `mlx5_core`
in-tree (kernel-ml build), fw **28.43.3608**, one active 200GbE port
per card (`ens1f0np0` @ 10.181.177.194, `ens2f0np0` @ 10.181.178.194),
MTU 9000, channels 32, striding-RQ on, LRO off, HW-GRO off, ntuple
off, 0 steering rules. Also present (inactive): 2× BCM57414
10/25G RDMA (bnxt_en, link down), 2× BCM5720 1G (mgmt). Live nvme-tcp
MSS census: 58 conns @ 4144, 9 @ 8256 (jumbo, NOT 4 KiB-multiple —
matters for TZC below). nvme-tcp geometry: 20 controllers × 32 I/O
queues, `so_priority=0`, `wq_unbound=N`, digests off.

| Candidate mechanism | Kernel surface (7.1.2-1.el8.elrepo) | Driver/NIC surface (mlx5/CX-7) | Other preconditions | Verdict |
|---|---|---|---|---|
| **nvme-tcp DDP offload (ULP DDP)** | `CONFIG_ULP_DDP` **absent from the config**, **0** `ulp_ddp` kallsyms | CX-7 is the hardware class the out-of-tree patchset targets — hw-capable, sw-absent | the NVIDIA ulp_ddp patchset was never merged upstream; enabling means a **custom kernel** | **ABSENT** — not approval-gated, *unavailable*: there is no toggle to approve on this kernel; kernel replacement is an infra change, out of scope |
| **io_uring zcrx** | **present**: `CONFIG_IO_URING_ZCRX=y`, `IORING_OP_RECV_ZC` (uapi 314), `IORING_REGISTER_ZCRX_IFQ`=32/`_CTRL`=36, `io_pp_zc_*` memory-provider ops, `page_pool_check_memory_provider`, `net_devmem_*`, `netdev_rx_queue_restart` | `mlx5e_queue_mgmt_ops` present, page-pool RX present, **but the ethtool-netlink RINGS_GET reply carries NO `ETHTOOL_A_RINGS_TCP_DATA_SPLIT` attr on either port** (read-only genetlink probe, §2.1) ⇒ this mlx5 build has **no HDS control surface**, and zcrx binding hard-requires `ETHTOOL_TCP_DATA_SPLIT_ENABLED` | (a) HDS enable + ntuple steering of the lane's connections = **NIC toggles, approval-gated**; (b) a **userspace io_uring-native NVMe/TCP initiator lane** for cold read fills (weeks-class build; SPDK prior art `.benchmarks/2026-07-17-spdk-target-scoping.md`); (c) no dev-box prototyping venue (no wired NIC) | **DRIVER-BLOCKED** on this venue (before the approval question is even reached) |
| **devmem-TCP** | `CONFIG_NET_DEVMEM=y` + full symbol surface | same HDS requirement as zcrx | **`CONFIG_UDMABUF` not set** and **no GPU on the box** ⇒ no dma-buf source for host-RAM placement | **ABSENT-for-us** |
| **TCP_ZEROCOPY_RECEIVE** | present (`tcp_zerocopy_receive`) | page-flip needs page-aligned, page-multiple payload frags ⇒ effectively HDS + 4 KiB-multiple MSS; live MSS is 4144/8256 | fixing the geometry = MTU/MSS change = **network reconfiguration, banned** | **DEAD by constraint** |
| **MSG_ZEROCOPY (TX)** | present | — | kernel nvme-tcp TX is already zero-copy (spliced GUP pages — standing verified posture) | N/A — no term to delete |
| **FUSE passthrough** | `CONFIG_FUSE_PASSTHROUGH=y`, `FOPEN_PASSTHROUGH`/`fuse_backing_map` in uapi 7.45 | — | needs a **backing file on a lower filesystem**; SqueezeFS data lives on raw block namespaces behind our own layout | **N/A architecturally** |
| **fuse-over-uring payload placement** | uapi 7.45 has REGISTER / COMMIT_AND_FETCH only; the commit copies via `fuse_uring_copy_from_ring` → `fuse_copy_*` (kallsyms + §3 profile) — no registered-dest / zc-reply capability | — | — | **ABSENT — kernel-boundary confirmed** (the K1 twin priced in §3) |
| **nvme_tcp module params** (`wq_unbound`, `so_priority`) | present (`N`, `0`) | — | retoggle requires disconnect/reconnect of the live fabric connections (venue-perturbing); placement lever only — deletes no copy | census-only; not pursued |

Census riders: (1) the daemon's standing log line "UNKNOWN init
capability bits 0x40000000000" is **bit 42 = `FUSE_REQUEST_TIMEOUT`**
(7.43+) — a request-timeout capability, *not* an atomic-open-class bit
(answers the D2.d investigation note in passing). (2) `enable_uring=Y`
already set; fuse module params otherwise default. (3) `/sys` HDS
thresh reads 0/0 on both ports (consistent with no HDS support).

### 2.1 The decisive probe (read-only, reproducible)

Box ethtool is 5.13 (predates `tcp-data-split`), so the census cell
was taken with a ~120-line C genetlink probe (compiled on-box with gcc
8.5, retained at `/scratch/tmp/hds_query.{c,ifr}`): resolve the
`ethtool` genl family, send `ETHTOOL_MSG_RINGS_GET` (=15) with a
nested `ETHTOOL_A_RINGS_HEADER`/`DEV_NAME`, walk the reply for attr 11
(`TCP_DATA_SPLIT`), 17/18 (`HDS_THRESH{,_MAX}`). Result on **both**
active ports: `tcp-data-split: ATTR NOT REPORTED` — the driver does
not register the HDS ring param, so there is nothing an approval could
toggle; zcrx's `net_mp` binding would refuse the queue. When a future
kernel/driver build reports this attr, re-run the probe before
re-opening candidate 1.

## 3. Phase 1 — the counted pricing (whole-system cycle decomposition)

Three rows on the pristine reset-v4 store, each with a mid-window
30 s/25 s system-wide dwarf capture; flat self-cycle shares of the
**whole box** (all comms). Artifacts: `/scratch/tmp/ifr/rows/*`
(fio json, stats before/after, DRAM census); captures analyzed
locally.

**Row A — kern EXA read, libaio 1M qd8 nj32 cold: 28.61 GB/s w/perf**
(canonical 27.33; read_amp 0.677, DRAM 10.26 B/B, ledger closure
1.167 ramp-inclusive; mpstat ~80 % busy = 19 %usr/52 %sys/7 %soft):

| Term | % of ALL cycles | Owner | Fate |
|---|---|---|---|
| FUSE ent→user commit machinery: `__pi_memcpy` under `fuse_uring_copy_from_ring` **16.9 %** + `_raw_spin_lock` under `fuse_copy_fill` **12.8 %** + fill/GUP/unpin ≈ 2.6 % | **≈ 32.3 %** | kernel (`fuse-over-uring` queue-worker comms) | interface-class; deletable TODAY only by the il lane (Row C) |
| nvme-tcp RX copy class: `__pi_memcpy` under `__skb_datagram_iter` **13.7 %** + iter self 1.6 % + usercopy hardening ≈ 1.1 % | **≈ 16.3 %** (+ ~4 % skb/page-pool/GRO riding it) | kernel (nvme-tcp io_work kworkers) | interface-class; every deletion mechanism unavailable (§2) |
| daemon serve copy `routing::serve_copy_to_dest` (NT, the ONE lawful pass) | 17.9 % | daemon | load-bearing (2026-08-02 adjudication stands) |
| daemon beyond-copies machinery (everything else userspace: vdso clock ≤ 1.4 % incl. fio's share, scc hot-tier bucket 0.7 %, read closures ≈ 0.9 %, moka 0.2 %, …) | **≈ 5.3 %** | daemon | candidate 4's honest bound |

The **new decomposition fact** this campaign adds: the commit copy's
machinery costs ≈ 2× the byte move itself — `fuse_copy_fill`'s
per-page `FR_LOCKED` request-lock discipline alone is 12.8 % of all
client cycles (≈ 7 M lock round-trips/s at 27k × 1 MiB ops = 256
fills/op), on top of the 16.9 % memcpy. An upstream FUSE change
(batch the lock discipline per commit, or pin the registered ent
payload once at REGISTER like io_uring fixed buffers — the payload
span is stable for the ring's life) would delete ~13 % of client
cycles on kernel-path reads with zero daemon change. Filed as the
upstream note below; not buildable here (we do not own the field
kernel).

**Row B — raw read ceiling, 8 data namespaces, 4M qd16 ×8 jobs/dev
(read-only): 46.6 GB/s** (98.4 % busy: 73.5 %sys + 23.7 %soft — the
ceiling is hard client-CPU-bound, the NIC line rate is 49.7):

| Term | % of ALL cycles |
|---|---|
| `__pi_memcpy` (nvme-tcp RX skb→app O_DIRECT pages) | **55.4 %** |
| `__skb_datagram_iter` + `_copy_to_iter` + usercopy hardening | ≈ 9.2 % |
| skb/page-pool/GRO/mlx5e machinery (`napi_pp_put_page` 5.1, `sock_rfree` 3.4, mlx5e cqe/frag 4.5, gro 2.2, …) | ≈ 15 % |

**≈ 75–80 % of the entire client at the read ceiling is the RX copy
and its riders.** This is the counted proof of the charter's "the raw
ceiling is itself RX-copy-bound": deleting the RX copy (zcrx/DDP
class) is the only thing that moves the 44–46.6 ceiling toward line
rate — and §2 shows every path to it is closed on this venue.

**Row C — il EXA read, psync 1M nj32 cold + shim: 34.37 GB/s clean /
32.64 w/perf** (engagement 1.156–1.158, DRAM 6.72 B/B, ledger closure
exact: dest 596.5 + arena 1788.6 ≡ ipc_bytes_out 2385.1 GB):

| Term | % of ALL cycles |
|---|---|
| client `slab_read` arena→app (`__memmove_avx512` in fio) | 19.5 % — structural POSIX |
| daemon arena serves (svc threads `write_at` warm + tpc E-IL2 dest serves) | ≈ 26–28 % — the lawful pass (NT counted negative here; arena exemption stands) |
| nvme-tcp RX copy class (`__pi_memcpy` 17.9 % + iter/machinery ≈ 8 %) | ≈ 26 % |
| FUSE commit machinery | **0 % — absent, as designed** |

Row C is the live measurement of what deleting ONE interface copy is
worth (+6.8 GB/s over Row A canonical), and simultaneously the bound
on what zcrx would add on top: the RX class is now the largest term on
the il row too.

## 4. The ranking (written before any build, per charter — and where it ends)

| Rank | Candidate | Expected win @ reset-v4 shapes | Cost/risk | Status |
|---|---|---|---|---|
| 1 | **il adoption** (candidate 3b — the commit-copy deletion that already ships) | **+6.8–7 GB/s** qd8 counted (34.11 vs 27.33 canonical); commit machinery = 32 % of Row-A cycles | zero build — ops lever (LD_PRELOAD any EXA-class psync/libaio workload; coverage per `docs/design-preload-interception.md`: posix + libaio interposers; N/S by design: mmap-coherent mixes, io_uring-native apps, O_APPEND/O_SYNC) | **AVAILABLE NOW**; no cheap widener found beyond what ships — widening = field application onboarding, not daemon code |
| 2 | **io_uring zcrx userspace NVMe/TCP read-fill lane** (candidate 1) | RX class = 16–26 % of FS-row cycles, 75 % at the raw ceiling ⇒ est. **+3–5 GB/s** kern, il toward ~40+, ceiling → line rate | weeks-class build (io_uring-native initiator lane, cohort-shareable zcrx rope fills), **driver-blocked** (§2.1) + approval-gated (HDS/ntuple toggles) + no prototyping venue off the field box | **BLOCKED on this venue** — re-open on a kernel/driver whose RINGS_GET reports `tcp-data-split` (probe retained) |
| 3 | **nvme-tcp DDP offload** (candidate 2) | same term as rank 2 (its NIC-hardware twin) | requires a kernel that carries the never-upstreamed ulp_ddp patchset — kernel replacement, out of scope | **ABSENT — there is no toggle to approve**; the DDP approval question dissolves on census facts |
| 4 | **daemon DRAM/alloc slimming** (candidate 4) | beyond-copies daemon CPU = 5.3 % of Row-A cycles; dhat pass (§5): read path is alloc-clean — nothing cheap clears even 1 % | — | **BELOW THE BAR** — filed, not built |
| — | TZC / devmem / FUSE-passthrough / MSG_ZEROCOPY variants | — | — | dead per matrix (§2) |

**STOP-clause invocation:** the charter's bar ("if ALL candidates
bound below ~1 GB/s expected win, stop after Phase 1") is met in
spirit and letter for *buildable* candidates: rank 1 is already
product, ranks 2–3 are unavailable on this venue regardless of effort,
rank 4 counts below ~0.4 GB/s realistic. Phase 2 was therefore not
entered; nothing shipped; the field pair is unchanged.

## 5. The candidate-4 rider (charter-mandated dhat pass — report-only)

Dev box, `--features dhat-on` build of the branch tip (binary-identical
tree to dev tip; docs-only delta), fresh devsub format, kernel-path
fileset write 8×256 MiB O_DIRECT + libaio 1M qd8 read pass (2.1 GB
dest-serve bytes, 2.4 GB fill DMA on the stats delta), clean-unmount
dhat dump (`dhat-heap.json`, 1.74 GB / 474,730 blocks total across the
whole mount life):

* **The kernel read path is allocation-clean per-op**: the only
  op-scaling sites are the fuse3 `TpcScheduler::spawn` task boxing
  (~3 allocs ≈ 13 KB/op — the known handler-lane venue cost) and the
  pooled-buffer REFILLS (`cache::pool::alloc_pooled`, 137 × 4 MiB —
  bounded churn, not per-op). Write-side micro-allocs
  (`try_allocate_block` 64k × 24 B, publish-pass 16k × 16 B) are
  write-plane, out of this charter.
* Field-capture cross-check: daemon beyond-copies CPU is 5.3 % with no
  sub-term ≥ 1.4 % (and the largest — vdso clock reads — is shared
  with fio's own clat timing and feeds the charter-mandated ALWAYS-ON
  phase instruments; downgrading it is instrument-hostile).
* The "~6 B/B DRAM beyond copies" is mostly **kernel-interface
  machinery, not daemon fat**: kern 10.26 vs il 6.72 B/B on matched
  rows this window — deleting the FUSE commit term removed ≈ 3.5 B/B
  of "machinery" with zero daemon change. What remains beyond-copies
  on il (≈ 2.5 B/B over the copy prediction) is skb/page-pool overhead
  riding RX (§3 Row B: ~15 % machinery next to the 55 % copy) plus
  per-op page-table/task traffic. Verdict: **no daemon-side fix worth
  its regression risk; filed.**

## 6. No-regression + venue state

No binary changed hands: every field row ran the standing `1b936bf`
pair, read-only. Spot checks against the canonical table: rd-kern qd8
28.61 w/perf (≥ canonical 27.33), rd-il 34.37 clean (canonical 34.11,
engagement 1.156, closure exact), raw read 46.6 (record family 44–45 —
consistent, this rep ran with the FS mounted idle and perf attached).
Store left unaged (reads only); settle green before/after every row;
standing pair verified mounted + armed at SESSION END; wedge
tripwires (`ipc_sessions_poisoned`, `ipc_descriptor_rejects`,
`fsck_findings`, `writer_guard_fenced`) zero.

Incidents (journaled live, both contained): (1) the box root fs (16G)
filled mid-window — root cause was a **stale iostat from 07/29** (a
prior-session leftover, pid 706822) that had grown `/tmp/io_.txt` to
1.09 GB, compounded by this window's perf captures under `/tmp`;
killed + removed, all ifr captures moved to `/scratch/tmp/ifr/`,
root fs recovered to 96 % (note: `/root/.debug` carries 1.4 GB of
accumulated build-id caches from every session's perf records —
flagged for operator pruning, not touched). One il row's summary
here-docs died in the ENOSPC window; the row's persisted artifacts
were recomputed and journaled as CORRECTED (34.37 GB/s, engagement
1.156) and a fresh capture-carrying rep was run clean. (2) none other.

## 7. Where the frontier now ends (the honest statement)

1. **Daemon copy floor: reached** (read-copy-count campaign) — one
   lawful CPU pass per served byte on both transports; this campaign
   adds the whole-box proof that everything larger is kernel-side.
2. **The two interface terms are now priced to the cycle** on this
   hardware: FUSE commit ≈ 32 % of kern-row cycles (memcpy 16.9 +
   lock discipline 12.8 + pin 2.6), nvme-tcp RX ≈ 16–26 % of FS-row
   cycles and ~75 % of the raw ceiling. Both are counted, attributed,
   and **closed to host-side software on this venue**: the shipped il
   lane already deletes the first; the second has no available
   mechanism (matrix §2) without a kernel/driver change or network
   reconfiguration the user has ruled out.
3. **Ranked residuals** (all outside daemon scope, retained for the
   day the preconditions change):
   1. mlx5 build with HDS/`tcp-data-split` control ⇒ re-open rank 2
      (zcrx lane; the §2.1 probe is the gate; NIC toggles will need
      user approval at that point).
   2. Upstream FUSE commit-path economy (batch `FR_LOCKED`/pin
      registered payloads once) ⇒ ~13 % of client cycles on kern
      reads; upstream-submission candidate, not a field deploy.
   3. Kernel with ulp_ddp (vendor kernel or future upstream) ⇒ rank 3
      revives with zero protocol change.
   4. il adoption breadth in the field (ops): every EXA-class
      psync/libaio workload that can carry LD_PRELOAD gets +20–25 %
      read today.
4. Anything further **requires the class of change the user has
   excluded** (fabric/RDMA conversion, MTU geometry, kernel
   replacement) — documented and stopped on those terms, per the
   STOP-clause.

## 8. Client state

Standing pair `1b936bf` mounted + armed at SESSION END (redeployed
cold several times during the window; final deploy healthy, settle
green). No new pair was shipped (nothing built) — `/scratch/tmp/*.ifr`
holds the census/probe artifacts (`hds_query.{c,ifr}`, row wrapper,
per-row artifacts under `/scratch/tmp/ifr/rows/`), not binaries. The
one perf capture retained on-box lives at
`/scratch/tmp/ifr/ifril2.data`; the /tmp copies were removed with the
incident cleanup. Journal: SESSION START/END + every row + both
incident lines in `/scratch/tmp/agent_runs.log`.
