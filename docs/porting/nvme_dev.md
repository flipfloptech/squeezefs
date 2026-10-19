# PORTING SPEC — `nvme_dev` (NVMe block-device io_uring workers)

Reference: `/Users/joberdorf/Source/squeezefs/src/nvme_dev.rs` (2831 lines), tests
`tests/nvme_dev_tests.rs` (821 lines). All `L<n>` cites are `src/nvme_dev.rs` unless
prefixed. Linux-only; the one `#[cfg(target_os="linux")]` is the `O_DIRECT` flag.

What is NOT in this file (do not port here): discard/trim (`src/block_reclaim.rs`),
device identity/NGUID/WWID (`src/zcrx_lane/probe.rs`, `src/meta_backend/reservation.rs`),
logical/physical block size (`src/uring_fs.rs:738`, `src/meta_backend/atomicity.rs:139`),
discard elision (`SQUEEZEFS_DISCARD_ELISION`, reclaim side). The zcrx userspace read lane
(`try_lane_read`, L2246-2376) is a separate module's client; port the hook point only.

---

## 0. Object model

| Type | Visibility | Role | Lines |
|---|---|---|---|
| `NvmeBlockDev` | `pub`, `#[derive(Clone)]` | Handle; every field is an `Arc`/`Copy` so clones share ONE device state | L1488-1567 |
| `UringWorker` | private | One "lane": thread + ring + O_DIRECT fd + bounded channel + watermark + eventfd wake | L595-603 |
| `LaneWake` | private | eventfd + `WakeCoalescer` (arm/disarm flag) | L553-593 |
| `WriteWatermark` | private | per-lane `submitted`/`completed` counters + `Notify` for the DUR-2 barrier drain | L463-541 |
| `DeviceFence` | `pub(crate)` | sticky `halted` + `dismounted` latches + optional probe closure | L1438-1485 |
| `NodeProbe` | private | TTL-cached `Path::exists` | L1418-1423 |
| `UringRequest` | private enum | `Read{offset,buf_ptr,size,bytes,tx,enq,trace_id,dest_token}` / `Write{offset,data,tx,enq,trace_id}` / `Fsync{datasync,tx}` | L303-349 |
| `UringResponse` | private enum | `Read{bytes,size,offset,tx,submitted,trace_id}` / `Write{tx,submitted:Option,trace_id}` (fsync rides `Write` with `submitted=None`) | L351-371 |
| `WriteData` | private enum | `Aligned{Bytes}` / `Unaligned{ptr,len}` (posix_memalign, free with `libc::free`) / `PooledUnaligned{ptr,len}` (`ALIGNED_BUF_POOL.recycle`) | L239-253 |
| `DestToken` | `pub type Box<dyn Any+Send>` | opaque owner token for a zero-copy read destination; dropped at CQE | L175 |
| `DestResolver` | `pub type Arc<dyn Fn(u64,usize)->Option<DestToken>+Send+Sync>` | registry callback | L180 |
| `ShortReadPrefix` | `pub struct {pub got: usize, msg}` | typed payload of the exact-length `UnexpectedEof` | L412-424 |

`NvmeBlockDev` fields (L1489-1567): `device_path: String` (pub), `worker: Arc<UringWorker>`
(lane 0), `lane_pool: Arc<ArcSwap<Vec<Arc<UringWorker>>>>` (always contains lane 0 at index 0),
`lane_rr: Arc<AtomicUsize>`, `read_lanes: Arc<AtomicUsize>` (init 1), `write_lanes:
Arc<AtomicUsize>` (init 1), `write_grain: Arc<AtomicU64>` (init 0), `node_probe`, `fence:
Arc<DeviceFence>`, `sync: Arc<SyncCoalescer>`, `write_cache: WriteCacheClass` (Copy, probed
once in `new`), `write_cache_override: Arc<AtomicU8>` (0=none, else class+1), `fsync_ordinal:
Arc<AtomicU32>` (u32::MAX = unassigned), `lane: Arc<OnceCell<Option<Arc<LaneSession>>>>`
(zcrx), `zc_fd: Arc<OnceLock<Option<OwnedFd>>>`, `zc_wfd: Arc<OnceLock<Option<OwnedFd>>>`.

---

## 1. Worker architecture

### 1.1 Lane (thread) count derivation

- Construction (`NvmeBlockDev::new`, L1658) spawns exactly ONE lane (lane 0). Offline tools and
  tests that never arm lanes keep one worker per device (routing.rs:3009-3010).
- Mount arms more via `BackendRouter::arm_io_lanes` (routing.rs:3011-3039): per plane,
  `derived = io_lanes_for(cpus, distinct_data_devices.len())` with `cpus =
  crate::cpu::process_parallelism()` (affinity-mask CPUs ÷ fleet share, ceil, floor 1 —
  cpu.rs:66). Env `SQUEEZEFS_NVME_READ_LANES` / `SQUEEZEFS_NVME_WRITE_LANES` win verbatim
  (`n.max(1)`). Then `dev.set_read_lanes(read_lanes)`; `dev.set_write_lanes(write_lanes, grain)`
  with `grain = router.block_size.max(1)`; gauges `data_read_lanes` / `data_write_lanes` stored.
  Re-run by online `volume add-data`; lane counts only grow.
