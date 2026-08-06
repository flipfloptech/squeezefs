# Design: the zcrx read lane — a userspace NVMe/TCP initiator for cold read fills

Rev 4 — 2026-08-06. Branch `perf/zcrx-engagement` (Rev 3:
`perf/zcrx-z3`; Rev 2: `perf/zcrx-lane-z2`; Rev 1: `perf/zcrx-lane`,
2026-08-03). Status: **Phase-1 bracket GO**
(`.benchmarks/2026-08-03-zcrx-lane.md`); PR Z1 shipped the initiator
core + probes + gauges + opt-in wire-in; PR Z2 shipped the
zcrx recv backend — the area/refill/gather machinery, the steering
state machine + live ethtool/netlink surface, and the raw
`REGISTER_ZCRX_IFQ`/`RECV_ZC` driver (local contracts + loom;
live-NIC execution field-owed — `.benchmarks/2026-08-04-zcrx-z2.md`);
PR Z3 shipped MEM-3 cancellation custody + gather-serve
fusion (`.benchmarks/2026-08-04-zcrx-z3.md`); the eight-round Z3 field
campaign (`.benchmarks/2026-08-05-zcrx-z3-field-rows.md`) landed
correct-and-safe and PARKED the lane on engagement economics (0.05 %
engagement could not pay the RSS rent); **PR Z4 (this branch) is the
ENGAGEMENT campaign** — phase 2 of the read copy-elimination program
(`.benchmarks/2026-08-06-read-cpu-wall.md`: reads are whole-box
CPU-bound, so every zero-copy RX byte is direct capacity;
`.benchmarks/2026-08-06-read-dest-lease.md` FIELD ACCEPTANCE: 27.9 of
41.8 raw after phase 1, the 85 % bar = 35.5). Default-on adjudication
stays field-owed behind the D5 gate chain (MEM-3 ✓ → TEST-6 ✓ → the
Rev-4 engagement field rows, §13).

**Rev 4 amendments (the engagement geometry — all derived, no
constants):**

* **The census-driven eligible pool** (`steering::lane_eligible_queues`)
  — width `clamp(devices_via_nic, channels/4, channels/2)`, highest-
  indexed slice. The round-8 verdict's flat `/4` pool (8 of 32) left
  2 of 10 field devices with NO lane — 20 % of row bytes structurally
  kernel-path. The DEVICE CENSUS (`probe::tcp_devices_via_nic`, the
  finding-C/D machinery) now widens the pool exactly as far as fabric
  breadth demands: floor = the standing ¼ posture (a sole-device arm is
  byte-identical to Rev 3), ceiling = the kernel path (writes,
  metadata, admin, declined reads) keeps ≥ HALF the NIC's RSS width.
  Field: `clamp(10, 8, 16) = 10` → every device leases one queue (the
  two-rail 5/5 split stays at the floor 8, all covered);
  `fair_queue_want` is unchanged (`clamp(10/10, 1, 4) = 1`).
* **Full-window admission** (`area::admission_permits`) — the admitted
  payload window is the WHOLE `depth × max_xfer` fill window; the
  retired `/2` was an implicit delivery-slack budget that halved
  engagement capacity (32 MiB/queue admitted vs the ~51 MiB/device the
  cold field row offers = whole-read atomic admission declining most of
  the row). The admitted window and the CID namespace are now the SAME
  arithmetic (`depth` commands of `max_xfer`). Field: 64 MiB/queue ≥
  51.2 MiB offered — ≥ 16 concurrent whole 4 MiB reads admit (was 8).
