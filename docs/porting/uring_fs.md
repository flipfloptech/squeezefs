# PORTING SPEC — `uring_fs` (pooled io_uring file layer) + the O_DIRECT metadata posture

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

Reference: `/Users/joberdorf/Source/squeezefs/src/uring_fs.rs` (3036 lines; the posture lives IN this file,
lines 542–1080), `tests/uring_fs_tests.rs`, `tests/meta_io_direct_tests.rs`, pad law in
`src/meta_backend/kv/journal_core.rs` + `journal.rs`. All `file:line` cites are into that tree.

---

## 0. Scope in one paragraph

A process-global pool of N worker threads, each owning ONE io_uring, consuming ONE shared bounded MPMC queue
of `FsReq` messages. Callers push a request with a oneshot reply channel, `await` the oneshot. Each worker
burst-drains the queue, builds one SQE per op (or per batch entry), `submit_and_wait(1)`, reaps CQEs,
resubmits short reads/writes from the completion handler, and sends outcomes on the oneshots. A second,
OWNED-ring mode (`OwnedRing`/`OwnedReactor`) reuses the same `Reactor` state machine but is driven by the
thread that also awaits completions (the journal lane) and parks in `io_uring_enter` with an eventfd READ SQE
armed. A per-path registry (`META_IO_MODES`) says whether a path is a registered metadata device and, if so,
its O_DIRECT grain; the worker gives every shape its aligned form (reads widened, writes refused if
misaligned, buffers bounced). A fault-injection shim (test-only, one relaxed atomic load when disarmed) sits
at admission. No fixed-file registration, no registered buffers, no SQPOLL, no SQE128/DEFER_TASKRUN/
SINGLE_ISSUER — plain `IoUring::new(entries)` (uring_fs.rs:2425, :2623).

---

## 1. Worker architecture (the pool)

### 1.1 Threads
- Count: `worker_count()` (uring_fs.rs:183–188) = `resolve_worker_count(env SQUEEZEFS_URING_FS_WORKERS, cpu::process_parallelism())`.
- `resolve_worker_count` (:201–208): env parses as usize → `clamp(1, 64)` (junk ⇒ fall through to derived);
  derived = `(cpus / 4).clamp(4, 64)`. Pinned: `(None,32)→8`, `(None,2)→4`, `(None,64)→16`, `(None,512)→64`,
  `(Some("2"),32)→2`, `(Some("999"),32)→64`, `(Some("junk"),32)→8` (derivation_sweep_tests.rs:921–928).
  WHY (:190–200): cpus/4 is "the measured drain-thread slope"; floor 4 = shipped floor; ceiling 64 = env-clamp
  parity; `cpus` MUST be the process affinity mask ÷ fleet share (`cpu::process_parallelism`, cpu.rs:66),
  never the calling thread's `available_parallelism()` ("Hang-1 pinned-first-toucher sizing poison").
- Thread names: `squeezefs-uring-fs-{i}` for i in 0..count (:164).
- Spawned lazily on first use of the global `URING_FS: Lazy<Arc<UringFsWorker>>` (:220).
- Test-only extra thread `sqz-uringfs-latency` (latency lane, :1210), spawned on first `arm_device_latency`.

### 1.2 Queue (caller → worker)
- `crossbeam::channel::bounded(URING_FS_QUEUE_CAP)` with `URING_FS_QUEUE_CAP = 4096` (:45), MPMC, all workers
  `rx.clone()` (:162). WHY (:39–44): "backpressure, not custody… small request structs; payload bytes live
  with the caller until service. Same posture as `nvme_dev::URING_REQ_QUEUE_CAP`" (also 4096, nvme_dev.rs:606).
- Callers use `try_send` — never block. Full ⇒ `queue_full_err` (:222–227): increments
  `fuse_client::METRICS.uring_queue_full` (Relaxed) and returns
  `SqueezefsError::InvalidOperation("uring-fs queue full: {e:?}")` (maps to EINVAL via `to_errno`, error.rs:355).
- Only the test latency lane uses a BLOCKING `send` (:1239), because it is the sole producer allowed to wait.
- Wake mechanism: crossbeam's internal futex/condvar on `rx.recv()` — the worker blocks in `recv()` only when it
  has zero ops in flight (:2439–2446).

### 1.3 Reply (worker → caller)
- `squeezefs_ipc::sqz_channel::oneshot` (sqz_channel.rs:62–200): `Arc<Mutex<{value, closed, rx_gone, waker}>>`.
  `send` stores value, sets `closed`, wakes stored waker; Sender `Drop` sets `closed` (receiver sees
  `RecvError` ⇒ `worker_closed_err`); Receiver `Drop` sets `rx_gone` (send returns `Err(value)`, ignored).
  `try_recv` → `Empty | Closed | Ok`. Writes reply with `WriteOutcome { result, admitted_at, woken_at }`
  (:83–94); reads with `Result<Bytes>`; fdatasync with `Result<()>`.

### 1.4 Request enum `FsReq` (:109–146)
```
WriteAll     { path, data: Bytes, tx: WriteTx }            // create|truncate whole file (control path, buffered)
ReadAll      { path, tx: oneshot<Result<Bytes>> }
ReadAt       { path, offset: u64, size: usize, tx }
WriteAt      { path, offset: u64, data: Bytes, tx: WriteTx }
WriteAtBatch { path, ops: Vec<(u64, Bytes)>, tx: WriteTx } // one logical commit, N parallel SQEs, one reply
Fdatasync    { path, tx: oneshot<Result<()>> }
Delayed(Box<FsReq>)                                        // test latency-lane re-admission marker
```

### 1.5 Ring setup
- `io_uring::IoUring::new(RING_ENTRIES)` with `RING_ENTRIES: u32 = 512` (:47, :2425). Default builder: no setup
  flags; CQ = 2×SQ (io-uring 0.6 default). If creation fails ⇒ `log::error!` and the worker runs
  `blocking_fallback_loop` for the process lifetime (:2426–2432).
- `ADMIT_CAP: usize = 256` (:50) — max ops a worker admits/keeps in flight. WHY: "continuations reuse their
  slot, so SQ pressure is bounded by this plus one burst of resubmits."
- Owned ring (journal lane): `JOURNAL_RING_ENTRIES: u32 = 512` (journal_lane.rs:51) — "a window's entries
  (≤ the commit batch cap, each one or two segment SQEs plus its owned page headers) plus the wake READ".