- ```rust
  pub fn io_lanes_for(cpus: usize, data_devices: usize) -> usize {
      cpus.div_ceil(data_devices.max(1)).clamp(1, cpus.max(1))      // L1637-1639
  }
  ```
  WHY (L1605-1636): one submitting thread per device = one blk-mq software queue = one nvme-tcp
  connection = one RX softirq core; nvme-tcp RX copy ≈ 2.7 GB/s/core ⇒ 8 namespaces × 1 core ≈
  the measured 22 GB/s read wall. Discriminator: 1 submitter/dev qd16→qd32 = 11.5→12.1 GB/s;
  4 submitters/dev at same in-flight = 16.7 (8 = 17.7, test header). Writes: kernel seq writes
  walled at 35.31 GB/s = 7.06 GB/s × 5 namespaces while fio with spread queues reached 49.7
  GB/s. CEILING since 2026-08-14 (a fractional share rounds UP — "a submission lane is cheap,
  the wall is not"). Pinned table (tests L549-571): (32,8)=4, (32,5)=7, (32,4)=8, (30,8)=4,
  (32,1)=32, (4,8)=1, (0,0)=1. Derivation-sweep tie (derivation_sweep_tests.rs:1500-1504):
  `io_lanes_for(effective_parallelism_from(32,4)=8, 2) == io_lanes_for(32,2)/4` (4 == 16/4).
- Pool growth `grow_pool(n)` (L1765-1780): CAS loop on the `ArcSwap`; clone the Vec, push
  `UringWorker::new(device_path)` until `len == n`, `compare_and_swap`; a lost race drops only
  freshly spawned idle workers. Monotone, idempotent.
- `set_read_lanes(n)` (L1785): `n=max(1)`, `grow_pool(n)`, `read_lanes.fetch_max(n, Relaxed)`;
  logs info when grown. `set_write_lanes(n, grain)` (L1808): same plus
  `write_grain.store(grain.max(1), Relaxed)`. `set_write_lane_grain(grain)` (L1829) re-stamps
  grain only.

### 1.2 Lane pick

- Reads (L1865-1877): `lanes = read_lanes.load(Relaxed).min(pool.len()).max(1)`;
  `idx = lane_rr.fetch_add(1, Relaxed) % lanes`; returns `pool[idx].clone()` (Arc kept alive
  across the await).
- Writes (L1883-1892): if `write_lanes <= 1` → `self.worker` (lane 0, byte-identical pre-fanout).
  Else `pool[write_lane_index(offset, grain, lanes.min(pool.len()).max(1))]`.
  ```rust
  pub fn write_lane_index(offset: u64, grain: u64, lanes: usize) -> usize {
      ((offset / grain.max(1)) % lanes.max(1) as u64) as usize            // L1653-1655
  }
  ```
  WHY (L1641-1652): same block → same lane → same channel FIFO → same connection, so two DMAs
  naming one offset keep per-offset SUBMISSION order structurally (completion order was never
  guaranteed: no IO_LINK/IO_DRAIN). Per-offset serialization lives ABOVE the device layer.
- Barriers (`Fsync`) and probes always use lane 0 (`self.worker`), L2167-2169.

### 1.3 Thread name, channel, wake

- `UringWorker::new` (L609-637): `crossbeam::channel::bounded(URING_REQ_QUEUE_CAP=4096)`
  (L606, "P1-6: bound io_uring request queue to apply backpressure under overload").
  `LaneWake::new()` = `eventfd(0, EFD_CLOEXEC|EFD_NONBLOCK)` (L561), panics on failure.
  Thread name: `comm_name(format!("sqz-nvme{lane}"))` with a process-global `NEXT_LANE`
  counter (Relaxed fetch_add) — numbering is process-wide across ALL devices, not per device
  (L617-621). `comm_name` appends `m<hex nibble>` of the mount slot when nonzero, truncating the
  base to fit 15 bytes (comm_core.rs:35,52-64). Named so `daemon_cpu_ns_by_class` can attribute.
- Thread body: `worker_thread_loop(device_path, rx, &wm, &wake); wm.mark_dead();` — EVERY exit
  path (normal drain, refused open, refused ring) marks the watermark dead (L623-629).
- `UringWorker::submit(req)` (L641-651): `tx.try_send(req)?` then `wake.wake()`. Returns
  `TrySendError` (Full or Disconnected) — the caller counts `uring_queue_full` for either.
- `LaneWake::wake()` (L573-584): `if coalescer.arm() { write(fd, 1u64) ; METRICS.dev_wake_writes++ }
  else { METRICS.dev_wakes_elided++ }`. `arm()` = `!armed.swap(true, SeqCst)`; worker `disarm()` =
  `armed.swap(false, SeqCst)` (wake_core.rs:90-102 — an RMW on purpose, loom-verified protocol:
  producer publishes state THEN arms; worker consumes wake CQE → disarm → scan channel).
  `LaneWake::write()` is the unconditional 8-byte write used at teardown (L588-592).

### 1.4 Worker thread start-up (`worker_thread_loop`, L729-848)

1. `dev_power_cut::arm_from_env(&device_path)` — reads `SQUEEZEFS_TEST_POWER_CUT_DEVS` once
   per worker (L737).
2. Open `OpenOptions::new().read(true).write(true).custom_flags(O_DIRECT)` (L739-746). On
   failure: `log::error!("REFUSING data volume ...: O_DIRECT open failed ... Buffered device I/O
   is not a supported data-plane mode (DUR-2 ...)")` and RETURN — no buffered fallback ever
   (L747-767). Every queued request then fails via the exit drain (§1.8).
3. `build_worker_ring(1024)` (L771) — runtime probe ladder, never a kernel-version table
   (L699-727): try `SINGLE_ISSUER|DEFER_TASKRUN` ("single_issuer+defer_taskrun"), then
   `SINGLE_ISSUER|COOP_TASKRUN`, then `SINGLE_ISSUER`, then plain `IoUring::new(1024)`. Log
   debug which rung. Error → log + return. Entries = **1024** constant (also `active`
   `Vec::with_capacity(1024)`, L925).
4. Fixed FILE: `ring.submitter().register_files(&[fd])` → `use_fixed = Ok` (L790-806). On
   failure log debug and use `Fd(fd)`. Hot SQEs then use `types::Fixed(0)`.
5. Fixed BUFFER (PERF-4 (b), L808-848): `slab_range = RANGED_BUF_POOL.slab_range()`
   (`Option<(base,len)>`); if `Some`, `register_buffers(&[iovec{base,len}])` → `use_fixed_buf`.
   Failure (memlock cap / old kernel) → plain `Read`. WHY: ranged cold-fill DMAs skip the
   per-op page pin (gup). The 4 MiB whole-block pool is deliberately NOT registered: pinning
   cores×16×4 MiB (GiBs) trades real RAM for a pin already amortized over 4 MiB DMAs.
   RANGED slab = `max(cores×64, 512)` × 64 KiB slots (pool.rs:326-327, 813-826).

### 1.5 Worker main loop (L957-1352) — one pass per iteration

State: `active: Vec<Option<ActiveReq>>`, `free_slots: Vec<usize>`, `active_count`,
`disconnected`, `wake_buf: &'static mut [u8;8]` (Box::leak — 8 B leaked once per lane because
ring teardown cancels the eventfd read asynchronously, L931-937), `wake_armed`, `wake_consumed`.

`ActiveReq { response: UringResponse, free_ptr: Option<(FreePtrKind, SendPtr)>, _keep_alive:
Option<Bytes>, dest_token: Option<DestToken> }` (L850-859).

Pass:
1. `if wake_consumed { wake.coalescer.disarm(); wake_consumed=false }` (drain → disarm → scan).
2. `pass_fills = 0`. `if !wake_armed { wake_armed = arm_wake(...) }` — pushes
   `opcode::Read::new(Fd(eventfd), wake_buf, 8).user_data(UD_WAKE)`; if SQ full, `ring.submit()`
   once and retry push (L943-955).
3. Pump: loop `rx.try_recv()` — NEVER blocks on the channel (L970-977). `Empty` → break;
   `Disconnected` → `disconnected=true; break`. For each request:
   - slot: `free_slots.pop()` else `active.push(None)` (the active table grows unbounded in
     principle; bounded in practice by channel cap 4096 + what one pass pumps).
   - **Read**: stall seam (CAS-decrement `STALL_NEXT_READS`, sleep `STALL_READ_MS`); `slow_link_ms
     = take_slow_read()`; `submitted = Instant::now()` — ONE clock read records
     `read_fill_phase_ns.dev_queue = submitted - enq` and op-trace `DevSubmit` (L1035-1042). SQE
     choice (L1059-1083): `in_slab = use_fixed_buf && base <= buf_ptr && buf_ptr+size <= base+len`;
     `(in_slab,use_fixed)` → `(true,true)`: `ReadFixed(Fixed(0), ptr, size, buf_index 0)`;
     `(true,false)`: `ReadFixed(Fd(fd),…)`; `(false,true)`: `Read(Fixed(0),…)`; `(false,false)`:
     `Read(Fd(fd),…)`. All `.offset(offset).user_data(slot_idx)`.
   - **Write**: stall seam (`STALL_NEXT_WRITES`/`STALL_WRITE_MS`); resolve `(ptr,len,free_ptr,
     keep_alive)` from `WriteData` (Aligned keeps the `Bytes` alive; Unaligned→`FreePtrKind::Libc`;
     PooledUnaligned→`FreePtrKind::Pool`); `dev_power_cut::note_write(path, offset, len)` BEFORE
     the SQE (journals prior bytes; one relaxed load unarmed); `submitted=now` records
     `write_pipeline_phase_ns.dev_queue` + `DevSubmit`; SQE `Write(Fixed(0)|Fd(fd), ptr, len)
     .offset(offset)` (L1161-1171). Writes never use WriteFixed.
   - **Fsync**: `ActiveReq{response: Write{tx, submitted:None, trace_id:0}, …}`; SQE
     `Fsync(Fixed(0)|Fd(fd)).flags(DATASYNC if datasync else empty)` (L1179-1208).
   - slow-read seam: if `Some(ms)`, push `Timeout(ts).flags(ETIME_SUCCESS)` with sqe flag
     `IO_LINK`, `user_data(UD_SEAM)` BEFORE the read SQE (L1210-1226).
   - `METRICS.dev_fills++`. Push with up to 3 retries: on SQ-full, `METRICS.dev_enters++` and
     `ring.submit()` (plain submit — no wait; frees the SQ) then retry (L1230-1252).
   - **Queue-full refusal** (L1254-1284): if still not pushed, log error, take the ActiveReq,
     send `Err(Io(Other, "Submission queue full"))` to the oneshot (for Write/Fsync also
     `wm.note_completed()` — a refusal is terminal), release `free_ptr`, `free_slots.push`,
     `break` out of the pump (remaining channel items wait for the next pass). NOTE: the
     `dest_token` of a refused read drops with `act` here.
   - else `active_count += 1; pass_fills += 1`.
4. `if disconnected && active_count == 0 { break }` (L1290).
5. ONE enter: `METRICS.dev_enters++`; `if pass_fills>0 { METRICS.dev_submit_enters++ }`;
   `ring.submit_and_wait(1)` — flushes every SQE pushed this pass (requests + re-armed wake) and
   parks for the FIRST event of either class (L1300-1312). `EINTR` ignored; other errors logged.
6. Reap: `cq.sync()`; for each CQE: `ud == UD_WAKE` → `wake_consumed=true; wake_armed=false;
   continue` (level-triggered: a racing write completes the next read immediately);
   `ud >= UD_RESERVED` → continue (seam timeout); else `slot=ud`, `res=cqe.result()`,
   `io_res = res<0 ? Err(from_raw_os_error(-res)) : Ok(res as usize)`, `complete_one(act, io_res,
   wm)`, push slot to `completed_slots` (L1314-1341). If any completed: `METRICS.dev_wake_batches++`.
   Then `free_slots.push(idx); active_count -= 1` per completed slot.

Sentinels (L125-131): `UD_RESERVED = u64::MAX-1`, `UD_SEAM = u64::MAX-1`, `UD_WAKE = u64::MAX`.
Slot indices are small; anything `≥ UD_RESERVED` is never a request.

### 1.6 Completion (`complete_one`, L868-924) — ORDER MATTERS

1. `drop(dest_token)` FIRST (DMA is over; releases the transport re-arm gate before any reply).
2. Read: `now = Instant::now()`; record `read_fill_phase_ns.dev_service = now - submitted`;
   op-trace `DevComplete`; `tx.send(finish_read(io_res, bytes, size, offset))`.
   Write: if `submitted.is_some()` record `write_pipeline_phase_ns.dev_service` + `DevComplete`;
   `wm.note_completed()` (BEFORE the oneshot — barrier drains must not depend on caller
   scheduling; writes AND fsyncs count symmetrically); `tx.send(io_res.map(|_|()))`.
3. Release `free_ptr` (`libc::free` or `ALIGNED_BUF_POOL.recycle`); drop `keep_alive`.

`finish_read` (L381-401): `got != size` ⇒ `Err(Io(UnexpectedEof, ShortReadPrefix{got, msg:
"short read: kernel returned {got} of {size} bytes at offset {offset} (past-EOF/short reads
must not surface recycled buffer bytes)"}))`; else `Ok(bytes.slice(0..size))`. Exact-length
contract (VL8 item 5): a short read would surface recycled pool bytes as data.
`short_read_prefix_len(&e) -> Option<usize>` (L428-437) extracts `got` for the ONE sanctioned
prefix-tolerant consumer (`DataRouter::read_nvme_block_old_image`, routing.rs:12079).

### 1.7 Caller-side waits and timeouts

- **Reads**: `sqz_time::timeout(read_timeout_ms, rx).await` (L2798-2821). Default **30 000 ms**;
  cell initialised from `SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS` (`.max(1)`), overridable by
  `set_test_read_timeout_ms(ms)` (L138-152). On expiry → `Err(Io(TimedOut, "NvmeBlockDev read
  timed out after {ms} ms (offset=, size=)"))`. The SQE stays in flight; the worker still holds
  `dest_token`/`bytes` until the CQE (MEM-1). AGENTS.md:922: on the sqz kernel the kern READ hot
  path bypasses this (zc direct leg); the per-`read_block` timeout engages on the stock-kernel
  (`SQUEEZEFS_FUSE_ZC=0`), transform, unaligned and device-true postures; measured ≤ 3 % daemon
  CPU / < 0.2 % clat.
- **Writes**: NO timeout. The await is wrapped in `LockWaitToken::begin(LockClass::Device,
  site=0, ino=0, key=offset)` (L2616-2621) so the watchdog names a lost completion ("LOST DEVICE
  COMPLETION"). Barrier: `site=1` (L2180-2185).
- Oneshot `RecvError` (worker dropped the sender) → `InvalidOperation("Worker thread closed
  receiver: …")`.
- **Port — how the caller parks** (decision 0001 amendment 2026-10-18). The reference's oneshot is
  a tokio future, so its await yielded the task; the port's cell (`sync/completion.zig`) parks through
  the device's wait route: `Device.routeWaits(io)` installs `SqzIo`'s futex face once before the first
  request (the data plane does it at volume open), the waiter takes `Completion.waitIo` (a fiber
  parks in the executor's futex table — its lane thread runs the other fibers for the whole DMA; a raw
  thread takes the counted kernel wait) and the lane completes with `Completion.completeIo` (the
  `exec/futex_table.zig` Dekker: fibers first, the kernel wake iff a raw waiter is counted). The
  barrier's watermark drain (§4) takes the same route. A device with no route (tests, offline tools)
  parks on the raw futex on both sides. Counted: `dev_wait_fiber_parks` / `dev_wait_raw_parks`; the
  DMAs in flight are the gauge `dev_dma_inflight` (queue push → publish), bounded by the callers'
  admission (the write pipeline's depth target), never by the lane threads.

### 1.8 Shutdown

`impl Drop for UringWorker` (L683-697): `drop(tx)` (close channel) → `wake.write()`
(unconditional eventfd write; the worker parks on the ring so disconnect is only observable
through the wake) → `thread.join()` (panic logged). Join is deliberately SYNCHRONOUS (L654-682):
P0-1 contract = exit cleanup has run when `Drop` returns; async drop sites hop through
`detached::drop_off_runtime`. Worst case = device latency of in-flight SQEs.

Worker exit (L1354-1407): the loop exits only when `disconnected && active_count == 0`, so all
in-flight SQEs have completed. Then: drain remaining channel items — Write: `release_write_buf`,
`wm.note_completed()`, send `Err(InvalidOperation("NvmeBlockDev worker shutting down"))`; Read:
same error; Fsync: `note_completed` + error. Then sweep `active` (defensive — empty by
construction) with the same handling. Finally `wm.mark_dead()` in the spawn closure (L628):
`dead.store(true, SeqCst); drained.notify_waiters()` — parked barriers fail loud.

---

## 2. Public API (Rust pseudo-signatures)

Module-level:
```rust
pub static SIMULATE_CORRUPTION: AtomicBool;            // L24: verify_write_block reports mismatch
pub static FAIL_NEXT_WRITES: AtomicUsize;              // L29: next N write_block fail before I/O
pub fn set_simulate_corruption(val: bool);             // L32 (Relaxed store)
pub fn set_fail_next_writes(n: usize);                 // L36 (SeqCst)
pub fn set_fail_next_writes_errno(n: usize, errno: i32);// L47: injected failure carries errno (EBADE = fence class); 0 = classless
pub fn clear_fail_next_writes();                       // L51
pub fn set_test_read_stall(count: usize, ms: u64);     // L67: worker sleeps before next `count` read SQEs
pub fn set_test_read_slow(count: usize, ms: u64);      // L84: link TIMEOUT(ms, ETIME_SUCCESS) ahead of next `count` reads
pub fn set_test_write_stall(count: usize, ms: u64);    // L120: worker sleeps before next `count` write SQEs
pub fn set_test_read_timeout_ms(ms: u64);              // L150: caller read wait bound, clamp ≥1
pub type DestToken = Box<dyn Any + Send>;              // L175
pub type DestResolver = Arc<dyn Fn(u64, usize) -> Option<DestToken> + Send + Sync>; // L180
pub fn register_dest_resolver(anchor: Weak<dyn Any+Send+Sync>, resolve: DestResolver); // L201
pub fn claim_dest_token(addr: u64, len: usize) -> Option<DestToken>;  // L222
pub struct ShortReadPrefix { pub got: usize, .. }      // L412; Display = msg; impl Error
pub fn short_read_prefix_len(e: &SqueezefsError) -> Option<usize>;    // L428
pub const NODE_PROBE_TTL_MS: u64 = 1000;               // L1414
pub fn device_capacity_bytes(path: &str) -> io::Result<u64>;          // L1571
pub fn probe_direct_io(device_path: &str) -> Result<()>;              // L1581
pub fn io_lanes_for(cpus: usize, data_devices: usize) -> usize;       // L1637
pub fn write_lane_index(offset: u64, grain: u64, lanes: usize) -> usize; // L1653
```

`impl NvmeBlockDev`:
```rust
pub fn new(device_path: &str) -> Self;                 // L1658: spawn lane 0, probe write cache (sysfs once); never fails
pub fn open_checked(device_path: &str) -> Result<Self>;// L2053: probe_direct_io FIRST (loud refusal), then new(), log write_cache
pub fn zc_read_fd(&self) -> Option<RawFd>;             // L1689: lazy O_DIRECT|O_RDONLY fd for the fuse3 READ_FIXED direct leg; None cached on failure (warn once)
pub fn zc_write_fd(&self) -> Option<RawFd>;            // L1720: lazy O_DIRECT|O_WRONLY fd for the D14 WRITE_FIXED store leg
pub fn authorize_zc_store(&self) -> Result<()>;        // L1753: = fence_gate(None) — the gate for writes that bypass the worker
pub fn set_read_lanes(&self, n: usize);                // L1785
pub fn set_write_lanes(&self, n: usize, grain: u64);   // L1808
pub fn set_write_lane_grain(&self, grain: u64);        // L1829
pub fn read_lanes(&self) -> usize;                     // L1836
pub fn write_lanes(&self) -> usize;                    // L1842
pub fn write_lane_watermarks(&self) -> Vec<(u64 /*submitted*/, u64 /*completed*/)>; // L1850 (SeqCst loads, per pool lane)
pub fn set_fence_signal(&self, sig: Arc<dyn Fn() -> bool + Send + Sync>); // L1897: OnceLock set, later calls no-op
pub fn fenced(&self) -> bool;                          // L1902: = fence.halted(path) (may latch)
pub fn close_at_dismount(&self);                       // L1912: dismounted.swap(true, AcqRel); log once
pub async fn drain_write_lanes_at_dismount(&self);     // L1934: drain_write_lanes, warn on error
pub fn dismounted(&self) -> bool;                      // L1944 (Acquire)
pub fn write_cache(&self) -> WriteCacheClass;          // L2067: override (as_u8+1) else probed
pub fn override_write_cache_for_test(&self, class: Option<WriteCacheClass>); // L2080
pub fn fsync_ordinal(&self) -> Option<u32>;            // L2092
pub fn set_fsync_ordinal(&self, ordinal: u32);         // L2104: CAS u32::MAX→ordinal (first wins)
pub async fn flush(&self) -> Result<()>;               // L2135: DUR-2 barrier (§4)
pub fn lane_session_for_test(&self) -> Option<Arc<LaneSession>>; // L2242
pub fn node_exists_cached(&self) -> bool;              // L2387: TTL-cached Path::exists
pub async fn write_block_authorized(&self, offset: u64, data: Bytes, auth: CustodyEpoch) -> Result<()>; // L2413
pub async fn write_block(&self, offset: u64, data: Bytes) -> Result<()>;  // L2423
pub async fn verify_write_block(&self, offset: u64, expected: &[u8]) -> Result<bool>; // L2652
pub async fn read_block(&self, offset: u64, size: usize) -> Result<Bytes>; // L2661 = read_block_with_dest(.., None)
pub async fn probe_read_block(&self, offset: u64, size: usize) -> Result<Bytes>; // L2673: same I/O, NOT counted in get_obj
pub async fn read_block_with_dest(&self, offset: u64, size: usize, dest_addr: Option<u64>) -> Result<Bytes>; // L2683
```
There is no ranged-read / ranged-write distinct API: `read_block(offset,size)` and
`write_block(offset,Bytes)` ARE the ranged primitives (any offset/size; the device enforces
O_DIRECT alignment).

### 2.1 Buffers, alignment, O_DIRECT requirements

- Everything goes through an `O_DIRECT` fd ⇒ the kernel requires `offset`, `len` and buffer
  address aligned to the logical block size. The code enforces a **4096** contract
  (`POOLED_BUF_ALIGN = 4096`, pool.rs:28) on the buffer side only; offsets are the caller's.
- **Write fast path** (L2473): `data.as_ptr() % 4096 == 0 && data.len() % 4096 == 0` ⇒
  `WriteData::Aligned{data.clone()}` — zero-copy, the `Bytes` is the keep-alive.
- **Write bounce path** (L2496-2611): `METRICS.nvme_unaligned_write_fallbacks++`;
  `aligned_len = (len + 4095) & !4095`; if `aligned_len <= ALIGNED_BUF_POOL.buf_size()` (4 MiB,
  pool.rs:753) → `pool.alloc_raw()` + `memcpy` + zero-fill tail → `PooledUnaligned`; else
  `posix_memalign(4096, aligned_len)` (+copy, zero tail) → `Unaligned`; `posix_memalign` failure
  → `InvalidOperation("posix_memalign failed for write block")`. **The padded tail is WRITTEN to
  the device** (zero-fill), so an unaligned-length write extends to the next 4 KiB boundary
  (tests L415-417 note this). On `try_send` failure the buffer is released immediately.
- **Read, pooled** (`dest_addr=None`, L2759-2766): `size > ALIGNED_BUF_POOL.buf_size()` (4 MiB)
  ⇒ `InvalidOperation("Read size {size} exceeds pool buffer size {buf}")` (L2717). Pool pick
  `read_bounce_pool(size)`: `size <= 64 KiB` → `RANGED_BUF_POOL` (slab, fixed-buffer eligible);
  else `ZC_FILL_POOL` (memfd slab, `SQUEEZEFS_READ_ZC_SERVE` default on) or `ALIGNED_BUF_POOL`
  (pool.rs:832-838). `alloc()` returns `(ptr, Bytes::from_owner)` — the `Bytes` recycles the slot
  on last drop; the result is `bytes.slice(0..size)`.
- **Read, dest** (`dest_addr=Some(addr)`, L2740-2757): no size cap; `token =
  claim_dest_token(addr, size)`; `bytes = Bytes::from_static(slice(addr, size))` (caller owns
  registered, pinned memory). Returned `Bytes` aliases the destination.
- **Zero-length read** (L2702-2716): `size == 0` ⇒ `Ok(Bytes::new())` with no SQE. WHY: on sqz
  kernel 7.1.8 a 0-len `ReadFixed` against the registered slab oopses
  (`iov_iter_alignment_bvec` NULL deref via `io_read_fixed` → `btrfs_direct_read`), SIGKILLing the
  worker thread alone and stranding the lane.
- **ReadFixed only when** the buffer lies inside the registered RANGED slab (§1.5). Writes and
  dest reads: plain opcodes.

### 2.2 `write_block_gated` sequence (L2433-2650)

1. `FAIL_NEXT_WRITES` CAS-decrement; if taken: errno from `FAIL_NEXT_WRITES_ERRNO` (0 →
   `io::Error::other("injected write_block failure (FAIL_NEXT_WRITES)")`), returned through
   `classify_dma_outcome` (so EBADE injection latches the fence).
2. `lane = write_lane_pick(offset)`; `lane.wm.note_submitted()` BEFORE `submit` (balanced by
   `note_completed` on every terminal outcome incl. send failure).
3. `submit(Write{offset, data, tx, enq: now, trace_id: op_trace::current_op()})`; failure →
   `note_completed`, `METRICS.uring_queue_full++`, `InvalidOperation("Uring request queue full or
   closed (backpressure): …")`.
4. Await (no timeout, census site 0); `classify_dma_outcome(outcome)?`; `METRICS.put_obj++`.
5. If `write_verification_should_check()` (lib.rs:510 — enabled && every-Nth sample):
   `verify_write_block` (read back, compare; `SIMULATE_CORRUPTION` forces false) → mismatch ⇒
   `InvalidOperation("Write verification failed: checksum mismatch at offset {} on device {}")`.

### 2.3 Read sequence (`read_block_with_dest_inner`, L2695-2830)

size==0 guard → pool cap check (pooled only) → `try_lane_read` (zcrx; `None` = kernel path) →
buffer/token acquisition → `read_lane_pick()` → `submit(Read{…})` (failure: `uring_queue_full++`,
`InvalidOperation`) → `timeout(read_timeout_ms, rx)` → on `Ok(Ok(r))` `r?` → `get_obj++` iff
`count_in_get_obj` (`probe_read_block` passes false: a control-plane probe completing behind data
I/O inflated counter-window gates, the 2026-07-26 flake class). Reads are NOT fence-gated
(L1962-1964: a fenced holder reading its own device corrupts nothing).

---

## 3. Device probing

- **Capacity** (L1571-1575): `File::open(path)?.seek(SeekFrom::End(0))` — works for a regular
  file and a block device (`metadata.len()` is 0 for block devices). No BLKGETSIZE64/statx.
- **O_DIRECT probe** (L1581-1596): open `read|write|O_DIRECT`, close; failure ⇒
  `InvalidOperation("data volume {path} cannot be opened with O_DIRECT ({e}); buffered device I/O
  is not a supported data-plane mode (DUR-2: …). Use a substrate that supports direct I/O.")`.
  Called by `open_checked` (mount) so a mount fails at setup, not at first I/O.
- **Write-cache class** (`write_cache::probe_data_volume`, write_cache.rs:140-162), once in
  `new()`, never on a cadence: `fs::metadata(path)` Err → `Unknown`; `is_file()` → `FileBacked`;
  not block device → `Unknown`; else `queue_dir_for(rdev)`: `/sys/dev/block/{major}:{minor}/queue`
  if dir, else `/sys/dev/block/{major}:{minor}/../queue` (partition → parent disk), else `Unknown`;
  read `queue/write_cache`, trim: `"write back"` → `WriteBack`, `"write through"` → `WriteThrough`,
  else `Unknown`. Strings: `"write-back" | "write-through" | "file-backed" | "unknown"`;
  `is_volatile()` = `!= WriteThrough`; `as_u8`: WB=0, WT=1, FB=2, Unknown=3; `from_u8` out of
  range → Unknown. Published per device as `data_volume_write_cache: ["<path>=<class>", …]`
  (routing.rs:2985-2990). Informational only — never gates a mount; the barrier is what makes
  writes safe (a write-through device pays nothing). NVMe identify VWC bit deliberately not
  probed (needs CAP_SYS_ADMIN; sysfs honors operator override).
- **Regular file vs block device**: distinguished only by the write-cache probe above and by the
  reclaim side (block_reclaim.rs:1769-1781 `free_reclaim_op(mode)`). The I/O path treats both
  identically (O_DIRECT fd, same SQEs). File-backed substrates are where short reads happen
  (past-EOF) — real block devices are all-or-EIO (L379).
- **Node liveness** (L2387-2399): `now = coarse_monotonic_ms()` (Instant since first call);
  if `at != 0 && now - at < 1000` return cached `seen`; else `seen = Path::exists`, store
  `seen` then `at = now.max(1)` (0 = never-probed sentinel), both Relaxed; torn pairs self-heal
  within one TTL. WHY: per-call `Path::exists()` was 0.76 statx/op (L3 transport-economy).

---

## 4. Flush / barrier (`flush`, L2135-2200)

Mechanism: **io_uring `IORING_OP_FSYNC` with `IORING_FSYNC_DATASYNC`** on lane 0's O_DIRECT fd
(`UringRequest::Fsync{datasync: true}`, L2169). Never `fsync(2)` syscalls; `datasync=false` is
representable but no caller uses it. WHY (L338-345): `O_DIRECT` bypasses the page cache, NOT
the device's volatile write cache.

Sequence inside `self.sync.barrier(|| async { … })` (the metadata plane's `SyncCoalescer`,
sync_coalescer.rs — leader/follower group commit: a caller is released only by a `sync_fn`
that STARTED after it registered; `flushing` latch under a std Mutex held only for O(1) pushes;
`LeaderGuard` fails queued waiters if the leader future is dropped; the unbounded `barrier`
form is used here, not `barrier_bounded`):
1. `METRICS.data_device_sync_requests++` (outside the closure — per caller).
2. In closure: `METRICS.data_device_syncs++` (per actual device op).
3. `dev_power_cut::barrier_fault(path)` → `Some(code)` ⇒ `Err(Io(from_raw_os_error(code)))`.
4. `dev_power_cut::barrier_latency(path)` → `Some(d)` ⇒ `sqz_time::sleep(d).await`.
5. `self.drain_write_lanes().await?` (L2208-2223): snapshot EVERY pool lane's `wm.submitted`
   (SeqCst) FIRST, then sequentially `wm.wait_completed(target)` per lane. WHY (L2118-2129): a
   device flush covers only commands the device completed before processing it, and an io_uring
   Fsync never drains its own ring's in-flight SQEs (no IO_DRAIN); chosen over an IO_DRAIN Fsync
   fan-out which would order behind in-flight READS (a wedged read's 30 s class) and pay N
   flushes. Reads do NOT ride the watermark (L456-457).
6. `covered = dev_power_cut::mark_barrier_start(path)` (TEST-1 coverage frontier).
7. `self.worker.wm.note_submitted()`; `self.worker.submit(Fsync{datasync:true, tx})`; failure →
   `note_completed`, `METRICS.uring_queue_full++`, `InvalidOperation("Uring request queue full or
   closed (data-device barrier): …")`.
8. `LockWaitToken::begin(Device, site=1, 0, 0)`; `rx.await` (no timeout) → `RecvError` ⇒
   `InvalidOperation("Worker thread closed receiver during data-device barrier: …")`.
9. `self.classify_dma_outcome(outcome)?` — a barrier is a write-class command: EBADE latches the
   fence (§5).
10. `dev_power_cut::complete_barrier(path, covered)`; `Ok(())`.

`WriteWatermark::wait_completed(target)` (L509-540): loop { if `completed >= target` Ok; if
`dead` → `Err(InvalidOperation("NvmeBlockDev worker shut down during data-device barrier
drain"))`; `waiters.fetch_add(1, SeqCst)` (RAII guard decrements on drop — cancellation safe);
`notified = drained.notified_raw(); notified.enable()`; re-check both; `notified.await` }.
Completer (`note_completed`, L492-498): `completed.fetch_add(1, SeqCst)`; `if waiters.load(SeqCst)
> 0 { drained.notify_waiters() }` — Dekker pair, both SeqCst, so a completer that misses the
waiter forces the waiter to see the count.

Coalescing summary: N concurrent `flush()` callers ⇒ ≥1 and ≤ N device Fsyncs;
`data_device_sync_requests − data_device_syncs` = saved work (ratio → 1.0 means no coalescing).

W-5 fsync economy hooks (`fsync_ordinal`, `write_cache().is_volatile()`) are consumed by
routing.rs:3079-3090 (`fsync_barrier_step`): a write-through device's barrier may be skipped
when the touched-namespace lever is on. Not implemented inside `flush` itself.

---

## 5. Fence latch, custody gate, dismount close

`fence_gate(carried: Option<CustodyEpoch>)` (L1965-2001), run by `write_block`,
`write_block_authorized`, `authorize_zc_store` — NEVER by reads:
1. `if fence.halted(path) { data_custody::poison("data volume {} writer guard fenced") }`.
   `halted()` (L1466-1485): `halted.load(Relaxed)` true → true; no signal set → false; `sig()`
   true → `halted.swap(true, AcqRel)`, log error once ("writer guard FENCED / volume
   fail-stopped — refusing ALL further data-plane DMA permanently … Counted in
   data_dma_fence_refusals; remount required"), return true. Sticky until remount.
2. `data_custody::authorize_dma(carried)?` (data_custody.rs:271-299): if process `POISONED` →
   `data_dma_fence_refusals++`, `Err(WriterGuardFenced)` (errno EIO, error.rs:361); if
   `carried != current_epoch()` → `data_dma_fence_refusals++`, `data_dma_epoch_refusals++`, Err.
3. `if fence.dismounted.load(Relaxed)` → `data_dma_dismount_refusals++`, `Err(Refused{errno:
   EROFS, msg: "data volume {}: DMA refused — this mount's dismount teardown completed …
   (data_dma_dismount_refusals)"})`. Judged AFTER the fence so a fenced holder keeps its verdict.

`classify_dma_outcome(res)` (L2022-2046) — the ONE write-outcome funnel (write completion,
barrier completion, injected failure): if `Err(Io(e))` and `is_reservation_conflict(e)`
(`e.raw_os_error() == Some(EBADE)` = 52, reservation.rs:467 — the block layer's mapping of
`BLK_STS_RESV_CONFLICT`): `fence.halted.swap(true, AcqRel)` (log error once: "DEVICE rejected a
DMA with the reservation-conflict errno class (EBADE/os error 52) … writer guard FENCED … DLM
S7"), `data_custody::poison("device reservation conflict on data volume {}")`; return `res`
UNCHANGED (callers still see the errno). Every other error passes through untouched — a
transient hiccup never self-inflicts a fail-stop.

`set_fence_signal` is wired by `DataRouter::set_meta_backend` with the same per-volume `failed`
latch the reclaim queue uses (L1525-1529); bare devices (tests/offline) never halt.

Incarnation words are NOT in this file (they live above the device layer).

---

## 6. Discard / trim

Not in `nvme_dev.rs`. `src/block_reclaim.rs:1769-1845`: open target `write(true)` (no O_DIRECT),
classify by `st_mode`: regular file → `fallocate(fd, FALLOC_FL_PUNCH_HOLE|FALLOC_FL_KEEP_SIZE,
start, len)`; block device → `ioctl(fd, BLKDISCARD=0x1277, [start,len]: [u64;2])`; otherwise
Skip (counted, never a zeroing write). Plain syscalls, not io_uring (block_reclaim.rs:40). Discard
elision (`SQUEEZEFS_DISCARD_ELISION`, default on) is also reclaim-side.

---

## 7. Stats published from this file (all `crate::fuse_client::METRICS`, Relaxed `fetch_add`)

| Name | Kind | Meaning / law |
|---|---|---|
| `get_obj` | counter | data device READ ops completed through `read_block*` (incl. zcrx-served). NOT probes. The churn detector. |
| `put_obj` | counter | `write_block` completions (after `classify_dma_outcome` Ok). |
| `uring_queue_full` | counter | `try_send` refused (channel Full or Disconnected) on read, write or barrier submit. Expected 0 in healthy rows (docs/design-random-small-writes.md:46). NOTE the worker's SQ-full refusal ("Submission queue full" errors) is NOT counted here — only logged. |
| `nvme_unaligned_write_fallbacks` | counter | writes that took the bounce-copy branch. **Must stay 0 on aligned full-block workloads**; only non-4 KiB-multiple payloads (compressed/encrypted/tail) may move it (L2497-2502). |
| `dev_fills` | counter | SQEs pushed (read/write/fsync), counted before the push attempt. |
| `dev_enters` | counter | `io_uring_enter` calls: one per pass (`submit_and_wait`) + each SQ-full `submit()`. `enters ÷ fills ≪ 1` under concurrency. |
| `dev_submit_enters` | counter | passes whose enter carried ≥1 fresh fill. Test: 64 queued fills ride ≤ 4 (nvme_fill_issue_economy_tests.rs:140-146). |
| `dev_wake_batches` | counter | completion drains that resolved ≥1 request. |
| `dev_wake_writes` / `dev_wakes_elided` | counters | eventfd writes paid vs elided by the coalescer; `writes/(writes+elided) ≈ 1` on a saturated lane = coalescer stopped eliding. |
| `data_device_sync_requests` | counter | `flush()` callers. |
| `data_device_syncs` | counter | device Fsync ops actually issued (per coalesced batch). |
| `data_dma_fence_refusals` | counter (data_custody.rs) | DMA refused because poisoned/fenced or epoch mismatch. **Must stay 0** on healthy mounts. |
| `data_dma_epoch_refusals` | counter | subset: carried epoch ≠ current. **Must stay 0** on single-writer mounts. |
| `data_dma_dismount_refusals` | counter | DMA refused in the dismount class (EROFS). Moves only between teardown and process exit; **0 on a serving mount**. |
| `data_read_lanes` / `data_write_lanes` | gauges (routing.rs:3033-3038) | lanes armed at mount. |
| `data_write_lane_submits` | stats face (routing.rs:3054) | `"<path>=<s0>,<s1>,…"` per-lane `submitted` watermarks; a fan-out row is invalid unless >1 lane moved. |
| `data_volume_write_cache` | stats face (routing.rs:2985) | `"<path>=<class>"`. |
| `read_fill_phase_ns.dev_queue` | histogram | read enqueue → SQE build (channel + slot wait). Stock-kernel row measured 500 µs vs `dev_service` 285 µs before R-3 (nvme_fill_issue_economy_tests.rs:3-5). |
| `read_fill_phase_ns.dev_service` | histogram | SQE submit stamp → CQE handled. |
| `write_pipeline_phase_ns.dev_queue` / `.dev_service` | histograms | write twins (fsyncs record neither). `dma ⊇ dev_queue + dev_service`. |
| `zcrx_fills`, `zcrx_fill_bytes`, `zcrx_fill_fallbacks` (≈0 by design), `zcrx_degraded_bypasses` | counters | the zcrx lane hook (L2298-2373). |
| `aligned_pool_hits` / `aligned_pool_misses` | counters (pool.rs) | moved indirectly by bounce allocs. |
| op-trace stages `dev_submit` (32) / `dev_complete` | stamps | per request `trace_id` (0 = untraced), stamped on the worker thread. |
| `dev_dma_inflight` (port) | gauge | requests between the lane-queue push and the completion publish, every class and device: the DMAs in flight, bounded by the callers' admission (the depth governor), never by the lane threads (§1.7 port note). |
| `dev_wait_fiber_parks` / `dev_wait_raw_parks` (port) | counters | device waits that parked a fiber (the executor's futex table) vs a raw thread (the kernel futex). On a serving mount every handler wait is a fiber park. |

---

## 8. Tests — `tests/nvme_dev_tests.rs`

All use a `NamedTempFile` regular-file substrate with `set_len`; they need Linux + io_uring
(O_DIRECT open on tmpfs/ext4 — a tmpfs that refuses O_DIRECT fails every I/O). No root, no
block device. Write-submitting tests serialize on a process-global `WRITE_SERIAL` mutex because
`nvme_unaligned_write_fallbacks` is global (L11-22).

| Test | Asserts | Class |
|---|---|---|
| `test_write_block_to_offset` (L31) | 4 MiB write at offset 4 MiB lands byte-exact (verified with std File read). | Linux/io_uring, file |
| `test_read_block_from_offset` (L63) | write then `read_block` round-trips 4 MiB exactly. | same |
| `test_concurrent_stress` (L97) | 256 tasks × (write 4 MiB to one of 4 offsets + read back, len == 4 MiB) complete without error. | same |
| `test_pooled_write_sources_take_aligned_dma_branch` (L142) | `BUFFER_POOL.alloc().into_bytes()` and `ActiveBlockBuf::snapshot()` payloads are 4096-aligned and leave `nvme_unaligned_write_fallbacks` unchanged; round-trip exact. | same |
| `test_unaligned_length_write_counts_fallback` (L220) | a 123-byte write bumps the fallback counter by exactly 1 and still round-trips. | same |
| `test_write_unaligned_size` (L250) | 123-byte write at offset 4096 via posix_memalign/pool bounce round-trips. | same |
| `test_drop_device_during_unaligned_writes_joins_cleanly` (L280) | drop the last `Arc<NvmeBlockDev>` while 64 unaligned writes are in flight; each task finishes Ok or Err; total < 30 s (no hang). multi_thread 4. | same |
| `test_drop_after_unaligned_burst_does_not_hang` (L318) | 32 sequential unaligned writes then `drop` completes < 10 s. | same |
| `test_read_block_exceeds_pool_size` (L345) | `read_block(0, 5 MiB)` errs with message containing `"exceeds pool buffer size"`. | pure door check (no I/O) |
| `test_read_past_eof_errors_instead_of_recycled_garbage` (L366) | 1 MiB file; read at `file_len` of 4096 must be `Err` (kernel returns 0). | Linux, file |
| `test_read_straddling_eof_errors_instead_of_garbage_tail` (L405) | read `[file_len-2048, +4096)` must be `Err` (kernel returns 2048); file did not grow from a 4 KiB write at 0. | Linux, file |
| `read_bounce_pool_routing_and_home_recycle` (L451) | `read_bounce_pool(4096)` and `(64 KiB)` → `RANGED_BUF_POOL`; `(64 KiB+1)` and `(4 MiB)` → zc fill pool or `ALIGNED_BUF_POOL`; alloc/drop returns each pool's `allocated_bytes` exactly. | pure logic (pool) |
| `io_lanes_derivation_table` (L550) | the §1.1 table. | pure logic |
| `write_lane_affinity_is_pure_and_block_stable` (L582) | grain 4 MiB, 6 lanes: pure; every rel offset within a block maps to the block's lane; 6 consecutive blocks cover all 6 lanes; `(123,0,4)==(123,1,4)`; `(u64::MAX,grain,1)==0`; `(42,grain,0)==0`. | pure logic |
| `write_lane_pool_spreads_by_offset_and_stays_byte_exact` (L620) | default `write_lanes()==1`; `set_write_lanes(4,1 MiB)` → 4; `set_write_lanes(2,…)` stays 4; 8 writes at 1 MiB stride move each lane's `submitted` by exactly 2; two writes to block 5 move only lane 1 by 2; after `flush` all 8 blocks byte-exact. | Linux, file |
| `flush_completes_no_earlier_than_writes_on_every_lane` (L716) | 4 write lanes; `set_test_write_stall(1, 800)`; `join!(write_block(3*grain), flush())` — at flush return lane 3 shows `submitted==1` and `completed==submitted`; bytes present. multi_thread 2. | Linux, file |
| `read_lane_pool_serves_exact_bytes_and_leaves_writes_on_lane_zero` (L768) | default `read_lanes()==1`; `set_read_lanes(4)` → 4, `(2)` stays 4; 8 writes + flush; 32 concurrent `read_block_with_dest(.., None)` each exactly 1 MiB with byte parity. | Linux, file |

Sibling suites that pin this module (names only): `tests/nvme_fill_issue_economy_tests.rs`
(a fast fill completes while a slow fill holds the lane; 64-burst ≤ 4 submit enters; idle lane
wakes through the ring — `dev_wake_writes` moves), `tests/nvme_dest_ownership_tests.rs`
(timeout/dropped future keep the dest token until the late CQE; completed read releases
promptly; pooled reads claim no token), `tests/nvme_zero_len_dma_tests.rs` (size 0 → empty, no
device op), `tests/backend_health_probe_tests.rs` (TTL cache; probe reads never count in
`get_obj`).

---

## 9. Constants and measured numbers

| Value | Where | WHY (from source) |
|---|---|---|
| `URING_REQ_QUEUE_CAP = 4096` | L606 | "P1-6: bound io_uring request queue to apply backpressure under overload." |
| ring entries `1024` | L771, L925 | ring depth per lane (no comment beyond the debug log). |
| `UD_RESERVED = u64::MAX-1`, `UD_SEAM = u64::MAX-1`, `UD_WAKE = u64::MAX` | L129-131 | "Slot indices are small; anything ≥ UD_RESERVED is never a request." |
| read timeout default `30_000` ms, clamp ≥ 1 | L133-147 | "a wedged device/worker must not freeze the whole FUSE session"; tests shorten it. |
| push retry count `3` | L1231 | SQ-full: submit then retry. |
| `NODE_PROBE_TTL_MS = 1000` | L1414 | per-call probe was 0.76 statx/op (L3 transport-economy regression). |
| alignment `4096` (`POOLED_BUF_ALIGN`), `aligned_len = (n+4095)&!4095` | L2462, L2510 | O_DIRECT / pooled-buffer contract. |
| `ALIGNED_BUF_POOL` slot 4 MiB, cap `max(cores×16, 64)` | pool.rs:319-321, 750-754 | read size cap; "pinning cores×16×4 MiB (GiBs)" is why it is not registered (L816-818). |
| `RANGED_BUF_SIZE = 64 KiB`, cap `max(cores×64, 512)` | pool.rs:326-327, 813 | 4 KiB ranged reads checking out 4 MiB backings = 6 ms/op convoy (THP-zeroing + TLB storm); ≈ 92 MiB on a 23-CPU box. |
| wake landing buffer 8 B leaked once per lane | L936 | ring teardown cancels the eventfd read asynchronously. |
| eventfd write value `1u64` | L589 | level-triggered counter. |
| `EBADE` (52) | reservation.rs:462-469 | block layer's `BLK_STS_RESV_CONFLICT` mapping. |
| `EROFS` | L1991 | dismount-class refusal errno. |
| measured: 2.7 GB/s/core RX copy; 22 GB/s read wall; 11.5→12.1 GB/s (qd16→32, 1 submitter); 16.7 GB/s (4 submitters); 17.7 (8); 35.31 GB/s = 7.06×5 write wall; fio 49.7 GB/s; dma mode 2–8 ms | L1609-1624, tests L525-538 | lane fan-out rationale. |
| measured: `dev_queue` 500 µs vs `dev_service` 285 µs (stock kernel, pre-R-3) | nvme_fill_issue_economy_tests.rs:3-5 | arrival-wake-in-ring rationale. |
| measured: read timeout cost ≤ 3 % daemon CPU, < 0.2 % clat | AGENTS.md:922 | per-read timeout retained. |
| test numbers: stall 800 ms (flush test), 150 ms (burst), ≤ 4 enters for 64 fills, drop < 30 s / < 10 s | tests | — |

---

## 10. Concurrency details

- **Channel**: crossbeam bounded MPSC-style (many producers, one consumer); producers use
  `try_send` only (never block); the consumer uses `try_recv` only (never blocks) — the single
  park is `submit_and_wait(1)` on the ring.
- **Ring ownership**: one thread per ring; the thread that builds the ring is its only submitter
  (`SINGLE_ISSUER` is free by construction, L701-703). `DEFER_TASKRUN` means completions are
  only reaped inside this thread's own `io_uring_enter` with GETEVENTS — hence the pass-bottom
  `submit_and_wait` is "the DEFER_TASKRUN-correct reap" (L1245) and SQ-full mid-pass uses plain
  `submit()`.
- **Buffer lifetime across DMA**: the worker owns everything the SQE borrows until the CQE:
  Read — the pooled `Bytes` (recycle-on-drop owner) or the dest `Bytes::from_static` plus the
  `DestToken`; Write — `_keep_alive: Option<Bytes>` for Aligned, raw `free_ptr` for bounce
  buffers. A caller timing out or dropping its future does NOT release anything; the worker's
  slot does, at CQE or teardown. `SendPtr(*mut u8)` is `unsafe impl Send + Sync` (L235-237).
- **DestToken registry** (L190-233): `ArcSwap<Vec<Arc<DestResolverEntry{anchor: Weak<dyn Any>,
  resolve}>>>`; writers serialize on a static `Mutex<()>`, prune entries whose
  `anchor.strong_count()==0`, push, `store`. `claim_dest_token` is lock-free (`load`, skip dead
  anchors, first `Some` wins). `None` for pooled reads and for dests with no registered owner
  (IPC-arena dests have their own lifetime machinery).
- **Atomics & orderings**:
  - `WriteWatermark.submitted/completed/waiters/dead`: all **SeqCst** (Dekker with the
    `Notify`); `submitted` bumped on the CALLER thread before `try_send`; `completed` on the
    WORKER thread (authoritative) or on the caller for send failures.
  - `WakeCoalescer.armed`: `swap` **SeqCst** both sides.
  - `DeviceFence.halted`: `load Relaxed` fast path, `swap AcqRel` latch; `dismounted`: `swap
    AcqRel`, `load Relaxed` in the gate, `load Acquire` in `dismounted()`; `signal: OnceLock`.
  - `read_lanes/write_lanes`: `fetch_max Relaxed`, `load Relaxed`; `write_grain`:
    `store/load Relaxed`; `lane_rr`: `fetch_add Relaxed`; `lane_pool`: `ArcSwap`
    `load`/`load_full`/`compare_and_swap`.
  - `write_cache_override`: `AtomicU8` Relaxed; `fsync_ordinal`: `compare_exchange(u32::MAX,
    n, AcqRel, Relaxed)`, `load Relaxed`.
  - `NodeProbe.at_ms/seen`: Relaxed (racing expirers both re-probe; idempotent).
  - Test seams `FAIL_NEXT_WRITES*`, `STALL_*`, `SLOW_*`: `SeqCst` CAS-decrement loops;
    `SIMULATE_CORRUPTION`: Relaxed; `read_timeout_ms` cell: Relaxed.
  - Lane numbering `NEXT_LANE`: Relaxed `fetch_add`.
- **Locks**: `SyncCoalescer.inner: std::Mutex` (O(1) critical sections); dest-resolver writer
  `Mutex<()>`; nothing else. No lock is held across an await or a syscall.
- **Clone sharing**: every clone of `NvmeBlockDev` shares the worker pool, fence, coalescer,
  node probe, override cells, lane session and zc fds (all `Arc`). The worker threads die when
  the LAST clone drops (Arc<UringWorker> count → 0 → `Drop` join).
- **Barrier vs lanes**: `drain_write_lanes` snapshots targets before waiting, so writes racing in
  behind the snapshot never extend the wait; a lane whose worker died fails the barrier
  (`dead`) — never a false completion.
- **Per-offset ordering**: within one lane the channel FIFO + single pump gives submission order
  for equal offsets; across lanes the affinity hash guarantees equal offsets share a lane.
  Completion order is unordered everywhere (no IO_LINK/IO_DRAIN).

---

## 11. Env knobs read by / for this module (`src/env_knobs.rs`)

| Name | Kind | Range | Default | Purpose | Read at |
|---|---|---|---|---|---|
| `SQUEEZEFS_NVME_READ_LANES` | Int | 1..=1024 | derived `io_lanes_for(cpus, data devices)` | per-device READ fan-out lanes; `1` = single-worker A/B posture | routing.rs:3015 (`opt_int_knob`) |
| `SQUEEZEFS_NVME_WRITE_LANES` | Int | 1..=1024 | derived (same) | per-device DATA-WRITE lanes, block-offset affinity; `1` = pre-fanout A/B | routing.rs:3021 |
| `SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS` | Int | 1..=600_000 (`MS_MAX`) | 30000 | test seam: caller read wait bound | L141 (raw `std::env::var`, trimmed parse, `unwrap_or(30_000)`, `.max(1)`) |
| `SQUEEZEFS_TEST_POWER_CUT_DEVS` | Str | — | none | comma list of device paths armed for the power-cut simulator | dev_power_cut.rs:126, once per worker thread start |
| `SQUEEZEFS_ZCRX_LANE` | Bool | — | off | arm the zcrx userspace read lane (hook at L2252) | zcrx_lane/mod.rs:116 |
| `SQUEEZEFS_READ_ZC_SERVE` | Bool | — | on | whole-block read fills ride the memfd `ZC_FILL_POOL` (affects `read_bounce_pool`) | pool.rs:774 |

Convention (env_knobs.rs:10-27): absent = unset/empty/whitespace; malformed or out-of-range
refuses the PROCESS at startup naming every offending knob; unknown `SQUEEZEFS_*` names are
announced not refused; explicit value wins verbatim over derived.

---

## 12. Error strings (callers and tests match on these)

- `"Read size {size} exceeds pool buffer size {buf}"` (InvalidOperation) — test matches
  `"exceeds pool buffer size"`.
- `"short read: kernel returned {got} of {size} bytes at offset {offset} (past-EOF/short reads
  must not surface recycled buffer bytes)"` (Io UnexpectedEof + `ShortReadPrefix{got}`).
- `"Submission queue full"` (Io Other, from the worker on SQ-full after 3 retries).
- `"Uring request queue full or closed (backpressure): {e:?}"` / `"…(data-device barrier): …"`
  (InvalidOperation, caller side `try_send` failure).
- `"NvmeBlockDev read timed out after {ms} ms (offset={}, size={})"` (Io TimedOut).
- `"Worker thread closed receiver: {e:?}"` / `"… during data-device barrier: …"` (InvalidOperation).
- `"NvmeBlockDev worker shutting down"` (InvalidOperation, teardown drain).
- `"NvmeBlockDev worker shut down during data-device barrier drain"` (InvalidOperation).
- `"posix_memalign failed for write block"` (InvalidOperation).
- `"Write verification failed: checksum mismatch at offset {} on device {}"` (InvalidOperation).
- `"injected write_block failure (FAIL_NEXT_WRITES)"` (Io Other) when errno seam is 0.
- Dismount refusal: `Refused{errno: EROFS, …}`; fence/custody: `WriterGuardFenced` (→ EIO).