* **The delivery slack is explicit and derived**
  (`area::delivery_slack_bytes` over `area::burst_geometry`): an
  MTU-grain payload burst lands in `⌈mtu/chunk⌉` page-grain niovs (HDS
  splits headers off; payload starts a fresh niov), so the fills' chunk
  budget carries `window × (burst − payload)/payload` extra bytes,
  PMD-rounded (field: 64 MiB × 3288/9000 → 24 MiB). Unknown MTU
  degrades to `window` — occupancy ½, the retired posture's exact
  budget. Area = **window + slack + ring-standing** (rounds 6–7
  arithmetic unchanged underneath; ≈ 184 MiB/queue on the field rail,
  R5-gauged as before — Red still blocks arms). The CQ demand and the
  round-7 either/or clamp are burst-occupancy-aware through the same
  function (`cq_entries_for` worst-case chunk-touch = `⌈max_xfer/mtu⌉ ×
  ⌈mtu/chunk⌉`; `cq_admitted_window_bytes` = spans × occupancy — the
  `× 2` half-window compensation retired with the `/2`). Sub-MTU
  segmentation storms stay the round-5/6 park governor's job — parks
  are flow control, never poison, and the round-8 structural-starvation
  teardown (no-harm) is untouched.
* **BDP depth ADJUDICATED AGAINST** (the §8 line is superseded): the
  wire-BDP depth class is a self-fulfilling equilibrium under
  demand-concurrent venues — falsified twice on the read side
  (2026-08-01 read-lane; the 2026-08-06 queue-wall audit credits the
  absence of measured-BW terms) — and at fabric RTT it derives a ~6 MiB
  window that would decline nearly everything the row offers. Depth
  stays the OFFERED-CONCURRENCY slope (`clamp(cpus × 2, 4, 64)`,
  MQES-clamped at connect): depth × max_xfer IS the admitted window,
  so the client-concurrency slope is what scales admission to offered
  demand.
* **Dest-lease composition (§12)** and the **engagement field rows
  (§13)** are specified below. Lazy re-arm after structural teardown
  stays a filed follow-on (with the sized window the structural trigger
  should not fire; if it does, the row is INVALID by the engagement law
  and gets diagnosed, not presented).

**Rev 4a amendments (engagement round 2 — the volume gates, 2026-08-06):**

The Rev-4 geometry deployed and the field rows stayed ~0 engaged
(`fill ≡ dest ≡ 0.6 GB` of a ~1500 GB row; waits 1045/2740, parks 5,
failovers 10, fallbacks 401, poisoned 0). The audit's findings and the
round-2 machinery:

* **The volume-path audit (recorded)**: EVERY device read class reaches
  the funnel — pooled cohort fills (`fetch_block_from_remote` →
  `read_nvme_block` → `BackendRouter::read_block`, dest-less; R2
  prefetch and read-lane fetches ride `get_cached_or_fetch_block` into
  the same funnel) and lease/ranged windows
  (`get_block_range_for_index` → `read_block_range`, dest-carrying)
  both land in `read_block_with_dest_inner` → `try_lane_read`; nothing
  bypasses. On a lease-armed row the ~1500 GB rode the ranged dest leg
  and REACHED the funnel; the gate was the round-5 **degraded bypass**
  (`refill_degraded()`), deliberately uncounted since round 5 — held
  latched by the park → failover → trickle-progress cycle for most of
  the row. **The bypass is now COUNTED** (`zcrx_degraded_bypasses`):
  the three volume gates are mutually exclusive counters — bypasses
  (the pool/degraded term), `zcrx_area_admission_waits` (the whole-read
  window arm — the ONLY counting decline site; the CID gate parks,
  never declines), `zcrx_fill_fallbacks` (per-op errors).
* **The waits decomposition (recorded)**: waits are whole-read-window
  declines whose permits were HELD BY PARKED FILLS (admission permits
  ride pending fills for up to `park_fail_bound()` during an episode) —
  offered ~13 concurrent 1 MiB windows/device against a 64-read window
  cannot fill it by demand alone. The admission derivation is not the
  refusing term; the pool underneath is.
