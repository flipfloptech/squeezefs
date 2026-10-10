# Rung 8 wave B1 — the zcrx read lane: cores, the userspace NVMe/TCP association, the probe-decided lane

Charter §6.3; spec `docs/porting/ops_zcrx.md`; decision 0003 §G; decision 0002 rows 419–433.
Venue: `sqz-zig` (7.2.6-sqz, `CONFIG_IO_URING_ZCRX=y`, `CONFIG_NVME_TARGET_TCP=y`, `eth0` =
virtio_net with ONE RX queue, `lo` for the loopback target).

## What landed (`src/ops/zcrx/`)

| Module | Stratum | Contract pinned |
|---|---|---|
| `abi.zig` | portable | `io_uring_zcrx_{rqe 16, offsets 32, area_reg 48, ifq_reg 96}`, `io_uring_region_desc 64` with offset pins (`zcrx_id @64`, `rx_buf_len @68`); `REGISTER_ZCRX_IFQ = 32`, `RECV_ZC = 58`; `refillRegionLen`; the registration builder zeroes every kernel-OUT field; the provenance (`>> 48`) and span-bounds gates, fuzzed total |
| `pdu.zig` | portable | ICReq/ICResp, Connect (72 + the 1024 B `nvmf_connect_data`), Property Get/Set (the attribute = the register's width), Read (Transport SGL 0x5A), CapsuleResp (phase bit stripped), C2HData (geometry law `plen == pdo + datal`, digest bits refused); status names; fuzz: every decoder total, exactly the encoder's form decodes |
| `pdu_stream.zig` | portable | the 24 B scratch law (`hlen != 24` ⇒ violation at byte 8); payload spans `(slot, off, len)` never copied; the stream split at EVERY byte yields one event sequence; header copy = 24 × PDUs; H2CData/R2T refused; fuzz total |
| `span_ledger.zig` + `span_ledger_model.zig` | portable | refcounts + MPSC free stack (`head = (tag << 32) ‖ top + 1`); 4-thread contention: every slot recycles once. **Models** (vector-clock happens-before, 3 threads): `chunk_recycle_never_races_consumers` HOLDS; WEAKENINGS that FAIL: `fetch_sub_relaxed`, `no_acquire_fence`, `push_relaxed` (each `InvariantViolated`); `free_stack_returns_exactly_once_no_loss` HOLDS and pins the packed-head arithmetic |
| `area.zig` | sizing portable / mapping Linux | ties: `W = 64 × 1 MiB = 64 MiB`; `S = 64 MiB × 3288/9000 → 24 MiB`; stride `next_pow2(9000 + 512) = 16 KiB`; `R = 8192 × 16 KiB = 128 MiB` (vs legacy 96 MiB); area `216 MiB`; `rq_entries = next_pow2(chunks)`; geometry `(clamp(cpus/8,1,8), clamp(2cpus,4,64))`; `depth = max(min(d, MQES+1), 2)`; the `2147483644` MDTS sentinel saturates to 1 MiB; the PMD-aligned, THP-advised, NUMA-preferred, R5-gauged mapping |
| `fill_table.zig` | portable | depth cells by CID, no map; the exact-length law; a status fails the OP only; refs taken per span, released after the gather; MEM-3 (an abandoned requester's CID returns to the pool); try-only admission; the whole-read plan (ONE acquire per queue, `MAX_SEGMENTS = 16`) |
| `governor.zig` | portable | the 10 governor laws (episodes not retries, idle-drain keeps the episode, failover once per window, the second window structural, the economic majority arm); `classifyRecvEnd` (ENOMEM/ENOBUFS/ENOSPC park by class); CQ sizing `64 × 351 + 128 → 32768`; `parkFailBound = clamp(4000 × rtt, 500 ms, 2 s)` |
| `steering.zig` | portable (mock NIC) | pool ties `(32,10)→10, (32,5)→8, (32,0)→8, (32,100)→16, (64,20)→20, (3,10)→∅`; picks highest-indexed; restricted RSS; the capacity gate (read-only; the mlx5 0-table lie ⇒ kernel-assigned ids; `@ANY` bouncing ⇒ explicit top-down); phase A/B with the `FailAt` lattice leaving the NIC untouched; residue reap only inside the reserved range; converge-in-any-order restore; the rxq arbiter (distinct queues, retired-ring rotation, the "needs ≥ 4" refusal); the census majority law |
| `nic_census.zig` | portable (fixture sysfs) | `/dev/nvme<C>n<N>` only; the tcp target from sysfs; both census shapes counted behind the route |
| `probe.zig` | ladder portable / answers Linux | the 14-gate verdict table (every gate refuses in order, typed; terminal vs transient); the target-override grammar; **the venue's own verdict**: `RECV_ZC` supported, route `lo`, `no_hds` (code 6) LOUD with `zcrx_lane_absent_reason` set |
| `ethtool.zig` / `ethtool_nl.zig` | Linux | ioctl struct pins (`rx_flow_spec 168`, `ring_cookie @152`, `location @160`, `rxnfc 192`, `rxfh 24`); the flow-spec encoding; genetlink message pins (`GETFAMILY` 32 B, nested `RINGS_GET` header); on the venue `lo` has no channels control, and 7.2.6-sqz ships `# CONFIG_ETHTOOL_NETLINK is not set` — `GETFAMILY("ethtool")` answers NLMSG_ERROR (a LOUD ledgered capability skip) and the HDS verdict is OFF on every NIC |
| `lane.zig` | Linux | the ring `SINGLE_ISSUER|DEFER_TASKRUN|CQE32` with the derived CQ (`Ring128.initWithCq` + CLAMP); the refill ring arithmetic (wrapping distance, mask wrap); the ifq refusal against ifindex 0 reaches `ready` and unwinds clean; unwound-before-go exits without poison; **the sim leg over a socketpair** against a mock controller: 100 KiB straddling chunks, a padded SUCCESS-elided pair + CapsuleResp, byte-exact, every chunk back, `free == total`, a framing violation poisons and drains |
| `association.zig` + `association_e2e_linux.zig` | Linux (venue) | **THE association against REAL nvmet-tcp**: a 64 MiB file namespace through `configfs.apply`, the kernel initiator connected (`/dev/nvme0n1`), the lane's target discovered from sysfs, the SECOND association under identity B, 2 IO queues, reads of 4 KiB / 1 MiB / 3 MiB (two queues) / the last block byte-exact against the kernel path and the pattern; `gather ≡ fill` byte-exact; the MQES clamp observed; `Capacity Exceeded` fails the op only; CID custody `free == total`; teardown orderly (never poison), idempotent; zero nvmet residue |
| `funnel.zig` | portable | the dispatch ladder over a lane double: the three exclusive outcomes (`zcrx_fills` / `zcrx_kernel_fills` / `zcrx_lane_refused`) with the reason split (`degraded_bypasses`, `area_admission_waits`, `fill_fallbacks`); poison/structural retire on sight; the `Volumes` decorator |
| `arm.zig` | Linux | `armVolume` at volume open: the lever off is silent-Absent; a malformed target seam is loud-Absent; R5 Red blocks before any probe |

Wiring: `read_path.Volumes.read_range(…, DestShape) → Served` (the DMA byte instruments count only
`.dma`); `data/plane.zig` runs the read seam through the funnel and arms/tears the lane per
volume (`@anchor:ops_zcrx_plane`); `stats` family `zcrx`; knob rows `SQUEEZEFS_ZCRX_LANE`
(A/B lever, off), `SQUEEZEFS_TEST_ZCRX_{AREA_SIM,SIM_CHUNK,TARGET}`; `Ring128.initWithCq`,
`prepRecv/prepSend/prepRecvZc`, `zcrxOff`; `ring_park.Parked.initOver`.

## Hardware-owed (loud probe + `Absent` here)

`REGISTER_ZCRX_IFQ` against a real HDS-capable multi-queue NIC, RSS/ntuple mutation through
`EthtoolNic`, the genetlink HDS verdict `== ENABLED`, multishot `RECV_ZC` delivery, the
park/failover/structural arms under a live provider pool, the engagement field rows. On the venue
every one resolves to its probe refusal (`no_hds` on `lo`/`eth0`; `too_narrow` on a 1-queue NIC;
`EPERM` at the ifq registration) and the mount stays on the kernel path byte-identical.

Measured (reference, carried beside what it justifies): nvme-tcp RX copy = 55.4 % of client
cycles at 46.6 GB/s (the term the lane deletes); 4 queues at matched 24.79 GB/s: 3.32 → 1.06 busy
cores; 8 queues at 49.5 GB/s: 7.94 → 2.75.