### 1.6 Reactor state (:1971–1977)
```
struct Reactor { ring: IoUring, slots: Vec<Option<Pending>>, free: Vec<usize>, inflight: usize, cache: FdCache }
```
- `user_data` of every SQE = slot index (`i as u64`) (:2023, :2038, :2042). Reserved sentinels:
  `WAKE_UD = u64::MAX` (:2483), `SEAM_TIMEOUT_UD = u64::MAX - 1` (:2480).
- `claim_slot` (:1990): `inflight += 1`; pop `free` else push new slot. `release_slot` (:2001):
  `inflight -= 1`, push to `free`, `take()` the Pending.
- `Pending` (:1835–1864) — one in-flight op, OWNS everything the SQE references:
```
Read  { file: Rc<File>, buf: AlignedBuf, file_offset: u64, filled: usize, skip: usize, want: usize, grain: usize, tx }
Write { file: Rc<File>, data: Bytes, file_offset: u64, written: usize, done: UnitDone }
Fsync { file: Rc<File>, tx }
```
  "Holds its own `Rc<File>` so fd-cache eviction can never close a file with an outstanding SQE" (:1833).
- `UnitDone` (:1781–1787): `Single { tx, admitted_at }` or `Batch(Rc<RefCell<BatchState>>)`;
  `BatchState { remaining, first_err: Option<Error>, tx: Option<WriteTx>, admitted_at }` (:1789–1794).
  `complete(res, now)`: Single sends outcome; Batch records first error, decrements `remaining`, and when 0
  sends `first_err.map_or(Ok)` with `woken_at = now` (:1796–1830). Any failure fails the whole batch.

### 1.7 SQE construction (`sqe_for`, :2008–2044)
- Read: `opcode::Read(Fd(raw), buf.ptr + filled, (total - filled) as u32).offset(file_offset + filled)`.
- Write: `opcode::Write(Fd(raw), data.ptr + written, (len - written) as u32).offset(file_offset + written)`.
- Fsync: `opcode::Fsync(Fd(raw)).flags(FsyncFlags::DATASYNC)` — all "fdatasync" are `IORING_FSYNC_DATASYNC`.
- NO fixed files, NO registered buffers, NO IOSQE_ASYNC.

### 1.8 Push + SQ-full handling (`push_slot_flags`, :2058–2081)
```
sqe = sqe_for(i).flags(flags)
res = sq.push(sqe)                       // Err when SQ full
if res.is_err() && ring.submit().is_ok(): res = sq.push(sqe)   // flush SQ to kernel, retry once
if res.is_err(): release_slot(i); complete with Io(other("uring-fs submission queue overflow"))
```
WHY (:2046–2051): "A full SQ is normal backpressure (a single `WriteAtBatch` may carry more entries than the
ring): flush… with `submit()` — which consumes SQ entries immediately, independent of completions — and push
again. Only a push that fails right after a successful flush is a real error." Pinned by
`test_write_at_batch_larger_than_ring_completes` (600 entries > 512 SQ).
- Continuation push (`push_slot_continue`, :2542–2564) does NOT retry-after-submit (the slot was just
  reaped, so an SQ slot is free); on failure releases the slot with "…overflow (continuation)".

### 1.9 Main loop (`worker_loop`, :2424–2475)
```
loop:
  if inflight == 0: if disconnected return; match rx.recv() { Ok(req)=>admit, Err=>return }
  while inflight < ADMIT_CAP && !disconnected:
      match rx.try_recv() { Ok=>admit, Empty=>break, Disconnected=>disconnected=true }
  if inflight == 0: continue
  match ring.submit_and_wait(1) {       // io_uring_enter(to_submit, min_complete=1, GETEVENTS)
      Ok => {}, Err(Interrupted) => continue,
      Err(e) => { log::error!; fail_all("uring-fs ring failed"); blocking_fallback_loop(rx); return }
  }
  reap()
```
- Submission batching = everything admitted in the burst goes in one `submit_and_wait`.
- Parking = blocking `recv()` when idle, else `io_uring_enter` wait. No eventfd in the pool mode.
- `reap` (:2491–2517): `cq.sync()`, collect all `(user_data, result)`, drop CQ borrow, ONE `Instant::now()`
  per pass ("ufs_wake"), `advance` each; collect slots needing resubmit; after the CQ borrow ends push each
  continuation. Returns `true` if `WAKE_UD` was seen (owned mode).
- `fail_all(what)` (:2520–2538): release every occupied slot, complete with `Io(other(what))`.