* **The no-harm ECONOMICS arm** (the round-8 law made trickle-proof):
  round 8's zero-progress streak resets on ANY payload byte, so a pool
  that trickles one fill per window never proves structural while
  paying full RSS rent at ~0 engagement. The governor now keeps a
  cumulative **starved-time ledger** (monotone — trickle cannot reset
  it): starved time ≥ HALF the armed lifetime, evaluated at failover
  windows past the round-8 horizon (`REFILL_STRUCTURAL_FAILOVERS ×
  park_fail_bound()`), latches the SAME `starved_structural` →
  funnel-teardown path. Derivations: the horizon is the existing
  structural law's own time constant (this arm can never fire faster
  than the transient allowance); ½ is the majority boundary (rent is
  paid for the whole armed life, savings accrue only while serving).
  Instruments: `zcrx_starved_ms` (the ledger, exported),
  `zcrx_structural_teardowns` (escalations latched, either arm — the
  "did no-harm fire?" adjudication is a number now).
* **Round-8 non-firing, adjudicated**: at the field tape's shape the
  zero-progress arm COULD NOT fire — 5 episodes / 10 windows means
  trickle progress ended episodes between windows, resetting the
  streak each time; the economics arm exists precisely for this gray
  zone and would have fired within ~2 windows of majority-starved
  life, returning the RSS width for the rest of the row.
* **The ahead-governor feed, adjudicated (deferred — do not force)**:
  the demand/lease stream alone carries ~100 % of cold row bytes on the
  field shape, so ahead-depth is NOT required for share ≥ 0.5; and the
  ahead governor measures DELIVERY response, so once the lane actually
  serves volume its CPU savings show up in the governor's own signal —
  the honest coupling already exists through measurement. Revisit only
  if a fixed-lane field row shows demand-stream share < 0.5 with
  ahead-depth 0.
* **The pool term itself is field-owed**: whether the provider pool's
  standing demand exceeds the round-6 model on this NIC (striding-RQ
  vs legacy-RQ geometry) is only measurable live; `zcrx_starved_ms` vs
  armed wall time now names it directly (starved-share ≈ 0 ⇒ the area
  is sized; immediate majority-starvation ⇒ the ring-standing model is
  the term — fix the PROBE, never a constant).

**Rev 4b amendments (engagement round 3 — the pool term adjudicated at
the driver source, 2026-08-06):**

The round-2 discriminator fired outcome (2): A-sides `starved_ms =
9370` ≡ 10 × `park_fail_bound` (EVERY failover window starved),
`structural = 5`, fill stuck at 0.6 GB. The term was then read out of
the sqz linux-6.19.14 tree (the exact field kernel; the 27-patch series
touches io_uring/FUSE only — mlx5 is pristine stable):

