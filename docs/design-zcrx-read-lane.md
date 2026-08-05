# Design: the zcrx read lane — a userspace NVMe/TCP initiator for cold read fills

Rev 3 — 2026-08-04. Branch `perf/zcrx-z3` (Rev 2: `perf/zcrx-lane-z2`;
Rev 1: `perf/zcrx-lane`, 2026-08-03). Status: **Phase-1 bracket GO**
(`.benchmarks/2026-08-03-zcrx-lane.md`); PR Z1 shipped the initiator
core + probes + gauges + opt-in wire-in; PR Z2 shipped the
zcrx recv backend — the area/refill/gather machinery, the steering
state machine + live ethtool/netlink surface, and the raw
`REGISTER_ZCRX_IFQ`/`RECV_ZC` driver (local contracts + loom;
live-NIC execution field-owed — `.benchmarks/2026-08-04-zcrx-z2.md`);
**PR Z3 (this branch) ships MEM-3 cancellation custody + gather-serve
fusion** (`.benchmarks/2026-08-04-zcrx-z3.md`); default-on adjudication
stays field-owed behind the D5 gate chain (MEM-3 ✓ → TEST-6 ✓ — the
in-module contract suite `src/zcrx_lane/uring_zcrx.rs::tests` — → Z3
field rows, now the chain's remaining link).

**Rev 3 amendments (Z3 as built):**

* **Gather fusion (the PERF-1 win)**: registered-destination funnel
  reads (`dest_addr` — the routing raw full-block DMA leg that serves
  EXA-class cold reads, and the R3 ranged zero-copy leg) are
  lane-eligible: each sub-command's ONE completion gather lands
  DIRECTLY in the caller's dest (`LaneSession::read_into_dest`). The
  Z2 intermediate on these shapes (gather → pooled bounce → upstream
  serve copy) is deleted — dest-less pooled reads keep the Z2 shape by
  design (§4.4: memory that outlives the serve pays the pooled
  gather). New gauge `zcrx_dest_gather_bytes` = the fused SUBSET of
  `zcrx_gather_bytes` (a fused row must account its dest-read bytes;
  `gather − dest_gather` is the remaining two-pass traffic). Routing's
  `read_dest_dma_bytes` keeps counting dest-leg bytes (ledger closure
  unchanged); the lane gauge separately attributes the CPU pass.
* **Dest fusion is AREA-BACKEND-ONLY by law**
  (`dest_serve_eligible()`): the classic backend's reader task writes
  destinations from a FOREIGN task — the MEM-1 hazard class for
  registered ent/arena memory under cancellation. Area backends gather
  on the REQUESTER: a dropped future gathers nothing, so no lane
  context ever writes a registered dest after its op resolves.
  Classic ineligibility is not a fallback (the ≈ 0 gauge stays
  honest); per-op lane errors retry on the kernel path into the SAME
  dest (idempotent — partial gathers overwritten).
* **MEM-3 cancellation custody (the D5 gate chain's first link,
  closed)**: CID + depth permit live in the PENDING ENTRY as RAII
  (`CidSlot`), returning exactly when the driver destroys the entry —
  never on the requester's happy path, so a dropped future leaks
  nothing (pre-fix: `queue_depth` cancellations emptied the pool and
  the lane degraded for the mount lifetime). Classic entries own a
  destination keep-alive (`read_into_pooled` — a clone of the pooled
  `Bytes`; `read_into_slice` bounces through an op-owned allocation),
  so the pool can never recycle a buffer the reader still holds a span
  pointer into; `read_into_ptr` keeps a documented raw contract
  (caller-owned lifetime past cancellation — area backends meet it
  trivially). Area admission permits ride `PendingFill` → `ZcrxFill`
  (released after the gather or in the dead completion channel —
  exact accounting under future-drop). Drain discipline:
  poison(drain=true) only where the destination writer provably writes
  no more (reader/driver exiting, or abort+JOINED); writer-side
  failures poison flags only. **Cancellation is NOT a poison event** —
  `zcrx_lane_poisoned` stays an honest must-stay-0 tripwire under
  default-on. (Deliberate deviation from the pre-rc spec's minimal
  poison-from-drop-guard prescription; same acceptance, lane
  survives.) No lock-free protocol changed — `SpanLedger` + its loom
  models untouched (re-run green).
* **Microbench**: `benches/zcrx_bench.rs` `zcrx_gather` group — the Z2
  two-pass vs Z3 fused gather at the 128 KiB MDTS-face and 4 MiB
  whole-block span shapes (sim venue, real area/ledger machinery).

**Rev 2 amendments (Z2 as built):**

* **Completion-gather posture (Z2)**: a fill completes as a scatter list
  of refcounted area spans (`ZcrxFill`); the funnel's destination is
  filled by ONE gather pass at completion (`zcrx_gather_bytes` ≈
  `zcrx_fill_bytes` in Z2). This keeps §4.4's pass count: the kernel
  path's RX copy is replaced 1:1 by a userspace gather, which Z3 then
  FUSES into `serve_copy_to_dest` (deleting the standalone pass — the
  Phase-1 CPU win lands fully at Z3). Chunk-backed `Bytes` never crosses
  the funnel in Z2: downstream tiers may retain served `Bytes`
  indefinitely, and a pinned chunk is admission starvation — the area is
  never a cache tier (§4.4 law, upheld by construction).
* **Admission law**: in-flight admitted payload per queue ≤ HALF the
  area (derived, floor one chunk) — the other half absorbs delivery
  slack (short-recv fragmentation, headers riding payload chunks).
  Parking counts `zcrx_area_admission_waits`; exhaustion backpressures
  ADMISSION, never mid-stream (§4.3 pinned by the contract suite).
* **Span-record refill (real backend)**: the recycle grain is the CQE
  span (one rqe per span, off/len echoed); grant-ledger slots are span
  RECORDS (count = area chunks; rqe ring 1:1 next-pow2 per §8), and the
  ledger's free stack IS the refill feed — a freed record posts its rqe
  before re-grant. Slot exhaustion (pathological frag) poisons loud.
* **Ordering note**: per-queue TCP connect + IO Connect happen BEFORE
  ifq registration (both before steering, which stays LAST — every
  pre-steering refusal leaves the NIC byte-identical). Registration
  runs ON the driver thread (SINGLE_ISSUER + DEFER_TASKRUN law) with a
  ready→steer→go handshake; RECV_ZC arms only after steering.
* **AREA_SIM contract venue**: `SQUEEZEFS_ZCRX_LANE_AREA_SIM=1` arms
  the REAL area/parser/ledger/gather/poison machinery with socket recv
  standing in for NIC DMA (chunk geometry shrinkable via the
  `SQUEEZEFS_ZCRX_LANE_SIM_CHUNK` test lever to force header splits
  across chunk seams). The io_uring syscall surface is exactly the seam
  boundary — everything above it is contract-tested locally; never a
  product posture.
* **New gauges (§9 extension)**: `zcrx_area_bytes` (R5 `zcrx_area`
  component source), `zcrx_gather_bytes` (the priced completion pass),
  `zcrx_area_admission_waits` (honest backpressure),
  No-harm posture (round 8): admission over-demand DECLINES to the
kernel path immediately (`zcrx_area_admission_waits` counts declines,
one per declined READ), and admission is WHOLE-READ ATOMIC — a read's
segments admit in one try-acquire per queue, so a partial hold can
never shred the window (per-segment admission let racing multi-segment
reads each hold one segment while the sibling declined: the window
admitted fewer whole reads than its arithmetic capacity, worst case
zero); at any decline instant a full window's worth of whole reads is
admitted and completing.
`zcrx_gather_bytes` closure note (round 7): gather ≡ fill holds
UNCONDITIONALLY — gather is counted at the whole-read success boundary,
so a torn multi-segment read (poison/op-error mid-read) contributes to
neither counter and a poisoned row still closes byte-exact.
`zcrx_recv_parks` (refill-starvation EPISODES — ENOMEM/ENOBUFS from
the provider pool; the parked recv retries at poll cadence: flow
control, never a poison; sustained growth = area undersized for the
offered in-flight demand — 2026-08 field finding E),
`zcrx_recv_failovers` (park episodes that exceeded
`LANE_READ_TIMEOUT/32`: pending fills failed over to the kernel path —
fallback, not poison — and reads bypassed the lane until recovery; the
round-5 blast-radius instrument),
`zcrx_lane_poisoned` (session poison transitions — REAL transport
poison only: orderly teardown and refill-starvation failovers are NOT
poison, round 6 — must-stay-0
  tripwire; poison also drops `zcrx_lane_armed` and the lane stays
  kernel-path for the mount lifetime).
* **R5**: Red blocks NEW lane arms (`arm_admission` in the ladder);
  the `zcrx_area` component is non-sheddable (fixed registered DMA
  memory) — in-flight converges by completion, teardown credits the
  gauge.

## 1. Charter and the term this deletes

The READ copy ledger (`.benchmarks/2026-08-02-read-copy-count.md` §3.1)
priced the kernel nvme-tcp RX copy (skb → destination pages,
`__pi_memcpy` under `__skb_datagram_iter`) at **0.69–0.72 CPU passes per
user byte** on FS reads and **55.4 % of ALL client cycles at the raw
read ceiling** (46.6 GB/s, `.benchmarks/2026-08-02-interface-frontier.md`
§3 Row B) — the term that caps raw reads at 44–46.6 while TX-zero-copy
writes reach the 49.7 NIC line rate. The interface-frontier campaign
initially closed this cell as driver-blocked; the 2026-08-03 correction
addendum retracted that (probe-spelling false negative — `TCP data
split: on` on BOTH fabric ports under ethtool 7.1, mlx5 zcrx in-tree
from 6.17, `CONFIG_IO_URING_ZCRX=y` on the field kernel 7.1.2). The
mechanism is OPEN.

**Product shape (this design):** the daemon owns a **userspace NVMe/TCP
initiator lane** whose receive side is **io_uring zcrx**
(`IORING_OP_RECV_ZC` + `IORING_REGISTER_ZCRX_IFQ` + refill ring) — RX
payload is DMA'd by the NIC into a daemon-registered area and never
copied by a CPU. The lane serves **cold read fills only**; kernel
nvme-tcp keeps everything else (writes, metadata, admin, discovery,
multipath, error recovery). The lane is a *second host association* to
the same target subsystems the kernel initiator already uses — it never
replaces the kernel connection.

## 2. Phase-1 evidence (the bracket that prices the program)

Counted on the field client (2× ConnectX-7 200GbE, Rocky 8.10,
kernel-ml 7.1.2), standalone bench pair (`.zcrx-scoping/`), classic
io_uring RECV vs RECV_ZC, identical ring geometry, pattern-validated,
per-queue engagement exact (full table + method:
`.benchmarks/2026-08-03-zcrx-lane.md`):

| shape | classic recv | zcrx | delta |
|---|---|---|---|
| 1 queue, 4 conns, open-loop | 10.21/10.33 GB/s (copy core saturated) | 20.93/22.21 GB/s | **~2.1× ceiling** |
| 4 queues @ 1 port, matched 24.79 GB/s | 3.32 busy cores, 0.50 DRAM-read B/B | 1.06 cores, 0.018 B/B | **−68 % CPU** |
| 8 queues @ 2 ports, matched 49.5 GB/s (line rate) | 7.94 cores, 0.69 B/B | 2.75 cores, 0.035 B/B | **−65 % CPU at line rate** |

The RX copy pass and its DRAM read-back are gone (0.69 → 0.035 B/B);
what remains on the zcrx side is softirq/page-pool machinery. Projection
on the FS rows (RX class = 16–26 % of row cycles): **+3–5 GB/s** kern
read, il toward ~40, raw ceiling → line rate.

## 3. Non-negotiables inherited

* **io_uring-native throughout**: the lane's receive is RECV_ZC on its
  own rings; its transmit (command capsules, tiny) rides the same ring
  (`Send`/`Write` SQEs). No classical fallback path ships as a product
  posture — see §7 (a lane that cannot arm zero-copy DOES NOT ENGAGE;
  today's kernel path is byte-identical).
* **Portable by default**: every capability is runtime-probed (kernel
  zcrx surface, NIC HDS state, transport type per device); no CPU/NIC
  model tables. Absent capability ⇒ structurally inert.
* **No fixed constants**: queue count, queue depth, area size are
  derived (§8).
* **Zero-copy / latch-free hot path**: fills land in the zcrx area by
  NIC DMA; the ONE lawful serve pass (ledger R-S) becomes a *gather*
  over area chunks (§6). No locks on the completion path; refill-ring
  recycling is single-owner per queue.
* **D0/fencing untouched**: the lane issues exactly {ICReq, Connect,
  Property Get/Set, Read}. There is no write/reservation encode path in
  the initiator *by construction* (the codec exposes no H2CData/write
  builder), so custody, PR, and fencing surfaces are unreachable. NVMe
  PR Write-Exclusive on data namespaces (D0/WERO) permits reads from
  other hosts by definition; the lane never joins a reservation.

## 4. The NVMe/TCP mini-initiator surface

### 4.1 Discovery (sysfs, zero new config)

The lane rides the kernel initiator's own attachment: for a data device
`/dev/nvmeXnY` the daemon reads
`/sys/class/nvme/nvmeX/{transport,address,subsysnqn}` and
`/sys/block/nvmeXnY/{nsid,queue/logical_block_size}`. `transport==tcp`
is the eligibility predicate; `address` yields `traddr`/`trsvcid`;
`hostnqn`/`hostid` are read from `/etc/nvme/{hostnqn,hostid}` when
present (field parity with the kernel host — target `allow_host` lists
keep working) else generated per process. Multipath devices resolve to
their primary live path's controller.

### 4.2 Association (per device, per lane queue)

* **ICReq/ICResp**: PFV 0, HPDA 0, digests OFF (standing perf-fleet
  posture; an ICResp asserting digests terminates the arm loud — never
  silently accept a per-byte CRC pass).
* **Admin queue (qid 0)**: Fabrics Connect (RECFMT 0, cntlid 0xFFFF,
  KATO 0 — keep-alive disabled; the lane is same-host-managed and
  fails loud on TCP errors; Identify-based re-validation is Z2), then
  Property Get CAP (MQES), Property Set CC.EN, Property Get CSTS until
  RDY.
* **IO queues (qid 1..N)**: one TCP connection per lane queue, Connect
  with the admin-assigned CNTLID, SQSIZE derived (§8).
* **Read command**: opcode 0x02, Transport SGL Data Block (0x5A),
  SLBA/NLB from the byte range (LBA-aligned by predicate), payload
  returned as C2HData PDUs (in-order per command, interleaved across
  commands), completion via CapsuleResp or C2HData SUCCESS-flag
  elision. Both accepted.

### 4.3 The PDU-framing reality under zcrx

zcrx delivers **raw TCP payload** — PDU headers arrive interleaved with
data in the same area chunks. The receive loop is therefore a streaming
PDU parser over a chunk sequence:

* Header bytes (CH + PSH, ≤ 24 B, possibly split across chunks) are
  copied into a tiny stack scratch — bounded, priced: < 24 B per PDU ≈
  0.0015 passes/byte at 16 KiB C2HData grains. This is the "honestly
  priced edge copy" and it is negligible *because it excludes payload*.
* C2HData payload spans are **never copied at fill time**: the parser
  records `(chunk, offset, len)` refs into the command's scatter list
  (`ZcrxFill`), each ref holding a refcount on its area chunk.
* Chunk recycling: a chunk returns to the refill ring when its refcount
  drops to zero (all referencing fills served/dropped). The refill ring
  is sized 1:1 with area chunks (§8) so recycling can never stall the
  ring while refs are outstanding — instead, area exhaustion applies
  backpressure at command admission (bounded in-flight fills), never
  mid-stream.

### 4.4 Serving from the area (the gather law)

Ledger composition (the closure law stays byte-exact):

* The demand serve (ledger R-S, the ONE lawful pass) becomes a
  **gather**: `serve_copy_to_dest` walks the scatter list into the ring
  ent / arena dest. Same single CPU pass, counted in
  `read_copy_dest_bytes` exactly as today; NT-store policy applies
  unchanged (ring-ent dests NT, arena dests cached).
* `read_fill_dma_bytes` (pooled-DMA fill provenance) is NOT incremented
  by lane fills; `zcrx_fill_bytes` is the lane's fill-provenance
  counter. Extended closure: every served byte's fill provenance ∈
  {pooled DMA, zcrx area, dest DMA}.
* Tier admission (R1b second-touch) and read-lane hold retention
  require memory that outlives the fill: those legs pay an explicit
  gather into a pooled buffer, counted in the existing admission
  machinery — governed, minority by design (streamed cold blocks skip
  publish). The zcrx area is never a cache tier: refs are
  serve-lifetime only (bounded by the §5.4-class one-invocation law).

## 5. zcrx receive backend (PR Z2)

One lane queue = one io_uring (DEFER_TASKRUN | SINGLE_ISSUER | CQE32) =
one ifq (REGISTER_ZCRX_IFQ) = one NIC RX queue = one TCP connection
(plus its share of command TX). Multishot RECV_ZC; refill-ring tail
published per drain batch.

* **Queue isolation**: at arm, the lane (a) picks the NIC/port by route
  lookup toward `traddr`, (b) claims the highest-indexed RX queues,
  (c) shrinks the RSS indirection set to exclude them (`ETHTOOL_MSG_
  RINGS`/RSS netlink), and (d) installs ntuple 4-tuple rules steering
  ONLY its own connections' flows to its queues. All three changes are
  recorded and reverted at disarm/unmount; arm-time also sweeps stale
  lane rules from a previous crash (rules carry a reserved `loc` range
  — the crash-residue law; a leaked rule matches a dead 4-tuple and is
  inert but must be reaped). Steering failure ⇒ arm fails loud (a
  mis-steered flow degrades zcrx to its kernel copy fallback silently —
  ban by construction, verified by the engagement gauge).
* **Registration order**: area mmap (+ NUMA bind, §8) → ifq register →
  connect → steer → verify first fill's chunk provenance == area.
  Teardown in reverse; closing the ring fd is the crash path (kernel
  restarts the queue and reclaims the provider — no persistent NIC
  state beyond the reapable rules).
* **HDS precondition**: `tcp-data-split on` (+ thresh 0) verified via
  ethtool netlink at probe time — the corrected genetlink probe (attr
  `ETHTOOL_A_RINGS_TCP_DATA_SPLIT`, both spellings lesson pinned:
  compare the ATTR, not a rendered string).

## 6. Wire-in (PR Z1 scope)

`NvmeBlockDev::read_block_with_dest_inner` is the single funnel: when
the lane session for `device_path` is armed and the op is eligible
(`dest_addr.is_none()`, LBA-aligned offset+size, size > 0), the read is
served by the lane; ineligible or lane-error ops ride the existing
uring worker unchanged. Lane per-op failure = loud log (rate-limited) +
kernel-path retry + `zcrx_fill_fallbacks` (must stay ≈ 0; reads are
idempotent so the retry is safe by construction); lane session death =
disarm + permanent kernel path for the mount lifetime + loud log. Arm
is **opt-in** (`SQUEEZEFS_ZCRX_LANE=1`) until the Z2/Z3 counted field
brackets adjudicate a default.

PR Z1 ships the initiator over classic in-process recv (`read_exact`
into the destination — copy-parity with the kernel path, no win, the
CONTRACT venue) so every framing/association/serve law is pinned by
cargo tests against an in-process mock target; the zero-copy backend
refuses to arm until Z2. The classic backend is a test seam and a
`SQUEEZEFS_ZCRX_LANE_FORCE_COPY=1` measurement lever, never a product
posture.

## 7. Failure and capability laws

| Condition | Behavior |
|---|---|
| Kernel lacks RECV_ZC / REGISTER_ZCRX_IFQ | probe fails → lane never arms → **byte-identical today's path** |
| NIC lacks/disables tcp-data-split | same (probed per port at arm) |
| Device not nvme-tcp (local NVMe, loop) | ineligible at discovery — inert |
| ICResp demands digests / PFV mismatch | arm fails loud |
| Mid-stream framing violation (bad plen/type/datao) | session poisoned, disarm loud, kernel path (tripwire counter) |
| Read completes short / status != 0 | op fails to the kernel-path retry; counted |
| R5 Red | lane area is a non-sheddable component registered at arm; Red blocks NEW lane arms and sheds nothing (area is fixed); in-flight converges by completion |
| Fenced mount (`writer_guard_fenced`) | lane is read-only and keeps serving reads exactly like the kernel path; no interaction |

## 8. Derived sizing (no fixed constants)

* **Lane queues per device**: `clamp(possible_cpus / 8, 1, nic_queues / 4)`
  — bounded so the RSS set keeps ≥ ¾ of the NIC queues.
* **Queue depth (SQSIZE)**: `min(MQES + 1, derived_inflight)` where
  `derived_inflight = clamp(bdp_bytes / block_size, 4, 64)`;
  `bdp_bytes` from link speed (sysfs) × measured connect RTT class.
* **Area per queue**: `depth × max_stored_image_len(block_size)`
  rounded to PMD, floor one PMD; PMD-aligned mmap (the thp.rs
  `map_shared_pmd_aligned` law), `MADV_HUGEPAGE`, NUMA-bound to the
  NIC's node (`numa_core::is_local_choice` map), populated at arm.
* **Refill ring entries**: area chunks (1:1), pow2.

## 9. Observability (stats inode)

`zcrx_lane_armed` (0/1 per mount), `zcrx_fills`, `zcrx_fill_bytes`
(fill-provenance engagement — a lane row is INVALID unless its delta
accounts for the row's cold-fill bytes), `zcrx_fill_fallbacks`
(per-op lane→kernel retries, ≈ 0), `zcrx_frame_violations`
(must-stay-0 tripwire), `zcrx_conn_errors`, `zcrx_area_bytes` (R5
component gauge), `zcrx_hdr_copy_bytes` (the priced header edge copy —
bounded ≪ 1 % of fill bytes by construction),
`zcrx_dest_gather_bytes` (Z3 — the fused-serve subset of
`zcrx_gather_bytes`: gathers that landed directly in registered
dests; a fused row is engaged iff its delta accounts for the row's
dest-read bytes). Ledger closure extension per §4.4.

## 10. PR sequence + merge bars

* **Z1 (this branch)**: `src/zcrx_lane/` (pdu codec, initiator,
  probes, gauges), funnel wire-in, mock-target contract suites, design
  doc, Phase-1 evidence note. Bar: full cargo gate; read family +
  copy-ledger suites green; lane default-off inert (byte-identical
  paths proven by the no-arm contract test).
* **Z2** (shipped 2026-08-04, `perf/zcrx-lane-z2` —
  `.benchmarks/2026-08-04-zcrx-z2.md`): zcrx recv backend (area/refill/
  gather + poison lattice + R5, contract-tested via AREA_SIM; grant
  ledger loom-modeled, weakening-verified ×3), steering state machine
  (mock-proven record/apply/rollback/restore/reap) + live EthtoolNic
  (ioctl) + genetlink HDS probe, raw `REGISTER_ZCRX_IFQ`/`RECV_ZC`
  driver. The FIELD bar carries to the reformat window (no zcrx-capable
  NIC exists locally): engagement exact (fill provenance == area),
  `zcrx_frame_violations`=0, wedge tripwires 0, A-B-B-A cold-read
  bracket vs `SQUEEZEFS_ZCRX_LANE=0`, loaded soak.
* **Z3** (shipped 2026-08-04, `perf/zcrx-z3` —
  `.benchmarks/2026-08-04-zcrx-z3.md`): MEM-3 cancellation custody
  (the D5 gate chain's first link) + gather fusion on
  registered-destination funnel reads (`read_into_dest` — the Rev 3
  amendments above) + the `zcrx_gather` microbench pair. Local bar
  met (contract suites, loom re-attested, clippy/fmt, bench smoke).
  **Field-owed remainder** (the reformat-window bracket): live-NIC
  fused rows (sustained ≥ 60 s, both substrates), NUMA placement
  retune, Identify-verify hardening, derived-sizing retune vs real
  C2HData grain, and the default-on adjudication behind the D5 gate
  chain (MEM-3 ✓ → TEST-6 ✓ (`src/zcrx_lane/uring_zcrx.rs::tests`) →
  Z3 rows + engagement laws — the field rows are the remaining link).

## 11. Residual risks (named)

1. **Fallback-copy contamination**: packets reaching a zcrx queue
   outside the provider (retransmits assembled from linear skbs, flows
   mis-steered during rule churn) are copied by the kernel into area
   chunks — correct but silently un-zero-copy. Gauge: periodic
   provenance sampling is Z2 scope; the Phase-1 bench proved steering
   fidelity is achievable (per-queue byte deltas exact).
2. **Area sizing vs C2HData grain**: HW-GRO coalescing determines
   chunk-span shape; derived sizing must be re-bracketed on the field
   NIC (Z2).
3. **Target multi-association limits**: nvmet default allows it; a
   target with restrictive `attr_allow_any_host`/cntlid ceilings
   surfaces at Connect — arm fails loud, kernel path intact.
4. **Keep-alive**: KATO 0 in v1; if a field target enforces nonzero
   KATO, arm fails loud at Connect and Z2 adds the keep-alive tick.
