# Porting spec — the zcrx read lane: a userspace NVMe/TCP host association serving cold read fills over io_uring zero-copy receive (squeezefs → Zig)

Scope: charter §6.3 "The zcrx read lane" (`docs/charter.md:230`), §1.2/§1.4 (probe, never
kernel version; portable), §12 (`RECV_ZC`, zcrx registration opcodes resolved by probe), §13
rung 8. Every claim cites `file:line` in `/Users/joberdorf/Source/squeezefs` (`src/zcrx_lane/*`,
`src/nvme_dev.rs`, `src/fuse_client.rs`, `docs/design-zcrx-read-lane.md` = "design", tests,
`loom-models/src/lib.rs`). Nothing here is advice; it is the contract. Reference quirks are
flagged `QUIRK n` inline and collected in §11 with the better design each takes (decision 0002).

**Venue facts (probed 2026-10-09, `scripts/venue`):** kernel 7.2.6-sqz, `CONFIG_IO_URING_ZCRX=y`,
`CONFIG_NVME_TCP=y`, `CONFIG_NVME_TARGET=y`, `CONFIG_NVME_TARGET_TCP=y`, `CONFIG_NVME_TARGET_LOOP=y`
(all built-in; configfs NOT mounted — `mount -t configfs none /sys/kernel/config` as root is the
nvmet bring-up's first step; `/dev/nvme-fabrics` exists); `eth0` = `virtio_net`, MTU 1280, ONE RX
queue (`/sys/class/net/eth0/queues/rx-0` only), `device/numa_node` absent; no `ethtool`/`ip`
binaries; `AF_NETLINK/NETLINK_GENERIC` sockets open. Consequences: the NIC leg (ifq registration
against a real queue, RSS/ntuple steering, HDS) is untestable here — a 1-queue NIC is the lane's
own "too narrow" refusal (`steering.rs:186-190`: `channels/4 == 0 ⇒ empty pool`), which IS a
venue-reachable probe verdict. Zig 0.17 std carries `IORING_OP.RECV_ZC = 58` and
`IORING_REGISTER.REGISTER_ZCRX_IFQ = 32` (`std/os/linux.zig`, verified by enumeration) and NO
`io_uring_zcrx_*` / `io_uring_region_desc` structs — the port mirrors them (§1.2).

---

## 0. Shape, strata, the one degrade point

- **What it is** (design §1 `:404-412`, charter `:230`): a SECOND NVMe/TCP host association per
  nvme-tcp data device, owned by the daemon, serving **cold read fills only**; the kernel initiator
  keeps writes, metadata, admin, discovery, multipath, error recovery. One lane queue = one
  io_uring (`DEFER_TASKRUN | SINGLE_ISSUER | CQE32`) = one zcrx ifq (`REGISTER_ZCRX_IFQ`) = one NIC
  RX queue = one TCP connection; multishot `RECV_ZC`; refill ring fed by a span-grant ledger
  (`uring_zcrx.rs:1-5`, design §5 `:530-533`).
- **Read-only by construction** (design §3 `:449-455`, `pdu.rs:1-6`): the codec's encoder set is
  exactly {ICReq, Connect, Property Get/Set, Read}; no H2CData/write/reservation builder exists, so
  D0/PR/fencing surfaces are unreachable. NVMe PR WERO permits other hosts' reads by definition; the
  lane never joins a reservation.
- **Two strata, probe is the degrade point** (design §7 table `:577-588`): kernel lacks
  `RECV_ZC`/`REGISTER_ZCRX_IFQ` ⇒ lane never arms ⇒ kernel path byte-identical; NIC lacks HDS ⇒
  same; device not nvme-tcp ⇒ inert at discovery; ICResp demands digests / PFV ≠ 0 ⇒ arm fails
  LOUD; mid-stream framing violation ⇒ session POISONED, disarm loud, kernel path, tripwire; short
  completion / status ≠ 0 ⇒ op fails to kernel-path retry, counted; R5 Red ⇒ blocks NEW arms, sheds
  nothing; fenced mount ⇒ no interaction.
- **Measured** (design §2 `:414-431`, charter `:230`): kernel nvme-tcp RX copy = 0.69–0.72 CPU
  passes/user byte, **55.4 % of all client cycles at 46.6 GB/s** (`:391-396`); 1 queue/4 conns
  open-loop 10.21/10.33 → 20.93/22.21 GB/s (~2.1×); **4 queues @ matched 24.79 GB/s: 3.32 → 1.06
  busy cores, DRAM 0.50 → 0.018 B/B (−68 %)**; 8 queues @ line rate 49.5 GB/s: 7.94 → 2.75 cores
  (−65 %). Residual on the zcrx side = softirq/page-pool machinery.
- **Status in the reference**: Z1–Z3 shipped; the Rev-4 engagement campaign PARKED default-on on
  engagement economics (0.05 % engagement could not pay the RSS rent — design `:12-17`); field
  rounds 2–4 each falsified a sizing term (design `:81-264`). Default stays opt-in; adjudication is
  field-owed (design §13 `:740-786`).

---

## 1. The probe ladder

### 1.1 Order and verdicts (`mod.rs:165-432` `arm_for_device`)

Every gate refuses and leaves the kernel path byte-identical; only steering mutates the NIC and it
is the LAST step and rolls itself back (`mod.rs:246-249`). Loud once per device per mount (the
caller's OnceCell caches the refusal, `mod.rs:186-199`); per-NIC remedies print once
(`steering.rs:559-574` `nic_note_once`/`note_arm_refusal_once`).

| # | Gate | Source | Verdict |
|---|---|---|---|
| 1 | `SQUEEZEFS_ZCRX_LANE` opt-in | `mod.rs:166-168` | off ⇒ `None`, silent (the default) |
| 2 | R5 `arm_admission(level)`: Red blocks | `mod.rs:136-138, 173-179` | warn, `None` |
| 3 | target: `SQUEEZEFS_ZCRX_LANE_TARGET` override else sysfs probe | `mod.rs:180-200`, `probe.rs:36-112` | probe refusal ⇒ WARN naming §4.1, `None` |
| 4 | test seams `AREA_SIM` / `FORCE_COPY` arm their backend here | `mod.rs:201-244` | (test venues only) |
| 5 | `recv_zc_supported()`: `io_uring_setup(2)` + `REGISTER_PROBE`, `is_supported(58)` | `probe.rs:16-25` | lacks ⇒ warn, `None` |
| 6 | `traddr` parses as an IP literal | `mod.rs:257-264` | else warn, `None` |
| 7 | `route_ifname_for(ip)`: connected UDP socket (port 9, no packet) → local addr → `getifaddrs` | `ethtool.rs:423-473` | no route ⇒ warn, `None` |
| 8 | HDS: `ethtool_nl::tcp_data_split_on(ifname)` (genetlink RINGS_GET attr == ENABLED) | `mod.rs:274-290`, `ethtool_nl.rs:196-227` | OFF/Err ⇒ warn, `None` |
| 9 | `nic_ifindex` (`if_nametoindex`) | `mod.rs:291-294`, `ethtool.rs:495-504` | 0 ⇒ `None` |
| 10 | ONE ethtool control socket: `combined_channels`, `steering_capacity_gate` (ntuple on? table size?), `rx_ring_descriptors` (degrades to 0 LOUD once) | `mod.rs:300-336` | capacity refusal ⇒ warn once per NIC, `None` |
| 11 | census `tcp_devices_via_nic(ifname).max(1)` → `lane_eligible_queues(channels, devices)` → `fair_queue_want(eligible, devices, io_queues)` → clamp by `free_reserved_slots` (0 ⇒ refuse) | `mod.rs:357-373` | `Some(0)` ⇒ warn, `None` |
| 12 | `rxq_alloc::acquire(ifindex, ifname, channels, devices, want)` — distinct queues | `mod.rs:374-389` | Err ⇒ warn, `None` |
| 13 | `mtu = nic_mtu`, `ring_fill_bytes = ring_standing_bytes(rx_descs, mtu, chunk)`; build `ZcrxPlan{numa_node, ifname, ifindex, rx_queues, rxq_lease, ring_fill_bytes, mtu}` | `mod.rs:398-408` | — |
| 14 | `register_r5_component()`; `LaneSession::connect_with(target, Zcrx(plan))` (§2.4, §6) | `mod.rs:409-431` | Ok ⇒ `zcrx_lane_armed = 1`, `nic_census::note_armed`; Err ⇒ `zcrx_conn_errors += 1`, ERROR log, `None` |

Inside `connect_with` the REAL gates continue: phase-A RSS exclusion BEFORE any TCP connect
(`initiator.rs:596-626`), per-queue `REGISTER_ZCRX_IFQ` on the driver thread (refusal names
`if_idx`/`rxq`, `uring_zcrx.rs:1006-1016`), phase-B flow rules after connect, `go` only then
(`initiator.rs:847-868`). A refusal anywhere unwinds: drop `go` senders → drain/JOIN every queue →
restore steering (`initiator.rs:873-892`) — ifqs gone BEFORE RSS re-includes the queues.

### 1.2 The io_uring surface, byte-exact (`uring_zcrx.rs:34-193`)

Constants (uapi `<linux/io_uring.h>`, both field kernels 7.1.2 / 6.19.14-sqz carry the same zcrx
ABI, `uring_zcrx.rs:15-17`; venue 7.2.6-sqz is 6.15+-class): `IORING_REGISTER_ZCRX_IFQ = 32`
(`:53`), `IORING_OP_RECV_ZC = 58` (`probe.rs:10`), `IORING_OP_SEND = 26`, `IORING_OP_READ = 22`
(`:55-56`), `IORING_RECV_MULTISHOT = 1 << 1` in `sqe.ioprio` (`:57`), `IORING_CQE_F_MORE = 1 << 1`
(`:58`), `IORING_MEM_REGION_TYPE_USER = 1` (`:60`), `IORING_ZCRX_AREA_SHIFT = 48` (`:61`). Setup
flags: `CQSIZE 1<<3, CLAMP 1<<4, CQE32 1<<11, SINGLE_ISSUER 1<<12, DEFER_TASKRUN 1<<13` (`:39-43`).

| Struct | Layout (LE, `extern`) | Size / pins | Source |
|---|---|---|---|
| `io_uring_zcrx_rqe` (refill entry) | `off u64 @0 ‖ len u32 @8 ‖ __pad u32 @12 (= 0)` | 16 | `uring_zcrx.rs:506-510` |
| `io_uring_zcrx_cqe` (CQE32 big words) | `off u64 @16 of the CQE ‖ __pad u64 @24`; `off = (area_token_hi << 48) ‖ byte offset` | — | `:392-395, 413-424` |
| `io_uring_zcrx_offsets` | `head u32, tail u32, rqes u32, __resv2 u32, __resv [2]u64` | 32 | `:133-141, 190` |
| `io_uring_zcrx_area_reg` | `addr u64, len u64, rq_area_token u64 (OUT), flags u32 (0), dmabuf_fd u32 (0), __resv2 [2]u64` | 48 | `:143-152, 191` |
| `io_uring_region_desc` | `user_addr u64, size u64, flags u32 (= TYPE_USER), id u32, mmap_offset u64 (OUT), __resv [4]u64` | 64 | `:154-163, 192` |
| `io_uring_zcrx_ifq_reg` | `if_idx u32, if_rxq u32, rq_entries u32 (IN/OUT clamped), flags u32, area_ptr u64, region_ptr u64, offsets @32, zcrx_id u32 @64 (OUT), rx_buf_len u32 (0 = kernel default page grain), __resv [3]u64` | 96 | `:165-179, 187-189` |
| RECV_ZC SQE fields | `opcode 58, fd = socket, ioprio = MULTISHOT, zcrx_ifq_idx = zcrx_id @44 (the `splice_fd_in`/`file_index` union slot), len = 0, user_data = TAG_RECV` | — | `:106-124, 185-186, 1066-1075` |

Registration law (`build_ifq_registration`, `:445-482`, pinned `:1896-1934`): every kernel-OUT
field goes DOWN as zero (`rq_area_token`, `mmap_offset`, `offsets`, `zcrx_id`, the `rq_entries`
echo); `area_ptr`/`region_ptr` are wired at the call site AFTER the structs reach their final stack
slots (a pointer minted inside the builder dangles on move-out, `:1004-1005`). The refill region
is caller memory: `refill_region_len(rq_entries, page) = ceil((page + rq_entries × 16) / page) ×
page` (one page for head/tail + 16 B per rqe; `:441-443`, pinned `:1876-1893`), anonymous private
mmap (`:227-246`). After `register(32, &ifq, 1)` the refill ring view is
`{region, head_off = offsets.head, tail_off = offsets.tail, rqes_off = offsets.rqes, entries =
ifq.rq_entries (kernel-clamped echo), tail_cache = 0, area_token = area_reg.rq_area_token}` (`:1017-1025`).

Two per-completion gates, pure and proptest-total (`:1829-1871`): **provenance**
`cqe_area_matches(raw_off, token) ≡ raw_off >> 48 == token >> 48` — a mismatch is a FRAME
VIOLATION (the CQE names memory this lane does not own), never a retry (`:413-424`); **span
bounds** `cqe_span_offset(raw_off, len, area_len) = off iff off + len ≤ area_len with checked add`
(`:430-434`).

Ring geometry: `sq_entries = next_pow2(depth + 8)` (commands in flight + recv/doorbell arms +
partial-send resubmits, `:576-580`, `initiator.rs:787`); `cq_entries = cq_entries_for(depth,
max_xfer, chunk, sq, mtu)` (§6.2); CQSIZE|CLAMP so an oversize ask clamps-then-rounds
(`:594-597`). `KERNEL_MAX_CQ_ENTRIES = 2 × 32768` (io_uring.h:160-161, `:597`).

### 1.3 Target discovery — sysfs, zero new config (`probe.rs:36-112`, design §4.1 `:459-468`)

Eligible device path shape: `/dev/nvme<C>n<N>` ONLY (digits both sides of `n`; multipath
`nvme<C>c<P>n<N>` and partitions ineligible v1, `:37-47`; pinned `tests/zcrx_lane_tests.rs:571-577`).
Reads: `/sys/class/nvme/nvme<C>/{transport (== "tcp" or ineligible), address
(traddr=…,trsvcid=…), subsysnqn}`, `/sys/block/nvme<C>n<N>/{nsid, queue/logical_block_size (power
of two), queue/max_hw_sectors_kb}` (`:48-81`). `max_xfer_bytes = clamp(max_kb × 1024, 1 << lba_shift,
LANE_MAX_XFER_CAP_BYTES)` with saturating mul — the **unlimited-MDTS sentinel**
`max_hw_sectors_kb = 2147483644` on every field nvme-tcp controller overflowed a `u32 checked_mul`
and SILENTLY refused every real arm (`:88-101`; red test `tests/zcrx_lane_tests.rs:580-627`).
`lane_geometry(cpus)` from the PROCESS core mask, never the calling thread's
`available_parallelism` (`:82-87`; decision 0002 row 22).

Host identity (`probe.rs:262-279`): `/etc/nvme/{hostnqn,hostid}` when present (field parity — target
`allow_host` lists keep working), else `hostid = hash(pid, boot_id)` (`:301-315`) and `hostnqn =
"nqn.2014-08.org.nvmexpress:uuid:<hostid>"` (`:276-277`).

Device census (`probe.rs:180-226`): count nvme-tcp NAMESPACES whose controller `traddr` routes
through `ifname` (`route_ifname_for`); both child shapes `nvme<C>n<N>` and `nvme<C>c<P>n<N>`
(`is_namespace_of`, `:229-250`; counting only the plain shape read 0 on the fleet and collapsed the
fair want — finding D). Over-counting multipath c-paths is the safe direction (widens the spread).

---

## 2. The NVMe/TCP userspace host association

### 2.1 PDU formats, byte-exact (`pdu.rs`; NVMe/TCP 1.0, LE throughout, `:7-9`)

Common header CH (8 B, every PDU): `type u8 @0 ‖ flags u8 @1 ‖ hlen u8 @2 ‖ pdo u8 @3 ‖ plen u32 @4`
(`:59-81`). Types: ICReq 0x00, ICResp 0x01, H2CTerm 0x02, C2HTerm 0x03, CapsuleCmd 0x04,
CapsuleResp 0x05, C2HData 0x07 (`:14-20`). C2HData flags: LAST `1<<2`, SUCCESS `1<<3` (`:23-24`).
QUIRK 3: CH flag bits 0/1 (HDGSTF/DDGSTF) are never checked (§11).

| PDU | Bytes | Source |
|---|---|---|
| **ICReq** (host→ctrl) | 128 B: CH `{0x00, 0, hlen 128, pdo 0, plen 128}`; `pfv u16 @8 = 0`, `hpda u8 @10 = 0`, `digest u8 @11 = 0` (both OFF), `maxr2t u32 @12 = 0` (no writes ⇒ no R2T); rest zero | `:92-97`; pinned `tests:385-393` |
| **ICResp** (ctrl→host) | 128 B: CH type 0x01; `pfv u16 @8`, `cpda u8 @10`, `digest u8 @11`, `maxh2cdata u32 @12`. Law: `pfv != 0` ⇒ `Negotiation` refusal; `digest != 0` ⇒ `Negotiation` refusal ("never silently accept a per-byte CRC pass"); wrong type ⇒ `UnexpectedType`; `< 128` ⇒ `Short` | `:100-144`; pinned `tests:474-494` |
| **CapsuleCmd** | CH `{0x04, 0, hlen 72, pdo (72 if in-capsule data else 0), plen}` ‖ 64 B SQE @8 ‖ in-capsule data @72 | `:183-216, 218-236, 248-259` |
| SQE scaffold | `opcode @0, psdt flags 0x40 @1 (SGL), cid u16 @2` | `:147-153` |
| In-capsule SGL Data Block (offset form) | SQE bytes 24..40: `addr @24 = 0`, `len u32 @32`, `type byte @39 = 0x01` | `:156-161` |
| Transport SGL Data Block | `len u32 @32`, `type byte @39 = 0x5A` (type 5, sub 0xA) — data rides C2HData | `:165-168` |
| **Connect** (fctype 0x01 @4) | plen `8 + 64 + 1024`; SQE: `OPC_FABRICS 0x7F`, in-capsule SGL len 1024, `recfmt u16 @40 = 0`, `qid u16 @42`, `sqsize u16 @44 (0-based)`, `cattr u16 @46 = 0`, `kato u32 @48`; data (1024 B = `sizeof(struct nvmf_connect_data)`): `hostid [16] @0`, `cntlid u16 @16` (0xFFFF admin / assigned for IO), resv 238, `subsysnqn [256] @256` NUL-padded, `hostnqn [256] @512`, resv 256. The 1024 is LOAD-BEARING: nvmet `nvmet_check_transfer_len` refuses any other length with `0x400f` (Data SGL Length Invalid ‖ DNR) — the 2026-08-04 field arm refusal (a 4096 B blob) | `:170-216`; pinned `tests:395-432, 1801-1848` |
| **Property Get/Set** (fctype 0x04 / 0x00) | 72 B capsule (`pdo 0`); SQE: `attrib u8 @40 (1 = 8-byte)`, `offset u32 @44`, Set: `value u64 @48` | `:218-244` |
| Properties | `CAP 0x00` (MQES = low 16 of dw0, 0-based), `CC 0x14`, `CSTS 0x1C`; `CC_ENABLE_NVM = 1 ‖ 6 << 16 (IOSQES 64 B) ‖ 4 << 20 (IOCQES 16 B)` | `:36-41`; `initiator.rs:655, 657, 668-672` |
| **Read** (OPC 0x02) | 72 B capsule; SQE: `nsid u32 @4`, Transport SGL `xfer_len`, `slba u64 @40` (cdw10/11), `nlb0 u16 @48 = nlb − 1` (`1 ≤ nlb ≤ 65536` debug-asserted) | `:248-259`; pinned `tests:460-471` |
| **CapsuleResp** (ctrl→host) | 24 B: CH `{0x05, 0, hlen 24, pdo 0, plen 24}` ‖ 16 B CQE @8: `dw0 u32 @0, dw1 u32 @4, sqhd u16 @8, (sqid u16 @10), cid u16 @12, status u16 @14 >> 1` (phase bit stripped — transport-reserved on NVMe-oF) | `:326-351`; pinned `tests:525-533, 99-108` |
| **C2HData** (ctrl→host) | CH `{0x07, flags, hlen 24, pdo ≥ 24, plen}` ‖ PSH: `cccid u16 @8`, (resv @10), `datao u32 @12`, `datal u32 @16`, (resv @20). **Geometry law**: `pdo ≥ hlen ∧ plen == pdo + datal` (digests off ⇒ payload runs exactly to the PDU end); else `Geometry` violation (poison) | `:354-395`; pinned `tests:497-522` |
| H2CData / R2T | NOT encoded, NOT accepted on an IO queue (`unexpected PDU type` ⇒ violation) — read-only by construction | `pdu.rs:1-6`, `pdu_stream.rs:224` |

Status rendering (`describe_status`, `:267-322`): SF post-phase-strip → `SCT = bits 10:8`, `SC =
bits 7:0`, MORE bit 13, DNR bit 14, with names only for codes this surface can meet (generic
0x00–0x11, 0x80–0x82; fabrics 0x80–0x84; media 0x81); honest `None` otherwise. A raw hex cost a
field session (`0x400f` named nothing, `:264-266`).

### 2.2 Connect and queue set-up (`initiator.rs:594-992`; design §4.2 `:470-487`)

1. **Phase A steering** (real backend only): `ArmedSteering::arm(nic, rx_queues)` BEFORE ANY TCP
   connect, admin included (`:596-626`) — a zcrx-bound queue yields unreadable net_iov skbs; any
   host flow RSS-hashed onto it reads `EFAULT` (the field's `ICResp read: Bad address`), and a HW
   flow cache latches an established flow's queue across indirection changes (round-4 finding G).
2. **Admin association** (blocking pool, cold): `TcpStream::connect(traddr:trsvcid)`, `TCP_NODELAY`
   (`:636-638`); `ic_exchange` = write ICReq, `read_exact` 128 B, `parse_icresp` (`:410-419`);
   Connect `qid 0, sqsize0 31, kato 0, cid 0, cntlid 0xFFFF` (`:641-645`; `cntlid := cqe.dw0 &
   0xFFFF`, `:648`); Property Get CAP (cid 1, attrib8) → `mqes = dw0 & 0xFFFF` (`:650-655`); Property
   Set CC = `CC_ENABLE_NVM` (cid 2, `:657-659`); Property Get CSTS (cid 3) loop until `RDY` (bit 0),
   `CFS` (bit 1) ⇒ error, deadline 10 s, 20 ms sleeps (`:661-678`). QUIRK 4 (§11). Admin round trip
   = write capsule, read CH, expect CapsuleResp, read `plen − 8`, parse CQE (`:376-397`); any
   other PDU type during bring-up is an error.
3. **Depth** = `min(target.queue_depth, mqes + 1).max(2)` (`:685`) — MQES clamps the derived
   `clamp(cpus × 2, 4, 64)`; floor 2 because `sqsize0 = depth − 1 ≥ 1`.
4. **IO queues** `qid = 1..=io_queues`: own TCP connection each, `TCP_NODELAY`, ICReq/ICResp
   again, Connect `qid, sqsize0 = depth − 1, kato 0, cid 0, cntlid` (`:695-720`). Then the backend
   arm (§6.1). Zcrx: the queue's flow `{src: local_addr, dst: peer_addr, queue: rxq}` is recorded
   for phase B (`:755-760, 828-832`).
5. **Phase B**: `hold.flow_rules(flows)` under the phase-A guard (ephemeral ports exist only
   post-connect), then `go = true` to every parked driver ⇒ `RECV_ZC` arms (`:847-868`).
6. **Admin watchdog** `sqz-zcrx-admin`: one OS thread blocked in `read(8)` on the admin socket;
   any data/EOF/error after bring-up = association event ⇒ `mark_session_poisoned` UNLESS the
   teardown latch is set (orderly unwind is never poison — the round-6 `poisoned = 8` with zero
   poison logs, `:894-955, 1086-1104`).
7. Log line `zcrx-lane armed: <subnqn> nsid lba_shift queues depth max_xfer (backend)` (`:957-970`);
   `register_session` (`:990`).

Coexistence with the kernel's association (design §1 `:410-412`, §3 `:449-455`, §4.1 `:460-463`):
the lane rides the kernel initiator's OWN attachment (same `subsysnqn`, `traddr`, `nsid`, the
kernel-resolved `max_hw_sectors_kb` as its per-command cap) as a separate host association with
the same or a process `hostnqn`; it issues only reads; KATO 0 (same-host-managed, fails loud on TCP
errors; residual risk 4: a target enforcing nonzero KATO refuses at Connect, `:680-681`); a
target's `cntlid` ceiling / `allow_any_host` surfaces at Connect — loud, kernel path intact
(`:677-679`). The lane's queue ids are its OWN association's `qid 1..N` (`initiator.rs:695`), one
TCP connection each; the kernel's qids are unrelated. The one-definition read timeout
`LANE_READ_TIMEOUT = 30 s` (`:40-46`) — QUIRK 1 (§11).

### 2.3 Session lifecycle and custody (MEM-3, design `:290-311`)

- `CidSlot` = CID + depth permit as ONE RAII unit held by the PENDING ENTRY, returned when the
  entry is destroyed (completion, send-failure cancel, poison drain) — never on the requester's
  happy path, so a dropped future leaks nothing (`initiator.rs:82-105, 115-140`). Lock order:
  fill-table `pending` → `free_cids` (`:159-164`, `area_queue.rs:102-105`). Closed semantics: the
  `poisoned` latch is checked before AND after the permit acquire (`:115-129`).
- `cid_slots() = (free, total)` is the no-leak instrument: `free == total` at quiescence
  (`:520-540`); `area_chunks()` the refill-discipline instrument (`:555-568`).
- Poison lattice (`mark_session_poisoned`, `:55-66`): idempotent swap; the winning transition
  counts `zcrx_lane_poisoned`, stores `zcrx_lane_armed = 0`, and ALWAYS logs the canonical line
  ("a poisoned increment without a greppable reason is unrepresentable", round 8). Queue poison
  (`area_queue.rs:154-161`): `drain = true` ONLY when the driver provably pushes no more events
  (exiting, or joined) ⇒ `table.fail_all`; writer-side failures pass `false`.
- `teardown()` (`:1082-1134`, idempotent latch): retire the admin watchdog FIRST (shutdown socket,
  JOIN) → `quiesce()` (signal every queue: close the ring doorbell / `Stop` + socket shutdown, then
  JOIN every driver — the join is the PROOF of quiescence, `area_queue.rs:199-248`) →
  `restore_steering` → release the rxq lease (only after the ifqs are joined, so a successor
  re-registers a freed queue) → structural census vote + whole-NIC sweep (§6.4) → `note_released`.
  `Drop` is a teardown with the same order (`:324-358`): ifqs gone BEFORE RSS re-includes the
  queues (finding 3). `spawn_teardown` = fire-and-forget on the meta pool for the poison path
  (`:1140-1148`). `teardown_all_lanes` is the daemon-shutdown hook (sessions live in per-device
  OnceCell statics that never drop at exit, `mod.rs:57-78`).

---

## 3. The area

### 3.1 Mapping (`area.rs:202-234, 349-396`)

`ZcrxArea::new(len, chunk, numa_node)`: `len % chunk == 0`, both nonzero (`:207-210`);
`map_anon_pmd_aligned` = over-reserve `len + 2 MiB` `PROT_NONE`, `MAP_FIXED` the real RW anonymous
mapping at the first PMD-aligned address inside it, `munmap` head and tail slack (`:349-396`; the
thp.rs `map_shared_pmd_aligned` trick — an unaligned vma maps huge folios 4 KiB-wise, `:19-21`);
`madvise(MADV_HUGEPAGE)` (`:217`); NUMA `bind_region_preferred(base, len, node)` BEFORE first touch,
refusal-tolerant (placement is an optimization, `:218-223`); charge `zcrx_area_bytes += len`;
`Drop` munmaps and credits (`:332-343`). `PMD_BYTES = 2 MiB` (`:21`). The chunk grain =
`sysconf(_SC_PAGESIZE)` (the kernel zcrx net_iov granule; `:157-165`; `rx_buf_len = 0` at
registration keeps the kernel default, `uring_zcrx.rs:1920`). The NIC's NUMA node from
`/sys/class/net/<if>/device/numa_node` (`ethtool.rs:477-481`); the driver thread pins to that
node before touching ring or area (`uring_zcrx.rs:981-984`).

### 3.2 The grant ledger — `area_core.SpanLedger` (the loom-modelled core)

Protocol (`area_core.rs:1-38`): per-slot refcount `refs[i]` (0 = free) + an MPSC intrusive free
stack (`next[i] = successor + 1`, packed `head = (pop_tag u32 << 32) | (top + 1)`, 0 = empty;
`:52-71`). **grant** (SINGLE consumer — the queue driver): pop the top, `refs = 1` (`:107-139`);
**add_ref** (any thread; precondition: caller holds a ref — a clone edge, never resurrection,
`:143-146`); **release** = Arc pattern `fetch_sub(Release)`; on the 0-crossing `fence(Acquire)` then
`push_free` (Release CAS; `:152-180`). Returns `true` to exactly one releaser per grant epoch.
Model precondition the model cannot see: ONE grant consumer per ledger — structural (one driver per
queue), not enforced inside (`:19-28`; the `ipc_ring_core` lesson). The single consumer also makes
the pop ABA-free structurally; the tag is a belt.

Loom models (`loom-models/src/lib.rs:392-503`): `zcrx_chunk_recycle_never_races_consumers` — two
consumers read the payload then release; the driver's re-grant overwrites; loom's UnsafeCell
tracking proves every read happens-before the recycle (`:418-463`); `zcrx_free_stack_returns_
exactly_once_no_loss` — racing last-ref releases of distinct slots vs the single popper recycle
each slot exactly once (`:466-503`). **Weakenings that FAIL** (`:401-403`, `area_core.rs:30-35`):
`fetch_sub` to `Relaxed`; the 0-crossing `fence(Acquire)` removed; the push CAS weakened from
`Release`. The Zig model (§12) adds the explicit per-thread pending-store buffer the AGENTS.md
convention requires.

In the real backend the slot is the bookkeeping RECORD for a kernel-granted CQE span, not the chunk
the next recv lands in (`:6-9`); `side[slot] = (raw_off, len)` is the driver's span record
(`uring_zcrx.rs:1079-1081`). `GrantRef` (`area.rs:400-448`): Clone = `add_ref`, Drop = release;
`into_raw_slot` keeps the ledger ref while dropping the embedded `Arc` properly (`:418-425`);
`adopt_grant(slot)` re-materialises RAII custody — one raw ref in, one RAII ref out, never both
(`:305-310`; pinned `uring_zcrx.rs:2218-2246`). `AreaSlice{grant, ptr, len}` = a read-only byte
span pinned by its grant; `sub(off, len)` clones the ref (`:452-502`, MEM-4: constructor `unsafe`
because `as_slice` dereferences without a check).

### 3.3 The refill protocol (`uring_zcrx.rs:1079-1124, 1275-1296, 498-516`)

- The recycle grain is the CQE SPAN: one rqe per span, `(raw_off, len)` echoed exactly as the CQE
  delivered them (design `:333-337`). `RefillRing::post(raw_off, len)`: `head = load(Acquire)` of
  the kernel's head word; full iff `tail_cache − head ≥ entries` (wrapping distance, never raw
  compare — pinned `:2123-2136`) ⇒ `false` (caller DEFERS; overwriting an unconsumed rqe would
  hand the NIC a span the lane still owns, `:2101-2120`); else write `rqe[tail & (entries−1)] =
  {off, len, 0}` and `store(tail+1, Release)`.
- Driver loop top (`:1101-1124`): post every `deferred_returns` slot whose rqe bounced; then
  `while try_grant_chunk()`: a grant whose `side` record is `(0,0)` is fresh (nothing to return) ⇒
  `ready_slots`; else post its rqe (success ⇒ clear the record, `ready_slots`; full ⇒ `deferred`).
- At a RECV_ZC CQE (`:1275-1296`): pop a `ready_slot` (or late-grant from the free stack, posting
  its pending return first — "span records exhausted" is terminal); `side[slot] = (raw_off, len)`;
  `adopt_grant(slot)` → `AreaSlice::new(grant, base + off, len)` → parser; dropping the recv-pass
  slice leaves payload spans holding their own refs (`:1303-1317`).
- Grant-ledger slots = AREA CHUNKS in count (`SpanLedger::new(len / chunk)`, `area.rs:231`);
  `rq_entries = rq_entries_for(chunks) = next_pow2(max(chunks, 1))` (`:152-154`) — 1:1, so
  recycling can never stall the ring while refs are outstanding; exhaustion backpressures at
  ADMISSION (§4), never mid-stream (design §4.3 `:501-507`). `SlotBag::drop` releases every raw
  ref the driver holds on ANY exit (`uring_zcrx.rs:914-935`; pinned `:2191-2216`).

### 3.4 Sizing — every term derived (`area.rs:25-154`; design §8 `:590-620`, Rev 4 `:40-64`, 4b `:182-192`)

| Term | Formula | WHY / measured | Source |
|---|---|---|---|
| fill window `W` | `area_bytes_per_queue(depth, max_xfer) = ceil(depth × max_xfer / PMD) × PMD`, floor 1 PMD | depth commands of `max_xfer` = the CID namespace = the admitted window (one arithmetic) | `area.rs:25-28`; tie `tests:1002-1039` |
| admission permits | `admission_permits(W, chunk) = max(max(W, chunk) / 4096, 1)`; `ADMISSION_UNIT = 4096` | the FULL window since Rev 4 (the retired `/2` halved engagement: 32 MiB/queue admitted vs ~51 MiB/device offered ⇒ 0.05 % engagement) | `:33-48`; `area_queue.rs:399-416` |
| burst geometry | `burst_geometry(mtu, chunk) = {payload = mtu, burst = ceil(mtu/chunk) × chunk}`; `None` if MTU unknown | HDS splits headers off; an MTU-grain payload starts a fresh niov and occupies `ceil(mtu/chunk)` chunks (≈ 73 % occupancy at 9000/4096) | `:50-75` |
| delivery slack `S` | `ceil(W × (burst − payload) / payload)` PMD-rounded; unknown MTU ⇒ `W` (occupancy ½) | field: `64 MiB × 3288/9000 → 24 MiB` | `:127-148`; tie `derivation_sweep_tests.rs:1215-1219` |
| MPWQE stride | `mpwqe_stride_bytes(mtu) = next_pow2(mtu + 512)` | mlx5 `roundup_pow_of_two(SKB_FRAG_SZ(headroom + hw_mtu))`: NET_SKB_PAD 64 + hard_mtu ≤ 22 + skb_shared_info 320 + two SKB_DATA_ALIGN roundings ≤ 128 ⇒ ≤ 498, ceiled 512 — over-estimating only rounds UP at a pow2 boundary (safe) | `:77-97` |
| ring standing `R` | `max(descs × ceil(mtu/chunk) × chunk, descs × stride(mtu))`; unknown MTU ⇒ `descs × chunk` | legacy/cyclic RQ vs striding RQ (`frames × linear_stride`, `log_wqe_sz` cancels); RQ mode not probeable ⇒ `max`. Field rail 8192 × 16 KiB = **128 MiB** vs legacy 96 MiB — the 32 MiB shortfall held every round-2 window starved | `:99-125`; design `:153-192` |
| **area** | `W + S + R`, chunk-granular | field ≈ 64 + 24 + 128 = **216 MiB/queue**, ~2.2 GiB over 10 sessions, R5-gauged | `initiator.rs:778-786`; design `:189-192` |
| refill entries | `next_pow2(chunks)` | kernel requires pow2 | `area.rs:152-154` |
| rx descriptors | `ETHTOOL_GRINGPARAM.rx_pending` (frames on mlx5) | probe failure ⇒ 0, loud once — sizing flies blind, the park governor bounds the damage | `ethtool.rs:186-194`, `mod.rs:303-314` |
| sim area | `W` only; admitted window `= area.len() / 2` | no NIC ring, unprobed grain — byte-identical to the Z2 contracts | `area_queue.rs:258-265`, `initiator.rs:731-739` |

R5 (`area.rs:507-524`, design §7): the `zcrx_area` component `{floor 0, weight 1, gauge =
zcrx_area_bytes, shed = no-op}` registered once; non-sheddable (fixed registered DMA memory) — Red
blocks NEW arms (`arm_admission`), in-flight converges by completion, teardown credits the gauge.

---

## 4. The fill table and the ONE gather (`fill_table.rs`; design §4.4 `:509-527`, Rev 3 `:267-289`)

- `FillTable = Mutex<HashMap<cid, PendingFill{len, received, segs: Vec<(datao, AreaSlice)>, tx,
  admission: Option<AdmissionUnits>, _slot: CidSlot}>>` — a std mutex: critical sections are
  pointer pushes with no await inside; the real driver is a plain thread (`:15-17, 85-104`).
- `insert(cid, len, slot, admission) → oneshot rx` (`:118-140`); `cancel(cid)` for a send-side
  failure before the wire (`:144-146`); `apply(ParseEvent)` (`:150-156`):
  - `C2hSpan{cid, datao, slice}`: unknown CID ⇒ `Err` (violation); `datao + len > p.len` ⇒ `Err`;
    else `received += len`, push `(datao, slice)` (`:158-178`).
  - `C2hEnd{cid, last, success}`: `!success` ⇒ nothing; `success ∧ !last` ⇒ `Err` ("SUCCESS on a
    non-LAST C2HData"); else remove + `complete` (`:180-194`).
  - `Cqe`: unknown CID ⇒ `Err`; `status != 0` ⇒ the OP fails (`describe_status`), session lives;
    else `complete` (`:196-213`).
  - `complete` (`:234-255`): `received == len` exactly, else `zcrx_frame_violations += 1` and the op
    fails ("completed with N of M bytes") — short data + success is a violation, never partial data.
  - `fail_all(why)`: every waiter fails; dropping segs releases chunk refs (the drain's recycle
    edge, `:217-225`).
- `ZcrxFill{segs, len, _admission}` (`:40-83`): the completed fill = refcounted area spans + the
  op's admission custody. `gather_into(dest)` = ONE `copy_nonoverlapping` per span at `dest +
  datao` — the single CPU pass over payload bytes (`:73-82`). Dropping the fill releases every
  chunk ref AND the admission permits (after the gather, or inside the dead completion channel on a
  cancelled requester — exact accounting under future-drop).
- **The gather law** (design §4.4): the demand serve (ledger R-S, the one lawful pass) BECOMES the
  gather; `read_fill_dma_bytes` is NOT incremented by lane fills — `zcrx_fill_bytes` is the fill
  provenance; extended closure: every served byte's provenance ∈ {pooled DMA, zcrx area, dest DMA}
  (`:516-520`). The area is NEVER a cache tier: refs are serve-lifetime only; tier admission / hold
  retention pay an explicit gather into pooled memory (`:521-527`, Rev 2 `:325-328`).
- **Gather fusion (Z3)**: `read_into_dest(off, dest, len)` lands each sub-command's gather DIRECTLY
  in the caller's registered destination; AREA-backend-only by law (`dest_serve_eligible` = every
  queue is `Area`; the classic reader writes from a FOREIGN task = the MEM-1 hazard for registered
  ent/arena memory under cancellation) (`initiator.rs:1208-1249`). Counted in
  `zcrx_dest_gather_bytes` (subset of `zcrx_gather_bytes`).
- **Whole-read atomic admission** (`read_into_wrapped` + `admit_whole_read`, `:1251-1403`; round 8
  finding I): segment plan first (`cap = max(max_xfer, 1 << lba_shift)`, queue by a `fetch_add`
  rotor); per-queue TOTAL units `ceil(seg / 4096)` taken in ONE `try_acquire` per queue — never per
  segment (partial holds shredded the window: fewer whole reads admitted than its capacity, worst
  case zero); any queue's refusal drops every held permit and the read DECLINES
  (`WouldBlock`, `zcrx_area_admission_waits += 1` once per read, `:1383-1399`); the owned units
  SPLIT per segment and ride entry → fill. **`gather ≡ fill` unconditionally**: `zcrx_gather_bytes
  += len` at the WHOLE-read success boundary (`:1312-1326`), and the funnel counts `zcrx_fills` /
  `zcrx_fill_bytes` at the same boundary (`nvme_dev.rs:2298-2304, 2347-2353`) — a torn
  multi-segment read contributes to neither (the +6.3 MB poisoned-row delta, design `:367-371`).
- `read_segment_area` (`:1437-1504`): `take_cid` → `table.insert` → `encode_read_capsule(cid, nsid,
  off >> lba_shift, len >> lba_shift, len)` → `sink.send` (failure ⇒ `table.cancel`) → await with
  `LANE_READ_TIMEOUT` → `gather_into(dest)`; timeout ⇒ `q.drain()` (close doorbell, JOIN the
  driver) THEN `poison(drain = true)` (quiescence before poison, no pre-store — the pre-store
  suppressed the first-swap log: silent poison, round 8). QUIRK 1.
- `range_eligible(off, len) = len > 0 ∧ off & mask == 0 ∧ len & mask == 0`, `mask = (1 << lba_shift)
  − 1` (`:1155-1158`).

---

## 5. The PDU streaming parser (`pdu_stream.rs`; design §4.3 `:489-507`)

Push-based, sync, one instance per queue, shared by the sim reader and the real driver
(`:10-14`). States (`:34-53`): `Header{have, need}` (accumulating CH+PSH into scratch), `Pad{left,
cid, datao, datal, last, success}` (skipping PDO padding by arithmetic — no copy, no emit),
`Payload{left, cid, next_datao, last, success}` (mapping payload to ref spans).

`push(chunk: &AreaSlice, out)` (`:82-157`) walks the extent:

1. **Header**: copy `min(need − have, remaining)` bytes into scratch, `zcrx_hdr_copy_bytes += take`
   (the priced edge copy, `:88-96`); at `have == 8` read `hlen = scratch[2]`, refuse `hlen ∉
   [8, 128]`, extend `need = hlen` (`:100-110`); at `have == need` → `dispatch_header`.
2. **dispatch** (`:190-226`): `parse_common`; `C2HData` ⇒ `parse_c2h_data` (geometry law), `pad =
   pdo − hlen`; `pad > 0` ⇒ `Pad`, else `enter_payload` (`datal == 0` ⇒ emit `C2hEnd` and reset;
   else `Payload`, `:166-187`). `CapsuleResp` ⇒ `hlen < 24` refused; the 16 B CQE IS the PSH
   (`scratch[8..24]`) — consuming past it desynchronises the stream (the Z1 contract catch: every
   CapsuleResp-completed read hung while SUCCESS-elision passed, `:212-222`, `initiator.rs:1750-1760`);
   emit `Cqe`, reset. Any other type ⇒ `Err("unexpected PDU type … on IO queue")`.
3. **Pad**: consume `min(left, remaining)`; at 0 → `enter_payload` (`:113-129`).
4. **Payload**: emit `C2hSpan{cid, datao: next_datao, slice: chunk.sub(pos, take)}` (a REF
   holding a cloned chunk grant, never a copy), advance; at `left == 0` emit `C2hEnd` and reset
   (`:130-153`).

Laws: headers may split at ANY byte across chunk seams (the `SQUEEZEFS_ZCRX_LANE_SIM_CHUNK` lever
shrinks chunks to force it; `initiator.rs:435-449`); the scratch is 128 B (`:57`) — charter: "≤ 24 B"
(QUIRK 2: on an IO queue with digests off both accepted PDU types have `hlen == 24` exactly; the port
pins a 24 B scratch and refuses any other `hlen`). Priced: `< 24 B per PDU ≈ 0.0015 passes/byte at
16 KiB C2HData grains` (design `:494-497`). Every `Err` is a FRAMING violation the CALLER counts
(`zcrx_frame_violations`) and poisons on (`:13-15`; `uring_zcrx.rs:1311-1325`, `area_queue.rs:375-388`).
Totality: the parser never panics on arbitrary bytes — every refusal is a verdict; the bench
`zcrx_pdu_stream` streams a synthetic C2HData sequence through area chunks (`benches/zcrx_bench.rs:91-129`).
Resync is NOT attempted: a malformed PDU poisons the SESSION (design §7 "Mid-stream framing
violation") — the stream has no self-synchronising marker, and a lane that re-synchronised would
be guessing which bytes were payload.

Digests: negotiated OFF at ICReq/ICResp (§2.1); the parser therefore has no digest verification
step. DDGST present would break `plen == pdo + datal`; HDGST present (CH flag bit 0, `pdo ≥ 28`)
would be silently skipped as padding — QUIRK 3.

---

## 6. The lane io_uring loop (`uring_zcrx.rs:957-1414`)

### 6.1 Arm order on the driver thread (SINGLE_ISSUER law, `:19-24`)

Thread `sqz-zcrx-q<rxq>` (`:963`): pin to the NIC's NUMA node (`:981-984`) → `RawRing::new(sq,
cq)` with `CQE32 | SINGLE_ISSUER | DEFER_TASKRUN | CQSIZE | CLAMP` (`:282-289`) → anonymous refill
region → `REGISTER_ZCRX_IFQ` (refusal names `if_idx`/`rxq`) → `ready.send(Ok)` → park on `go`
(`false`/dropped ⇒ exit silently: nothing armed, nothing to poison, `:1040-1044`) → governor born
(`:1052`) → arm the doorbell (`READ` 8 B from the `RingCmd` eventfd, `user_data = TAG_DOORBELL 2`)
and the multishot `RECV_ZC` (`TAG_RECV 1`) (`:1055-1077`). `RingCmd` = `SegQueue<Vec<u8>>` +
`eventfd(EFD_CLOEXEC | EFD_NONBLOCK)` doorbell + `closed` latch (`:521-567`): requesters push
capsules and ring; `close` latches and rings so a parked driver observes teardown (pinned
`:2141-2187`). QUIRK 11.

### 6.2 CQ sizing and the either/or clamp (`:598-655`; round 7 field: 8 queues died `RECV_ZC
terminal: ENOSPC` ~134 MB into the first healthy row)

`cq_entries_for(depth, max_xfer, chunk, sq, mtu)`: `flat = ceil(max_xfer / chunk)`; with a known
MTU `per_cmd = max(flat, ceil(max_xfer / mtu) × (burst / chunk))` else `2 × flat` (occupancy ½);
`cq = next_pow2(depth × per_cmd + sq)` clamped to `KERNEL_MAX_CQ_ENTRIES` (`:617-634`). Field:
`depth 64 × 351 + 128 = 22,592 → 32,768` (pinned `:1740-1778`). Kernel law being sized for: a
full CQ makes `io_cqe_cache_refill(overflow = false)` fail — zcrx NEVER takes the overflow path
(`io_zcrx_queue_cqe → io_defer_get_uncommited_cqe → io_get_cqe`, io_uring.h:286-293 → :257-264) and
the recv terminates `-ENOSPC` (zcrx.c:1280/:1317); userspace must size adequately + reap promptly.
If the derived CQ hits the kernel max, admission shrinks: `cq_admitted_window_bytes(cq, sq, chunk,
mtu) = (cq − sq) × chunk × payload / burst` (unknown MTU ⇒ `/2`), and `admit_window = min(W, that)`
(`:644-655`, `initiator.rs:794-796`).

### 6.3 The pass (`:1093-1412`)

1. Exit when `shared.poisoned ∨ cmds.closed` — closing the ring fd IS the teardown: the kernel
   restarts the NIC queue and reclaims the provider (`:1094-1099`; design §5 `:547-550`).
2. Refill (§3.3). 3. `gov.rearm_due()` ⇒ push `RECV_ZC` (an unarmed recv ALWAYS retries — a dry
   pool answers with one cheap `ENOMEM` CQE; the retry IS the probe, `:1125-1131, 751-762`).
4. Starvation bookkeeping (§6.4). 5. Capsules → `SEND` SQEs (`MSG_NOSIGNAL`, `user_data =
   TAG_SEND_BASE 0x1000 + seq`, buffer kept in `SendBufs` until its CQE; partial sends resubmit the
   remainder under the same `user_data`, `:1217-1232, 1384-1409`).
6. Enter: `gov.poll_bounded(pending_work)` ⇒ `enter(to_submit, 0)` + `sleep(200 µs)` (QUIRK 8),
   else `enter(to_submit, 1)` (`:1234-1244`).
7. Reap (`:1246-1412`): `TAG_RECV` with `res > 0` ⇒ progress ends any episode (tail → `zcrx_starved_ms`,
   clear `starved`, log "refill recovered"); provenance gate ⇒ violation; span gate ⇒ terminal;
   slot/adopt/slice → `parser.push` → `table.apply` (either `Err` ⇒ `zcrx_frame_violations += 1`,
   return `Err` ⇒ poison with drain). Without `F_MORE`: `classify_recv_end(res)` (`:870-885`):
   `0 ⇒ Eof` (table empty ⇒ orderly `Ok`; else "connection closed mid-operation"); `−ENOMEM`
   (`pool_dry`: provider pool has no free niov), `−ENOBUFS` (`rq_empty`: the RQ ring starved),
   `−ENOSPC` (`cq_full`: the lane's own CQ) ⇒ `Park` — flow control, never device death (field
   finding E: three queues died `terminal: Cannot allocate memory` in the arm's first second;
   `park_class_name`, `:887-907`); `res > 0 ⇒ Rearm` immediately; anything else ⇒ `Terminal`
   (`"RECV_ZC terminal: <errno>"` ⇒ poison). `TAG_DOORBELL` ⇒ re-arm the READ. Unknown `user_data`
   ⇒ terminal.

### 6.4 The park governor and the two structural arms (`RecvGovernor`, `:681-847`)

Field round 5 "THE PARK NEVER WOKE": the release-hook wake set could never fire with the pool
structurally dry, the rq ring not full and every span held by PARTIAL fills — deadlock by
construction; so every parked state POLLS bounded instead (`:681-689, 1234-1238`).

| Verb | Law | Source |
|---|---|---|
| `on_park(now) → bool` | unarm; a NEW episode starts iff none is open (count `zcrx_recv_parks` + the errno class ONCE per episode; retries never recount) | `:725-733, 1341-1365` |
| `on_progress(now) → Option<tail>` | the ONLY verb that ends an episode; resets the failover streak; the tail joins `starved_total` | `:735-749` |
| `rearm_due()` | once per park, unconditional | `:751-762` |
| `on_idle_drain()` | queue drained while starved: unlatch the probe gate ONCE (clear `starved` so one read may re-enter) WITHOUT ending the episode (ending it recounted a park per 200 µs retry — `zcrx_recv_parks = 2,860,473`, round 6) | `:770-781, 1133-1147` |
| `poll_bounded(pending)` | `episode_active ∧ pending_work` ⇒ poll 200 µs (vs the 235 µs fabric-RTT class); idle-starved blocks in enter | `:783-790` |
| `failover_due(now, bound) → Option<window>` | at most once per `bound` since the window start; restarts the window, `failover_streak += 1`, accounts `window` into `starved_total` | `:826-846` |
| `structural()` | `failover_streak ≥ REFILL_STRUCTURAL_FAILOVERS = 2` — the FIRST failover's releases ARE the recovery mechanism; a second consecutive window proves they did not refill the pool (finding J; field: 0.08 % engagement while 8 excluded queues cost the kernel path ~40 %) | `:657-666, 796-798` |
| `economic_structural(now, bound)` | `lifetime ≥ 2 × bound ∧ starved_total × 2 ≥ lifetime` — the trickle-proof arm: the round-1 tape (parks 5, failovers 10, 0.6 GB of ~1500 GB) cycled park → failover → trickle and never fired `structural()`; ½ = the majority boundary (rent paid for the whole life, savings only while serving) | `:800-824`; design `:110-124` |
| `park_fail_bound()` | `LANE_READ_TIMEOUT / 32 ≈ 937 ms` — ≥ ~4,000 × the 235 µs RTT class, ≤ ~3 % of the read timeout (round 5: 82 reads waited the full 30 s and every queue died by timeout-poison) | `:668-679` |

On `failover_due` (`:1148-1215`): `zcrx_recv_failovers += 1`, `zcrx_starved_ms += window`, latch
`shared.starved = true` (NEW reads bypass the lane, §8), evaluate `structural() ∨ econ`; first
latch of `starved_structural` ⇒ `zcrx_structural_teardowns += 1` + ERROR naming WHICH arm; then
WARN and `table.fail_all("zcrx refill starvation — failed over to the kernel path")` — failing the
fills is ALSO the recovery (their spans release, rqes post, the pool refills). The funnel sees
`refill_structural()` and runs `spawn_teardown` (§8): the session releases ifqs/rules/RSS/lease and
the kernel path serves this device at full width for the rest of the mount — "structural
starvation tears the session down" (charter `:230`). Lazy re-arm is a filed follow-on (design
`:76-79, 362-366`).

Whole-NIC release (`nic_census.rs`, `mod.rs:80-112`, design `:194-203, 258-264`): per ifindex
`{armed, structural, released}`; `note_structural` ⇒ `structural × 2 ≥ max(armed − released, 1)`
(the LIVE-armed denominator; the voter votes while still counted) ⇒ `sweep_nic_sessions` tears
every other live session on the NIC down (depth-one recursion: a swept peer is never structural).
Round-2 field: 5 of 10 tore down and the surviving 5 held ~16 % of queue width at ~0 engagement.

### 6.5 Exit laws

`SlotBag::drop` returns every raw ledger ref (`:914-935`); `SendBufs::drop` FORGETS in-flight
capsule buffers (ring-fd close cancels asynchronously in the kernel; freeing is a UAF window; leak
≤ depth tiny capsules per session death, `:937-952`) — QUIRK 9; `spawn_ring_driver` poisons with
`drain = true` on any `Err` (the driver is exiting, `:966-971`). `driver_reports_the_ifq_
registration_refusal_and_poisons_the_lane` (`:2273-2326`) is the universally reachable arm on every
dev box (ifindex 0 cannot register).

---

## 7. Queue isolation end to end (`steering.rs`, `ethtool.rs`, `ethtool_nl.rs`, `rxq_alloc.rs`)

### 7.1 Derivations

- **Eligible pool** `lane_eligible_queues(channels, devices) = [channels − clamp(devices,
  channels/4, channels/2), channels)`; `channels/4 == 0` ⇒ EMPTY (never dedicate ZC queues on a
  sub-4-queue NIC) (`steering.rs:186-194`). Floor = the standing ¼ posture (a sole-device arm is
  byte-identical to Rev 3); ceiling = the kernel path keeps ≥ HALF the NIC's RSS width. Field:
  `clamp(10, 8, 16) = 10` — every device leases one queue (the round-8 flat `/4` pool left 2 of 10
  devices laneless = 20 % of row bytes structurally kernel-path; design `:27-39`). Tie
  `derivation_sweep_tests.rs:1193-1208`: `(32,10)→10, (32,5)→8, (32,0)→8, (32,100)→16, (64,20)→20,
  (3,10)→0`.
- **Picks** `lane_queue_picks(channels, devices, want)` = the HIGHEST-indexed `want` of the pool
  (`:200-204`). **Fair want** `fair_queue_want(eligible, devices, geometry_want) = clamp(eligible /
  max(devices,1), 1, geometry_want)` (`:759-763`; field `clamp(10/10, 1, 4) = 1`). **Geometry want**
  `clamp(cpus/8, 1, LANE_IO_QUEUES_MAX = 8)` (`probe.rs:127, 156-160`; the 8 bounds the
  pre-steering pinned footprint at `8 × (64 MiB + slack + ring)` per device).
- **Reserved locs** `reserved_loc_range(table) = [table − min(64, max(table,1)), table)`
  (`:576-582`); `STEERING_RESERVED_SLOTS = 64` (`:30`) — QUIRK 7.
- **Restricted RSS** `restricted_rss(prior, lane_queues, channels)`: non-lane entries verbatim;
  entries pointing at a lane queue remapped round-robin over the remaining queues (`:587-605`).

### 7.2 The rxq arbiter (`rxq_alloc.rs`; field row 3 `REGISTER_ZCRX_IFQ (if_idx=6, rxq=31): EEXIST`)

Process-wide per-ifindex `{in_use: BTreeSet, retired: VecDeque (release order)}` (`:51-62`).
`acquire(ifindex, ifname, channels, devices, want)` (`:83-169`): `want == 0` refuses; empty pool
refuses naming "needs ≥ 4"; goal `min(want, pool_len)`; grant = the pure picks that are FRESH
(neither in use nor recently freed) → fill highest-first from the fresh pool → reuse `retired`
LEAST-recently-freed first (the kernel's ifq teardown is ASYNC — ring-fd close defers the
unregister — so an instantly re-granted index re-registers into EEXIST; finding 2); empty grant ⇒
loud refusal naming NIC, channel count, derived pool, `clamp(devices, /4, /2)`, leased count,
demand and the remedy `ethtool -L <if> combined <N>` (`:140-150`); partial grant degrades the
session's queue count with a WARN (`:152-160`). `RxqLease::drop` releases to the BACK of `retired`
(`:39-43, 171-186`). Cross-PROCESS arbitration stays the kernel's EEXIST (`:15-17`). Tests
`:195-338`.

### 7.3 The NIC control seam and its live implementation

`NicControl` (`steering.rs:129-170`): `ifname, combined_channels, tcp_data_split_on, rxfh_indir,
set_rxfh_indir, ntuple_enabled, ntuple_table_size, special_loc_supported, ntuple_table_size_hint,
ntuple_locs, insert_ntuple(loc, rule) → effective loc, delete_ntuple(loc)`. `EthtoolNic`
(`ethtool.rs`) over ONE `AF_INET/SOCK_DGRAM` socket and `SIOCETHTOOL 0x8946` (`:21, 163-210`):

| Method | ioctl / command | Detail | Source |
|---|---|---|---|
| `combined_channels` | `ETHTOOL_GCHANNELS 0x3c` | `max(combined_count, rx_count)` | `:258-267` |
| `rx_ring_descriptors` | `ETHTOOL_GRINGPARAM 0x10` | `rx_pending` (36 B struct) | `:186-194` |
| `ntuple_enabled` | `ETHTOOL_GFLAGS 0x25` | `data & ETH_FLAG_NTUPLE (1<<27)` | `:36-40, 298-306` |
| `ntuple_table_size` / `special_loc_supported` | `ETHTOOL_GRXCLSRLCNT 0x2e` | `data & !RX_CLS_LOC_SPECIAL (0x8000_0000)` / the flag bit | `:308-329` |
| `ntuple_table_size_hint` / `ntuple_locs` | `ETHTOOL_GRXCLSRLALL 0x30` | `data` = size (mlx5 reports 1024 here while GRXCLSRLCNT says 0); `rule_locs[]` tail | `:331-392` |
| `rxfh_indir` / `set_rxfh_indir` | `ETHTOOL_GRSSH 0x46` / `SRSSH 0x47` | sizes-only call then `[head 24 B ‖ indir u32 × n]`; `hfunc 0, key_size 0` = keep | `:213-221, 273-296` |
| `insert_ntuple` / `delete_ntuple` | `ETHTOOL_SRXCLSRLINS 0x32` / `SRXCLSRLDEL 0x31` | `ethtool_rx_flow_spec` 168 B: `flow_type TCP_V4_FLOW 1 / TCP_V6_FLOW 5`, `h_u` = `ip4src, ip4dst, psrc(be), pdst(be)` at 0/4/8/10 (v6: 0/16/32/34), masks all-ones, `ring_cookie = queue @152`, `location @160`; the kernel echoes the EFFECTIVE location | `:223-250, 394-417` |
| `tcp_data_split_on` | genetlink | §7.4 | `:269-271` |

Layout asserts (`:133-145`): `ethtool_value 8`, `ringparam 36`, `channels 36`, `rx_flow_spec 168`
(`ring_cookie @152`, `location @160`), `rxnfc 192` (`fs @16`, `rule_cnt @184`, `rule_locs @188`),
`rxfh head 24`. `nic_mtu` = `/sys/class/net/<if>/mtu` (`:486-492`).

### 7.4 HDS probe — genetlink, compare the ATTRIBUTE (`ethtool_nl.rs`)

Two round trips on `AF_NETLINK/SOCK_RAW/NETLINK_GENERIC`: `CTRL_CMD_GETFAMILY("ethtool")` on
`GENL_ID_CTRL 16` → family id (`:175-191`); `ETHTOOL_MSG_RINGS_GET 15` v1 with nested
`ETHTOOL_A_RINGS_HEADER 1 { ETHTOOL_A_HEADER_DEV_NAME 2 }` (`:196-215`); verdict =
`ETHTOOL_A_RINGS_TCP_DATA_SPLIT 11 == ETHTOOL_TCP_DATA_SPLIT_ENABLED 2`; attribute absent ⇒ `false`
(driver predates HDS); `NLMSG_ERROR` code ≠ 0 ⇒ `Err` (`:216-227`). The 2026-08-02 lesson: a rendered
string compared ("TCP data split: on" under ethtool 7.1) mislabelled the whole cell driver-blocked
(`:1-7`; design `:398-402`). Requirement `tcp-data-split on` (+ thresh 0) (design `:551-555`).

### 7.5 The arm state machine (`steering.rs:259-552, 609-837`; laws `tests/zcrx_steering_tests.rs:8-25`)

- **Capacity gate** (`steering_capacity_gate`, `:216-250`, READ-ONLY — the lane never flips NIC
  features): ntuple OFF ⇒ refuse naming `ethtool -K <if> ntuple on`; `reserved_loc_range(table)`
  empty with the feature ON = the mlx5-class advertisement LIE (0-size table, inserts succeed:
  auto-loc returned rule ID 1023) ⇒ `RuleSlots::KernelAssigned` (INFO once per NIC); else
  `Reserved{lo, hi}`.
- **Phase A** `arm_rss_exclusion(nic, lane_queues)` (`:259-350`): probes (HDS, channels, gate)
  before ANY mutation; under the `LIVE_ARMS` lock: reserved class needs `lanes ≤ free slots` up
  front (flows ≡ queues 1:1); read the current RSS table; pristine = the FIRST armer's read; write
  `restricted_rss(pristine, UNION of live lane queues, channels)`; register the session
  (`sessions += 1`, `queues`). Returns `SteeringGuard{rss_touched, registered, rule_locs = []}`.
- **Phase B** `arm_flow_rules(nic, guard, flows)` (`:362-552`): `Reserved` ⇒ **crash-residue
  reap** — delete every installed rule in `[lo, hi)` NOT owned by a live session (rules outside the
  range are operator-owned, never touched) — then explicit locs at the first free reserved slots;
  `KernelAssigned` ⇒ the ethtool-parity ladder: `RX_CLS_LOC_ANY 0xffff_ffff` if
  `special_loc_supported` (mlx5 does NOT set it and bounces `@ANY` with ENOSPC — finding A), else
  self-select the highest free explicit loc below the `GRXCLSRLALL` size (`rxclass_find_empty_slot`
  parity; `0` ⇒ refuse), recording the RETURNED ids; NO range reap for this class. A refusal rolls
  back ONLY this call's inserts and leaves the guard armed — re-including the queues in RSS while
  ifqs are bound is exactly the EFAULT window; the caller tears the ifqs down FIRST (`:355-361,
  431-449`).
- **Restore** (`restore_locked`, `:638-701`, idempotent, collects every failure loud): delete this
  guard's rules; deregister; RSS = pristine with the surviving PEERS' queues still excluded; the
  LAST restore writes the pristine table — restores converge in ANY order.
- **Drop** (`:709-745`): deregisters so residue is reapable and peers stop excluding us; an
  unrestored drop with mutations logs ERROR ("NIC state leaked"). `ArmedSteering<N>` owns NIC +
  guard from birth and `restore_now`s on Drop (finding B: a cancelled arm future left the live NIC
  at 75 % RSS width for a run AND the next arm recorded the crippled table as pristine, `:765-814`).
- **`arm_steering`** = the one-shot composition with TOTAL rollback for callers whose flows exist
  (`:824-837`); the PRODUCT uses the split halves with the ifq teardown between.
- Verification of the split: the design's "verify first fill's chunk provenance == area" is the
  per-CQE provenance gate (§1.2); fallback-copy contamination (packets reaching a zcrx queue outside
  the provider — mis-steered flows, retransmits from linear skbs — are copied by the kernel into
  area chunks: correct but silently un-zero-copy) is residual risk 1 (design `:667-673`) — the
  engagement gauges and per-queue `rx[i]_pp_*` ethtool counters are the instrument (design `:253-257`).

---

## 8. How the read path dispatches a cold fill (`nvme_dev.rs:2246-2376, 2695-2738`)

The ONE funnel `read_block_with_dest_inner(offset, size, dest_addr, count)`: `size == 0` ⇒ empty
(`:2714-2716`); pooled reads over the pool slot size refuse (`:2717-2723`); **then `try_lane_read`
FIRST** — `Some(result)` = the lane served (or failed terminally); `None` = kernel-path worker
unchanged (`:2725-2738`). Every device read class reaches it: pooled cohort fills (dest-less) and
lease/ranged dest-carrying windows (design `:87-96`).

`try_lane_read` ladder (`:2246-2376`):

| Step | Verdict | Counted |
|---|---|---|
| `lane_env_armed()` off | `None` | — |
| `lane.get_or_init(arm_for_device)` is `None` | `None` (refusal cached per device per mount) | — (QUIRK 12) |
| `sess.poisoned() ∨ sess.refill_structural()` | `spawn_teardown()`; `None` | — |
| `sess.refill_degraded()` (any queue `starved`) | `None` — the lane declines while recovering | `zcrx_degraded_bypasses` (Rev 4a: the formerly UNCOUNTED gate that held two campaigns at ~0 engagement) |
| `!range_eligible(offset, size)` (LBA alignment, nonzero) | `None` | — (ineligibility, not fallback) |
| `dest_addr = Some` ∧ `!dest_serve_eligible()` | `None` | — |
| `read_into_dest` / `read_into_pooled` Ok | `Some(Ok(bytes))` | `zcrx_fills`, `zcrx_fill_bytes += size` |
| `Err(WouldBlock)` (admission decline) | `None` | `zcrx_area_admission_waits` (at the decline site) |
| any other `Err` | WARN (per op) + `None` ⇒ kernel-path retry into the SAME dest (idempotent; partial gathers overwritten) | `zcrx_fill_fallbacks` (≈ 0) |

The three volume gates are mutually exclusive counters: `zcrx_degraded_bypasses` (the pool term),
`zcrx_area_admission_waits` (the window), `zcrx_fill_fallbacks` (per-op errors) (design `:97-103`).
Fallback is LOUD (rate-limited WARN naming offset/size) never silent; ineligibility is uncounted BY
DESIGN (the kernel path is simply the serve). Eligibility in design §6 (`:557-567`): armed session,
LBA-aligned offset+size, size > 0; both pooled and dest shapes since Z3. Composition with the dest
lease (design §12 `:683-738`): a leased window arrives as a dest-carrying LBA-aligned ranged read =
exactly the fused-gather shape; the lease's incarnation/still/binding ceremony is vehicle-blind;
zero daemon passes is NOT expressible (the area is a NIC-owned arrival-ordered pool, no mechanism
aims a specific C2HData at a specific ent window; the FUSE commit copy imports the REGISTER-time ent
VA) — ONE fused gather is the floor until the staged zc-serve kernel surface ships.

Port hook: the Zig read path's device leg is the `Volumes.read_range` seam
(`src/data/read_path.zig:306-312`), called at the dest-lease ranged DMA (`:1134`), the bounce
(`:1142`), the raw full-block dest leg (`:1059`) and the whole-block fill (`:1271, 1280`) — the lane
sits BEHIND that seam in the data plane's volume binding (`data/plane.zig`), judged per call.

---

## 9. Knobs, gauges, constants, measured numbers

### 9.1 Knobs (`src/env_knobs.rs:304-308`; port classes per `src/core/knobs.zig:57-66`)

| Reference knob | Kind / default | Reference use | Port name / class |
|---|---|---|---|
| `SQUEEZEFS_ZCRX_LANE` | Bool, off | master opt-in (`mod.rs:114-117`) | `SQUEEZEFS_ZCRX_LANE` — A/B lever (default off until the field adjudication; logged at mount when on; the arm is probe-gated regardless) |
| `SQUEEZEFS_ZCRX_LANE_FORCE_COPY` | Bool, off | classic-recv backend (`:120-123`) | DELETE (QUIRK 16: the classic reader is the Z1 scaffold the area backend superseded) |
| `SQUEEZEFS_ZCRX_LANE_AREA_SIM` | Bool, off | area backend over socket recv (`:128-131`) | `SQUEEZEFS_TEST_ZCRX_AREA_SIM` — test seam |
| `SQUEEZEFS_ZCRX_LANE_SIM_CHUNK` | Int 1..2^40, derived | sim chunk grain, pow2, clamp 64 B..PMD (`initiator.rs:435-449`) | `SQUEEZEFS_TEST_ZCRX_SIM_CHUNK` — test seam |
| `SQUEEZEFS_ZCRX_LANE_TARGET` | Str, none | `traddr,trsvcid,subnqn,nsid,lba_shift,max_xfer[,io_queues,queue_depth]`; malformed ⇒ loud, not armed (`mod.rs:145-161`) | `SQUEEZEFS_TEST_ZCRX_TARGET` — test seam (the one Str-kind row; the parser is the knob registry's) |

### 9.2 Gauges (`fuse_client.rs:5774-5852`; port primitive in brackets)

| Gauge | Meaning | Law |
|---|---|---|
| `zcrx_lane_armed` [Gauge 0/1] | set at arm; dropped by poison | engagement level |
| `zcrx_fills`, `zcrx_fill_bytes` [Sharded] | lane-served reads / bytes at the WHOLE-read success | a lane row is INVALID unless `fill_bytes` Δ accounts for the row's cold-fill bytes (≥ 50 % = the serve-the-bulk bar, design `:758-760`) |
| `zcrx_gather_bytes` [Sharded] | the priced completion pass | **`gather ≡ fill` byte-exact, unconditionally** |
| `zcrx_dest_gather_bytes` [Sharded] | fused SUBSET of gather (registered dests) | `gather − dest_gather` = remaining two-pass traffic |
| `zcrx_fill_fallbacks` [Sharded] | per-op lane→kernel retries | ≈ 0 |
| `zcrx_frame_violations` [Tripwire] | framing violations (each poisoned a session) | must-stay-0 |
| `zcrx_lane_poisoned` [Tripwire] | session poison transitions (REAL transport poison only) | **must-stay-0**; teardown and failovers are NOT poison |
| `zcrx_conn_errors` [Sharded] | arm failures | — |
| `zcrx_hdr_copy_bytes` [Sharded] | the header edge copy | ≪ 1 % of fill bytes by construction |
| `zcrx_area_bytes` [Gauge] | live area bytes = the R5 component source | — |
| `zcrx_area_admission_waits` [Sharded] | whole-read DECLINES (one per declined read) | bounded; the SIZING instrument |
| `zcrx_degraded_bypasses` [Sharded] | funnel reads bypassing a starved lane | ≈ 0 on an engaged row |
| `zcrx_recv_parks` [Sharded] | starvation EPISODES | `parks ≡ pool_dry + rq_empty + cq_full` |
| `zcrx_parks_{pool_dry,rq_empty,cq_full}` [Sharded] | the episode-starting errno class | the round-4 discriminator |
| `zcrx_recv_failovers` [Sharded] | windows past `park_fail_bound` | sustained ≈ 1/bound = a structurally dysfunctional pool |
| `zcrx_starved_ms` [Sharded] | cumulative starved time (windows + tails, exact) | vs armed wall = the live starved share |
| `zcrx_structural_teardowns` [Sharded] | structural escalations latched (either arm) | "did no-harm fire?" is a number |

Row-validity laws (design §13 `:756-775`): `poisoned = 0`, `frame_violations = 0`, `fallbacks ≈ 0`,
`bypasses ≈ 0`, parks/failovers bounded, six consecutive pristine teardowns (0 rules, full RSS).

### 9.3 Constants with WHY

| Constant | Value | WHY | Source |
|---|---|---|---|
| `LANE_MAX_XFER_CAP_BYTES` | 1 MiB = the FUSE transport's `PAYLOAD_BASE` | the largest single destination a dest-serve fills in one command; the Z2-benched MDTS face; one definition | `probe.rs:163-173`; tie `derivation_sweep_tests.rs:1174-1179` |
| `LANE_IO_QUEUES_MAX` | 8 | pre-steering pinned-footprint rail (the real ceiling is the census pool at steering) | `probe.rs:114-127` |
| `lane_geometry` | `(clamp(cpus/8, 1, 8), clamp(cpus × 2, 4, 64))` | queues: the §8 possible-CPUs slope; depth: the OFFERED-CONCURRENCY slope (BDP depth adjudicated AGAINST — self-fulfilling under demand-concurrent venues, falsified twice; at fabric RTT ~6 MiB would decline nearly everything); 64 × 1 MiB = 64 MiB ≥ the ~51 MiB/device offered | `probe.rs:129-160`; design `:65-74` |
| `ADMISSION_UNIT` | 4096 | one page class; composable with chunk arithmetic | `area.rs:30-33` |
| `PMD_BYTES` | 2 MiB | the huge-page granule | `area.rs:19-21` |
| `MPWQE_SKB_LINEAR_OVERHEAD_BYTES` | 512 | ≤ 498 real, ceiled — over-estimation rounds UP (safe) | `area.rs:93-97` |
| `REFILL_STRUCTURAL_FAILOVERS` | 2 | the first failover is the transient allowance (its releases ARE the recovery); the second proves it failed | `uring_zcrx.rs:657-666` |
| `park_fail_bound` | `LANE_READ_TIMEOUT / 32 ≈ 937 ms` | ≥ 4,000 × RTT, ≤ 3 % of the timeout | `:668-679` (QUIRK 1) |
| poll cadence | 200 µs | vs the 235 µs fabric-RTT class | `:1234-1241` (QUIRK 8) |
| `KERNEL_MAX_CQ_ENTRIES` | 65536 | `2 × IORING_MAX_ENTRIES`, io_uring.h:160-161 | `:594-597` |
| `sq_entries` | `next_pow2(depth + 8)` | commands + recv/doorbell + partial resubmits | `:576-580` |
| `STEERING_RESERVED_SLOTS` | 64 | (none stated) | `steering.rs:30` (QUIRK 7) |
| admin `sqsize0` | 31 | `NVME_AQ_DEPTH = 32` parity (bring-up only) | `initiator.rs:641-642` (QUIRK 4) |
| CSTS deadline / poll | 10 s / 20 ms | (none stated) | `:661-677` (QUIRK 4) |
| depth floor | 2 | `sqsize0 ≥ 1` | `:685` |
| `CONNECT_DATA_LEN` | 1024 | `sizeof(struct nvmf_connect_data)`, nvmet-enforced | `pdu.rs:170-177` |
| thread names | `sqz-zcrx-q<rxq>`, `sqz-zcrx-admin`, (sim: `-rd<qid>`, `-wr<qid>`) | the CPU-accounting class `sqz-zcrx` | `uring_zcrx.rs:963`, `initiator.rs:907`, `area_queue.rs:275, 287` |

### 9.4 Measured numbers (and what each justifies)

55.4 % of client cycles in the nvme-tcp RX copy at 46.6 GB/s (the term deleted); 0.69–0.72
passes/byte → 0.035 B/B DRAM (`design:391-396, 427-429`); 4 queues matched 24.79 GB/s: 3.32 → 1.06
cores; 8 queues at 49.5 GB/s: 7.94 → 2.75 (`:424-426`); the ~2.7 GB/s/core skb-walk class
(`:715-717`); engagement rounds: 0.05 % (round 8, the `/2` admission), 0.6 GB of ~1500 GB with
`starved_ms = 9370 ≡ 10 × bound` (round 2), 216 MiB area still starved (round 4 — the pool term
became a LIVE-input question, answered by the errno-class split) (`:81-264`); the `+6.3 MB`
poisoned-row gather delta that forced the whole-read accounting boundary (`:367-371`); `2,860,473`
parks when idle-drain ended episodes (`uring_zcrx.rs:1519-1521`); 82 reads × 30 s timeout-poison
(round 5, `:674-676`); `~40×` mlx5-lie INFO in 30 s (`steering.rs:553-555`); field geometry 32 RX
queues/rail, MTU 9000, 10 data namespaces, RSS width 22/32 per rail after arm (`design:746-752`).
The 35.5 GB/s composed-row bar = 85 % of the 41.8 raw ceiling (`design:776-780`); the phase-2
pricing: daemon passes on dest bytes 0 → 1 while kernel passes go 0.69 → 0, net ≈ −0.2 to −0.3
core-s/GB per lane byte on a box 92 % busy at 27.9 GB/s (`design:714-725`).

---

## 10. Tests (name — one sentence; venue)

**Portable (mock target / pure), `tests/zcrx_lane_tests.rs`:** `test_codec_common_header_roundtrip`
— ICReq CH + zeroed negotiation bytes. `test_codec_connect_capsule_layout` — every Connect byte
incl. the 1024 B blob. `test_codec_status_decode_names_sct_sc_dnr` — `0x400f`, `0x4182`, unknown
stays honest. `test_codec_read_capsule_layout` — nsid/slba/nlb0/0x5A/SGL len.
`test_codec_icresp_negotiation_laws` — digests/PFV/type refusals. `test_codec_c2h_geometry_law` —
`plen == pdo + datal`. `test_codec_cqe_status_strips_phase_bit`. `test_probe_sysfs_discovery_tcp_
and_refusals` — fixture sysfs tree; pcie and non-`nvme<C>n<N>` paths ineligible.
`test_probe_unlimited_mdts_sentinel_arms_with_saturated_xfer_cap` — the 2147483644 sentinel.
`test_association_and_multi_pdu_read`, `test_success_flag_elision_completes`,
`test_c2h_pdo_padding_is_skipped_and_priced`, `test_digest_demand_refuses_arm_loud`,
`test_error_status_fails_op_not_session`, `test_frame_violation_poisons_session_and_counts`,
`test_concurrent_reads_complete_by_cid`, `test_unaligned_range_is_ineligible`,
`test_disarmed_default_is_byte_identical_and_gauge_silent`, `test_funnel_engagement_via_target_
override`, `test_funnel_gauge_closure_byte_exact`, `test_funnel_lane_error_falls_back_to_kernel_
path`, `test_funnel_arm_refusal_without_force_copy_stays_kernel_path`,
`test_z2_area_sizing_derivation_laws`, `test_z2_arm_admission_red_blocks_new_arms`,
`test_z2_r5_component_registration_idempotent_and_gauged`, `test_z2_area_sim_multi_pdu_content_and_
full_recycle` — content byte-exact and every chunk back, `test_z2_area_sim_funnel_engagement_and_
gather_closure`, `test_z2_area_exhaustion_backpressures_admission_never_deadlocks`,
`test_z2_mid_flight_death_poisons_drains_and_falls_back`, `test_z2_frame_violation_in_area_mode_
poisons_and_recycles`, `test_z2_classic_backend_leaves_area_gauges_silent`, `test_mem3_classic_
cancellation_returns_cids_and_lane_survives`, `test_mem3_classic_cancellation_holds_destination_
keepalive`, `test_mem3_area_cancellation_returns_cids_chunks_and_stays_clean`, `test_z3_dest_fused_
serve_engages_and_closes`, `test_z3_pooled_reads_stay_two_pass_and_dest_gauge_silent`,
`test_z3_dest_reads_never_ride_the_classic_backend`, `test_z3_dest_lane_error_falls_back_to_kernel_
path`, `test_mock_refuses_oversize_connect_data_like_nvmet`, `test_rxq_arbiter_two_sessions_on_one_
nic_get_distinct_queues`, `test_rxq_arbiter_exhaustion_refuses_with_derived_numbers`,
`test_rxq_arbiter_release_then_reacquire_reuses_index`, `test_rxq_arbiter_range_derives_from_nic_
queue_count`, `test_steering_capacity_gate_ntuple_off_refuses_zero_advertised_arms`,
`test_rxq_arbiter_serial_rearm_rotates_pool_before_reuse`, `test_registration_rxqs_come_from_the_
arbiter_grant_disjoint_across_sessions`, `test_fair_queue_want_derives_from_nic_sharing`,
`test_tcp_devices_via_nic_counts_namespaces_behind_the_route`, `test_census_counts_native_multipath_
c_path_namespaces`, `test_ten_devices_one_nic_first_session_wants_one_queue`, `test_nic_note_once_
latches_per_tag_and_nic`, `test_f_shutdown_teardown_quiesces_and_releases_all_sessions`,
`test_f_poisoned_session_tears_down_promptly`, `test_teardown_is_not_poison`, `test_gather_fill_
closure_holds_across_mid_read_poison`, `test_poison_gauge_and_log_are_structurally_tied`,
`test_admission_overflow_declines_fast_never_parks`, `test_engagement_pool_scales_with_the_device_
census`, `test_engagement_all_field_devices_lease_a_queue`, `test_engagement_admission_is_the_full_
window_with_derived_slack`, `test_engagement_field_shape_admits_the_offered_row`, `test_compose_
dest_lease_shape_rides_the_fused_lane`, `test_degraded_bypass_is_counted_and_recovers`,
`test_pool_term_striding_rq_standing_demand`, `test_nic_census_majority_law`, `test_majority_
structural_releases_the_whole_nic`, `test_park_errno_class_is_attributable`, `test_park_class_
counters_export`, `test_nic_census_live_denominator_matches_the_field_tape`
(`tests/zcrx_lane_tests.rs:385-3225`). Mock target (`:17-329`): ICResp digest injection, C2HData span
size, SUCCESS elision, error status, corrupt `datao`, PDO pad, mid-C2HData death, nth-read error,
a read gate (cancellation venue), nvmet-strict 1024 B Connect check.

**Steering (mock NIC), `tests/zcrx_steering_tests.rs:232-836`:** `test_lane_queue_picks_are_highest_
indexed_and_bounded`, `test_reserved_loc_range_is_top_slots`, `test_restricted_rss_excludes_lane_
queues_and_preserves_rest`, `test_arm_applies_rss_restriction_and_reserved_rules`, `test_arm_failure_
at_every_step_leaves_nic_untouched` (the `FailAt` lattice), `test_crash_residue_reap_deletes_only_
reserved_range_rules`, `test_hds_off_refuses_with_zero_mutations`, `test_arm_refuses_empty_flows_and_
full_reserved_range`, `test_second_arm_on_one_nic_preserves_live_session_rules_and_locs`, `test_out_
of_order_restore_converges_to_pristine_rss`, `test_zero_advertised_table_arms_via_kernel_assigned_
ids_and_restores`, `test_zero_advertised_insert_failure_rolls_back_exactly`, `test_split_arm_rss_
exclusion_precedes_any_rule_install`, `test_split_arm_phase_b_failure_keeps_rss_excluded_until_
restore`, `test_mlx5_triple_falls_back_to_explicit_top_down_locs`, `test_any_refusal_despite_the_flag_
falls_back_to_explicit`, `test_hold_drop_converges_the_nic_without_explicit_restore`, `test_dropped_
hold_cannot_poison_the_next_arms_pristine`, `test_two_sessions_both_phase_b_enospc_converge_in_
either_order`, `test_phase_b_failure_beside_a_live_armed_session_keeps_it_steered`.

**Driver module (`uring_zcrx.rs:1454-2377`, any io_uring kernel):** `recv_governor_*` (6: cold-start
rearm, poll-bounded, idle-drain keeps episode, episodes-not-retries, failover once per window,
second failover structural), `recv_governor_economics_arm_{trickle_cannot_reset, majority_serving_
never_fires}`, `park_fail_bound_derives_from_the_one_read_timeout`, `recv_end_classification_parks_
not_poisons_on_pool_exhaustion`, `cq_sizing_derives_from_chunk_grain_completion_demand`, `cqe_
provenance_gate_rejects_a_foreign_area`, `cqe_span_gate_bounds_every_completion_against_the_area`,
proptest `cqe_gates_are_total_and_admit_only_in_area_spans`, `refill_region_len_is_page_rounded_…`,
`ifq_registration_args_echo_inputs_and_zero_every_kernel_out_param`, `raw_ring_setup_accepts_the_
lane_flags_and_round_trips_a_cqe`, `raw_ring_sq_is_bounded_…`, `raw_ring_indices_wrap_cleanly_…`,
`ring_fd_close_with_an_op_in_flight_is_the_crash_path_and_must_not_hang`, `refill_post_encodes_the_
rqe_and_advances_the_tail`, `refill_post_refuses_when_full_and_recovers_when_the_kernel_consumes`,
`refill_indices_wrap_by_mask_without_the_counters_wrapping_first`, `ring_cmd_queues_capsules_rings_
the_doorbell_and_latches_closed`, `ring_cmd_close_wakes_a_parked_driver`, `slot_bag_drop_returns_
every_ledger_ref`, `raw_slot_custody_round_trips_through_adopt_without_double_release`, `send_bufs_
drop_forgets_in_flight_capsules_but_frees_completed_ones`, `driver_reports_the_ifq_registration_
refusal_and_poisons_the_lane`, `driver_unwound_before_go_exits_clean_without_poisoning`. Ring-flag
refusals are LEDGERED capability skips (`uring_or_skip`, `:1457-1475`).

**Arbiter module** (`rxq_alloc.rs:195-338`, 7 tests) and `area_queue::admission_grants_the_full_
admitted_window` (`:399-416`). **Loom**: the two models (§3.2). **Derivation sweep**
(`derivation_sweep_tests.rs:1150-1220, 1451-1556`): `zcrx_lane_geometry_is_pinned_on_canonical_
shapes`, `zcrx_lane_xfer_cap_is_the_transport_payload_face`, `zcrx_engagement_geometry_derives_on_
canonical_shapes`, plus the fleet-share and physical-minima sweeps. **Bench** `benches/zcrx_bench.rs`:
`zcrx_span_ledger` (grant/release, add_ref/release, 4-thread contended), `zcrx_pdu_stream`,
`zcrx_gather` (two-pass vs fused at 128 KiB and 4 MiB; design `:312-315`).

**Needs a real nvmet-tcp target (loopback):** `test_live_nvmet_tcp_association_and_read_roundtrip`
(`tests:1909-1963`) — REAL nvmet accepts the association, one read round-trips, byte-parity with the
kernel initiator's device node when readable; skipped (`Hardware`) without a devsub-tcp namespace or
the target override. **Needs a zcrx-capable NIC (field-owed):** the armed serve loop, live
`EthtoolNic`/genetlink, `tests/fio/zcrx_field_rows.sh` A-B-B-A rows (design §13).

---

## 11. QUIRKS (decision 0002 ledger candidates)

| # | Reference quirk | Why it is poor | What we do |
|---|---|---|---|
| 1 | `LANE_READ_TIMEOUT = 30 s` per-op timeout on every lane read (`initiator.rs:46, 1475, 1547`), its expiry POISONS the queue; `park_fail_bound = T/32` derives from it | Decision 0002 row 34 bans per-op device-read timeout wrappers; a target that stops answering is a VOLUME event; a tuning constant dressed as a derivation | Reads wait unbounded; the lane's queues join `Device.overdueCensus` (submit stamps per CID) so the rung-3 watchdog logs and fences through the lattice; the failover bound derives from the measured fabric RTT class (`clamp(4000 × rtt_ns, 500 ms, 2 s)` — the reference's own stated reasoning, `uring_zcrx.rs:670-673`) with the RTT measured at the CSTS poll |
| 2 | 128 B parser scratch, `hlen ∈ [8, 128]` accepted on IO queues (`pdu_stream.rs:57, 103`) | Looser than the charter's "≤ 24 B"; with digests off both IO-queue PDU types have `hlen == 24` — accepting other forms is not a canonical total codec | `scratch: [24]u8`; `hlen != 24` is a framing violation; ICResp (128 B) is read in the bring-up phase, never by the stream parser |
| 3 | CH flag bits 0/1 (HDGSTF/DDGSTF) unchecked (`pdu.rs:392-393`) | A target that sends digests after negotiating them off is malformed; HDGST would be skipped as padding | `flags & 0x3 != 0` ⇒ violation |
| 4 | Admin `sqsize0 = 31`, CSTS poll 10 s / 20 ms sleeps ignoring `CAP.TO` (`initiator.rs:641, 661-677`) | Two literals; the timeout the spec defines (`CAP.TO × 500 ms`) is in the CAP word already fetched | `sqsize0 = NVME_AQ_DEPTH − 1` with the WHY on the line; CSTS deadline = `CAP.TO × 500 ms` (bits 31:24), poll on the Io timer at `max(deadline/64, 10 ms)` |
| 5 | Test seams named outside `SQUEEZEFS_TEST_*` (`AREA_SIM`, `SIM_CHUNK`, `TARGET`) | Charter §1.6 classes | Renamed `SQUEEZEFS_TEST_ZCRX_{AREA_SIM,SIM_CHUNK,TARGET}` |
| 6 | Five process-wide `LazyLock<Mutex<…>>` statics: `SESSIONS`, `LIVE_ARMS`, rxq `REGISTRY`, `CENSUS`, `NOTED` (`mod.rs:37`, `steering.rs:87, 560`, `rxq_alloc.rs:64`, `nic_census.rs:32`); tests pick unique ifindexes to avoid coupling | Hidden globals; explicit-allocator rule; the test-coupling smell proves it | ONE `LaneRegistry` struct (sessions, per-NIC live arms + rxq pool + census + notice latches) owned by the daemon, passed explicitly; tests build their own |
| 7 | `STEERING_RESERVED_SLOTS = 64` (`steering.rs:30`) | A constant with no stated reason | `reserved = max(channels/2, 1)` — one slot per leasable queue, so flows ≡ queues 1:1 always fits; tie test |
| 8 | Parked state = `enter(to_submit, 0)` + `thread::sleep(200 µs)` (`uring_zcrx.rs:1239-1241`) | Two syscalls per poll and a data CQE arriving during the sleep waits the full cadence | ONE `enter` with `EXT_ARG` timeout 200 µs (`Ring128.submitAndWaitTimed`); the doorbell/CQE wakes early |
| 9 | `SendBufs::drop` deliberately LEAKS in-flight capsule buffers because ring-fd close cancels asynchronously (`:937-952`) | A designed leak | Capsule buffers live in a session-lifetime arena (`depth × 72 B` per queue, zero per-op allocation); teardown issues `IORING_REGISTER_SYNC_CANCEL` (`IORING_ASYNC_CANCEL_ALL`) and waits before closing the fd; the arena frees after the join |
| 10 | `RingCmd` = `crossbeam::SegQueue<Vec<u8>>` — a heap `Vec` per capsule (`:522, 542-549`); `read_into_wrapped` builds a `HashMap<queue, AdmissionUnits>` per read (`initiator.rs:1363-1403`) | Per-op allocations on the read path (charter §12) | Fixed MPSC ring of capsule cells (`sync/mpmc.zig`) over the arena of #9; admission held in a `[max_segments]` array (≤ `ceil(4 MiB / 1 MiB) = 4`) |
| 11 | The arm runs inside a READ op (`lane.get_or_init(arm_for_device)` on `try_lane_read`, `nvme_dev.rs:2255-2259`) — TCP connects, CSTS polling, ethtool ioctls paid by the first cold read; the arm future is cancellable (finding B) | A multi-second control-plane bring-up on a data op; cancellation hazards the whole `ArmedSteering` drop machinery exists to survive | Arm at volume open on the blocking pool; publish the session through an `ArcSwap`; the read path only loads it |
| 12 | The classic-recv backend (`initiator.rs:1506-1569, 1572-1810`), its `read_into_slice` double copy and `Bytes::from_static` over a dest pointer | Z1 scaffolding the area backend superseded; a second reader with its own copy of the PDU state machine | Deleted; the sim leg (§12) is the SAME driver loop with `RECV` into a granted chunk standing in for `RECV_ZC` |
| 13 | `OnceCell` caches a refusal for the mount lifetime; lazy re-arm after structural teardown "filed" (design `:76-79`) | A transient probe refusal at mount time disables the lane forever | The arm is retried by the volume's health tick (bounded backoff) while the probe verdict is transient (`EAGAIN`-class); terminal verdicts (no RECV_ZC, no HDS) latch |
| 14 | Host identity hashed from pid + boot_id when `/etc/nvme` is absent (`probe.rs:301-315`) | Not the format's uuid-derived identity; `DefaultHasher` is not stable across builds | `hostid = HKDF(set_uuid ‖ mount_id)`-class derivation from the volume set identity; `hostnqn` the uuid form |
| 15 | Errors as `String`s through the driver/steering seams (`Result<_, String>`) and `std::io::Error::other(...)` | Charter §12: typed classes, errno in one table | Error sets partitioned refusal / retryable / terminal; log lines carry the detail |

---

## 12. PORT PLAN

Module layout (Linux-only modules gated in `src/squeezefs.zig` with `builtin.os.tag == .linux`, never
a stub): `src/zcrx/pdu.zig` (codecs, portable), `src/zcrx/pdu_stream.zig` (parser, portable),
`src/zcrx/area_core.zig` + `src/zcrx/area_core_model.zig` (ledger + explorer model, portable),
`src/zcrx/area.zig` (mmap/NUMA/sizing — sizing portable, mapping Linux), `src/zcrx/fill_table.zig`
(portable), `src/zcrx/governor.zig` (RecvGovernor + recv-end classification + CQ sizing, portable),
`src/zcrx/steering.zig` (state machine over a `NicControl` vtable, portable; `ethtool.zig`,
`ethtool_nl.zig` Linux), `src/zcrx/rxq_alloc.zig` + `nic_census.zig` (portable, owned by
`LaneRegistry`), `src/zcrx/initiator.zig` (association; Linux), `src/zcrx/lane_ring.zig` (the
driver; Linux), `src/zcrx/probe.zig` (sysfs with an injectable root; Linux).

### 12.1 Portable / venue-testable

1. **PDU codecs** — `extern struct`s with `comptime` size asserts for CH (8), ICReq/ICResp (128),
   CapsuleCmd (72 + 1024 Connect data as its own 1024 B `extern struct`), CapsuleResp (24),
   C2HData header (24); canonical total decoders; `std.testing.fuzz` "never panic, exactly the
   encoder's form decodes" for ICResp, CapsuleResp, C2HData header. Quirks 2–3 applied.
2. **Stream parser** — the state machine of §5 over `AreaSlice`-shaped spans; tests split the
   stream at EVERY byte (chunk grain 1..N) and assert the identical event sequence; all refusals
   as typed errors; `zcrx_hdr_copy_bytes` ≤ 24 × PDUs.
3. **Area core** — `SpanLedger` as the standalone MODEL (plain value type, per-thread PCs, one
   atomic action per step, an explicit per-thread pending store the `fence` step drains) explored
   with `src/model/explorer.zig`: `expectHolds` recycle-never-races-consumers and exactly-once
   return; `expectFails` for each weakening (`fetch_sub` Relaxed; the 0-crossing acquire fence
   removed; the push CAS Relaxed) — recorded in the rung-8 evidence note.
4. **Sizing** — `area_bytes_per_queue`, `admission_permits`, `burst_geometry`, `delivery_slack_
   bytes`, `mpwqe_stride_bytes`, `ring_standing_bytes`, `rq_entries_for`, `cq_entries_for`,
   `cq_admitted_window_bytes`, `lane_geometry`, `lane_eligible_queues`, `lane_queue_picks`,
   `fair_queue_want`, `reserved_loc_range` (quirk 7), `restricted_rss` as pure functions of
   `derive.Facts` + probed NIC facts; tie tests restating every formula on the canonical shapes
   (§3.4, §7.1 tables; the field numbers 64 MiB / 24 MiB / 128 MiB / 32768 CQ).
5. **Fill table + gather** — `FillTable` keyed by CID with preallocated entries (`depth` cells, no
   map); the exact-length law; `gather ≡ fill` at the whole-read boundary; admission as a
   try-only atomic units word (declines never park); whole-read atomic admission with the
   per-segment split.
6. **Governor** — `RecvGovernor` verbs and both structural arms as pure state over an injected
   clock (the reference's 10 tests ported verbatim); `classifyRecvEnd` and the errno-class
   closure `parks ≡ dry + rq + cq`.
7. **Steering state machine** — over a `NicControl` vtable with a mock NIC carrying the `FailAt`
   lattice; the 20 steering laws ported; `LaneRegistry`-owned live-arm state (quirk 6).
8. **rxq arbiter + census** — the 7 + 3 laws, instance-owned.
9. **Probe verdicts** — `nvmeTcpTargetFor(root, path)` over a fixture sysfs tree (the sentinel,
   the shape refusals, the census over c-paths); `recvZcSupported` via `Ring128.getProbe`
   (`src/io/uring128.zig:455-459`) — on the venue it reads TRUE (`CONFIG_IO_URING_ZCRX=y`); the
   HDS/channels/ntuple probes against `eth0` read `tcp_data_split` absent ⇒ false and
   `channels = 1` ⇒ empty pool — both LOUD refusals exercised end to end without a NIC.
10. **The lane ring without a NIC** — `Ring128` needs an init variant with an explicit
    `cq_entries` + `IORING_SETUP_CLAMP` (today `cq = 2 × entries`, `uring128.zig:184-198`); SQE128
    is harmless for `RECV_ZC`/`SEND`/`READ` (the `zcrx_ifq_idx` word is `Sqe128.splice_fd_in`,
    `uring128.zig:84`). Port the driver module's 25 NIC-free tests: ring flags accepted, CQE32
    round trip, SQ bounded, index wrap, fd-close-with-op-in-flight, refill ring arithmetic over an
    anonymous region, the two CQE gates (+ fuzz), registration argument zeroing, the ifq
    registration REFUSAL against ifindex 0 (reaches `ready` naming the step; poisons), unwound-
    before-go exits clean.
11. **The userspace association against REAL nvmet-tcp over loopback** — the venue has
    `CONFIG_NVME_TARGET_TCP=y`: mount configfs, create a `nvmet` subsystem with a file/loop-backed
    namespace, a tcp port on `127.0.0.1:4420` (the same ladder rung 8's `nvmeof` CLI lands;
    `attr_allow_any_host = 1`); then `connect` the lane with `SQUEEZEFS_TEST_ZCRX_TARGET` and read.
    Correctness leg WITHOUT zcrx: the SAME driver loop with `arm_recv` swapped to a multishot-less
    `IORING_OP_RECV` into a freshly granted chunk (`addr = chunk, len = chunk`), the "CQE offset"
    derived as `(slot, 0, res)` — everything above the opcode (grant, parse, table, gather, refill
    bookkeeping, poison, quiescence) is the product code. Pins: byte-parity with a classical read
    of the backing file, CID custody closes (`free == total`), every chunk returns, `gather ≡
    fill`, mid-stream death poisons and drains, an error status fails the op only.
12. **Funnel dispatch** — the §8 ladder behind `Volumes.read_range` in `data/plane.zig` with a
    test double session: eligibility, the three exclusive counters, loud fallback into the same
    dest, `read_fill_dma_bytes` untouched by lane fills, the dest-lease shape fused.

### 12.2 Hardware-owed (loud probe + `Absent` posture)

`REGISTER_ZCRX_IFQ` against a real multi-queue HDS-capable NIC; RSS/ntuple mutation via
`EthtoolNic`; the genetlink HDS verdict `== ENABLED`; multishot `RECV_ZC` delivery; the park/
failover/structural arms under a live provider pool; the engagement field rows (design §13). In the
venue every one of these resolves to a LOUD refusal at its probe (no second RX queue, no HDS attr,
ifq registration EEXIST/ENODEV) and the mount stays on the kernel path byte-identical — exactly
the stock-graceful stratum; the capability skips are LEDGERED in the testkit (promotable to
failures by the release gate), never silent greens.