* **A zcrx-provider-backed mlx5 queue runs STRIDING RQ (MPWQE) +
  SHAMPO HDS.** `mlx5_rq_shampo_alloc` is the striding arm's
  (en_main.c:961) and its unreadable-MP branch (en_main.c:826,
  `netif_rxq_has_unreadable_mp`) gives the queue a SEPARATE header
  page pool — headers on normal kernel pages, payload strides on the
  provider pool ("Shampo header data split allow for unreadable
  netmem", en_main.c:1005–1007). The queue-restart path
  (`mlx5e_queue_mem_alloc`, en_main.c:5561–5601) reopens the channel
  with `chs->params` VERBATIM — a REGISTER_ZCRX_IFQ restart changes no
  geometry, and `netdev_queue_mgmt_ops` (en_main.c:5668) has no
  per-queue ring-size hook, so `ethtool -G` stays device-global
  (option (b) of the round-3 charter is NOT expressible).
* **The real standing demand**: the RQ's provider draw when fully
  posted is `pages_per_wqe << log_rq_size` (en_main.c:940–941 — also
  the page_pool sizing), and it REDUCES to
  **`ethtool_rx_frames × linear_stride_sz`**: `log_rq_size =
  log_rq_mtu_frames − log_pkts_per_wqe` (params.c:415),
  `log_pkts_per_wqe = log_wqe_sz − order2(linear_stride_sz)`
  (params.c:292–301) — **`log_wqe_sz` cancels**, so no driver-internal
  input is needed. `ethtool -g` rx reports FRAMES (en_ethtool.c:372 —
  `1 << log_rq_mtu_frames`; the probe the lane already runs), and
  `linear_stride_sz = roundup_pow_of_two(SKB_FRAG_SZ(headroom +
  hw_mtu))` (params.c:284, :252–262; en.h:75 `SW2HW_MTU`, :79
  `MLX5_RX_HEADROOM = NET_SKB_PAD`). Field rail: 8192 × 16384 =
  **128 MiB/queue** vs the round-6 legacy model's 96 MiB — the 184 MiB
  area left the fills a 56 MiB budget against a 64 MiB fully-admitted
  window: **permanently starved**, which is exactly the tape.
* **The derivation landed** (`area::ring_standing_bytes`, probeable
  end-to-end): `max(legacy, striding)` where legacy is the round-6
  `descs × ⌈mtu/chunk⌉ × chunk` and striding is `frames ×
  mpwqe_stride_bytes(mtu)` (`roundup_pow2(mtu + ≤512 B of kernel skb
  arithmetic, ceiled — over-estimating only rounds UP at a pow2
  boundary, the safe direction)`). The RQ mode is not portably
  probeable, so max() is the round-6 safe direction — small-MTU rails
  keep the legacy floor (2 KiB strides < 1-chunk frames). Field area
  becomes ≈ 64 window + 24 slack + 128 ring = 216 MiB/queue
  (PMD-rounded), ~2.2 GiB across 10 sessions, R5-gauged — Red still
  blocks arms. The round-6 caveat "striding-RQ fleets over-provision"
  is hereby FALSIFIED and retired.
* **The whole-NIC release** (`nic_census` — the residual-rent
  adjudication): per-session teardowns DID restore their own RSS
  mid-row (the ArmedSteering restore rides `teardown()`), but the
  surviving minority held ~16 % of the queue width at ~0 engagement
  for the rest of the row — the residual 5–7 % A-vs-B gap. The pool
  term is per-NIC physics, so a structural vote that reaches the
  MAJORITY of the NIC's ever-armed sessions (the same ½ boundary as
  the per-session arm) sweeps the remaining live sessions
  (`sweep_nic_sessions` — awaited inside the voter's teardown, depth-
  one recursion by construction). Round-2's tape under this law:
  the 5th structural teardown releases all 10 sessions' width.
* **Options adjudicated**: (a) probe the real term — LANDED (above);
  (b) per-queue ring shrink — NOT EXPRESSIBLE on this kernel (cited
  above); (c) the honest stop — NOT NEEDED: the term is finite and
  probeable. A kernel-side per-queue page budget remains a nice-to-
  have, not a blocker.

**Rev 4c amendments (engagement round 4 — THE VERDICT, 2026-08-06):**

The 216 MiB area still starved (starved_ms = 9 windows, structural = 4
of 10). The deciding hypothesis — stride/chunk granularity
incompatibility between MPWQE and the zcrx provider — was adjudicated
at the source and is **FALSE**:

* **Compatibility is proven.** MPWQE "strides" are VIRTUALLY contiguous
  via the UMR: `mlx5e_alloc_rx_mpwqe` allocates `pages_per_wqe`
  INDIVIDUAL order-0 netmems per WQE (`mlx5e_page_alloc_fragmented` →
  `page_pool_dev_alloc_netmems`, en_rx.c:277–293) and maps each by DMA
  address into the WQE's inline MTTs (en_rx.c:797–808) — the device
  sees one contiguous IOVA range over discontiguous 4 KiB pages. A
  non-XSK queue's `page_shift` IS `PAGE_SHIFT` (params.c:24–34, "the
  NIC must be able to map order-0"), the UMR mode falls through to
  ALIGNED/MTT (params.c — the XSK arms are the only others), the
  provider pool is created `order = 0` (en_main.c:994), and the zcrx
  provider REQUIRES exactly `pp order + PAGE_SHIFT == niov_shift`
  (zcrx.c:1018) — the lane's 4 KiB chunks. The 16 KiB "linear stride"
  (Rev 4b) is the ethtool frames↔WQE conversion unit, never a
  physical-contiguity demand; **MTU has no bearing on provider
  compatibility**, and the 0.6 GB of pattern-correct zero-copy fills is
  the empirical co-proof. The campaign-ending stop is NOT filed.