### 1.10 Admission (`admit`, :2162–2317)
1. `admitted_at = Instant::now()` BEFORE the open ("a cold fd-cache miss counts against the device span").
2. Fault shim: `if FAULTS_ACTIVE.load(Relaxed) { fault_intercept(req)? } else { unwrap_delayed(req).0 }`.
3. Per variant:
   - `WriteAll`: `create_dir_all(parent)`; open `O_RDWR|O_CREAT|O_TRUNC|O_CLOEXEC` (NEVER O_DIRECT — "control
     path's (config records, never a metadata device): buffered posture"); insert into cache with
     `direct=false`; one `Pending::Write` at offset 0.
   - `ReadAll`: open `O_RDONLY|O_CLOEXEC` (not cached), `len = metadata().len()`, `AlignedBuf::zeroed(len, 1)`,
     `Pending::Read { skip:0, want:len, grain:1 }`; `len==0` ⇒ reply `Ok(empty)` immediately.
   - `ReadAt`: `cached_open(path, create=true)`; `read_span` → `(span_off, span_len, skip, grain)`;
     `buf = read_buf_for(path, span_len)`; `size==0` ⇒ reply empty immediately.
   - `WriteAt`: `cached_open(create=true)`; empty data ⇒ `Ok` now; `prepare_direct_write` (may refuse or
     bounce); one `Pending::Write`.
   - `WriteAtBatch`: `admit_write_batch(path, ops, tx, admitted_at, None)`.
   - `Fdatasync`: `cached_open(create=false)`; `Pending::Fsync`.
   Open/preflight failures reply via `send_now` (both stamps = now, :100–107).
- `admit_write_batch` (:2092–2158): open once; drop empty ops; all-empty ⇒ `Ok` now; run
  `prepare_direct_write` on EVERY op before any SQE ("a misaligned op fails the whole logical commit before
  any byte is submitted"); one `BatchState`; one slot+SQE per op, all sharing `file.clone()`. With
  `link_after=Some(ts)` (owned ring only): first push a `Timeout(ts).flags(ETIME_SUCCESS)` SQE with
  `IOSQE_IO_LINK` and `user_data=SEAM_TIMEOUT_UD`, then every op except the last carries `IO_LINK`.

### 1.11 Completion advance (`advance`, :2323–2422)
- Read: `res<0` ⇒ Done(error); `res==0` ⇒ Done (EOF, truncate); else `filled += res`; resubmit iff
  `filled < buf.len() && filled % grain == 0` ("A direct continuation must resume on the grain; a short read
  that ends off it is the file's end"). On Done: `end = min(filled, skip+want)`;
  `out = if end <= skip { empty } else { buf.into_bytes().slice(skip..end) }` — refcounted slice, no copy.
- Write: `res<=0` ⇒ Done; else `written += res`; resubmit iff `written < data.len()`. On Done: `res<0` ⇒
  `Io(from_raw_os_error(-res))`; `res==0` ⇒ `Io(WriteZero, "uring write returned 0")`; else `Ok`.
- Fsync: always Done; `res<0` ⇒ errno error else `Ok`.
- Error mapping everywhere: negative CQE res ⇒ `std::io::Error::from_raw_os_error(-res)` wrapped as
  `SqueezefsError::Io` (`map_io`, :538). `to_errno` returns the raw errno verbatim (error.rs:342).

### 1.12 Per-worker fd cache (`FdCache`, :1866–1919)
- `HashMap<PathBuf, (Rc<File>, lru_gen: u64, direct: bool)>`, `gen: u64` monotonic.
- `touch(path, direct)`: bump gen; hit with same posture ⇒ stamp + clone; posture MISMATCH ⇒ remove entry,
  return None ("a path registered AFTER its first (buffered) open is re-opened direct at its next use").
- `insert`: if `len >= fd_cache_cap()` evict min-gen entry (in-flight ops hold own Rc, so no busy fd closes).
- `fd_cache_cap()` (:60–71) = `fd_cache_cap_from(cpu::nofile_soft_as_found(), worker_count())`
  = `(soft / 4 / max(workers,1)).clamp(16, 1024)`. Pinned: `(1024,8)→32`, `(524288,8)→1024`, `(0,8)→16`,
  `(1<<40,1)→1024` (nofile_budget_tests.rs:32–35). WHY (:51–59): pool aggregate stays under a quarter of
  RLIMIT_NOFILE "as found at startup" (not the raised limit — raise serves listener caps alone).
- `cached_open` (:1923–1940): `direct = meta_io_mode(path).is_some_and(|m| m.direct)`; open
  `O_RDWR | O_CLOEXEC | (O_DIRECT if direct)`, `create` per arg (no truncate). WHY O_RDWR not O_WRONLY: "A
  write-only cached fd makes io_uring Read return EBADF". Open happens inline on the worker (blocking, rare).

### 1.13 Shutdown (:210–218)
`Drop for UringFsWorker`: `drop(tx)` so every worker's `recv()` errors; join all threads. Workers with in-flight
ops finish them first (loop only returns when `inflight==0 && disconnected`). (In practice the global `Lazy`
is never dropped.)

### 1.14 Blocking fallback (`blocking_fallback_loop`, :2822–2950)
Same queue, same fault shim, std `pread/pwrite`. ReadAt honours `read_span`/`O_DIRECT`/grain-continuation
rule; WriteAt/Batch honour `prepare_direct_write` and open with `O_DIRECT` when direct; Fdatasync opens
`O_WRONLY` and `sync_data`. Used when ring creation fails or the ring breaks.

---

## 2. Public API (Rust pseudocode; all `path: impl AsRef<Path>`)

| Signature | Purpose / cite |
|---|---|
| `async fn write_all(path, data: impl Into<Bytes>) -> Result<()>` | create/truncate whole file; buffered; `create_dir_all(parent)` (:234) |
| `async fn read_all(path) -> Result<Bytes>` | whole file, length from `metadata()` (:248) |
| `async fn fdatasync(path) -> Result<()>` | IORING_FSYNC_DATASYNC on cached O_RDWR fd (:261) |
| `async fn read_at(path, offset: u64, size: usize) -> Result<Bytes>` | positional read; direct paths widened+sliced (:274) |
| `async fn write_at(path, offset: u64, data: impl Into<Bytes>) -> Result<()>` | positional write; direct preflight (:289) |
| `fn submit_write_at_batch(path, ops: Vec<(u64,Bytes)>) -> Result<WriteCompletion>` | push one message, return awaitable handle; 0 ops ⇒ already done; 1 op ⇒ sent as `WriteAt` (:490) |
| `async fn write_at_batch(path, ops) -> Result<()>` | `submit_write_at_batch(..)?.wait().await` (:534) |
| `struct WriteCompletion { rx, submitted_at, done: Option<(Result<()>, WriteStamps)> }` | `poll_done(&mut) -> bool` (non-blocking; `Closed` ⇒ worker-closed error retained), `submitted_at()`, `async wait(self) -> Result<()>`, `async wait_stamped(self) -> (Result<()>, WriteStamps)` (:413–483) |
| `struct WriteOutcome { result, admitted_at, woken_at }` / `struct WriteStamps { submitted_at, admitted_at, woken_at, observed_at }` + `WriteStamps::stamp(&TracedBatch)` | hop decomposition payload (:83, :368) |
| `enum UringFsWritePhase { QueueHop=0, Device=1, WakeHop=2, Total=3 }`; `fn uring_fs_write_phase_json() -> Value`; `fn uring_fs_write_phase_totals(phase) -> (sum_ns, count)` | stats (:334–363) |
| `fn resolve_worker_count(env: Option<&str>, cpus: usize) -> usize` | pure sizing (:201) |
| `fn fd_cache_cap_from(nofile_soft: usize, workers: usize) -> usize`; `fn fd_cache_cap_in_force() -> (cap, workers)` | pure cache cap + live face (:69, :75) |
| `const QUEUE_CAP: usize = 4096` | exposed for tests (:2953) |
| **Posture** | |
| `struct MetaIoMode { grain: u64, mem_align: usize, direct: bool }`; `MetaIoMode::BUFFERED = {1,1,false}` | (:583–603) |
| `fn register_meta_device(path: &Path) -> Result<MetaIoMode>` | probe grain, trial O_DIRECT open, record posture; idempotent (:790) |
| `fn meta_io_mode(path: &Path) -> Option<MetaIoMode>` | registry lookup, one load + one hash probe; first miss canonicalizes once (:688) |
| `fn meta_io_grain(path: &Path) -> u64` | `mode.map_or(1, grain)` (:705) |
| `fn admit_meta_io_grain(path, source: &str, grain: u64) -> Result<u64>` | the one grain law (:770) |
| `fn clear_meta_devices()` | registry reset (test harness) (:903) |
| `fn pad_to_grain(path, image: Vec<u8>) -> Bytes` | zero-extend length to grain multiple (:1026) |
| `async fn patch_at(path, offset, data) -> Result<()>` | HARNESS byte plant via RMW of the covering span; never a product path (:1044) |
| `fn meta_io_stats_json() -> Value` | the `meta_io` stats object (:1071) |
| `struct AlignedBuf`: `zeroed(len, align)`, `from_slice(&[u8], align)`, `len()`, `is_empty()`, `as_mut_slice()`, `into_bytes() -> Bytes` (zero-copy via `Bytes::from_owner`), `ptr_aligned(ptr, align) -> bool`; `AsRef<[u8]>`; `Send+Sync` | (:909–986) |
| statics: `META_IO_DIRECT_PATHS`, `META_IO_BUFFERED_FALLBACK`, `META_IO_BOUNCE_BYTES`, `META_IO_UNALIGNED_REFUSALS`, `META_IO_READ_WIDENED`, `META_IO_MODE_RESOLVES: AtomicU64`; `META_IO_DEFAULT_GRAIN: u64 = 4096` | (:623–676) |
| **Owned ring** | |
| `struct OwnedRing`: `new(entries: u32) -> io::Result<Self>`, `wake_handle() -> RingWake`, `into_reactor(self) -> OwnedReactor` | ring + eventfd(0, EFD_CLOEXEC\|EFD_NONBLOCK) (:2615–2651) |
| `struct RingWake: Clone`: `wake(&self)` | `write(eventfd, &1u64, 8)` (:2590–2610) |
| `struct OwnedReactor`: `submit_write_at_batch(&mut, path, ops) -> WriteCompletion` (admit + `submit()`, no wait), `park(&mut, tick: Duration) -> bool` (true = tick fired), `service(&mut)` (reap) | (:2719–2818) |
| **Fault shim (test seams)** | `arm_torn_write(offset, keep)`, `arm_sector_write_error(offset)`, `arm_power_cut(path)`, `power_cut(path) -> usize`, `arm_barrier_error(path, errno)`, `disarm_barrier_error(path)`, `arm_barrier_stall(path) -> UnboundedReceiver<()>`, `release_barrier_stall(path)`, `arm_write_stall(path, offset, len) -> UnboundedReceiver<()>`, `release_write_stall(path)`, `arm_device_latency(path, write: Duration, barrier: Duration)`, `disarm_device_latency(path)`, `clear_faults()`, `static TORN_WRITE_FAULT` (:1095–1523) |

No `fsync` (non-data), `fallocate`, `statx`, `readv/writev` exist in this module.

---

## 3. The O_DIRECT metadata posture

### 3.1 Registry
- `META_IO_MODES: ArcSwap<HashMap<PathBuf, Option<MetaIoMode>>>` (:616). Keys: path as given AND canonical
  spelling; value `None` = remembered NEGATIVE. WHY ArcSwap (:609–615): hot path pays "ONE lock-free load and
  one hash probe"; the RwLock form "cost three reader acquisitions and a `canonicalize(2)` per MISS per I/O".
- `meta_io_mode` (:688–700): load; hit ⇒ return (positive or negative). Miss ⇒ `canonicalize(path)` → probe
  with canonical → `META_IO_MODE_RESOLVES += 1` → `remember_meta_io_mode(path, resolved)`.
- `remember_meta_io_mode` (:634–644): RCU; no-op if already known OR (`mode.is_none()` and
  `len >= meta_io_negative_cap()`), where negative cap = `fd_cache_cap()` (:629).
- `register_meta_device` RCU (:884–896): REBUILD keeping only `Some` entries (drop every negative — an alias
  spelling seen before registration must not stay BUFFERED forever), insert `path` and `canonical` as `Some(mode)`.
- `clear_meta_devices`: store empty map.

### 3.2 Grain probe ladder (`register_meta_device`, :808–822)
1. `statx(AT_FDCWD, path, AT_STATX_SYNC_AS_STAT, STATX_DIOALIGN)` (:711–733). Valid iff `rc==0 &&
   (stx_mask & STATX_DIOALIGN) != 0 && stx_dio_offset_align != 0`. Result
   `(grain = stx_dio_offset_align as u64, mem_align = max(stx_dio_mem_align, 1) as usize)`, source string
   `"statx(STATX_DIOALIGN)"`.
2. Else `sysfs_logical_block_size` (:738–762): `fs::metadata(path)`; must be a block device
   (else None); `rdev → (major(rdev), minor(rdev))`; `dev = /sys/dev/block/{major}:{minor}`; try, in order,
   `dev/queue/logical_block_size` then `dev/../queue/logical_block_size` (partition ⇒ parent disk);
   `read_to_string → trim → parse u64`, returned VERBATIM (no clamp — "whether the ring's page arithmetic can
   honour it is `admit_meta_io_grain`'s verdict"). Source `"sysfs logical_block_size"`, mem_align = grain.
3. Else `(META_IO_DEFAULT_GRAIN=4096, 4096)`, source `"the default"`. WHY (:673–676): "the largest logical
   block size in the field (4Kn), so an unknown device is never under-aligned."
4. `grain = admit_meta_io_grain(path, source, grain)?` (:770–782): OK iff `grain.is_power_of_two() &&
   grain <= 4096` (so 0, 3072, 520, 8192… refuse). Refusal text MUST contain path display, source, `"{grain} B"`,
   `"REFUSING"`, and must NOT contain `"clamp"` (tested). WHY: "a wider or odd grain clamped to the default
   would leave every write under-aligned, each then EINVAL with a misattributed message".
5. `mem_align = max(mem_align, grain).next_power_of_two()` (:822).

### 3.3 Registration flow (:790–897)
```
if let Some(m) = meta_io_mode(path) return Ok(m)                  // idempotent
md = metadata(path) else Err(Io("metadata device {p}: cannot stat: {e}"))
is_bdev = is_block_device; if !is_bdev && !is_file → Err(InvalidOperation("...neither a block device nor a regular file"))
(source, grain, mem_align) = ladder; grain = admit(..)?; mem_align = ...
mode = if bool_knob("SQUEEZEFS_TEST_META_BUFFERED", false):
          warn!; META_IO_BUFFERED_FALLBACK += 1; BUFFERED
       else match open(path, O_RDWR | O_CLOEXEC | O_DIRECT):          // the TRIAL open; fd dropped at once
          Ok(_)  → META_IO_DIRECT_PATHS += 1; info!("...: O_DIRECT (grain {g} B, buffer alignment {m} B) ..."); {grain, mem_align, direct:true}
          Err(e) if e.raw_os_error()==Some(EINVAL) && !is_bdev → META_IO_BUFFERED_FALLBACK += 1; warn!("metadata volume {p}: the filesystem refuses O_DIRECT ({e}) — falling back to BUFFERED ... meta_io_buffered_fallback counts it"); BUFFERED
          Err(e) → Err(Io(kind, "metadata device {p}: O_DIRECT open failed ({e}); shared-LUN metadata I/O must bypass the host page cache (design-symmetric-metadata §5.12) and a block device that refuses it cannot serve a coherent set — REFUSING"))
RCU rebuild (3.1); Ok(mode)
```
The ONE product fallback: EINVAL on a REGULAR FILE. A block device refusing O_DIRECT for any reason, or a
regular file failing with any other errno, REFUSES registration.

Registration points (superblock.rs:1317–1322, :1362; builder.rs:488, :1447): `classify_volume_slot` (every
door's first read), `format_v3_inner`, `ImageBuilder::build`.

### 3.4 Write law (`prepare_direct_write`, :991–1019)
```
let Some(m) = mode.filter(direct) else return Ok(())            // buffered: nothing
if offset % grain != 0 || len % grain != 0:
    META_IO_UNALIGNED_REFUSALS += 1
    return Err(Io(InvalidInput, "direct metadata write at offset {o} of {len} bytes to {p} is not aligned to the device grain ({g} B) — a caller bug: the aligned form belongs beside the shape (meta_io_unaligned_refusals)"))   // to_errno ⇒ EINVAL
if !ptr_aligned(data.as_ptr(), m.mem_align):                       // (ptr as usize) % align == 0, align<=1 always ok
    META_IO_BOUNCE_BYTES += len
    *data = AlignedBuf::from_slice(data, m.mem_align).into_bytes()
```
Only the POINTER is checked for the buffer; the LENGTH was already checked against grain. No RMW ever.
Applies in: `WriteAt`, every op of `WriteAtBatch` (before any SQE), owned ring, blocking fallback.
NOT applied to `WriteAll` (buffered by definition).

### 3.5 Read law (`read_span`, `read_buf_for`, :1945–1968)
- Direct: `start = offset - offset % g`; `end = ceil((offset+size)/g)*g`; `skip = offset - start`;
  if `start != offset || end != offset+size` ⇒ `META_IO_READ_WIDENED += 1`. Returns `(start, end-start, skip, g)`.
  Buffer: `AlignedBuf::zeroed(span_len, mem_align)`.
- Buffered/unregistered: `(offset, size, 0, 1)`, `AlignedBuf::zeroed(size, 1)`.
- Completion slices `[skip, min(filled, skip+want))` out of the refcounted buffer; short/EOF ⇒ shorter/empty.
- Continuation: only when `filled % grain == 0` (3.1.11).

### 3.6 `pad_to_grain` (:1026–1033)
`grain = meta_io_grain(path)`; if `>1` resize image to `ceil(len/grain)*grain` with zeros; `Bytes::from(image)`
(heap Vec — pointer may still bounce). Used by the slot-tails spill (backend.rs:7378–7384).

### 3.7 `patch_at` (:1044–1068) — HARNESS ONLY
Empty ⇒ Ok. Not direct ⇒ `write_at`. Direct: `lo = offset - offset%g`; `hi = ceil((offset+len)/g)*g`;
`cur = read_at(lo, hi-lo)`; `image = AlignedBuf::zeroed(hi-lo, mem_align)`; copy `cur` prefix; overlay data
at `offset-lo`; `write_at(lo, image.into_bytes())`. Never increments refusals (tested).

### 3.8 "Every unit is 4 KiB-granular except the journal ring" — how enforced
From design-symmetric-metadata.md §5.12 inventory table (docs:776+) and code:
- Superblock sector 0 / backup: 4096 at 0 / `backup_offset` — unchanged.
- Appender pages, root-ledger slots, allocator bitmap pages: 4096-sized at 4096 multiples — unchanged.
- Node images and bset appends: `check_addr_alignment(node_addr % 4096 == 0)` (node.rs:725–732);
  `append_bset` precondition `tail_offset >= NODE_PAGE && tail_offset % NODE_PAGE == 0` else `Corrupt("append
  tail offset … is not a 4 KiB-aligned log position")` (node.rs:1013–1021); frames `page_align(...)`
  (node.rs:444–450). `NODE_PAGE = 4096` (node.rs:123).
- Slot-tails spill: `pad_to_grain` (backend.rs:7381).
- Journal ring: the sector-pad law (§4 below) — the ONLY byte-positioned unit.
- Everything else that reaches a direct path misaligned is a caller bug ⇒ refused (`meta_io_unaligned_refusals`).
- The worker's grain ceiling (`admit_meta_io_grain ≤ 4096`) is what makes these 4 KiB units align on any device.

---

## 4. Journal ring sector-pad law (lives in `journal_core.rs` + `journal.rs`; summary)

- `CoreGeometry { page_data_len: 4072, pages, reserve_bytes, grain }` (journal_core.rs:87–103); `PAGE_HDR_LEN = 24`
  (:108); `PAD_MIN = 21` = `ENTRY_HDR_LEN(20) + 1` marker byte (:113; compile-time tie journal.rs:171–174).
- `sector_aligned(pos)` (:125): `grain<=1 ⇒ true`; else `off = pos % 4072`; `off==0 || (24+off) % grain == 0`.
- `pad_of(end)` (:137): 0 if aligned; else `target = end+PAD_MIN`; `off = target % 4072`; if `off==0` ⇒ target;
  else `phys = 24+off; up = ceil(phys/grain)*grain`; if `up >= 24+4072` ⇒ `target + (4072-off)` (page end)
  else `target + (up-phys)`; return `next - end`.
- `max_pad()` (:162) = `PAD_MIN + grain` (0 if grain ≤ 1). Every admission claims `len + max_pad` slack
  (:509–510); `reserve` on a padded ring is a CAS loop taking exact `pad` and releasing the rest (:584–619).
- PAD entry (journal.rs:1276–1286): ordinary checksummed entry at `res.end()` of `pad` bytes, header
  `seq(u64)=position | len(u32)=pad-20 | xxh3(u64)`, payload `PAD_TAG=0xFF` then zeros; replay parses it as
  `Parsed::Pad` chain link (journal.rs:1753–1761); a PAD whose checksum fails is a torn entry (resync).
- Coalescing (`aligned_ops`, journal.rs:1350–1407): a window's ops are merged into ONE `AlignedBuf` image per
  physically contiguous run (page-adjacent segments merge; a segment boundary/ring wrap starts a new run),
  holes zero-filled, runs with `lo % grain != 0 || hi % grain != 0` refused as `Corrupt` before any write.
  mem_align from `meta_io_mode(path)`. Then `write_at` (1 op) or `write_at_batch`.
- `ring_grain_for(path)` (journal.rs:189) = `meta_io_grain` if power-of-two ≤ 4096 else 1.
- Recovery pad (`align_head_for_writing`, journal.rs:1456–1481) is the one honest RMW (`rmw_unaligned_ops`
  :1488–1540: covering spans merged, read fresh, overlaid, `write_at_batch`).
- Counters: `META_KV_JOURNAL_PAD_ENTRIES`, `META_KV_JOURNAL_PAD_BYTES` (mod.rs:364–366).

---

## 5. Stats counters / gauges and tripwires

Published UNGATED on the stats inode (fuse_client.rs:12139, :12147):

`"meta_io": { ... }` (:1071–1080) — all `AtomicU64`, `Relaxed`:
| Name | Meaning | Tripwire |
|---|---|---|
| `meta_io_direct_paths` | registrations that landed O_DIRECT | — |
| `meta_io_buffered_fallback` | registrations that fell to BUFFERED (regular-file EINVAL, or the test seam) | **must-stay-0 on any block device** |
| `meta_io_bounce_bytes` | bytes copied into an aligned buffer because the caller's pointer was misaligned | economy item (hot conveyor path pays 0) |
| `meta_io_unaligned_refusals` | direct writes refused for misaligned offset/length | **must-stay-0** (caller bug) |
| `meta_io_read_widened` | direct reads whose window was widened to the grain | cold-path instrument |

Not in JSON but `pub static`: `META_IO_MODE_RESOLVES` — canonicalize() calls; "flat under a steady stream of
I/O to known paths" (:619–624).

`"uring_fs_write_phase_ns": { queue_hop, device, wake_hop, total }` — four `LatencyHistogram` (26 buckets +
count + sum_ns + mean_ns; fuse_client.rs:5013). Recorded ONCE per observed write at first observation
(:388–405): `queue_hop = admitted - submitted`, `device = woken - admitted`, `wake_hop = observed - woken`,
`total = observed - submitted` (all saturating).

`uring_queue_full` (fuse_client.rs:6184, :11523) — bumped by `queue_full_err` (shared with nvme_dev).

Journal-lane engagement (journal_lane.rs:54–69): `journal_lanes_spawned`, `journal_lanes_ringless` (must be 0
on an io_uring box), `journal_ring_lane_writes`, `journal_ring_pool_writes`.

---

## 6. Tests

### 6.1 `src/uring_fs.rs` unit tests (:2955–3036) — all Linux (io_uring)
| Test | Asserts |
|---|---|
| `test_uring_fs_write_read_roundtrip` | 12,345-byte write_all → read_all equal; fdatasync Ok |
| `test_uring_fs_write_creates_parents` | write_all to `a/b/c.dat` creates parents |
| `test_uring_fs_runs_a_worker_pool` | `URING_FS._threads.len() >= 4` |
| `test_uring_fs_fdatasync_then_read_no_ebadf` | 64 tasks: write_at, fdatasync, read_at same path — no EBADF (fd cached O_RDWR) |
| `test_uring_fs_concurrent_ops_integrity` | 64 files, len 4096+i, write/fdatasync/read equal across pool workers |

### 6.2 `tests/uring_fs_tests.rs` — Linux
| Test | Asserts |
|---|---|
| `test_write_at_batch_lands_all_entries` | unordered, non-contiguous batch (512 B, 4096, 12288, 100 B) lands; gap keeps seed; empty batch Ok |
| `test_write_at_batch_propagates_errors` | `/proc/definitely/not/...` batch is Err |
| `test_concurrent_mixed_io_byte_exact` | 64 tasks × 20 rounds on 8 files, disjoint 4 KiB regions, mixed write_at/batch/fdatasync/read_at; each read is exactly own fill; 60 s timeout |
| `test_fd_cache_churn_under_load` | 16 tasks × 200 distinct files, 1 KiB each; eviction never corrupts in-flight I/O |
| `test_write_at_batch_larger_than_ring_completes` | ONE batch of 600 × 4 KiB sectors (> 512 SQ) completes; entries 0,255,511,512,599 verified |

### 6.3 `tests/meta_io_direct_tests.rs` (`--test-threads=1`; declines via testkit `SkipClass::Capability` when
the scratch fs refuses O_DIRECT; `SQUEEZEFS_TEST_REQUIRE_CAPABILITY=1` turns the decline into failure)
| Test | Asserts | Kind |
|---|---|---|
| `the_pad_law_lands_every_end_on_a_sector_boundary_within_its_bound` | for grain ∈ {512, 4096}, every `end < 3×4072`: `end+pad` aligned; `pad==0` iff aligned else `≥ PAD_MIN`; `pad ≤ max_pad`; `in_page_off(end+pad)==0 || (24+off) % grain == 0`; grain 1 ⇒ max_pad 0, pad_of(123)=0 | pure |
| `a_registered_metadata_file_opens_o_direct_with_a_derived_grain` | unregistered grain 1; registered: grain pow2 ≤ 4096, mem_align pow2 ≥ grain; `meta_io_mode` by given AND canonical path; idempotent; buffered fallback ⇒ grain 1 and `META_IO_BUFFERED_FALLBACK ≥ 1` | Linux |
| `misaligned_direct_writes_refuse_and_misaligned_buffers_bounce_and_narrow_reads_widen` | offset 1 and length g−1 ⇒ Err EINVAL, refusals +2, nothing landed; `Bytes.slice(1..)` buffer ⇒ bounce_bytes += g, lands; read at `2g+7` len 21 widened and exact; straddle read `3g−4` len 8 → 4×C3 + 4×00; `pad_to_grain` len = ceil, zero tail, buffered path verbatim; `patch_at` at `4g+5` len 3 lands, refusals unchanged | Linux |
| `a_padded_ring_writes_aligned_runs_and_replay_walks_the_pads` | ring grain == mode.grain; every reservation start/padded_end aligned; pad ∈ [PAD_MIN, max_pad]; pad counters move; recover replays all entries, `dropped_torn 0`, head = padded end, `align_head_for_writing` → 0 | Linux |
| `a_pad_whose_checksum_fails_is_a_torn_entry_the_scan_resyncs_past` | flip pad checksum byte (+12 from pad pos): (i) first window's pad torn ⇒ 3 entries, `dropped_torn 1`; (ii) last window's pad torn ⇒ `dropped_torn 0`, head at UNPADDED end, `align_head_for_writing` writes exactly the torn pad's length | Linux |
| `a_ring_written_unpadded_replays_direct_and_the_writer_aligns_its_head` | grain-1 ring of 5 entries → register → recover 5, head mid-sector, recovery pad ≥ PAD_MIN, next reservation starts at `end+pad`, re-recover 6 entries | Linux |
| `a_grain_the_page_arithmetic_cannot_honour_refuses_the_registration` | {512,1024,2048,4096,1,2,4} admit verbatim; {8192,16384,65536,3072,520,0} refuse with text containing device, source, `"{g} B"`, `"REFUSING"`, not `"clamp"` | pure |
| `the_posture_lookup_resolves_a_path_once_and_never_on_a_hit` | 1000 hits ⇒ RESOLVES unchanged; unregistered path resolves once then remembered; registration overwrites negative; symlink alias resolves once; alias seen BEFORE registration re-resolves exactly once after; `clear_meta_devices` forgets all | Linux (symlinks/canonicalize) |
| `the_buffered_seam_keeps_the_pre_pr_13i_posture` | `SQUEEZEFS_TEST_META_BUFFERED=1` ⇒ `!direct`, grain 1, fallback +1, ring grain 1, write_at(3, "abc") verbatim | Linux |
| `a_ring_recovered_within_max_pad_of_full_opens_through_the_guarded_cycles` | full unpadded ring with mid-sector head opens for writing via guarded checkpoint cycles; pad written; head aligned; records survive remount | Linux, full KV |

### 6.4 Elsewhere
- `derivation_sweep_tests::uring_fs_workers_derive_from_process_cores` (pure; values in 1.1) and the fleet-share
  floor assertion `resolve_worker_count(None, c) == 4` (derivation_sweep_tests.rs:1549).
- `nofile_budget_tests::the_fd_cache_derives_from_the_limit_as_found_and_the_raise_moves_the_listener_alone`
  (Linux, setrlimit): values in 1.12; `fd_cache_cap_in_force()` == `fd_cache_cap_from(soft_before, workers)`.

---

## 7. Numeric constants (with WHY) and measured numbers

| Constant | Value | WHY (source) |
|---|---|---|
| `URING_FS_QUEUE_CAP` | 4096 | backpressure bound, parity with `nvme_dev::URING_REQ_QUEUE_CAP` (:39–45) |
| `RING_ENTRIES` | 512 | ring SQ depth per worker (:47) |
| `ADMIT_CAP` | 256 | max in flight; "continuations reuse their slot, so SQ pressure is bounded by this plus one burst of resubmits" (:48–50) |
| fd cache cap | `clamp(soft/4/workers, 16, 1024)` | aggregate < ¼ RLIMIT_NOFILE as found; "capped at 1024, floored at 16" (:51–59) |
| worker count | `clamp(cpus/4, 4, 64)`; env clamp 1..=64 | cpus/4 measured drain-thread slope; floor 4 shipped; 64 env parity (:190–200) |
| `META_IO_DEFAULT_GRAIN` | 4096 | "largest logical block size in the field (4Kn)" (:673–676) |
| grain admit bound | pow2 ≤ 4096 | ring page arithmetic; page boundaries always aligned (:764–769) |
| `WAKE_UD` / `SEAM_TIMEOUT_UD` | `u64::MAX` / `u64::MAX-1` | never a slot index (:2480–2483) |
| eventfd write | `1u64` | counter sticky; EAGAIN impossible/harmless (:2596–2601) |
| `JOURNAL_RING_ENTRIES` | 512 | window entries + wake READ (journal_lane.rs:47–51) |
| `PAD_MIN` | 21 | 20 B entry hdr + 1 marker (journal_core.rs:110–113) |
| `PAGE_HDR_LEN`/`JOURNAL_PAGE_LEN`/`DATA_LEN` | 24 / 4096 / 4072 | ring page framing |
| `max_pad` | `PAD_MIN + grain` | PAD_MIN short of a boundary then a whole grain (journal_core.rs:159–167) |
| `URING_FS_WRITE_PHASES` | 4 | queue_hop/device/wake_hop/total (:341–343) |

Measured numbers quoted in comments:
- Old `submit_and_wait(1)`-per-request model "burned ~27% of daemon cycles in queue churn under delete storms"
  and capped in-flight at pool size (:13–17).
- D-2: journal ring write "~1 KiB page-cache write reading 1.0–1.3 ms MEAN with a 50–200 µs MODE" (:312–314).
- journal_lane.rs:16–17: `wake_hop` "66 µs of a 116 µs quiet round trip; 923 of 1,206 under lane + box load;
  behind a 2 ms serve burst, 2.9 ms".
- §5.12 doc: bounce economy "1,344 pages = 5.5 MiB over 200 creates + 20 checkpoints" (two-host leg).
- Ring pad cost: ≤ `PAD_MIN + page_data_len` per window at grain 4096 (every window pads to a page end);
  ≤ 532 B/window at grain 512 (doc §5.12 table).

---

## 8. Concurrency, ownership, lifetimes

- **Atomics**: every counter (`META_IO_*`, `FAULTS_ACTIVE`, `TORN_WRITE_FAULT.*`, `SECTOR_WRITE_ERROR`,
  `METRICS.uring_queue_full`, latency lane `next_seq`) is `Relaxed`. Registry is `ArcSwap` (`load`/`rcu`/`store`).
  The `Reactor` has NO atomics — it is single-threaded per worker (`Rc`, `RefCell`).
- **Locks**: `FAULT_STATE: Mutex<FaultState>` (test shim; taken once per admission when `FAULTS_ACTIVE`);
  latency lane `Mutex<BinaryHeap<Reverse<DelayedReq>>>` + `Condvar`; oneshot `Mutex` per channel.
- **Buffer lifetime across DMA**: the SQE's pointer targets live in `slots[i]` — `Pending::Read.buf:
  AlignedBuf` (owned heap, stable address) or `Pending::Write.data: Bytes` (refcounted; the caller's
  `Bytes` clone or the bounce `AlignedBuf` via `Bytes::from_owner`). The slot is never touched until its CQE is
  reaped (`// SAFETY` :2060–2061, :2544–2545). On the owned reactor the wake READ target is `wake_buf: Box<u64>`
  (stable address, :2659–2663) and the linked TIMEOUT's `Timespec` lives on the caller's stack and "the kernel
  reads the timespec at submission: `link_after`'s caller submits before returning" (:2090–2091).
- **fd lifetime**: `Rc<File>` cloned into every `Pending`; cache eviction or posture-mismatch removal drops only
  the cache's Rc, so the fd closes after the last in-flight op (:1833, :1904–1906).
- **Batch aggregate**: `Rc<RefCell<BatchState>>` shared by N slots on one thread; the `tx` is `take()`n once.
- **Zero-copy read return**: `AlignedBuf::into_bytes()` ⇒ `Bytes::from_owner(self)`, then `.slice(skip..end)`;
  the aligned allocation frees with the last reference (:960–965).
- **Owned reactor** (:2566–2818): `OwnedRing` is `Send` (ring + `Arc<OwnedFd>`); `into_reactor` must run on the
  driving thread (slot table thread-bound). `park(tick)`: if `!wake_armed` push `Read(eventfd, wake_buf, 8)`
  with `user_data=WAKE_UD` (submit-and-retry on SQ full), set armed; then
  `submitter().submit_with_args(1, SubmitArgs.timespec(tick))` → `Ok ⇒ false`; `ETIME ⇒ true`;
  `EINTR ⇒ false`; `EINVAL` (no `IORING_FEAT_EXT_ARG`, pre-5.11) ⇒ untimed `submit_and_wait(1)`; other error
  ⇒ `fail_all`. `service()` = `reap()`; if WAKE_UD seen ⇒ `wake_armed = false` (next park re-arms).
  `RingWake::wake` from any thread writes 8 bytes to the eventfd ("a park not yet entered returns at once —
  the counter is sticky"). `submit_write_at_batch`: fault shim via `intercept_inline_write` (latency arm
  becomes a linked `TIMEOUT(ETIME_SUCCESS)` on the ring instead of the deadline lane), `admit_write_batch`
  with `admitted_at = submitted_at`, then non-blocking `ring.submit()`; a submit error other than EINTR
  `fail_all`s.
- **Fault shim ordering at admission** (:1598–1775): unwrap `Delayed` → poisoned path ⇒ EIO now → (if not
  delayed) device latency ⇒ park on deadline heap, re-admit as `Delayed` → per-variant: WriteAt: sector error
  EIO; torn write (persist `data[..keep]` via std pwrite+fdatasync, disarm, poison path, EIO); write stall
  (intersecting range ⇒ held, arrival tap); power-cut capture of ORIGINAL bytes. WriteAtBatch: same, batch
  held/failed whole; tear lands earlier entries fully, matching entry's prefix, later none. Fdatasync: barrier
  error (raw errno) → clear tracked (durable) → barrier stall (held). `clear_faults` drops held ops (callers
  see worker-closed). `power_cut` reverts tracked writes in reverse order with std pwrite + `sync_data`.

---

## 9. Zig porting checklist (behavioural must-haves, no advice)

1. N worker threads (`clamp(cpus/4,4,64)`, env override clamp 1..64), names `squeezefs-uring-fs-{i}`, one
   512-entry plain ring each, shared bounded MPMC queue of 4096 with non-blocking producer `try_send` and
   `uring_queue_full` counter + EINVAL-class error on full.
2. Burst-admit up to 256 in flight; `submit_and_wait(1)`; reap all; continuations resubmit from the reap pass
   with ONE clock read per pass; SQ-full ⇒ `submit()` then retry once; second failure ⇒ "submission queue
   overflow" error to that op only.
3. Slot-indexed `user_data`; `u64::MAX` and `u64::MAX-1` reserved; per-slot ownership of buffer + fd refcount.
4. Per-worker fd cache keyed by path with `(fd, lru, direct)`; cap `clamp(nofile_soft_as_found/4/workers,16,1024)`;
   posture mismatch evicts; O_RDWR|O_CLOEXEC(|O_DIRECT) opens inline.
5. Registry: ArcSwap-equivalent map path→Option<mode>, both spellings, negative cache bounded by fd cap,
   registration drops all negatives; `META_IO_MODE_RESOLVES` increments once per distinct unseen path.
6. Grain ladder exactly: statx DIOALIGN → sysfs `/sys/dev/block/M:m/queue/logical_block_size` then
   `../queue/…` → 4096; admit pow2 ≤ 4096 else refuse with device/source/grain/REFUSING text; mem_align =
   `max(mem, grain).next_power_of_two()`; trial `O_RDWR|O_CLOEXEC|O_DIRECT` open; EINVAL on regular file ⇒
   BUFFERED + warn + counter; anything else ⇒ refuse.
7. Writes: misaligned offset/len ⇒ EINVAL + `meta_io_unaligned_refusals` (before any SQE, whole batch);
   misaligned pointer ⇒ copy to aligned buffer + `meta_io_bounce_bytes += len`.
8. Reads: widen to grain, aligned buffer, slice `[skip, min(filled, skip+want))`; continuation only on grain
   multiple; `meta_io_read_widened`.
9. Write outcome carries `admitted_at`/`woken_at`; observer records 4 histograms once.
10. Owned ring: eventfd(EFD_CLOEXEC|EFD_NONBLOCK), one armed READ SQE across parks, `submit_with_args`
    timed wait with ETIME/EINTR/EINVAL fallback semantics, reap on the owning thread.
