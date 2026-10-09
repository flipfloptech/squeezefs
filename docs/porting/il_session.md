# Porting spec — the IL session: memfd layout, rendezvous, slot/ring/doorbell cores, sizing

Scope: charter §7 (`C:docs/charter.md:244`), the §9 executor law for the hop ("the app↔daemon hop is
shared memory + futex by design", `C:258-268`), §10 observability (`C:272-276`), §12 conventions
(`C:292-300` — the shim is a C-ABI shared library, `C:298`). Reference `R/` =
`/Users/joberdorf/Source/squeezefs`; port `P/` = `/Users/joberdorf/Source/squeezefs-zig`. The reference's
design record is `R/docs/design-preload-interception.md` (rev 22, cited `D:line`). Protocol crate
`R/crates/squeezefs-ipc/src/` (cited by bare file name), daemon host `R/src/ipc_host.rs`, shim
`R/crates/squeezefs-preload/src/{session,interpose,lane_gate,dev_cache}.rs`, loom models
`R/loom-models/src/lib.rs` (cited `L:line`).

Law (decision 0002): the charter's mechanisms, invariants and derivations are requirements; the
reference's incidental choices are not. §9 lists every quirk met here with the better design.

What the port already has: the arena/cap/population/per-uid derivations (`P/src/mem/sizing.zig:16-99`,
knob rows `P/src/core/knobs.zig:114-116`); the reserved-family xattr screen that already refuses
`user.squeezefs.il0` as a stored name (`P/src/fuse/handler/kv_fs.zig:688-696`, test `:3394`); the ONE
futex wrapper (`P/src/sync/futex.zig`) — **which is `private = true` on every call (`:33, :42, :52`) and
therefore cannot serve a cross-process shm word** (`slot_core.rs:285`: "Cross-process futexes on shm must
NOT use `FUTEX_PRIVATE_FLAG`") — a shared face is the first thing this rung adds; the exhaustive explorer
(`P/src/model/explorer.zig:76-136`, `expectHolds`/`expectFails`).

---

## 0. The verdict and the measured numbers

- Hop-ceiling rig (G-L4-1 GO, `R/.benchmarks/2026-07-18-l4-ipc-hop-ceiling.md:23-42, 67-73`): two
  processes over a REAL sealed memfd + SEQPACKET + SCM_RIGHTS; echo RTT p50 **0.79 µs** (t1), 1.32–1.54 µs
  (t4); syscalls/op **0.0001–0.0005 both sides** (strace-corroborated 0.000008/op at a 1 ms spin);
  serve-shaped single service thread **2.13 M ops/s at 98.6 % occupancy**, 2 threads 2.58 M (0-31) /
  **4.18 M (0-15 cage)**; the tokio-handoff adder **+0.46–1.26 µs p50**. Governed 0-15 rows BEAT the
  uncaged 0-31 rows on this shm path (cross-core cacheline transfers on ring/slot/arena lines, `:46`).
- Closing report (`R/.benchmarks/2026-07-19-l4-interception-closing.md:20-21`): il-warm
  **1,017,548 IOPS** median (971,709 / 1,017,548 / 1,020,676; elbencho sync `-t 8` rand-4k direct, hot
  tier, svc threads 8; 97 % sync fast-path serves), il-device-true **622,112** (523,995 / 622,112 /
  642,504; `-t 256`, 100 % handoffs by design). Engagement exact: `ipc_ops_read` Δ = 524,288 = the
  2 GiB dataset's op count (`:13-16`). The v1.1 libaio interposers later measured 633,183 device-true at
  jobs=16 qd=32 (`D:569`).
- The inject-queue finding (`R/.benchmarks/2026-07-26-ipc-handoff-economy.md:6, :18, :27`): spawning a
  handoff from a foreign (service) thread onto tokio's multi-thread runtime landed on the **global
  inject queue**, polled only between local-queue batches — a **~130 µs/op** queueing term the kernel
  lane never paid; fixed by handing off onto the per-core FUSE handler lanes (`D:32` rev 10; charter
  "never a runtime inject queue", `C:244`). Cold il rows moved from 0.82–0.89× to ≥ 1.07× kernel.
- The lever ladder that got there (`closing:29-36`): per-op `Arc` clone of the filesystem was 48.9 % of
  daemon CPU (dt 123 k → 285 k); fd-sharded sessions (285 k → 380 k); the adaptive spin (fixed 4096-spin
  pre-park = client CPU theft, inverse thread scaling); service threads `clamp(cpus/4,2,8)` then
  (`:33`) 2 ⇒ 643 k warm, 8 ⇒ 1.48 M; sync tier serve (warm 308 k → 1.27 M).

---

## 1. The memfd layout, byte-exact (`layout.rs`)

### 1.1 Constants and geometry

| Constant | Value | WHY (`layout.rs`) |
|---|---|---|
| `IPC_ABI` | 6 | coarse structural guard, bumped on any layout change; fine skew = build-commit equality at HELLO (`:39-74`; v2 doorbell took header line 2, v3 AdminHello fields, v4 typed stats page, v5 ingress stamp + 16 B doorbell, v6 mark-valued latch) |
| `IPC_MAGIC` | `u64 LE "SQZIPC01"` | header line 0 identity (`:77`) |
| `PAGE_BYTES` | 4096 | every region starts page-aligned (`:80`) |
| `SLOT_BYTES` | 128 | one slot = one cache-line PAIR (`:83`) |
| `RING_TAIL_LINE_BYTES` | 128 | the producer-contended tail CAS word never shares a line with the cells (`:85-88`) |
| `RING_CELL_BYTES` | 8 | `RingCell{seq u32, value u32}` (`:91`, `ring_core.rs:63-67`) |
| `DEFAULT_RING_ENTRIES` | 1024 | ring AND slot count default (`:94`); WHY 1024: `slots ≤ ring_entries` makes a full-ring refusal unobservable for the honest protocol (`:116-119`), and `slots × 4 KiB LBA = 4 MiB` is the arena DMA alignment grain (`R/src/mem_budget.rs:758-773`) |
| `DEFAULT_ARENA_BYTES` | 64 MiB | the shipped floor (`:95`; `mem_budget.rs:756`) |
| `DEFAULT_MAX_OP_BYTES` | 1 MiB | per-op payload ceiling, daemon-validated (`:96`, `:122-124`) |
| `MAX_ARENA_BYTES` | 1 TiB | arithmetic sanity bound only; admission is the budget (`:98-100`) |
| `OP_READ/OP_WRITE/OP_ECHO` | 1/2/3 | ECHO = the host's liveness ping op kept after the data plane landed (`:102-108`; see §9 Q8) |

`Geometry` (`repr(C)`, 24 B, `:111-126`): `ring_entries u32 @0, slots u32 @4, arena_bytes u64 @8,
max_op_bytes u32 @16, _pad u32 @20`. `validate()` refusals, each a distinct variant for the refusal ledger
(`:163-212`): ring_entries < `MIN_RING_ENTRIES` (2) / not pow2 / > `MAX_RING_ENTRIES` (65536); slots 0 /
> ring_entries; arena 0 / not a 4 KiB multiple / > 1 TiB; max_op 0 / > arena.

**The ONE slab law** `slot_slab()` (`:140-159`): `slab = min(arena/slots, max_op_bytes)`; if `slab ≥ 4096`
floor it to a 4 KiB multiple (so `slot × slab` is page-aligned for EVERY slot whatever the arena — an odd
explicit `SQUEEZEFS_IPC_ARENA_MB` otherwise made half the slots fail the O_DIRECT DMA screen and bounce
through the pooled copy; measured 50.006 % bounce rate, `mem_budget.rs:765-772`); sub-LBA slabs (toy
geometries) pass verbatim and ride the bounce path. Default: 64 MiB / 1024 = **64 KiB slab**. Slot `i`'s
arena window is `[i·slab, (i+1)·slab)` (`session.rs:1100`).

### 1.2 Region arithmetic (`SessionLayout::compute`, `:259-302`)

```
header_off     = 0                      header_bytes = 4096
ring_off       = 4096                   ring_bytes   = 128 + ring_entries × 8          (default 8320)
ring_cells_off = ring_off + 128
slots_off      = page_round(ring_off + ring_bytes)      slots_bytes = slots × 128      (default 131072)
stats_off      = page_round(slots_off + slots_bytes)    stats_bytes = 4096
arena_off      = page_round(stats_off + stats_bytes)    arena_bytes = geometry.arena_bytes
total_bytes    = arena_off + arena_bytes  (== memfd size; checked_add throughout → Overflow)
```
Default geometry: ring 0x1000–0x3080, slots 0x4000–0x24000, stats 0x24000, arena 0x25000, total
0x25000 + 64 MiB. All offsets page-aligned, strictly ordered, arena last (test `:675-697`).

### 1.3 Header page (`SessionHeader`, `repr(C, align(4096))`, `:307-342`; offsets asserted `:660-668`)

| Line | Offset | Field | Width | Writer / reader | Meaning |
|---|---|---|---|---|---|
| 0 | 0 | `magic` | u64 | daemon once | `IPC_MAGIC` |
| 0 | 8 | `abi` | u32 | daemon once | `IPC_ABI` |
| 0 | 12 | `_pad0` | u32 | — | 0 |
| 0 | 16 | `generation` | u64 atomic | daemon (init 1; `fetch_add(1, Release)` = POISON, `ipc_host.rs:2586-2590`) / shim (`Acquire` at map `session.rs:647`, every `poisoned()` check `:766-767`) | a bump ⇒ every fd passthrough |
| 0 | 24 | `heartbeat` | u64 atomic | **nobody** (`grep .heartbeat` → 0 production writers) | §5.7 death detection, unimplemented (§9 Q1) |
| 0 | 32 | `_pad1` | 32 B | — | fills line 0 |
| 1 | 64 | `doorbell` | u32 atomic | shim `fetch_add(1, Release)` per submit (`session.rs:1128`) / daemon snapshot `load(SeqCst)` + `FUTEX_WAIT` word (`ipc_host.rs:3046-3050, 3069-3075`) | submit doorbell |
| 1 | 68 | `daemon_parked` | u32 atomic | daemon `store(1/0, SeqCst)` around the park (`:3043, :3089`) / shim `load(SeqCst)` gates the wake (`session.rs:1129`) | the parked flag |
| 1 | 72 | `doorbell_coalescer` | 1 B (`WakeCoalescer` = one `AtomicBool`, asserted `:637`) | **rig only** (`ipc_hop_rig.rs:545, :853`); production never `arm`/`disarm`s it | dead byte (§9 Q1) |
| 1 | 73 | `_pad2` | 55 B | — | fills line 1 |
| 2 | 128 | `cqe` | `CqeDoorbell` 16 B (`seq @128, parked @132, wake_at @136, wake_paid_mark @140`) | §4 | completion doorbell, own line (`:331-338`) |
| 2 | 144 | `_pad3` | 48 B | — | fills line 2 |
| 3 | 192 | `geometry` | 24 B | daemon once / shim map-time `validate()` + equality with `SessionOk`'s copy (`session.rs:641-646`) | the client's geometry copy |
| | 216–4095 | (struct padding) | | | |

**PERF-18 falsified — do not split the pairs** (`:642-659`): giving `doorbell` (client RMW) and
`daemon_parked` (daemon store) their own lines measured 0.8 % in the field shape (one hot writer, the
other 1-in-1000) and **2.46× WORSE** in the symmetric shape (407 → 1000 µs per 20 k pairs): each side's
per-op READ of the other's word pulls a remotely-dirtied line instead of finding it in the line it
already owns. Co-locate what the hot writer must read every op; the protocol's job is keeping the OTHER
writer sparse. Same for `cqe.seq`/`.parked`/`.wake_at` on one line (`:662-666`).

Trust boundary (`:28-32`): every header word is client-writable for the session's life. The daemon
treats its own fields as write-once OUTPUTS it never re-reads (geometry authority is daemon-private);
`SessionHeader::validate()` is CLIENT-side only (`:364-377`).

### 1.4 The submission ring region (`ring_core.rs`)

`[tail u32 @ring_off | 124 B pad | cells[ring_entries] @ring_cells_off]`, `RingCell{seq: AtomicU32,
value: AtomicU32}` (`:63-67`). Cell `seq` lap encoding (Vyukov, `:58-61`): `seq == index` empty and
claimable at position `index`; `seq == position + 1` published; `seq == position + capacity` freed for
the next lap. A fresh (zeroed) memfd is a BROKEN seeding — every cell but 0 reads "previous lap still
occupied" = permanently full (`:142-160`, test `:323-349`) — so the daemon calls `seed_for_sharing()`
exactly once before the fd is shared (`ipc_host.rs:3253`; the fd pass IS the synchronization point).
Protocol in §5.

### 1.5 The slot array (`IpcSlot`, `repr(C, align(128))`, `layout.rs:507-524`; `SlotCore` `slot_core.rs:95-100`)

| Offset | Field | Width | Writer | Ordering | Notes |
|---|---|---|---|---|---|
| 0 | `core.state` | u32 atomic | both | the protocol word (§3) | low byte = state, bit 8 = WAITER; the per-slot futex word (`slot_core.rs:286-288`) |
| 4 | `core._pad` | u32 | — | | |
| 8 | `core.generation` | u64 atomic | client (`fetch_add` at claim) | Relaxed, ordered by the state edges | ABA guard, monotonic per slot, not dense (`:249`) |
| 16 | `op` | u32 atomic | client | Relaxed | READ/WRITE/ECHO |
| 20 | `flags` | u32 atomic | client | Relaxed | 0 in v1 (`session.rs:1106`) |
| 24 | `binding` | u64 atomic | client | Relaxed | daemon-issued binding id |
| 32 | `offset` | u64 atomic | client | Relaxed | file offset |
| 40 | `len` | u32 atomic | client | Relaxed | `≤ max_op_bytes`, validated |
| 44 | `ingress_stamp_ns` | u32 atomic | client | Relaxed | CLOCK_MONOTONIC low 32 bits at publish (v5); daemon reads ONCE at dequeue, display-only (`:514-521, :548-561`) |
| 48 | `arena_off` | u64 atomic | client | Relaxed | payload window base (`slot × slab`) |
| 56 | `result` | i64 atomic | daemon | Relaxed, published by `complete()`'s Release half (`:588-598`) | bytes or `-errno` |
| 64–127 | padding | 64 B | | | the second line of the pair |

All descriptor fields are ATOMICS so a hostile mid-serve mutation is bounded behavior, never a data race
(`:501-505`). `publish_descriptor` (`:565-572`) then `stamp_ingress` then `core.publish_submitted()`
(the Release); `snapshot_descriptor` (`:577-586`) is the ONE linearization read after
`try_begin_serve()` returned true (§5.3.1 rule 1, `D:264`). `SlotDescriptor` (`:492-499`) is the private
copy `{op, flags, binding, offset, len, arena_off}`.

Ingress delta law (`:600-623`): `None` if stamp == 0; `delta = now_u32 −wrap stamp`; discard
`≥ 1 s` (`INGRESS_PLAUSIBLE_MAX_NS`: a u32-ns stamp wraps every 4.295 s, so a same-host delta at/past
1 s is wrap-ambiguous or hostile — physical bound, not tuning). The clock is `mono_core.rs:26-43`
(`clock_gettime(CLOCK_MONOTONIC)` low 32 bits — the port's `clock.monotonicNs()`).

### 1.6 The client stats page (`ClientStatsPage`, `repr(C, align(4096))`, `:415-445`)

`lane_gate_kernel_routes u64 @0, lane_gate_kernel_bytes u64 @8, lane_gate_threshold_bytes u64 @16,
il_submit_harvested u64 @24, il_park_eras u64 @32, il_slot_reroutes u64 @40, _reserved[4048]`. Zero-
initialized by the fresh memfd. CLIENT-written, **display-only daemon-side**: summed verbatim into the
stats export, never a control input (`:402-407`; `D:504`). The daemon folds a session's page into host
accumulators exactly once at teardown under the registry lock (`ipc_host.rs:2524-2540`).

### 1.7 The arena: alignment and huge pages

- 4 KiB-aligned slabs by construction (`slot_slab` + page-aligned `arena_off`) make every window eligible
  as an O_DIRECT DMA target / io_uring fixed buffer (`D:364`; the port's `aligned pad law`).
- Arena SIZE alignment is **4 MiB** (`mem_budget.rs:758-773` `IPC_ARENA_DMA_ALIGN_BYTES` = `slots (1024)
  × LBA (4 KiB)`; every 4 MiB multiple is also PMD-aligned). WHY not PMD: an odd 2 MiB-multiple arena makes
  the slab `≡ 2048 (mod 4096)` — half the slots fail the DMA screen (the 2026-08-04 −15.4 % randread row).
- THP (`thp.rs`): the memfd is SHMEM, policy-gated by `shmem_enabled` (`advise` on dev, **`never` on the
  Rocky 8 fleet**, `:3-12`). Both sides map through `map_shared_pmd_aligned` (`:108-159`: over-reserve
  `len + 2 MiB` PROT_NONE, `MAP_FIXED` the real mapping at the PMD-aligned offset, trim head/tail with the
  slack math in the KERNEL's page size — a raw `aligned + len` munmap would be refused and leak the tail
  reservation, `:100-107`). Daemon posture `PopulateCollapse` = `MADV_HUGEPAGE` + `MADV_POPULATE_WRITE` +
  `MADV_COLLAPSE` (= 25; **collapse works regardless of the sysfs policy**, `:20-24`), DEFERRED to a
  `sqz-ipc-thp` prep worker after `SessionOk` (`ipc_host.rs:2412-2418, :3224-3230`: running it inline
  put O(fleet × arena) of memory work in front of every client's first op); shim posture `Advise` only
  (`session.rs:616-636`). Knob `SQUEEZEFS_IPC_ARENA_THP` (default on) disables both halves.
- NUMA (`numa_core.rs`): the session's node is inferred from the peer's last-run CPU
  (`ipc_host.rs:2311-2320`; siblings of one pid rotate across exec nodes), the owner is picked
  **balance-first** `(load, distance, index)` (`numa_core.rs:437-455`; the retired locality-first order
  confined a fork-clustered fleet to one node's owner subset = the ~530 k ceiling, `:425-436`), and the
  arena is `mbind(MPOL_PREFERRED)` to the CHOSEN owner's node BEFORE first touch (`:323-349`;
  `ipc_host.rs:3213-3223` — memory follows thread). Owner-index → node partition is a largest-remainder
  weighted round-robin by CPU share (`:286-316`). Single-node maps are a structural no-op (`:16-18`).

### 1.8 Proposed v4 `extern struct`s (the port)

Every struct `extern`, little-endian, `comptime` size/offset asserts, zero-initialized valid
(charter §12 `C:294`). Atomics as `std.atomic.Value(u32/u64/i64)`.

```zig
pub const IPC_MAGIC: u64 = std.mem.readInt(u64, "SQZIPC01", .little);
pub const IPC_ABI: u32 = 1;                     // the PORT's own series (no wire compat owed, 0002)
pub const Geometry = extern struct { ring_entries: u32, slots: u32, arena_bytes: u64, max_op_bytes: u32, _pad: u32 }; // 24
pub const CqeDoorbell = extern struct { seq: Atomic(u32), parked: Atomic(u32), wake_at: Atomic(u32), wake_paid_mark: Atomic(u32) }; // 16
pub const SessionHeader = extern struct {
    magic: u64, abi: u32, _pad0: u32, generation: Atomic(u64), heartbeat: Atomic(u64), _pad1: [32]u8,   // line 0
    doorbell: Atomic(u32), daemon_parked: Atomic(u32), _pad2: [56]u8,                                  // line 1 (no coalescer byte, §9 Q1)
    cqe: CqeDoorbell, _pad3: [48]u8,                                                                   // line 2
    geometry: Geometry, _pad4: [4096 - 216]u8,
};
pub const SlotCore = extern struct { state: Atomic(u32), _pad: u32, generation: Atomic(u64) };           // 16
pub const IpcSlot = extern struct {
    core: SlotCore, op: Atomic(u32), flags: Atomic(u32), binding: Atomic(u64), offset: Atomic(u64),
    len: Atomic(u32), ingress_stamp_ns: Atomic(u32), arena_off: Atomic(u64), result: Atomic(i64), _pad: [64]u8,
};                                                                                                     // 128, align 128
pub const RingCell = extern struct { seq: Atomic(u32), value: Atomic(u32) };                            // 8
pub const ClientStatsPage = extern struct { lane_gate_kernel_routes: Atomic(u64), lane_gate_kernel_bytes: Atomic(u64),
    lane_gate_threshold_bytes: Atomic(u64), il_submit_harvested: Atomic(u64), il_park_eras: Atomic(u64),
    il_slot_reroutes: Atomic(u64), _reserved: [4096 - 48]u8 };                                        // 4096
comptime { assert(@sizeOf(SessionHeader) == 4096 and @offsetOf(SessionHeader, "doorbell") == 64
    and @offsetOf(SessionHeader, "cqe") == 128 and @offsetOf(SessionHeader, "geometry") == 192
    and @sizeOf(IpcSlot) == 128 and @alignOf(IpcSlot) == 128 and @sizeOf(ClientStatsPage) == 4096); }
```
`SessionLayout.compute(geometry) !SessionLayout` is a pure function with the §1.2 arithmetic (error set
= the 11 geometry refusals); the daemon casts the mapping into these types at the computed offsets and
bounds-checks every index/window against `geometry` (`ipc_host.rs:867-889`: `slot(index)` refuses
`≥ slots`; `arena_window(off, len)` refuses `off + len > arena_bytes` via `checked_add`).

---

## 2. The rendezvous (`D:156-215`; `wire.rs`; `ipc_host.rs`; `session.rs`)

### 2.1 Classification on `open` (shim)

One `fstatfs`/`fstat` per successful `open*` (`D:303`: `f_type == FUSE_SUPER_MAGIC → st_dev`); a
process-wide **negative `st_dev` cache** (`dev_cache.rs`: 1024 slots × 8 B, 4-way linear probe, splitmix64
home, slots store `dev + 1` so `st_dev == 0` is representable, plain-overwrite eviction = one
re-classification, never a correctness signal, `:14-20, :44-79`) makes every later open on a foreign
filesystem one probe + the real syscall (~100–200 ns). A positive hit proceeds to the once-per-(process,
mount) xattr bootstrap. The mount root's `st_uid` from THIS `fstat` is the trust anchor of §2.8 (never
re-stat'ed, `session.rs:445-450`).

### 2.2 The `user.squeezefs.il0` blob (`wire.rs:200-255`)

Synthesized by GETXATTR ONLY when the mount's session host is armed (`-o interception`); a disabled mount
answers **ENODATA** (the cheap negative probe) (`R/src/fuse_client.rs:30699-30717`). `size == 0` ⇒ the
blob length; `size < len` ⇒ ERANGE. Never on-disk bytes. SETXATTR of any `user.squeezefs.*` name ⇒
**EPERM** via the reserved-family screen (`:30663-30667`; the port already refuses it,
`P/src/fuse/handler/kv_fs.zig:694`, `:2489`); the family is filtered from `listxattr` (`:30728-30731`).
The shim reads it with `fgetxattr(fd, "user.squeezefs.il0")` on the just-opened fd — it rides the
already-authorized kernel path (read access ⇒ may read it, `D:164`).

Byte layout (LE, fixed, 312 B):

| Off | Width | Field | Value |
|---|---|---|---|
| 0 | 8 | `BLOB_MAGIC` | `u64 LE "SQZIL0\0\0"` (`:205`) |
| 8 | 4 | `abi` | `IPC_ABI` |
| 12 | 4 | `flags` | reserved feature bits, 0 (`:212-213`; `ipc_host.rs:1778`) |
| 16 | 48 | `build_commit` | NUL-padded, always NUL-terminated inside the width (`BUILD_COMMIT_LEN` 48: the design's `[u8;41]` could not hold the 46-char `-dirty` form the skew gate must SEE, `:28-32`) |
| 64 | 108 | `socket` | abstract AF_UNIX name WITHOUT the leading NUL (`SOCKET_NAME_LEN` 108 ≥ 107 + NUL) — `sqz-il0-<pid>-<rand u32 hex>` (`fuse_client.rs:31698`) |
| 172 | 108 | `socket_path` | the filesystem-path socket, `<dir>/<socket>.sock`; empty when disabled/degraded (`ipc_host.rs:1781-1787`) |
| 280 | 32 | `nonce` | the current anti-replay nonce (`:1788`) |

Decode is total: `Truncated` on short input, `BadString` on a bad magic or a field with no NUL inside its
width / non-UTF-8 (`:240-255`; test `:493-511`).

### 2.3 Sockets

- Abstract `SOCK_SEQPACKET` listener (zero residue, no permission bits; per network namespace, `D:161`).
- Path listener (v1.1, OQ-6, `D:36`): dir = `SQUEEZEFS_IPC_SOCKET_DIR` (`none` disables) else
  `/run/squeezefs` (euid 0) else `$XDG_RUNTIME_DIR/squeezefs` else `/tmp/squeezefs-il-<uid>`
  (`fuse_client.rs:31678-31696`). `path_listen` (`ipc_host.rs:3355-3430`): create 0755; open
  `O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC`; `fstat` the DIRFD: `st_uid == geteuid()` ∧ `mode & 0o022 == 0`
  else **refuse loudly and degrade to abstract-only** (never fail the mount); unlink/bind/chmod through
  `/proc/self/fd/<dirfd>/<name>` (AF_UNIX has no `bindat`; a swapped directory cannot redirect an open
  fd). The socket file is **0666 because connecting is not a credential** (`:3371-3373`). Stale same-name
  residue is OUR corpse (names embed pid + random) and is replaced; shutdown unlinks.
- `listen(2)` backlog = `clamp(ctl_conn_cap, 64, 4096)` (`:271-286`: the hardcoded 64 sat at ¼ of a
  256-process fleet and a full SEQPACKET backlog makes `connect` BLOCK — part of the measured 150–190 ms
  per-process launch convoy); ctl connection cap = `clamp(2 × arena_cap/footprint, 32, 4096)` (`:250-269`;
  rail 4096 because each ctl connection is an OS thread in the reference — §9 Q18).
- The shim's connect ladder: abstract first, then the path rung (only after §2.8 rung 1 passes);
  `SO_RCVTIMEO` 10 s on the ctl socket (`session.rs:95, :520-529`).

### 2.4 Ctl frames (`wire.rs:261-478`) — one SEQPACKET datagram each, `u32 LE tag` first

| Tag | Frame | Direction | Body (LE) | fd via `SCM_RIGHTS` |
|---|---|---|---|---|
| 1 | `HELLO` | shim → daemon | `abi u32, pid u32, uid u32, build_commit [48], nonce [32]` (92 B body, 96 B frame) | exactly ONE: a dup of the credential fd |
| 2 | `SESSION_OK` | daemon → shim | `ring_entries u32, slots u32, arena_bytes u64, max_op_bytes u32` (20 B body; `_pad` not on wire) | the sealed memfd |
| 3 | `REFUSE` | daemon → shim | `class u32` | — |
| 4 | `BIND` | shim → daemon | (tag only) | exactly ONE: the fd to bind |
| 5 | `BIND_OK` | daemon → shim | `binding_id u64, ino u64, read_ok u32, write_ok u32` | — |
| 6 | `BIND_REFUSED` | daemon → shim | `class u32` | — |
| 7 | `UNBIND` | shim → daemon | `binding_id u64` (last close of the refcounted binding) | — |
| 8–11 | `ADMIN_HELLO / ADMIN_OK / ADMIN_REQ / ADMIN_REPLY` | the CLI's admin lane (VL2) — `ADMIN_HELLO` = HELLO's body with no fd; `ADMIN_REQ{verb var-str ≤ 64, arg var-str ≤ 60 KiB}`; `ADMIN_REPLY{ok u32, body var-str}` (`:277-281, :318-342`) | — |

`CTL_MSG_MAX` = 64 KiB (admin JSON bodies; data-plane frames are tiny, `:273-276`). `RefuseClass`
(`:70-98`, `repr(u32)`): `Version=1, Nonce=2, Flags=3, Mode=4 (st_dev mismatch — misnamed, §9 Q5),
Budget=5, Peercred=6, Internal=7, Disabled=8`; `from_u32` refuses anything else (`BadClass`). Decoders
are total on garbage (`:551-565`). `build_commit_degenerate(c)` = `c == "unknown" ∨ c.ends_with("-dirty")
∨ c.is_empty()` (`:484-486`).

### 2.5 `SCM_RIGHTS` discipline (VAL-5, `D:215`; `ipc_host.rs:3553-3630`)

`recv_ctl` derives the fd count from `cmsg_len`, takes ownership of EVERY received descriptor
immediately, and refuses on `MSG_CTRUNC` or count > 1 (tests `ipc_host_tests.rs:1695, :1740, :1780`);
the shim's receive mirrors the bound (`preload_authn_tests.rs:517` "a session reply carrying extra
descriptors" is refused and no fd leaks, `:672`). The daemon closes its dup of a credential fd right
after the screen (`:2265`, `:2020-2022`): it needs `(st_dev, st_ino, flags, mode)`, never a live handle.
The daemon's memfd handle closes after `SessionOk` is sent (`:2443-2446`) — the mapping keeps the memory;
zero persistent residue by construction (`D:398`).

### 2.6 The daemon's HELLO ladder (`handle_hello_msg`, `ipc_host.rs:2175-2448`) — every refusal

In order; each refusal is counted (`count_refusal`, `:3137`), logged once per class, answered `REFUSE`:

1. First datagram not HELLO ⇒ drop the connection (`:2193-2198`); an ADMIN frame before ADMIN_HELLO ⇒
   `Refuse{Flags}` (`:1960-1970`).
2. `!cfg.data_plane` (mount without `-o interception`) ⇒ **`Disabled`** before any fd screen
   (`:2207-2216`; the shim prints "interception not armed on this mount (mount with --interception)",
   `session.rs:299-301`).
3. `abi != IPC_ABI ∨ build_commit != ours` ⇒ **`Version`** (`:2220-2222`).
4. `degenerate(theirs) ∨ degenerate(ours)` ⇒ `Version` unless `allow_dev`, which admits and counts
   `ipc_binds_dev_override` + warns (`:2223-2236`; the lever announces itself ONCE per process at
   engagement, `:124-151`).
5. Nonce not current-or-previous ⇒ **`Nonce`** (`:2239-2246`; §2.9).
6. `SO_PEERCRED` unreadable ⇒ `Internal`; kernel `(pid, uid) != claimed` ⇒ **`Peercred`** (`:2251-2256`;
   accounting integrity — the fd stays the authorizer).
7. No fd attached ⇒ `Flags`; fd screen (§2.7) refusal class.
8. Budget: `shed_target` set ∧ `arena_bytes > shed` ⇒ `Budget` (+`ipc_admission_refusals`); the
   one-shot demand clears once met (`:2272-2283`); `arena_bytes + footprint > arena_cap_bytes` ⇒
   `Budget`; `uid_sessions[uid] ≥ per_uid_session_cap` ⇒ `Budget` (`:2284-2301`). `footprint =
   layout.total_bytes` (header+ring+slots+stats+arena, `:2269-2271`).
9. Owner pick (`reserve_owner`, §1.7) + `ensure_service_threads(owner)` — **spawn-on-bind**: a session
   pinned to a never-spawned owner would strand forever (`:2342-2349`); spawn failure ⇒ `Internal`.
10. `create_session_shm` failure ⇒ `Internal` (`:2361-2373`).
11. Register: `arena_bytes += footprint`, `ipc_arena_bytes`, `ipc_sessions_{active,total}`, registry
    insert, THEN `session_epoch.fetch_add(Release)` (a service thread seeing the epoch always finds the
    session), `uid_sessions += 1` (`:2392-2410`); enqueue the THP prep (O(1)) and record
    `ipc_session_admission_ns` BEFORE the `SessionOk` send (anything a client may observe after
    `SessionOk` is visible before it, `:2412-2429`); send failure ⇒ teardown.

The ADMIN lane runs the SAME version → nonce → peercred ladder, then `cred.uid ∈ {0, owner_uid}`
(`admin_loop`, `:2082-2128`; VAL-7c: the lane mutates durable state). `owner_uid` is explicit
(`-o admin_uid=N`) because the fallback derives from `SUDO_UID` — caller-controlled environment
(`fuse_client.rs:31721-31730`).

### 2.7 The daemon fd screen — THE security boundary (`screen_fd`, `ipc_host.rs:2452-2505`; `D:186-196`)

Run on every received fd (HELLO credential and every BIND), all daemon-side:

1. `F_GETFL` fails ⇒ `Flags`. **`O_PATH` set ⇒ `Flags`** — load-bearing: an `O_PATH` fd is obtainable
   with search-only permission and its access-mode bits read `O_RDONLY` (0); a naive mode check would grant
   ring reads of a file the uid cannot read (cross-user disclosure on `--allow-other`) (`:2458-2462`).
2. `fstat` fails ⇒ `Flags`; `st_mode & S_IFMT != S_IFREG` ⇒ `Flags` (directories, devices, FIFOs,
   sockets) (`:2469-2471`).
3. `expected_st_dev == u64::MAX` (mount device unresolved) ∨ `st_dev != expected` ⇒ `Mode` (`:2475-2477`).
4. `O_APPEND` ∨ `O_SYNC` ∨ `O_DSYNC` ∨ `O_TMPFILE` ∨ `st_nlink == 0` ⇒ `Flags` (`:2485-2491`; append
   needs an atomic size authority round trip, sync needs per-op barriers the kernel path already provides;
   `st_nlink == 0` also refuses open-then-unlinked fds — conservatively correct, passthrough serves them).
5. Access mode ⇒ rights BOTH directions: `O_RDONLY → (r, !w)`, `O_WRONLY → (!r, w)`, `O_RDWR → (r, w)`,
   anything else ⇒ `Flags` (`:2494-2499`). Per op a wrong-direction READ/WRITE completes **`-EBADF`**
   and counts `ipc_descriptor_rejects` (`:1100-1117`), matching the kernel's wrong-direction errno.
6. Returns `(st_ino, read_ok, write_ok, odirect)`; `O_DIRECT` is carried into every ring op like
   `fuse_read_in.flags` (`:2500-2504`).

Why forgery fails (`D:200-206`): to bind ino X you must present a description whose `fstat` shows
`(st_dev == mount, st_ino == X)` AND survives the screen — only a permission-checked open (or POSIX fd
passing from one) produces that; the daemon reads the DESCRIPTION's flags, so a client cannot claim
`O_RDWR` on an `O_RDONLY` description; `SO_PEERCRED` is for accounting/rate caps/logs, never the authorizer.
BIND handling (`:1987-2026`): mint `binding_id` (monotonic u64 from 1), insert `BindingRights{ino,
read_ok, write_ok, odirect, kill_priv}` in the session's binding map, `ipc_binds += 1`, refresh the idle
clock, `sink.on_bind(ino)` (the W1 invalidation hook), reply `BIND_OK`. A BIND with no fd ⇒ **poison**
(`:2027-2031`); any other frame on an established session ⇒ poison; a malformed datagram ⇒ poison
(`:2049-2057`); EOF ⇒ ordinary teardown ("ctl socket EOF").

### 2.8 The shim authenticates the daemon (VAL-4, `D:206-212`; `session.rs:488-587`)

The blob — socket name included — is ANSWERED BY THE MOUNT, so whatever filesystem answers dictates
where the shim connects and then hands over a real fd and maps whatever memfd comes back. Three rungs,
the first two BEFORE the credential fd leaves the process:

1. **Path-rung screen** (`path_rung_trusted`, `:1737-1778`): the advertised `socket_path`'s PARENT
   directory, opened `O_PATH|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC`, must have `st_uid ∈ {0, mount_uid}` and
   `mode & 0o022 == 0`; else `UntrustedRendezvous` — no `connect` happens. The abstract rung needs no
   screen (no residue, no bits).
2. **`getsockopt(SO_PEERCRED)`** after connect, BEFORE HELLO: `peer_uid ∈ {0, mount_uid}` else
   `PeerUntrusted{peer_uid, mount_uid}` — no HELLO, no `SCM_RIGHTS` (`:530-554`). Deliberately NO "or our
   own uid" disjunct: the conservative predicate only costs passthrough on a mount served by a third-party
   service account (`:540-547`).
3. **`fcntl(memfd, F_GET_SEALS)`** on the received session fd: `F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW`
   all present else `Unsealed{seals}` (`:1721-1729`, `:578-587`): without SHRINK the peer can `ftruncate`
   under the mapping and every later slot/arena access is an in-app SIGBUS — not POSIX-legal, not
   recoverable by the ladder.

Then: map (PMD-aligned, plain fallback), close the memfd (the mapping holds the memory), validate the
header (`magic`, `abi`, `geometry.validate()`) and `header.geometry == SessionOk.geometry` else
`Protocol` + munmap (`:640-646`); snapshot `my_generation` (Acquire); `slab = geometry.slot_slab()` must
be nonzero; resolve the lane-gate threshold (§6.6) and publish it to the stats page (`:659-686`).

Client-side pre-check before connecting (`:495-504`): `blob.abi != IPC_ABI ∨ blob.build_commit !=
mine ∨ (degenerate(either) ∧ !allow_dev)` ⇒ `VersionSkew` — skip the doomed round trip; the daemon
enforces the same law authoritatively. Each `SessionError` carries a stable `reason_code` (`:229-243`)
and a reason line naming BOTH commits / the remedy (`describe`, `:249-289`), printed ONCE per
(mount `st_dev`, reason) per process (`D:35` rev 8).

### 2.9 Nonce lifecycle (`ipc_host.rs:82-84, :1142-1181`; `D:164`)

One random 32-byte nonce per daemon instance (`rand::thread_rng().fill_bytes`), `NONCE_TTL` = 60 s,
rotated LAZILY (`rotate_if_stale` on every blob synthesis and every HELLO under a mutex — control-plane
cadence), **multi-use within its TTL** (concurrent handshakes need no serialization), accepted =
`current ∨ previous` (the rotation race). The nonce is anti-replay for stale traces only; fd possession
is the sole authorizing credential. Freshness window therefore: a blob read at `t` is valid for a HELLO
until the SECOND rotation after `t`, i.e. 60–120 s.

### 2.10 Session lifecycle (`D:394-405`; `ipc_host.rs`)

| Event | Detection | Behavior |
|---|---|---|
| Client exit/crash | SEQPACKET EOF on the ctl thread — reliable even with forked descendants because the atfork child `close(2)`s its inherited socket (`D:314`) | `teardown_session` (`:2511-2573`): `torn_down.swap`, clear bindings, `shutdown(Both)`, registry remove + stats-page fold under ONE lock hold, `arena_bytes −= footprint`, owner release, `uid_sessions −= 1`, epoch bump; the mapping is an `Arc` every in-flight `SlotCompletion`/`ArenaWindow` clones, so the munmap happens when the LAST accessor drops — rule 4's lease-refcounted unmap (`:56-61`, `:1046`) |
| Idle client | `last_active_ms` refreshed by served ops and BINDs (`:940-942`); reaper tears down past `SQUEEZEFS_IPC_IDLE_SECS` (300 s; 0 disables) | generation bump; client lazily re-binds (`ipc_sessions_reaped`, never the poison tripwire, `:1529-1531`) |
| Protocol violation (impossible ring/slot observation, over-admission, bad ctl frame) | the drain / ctl loop | `poison_session` (`:2578-2592`): `ipc_sessions_poisoned += 1`, `log::error!`, `generation.fetch_add(1, Release)`, teardown |
| Daemon death (shim side) | per-op deadline `SQUEEZEFS_IL_OP_TIMEOUT_MS` (30 s) in 50 ms futex slices (`session.rs:42, :1340-1357`) ⇒ `poison()`; `poisoned()` also reads `generation != my_generation` (`:762-768`) | every fd passthrough; the op completes via the real call (`Abandoned ⇒ Fallthrough`, `:1301-1309`). The design's "1 s escalation → socket probe → heartbeat" is NOT shipped (§9 Q1) |
| Abandoned slot (deadline) | — | NEVER released: the daemon may complete it arbitrarily late, recycling would hand a stale completion to a future op; GC'd with the session (`:1318-1321`, `D:254`, `D:404`) |
| fork | `pthread_atfork` child: flag-poison + `close(2)` the socket (AS-safe); a poisoned session never emits ctl traffic (`unbind` returns early, `session.rs:724-729`) | §7 bail-out ladder (`D:314`) |

Budget component `ipc_session_arenas` (`mem_budget.rs:898-904`): floor 0, non-reclaimable, shed =
refuse-new until under target + reap idle — never tearing live sessions (`ipc_host_tests.rs:899`).

---

## 3. The slot state machine (`slot_core.rs`) and its model

States (low byte): `FREE 0 → CLAIMED 1 → SUBMITTED 2 → SERVING 3 → DONE 4 → FREE`; `WAITER = 1 << 8`
(`:58-68`); `state_bits(w) = w & 0xff` (`:74-76`).

| Transition | Who | Primitive + ordering | Pairs with / WHY (`slot_core.rs`) |
|---|---|---|---|
| FREE → CLAIMED | client | `CAS(FREE → CLAIMED, Acquire, Relaxed)`; then `generation.fetch_add(1, Relaxed) + 1` returned | Acquire pairs with `release`'s Release store: the new claimant's descriptor writes are ordered after the previous consumer's result read (`:124-141`). The bump is Relaxed: sequenced before `publish_submitted`'s Release, and every DONE observer Acquire-loads a word in that release sequence ⇒ whoever sees this life's DONE sees this generation |
| CLAIMED → SUBMITTED | client | `store(SUBMITTED, Release)` (plain store is exact: WAITER cannot be set on an unsubmitted slot) | pairs with `try_begin_serve`'s Acquire ⇒ the daemon's snapshot sees every descriptor write (`:146-155`) |
| SUBMITTED → SERVING | daemon | CAS loop `cur → SERVING | (cur & WAITER)` Acquire; `false` if `state_bits(cur) != SUBMITTED` | PRESERVES WAITER (a client may be parked already); success is the Acquire edge licensing the ONE descriptor snapshot (`:163-183`). `false` = hostile/duplicate ring entry ⇒ caller poisons, never panics |
| SERVING → DONE | daemon | `set_result` then `swap(DONE, AcqRel)`; returns `prior & WAITER != 0` ⇒ `FUTEX_WAKE(state, INT_MAX)` | Release half publishes the result (and the generation via the release sequence headed at `publish_submitted`); Acquire half orders the WAITER decision; the swap CLEARS WAITER — a woken client re-checks via `is_done_for`, never via the bit (`:190-201`). Wake breadth `i32::MAX`: a split submitter/reaper pair may both park on one word (`ipc_host.rs:425-433`) |
| DONE → FREE | client | `store(FREE, Release)` after consuming the result | pairs with the next `try_claim`'s Acquire; clears a late parker's WAITER with the rest of the word (`:261-270`) |
| CLAIMED → FREE | client | `store(FREE, Release)` (`release_claimed`) | the multi-slab **arena-extension hold**: a large op claims a contiguous non-wrapping run, submits ONLY the base slot, the extension slots reserve arena the daemon never observes (`:240-256`; `session.rs:1406-1462` — aligned-stride pass then greedy; fragmentation shortens the run, never refuses) |
| park entry | client | `fetch_or(WAITER, AcqRel)`: `state_bits(prior) == DONE ⇒ Ready` else `Park{expected: prior | WAITER}` | ONE RMW does publish-then-recheck: it lands before `complete`'s swap (daemon reads the bit, wakes) or after it (prior reads DONE ⇒ Ready); there is no third interleaving (`:223-238`) |
| poll | anyone | `is_done_for(gen)`: `state_bits(load(Acquire)) == DONE` FIRST, then `generation.load(Relaxed) == gen` | state Acquire synchronizes with `complete`'s Release half; once this life's DONE is visible so is its generation bump ⇒ a recycled slot's DONE is never attributed to an old generation (`:207-218`) |

Client wait (`session.rs:1322-1359`): adaptive spin on `is_done_for` — `WAIT_SPINS_DEFAULT` 4096
iterations, or `WAIT_SPINS_PARKY` 256 when the session's LAST op parked (a parked op means daemon-side
async work; the full window is CPU theft — the 2026-07-19 device-true sweep measured 336 k → 605 k IOPS
at 64 threads from exactly this, `:44-59`); then `park_prepare` → `FUTEX_WAIT(state, expected,
min(deadline − now, 50 ms))` loop → `is_done_for` → `poisoned()`; deadline ⇒ `poison()`, `Abandoned`.
`SQUEEZEFS_IL_SPINS` pins both (A/B lever). `consume` (`:1361-1382`): `r < 0` ⇒ release the slot;
`-EINVAL` (a descriptor reject = OUR bookkeeping diverged) ⇒ `Fallthrough` to the real call; other errno
⇒ `Errno(e)` (kernel-identical, e.g. `EBADF`).

Daemon drain (`ipc_host.rs:963-1057`), per popped index: `slot(index)` out of range ⇒ **poison**;
`!try_begin_serve()` ⇒ **poison** ("ring published slot in a non-SUBMITTED state"); the VAL-5c in-flight
ledger `begin()` — `live > slots` ⇒ **poison** (the honest protocol cannot exceed one ring entry per
CLAIMED slot; a higher count proves a re-published slot, `:987-1003`; ledger `finish()` runs FIRST in
`complete` so a back-to-back resubmit never looks like over-admission, `:412-419`; a dropped
`SlotCompletion` releases the ledger in `Drop`, `:473-482`); `snapshot_descriptor()` once; ONE clock read
(`t0_ns`) serves the ingress delta AND the op's residence base (`:1005-1014`); `serve_validated`
(`:1064-1139`): op ∉ {ECHO, READ, WRITE} ⇒ `-EINVAL` (+`ipc_descriptor_rejects`); dead binding id ⇒
`-EINVAL`; `len > max_op_bytes` ⇒ `-EINVAL`; `arena_window(arena_off, len)` out of bounds ⇒ `-EINVAL`;
wrong direction ⇒ `-EBADF`; ECHO = ONE severed arena read, byte-sum + inverted write-back from the COPY
(rule 2, `:1118-1128`); READ/WRITE ⇒ `sink.serve_data(DataOp, completion)`. The drain budget per session
per pass is `geometry.slots` and the sweep start rotates (VAL-5e fairness: one saturating session cannot
own its thread, `:954-962`, `:2866`).

**Loom models** (`L:2804-3153`), each asserting on the SHIPPED core:
- `ipc_slot_completion_exactly_once_parked_waiter_never_stranded` (`L:2818`): weakening —
  `complete`'s swap AcqRel→Relaxed FAILS (a Ready/woken consumer reads result 0: the result write not
  published with its DONE, `L:2813-2816`).
- `ipc_slot_claimed_hold_release_reuse_clean` (`L:2895`): a CLAIMED hold is daemon-invisible; its
  generation never consumes a later life's DONE.
- `ipc_slot_free_reuse_never_observes_stale_done` (`L:2936`): the gen-1 probe reads false forever once
  gen 1 was consumed, racing life 2.
- `ipc_slot_snapshot_single_read_never_pre_submit_hostile_bounded` (`L:2992`): with a hostile sibling
  mutating the descriptor, the one post-`begin_serve` snapshot reads the submitted or the hostile value —
  NEVER the pre-submit seed — and the served op equals the snapshot; weakening — `publish_submitted`
  Release→Relaxed FAILS (snapshot reads the seed 0). Model-fidelity note: the server must be spawned
  BEFORE claim/submit or the spawn smuggles the happens-before edge and the weakening passes
  (`L:2997-3002`).
- `ipc_slot_multi_park_admission_never_strands` (`L:3087`): two slots, the reaper's `park_prepare` on
  both racing A's completion — parked ∧ no wake ∧ admission value current is unrepresentable; B's
  admission value holds.

**Port model** (`P/src/model/explorer.zig`, SC with explicit step per atomic): threads {server: begin_serve,
result store, DONE swap, wake} × {client: spin-check, fetch_or WAITER, admit(expected == word), consume};
invariant: never (parked ∧ word == expected ∧ no wake pending ∧ DONE); final: exactly one consume.
Weakening to prove red: split the DONE swap so the result store follows it (the Relaxed-swap shape).
Second model: two lives on one slot with a stale gen-1 probe thread; weakening: generation bump AFTER
`publish_submitted` (moves the bump out of the release sequence) must fail.

---

## 4. The completion doorbell (`cqe_core.rs`) — exactly

### 4.1 Words and orderings

`CqeDoorbell{seq, parked, wake_at, wake_paid_mark}` (4 × u32 atomic, 16 B, header line 2, `:129-161`).
Every access is **`SeqCst`**, with an explicit **`fence(SeqCst)`** on each side of the Dekker pair
(`:229-237`, `:300-303`) — load-bearing: "loom's SeqCst-atomics modeling is weaker than the C++ total
order, and the `ipc_cqe_*` models fail without it". `reached(seq, at) = seq −wrap at < 2³¹` (`:165-167`).
`NO_PAY_RECORDED = u32::MAX` initial latch value (`:169-178`): `wake_at` initializes to 0 and the first
mark is `snapshot + k ≥ 1`, so a completer reading the mark word before the first store (stale-init read,
reads-as-reached) must find a record that is NOT that stale value — it over-pays one wake (benign)
instead of collapsing against a pay nobody made.

**Completer** (`complete(latch) → {Elided, Collapsed, Wake}`, `:228-276`), called by ANY completing
thread (service fast path, handler lane, direct-drive reaper) AFTER the slot's DONE publish:
```
seq' = seq.fetch_add(1, SeqCst) + 1
fence(SeqCst)                                   // the Dekker fence
if parked.load(SeqCst) == 0            → Elided
at = wake_at.load(SeqCst)
if !reached(seq', at)                  → Elided  // below the batch mark; a stale behind-the-seq mark reads as reached: over-wake, never a strand
if !latch                              → Wake    // the A/B control arm: wake on every mark-passed completion
wake_paid_mark.fetch_update(SeqCst, |paid| paid == at ? None : Some(at))
   Ok  → Wake        // this era's ONE syscall: FUTEX_WAKE(seq_word, i32::MAX)
   Err → Collapsed   // a wake for this mark is already paid/in flight
```
**Parker** (`park_begin_batch(k) → snapshot`, `:298-349`; `park_begin() ≡ park_begin_batch(1)`):
```
k = max(k, 1)
prior = parked.fetch_add(1, SeqCst)             // REGISTER FIRST
fence(SeqCst)                                   // the mirrored Dekker fence
snap   = seq.load(SeqCst)                       // THEN SNAPSHOT
target = snap + k
if prior == 0: wake_at.store(target, SeqCst)    // first parker of the era owns the mark (a racing second parker's earlier min may be overwritten: its wake is DELAYED to our mark or its own bounded timeout, never lost)
else: wake_at.fetch_update(|cur|): ahead = cur − snap
        ahead == 0  → if wake_paid_mark == cur then Some(target) else None   // an AT-snapshot mark: SPENT (its pay recorded) ⇒ replace; UNPAID ⇒ keep (its completer may still be in flight before its mark read)
        ahead ≤ target − snap → None             // a live earlier mark governs
        else → Some(target)                       // stale/later mark replaced
return snap
then the caller MUST re-scan its pending set, then FUTEX_WAIT(seq_word, snap, bound); then park_end(): parked.fetch_sub(1, SeqCst)
```
No parker-side latch write exists (the v3 `wake_paid.store(0)` was retired 2026-09-06, `:304-309`).

### 4.2 Why no wake is lost (`:25-41`) and the collapse latch (`:43-100`)

A strand needs the reaper to (a) miss the DONE in its post-registration scan, (b) pass the futex
admission (seq unchanged), while the daemon (c) read `parked == 0`. (a) requires the scan to precede the
DONE publish; the scan follows `park_begin`'s RMW, so the daemon's later `parked.load` — after its own
DONE publish AND seq bump — observes the registration: (c) impossible. If instead the bump precedes the
snapshot, the DONE (sequenced before the bump) is visible to the scan: (a) impossible. A bump between
snapshot and wait fails the admission: (b) impossible.

Batch mark (reap-fanin 2026-08-08): admission (`seq == snapshot`) certifies ZERO completions since the
snapshot, so all `k` marks lie in the future and each below-mark completion is survivable; the k-th pays.
Liveness is the CALLER's half: `k ≤ its own pending population on this session` (only its own ops are
guaranteed to produce completions) and every wait deadline-bounded (`:293-298`). Measured herd it
replaced: 17.8 M wakes / 22.2 M ops, +43 µs daemon inflight at 32×8 (`:48-51`).

**The mark-valued collapse latch** (wake-economy 2026-08-14; mark-valued since 2026-09-06): under the
latch a park era pays ONE `FUTEX_WAKE`. The v3 latch was a per-era 0/1 flag cleared by `park_begin`, and
its argument assumed every completion whose bump the snapshot absorbs is FOUND by the post-registration
scan — true only for ops in the parker's PENDING SET. The daemon publishes a slot's DONE and bumps the
doorbell AFTER, so an op the reaper consumed off its slot word in that gap (or a sync-lane op sharing the
session) bumps with nothing left to scan; its bump is absorbed by the snapshot (admission passes) yet its
CAS could land after the clear — winning the era's one wake toward a parker not yet asleep (LOST), and the
parker's real completion collapsed: a strand to the age bound (`:72-86`;
`R/.benchmarks/2026-09-06-cqe-doorbell-lost-wake.md`, found at loom iteration 2702: snapshot 1, seq 2,
mark 2, latch consumed, `L:3586-3589`). Mark-valued: `wake_paid_mark` records the mark the era's paid
wake SATISFIED; a mark-passed completion collapses iff the record equals the mark it just read. A
pre-snapshot completer paying late read a STALE mark (the parker's mark follows its snapshot, which
follows that bump) so it records a mark the parker never set and covers nothing; the k-th post-snapshot
completion always pays; two completers racing past one mark pay once (`:88-100`).

Trust boundary (`:102-109`): scribbling `parked` huge ⇒ one wake per completion (the pre-campaign
posture, self-harm); scribbling `seq`/`wake_at`/`wake_paid_mark` confuses only the scribbler's own
reaper, bounded by its §5.3.1-rule-5 wait bound. The daemon never WAITS on any of these words.

### 4.3 The client's park: register → snapshot → re-scan (`interpose.rs:2490-2650`, `session.rs:1217-1268`)

The libaio reaper (`io_getevents` merge loop) holds `parks: [(session_token, ticket)]`:
- **Deep regime** (`parks.len() > reap_event_park_max()`, default **24**, `:2512-2563`): per DISTINCT
  session `k = reap_batch_wake_threshold(pending_on_that_session)` (§6.4) → `cqe_park_begin_batch(k)`
  (+`il_park_eras`); the MANDATORY post-registration re-scan of every ticket (`ticket_done` =
  `is_done_for(gen)`); if none ready: `wait_any(entries, min(deadline − now, reap_quantum()))` where
  `reap_quantum` = `SQUEEZEFS_IL_REAP_QUANTUM_US` (default **50 µs**, clamp 1..=1000 — the former BLIND
  sleep, now the park's AGE BOUND: the k-th completion cuts it short, the quantum caps it, worst case
  identical to the sleep it replaced, `:2793-2805`); then `cqe_park_end()` on every registered session.
  No pre-park spin in this arm (O(sweeps × qd) remote-line probes are CPU theft at fan-in).
- **Sparse regime** (`:2564-2650`): a 4-SWEEP spin over the pending set (sweep-bounded, not
  iteration-bounded — 64 sweeps over qd32 was 2048 cross-line shm probes per empty pass), then per
  session `k = sparse_park_k(pending)` = 1 if `pending ≤ 1` or `SQUEEZEFS_IL_SPARSE_BATCH_MARKS=0`, else
  the same threshold (`:2751-2756`; qd1 stays event-exact BY DERIVATION); register, re-scan, `wait_any`
  bounded by the quantum when any `k > 1`, else the coarse cap; `park_end` on every exit.
- `REAP_EVENT_PARK_MAX` history (`:2705-2730`): shipped 24 → **2** (2026-07-28: the doorbell's
  shared-session wake herd, −21 % elbencho t16qd16) → **24** again (2026-08-13, after the batch mark and
  the lost-wake fix: write 32×8 +14 % median, 16×16 +7 %, qd1 p99 94 vs 98 µs; no losing shape).
  `SQUEEZEFS_IL_REAP_PARK_MAX` clamp 0..=4096.
- `wait_any` (`session.rs:1937-2030`): a userspace admission pre-check (any word already moved ⇒ return
  — at qd32 saturation 42 % of waits failed kernel admission, `:1942-1949`); one entry ⇒ `FUTEX_WAIT`;
  many ⇒ `futex_waitv(2)` (Linux ≥ 5.16, `FUTEX_WAITV_MAX` 128, absolute `CLOCK_MONOTONIC` timeout);
  ENOSYS ⇒ a single wait on the oldest entry capped at `WAITV_FALLBACK_QUANTUM` 200 µs (`:1920-1935`).
- The sync `pread` path never uses the cqe doorbell — it parks on its slot's WAITER bit (§3); both wake
  paths fire from `SlotCompletion::complete` (`ipc_host.rs:425-458`): slot WAITER wake, then
  `cqe.complete(latch)` → `ipc_cqe_wake_{writes,collapsed,elided}`.

### 4.4 The daemon's spin-before-park governor (`spin_governor_core.rs`; service loop `ipc_host.rs:2967-3092`)

Service-thread side (the client's spin is §3's `WAIT_SPINS_*`). Per empty drain pass: window =
`SQUEEZEFS_IPC_SPIN_US` verbatim if set (governor OFF; `0` = the pre-campaign posture), else the governor:
```
window(ewma_park_ns, busy_pct, lanes, cores, lane_sessions):
  lane_sessions < 2            → 0   // single-session lane = the sync-RTT regime: parks are PRODUCTIVE waits; the engaged spin cost qd1 2.5× (p99 70 → 157 µs) — the structural guard, the temporal test alone was falsified (`:126-135`)
  ewma_park_ns == 0            → 0   // never parked: engage on OBSERVED churn only
  ewma_park_ns > 200 µs (SPIN_RAIL_US) → 0   // idle/device-RTT-spaced parks: spinning cannot absorb them
  busy_pct > busy_ceiling_pct(lanes, cores) → 0   // theft guard
  else                         → 100 µs (SPIN_WINDOW_US)
busy_ceiling_pct = 100 − ceil(100 × lanes / cores)   // 12 lanes / 32 cores → 62 (a 2× margin disengaged exactly where the win lived because /proc/stat busy includes the workload's own nvme-tcp softirq, `:93-111`)
fold_park(ewma, obs) = obs if ewma == 0 else ewma − ewma/8 + obs/8       // α = 1/8 (`:89-120`)
```
`SPIN_WINDOW_US = 100` is MEASURED (A-B-B-A at 32×8: 100 µs → +6.7 % median, 20 µs → +2 %, 200 µs →
+4 %, 400+ flat; the first governor's `2 × EWMA ≈ 10–20 µs` counted a WASH — the win spans inter-burst
TAIL gaps, `:76-83`); regime membership is derived, magnitude measured. The headroom gauge samples
`/proc/stat` once per 100 ms across ALL lanes via a monotonic-ns CAS (`:231-289`). Gauges:
`ipc_spin_window_us`, `ipc_spin_absorbed_parks`, `ipc_spin_disengaged_busy`. The queue-worker variant
(`worker_window_ns`, `:200-211`, cap 0 = shipped OFF: it deleted 69 % of the worker's parks and moved no
latency term at +5.5 % CPU/op) is the FUSE transport's business, not this rung's.

Service park (`ipc_host.rs:3013-3090`), per owner thread over its pinned sessions: `daemon_parked.store(1,
SeqCst)` on EVERY owned header → **SNAPSHOT every doorbell** (`load(SeqCst)`) → re-scan (a full drain
pass + `sink.flush()`) → if nothing served: `futex_wait_many((doorbell_i, observed_i)…, SERVICE_PARK_MAX
= 5 ms)` (`futex_waitv`, cap 128; fewer-kernel fallback = the first word) → fold the park duration into
the EWMA → `daemon_parked.store(0)`. Two lost-wake classes closed (`:3020-3041`): snapshot BEFORE rescan
(snapshotting after folded a submission that raced the rescan's tail INTO `observed`; 13 % of parks
expired by timeout under load), and wait on ALL owned doorbells (a single-doorbell wait deafened the
thread to its other sessions: the sessions=8 < sessions=4 inversion). A session-less owner parks on
`thread::park_timeout(5 ms)` (`:3064-3065`; §9 Q19).

### 4.5 Loom models and weakenings (`L:3155-3678`) → port models

- `ipc_cqe_parked_reaper_never_stranded` (`L:3176`): server = serve + `SlotCore::complete` + `cqe.complete`
  → wake witness; reaper = `park_begin` → scan `is_done_for` → admission `seq == expected` → if admitted,
  the wake MUST have fired. Weakenings verified to FAIL (`L:3169-3174`): (a) either `fence(SeqCst)`
  removed, (b) `park_begin` permuted to snapshot-before-register, (c) the daemon's `parked` load weakened to
  Relaxed.
- `ipc_cqe_batch_parked_reaper_never_stranded` (`L:3260`): two ops, `park_begin_batch(2)`, two
  completers; same three weakenings fail.
- `ipc_cqe_latched_parked_reaper_never_stranded` (`L:3362`): latch live, two completers, k=1; asserts
  BOTH covered (witness ≥ 1) AND collapsed (witness == 1 — the era paid exactly one syscall). Admission is
  modeled as an RMW (`fetch_add(0, SeqCst)`) — FUTEX_WAIT's bucket-lock read is a latest-value read; a plain
  load under loom's weaker SC manufactures a false strand (`L:3394-3406`).
- `ipc_cqe_latch_two_parkers_covered` (`L:3451`): split submitter/reaper, per-parker witnesses the
  completer sets together on `Wake` (breadth `i32::MAX`).
- `ipc_cqe_latched_reaped_prior_completion_never_strands` (`L:3597`): the 2026-09-06 class with a
  **futex bucket** (`FutexBucket{queued, expected}`, `L:3556-3572`: WAKE delivers only to a QUEUED sleeper;
  a spurious wake re-queues) — D1 = the already-reaped prior op's late doorbell completion, D2 = the pending
  op's serve + DONE + doorbell, one k=1 parker; RED on the v3 flag latch, GREEN on the mark-valued latch.
- `ipc_park_snapshot_before_rescan_never_strands` (`L:3790`): the SUBMIT side — producer = push →
  `doorbell.fetch_add`; consumer = snapshot → rescan → admit; an admitted park implies an empty ring;
  weakening — snapshot after rescan FAILS.
- `ipc_wake_ring_publication_never_stranded` (`L:3695`): the `WakeCoalescer` composition — rig-only in
  production (§9 Q1).

**Port models** for `explorer.zig` (SC + explicit per-thread pending store drained by the fence step, as
AGENTS.md requires for a Dekker core): (1) `CqeDoorbellModel` — threads D1 (prior op: `seq+=1`, FENCE,
`parked` load, `wake_at` load, latch CAS, bucket wake), D2 (serve, DONE, same), P (`parked+=1`, FENCE,
`seq` snapshot, mark compose, scan, bucket admit); invariant after every step: ¬(P queued ∧ D1 done ∧ D2
done); final: wakes paid ≤ 1 per era. Weakenings that must `expectFails`: drop D's fence (the store
buffer lets `parked` read 0), drop P's fence, snapshot-before-register, and the 0/1 flag latch with a
parker-side clear. (2) `SubmitDoorbellModel` — producer (ring push, `doorbell+=1`, FENCE, `parked` load),
consumer (`parked=1`, FENCE, snapshot, rescan, admit); weakenings: snapshot-after-rescan, and dropping
either fence under the store buffer (the reference's production shape has no producer fence — §9 Q2).

---

## 5. The submission ring (`ring_core.rs`) — bounded MPSC of slot indices

Producers: app threads (untrusted); consumer: the ONE service thread that owns the session's drain
(`:3-6`). **Single-consumer PRECONDITION, structural** (`:12-23`): a session is pinned to one service thread
for life (`D:341`), and the consumer cursor `RingConsumer{head}` lives in daemon-PRIVATE memory —
deliberately not in the mapping, so a hostile client cannot fake or fork consumption; `!Clone`.

```
push(value):                                   // any producer
  tail = tail.load(Relaxed)
  loop: cell = cells[tail & mask]; seq = cell.seq.load(Acquire)      // pairs with the consumer's freeing Release
        dif = seq −wrap tail (as i32)
        dif == 0: CAS_weak(tail → tail+1, Relaxed, Relaxed) ok → cell.value.store(value, Relaxed); cell.seq.store(tail+1, Release); return true   // publication rides the cell seq, never the tail
                  err(observed) → tail = observed
        dif <  0: return false                  // previous-lap occupant not freed: FULL (client-visible backpressure; the daemon is never blocked)
        dif >  0: tail = tail.load(Relaxed)     // another producer published past us
pop():                                         // the one consumer
  cell = cells[head & mask]; seq = cell.seq.load(Acquire)            // pairs with the publishing Release
  seq −wrap (head+1) == 0 → value = cell.value.load(Relaxed); cell.seq.store(head + capacity, Release); head += 1; Some(value)
  else None                                     // not published (or a hostile scribble: observed as empty, never OOB)
```
(`:166-247`). Capacity rails: `MIN_RING_ENTRIES` **2** — a 1-cell ring is structurally broken: position
1's claim check (`seq == 1`) is indistinguishable from position 0's publication (`seq == 0 + 1`), found by
the ring-vs-reference property suite (`:48-54`); `MAX_RING_ENTRIES` 2¹⁶ keeps `seq − pos` inside `i32`
under wrapping u32 arithmetic (`:44-46`). WHY 1024 default: see §1.1 — `slots ≤ ring_entries` makes "full"
unreachable for an honest client (`session.rs:1117-1121`: a full ring observation means OUR state is
corrupt ⇒ `poison()`), and 1024 slots × 64 KiB default slab = the 64 MiB arena.

Guarantee to the daemon (`:25-34`): memory safety and bounded behavior, never semantic validity — all
indexing masked, corrupted seqs degrade to full/empty, whatever indices come out are validated against
the daemon-owned slot/binding tables (§3).

Submit-side doorbell (`session.rs:1116-1131`): `publish_submitted()` → `ring.push(idx)` →
`header.doorbell.fetch_add(1, Release)` → `if daemon_parked.load(SeqCst) != 0 { FUTEX_WAKE(doorbell) }`.
The daemon's mid-scan case (flag clear) finds the push without a wake by the snapshot→rescan order; "the
daemon's bounded park (≤ 5 ms) absorbs the residual race" (`:1123-1126`; §9 Q2 on the missing fence).
Measured economy: at saturation the daemon never parks ⇒ 0 wake syscalls/op (`hop-ceiling:69`).

**Loom** (`L:2642-2803`): `ipc_ring_racing_producers_never_lost_nor_duplicated` (cap 2, 2 producers;
weakening — the publishing seq store Release→Relaxed FAILS: the consumer pops the seeded phantom 0,
`L:2650-2654`); `ipc_ring_never_reserves_past_capacity` (3 pushes into cap 2 accept exactly 2);
`ipc_ring_lap_reuse_keeps_fifo_exact` (3 values through cap 2 racing the consumer: exactly the accepted
prefix, in order, across the cell-0 lap boundary). Real-atomics stress: 4 producers × 4096 through cap 64,
exactly once (`ring_core.rs:404-465`).

**Port**: the daemon-side ring view is a NEW type (the port's `sync/mpmc.zig` `BoundedMpmc` is a
Vyukov MPMC over OWNED cells — the IL ring's cells are foreign client-writable shm and the consumer is
single, so the view is a borrowed `MpscRingView{tail: *Atomic(u32), cells: []RingCell, mask}` with the
cursor in the daemon's `Session` struct). Model: 2 producers + 1 consumer, cap 2; weakening — value store
AFTER the seq publish.

---

## 6. Sizing derivations (every one a pure function of `derive.Facts` + budget; each with its tie test)

### 6.1 Sessions per mount (shim) — `il_sessions_default(cpus) = clamp(ceil(cpus/4), 2, 16)` (`sizing.rs:55-59`)

Charter: `clamp(cpus/4, 2, 16)` (`C:244`). WHY cpus/4: the measured drain-thread saturation slope of
the 2026-07-19 service sweep (`:39-41`); the 2026-07-28 field capture convicted a DEFAULTS MISMATCH (shim
flat 4 sessions vs daemon `clamp(cpus/4,2,8)` threads: `svc0..3` at 94–99 %, `svc4..7` idle; sessions=8
by hand recovered +44 %, 7.5 → 10.8 GB/s against a 16.6 GB/s raw ceiling, `:3-14`). Rounds UP (the
2026-08-14 allocation ruling: a fractional share derives the next whole unit). Floor 2 = the pre-L4-8
single-dequeue-thread plateau; ceiling 16 = `SESSION_REGISTRY_SLOTS (32) / 2` — the shim's fixed static
session table (constructor-safe, alloc-free) over two concurrently-bound mounts (`:43-50`, test `:227-233`).
Values: 1→2, 8→2, 16→4, 22→6, 32→8, 64→16, 256→16. `SQUEEZEFS_IL_SESSIONS` overrides (1..=16).

### 6.2 Service threads = direct-drive shards — `il_drain_lanes_default(cpus) = clamp(ceil(3·cpus/8), 2, 64)` (`:100-105`)

Charter: "spawned on session admission up to `clamp(3 × cpus/8, 2, 64)`, the drain-lane width — one
function sizes the direct-drive shards too" (`C:244`). WHY 3/8: the counted 2026-08-06 width sweep
(32 CPUs / 2 nodes, il rand-4k 32×qd32, 3×30 s per width, W8 brackets): W8 622–636 k → **W12 695–700 k
(+10.4 %, best clat)** → W16 675–690 k → W24 616–626 k (regression). One lane = 2 OS threads (`sqz-ipc-svcN`
+ `sqz-ipc-ddN`), so the grid is `2W/cpus ∈ {0.5, 0.75, 1.0, 1.5}`: the optimum is ¾ of the core budget
(the complement left to the co-located client fleet) and the one point past 1.0× is the one regression
(`:70-84`; `D:20`). Rails: floor 2; 64 = explicit-lever parity (engages ≥ 174 cpus). **Dominance**
`⌊3c/8⌋ ≥ ⌊c/4⌋` pointwise (test `:268-276`) so every default session owns its own drain thread; spawn-
on-bind keeps idle spares unrepresentable (`ipc_host.rs:244-248`; `SQUEEZEFS_IPC_SERVICE_THREADS` 1..=64).
Values: 4→2, 8→3, 16→6, 32→12, 64→24, 256→64. The port's `cpus` = `derive.facts().cpus` (the process
mask, 0002 row 22).

### 6.3 Arena, admission cap, population, per-uid cap (already in `P/src/mem/sizing.zig:16-99`)

`ipc_arena_cap = budget/8` (12.5 %, no absolute ceiling — the deleted 2 GiB ceiling starved a 251 GB
client: ~31 × 64 MiB vs a 48-HELLO fleet, `ipc_bind_refused_budget` 13/48; `mem_budget.rs:681-701`);
precedence `SQUEEZEFS_IPC_MEM_MAX` (MiB) > `SQUEEZEFS_IPC_MEM_PCT` > derived (`:718-751`; the port's
`resolveIpcArenaCap` takes integers the ONE parser admitted — 0002 row 14). Population target
`max(128, cpus × 8)` (`EXA_CANON_QD` 8 = the matched-inflight fleet slope; 32 cpus → the 256 processes
that refused twice at `ipc_admission_refusals` ~125/pass, engagement 0.697 ⇒ INVALID; floor 128 = the
retired `/128` arithmetic byte-identical, `:786-811`). Arena = `max(64 MiB, align_down(cap/population,
4 MiB))` (`:834-851`; cluster: 22.5 GiB / 256 → **88 MiB**). Per-uid cap = `clamp(cap/arena, 64, 4096)`
(`:889-891`; ≈ the population target by construction; 4096 = the ctl-thread rail). Charter: "arena
`max(64 MiB, cap/population)`; admission cap `budget/8`" (`C:244`).

### 6.4 Batch-wake threshold — `reap_batch_wake_threshold(pending) = min(max(pending/4, 2), max(pending, 1))` (`sizing.rs:186-191`)

Charter: `k = clamp(pending/4, 2, pending)` (`C:244`). FLOOR division deliberately (a wake threshold
rounded up is a LATER wake — the unsafe direction for delivery latency, `:187-190`). `pending/4`
amortizes the wake + reaper wakeup over a quarter of the visible depth (~4× below the flat park's 0.8
wakes/op at 32×8) while a completion BURST still trips the mark inside the age bound; floor 2 = the first
value distinguishable from the flat event park (k=1 IS the sparse regime); ceiling `pending` = the
LIVENESS cap, which DOMINATES the floor (`:173-185`). Values: 0→1, 1→1, 2→2, 3→2, 8→2, 16→4, 32→8,
128→32 (test `:281-307`).

### 6.5 Service-park bound, nonce TTL, deadlines (constants, §7.3)

### 6.6 The hybrid lane gate — `kernel_lane_min_default(memBW, slab, max_op)` (`sizing.rs:107-164`)

Model: `ring_time(size) ≈ ring_rtt + copies × size/memBW`, `kernel_time ≈ fuse_rtt` (the kernel lane's
zc serve moves payload at line rate); equal at `size* = memBW × Δrtt / copies` with `KERNEL_LANE_RTT_DELTA_NS
= 2300` (the D14 field bracket: ring 594 k vs kernel ≈ 253 k IOPS rand-4k ⇒ `1/253k − 1/594k ≈ 2.27 µs`,
a MEASURED class constant, `:107-116`) and `KERNEL_LANE_RING_COPIES = 2` (daemon serve-into-arena +
client arena→app; writes pay one copy, their crossover is 2× and collapses to the same floor, `:118-125`).
Rails: round DOWN to the 4 KiB grain; floor `slab` (at or below it a ring op is the single-flight
allocation-free path the gate exists to protect); ceiling `max_op_bytes` (past it an op must chunk into
multiple flights). Strictly-greater routes kernel; `SQUEEZEFS_IL_KERNEL_LANE_MIN` verbatim, 0 = off
(`lane_gate.rs:49-62`). memBW = the shim's startup micro-probe: best-of-3 `memcpy` of a 4 MiB buffer
(> L2 on every target part; best-of because interference only slows), ~1 ms once per process, memoized,
run at the first establish (never in a constructor) (`:64-100`). Values: 15 GB/s → 64 KiB (slab floor);
100 GB/s → 112 KiB; the charter's "≈ 140–164 KiB" (`C:244`) corresponds to probed 125–146 GB/s field
clients. Tie test `R/tests/derivation_sweep_tests.rs:2370-2397`. The threshold is published to the stats
page (`lane_gate_threshold_bytes`) and exported as `ipc_lane_gate_threshold_bytes` (max over live sessions).

### 6.7 Ctl bounds — `ctl_conn_cap = clamp(2 × cap/footprint, 32, 4096)`, `backlog = clamp(ctl_cap, 64, 4096)` (§2.3)

---

## 7. Knobs, gauges, constants

### 7.1 Knobs (reference → port class; every row through `P/src/core/knobs.zig`)

| Knob | Reference default / range | Reads at | Port disposition |
|---|---|---|---|
| `-o interception` / `SQUEEZEFS_IPC=1` | opt-in; forces `write_back = false` (KD-11), refuses an explicit writeback request | mount | mount option (`P/src/fuse/mount_options.zig:475` currently refuses it — add); the env spelling is dropped |
| `SQUEEZEFS_IPC_MEM_MAX` / `_MEM_PCT` / `_ARENA_MB` | derived (§6.3) | mount | landed, `derived_override` (`P/src/core/knobs.zig:114-116`) |
| `SQUEEZEFS_IPC_SERVICE_THREADS` | derived `il_drain_lanes_default`, 1..=64 | mount | `derived_override` |
| `SQUEEZEFS_IPC_DD_SHARDS` | derived = the same function, 1..=64 | mount | FOLD into one row with the above (one lane = one function, `C:244`) |
| `SQUEEZEFS_IPC_MAX_OP_BYTES` | 1 MiB literal (`fuse_client.rs:31661-31665`) | mount | `derived_override`, derived = the transport's `max_write` (§9 Q14) |
| `SQUEEZEFS_IPC_IDLE_SECS` | 300 s literal, 0 disables (`:31715-31718`) | mount | constant `IDLE_REAP_NS` (§9 Q12) |
| `SQUEEZEFS_IPC_INVAL_WINDOW_MS` | 1000 ms literal (`:31738-31742`) | mount | constant (§9 Q13) — the W1 handoff is the data-plane rung's |
| `SQUEEZEFS_IPC_SOCKET_DIR` | path / `none` | mount | `derived_override` (string row) |
| `SQUEEZEFS_IPC_ALLOW_DEV` | false; both ends; counted | HELLO | `SQUEEZEFS_TEST_IPC_ALLOW_DIRTY`, `test_seam` (§9 Q15) |
| `SQUEEZEFS_IPC_SPIN_US` | absent ⇒ adaptive governor; explicit (incl. 0) = static window, governor off | service pass | `ab_lever` (control arm 0 = the 2026-07-26 posture) |
| `SQUEEZEFS_IPC_SPIN_ADAPTIVE` | true | service pass | `ab_lever` (0 = window 0) |
| `SQUEEZEFS_IPC_CQE_WAKE_LATCH` | true (`ipc_host.rs:468-471`) | completion | `ab_lever` (0 = wake-per-mark-passed control arm) |
| `SQUEEZEFS_IPC_ARENA_THP` | true (both sides) | map time | `ab_lever` |
| `SQUEEZEFS_NUMA` | true | placement | `ab_lever` (instrument stays alive at 0) |
| `SQUEEZEFS_IL_SESSIONS` | derived `il_sessions_default`, 1..=16 | shim establish | shim `derived_override` |
| `SQUEEZEFS_IL_SPINS` | adaptive (4096 / 256) | shim wait | `ab_lever` (pins both) |
| `SQUEEZEFS_IL_OP_TIMEOUT_MS` | 30 s | shim wait | constant + `SQUEEZEFS_TEST_IL_OP_TIMEOUT_MS` seam (the bounded-wait pin needs a small value) |
| `SQUEEZEFS_IL_REAP_PARK_MAX` | 24, 0..=4096 | reaper | `ab_lever` |
| `SQUEEZEFS_IL_REAP_QUANTUM_US` | 50, 1..=1000 | reaper | `ab_lever` |
| `SQUEEZEFS_IL_SPARSE_BATCH_MARKS` | true | reaper | `ab_lever` (0 for latency-sensitive unsaturated mounts) |
| `SQUEEZEFS_IL_KERNEL_LANE_MIN` | derived (§6.6); 0 = off | establish | `derived_override`/`ab_lever` |
| `SQUEEZEFS_IL_MAX_RUN_SLOTS` | 0 = uncapped; TESTING ONLY (`session.rs:77-92`) | claim_run | `SQUEEZEFS_TEST_IL_MAX_RUN_SLOTS`, `test_seam` |
| `SQUEEZEFS_IL_PARANOID_FSTAT` | DESIGNED (`D:319`), **never implemented** (0 hits) | — | not carried |
| `SQUEEZEFS_IL_BUILD_COMMIT` | build-time `env!` (`preload/lib.rs:61`) | compile | the port bakes the commit via `build.zig` options |

Shim knob asymmetry (ENG-10): a malformed shim-side value announces once on stderr and keeps the
default — never kills the host app (`lane_gate.rs:102-124`). The daemon side goes through the port's ONE
parser and the startup gate.

### 7.2 Gauges (`R/src/fuse_client.rs` METRICS, names verbatim; `D:482-507`)

Sessions: `ipc_sessions_active/total/reaped/poisoned` (poisoned MUST stay 0), `ipc_session_admission_ns`
(always-on histogram HELLO receipt → SessionOk sent; the fleet-launch convoy instrument),
`ipc_session_owners`, `ipc_service_threads` (SPAWNED), `ipc_arena_bytes`, `ipc_admission_refusals`.
Refusal ledger: `ipc_binds`, `ipc_bind_refused_{version,nonce,flags,mode,budget,peercred,disabled}`,
`ipc_binds_dev_override`. Engagement: `ipc_ops_{read,write}`, `ipc_bytes_{in,out}` (an il row is INVALID
unless their Δ accounts for its ops — charter rule, `C:244`). Serve split: `ipc_fast_path_serves`,
`ipc_async_handoffs`, `ipc_fast_path_{lock,miss}_demotions`, `ipc_hold_probe_{serves,misses}`.
Protocol: `ipc_descriptor_rejects` (MUST stay 0), `ipc_drain_empty_passes`, `ipc_service_parks`.
Wake economy: `ipc_cqe_wake_{writes,collapsed,elided}` (denominator `writes/(writes+elided+collapsed)`,
≤ 0.25 at 32×8 the campaign target; ≈ 1 under a saturated parked reaper = the latch stopped engaging),
`ipc_cqe_pass_wake_flushes`. Governor: `ipc_spin_window_us`, `ipc_spin_absorbed_parks`,
`ipc_spin_disengaged_busy`. Display-only (client page, summed verbatim): `ipc_lane_gate_kernel_{routes,
bytes}`, `ipc_lane_gate_threshold_bytes`, `il_submit_harvested`, `il_park_eras`, `il_slot_reroutes`.
THP/NUMA: `ipc_arena_prep_{queued,done,skipped_dead,skipped_pressure}`, `ipc_direct_shards`. Port
mapping: per-op counts = `counters.ShardedCounter`; levels = `Gauge`; `ipc_session_admission_ns` and the
ingress delta = `ShardedLatencyHistogram`; `ipc_sessions_poisoned`, `ipc_descriptor_rejects`,
`ipc_ring_full_stalls` (shim) = `Tripwire` with per-site labels.

### 7.3 Constants with WHY

| Constant | Value | WHY |
|---|---|---|
| `NONCE_TTL` | 60 s | anti-replay horizon for stale traces; current+previous accepted so a rotation never races a live handshake (`ipc_host.rs:82-84`) |
| `SERVICE_PARK_MAX` | 5 ms | §5.3.1 rule 5: no daemon wait on a client-writable word exceeds this (the D18 ladder cap, `:86-88`) |
| `CTL_HANDSHAKE_TIMEOUT` | 10 s | an unestablished peer silent this long has given up; matched to the shim's `CTL_RECV_TIMEOUT` (`:98-102`; `session.rs:95`) |
| `CTL_RECV_POLL` | 500 ms | `SO_RCVTIMEO` liveness poll so teardown is bounded even if a sever is missed (`:90-96`) |
| `OP_TIMEOUT_DEFAULT` | 30 s | the per-op deadline that poisons (`session.rs:42`); the 50 ms futex slice re-checks poison (`:1348`) |
| `WAIT_SPINS_DEFAULT / PARKY` | 4096 / 256 | §3; 256 measured best floor (shorter over-parks just-completing ops) (`:44-59`) |
| `REAP_EVENT_PARK_MAX` | 24 | §4.3 history 24 → 2 → 24, counted (`interpose.rs:2705-2730`) |
| `REAP_BATCH_QUANTUM_US` | 50 | the park's age bound (`:2793-2805`) |
| `SPIN_RAIL_US / SPIN_WINDOW_US` | 200 / 100 µs | §4.4 (`spin_governor_core.rs:74, :83`) |
| `HEADROOM_SAMPLE` | 100 ms | one `/proc/stat` read per window across all lanes (`:87`) |
| `EWMA_SHIFT` | 3 (α = 1/8) | the standing smoothing class (`:91`) |
| `WORKER_BUSY_CEILING_PCT` | 80 | Erlang-C knee (`:169-178`) — transport rung |
| `INGRESS_PLAUSIBLE_MAX_NS` | 1 s | u32-ns wrap ambiguity (`layout.rs:606`) |
| `KERNEL_LANE_RTT_DELTA_NS` | 2300 | §6.6 measured (`sizing.rs:116`) |
| `SESSION_REGISTRY_SLOTS` | 32 | the shim's static session table (`:50`) |
| `FUTEX_WAITV_MAX` | 128 | kernel cap (`session.rs:1921`) |
| `WAITV_FALLBACK_QUANTUM` | 200 µs | pre-5.16 degradation (`:1927`); not carried (§9 Q20) |
| `IPC_ARENA_DMA_ALIGN_BYTES` | 4 MiB | `slots × LBA` (`mem_budget.rs:773`; port `P/src/mem/sizing.zig:23`) |
| `PMD_SIZE` | 2 MiB | x86_64 huge page (`thp.rs:72`) |
| `MADV_COLLAPSE` | 25 | UAPI value pinned against old libc (`:69`) |
| dev-cache `SLOTS / WAYS` | 1024 / 4 | 8 KiB > any process's mounted-filesystem set; O(1) probe (`dev_cache.rs:17-20`) |
| ctl `CTL_MSG_MAX` | 64 KiB | admin JSON bodies (`wire.rs:276`) |

---

## 8. Tests (reference name → the sentence the port's test must assert)

Layout/protocol units (`layout.rs`, `slot_core.rs`, `ring_core.rs`, `cqe_core.rs`, `wire.rs`):
- `default_geometry_validates_and_computes` — regions in order, page-aligned, non-overlapping, arena last.
- `geometry_refusals_attributed_exactly` — 11 refusals each hit their own variant via `validate` AND `compute`.
- `header_roundtrip_and_refusals` — bad magic / abi+1 / bad geometry each refuse at map time.
- `ingress_stamp_roundtrip_and_delta_law` — 0 stamp, future stamp, ≥ 1 s all discard; wrap-safe.
- `slot_descriptor_publish_snapshot_roundtrip` — snapshot equals the published descriptor; result round-trips.
- `full_cycle_walk_and_generation_stamps` / `release_claimed_returns_an_unsubmitted_hold_to_free` /
  `begin_serve_refuses_every_non_submitted_state` / `waiter_bit_preserved_across_begin_serve_and_reported_by_complete`
  / `park_prepare_after_done_is_ready_not_stranded` / `stale_generation_never_sees_recycled_done` /
  `cycle_stress_exactly_once_per_generation` (10 k lives, consumer racing the server).
- `geometry_refusals_exact` / `seed_for_sharing_revives_zeroed_cells` / `fifo_single_producer_single_consumer`
  / `full_ring_refuses_then_reopens_after_pop` / `many_laps_keep_order_and_capacity_exact` /
  `multi_producer_stress_exactly_once`.
- `wake_gated_on_parked_count` / `batch_park_wakes_at_kth_completion` / `park_begin_is_batch_one` /
  `earliest_mark_governs_across_parkers` / `wake_covers_every_parker` / `latch_collapses_an_era_to_one_wake`
  / `latch_new_era_is_payable` / `latch_pay_for_a_stale_mark_never_covers_the_era` /
  `latch_initial_record_never_collapses` / `compose_keeps_an_at_snapshot_mark_while_its_pay_is_unrecorded` /
  `compose_replaces_a_spent_at_snapshot_mark` / `latch_first_completion_still_pays_immediately` (qd1 inert)
  / `latch_false_is_the_shipped_posture_verbatim` / `latch_composes_with_the_batch_mark`.
- `blob_roundtrip_and_negative_probe` / `ctl_roundtrip_every_variant` / `decoders_are_total_on_garbage` /
  `degenerate_identities`.

Daemon host (`R/tests/ipc_host_tests.rs`):
- `hello_on_control_plane_only_host_refuses_class_disabled`; `hello_version_skew_refuses_including_unknown_and_dirty`;
  `hello_stale_nonce_refuses`; `hello_peercred_mismatch_refuses`.
- `fd_screen_refuses_adversarial_fds_at_hello_and_bind` — a raw-socket client presenting O_PATH, a
  directory, O_APPEND/O_SYNC, wrong-`st_dev`: every row refuses with its class.
- `wrong_direction_ring_ops_refuse_ebadf_both_directions`; `happy_path_bind_serves_echo_and_only_echo`.
- `arena_budget_and_uid_caps_refuse_admission`; `shed_refuses_new_sessions_never_tears_live_ones`;
  `session_teardown_on_eof_frees_arena_and_gauges`; `ipc_session_arena_component_registers_and_sheds`.
- `kd11_interception_forces_write_through_and_refuses_explicit_writeback`.
- `path_socket_binds_advertises_and_serves_a_full_establish`; `unbindable_socket_dir_degrades_to_abstract_only_never_fails_spawn`;
  `no_socket_dir_means_no_path_socket_and_empty_blob_field`; `stale_socket_file_is_replaced_on_spawn`.
- `hello_with_many_fds_in_one_cmsg_refuses_and_installs_none`; `hello_with_truncated_control_data_refuses_and_installs_none`;
  `hello_with_exactly_one_fd_still_establishes`.
- `a_silent_connection_cannot_wedge_host_shutdown`; `an_established_but_idle_connection_survives_the_recv_timeout`.
- `resubmitting_slots_past_the_in_flight_bound_poisons_the_session`; `honest_in_flight_up_to_the_slot_count_never_poisons`.
- `a_connect_storm_is_capped_at_the_derived_ctl_conn_bound`; `a_saturating_session_cannot_starve_its_service_thread_siblings`.
- `admission_defers_arena_thp_prep_off_the_hello_path`; `session_teardown_never_waits_for_prep`.
- `two_colocated_hosts_discover_by_mount_and_never_cross` — two mounts, each shim binds its own daemon.

Shim authn (`R/tests/preload_authn_tests.rs`): `shim_refuses_a_daemon_whose_uid_is_neither_root_nor_the_mount_owner`;
`shim_accepts_the_real_daemon_when_the_mount_owner_matches`; `shim_refuses_an_unsealed_session_memfd`;
`shim_accepts_a_fully_sealed_session_memfd`; `shim_refuses_a_session_reply_carrying_extra_descriptors`;
`shim_refuses_a_path_rendezvous_in_a_world_writable_directory`; `shim_accepts_a_path_rendezvous_under_an_owner_private_directory`;
`host_refuses_to_bind_a_path_socket_in_a_group_or_world_writable_dir`; `refused_rendezvous_never_leaks_descriptors`
(open-fd census before/after).

Sizing ties (`sizing.rs` tests; `R/tests/ingest_economy_tests.rs`; `derivation_sweep_tests.rs`):
`derivation_values`; `ceiling_is_registry_derived`; `drain_lane_derivation_values`;
`drain_lanes_dominate_sessions_pointwise` (1..=4096); `reap_batch_wake_threshold_values` (liveness
pointwise); `kernel_lane_min_derivation_values`; `service_ceiling_is_the_drain_lane_width_and_dominates_shim_sessions`;
`service_threads_spawn_on_bind_no_parked_spares`; `ipc_session_population_target_derives_from_cores`;
`ipc_arena_default_derives_from_admission_cap`; `slot_slab_is_dma_aligned`;
`ipc_per_uid_session_cap_derives_from_session_budget`; `il_drain_lane_width_is_the_three_eighths_lane_pair_slope`;
`il_kernel_lane_min_derives_from_membw_and_lane_rtt_delta`. Governor: `churn_regime_engages_the_measured_window`;
`idle_regime_disengages`; `saturation_disengages`; `ceiling_derives_from_lanes_and_cores`;
`unseeded_lane_never_spins`; `single_session_lane_never_spins`; `ewma_folds_toward_observations`;
`headroom_gauge_samples_and_publishes`. Shim: `reap_park_max_default_and_clamp`; `reap_quantum_default_and_clamp`.
Large ops (`preload_session_tests.rs:1104-1515`): `large_{write,read}_is_one_ring_op_per_max_op_window`,
`_pipelines_all_chunks_before_waiting`, `_short_first_completion_reports_posix_prefix`,
`_error_semantics_prefix_or_errno`, `_releases_every_slot_after_serve`, `fragmented_slots_still_serve_large_*_via_smaller_runs`.

Models: the eleven `ipc_*` loom models of §3–§5 → explorer models with `expectHolds` + an `expectFails`
per recorded weakening (slot: Relaxed swap; snapshot: Relaxed publish; ring: Relaxed publish; cqe: both
fences, snapshot-first, Relaxed parked load, 0/1 flag latch; submit park: snapshot-after-rescan).

Rig (`ipc_hop_rig.rs`): two processes, `--leg echo|serve|handoff`, `--verify` full pattern verification
both directions (`rig_smoke.rs`), `--kill-client-after-ms` for the kill-9 residue row; the port's rig is a
`zig build` step on the venue with the G-L4-1 thresholds (RTT p50 ≤ 3 µs, syscalls/op ≤ 0.1, ≥ 650 k/core).

---

## 9. Quirks per decision 0002 (each needs a ledger row before diverging) and the better design

| # | Reference quirk | Why it is poor | What we do |
|---|---|---|---|
| Q1 | Header carries a `heartbeat` word NO production code writes (`layout.rs:317`; zero writers) and a 1-byte `WakeCoalescer` only the rig arms/disarms (`:328-330`; `ipc_hop_rig.rs:545, :853`), with a 55 B pad computed around it; the design's death detection ("1 s futex escalation → socket probe → heartbeat stale", `D:400`) is not shipped — the shim poisons on a 30 s op deadline (`session.rs:1344-1346`) | A dead word is a lie in a byte-exact layout; the charter NAMES heartbeat in the header (`C:244`); 30 s of app stall after a daemon SIGKILL is not "bounded-time detection" | Drop the coalescer byte (line 1 = `doorbell`, `daemon_parked`, pad); the service pass stamps `heartbeat` (`clock.monotonicNs()`) at every pass top and park exit; the shim, after ≥ 20 × 50 ms slices with `heartbeat` unchanged, probes the ctl socket (`recvmsg(MSG_PEEK|MSG_DONTWAIT)`: EOF/ECONNRESET ⇒ poison now); the 30 s deadline stays the last rung |
| Q2 | Submit doorbell is `fetch_add(Release)` then `daemon_parked.load(SeqCst)` with no fence (`session.rs:1128-1129`); correct only because x86 RMWs are full barriers and "the daemon's bounded park (≤ 5 ms) absorbs the residual race" (`:1125-1126`); the loom model uses `fetch_add(SeqCst)` for the producer (`L:3803`) — stronger than production | A Dekker pair that relies on the ISA and a timeout, and a model that checks a different program than ships | The submit side is the SAME `fence(SeqCst)` Dekker as the cqe side (bump → fence → parked load / parked store → fence → snapshot); the port's `SubmitDoorbellModel` carries the fence step with a per-thread store buffer and its removal `expectFails` |
| Q3 | `SessionOk` carries the geometry AND the sealed header carries it; the shim compares the two (`session.rs:642`); wire strings are UTF-8-validated `String`s with `BadString` on non-UTF-8 (`wire.rs:172-181`) | Two copies of one authority; a NUL-padded byte field is not text | `SessionOk` = tag only + the fd; geometry authority is the SEALED header: shim `fstat(memfd).st_size`, maps, validates header, `SessionLayout.compute(header.geometry).total_bytes == st_size`; frames are `extern struct`s decoded by `@sizeOf` equality (`Truncated` otherwise), byte fields are bytes |
| Q4 | `RefuseClass::Mode` (=4) means "wrong `st_dev`" and feeds `ipc_bind_refused_mode` (`wire.rs:79-82`) | A class named for a check it does not perform | Class `mount` / gauge `ipc_bind_refused_mount`; `flags` keeps the O_PATH/S_ISREG/append/sync/tmpfile family; a `rights` class for the access-mode arm |
| Q5 | `BindOk{read_ok u32, write_ok u32}` | Two words for two bits | `rights: u8` bitfield `{read, write, odirect}` + `_pad[7]`; the `odirect` bit the daemon already derives (`ipc_host.rs:2503`) travels so the shim can gate `RWF`/direct shapes without a second `F_GETFL` |
| Q6 | HELLO echoes `pid, uid` the daemon then compares to `SO_PEERCRED` (`ipc_host.rs:2251-2256`) | The kernel already tells the daemon who connected; the echo is a self-report that can only be wrong | HELLO carries no pid/uid; `SO_PEERCRED` is read once per connection for logs/caps; the admin lane's authorizer is `SO_PEERCRED` alone |
| Q7 | Mount-time ad-hoc `std::env::var(...).parse()` for `MAX_OP_BYTES`, `IDLE_SECS`, `INVAL_WINDOW_MS` (`fuse_client.rs:31661-31742`) beside the knob registry | Second parser (0002 rows 14/16) | Every row through `knobs.*`; see Q12–Q14 for the rows that become constants |
| Q8 | `OP_ECHO` kept in the product daemon as a "ping" that reads+inverts arbitrary arena bytes (`layout.rs:102-108`; `ipc_host.rs:1118-1128`) | A non-product op on the product path, serving no app | The op table is `{READ, WRITE}`; liveness in tests = a 0-byte READ on a bound fd; the rig has its own ECHO sink behind a `-Drig` build option |
| Q9 | Degenerate-build admission `SQUEEZEFS_IPC_ALLOW_DEV` as a product knob announced once (`ipc_host.rs:124-151`) | A product knob whose only legitimate use is a dev box | `SQUEEZEFS_TEST_IPC_ALLOW_DIRTY`, class `test_seam`, default-inert; still counted (`ipc_binds_dev_override`) |
| Q10 | Thread-per-ctl-connection (`ctl_conn_cap_from` rail 4096 "every ctl connection is an OS thread", `ipc_host.rs:263-265`) and per-session `JoinHandle` pruning per accept | 256 fleet processes = 256 idle threads; charter §9 puts fd readiness on a ring, not a thread | ONE `sqz-ipc-ctl` thread per host on a `ParkedRing` (`POLL_ADD`/`RECVMSG` per accepted socket); the cap/backlog derivations bound connections, not threads |
| Q11 | A session-less owner thread loops on `thread::park_timeout(5 ms)` forever (`ipc_host.rs:3064-3065`) | A 200 Hz idle poll on a thread that exists only because a session once did | A session-less owner parks unbounded on its own daemon-private futex word; admission wakes it; teardown of its last session leaves it parked |
| Q12 | `SQUEEZEFS_IPC_IDLE_SECS` = 300 literal with a knob on top | 0002 row 17 (the `tuning` class) | Constant `IDLE_REAP_NS = 300 s` with WHY (a leaked idle session holds one arena of the cap; 300 s ≫ any open→read gap, ≪ the per-uid cap's time to fill under a fork-bomb) |
| Q13 | `SQUEEZEFS_IPC_INVAL_WINDOW_MS` = 1000 literal | same | Constant `INVAL_WINDOW_NS = 1 s` on the data-plane rung (= the attr-TTL staleness class the design bounds W1 by, `D:377`) |
| Q14 | `SQUEEZEFS_IPC_MAX_OP_BYTES` = 1 MiB literal | A geometry ceiling that duplicates the transport's | `derived_override`, derived = the transport's `max_write` (`P/src/core/knobs.zig:137` `max(block_size, 1 MiB)`) so both lanes share one per-op ceiling and the lane gate's rail is the same number |
| Q15 | `futex_waitv` ENOSYS fallback (`session.rs:1920-1935`; `ipc_host.rs:3039-3041`) | The venue is 7.2.6; a pre-5.16 arm is dead code (charter §1.9) | Require `futex_waitv`; the shared futex face exposes `waitMany(entries, timeout_ns)`; fail loud at session establish if ENOSYS |
| Q16 | The shim's ring-full observation poisons the SESSION (`session.rs:1117-1121`) | Correct (impossible for an honest client) but it conflates "our bookkeeping is corrupt" with "daemon violated protocol" | Keep the poison, add the `il_ring_full_impossible` tripwire label so the two classes are distinguishable in the refusal line |
| Q17 | `expected_st_dev` learned lazily (`u64::MAX` until set, `ipc_host.rs:1200-1203`, `:1519-1521`), refusing early HELLOs as `Mode` | A refusal class spent on a startup race | The port resolves the mount's `st_dev` before the host arms (the mount path already `fstat`s the root) |
| Q18 | Reference `IPC_ABI` series 1–6 and `SQZIPC01` magic | No wire compatibility is owed (0002) | `IPC_ABI = 1` of the port's own series; magic `SQZIPC02` so a reference shim can never map a port session by accident |

Not quirks (carried as-is): the 128 B slot pair with 64 B of fields (the pad is the second line the
protocol word must own); the client-written ingress stamp (a cheap display input); the 4 MiB arena
alignment; PERF-18's co-located pairs; the deferred THP collapse; balance-first owner pick; the
stats-page fold-once-at-teardown; `st_nlink == 0` conservatism; the `-dirty` refusal.

---

## 10. Delivery order for this rung (red-first)

1. `sync/futex.zig` shared face (`.private = false`) + `waitMany` over `futex_waitv`; red test: two
   processes (fork) parking on a shared `memfd` word, the parent's wake ends the child's wait.
2. `ipc/layout.zig`: §1.8 structs, `SessionLayout.compute`, `slotSlab`, the 11 geometry refusals; ties
   for every offset/size in §1.3–1.6.
3. `ipc/slot.zig`, `ipc/ring.zig`, `ipc/cqe.zig` (dependency-free value types over the shm words) + the
   four explorer models with their `expectFails` weakenings (§3–§5); evidence note records which weakening
   fails each.
4. `ipc/wire.zig`: blob + ctl frames as `extern struct`s, total decoders (fuzz: never panic, round-trip).
5. Daemon host: `il0` synthesis in `kv_fs.zig` (armed-mount gate, ENODATA otherwise; the screen already
   refuses set/list); listener(s) + the `sqz-ipc-ctl` ring thread; HELLO ladder (§2.6) + fd screen (§2.7)
   with the adversarial matrix; memfd create/seed/seal; admission + owner pick + spawn-on-bind; `sqz-ipc-svc`
   drain/park loop (§4.4) with snapshot-before-rescan; poison/teardown with the lease-refcounted unmap.
6. Shim (`libsqueezefs_il.so`, C ABI, `-z nodelete`): classification + negative cache, blob read, the three
   authentication rungs, map + header validation, slot claim/run, sync wait (§3), reaper park (§4.3),
   lane gate (§6.6). Every refusal a reason line once per (mount, reason).
7. Sizing: `ilSessionsDefault`, `ilDrainLanesDefault`, `reapBatchWakeThreshold`, `kernelLaneMinDefault`
   in `P/src/mem/sizing.zig` beside the landed arena family, each with its tie test and the dominance sweep.
8. The hop rig on the venue; the evidence note with the G-L4-1 table.