* **The free-pool stranding theories are also closed at the source**:
  the pool's alloc path drains alloc-cache → ptr_ring → provider IN
  ORDER (page_pool.c:654–668), a full ptr_ring overflows driver
  recycles to the provider's freelist (page_pool.c:753–773 →
  `release_netmem`, zcrx.c:994–1005), and user refill returns wait in
  the rqe ring for the slow-path pull (`io_pp_zc_alloc_netmems` →
  `io_zcrx_ring_refill`, zcrx.c:920–991). Free chunks cannot strand.
  Every enumerable holder — posted WQEs (the Rev-4b standing term),
  in-flight payload (⊆ the admitted window), socket transit (⊆ issued
  ⊆ admitted), copy-fallback niovs (⊆ transit, zcrx.c:1254+),
  partial-WQE tails (≤ 2 WQEs) — is funded at 216 MiB.
* **Therefore the remaining unknown is a LIVE-INPUT question, not a
  source question** — and the round-4 instrument closes the
  discrimination gap that kept it unknowable: `classify_recv_end`
  files three DIFFERENT starvation terms under one Park verdict —
  **the park errno-class split** (`zcrx_parks_{pool_dry,rq_empty,
  cq_full}`; closure `parks ≡ dry + rq + cq` per row; the
  episode-starting errno is the counted class, and the park log names
  it). `pool_dry` = the area/pool arithmetic's face; `rq_empty` = the
  refill-posting face; `cq_full` = the lane's own reap/CQ face —
  **round 2's "every window starved" is a different bug in each
  class**, and the next field row names it in one line beside the
  per-queue `rx[i]_pp_*` ethtool counters (alloc_empty/slow +
  hold−release inflight = the pool's actual holding, readable per row
  with no binary change).
* **The census denominator is LIVE-armed** (the structural=4-of-10
  miss): `note_released` (every teardown — voter, poison, shutdown)
  removes the session from the rent denominator, because a torn
  session holds no RSS exclusion; the voter votes while still counted
  live. The round-4 tape's 4th vote now fires (2×4 ≥ 10 − 3) where
  ever-armed idled one vote short for the whole row.

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
admitted and completing. A starvation episode surviving 2 consecutive
failover windows without payload is STRUCTURAL and the funnel tears
the session down — the RSS width restores within ~2 × the failover
bound and the kernel path serves at full width for the rest of the
mount (worst-case lane-on ≈ lane-off; lazy re-arm is a filed
follow-on).
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

## 8. Derived sizing (no fixed constants — Rev 4 supersedes the BDP line)

* **Eligible pool per NIC** (Rev 4): the highest-indexed
  `clamp(devices_via_nic, nic_queues/4, nic_queues/2)` RX queues —
  census-widened so every fabric device behind the rail can hold a
  lane; the kernel path keeps ≥ half the NIC's RSS width.
* **Lane queues per device**: geometry want `clamp(possible_cpus / 8,
  1, 8)`, fair-spread `clamp(eligible / devices, 1, want)`, then the
  free-rule-slot clamp and the rxq arbiter's grant.
* **Queue depth (SQSIZE)**: `min(MQES + 1, clamp(cpus × 2, 4, 64))` —
  the OFFERED-CONCURRENCY slope. (The Rev 1–3 BDP line — `bdp_bytes /
  block_size` from link speed × RTT — is ADJUDICATED AGAINST, Rev 4:
  the wire-BDP class is a self-fulfilling equilibrium, falsified twice
  on the read side, and at fabric RTT it would decline nearly all
  offered demand. Depth × max_xfer IS the admitted window.)
* **Admitted window per queue** (Rev 4): `depth × max_xfer`, granted in
  FULL by the admission semaphore (whole-read atomic, round 8), clamped
  by the round-7 either/or law to the CQ's occupancy-aware payload
  budget.
* **Area per queue**: `depth × max_xfer` (PMD-rounded, floor one PMD)
  **+ delivery slack** (`window × (burst − payload)/payload` from the
  MTU/chunk burst geometry; unknown MTU ⇒ `window`, occupancy ½) **+
  the NIC RX ring's standing pool demand** (round 6); PMD-aligned mmap
  (the thp.rs `map_shared_pmd_aligned` law), `MADV_HUGEPAGE`,
  NUMA-bound to the NIC's node (`numa_core::is_local_choice` map),
  populated at arm.
* **Refill ring entries**: area chunks (1:1), pow2.
* **CQ entries**: `depth × per_cmd + sq` next-pow2, kernel-max-clamped,
  where `per_cmd` is the burst-occupancy worst-case chunk-touch count
  (`max(⌈max_xfer/chunk⌉, ⌈max_xfer/mtu⌉ × ⌈mtu/chunk⌉)`; unknown MTU ⇒
  2× flat).

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

## 12. Composition with the READ dest-window lease (Rev 4 adjudication)

The dest-lease (copy-elimination phase 1,
`.benchmarks/2026-08-06-read-dest-lease.md`) serves cold 4 KiB-aligned
sub-block kernel windows by aiming the ranged primitive's device DMA at
the reply's registered ent window — the serve dest-copy deleted. The
question the engagement campaign owed an answer: do the zcrx gather
path and the dest-lease DMA **compose or conflict**?

**Verdict: they COMPOSE at the funnel, by construction — and the
compose's floor on this kernel is ONE fused gather, not zero.**

* A leased window arrives at `read_block_with_dest_inner` as a
  dest-carrying, LBA-aligned ranged read — exactly the Z3 fused-gather
  shape (`dest_addr = Some`, area backend). The lane serves it with the
  ONE requester-side gather (`read_into_dest`) landing directly in the
  leased ent window; the lease's incarnation/still-check/binding-
  recheck ceremony is vehicle-blind (it wraps the device read whatever
  serves it), and `read_dest_lease_bytes ⊆ read_dest_dma_bytes` keep
  counting at the routing serve regardless of vehicle — the 2026-08-02
  ledger closure is untouched. Pinned:
  `test_compose_dest_lease_shape_rides_the_fused_lane`.
* **Zero daemon passes end-to-end is NOT expressible**: the zcrx area
  is a NIC-owned, arrival-ordered page pool — the refill ring hands
  free chunks to the NIC in delivery order, so no mechanism can aim a
  SPECIFIC read's C2HData payload at a SPECIFIC ent window; and the
  FUSE commit copy imports the REGISTER-time ent VA (the dest-lease
  kernel adjudication §1A — the reply body must sit at the ent window
  base), so the area can never BE reply-reachable memory. The serve
  from lane-area memory to the reply window is therefore exactly one
  gather — which the Z3 fusion already made the ONLY daemon pass.
* **What phase 2 alone buys (the pricing)**: on lease-served bytes the
  kernel path pays the nvme-tcp RX skb copy — 0.69–0.72 passes/byte at
  the ~2.7 GB/s/core skb-walk class (the queue-wall's measured
  per-core RX wall), the exact residual term the CPU-wall ruling names.
  Through the lane those bytes arrive by NIC DMA (zcrx-side residual
  0.035 B/B DRAM, Phase-1) and pay one userspace NT gather at memcpy
  class (~3–4× cheaper per byte than the skb walk, non-cache-
  polluting). Daemon passes on dest bytes go 0 → 1 while kernel passes
  go 0.69 → 0: net ≈ −0.2 to −0.3 core-s/GB per lane byte on a box
  measured 92 % busy at 27.9 GB/s — the freed ~6–8 cores are the
  capacity the 35.5 bar needs if the row stays CPU-elastic (the
  CPU-wall arithmetic). Pooled fills (tier-admitted / hold-retained)
  keep the Z2 two-pass shape with the kernel RX pass killed — same
  direction, one more daemon pass by design (§4.4: memory that outlives
  the serve pays the pooled gather).
* **Interaction, stated**: the lease's LANE YIELD (dest-leaseable
  streams stand the R2/ahead speculative arms down) means leased
  streams offer DEMAND windows only — per-window lane reads at client
  concurrency. That is admission-friendly (windows ≤ 1 MiB = 1
  command) and orthogonal to the whole-block fill shapes the admission
  window was sized for; no code interaction exists beyond the funnel.
* **The true zero-pass serve** stays the staged zc-serve follow-on
  (dest-lease §8.1: READ_FIXED into the kernel-registered request
  folios — kills the serve AND commit passes; sqz-kernel-only). The
  lane's gather is the floor until that kernel surface ships.

## 13. The engagement field rows (the D5 chain's remaining link — field-only)

The engagement GEOMETRY is field-only by nature (real mlx5 queues, real
RSS width, real ntuple verdicts — ntuple is ON on both rails from the
earlier campaign); the sim pins every law up to the io_uring syscall
seam (TEST-6). The acceptance instrument is
`tests/fio/zcrx_field_rows.sh` (A-B-B-A lane-on/off remounts, one
prefilled beyond-RAM fileset), run on the squeeze-test client (sqz
kernel, 32 RX queues/rail, MTU 9000, 10 data namespaces):

* **Geometry engagement (arm-time, logged + snapshot)**: 10 lane
  sessions armed (`zcrx_lane_armed = 1`, one per device — the census
  pool), RSS width 22/32 per rail, `zcrx_area_bytes` ≈ 10 × ~184 MiB.
* **Row A (lane on) vs B (off), cold seq-read ≥ 60 s sustained + the
  rand-4k face, BOTH orders**: the verdict is the A-B-B-A side medians,
  never a single order (the aging-store rule).
* **Engagement laws (a row violating any is INVALID, printed loudly,
  never presented as a lane number)**:
  - `zcrx_fill_bytes` delta ≥ **50 % of row user bytes** (the
    serve-the-bulk bar — the rig's `eng > 0.5` verdict; round 8
    measured 0.0005),
  - `gather ≡ fill` byte-exact (round-7 whole-read boundary),
  - `zcrx_dest_gather_bytes` accounts the dest-leg share (with the
    dest-lease armed, expect dest_gather ≈ gather on the leased rows),
  - `zcrx_lane_poisoned = 0`, `zcrx_frame_violations = 0`,
    `zcrx_fill_fallbacks ≈ 0`,
  - `zcrx_area_admission_waits` bounded (declines are the SIZING
    instrument now: sustained growth at < 100 % engagement = the
    window is still under-derived — name the term, do not hand-tune),
  - **`zcrx_degraded_bypasses` ≈ 0 on an engaged row** (Rev 4a — the
    round-1 tape's silent gate: bypasses dominating fills = the pool
    term, read beside `zcrx_starved_ms` share and
    `zcrx_structural_teardowns`; a majority-starved session now tears
    down by the economics arm instead of limping at full rent),
  - `parks`/`failovers` bounded episodes, six-consecutive pristine
    teardowns (0 rules, full RSS after every mount — the round-8 bar).
* **The performance verdict**: CPU/byte (pidstat daemon cores/GB +
  mpstat %sys/%soft) DOWN on the lane side at ≥ par throughput — and
  the composed row (lane + dest-lease, both defaults-on-trial) read
  against the **35.5 GB/s bar** (85 % of the 41.8 raw ceiling, the
  2026-08-06 ruling). The RSS rent (item 3) is accepted or refused by
  exactly this row: at > 50 % lane share the kernel path's lost width
  taxes a minority of bytes on a CPU-freed box; the no-harm teardown
  machinery (round 8) remains the automatic exit if the row disproves
  it.
* **After the row**: default-on adjudication per the user sequencing
  (2026-08-03) — default-on last, and only on a counted win.
