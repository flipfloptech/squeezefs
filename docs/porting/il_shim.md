# Porting spec — the interception shim `libsqueezefs_il.so` (rung 7)

Scope: charter §7 (the client bypass: classify on `open`, HELLO, the shim authenticates the
daemon, the bail-out ladder, the offset mirror, the hybrid lane gate, the libaio interposers,
the park `k = clamp(pending/4, 2, pending)`), §11 (the daemon and shim pair by build commit),
§12 (a C-ABI shared library, SONAME, `-z nodelete`, no panics escape — every interposer
failure becomes passthrough). Reference paths are relative to `~/Source/squeezefs` (`R:`);
port paths to `~/Source/squeezefs-zig` (`P:`). Every claim carries `file:line`. The reference
is evidence (decision 0002); §11 lists the quirks NOT carried and what the port does instead.

The daemon half (session host, service threads, direct-drive lanes, the completion doorbell's
daemon side, the stats inode export) is a sibling spec; this one covers the daemon only where
the shim's expectations need it (§3.9).

Reference shape: `R:crates/squeezefs-preload/` — `interpose.rs` 3086 lines (the 59 exported
symbols), `session.rs` 2052 (the session client), `fd_table.rs` 654 + `fd_table_core.rs` 409
(the bound-fd table; the core is loom-modelled, `R:loom-models/src/lib.rs:4956-5250`),
`aio_core.rs` 374 + `aio_glue.rs` 263 (libaio), `bailout.rs` 75, `dev_cache.rs` 80,
`mapped_inos.rs` 181, `lane_gate.rs` 124, `build.rs` 69, `src/bin/aio_lifecycle_harness.rs`
396, seven `tests/*.rs` (2251 lines). Shared wire/layout cores live in
`R:crates/squeezefs-ipc/src/{layout,wire,ring_core,slot_core,cqe_core,sizing,thp,
env_knob_core}.rs`. Design: `R:docs/design-preload-interception.md` §5.1–§5.8, §10–§12.

## 0. Shape in one paragraph

Control plane on the kernel mount; data on bound fds over a sealed memfd. The shim is a
`cdylib` named `libsqueezefs_il.so` (`R:Cargo.toml:33-37`) depending on `libc` + the ipc
protocol crate only — no runtime, no allocator replacement, no panics across `extern "C"`
(`R:src/lib.rs:1-20`). Every interposed symbol resolves its real function by
`dlsym(RTLD_NEXT)` lazily on first call (`R:interpose.rs:376-397`); a thread-local reentrancy
guard routes nested entries straight to the real call (`R:interpose.rs:251-279`); a fixed
32-slot session registry keyed by `(st_dev, shard)` holds leaked `Session`s
(`R:interpose.rs:82-104, 133-249`); a lock-free two-level fd table maps fd → refcounted
`BindingCell` (`R:fd_table.rs:1-65`). Everything that is not a served ring op is *literally*
the real call (`R:interpose.rs:22-25`; design §5.4.2 `R:docs/design-preload-interception.md:
321-323`).

## 1. The interposed libc symbols — exact behaviour

### 1.1 The plumbing law every symbol obeys

- **Resolve eagerly, call lazily**: the real pointer is resolved before the body, but the real
  CALL executes only on passthrough paths (no guard, body says "real", body panicked). The
  first sudo gate caught the eager-call form double-applying every bound-fd write
  (`R:interpose.rs:952-977`, the `interposed!` macro).
- **No RTLD_NEXT symbol** ⇒ synthesize the kernel answer: `ENOSYS`/-1 for most, the raw
  syscall for `lseek`, `close_range`, `sendfile`/`copy_file_range`/`splice`
  (`R:interpose.rs:960-964, 1221-1225, 1583, 1266-1267`).
- **Guard unavailable (reentrant entry or TLS gone)** ⇒ real call (`R:interpose.rs:965-967`).
- **Panic in the body** ⇒ `panic_poison()` (every session poisoned + ONE stderr line) then the
  real call (`R:interpose.rs:347-350, 971-975`). Exception: the libaio async arms (§6.6).
- **Wrong-direction op on a bound fd** ⇒ real call: the kernel's own `EBADF` is the identical
  answer (`R:interpose.rs:641-643`).
- `offset < 0`, `count == 0`, `buf == NULL` ⇒ real call (its errno/0 is the answer)
  (`R:interpose.rs:634-637`).
- **LFS-64 discipline**: glibc ≥ 2.28 compiles `_FILE_OFFSET_BITS=64` callers against the
  64-suffixed symbols; every family covers both spellings (CPython's `os.dup` →
  `fcntl64(F_DUPFD_CLOEXEC)` was the invisible-dup tell) (`R:interpose.rs:1457-1463, 1660-1664`).

### 1.2 Positional data: `pread`, `pread64`, `pwrite`, `pwrite64` (`R:interpose.rs:989-1018`)

Bound fd, rights ok, session live: `ring_positional_core` → hybrid gate (§8; strictly larger
than the threshold ⇒ real call, counted, binding stays) → `session.ring_pread/ring_pwrite`
(`R:interpose.rs:626-672`). Outcomes (`R:interpose.rs:613-624, 676-700`):
- `Served(n)` (short/0 is POSIX-legal) ⇒ return `n`;
- `Errno(e)` (a daemon errno kernel-identical for the op) ⇒ `errno = e`, return -1;
- `Real{poisoned:false}` (transient backpressure: no slot claimable; arena/`max_op` shape)
  ⇒ real call, binding stays;
- `Real{poisoned:true}` ⇒ `demote_and_flush(fd)` (mirror → kernel `f_pos`), `on_close(fd)`
  (refcount release; the unbind ctl is silent on a poisoned session), then the real call.

### 1.3 Offsetful data: `read`, `write` (`R:interpose.rs:1020-1052`) — the mirror law

`ring_offsetful` (`R:interpose.rs:723-796`):
1. `lookup_with_mirror(fd)` — unbound ⇒ real call.
2. **Sticky kernel-lane latch** set ⇒ real call outright, counted as a gate route (the mirror
   was flushed and demoted at latch time) (`R:interpose.rs:733-738`).
3. Take the description's offset lock (dup siblings share it) — a poisoned lock ⇒ real call.
4. **Gate trigger** under the held lock: rights ok AND `len > threshold` ⇒ `latch_kernel_lane`,
   `disarm_if_current` → raw `SYS_lseek(SEEK_SET)` once, count, real call
   (`R:interpose.rs:745-758`).
5. Offset source: armed ⇒ `mirror.load()` (zero syscalls); unarmed ⇒ raw
   `lseek(fd, 0, SEEK_CUR)` (`f_pos` only — no FUSE request, pinned by the gate's lseek row);
   `< 0` (unseekable) ⇒ real call (`R:interpose.rs:759-770`).
6. `ring_positional_core(gate=false)`; then `Served(n>0)` ⇒ `finish_offsetful`: armed ⇒
   `publish(off+n)`; publish returning `false` (a demote raced) ⇒ the op itself writes
   `SEEK_SET off+n` through to the kernel; unarmed ⇒ raw `SEEK_SET off+n`
   (`R:interpose.rs:702-714, 772-777`). `Errno` ⇒ no bytes consumed, NEITHER offset moves
   (`R:interpose.rs:778-781`). `Real` ⇒ if armed, disarm-once + flush under the held lock
   (the real call consumes kernel `f_pos` next); if poisoned, release the entry
   (`R:interpose.rs:782-794`).

`O_APPEND` offset law: an `O_APPEND` description never binds (§2.4) and `F_SETFL` adding
`O_APPEND` unbinds first (`R:interpose.rs:1702-1706`), so an append offset is always the
kernel's; there is no shim-side append arithmetic.

### 1.4 Vector data: `readv`, `writev`, `preadv[64]`, `pwritev[64]`, `preadv[64]v2`, `pwritev[64]v2` (`R:interpose.rs:1054-1158`)

`ring_iovec(offset < 0 ⇒ offsetful)` (`R:interpose.rs:800-946`): gate on the TOTAL requested
bytes (the kernel serves the whole vector as one request); latched offsetful ⇒ real call; a
gate trip on the offsetful shape latches + flushes under the lock; hold the offset lock across
the whole chain (one atomic `f_pos` advance); serve segment by segment via
`ring_positional_core`; a short segment (incl. EOF 0) ends the vector op; a daemon errno
mid-chain returns the partial count (zero progress ⇒ the errno, no offset moves); a `Real`
mid-chain returns the partial count — only a ZERO-progress fallthrough takes the real call
(after flushing an armed mirror). Short-served AND dying ⇒ park `start+total` in the kernel
under the held lock, then unbind. `*v2`: `rwf_passthrough(flags)` (allow-list: `RWF_HIPRI`
only — `RWF_SYNC/DSYNC/APPEND/NOWAIT` and every unknown flag passthrough) ⇒ for the
offsetful shape `demote_and_flush` first, then the real call (`R:interpose.rs:1138-1148`;
`R:bailout.rs:59-75`).

### 1.5 `lseek`, `lseek64` (`R:interpose.rs:1166-1234`)

Unbound/unarmed/poisoned-lock ⇒ real call. Armed: `SEEK_SET`/`SEEK_CUR` are pure arithmetic
over the mirror (negative or sign-wrapped result ⇒ `EINVAL`, exactly `vfs_setpos`;
`R:fd_table.rs:221-236`); `SEEK_END`/`SEEK_DATA`/`SEEK_HOLE`/unknown ⇒ raw `SYS_lseek`, and on
success the mirror re-seeds to the kernel's result — the arm SURVIVES (none of these read
`f_pos`) (`R:interpose.rs:1190-1205`). Measured: `~101 ns` served SEEK_CUR vs `~1.48 µs` for the
replaced SEEK_CUR+SEEK_SET syscall pair, `~73 ns` armed op cycle
(`R:.benchmarks/2026-08-02-perf7-shim-lseek.md:30-32`).

### 1.6 `f_pos`-consuming passthroughs: `sendfile[64]`, `copy_file_range`, `splice` (`R:interpose.rs:1240-1318`)

The real call ALWAYS executes; interposed only as demote triggers: `sendfile` demotes
`out_fd` always (no out offset exists) and `in_fd` when `offset == NULL`; `copy_file_range`/
`splice` demote the side whose offset pointer is NULL. Non-NULL pointers never touch `f_pos`
⇒ no demote. `splice`'s pipe side never resolves in the table (no-op).

### 1.7 `posix_spawn`, `posix_spawnp` (`R:interpose.rs:1320-1369`)

glibc spawns via `clone` and runs NO atfork handlers, so the fork flush (§1.14) runs here:
`fork_flush_all()` (epoch bump + one `SEEK_SET` per armed cell) BEFORE the real spawn. Returns
errno directly (`ENOSYS` when unresolvable). `system`/`popen`/`execl*` internals bypass
interposition — the documented out-of-model residual (`R:interpose.rs:1329-1332`).

### 1.8 Detection: `open`, `open64`, `openat`, `openat64`, `creat`, `creat64` (`R:interpose.rs:1375-1526`)

Order: resolve real → **fork-epoch snapshot** (`epoch_snapshot()`, `catch_unwind`-wrapped,
`u64::MAX` on failure = never arms) → the real call → guard → `after_open` =
`atfork_init()` (register `pthread_atfork(prepare, NULL, child)` once) + `classify_and_bind
(fd, epoch)` (§2). The fd is returned regardless of any bookkeeping outcome. `open`'s variadic
`mode` is declared as a fixed third argument and forwarded verbatim (Rust has no C-variadic
definitions; harmless on every Linux ABI) (`R:interpose.rs:1424-1428`). NOT interposed:
`openat2`, `__open*` aliases, `fopen` (stdio reaches `open`/`read` through libc's own
PLT-free internals — the design's stated residual, `R:docs/design-preload-interception.md:152`).

### 1.9 Lifecycle: `close`, `close_range`, `dup`, `dup2`, `dup3`, `fcntl`, `fcntl64`

- `close` (`R:interpose.rs:1534-1553`): bookkeeping FIRST (an fd number can be reused the
  instant the real close returns): `route_release(table.on_close(fd))` — refcount release; the
  LAST ref reports the binding and sends the async `Unbind{binding_id}` ctl (silent on a
  poisoned session, `R:session.rs:724-731`). The real close ALWAYS executes (kernel
  FLUSH/RELEASE unchanged).
- `close_range` (`R:interpose.rs:1555-1585`): `CLOSE_RANGE_CLOEXEC` marks only ⇒ no sweep;
  otherwise `on_close_range(first, min(last, i32::MAX))` skips whole unallocated 1024-fd
  segments in O(1) (`close_range(3, ~0)` is the systemd idiom) (`R:fd_table.rs:550-575`).
- `dup`/`dup2`/`dup3`/`fcntl(F_DUPFD|F_DUPFD_CLOEXEC)` (`R:interpose.rs:1587-1652, 1731-1737`):
  real call first; on success `on_dup(oldfd, newfd)`: acquire one ref on oldfd's cell (refused
  if the count already hit zero — a racing last-close won), swap it into newfd's slot, release
  whatever was displaced (dup2 implicitly closes newfd) (`R:fd_table.rs:505-540`).
- `fcntl`/`fcntl64` (`R:interpose.rs:1654-1742`), BEFORE the real call: `F_SETLK|F_SETLKW|
  F_OFD_SETLK|F_OFD_SETLKW` ⇒ `demote_and_flush` + unbind (locks deserve the single-transport
  shape); `F_SETFL` adding `O_APPEND` ⇒ demote + unbind; `F_SETFL` toggling `O_DIRECT` on a
  BOUND fd (one extra `F_GETFL`, control-plane) ⇒ demote + unbind — the daemon captured the
  description's read class at bind (`BindingRights::odirect`, `R:src/ipc_host.rs:354-357`) and a
  diverged class would serve the wrong admission semantics; toggling `O_NOATIME` likewise (the
  daemon captured the description's atime exemption at bind, charter §4 "Access time"). The variadic arg is declared as a
  pointer and forwarded verbatim.

### 1.10 `mmap`, `mmap64` (`R:interpose.rs:1744-1831`) — the per-inode unbind + invalidation

With `fd ≥ 0` and the guard, BEFORE the real mmap: (a) **POSIX-7 poison**: if the mapping is
`MAP_SHARED` or `MAP_SHARED_VALIDATE` (the sharing field is a small enum in the low 4 bits,
not a bitmask — `R:mapped_inos.rs:52-65`), one `fstat`; a regular file on a dev not
negative-cached, with this fd bound OR any session on the dev ⇒ `mapped_inos.insert(dev, ino)`
— the inode is never bound again in this process; (b) if the fd is bound: `unbind_ino(ino)`
releases EVERY entry naming that ino (the mapping's authority is per-file; a same-inode sibling
left bound would recreate the W3 lost-update hazard), surfacing one `(fd, off)` flush per armed
cell which is written through by raw `SEEK_SET` before any real call, then one unbind ctl per
last-ref (`R:fd_table.rs:577-635`). The daemon's `on_last_unbind(ino)` fires the whole-inode
FUSE invalidation when no session anywhere still binds it (`R:src/ipc_host.rs:2032-2047`).
`munmap` is deliberately NOT interposed: the mapping's lifetime is unobservable (the set never
forgets; `R:mapped_inos.rs:19-36`). `mmap64` is the same body.

### 1.11 fd-creator hygiene: `socket`, `eventfd`, `epoll_create[1]`, `timerfd_create`, `inotify_init[1]`, `memfd_create`, `accept[4]`, `signalfd`, `pipe`, `pipe2`, `socketpair` (`R:interpose.rs:1833-1946`)

Real call; then every returned number is swept: a stale entry (a bound fd closed by a raw
syscall / `IORING_OP_CLOSE` whose number came back) is released EXACTLY as `close` would —
refcount decrement, unbind on last ref — never a bare pointer clear (`R:fd_table.rs:636-645`).
The common empty case is one segment load + one slot load + branch. `signalfd4` is listed in
the design table (`R:docs/design-preload-interception.md:151`) but not interposed.

### 1.12 libaio: `io_setup`, `io_destroy`, `io_submit`, `io_getevents`, `io_pgetevents`, `io_cancel` — §6; plus libaio's queue face `io_queue_init`, `io_queue_release`, `io_queue_run`, `io_queue_wait` (§6.9; the port's addition — the reference's 59 did not include it).

### 1.13 NOT interposed (kernel FUSE serves them; the fallback IS the design)

`fsync`, `fdatasync`, `fstat*`, `ftruncate`, `fallocate`, `posix_fadvise`, `munmap`, `fork`/
`vfork`/`clone` (fork handled by `pthread_atfork`), `exec*`, `openat2`, `signalfd4`, io_uring,
POSIX AIO, `FILE*` stdio (`R:docs/design-preload-interception.md:146-153`). fsync soundness:
a ring write returns only after the daemon acked it into the same state kernel-FUSE writes
reach; the subsequent kernel FSYNC flushes that same state (`R:docs/design-preload-
interception.md:386-388`).

### 1.14 The bound-fd offset mirror (PERF-7) — authority predicate and every exit

The mirror lives in the shared `BindingCell` (dup siblings share one offset + one lock,
exactly kernel shared-`f_pos`) (`R:fd_table.rs:16-24, 83-99`). **Authoritative iff** (a) armed at
bind with a fork-epoch snapshot taken BEFORE the fd's `open` syscall, (b) that snapshot still
equals the table's current epoch, (c) nothing has disarmed it (`R:fd_table.rs:27-33`;
`R:fd_table_core.rs:170-174`). The arm is monotone: it never re-arms (`R:fd_table_core.rs:137-139`).

**Every transition out of authority flushes mirror → kernel `f_pos` first** (raw `SYS_lseek`,
never the interposed symbol, never `dlsym` — AS-safe) (`R:interpose.rs:281-317`):

| Transition | Site |
|---|---|
| `fork` (atfork PREPARE, in the parent) / `posix_spawn{,p}` | `fork_flush_all` → `fork_demote_flush`: epoch bump, one flush per cell armed under the PREVIOUS epoch, under a 1024-spin `try_lock`; re-load the offset after the disarm (an op that completed meanwhile wrote through) (`R:fd_table.rs:395-465`; `R:interpose.rs:1379-1391, 1357-1360`) |
| `F_SETLK*`/`F_OFD_SETLK*`; `F_SETFL` + `O_APPEND`; `O_DIRECT` or `O_NOATIME` toggle | `demote_and_flush` + unbind (`R:interpose.rs:1693-1723`) |
| `mmap` W3(a) walk | `unbind_ino` flushes per armed cell (`R:fd_table.rs:604-617`) |
| session poison (per-op `Real{poisoned}`) | `demote_and_flush` + unbind (`R:interpose.rs:690-696, 784-792, 936-939`) |
| any offsetful fallthrough to the real call (incl. transient backpressure) | disarm-once + flush under the held lock; that fd prices at the two-syscall resync discipline thereafter (`R:interpose.rs:782-789, 925-931`) |
| the hybrid gate's offsetful latch | one-time flush at latch (`R:interpose.rs:750-753, 836-842`) |
| `sendfile`/`copy_file_range`/`splice` with a NULL offset pointer | `demote_and_flush` (`R:interpose.rs:1273-1318`) |
| `*v2` RWF passthrough, offsetful shape | `demote_and_flush` (`R:interpose.rs:1139-1146`) |

**The Dekker pair** (`R:fd_table_core.rs:185-217`): op side `publish` = store offset →
`fence(SeqCst)` → re-check armed (`false` ⇒ the caller owns the kernel write-through); demote
side `disarm_if_current` = armed? → `flag.swap(false)` once → `fence(SeqCst)` → load offset.
Release/Acquire alone admits the store-buffering outcome where the final offset reaches
neither consumer; removing either fence fails the loom model
(`R:loom-models/src/lib.rs:5076-5134`). `disarm_if_current` returns `None` for a never-armed
or stale cell — flushing a never-armed seed would REWIND an offset the kernel legitimately
advanced (`R:fd_table_core.rs:204-207`). Refcount law: monotone-to-zero CAS loops (`acquire`
refuses 0; `release` saturates at 0 — a wrapped `fetch_sub` is a binding that can never report
again) (`R:fd_table_core.rs:92-124`). Exactness scope, re-stated: exact under sequential mixing
within the predicate; outside it (cross-process shared descriptions racing, libc-invisible
`f_pos` consumers on an armed fd) the kernel-authoritative posture is restored by running
without the shim (`R:docs/design-preload-interception.md:332`).

## 2. Classification on `open` (`classify_and_bind`, `R:interpose.rs:460-606`)

1. `fd < 0` ⇒ return. `sweep_stale(fd)` releases any stale entry on the number.
2. `fstat(fd)`; failure ⇒ return. `dev_cache.contains(st_dev)` ⇒ return (the negative cache:
   1024 × 8 B slots, 4-way splitmix64 probe, `dev+1` tagged so `st_dev == 0` is legal, plain
   overwrite eviction — a lost entry costs one re-classification, never correctness;
   `R:dev_cache.rs:1-80`).
3. `mapped_inos.contains(dev, ino)` ⇒ return (POSIX-7; a `MAP_SHARED` inode stays kernel-served
   for the process's life).
4. `shard = fd % sessions_per_mount()` (default `clamp(ceil(cpus/4), 2, 16)` =
   `sizing::il_sessions_default`, `SQUEEZEFS_IL_SESSIONS` an override clamped to
   `1..=SESSION_REGISTRY_SLOTS/2`; sharding by fd keeps every session single-consumer while
   letting the daemon spread sessions over service threads — one session was the measured
   device-true plateau, ~285 k IOPS flat from 64 to 256 client threads;
   `R:interpose.rs:87-131`; `R:crates/squeezefs-ipc/src/sizing.rs:50-59`).
5. Registry hit on `(dev, shard)` ⇒ skip to 7. Miss ⇒ `fstatfs(fd)`; `f_type != FUSE_SUPER_MAGIC
   (0x6573_5546)` ⇒ negative-cache + return (`R:interpose.rs:58-59, 500-510`).
6. `fgetxattr(fd, "user.squeezefs.il0", buf[512])`; `n ≤ 0` (ENODATA/ENOTSUP: a foreign FUSE
   mount) or an undecodable blob ⇒ negative-cache + return. A decoded blob ⇒
   `announce_load_mode()` (§7) → the credential fd must itself pass `classify_fd` (an ineligible
   first fd only DEFERS establishment — the mount is ours, never negative-cached) →
   `Session::establish(blob, fd, BUILD_COMMIT, st.st_uid)` (§3.1) → `registry.insert`.
   Refusal ⇒ ONE stderr line per `(st_dev, reason_code)` for the process lifetime
   (`RefusalOnce`, 128-pair cap: past capacity it stops remembering, never stops printing) and
   return — the ladder RETRIES on every eligible open by design (`R:interpose.rs:326-335,
   550-563`; `R:session.rs:355-399`).
7. `F_GETFL`; `classify_fd(flags, st_mode, st_nlink)` failing ⇒ return (bind-time passthrough).
8. `session.bind(fd)` → `BindOk{binding_id, ino, read_ok, write_ok}` ⇒ seed = raw
   `lseek(fd, 0, SEEK_CUR)`; `≥ 0` ⇒ `bind_with_mirror(fd, binding, seed, epoch)` else `bind`
   (unarmed); the displaced stale entry is released. `BindRefused{class}` ⇒ one line per
   `(st_dev, 0x8000 | class)` (a BIND refusal and a HELLO refusal of the same class are distinct
   events). Socket/poison-class errors ⇒ silent (the poison machinery owns the loudness)
   (`R:interpose.rs:576-605`).

**Bootstrap blob** `user.squeezefs.il0` (`R:crates/squeezefs-ipc/src/wire.rs:203-255`): LE,
fixed layout — `magic u64 = "SQZIL0\0\0"`, `abi u32` (= `IPC_ABI`, currently 6,
`R:layout.rs:74`), `flags u32` (0), `build_commit [48]` (NUL-padded; 48 so `<40-hex>-dirty`
fits — the skew gate must SEE the suffix), `socket [108]` (abstract name, no leading NUL on
the wire), `socket_path [108]` (the path rung, empty when disabled), `nonce [32]`. Total 308 B.
Decode is total: short input, bad magic, non-UTF-8/unterminated strings ⇒ error (never a
panic). The daemon synthesizes it only when its session host is loaded; `size == 0` answers
the length; too small ⇒ `ERANGE`; unarmed mounts answer `ENODATA`
(`R:src/fuse_client.rs:30699-30717`). Socket name `sqz-il0-<pid>-<rand32:08x>`; the path dir is
`SQUEEZEFS_IPC_SOCKET_DIR` else `/run/squeezefs` (root) else `$XDG_RUNTIME_DIR/squeezefs` else
`/tmp/squeezefs-il-<euid>`, socket file `<dir>/<name>.sock` (`R:src/fuse_client.rs:31673-31699`;
`R:src/ipc_host.rs:292-301`).

**`classify_fd`** (`R:bailout.rs:19-57`), the client mirror of the daemon screen (an
optimization, never the boundary): `O_PATH` ⇒ Flags (load-bearing: O_PATH fds are obtainable
with search-only permission and read access mode 0); not `S_IFREG` ⇒ NotRegular; `O_APPEND` |
`O_SYNC` | `O_DSYNC` | `O_TMPFILE` bits | `st_nlink == 0` ⇒ Flags; rights strictly from
`O_ACCMODE`: RDONLY → (r,¬w), WRONLY → (¬r,w), RDWR → (r,w).

**Refusal reasons** (`R:session.rs:193-318`):

| Cause | `reason_code` | Line text |
|---|---|---|
| `VersionSkew` (client pre-check) | 1 | abi mismatch (shim X, daemon Y) / build mismatch (shim c1, daemon c2) / dev build identity — set `SQUEEZEFS_IPC_ALLOW_DEV=1` on both ends |
| `Protocol` (malformed reply, missing/extra fd, header/geometry/slab invalid) | 2 | malformed ctl reply / session layout |
| `Poisoned` (bind on a poisoned session) | 3 | session poisoned |
| `PeerUntrusted{peer_uid, mount_uid}` | 4 | socket peer uid P is neither root nor the mount owner (M) — not sending the credential fd |
| `Unsealed{seals}` | 5 | session memfd is not sealed (F_GET_SEALS = …; need SEAL\|SHRINK\|GROW) |
| `UntrustedRendezvous` | 6 | the advertised socket path does not resolve under a root- or owner-private directory |
| `Socket(errno)` / `Socket(0)` = EOF | `0x1000 \| errno` | ctl socket unreachable (os error e) / daemon closed the ctl socket |
| `Map(errno)` | `0x2000 \| errno` | session shm map failed |
| `Refused(class)` daemon classes 1–8: Version, Nonce, Flags, Mode, Budget, Peercred, Internal, Disabled | `0x4000 \| class` | per-class text (`refuse_reason`): Disabled = "interception not armed on this mount (mount with --interception)", Mode = "credential fd is not on this mount", …; unknown classes print their number |

## 3. The session client (`R:session.rs`)

### 3.1 Establish (`establish_inner`, `R:session.rs:488-687`)

1. **KD-7 pre-check** (skips the doomed round trip; the daemon enforces the same law):
   `blob.abi != IPC_ABI || blob.build_commit != my_commit || (degenerate(either) &&
   !allow_dev)` ⇒ `VersionSkew`. Degenerate = `"unknown"` | ends with `-dirty` | empty
   (`R:wire.rs:484-486`). `SQUEEZEFS_IPC_ALLOW_DEV` under the shared bool convention, announced
   ONCE per process when on (`R:session.rs:320-353`).
2. **Connect ladder**: abstract first (`SOCK_SEQPACKET | SOCK_CLOEXEC`, leading-NUL name;
   `R:session.rs:1629-1659`); on failure and a non-empty `socket_path`: **VAL-4 rung 1**
   `path_rung_trusted`: open the socket's PARENT with `O_PATH|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC`,
   `fstat`: `st_uid ∈ {0, mount_uid}` AND `mode & 0o022 == 0`, else `UntrustedRendezvous` and NO
   `connect(2)` (`R:session.rs:1734-1781`); then `connect_path` (NUL-terminated path,
   `R:session.rs:1663-1693`). Last-attempt error semantics. `SO_RCVTIMEO` = 10 s on the socket
   (`CTL_RECV_TIMEOUT`; the daemon's handshake deadline is matched to it,
   `R:src/ipc_host.rs:98-102`).
3. **VAL-4 rung 2 — BEFORE the credential fd leaves the process**: `getsockopt(SO_PEERCRED)`;
   a read failure refuses; `peer_uid ∉ {0, mount_uid}` ⇒ `PeerUntrusted` — deliberately no "or
   our own uid" disjunct (`R:session.rs:531-554, 1694-1716`). `mount_uid` is the `st_uid` the
   interposer already `fstat`ed on the credential fd, never re-stat'ed (`R:session.rs:445-450`).
4. `send_ctl(Hello{abi, pid, uid, build_commit, nonce}, SCM_RIGHTS[cred_fd])` — one
   `sendmsg(MSG_NOSIGNAL)` with a 64-byte cmsg buffer (`R:session.rs:1800-1837`).
5. `recv_ctl`: `recvmsg(MSG_CMSG_CLOEXEC)` into `CTL_MSG_MAX` (64 KiB); **VAL-5a**: own EVERY fd
   the kernel installed (count from `cmsg_len`, never "one per cmsg"); `MSG_CTRUNC` or `> 1` fd
   ⇒ `Protocol` and every received fd is closed; `n == 0` ⇒ `Socket(0)` (daemon gone)
   (`R:session.rs:1839-1902`). `SessionOk{geometry}` + memfd expected; `Refuse{class}` ⇒
   `Refused`; anything else ⇒ `Protocol`.
6. **VAL-4 rung 3**: `fcntl(memfd, F_GET_SEALS)` must contain `F_SEAL_SEAL | F_SEAL_SHRINK |
   F_SEAL_GROW` — without SHRINK a peer can `ftruncate` under the mapping and every later slot
   access is an in-app SIGBUS (`R:session.rs:578-586, 1718-1732`).
7. `SessionLayout::compute(&geometry)` (validates: ring entries power of two in `[2, 65536]`,
   `0 < slots ≤ ring_entries`, arena a 4 KiB multiple `≤ 2^40`, `0 < max_op ≤ arena`;
   `R:layout.rs:163-195, 258-302`).
8. **Map**: `thp::map_shared_pmd_aligned(memfd, total)` — over-reserve `len + 2 MiB`
   `PROT_NONE`, `MAP_FIXED|MAP_SHARED` the fd at the 2 MiB-aligned offset, trim head/tail
   (`R:thp.rs:87-160`); any failure ⇒ plain `mmap(PROT_READ|PROT_WRITE, MAP_SHARED)` ⇒ failure
   `Map(errno)`. Then `MADV_HUGEPAGE` (advise-only on the shim; refusals invisible) unless
   `SQUEEZEFS_IPC_ARENA_THP=0` (`R:session.rs:588-637`). The memfd is closed after mapping.
9. **Header validate** at offset 0: `magic == "SQZIPC01"`, `abi == IPC_ABI`, geometry valid AND
   equal to the reply's geometry; else `munmap` + `Protocol`. Capture `my_generation =
   header.generation` (`R:session.rs:640-647`; `R:layout.rs:364-378`).
10. `slab = geometry.slot_slab()` = `min(arena/slots, max_op)` floored to 4 KiB when ≥ 4 KiB (so
    `slot × slab` is DMA-eligible for every slot; `R:layout.rs:140-159`); 0 ⇒ `Protocol`.
11. `lane_gate_min = kernel_lane_min_for_geometry(slab, max_op)` (§8), published to the client
    stats page (`R:session.rs:659-686`).

Default geometry: `ring_entries = slots = 1024`, `arena = 64 MiB`, `max_op = 1 MiB` ⇒ slab
64 KiB (`R:layout.rs:94-96, 130-137`). Daemon-side the arena default is population-derived
`max(64 MiB, cap ÷ max(128, cpus × 8))` (`R:docs/design-preload-interception.md:403`).

### 3.2 Shared layout the client reads (`R:layout.rs:1-14, 239-255, 307-342`)

`[header page 4 KiB | ring: 128 B tail line + ring_entries × 8 B cells (page-rounded) | slots ×
128 B | client stats page 4 KiB | arena]`, all region offsets page-aligned, arena last. Header
line 0: `magic, abi, generation (AtomicU64, starts 1), heartbeat (AtomicU64)`; line 1:
`doorbell (AtomicU32), daemon_parked (AtomicU32), doorbell_coalescer`; line 2: `cqe:
CqeDoorbell{seq, parked, wake_at, wake_paid_mark}` (16 B, its own line); then the geometry copy.
Slot (128 B, `R:layout.rs:506-524`): `core: SlotCore{state: u32, _pad, generation: u64}`, `op,
flags: u32`, `binding, offset: u64`, `len: u32`, `ingress_stamp_ns: u32`, `arena_off: u64`,
`result: i64` — descriptor fields are Relaxed atomics ordered by the state word's edges.

### 3.3 One op, step by step (`submit_op` + `wait_consume` + `consume`, `R:session.rs:1087-1133, 1293-1382`)

1. `poisoned()` ⇒ `None` (fallthrough). `poisoned()` = flag OR `header.generation !=
   my_generation` (a daemon bump is the poison broadcast; latches the flag)
   (`R:session.rs:762-772`).
2. **Claim**: `claim_slot` scans from a rotating `slot_hint` for a FREE slot; `try_claim` = CAS
   FREE→CLAIMED (Acquire) then `generation += 1` (the ABA guard carried into every later check)
   (`R:session.rs:1384-1393`; `R:slot_core.rs:123-141`). No slot ⇒ `None`.
3. **Fill**: write payload into the slot's slab window `[slot × slab, +len)` (writes), then
   `publish_descriptor{op, flags=0, binding, offset, len, arena_off = slot × slab}`,
   `stamp_ingress(CLOCK_MONOTONIC low-32 ns)` (display-only residence instrument), then
   `publish_submitted` = `state.store(SUBMITTED, Release)` (`R:slot_core.rs:146-155`).
4. **Ring push**: bounded MPSC — reserve the tail cell by CAS, publish by storing the cell's
   `seq = pos + 1` (Release) (`R:ring_core.rs:166-214`). A `false` (full) is unreachable for an
   honest client (`slots ≤ ring_entries`) ⇒ OUR state is corrupt ⇒ `poison()`.
5. **Doorbell**: `header.doorbell.fetch_add(1, Release)`; `FUTEX_WAKE(doorbell)` iff
   `daemon_parked.load(SeqCst) != 0` (`R:session.rs:1127-1131`). The daemon sets `daemon_parked =
   1` for every owned session, snapshots the doorbells, re-scans (the disarm→scan law), then
   `futex_waitv` on all owned doorbells bounded by `SERVICE_PARK_MAX = 5 ms`, and clears the
   flag after (`R:src/ipc_host.rs:88, 3042-3090`) — so a wake lost to the race is bounded at 5 ms.
6. **Wait** (`wait_consume`): deadline = now + `op_timeout` (default 30 s,
   `SQUEEZEFS_IL_OP_TIMEOUT_MS`); adaptive spin: `WAIT_SPINS_DEFAULT = 4096` iterations of
   `is_done_for(gen)` + `spin_loop`, or the parky floor `256` when this session's LAST op parked
   (a parked op means daemon-side async work — the full window is CPU theft; measured 336 k →
   605 k IOPS at 64 threads with 256 the best floor) (`R:session.rs:44-74, 1322-1338`). An
   explicit `SQUEEZEFS_IL_SPINS` pins both. Then loop: `park_prepare` = `fetch_or(WAITER,
   AcqRel)`; prior DONE ⇒ `Ready`; else `FUTEX_WAIT(state, prior | WAITER, min(deadline − now,
   50 ms))` (shared futex, never PRIVATE), re-check `is_done_for`, re-check `poisoned()`
   (`Abandoned`); deadline reached ⇒ `poison()` + `Abandoned` (`R:session.rs:1339-1358`;
   `R:slot_core.rs:223-238`). The daemon's `complete()` = `swap(DONE, AcqRel)`; prior WAITER ⇒
   `FUTEX_WAKE(state, INT_MAX)` (breadth: a split submitter/reaper pair may both park on one word)
   (`R:slot_core.rs:190-201`; `R:src/ipc_host.rs:424-434`).
7. **Consume**: `result ≥ 0` ⇒ copy the slab out AFTER the DONE Acquire (reads), `release()` =
   `store(FREE, Release)`; `result < 0`: `EINVAL` ⇒ `Fallthrough` (a protocol-class reject means
   OUR bookkeeping diverged — the real call is the correct answer); any other errno ⇒ `Errno`
   (EBADF is a real fd property) (`R:session.rs:1361-1382`).
8. **Abandoned** ⇒ the slot is NEVER reused (the daemon may complete it arbitrarily late;
   recycling would hand a stale completion to a new op) — GC'd with the session.

### 3.4 Large ops: contiguous slot runs + pipelined flights (`ring_pread`/`ring_pwrite`, `R:session.rs:794-1079`)

`len ≤ slab` keeps the allocation-free serial path above. Larger: chunks size to the full
`max_op_bytes` window via `claim_run(want = ceil(chunk/slab))`: pass 1 tries bases aligned at
multiples of `want` (concurrent large writers PACK runs instead of interleaving — measured
ring/op 1.86 instead of 1.00 on the 16-thread 1 MiB row), pass 2 greedy from any FREE base; a
run never wraps the slot array; a shorter run under fragmentation is progress, never a refusal
(`R:session.rs:1395-1462`). Only the BASE slot is submitted; extension slots are CLAIMED arena
holds the daemon never sees, returned CLAIMED→FREE by `release_claimed` (`R:slot_core.rs:240-257`).
Every chunk is staged (claim + copy + publish) then the batch is ring-pushed under ONE doorbell
(sibling chunks landing in one drain pass is what lets the daemon's placed sever adopt a whole
block — measured ~50 % sever engagement pre-batch at t16×4 MiB; `R:session.rs:963-976`). Flights
reap oldest-offset first; POSIX prefix semantics: the count is the contiguous prefix ending at
the first short/failed chunk, later flights are drained and ignored; `Abandoned` mid-way ⇒
`Served(done)` if `done > 0` else `Fallthrough`; first-chunk `EINVAL` ⇒ `Fallthrough`
(`R:session.rs:866-920`). `SQUEEZEFS_IL_MAX_RUN_SLOTS` (testing only) caps the run.

### 3.5 No-wait tickets (the libaio ring lane) (`R:session.rs:1140-1291`)

`submit_pread_nowait(binding, len ≤ slab, off)` / `submit_pwrite_nowait` (the payload is copied
into the slab NOW — the app's buffer is free when `io_submit` returns) → `OpTicket{slot, gen}`;
`ticket_done(t)` non-consuming probe; `poll_ticket(t, out)` consumes on DONE: result > 0 copies
`min(res, out.len)` bytes, releases the slot, returns `res` (bytes or −errno; post-acceptance
there is no fallthrough — even `EINVAL` surfaces); `None` = in flight. An abandoned ticket is
never released.

### 3.6 The completion-doorbell park (register → snapshot → re-scan) (`R:session.rs:1217-1268`; `R:cqe_core.rs:1-70, 286-354`)

`cqe_park_begin_batch(k)`: `parked.fetch_add(1, SeqCst)`; `fence(SeqCst)`; `snap = seq.load`;
first parker ⇒ `wake_at.store(snap + k)`, else compose by EARLIEST live mark (a mark AT the
snapshot is replaced only once its pay is recorded in `wake_paid_mark`); returns `snap` as the
futex `expected`. The caller MUST re-scan its pending set before sleeping (a completion that
beat the registration is found there; one landing after fails the futex admission on the moved
seq or counts toward the mark). `cqe_park_end` = `parked.fetch_sub(1)`. `k =
reap_batch_wake_threshold(pending) = clamp(pending/4, 2, pending)` — FLOOR division (rounding a
wake threshold UP delays delivery), floor 2 = the first value distinguishable from the flat
event park, ceiling `pending` = the liveness cap (`R:sizing.rs:166-191`). Daemon side:
`complete(latch)` bumps `seq`, fences, `parked == 0` ⇒ Elided; `!reached(seq, wake_at)` ⇒
Elided; the mark-valued latch collapses repeat pays within one era (≤ 1 wake per park era)
(`R:cqe_core.rs:229-276`). The park entry also bumps the stats page's `il_park_eras`.

`wait_any(entries, timeout)` (`R:session.rs:1929-2017`): userspace admission pre-check (any word
already moved ⇒ return; 42 % of waits failed kernel admission at qd32 before this); one entry ⇒
`FUTEX_WAIT`; else `futex_waitv` (≤ `FUTEX_WAITV_MAX = 128` entries, `FUTEX2_SIZE_U32`, absolute
`CLOCK_MONOTONIC` deadline); `ENOSYS` (pre-5.16) latches a fallback: wait on the FIRST entry
capped at `WAITV_FALLBACK_QUANTUM = 200 µs`.

### 3.7 Heartbeat and the daemon-parked flag

`header.heartbeat` is declared "client-side death detection" (`R:layout.rs:316-317`) but the
shim never reads it and the daemon never writes it (grep: no `heartbeat` in the preload crate
or `ipc_host.rs`). Daemon death is detected by the per-op deadline (30 s → poison), the ctl
socket (EOF/ECONNRESET on the next bind), and the generation word (`R:docs/design-preload-
interception.md:400`). `daemon_parked` is the client's only wake gate (§3.3 step 5). See §11.

### 3.8 Bind, unbind, poison, teardown

- `bind(fd)` (`R:session.rs:689-719`): poisoned ⇒ `Poisoned`; under the ctl mutex `send_ctl
  (Bind, SCM_RIGHTS[fd])` → `recv_ctl`; an unexpected attached fd is closed and ⇒ `Protocol`;
  `BindOk` ⇒ grant; `BindRefused{class}` ⇒ `Refused`. The daemon closes its dup right after the
  screen — it needs `(st_dev, st_ino, flags, mode)`, not a live handle (`R:src/ipc_host.rs:
  2020-2022`).
- `unbind(id)` (`R:session.rs:724-731`): best-effort, silent, NEVER on a poisoned session (the
  atfork child must not unbind a binding the parent still uses).
- `poison()` same-process (`R:session.rs:733-745`): flag store + `shutdown(SHUT_RDWR)` on the ctl
  fd — never `close`, so the fd number cannot be reused under a racing ctl call.
- `poison_child()` (`R:session.rs:747-760`): flag store + `close(2)` of the child's inherited fd
  COPY — never `shutdown`, which acts on the description fork SHARES and would sever the
  parent's live session (pinned by `fork_child_poison_never_severs_the_parent_session`,
  `R:tests/preload_lifecycle_tests.rs:329`).
- Teardown: sessions are leaked for the process life (`Registry.insert` boxes and never frees;
  `R:interpose.rs:189-217`); `Drop` (`munmap` + `close`) runs only for a CAS-insert loser
  (`R:session.rs:1563-1574`). The daemon tears the session down on socket EOF (process exit),
  idle (`SQUEEZEFS_IPC_IDLE_SECS`, 300 s: generation bump, client lazily re-establishes), or
  poison (`R:src/ipc_host.rs:1540-1556, 2578-2592`).

### 3.9 What the shim expects of the daemon (the counterpart, to the extent needed)

HELLO ladder order: `data_plane` off ⇒ `Disabled`; `abi != IPC_ABI || build_commit != ours` ⇒
`Version`; degenerate identity on either side without `allow_dev` ⇒ `Version`; nonce not
current/previous (60 s TTL rotation) ⇒ `Nonce`; `SO_PEERCRED` unreadable ⇒ `Internal`;
claimed `(pid, uid) != kernel's` ⇒ `Peercred`; no credential fd ⇒ `Flags`; the fd screen
(`O_PATH` ⇒ Flags; `!S_ISREG` ⇒ Flags; `st_dev != mount` ⇒ Mode; `O_APPEND|O_SYNC|O_DSYNC|
O_TMPFILE` ⇒ Flags; rights from `O_ACCMODE`; `odirect = O_DIRECT`); budget/per-uid cap ⇒
`Budget` (`R:src/ipc_host.rs:2175-2300, 2452-2504`). Per-op serve: `try_begin_serve` (SUBMITTED→
SERVING preserving WAITER; failure = protocol violation ⇒ session poisoned), ONE descriptor
snapshot, validation of the copy — unknown op / dead binding / `len > max_op` / arena bounds ⇒
`-EINVAL` (`ipc_descriptor_rejects`), wrong direction ⇒ `-EBADF`
(`R:src/ipc_host.rs:979-1006, 1064-1117`). `on_bind(ino)` fires the bind-time invalidation;
`on_last_unbind(ino)` the whole-inode one (`R:src/ipc_host.rs:2004-2006, 2043-2047`). The stats
page is summed verbatim into the stats inode, display-only (`R:src/ipc_host.rs:1765-1771`).

## 4. The bail-out ladder — every failure is indistinguishable from the real call

| Condition | Where | Action |
|---|---|---|
| Not FUSE / foreign FUSE / undecodable blob | classify | negative-cache `st_dev`; passthrough forever (`R:interpose.rs:506-531`) |
| `O_PATH`, non-regular, `O_APPEND`, `O_SYNC/O_DSYNC`, `O_TMPFILE`, `nlink == 0` | classify (both the credential and each bind) | never bound (`R:bailout.rs:23-57`) |
| Version skew / dev identity / untrusted rendezvous / peer uid / unsealed memfd / socket error / protocol / map | establish | one reason line per (mount, reason); mount stays ours; RETRIED on every eligible open (`R:interpose.rs:550-563`) |
| Daemon BIND refusal (any class) | bind | fd stays kernel-served; one line per (mount, class) (`R:interpose.rs:590-602`) |
| Session registry full (32) | establish | mount stays passthrough, loud once (`R:interpose.rs:212-216`) |
| `MAP_SHARED` inode in this process | classify | never bound; set never forgets; overflow ⇒ "poison everything" (`R:mapped_inos.rs:19-29, 122-162`) |
| RWF flag outside `{RWF_HIPRI}` | per op (`*v2`, iocb) | that op real; binding stays (`R:bailout.rs:59-75`) |
| Hybrid gate (`len > threshold`) | per op / per description | real call, counted; binding stays (§8) |
| No slot claimable (ring full), op shape (`buf == NULL`, `count == 0`, `offset < 0`) | per op | that op real; binding stays (`R:session.rs:1302-1305`; `R:interpose.rs:634-637`) |
| Daemon `-EINVAL` on the first chunk | per op | real call (our bookkeeping diverged) (`R:session.rs:1368-1375`) |
| Daemon errno other than EINVAL | per op | surfaced verbatim (kernel-identical for that op) |
| Op deadline (30 s) / generation bump / ring-push impossible | per op | session POISONED (flag + `shutdown`); the op real; fd demoted + unbound; every later op on the session falls through immediately (`R:session.rs:1343-1346, 1117-1121`) |
| Panic in any interposer body | per call | all sessions poisoned, one stderr line, real call (`R:interpose.rs:347-350`) |
| `mmap` on a bound fd | interposed | unbind every in-process binding of the inode, flush mirrors, poison the inode for `MAP_SHARED`, then the real mmap (§1.10) |
| `fcntl` locks / `O_APPEND` / `O_DIRECT` toggle | interposed | flush + unbind, then the real call (§1.9) |
| `fork` | atfork child | flag-poison + `close(2)` the socket copy; registry slots cleared so the child lazily establishes its OWN session; stale fd entries route to empty registry slots ⇒ passthrough (`R:interpose.rs:231-249`) |
| `exec` | inherent | ctl socket and memfd are CLOEXEC (`R:session.rs:1631, 1853`), the mapping dies with the image; inherited fds are unknown ⇒ passthrough |
| Raw-syscall / `IORING_OP_CLOSE` close + number reuse | creator sweep, `close_range` | released as `close` would; the residual mixed-ABI shape is documented unsupported (`R:docs/design-preload-interception.md:319`) |

Law (`R:docs/design-preload-interception.md:321-323`): at any instant an fd is in exactly one of
{unbound, bound, poisoned}; in all three every intercepted symbol produces a result
indistinguishable from the real call (bound: the daemon through the paths kernel FUSE reaches;
otherwise literally the real call). Tests: the plain-file passthrough battery (cp/dd/cat/tar
byte parity + exit codes under `LD_PRELOAD` on a foreign fs, `R:tests/run_preload_gate.sh:
65-82`), `stalled_serve_times_out_poisons_and_falls_through`,
`explicit_poison_falls_through_and_stops_ctl_traffic` (`R:tests/preload_session_tests.rs:527,
895`), `bind_refuses_directory_and_wrong_direction_is_ebadf` (`:474`), the VAL-4 suite's
"refusal AND the credential fd stayed home" legs (`R:tests/preload_authn_tests.rs:26-29`), the
libaio establish-refused row 2k (`R:tests/run_preload_gate.sh:594-601`).

## 5. Process-lifecycle mechanics

- **`fork`**: `pthread_atfork(prepare, NULL, child)` registered once, lazily, from the first
  `open*` (`R:interpose.rs:1379-1402`). PREPARE (parent, pre-fork): epoch bump + flush every
  armed mirror so the child inherits and the demoted parent resyncs from true kernel offsets;
  AS-safe (atomics, bounded `try_lock`, raw `SYS_lseek`). CHILD: `poison_all_in_child` (§3.8;
  the child is single-threaded here). The inherited arena mapping stays mapped (munmap is not
  AS-safe), reclaimed at exit/exec (`R:docs/design-preload-interception.md:314`). The parent's
  bindings are demoted (resync discipline), never unbound.
- **`vfork`/`clone`**: not interposed; a vfork child shares memory and may only exec — any
  bookkeeping would be UB; `posix_spawn` (clone-based) gets the explicit flush (§1.7).
- **`exec`**: fresh shim init if `LD_PRELOAD` persists; nothing survives but CLOEXEC-free bound
  fds, which are unknown to the new image ⇒ passthrough (§4). The daemon sees socket EOF
  (CLOEXEC) and tears the session down.
- **Threads**: data path lock-free (fd lookup = up to three loads; slot CAS; ring CAS; futex on
  the slot's own word); the only mutexes are the per-description offset lock (interposer
  contexts only), the ctl mutex (open cadence), the `RefusalOnce` set, and one mutex per libaio
  ctx (`R:session.rs:5-14`; `R:fd_table.rs:5-14`).
- **Signals**: positional interposers are AS-safe post-init (`close` is AS-safe by POSIX and
  apps call it from handlers — hence NO mutex guards any fd-table operation; a handler entering
  the shim over an interposed frame on the same thread finds the guard set and takes the real
  call) (`R:fd_table.rs:5-14`); `EINTR`/`EAGAIN` from futex waits re-check under the deadline
  (`R:session.rs:2034-2051`). `io_pgetevents`' sigmask is NOT applied during the shim's own
  waits (strictly more wakeful; `R:interpose.rs:2880-2884`).
- **`dlopen` ordering / constructors**: the shim is **ctor-free** — every global is a lazily
  initialized `OnceLock`/atomic behind the first interposer call; `dlsym(RTLD_NEXT)` is
  position-relative so it is correct wherever the object sits in the link map
  (`R:docs/design-sdk.md:99`). Other objects' constructors calling interposed symbols before
  `main` are served by the lazy init (gate row 1e ctor-context I/O,
  `R:tests/run_preload_gate.sh:128-141`).
- **Symbol visibility**: the libc names are strong default-visibility exports, only compiled
  into the shipped `.so` (`--features interposers`) — an rlib carrying them would interpose
  INSIDE any test binary that links it (`R:Cargo.toml:60-73`; `R:src/bin/aio_lifecycle_harness.rs:
  36-44`). The shim's own libc calls (`socket`, `fcntl`, `mmap`, …) resolve against its own
  exports; the held guard routes them to the real functions (`R:interpose.rs:11-16`).
- **SONAME `libsqueezefs_il.so`** (`-Wl,-soname`) so by-path links record a clean `DT_NEEDED`;
  **`-z nodelete`** because `pthread_atfork` handlers can never be unregistered — `dlclose`
  must be a no-op (`R:build.rs:54-68`; asserted by `readelf -d` in the gate,
  `R:tests/run_preload_gate.sh:119-122`).
- **No-panic law**: every body under `catch_unwind`; the crate refuses to build with
  `panic = "abort"` (`#[cfg(panic = "abort")] compile_error!`, `R:src/lib.rs:11-20`) and ships
  only from the `preload-release` profile (`R:Cargo.toml:9-19`). The libaio async arms must NOT
  replay the real call (§6.6).
- **Build identity**: `SQUEEZEFS_IL_BUILD_COMMIT = <git rev-parse HEAD>[-dirty]` (or
  `SQUEEZEFS_BUILD_COMMIT`/`SQUEEZEFS_BUILD_DIRTY` packager env, `unknown` without git), with
  `rerun-if-changed` on `HEAD`/`refs`/`packed-refs` (`R:build.rs:1-52`).

## 6. libaio interposition in full

### 6.1 Resolution (`real_aio!`, `R:interpose.rs:399-446`)

`dlvsym(RTLD_NEXT, sym, version)` with the symbol's DEFAULT version table — `io_setup`/
`io_destroy`/`io_getevents`/`io_cancel` `LIBAIO_0.4`, `io_submit` `LIBAIO_0.1`, `io_pgetevents`
`LIBAIO_0.5` — plain `dlsym` only when the version is absent. Why: glibc < 2.36
`dlsym(RTLD_NEXT)` returns the BASE version of a multi-versioned symbol; on EL8 that is
`io_getevents@LIBAIO_0.1`, a 4-argument compat wrapper whose PLT call re-enters the
interposer; called with the 5-argument convention it faulted inside libaio (the 2026-07-25
Rocky 8 field segfault) (`R:aio_glue.rs:71-98`). Return convention is libaio's: 0-or-count,
NEGATIVE errno (never -1/errno) (`R:interpose.rs:1954-1956`).

### 6.2 Userspace ABI mirrors (`R:aio_glue.rs:22-65`)

`RawIocb` = libaio.h's `struct iocb` (NOT the kernel `aio_abi.h` shape): `data u64, key u32,
aio_rw_flags u32, aio_lio_opcode u16, aio_reqprio u16, aio_fildes i32, buf u64, nbytes u64,
offset i64, __pad3 i64, flags u32, resfd u32`. `RawIoEvent` = `data u64, obj u64 (the
ORIGINAL iocb pointer), res i64, res2 i64`. Layout pinned by size/offset tests
(`R:tests/aio_glue_tests.rs:79-116`) — drift is memory corruption inside a host.

### 6.3 Per-iocb screen (`screen_iocb`, `R:aio_glue.rs:112-136`; `classify_iocb`, `R:aio_core.rs:77-83`)

Allow-list: `IOCB_FLAG_RESFD` ⇒ Kernel (an eventfd completion the ring cannot signal);
`rwf_passthrough(aio_rw_flags)` ⇒ Kernel; `nbytes == 0 || nbytes > slab` ⇒ Kernel (one async op
= one slot, no chunking — a multi-slot async op would hold slots hostage); then `PREAD(0)` on a
bound fd with `read_ok`, `PWRITE(1)` with `write_ok` ⇒ Ring; every other opcode (vectored,
fsync, poll) ⇒ Kernel. The interposer runs the screen size-blind (`slab = u64::MAX`) and
applies the structural slab law and the hybrid gate in ONE place so every size-based kernel
route is counted (`R:interpose.rs:2301-2324`).

### 6.4 Context registry (`AioCtxRegistry`, `R:aio_glue.rs:140-263`)

256 slots of `(id: AtomicU64, state: AtomicPtr<Mutex<AioCtx>>)`; `io_context_t` 0 is "empty",
`u64::MAX` the claim sentinel. `register` FIRST `remove`s any existing entry for the value —
**collision retro-neutralization**: the kernel never hands out one live ctx twice, so a
colliding register is a recycled ring address whose `io_destroy` bookkeeping was missed; the
corpse's state must never own the new real context. Slot protocol: claim `0 → RESERVED`, store
fresh state, publish the id. At capacity the ctx passthroughs wholesale. State boxes are leaked
(a data-path reference never dangles). Per ctx ONE mutex over `{AioCtxState, tickets:
Vec<Option<LiveTicket{session, ticket, is_read, buf, len}>>}` (`R:interpose.rs:1992-2011`).

### 6.5 The mixed-batch core (`AioCtxState`, `R:aio_core.rs:92-374`)

- `submit_batch` (`:170-260`): walk the batch; Kernel iocbs accumulate into a contiguous run;
  a Ring iocb FIRST flushes the open run via `kern.submit_run` (prefix law: nothing later ever
  dispatches first); a partial kernel run ends the prefix there; a kernel errno on the FIRST op
  of the batch ⇒ `Errno(e)`, later ⇒ `Submitted(accepted)`; `ring.try_submit` `None` (no slot /
  poisoned) ⇒ the iocb is REROUTED into the open kernel run at its batch position — never
  `-EAGAIN`, never a truncated prefix (`:229-242`). The `data` cookie is captured AT submit.
- `getevents(min, max, timeout)` (`:288-363`): harvest every complete ring op first; if
  `≥ min` or `≥ max` return (plus an opportunistic non-blocking kernel drain when room and
  kernel pendings exist); else a bounded poll-slice loop: `kmin = min(need, kernel_pending,
  room)`; kernel wait = the remaining budget only when NO ring ops are pending and `kmin > 0`,
  else a 5 ms slice capped by the budget (a zero-timeout call is ONE non-blocking pass); with
  ZERO kernel pendings the real `io_getevents` is skipped entirely (a pure-ring pass used to pay
  one wasted syscall); `timeout == None` ⇒ a 30 000 ms default rather than literal forever.
- `destroy` (`:367-373`): abandon every ring pending (slots GC'd with the session), mark
  destroyed — a destroyed state serves nothing and the reap path forwards it to the real call
  (never merge-loops on empty harvests).

### 6.6 The interposers (`R:interpose.rs:2189-2957`)

- `io_setup`: real call; `r == 0` ⇒ `register(*ctxp)` under the guard; a panic mid-register ⇒
  poison + retro-neutralize (the value's definite owner is the real kernel).
- `io_destroy`: bookkeeping FIRST and unconditionally (libc-free, runs even without the guard
  and even when the real symbol is missing — a skipped vacate is the field crash class): vacate
  the registry, `state.destroy(ring)`; then the real call.
- `io_submit`: `nr ≤ 0 || ios == NULL` ⇒ real; unregistered ctx ⇒ real; classify every iocb
  (`id == 0` ⇒ Kernel — the real call faults it); a pure-kernel batch ⇒ the real call verbatim;
  else `submit_batch` under the ctx lock with `SessionRing{tickets}` (`try_submit` = fd-table
  lookup → session → `submit_p{read,write}_nowait`; `None` ⇒ `note_slot_reroute()` — counted only
  when not poisoned) and `RealKernel{ctx}`. **Panic arm**: a panicked body may have dispatched
  part of the batch — replaying the real call would double-submit ⇒ `panic_poison`,
  retro-neutralize the ctx, return `-EIO` (`R:interpose.rs:2343-2359`).
- `io_getevents`/`io_pgetevents` → `aio_reap_served` (`R:interpose.rs:2371-2686`): `nr ≤ 0 ||
  events == NULL` ⇒ real; unregistered ⇒ real; destroyed OR nothing tracked in flight ⇒ real
  (exact semantics for contexts that never touch SqueezeFS fds); deadline from the caller's
  timespec read ONCE. Loop, one merge pass per lock hold: ring-involved passes carry a ZERO
  budget (harvest + kernel probes), the kernel-lane-only shape waits inside the pass ≤ 50 ms;
  copy events into the caller's array at `got + i`; `got ≥ min_nr || got ≥ nr` ⇒ return `got`;
  deadline ⇒ return `got` (libaio allows fewer than `min_nr`). Waiting policy OUTSIDE the lock:
  nothing tracked ⇒ sleep 200 µs (a racing submitter is the only thing that can change it);
  **deep regime** (`pending > REAP_EVENT_PARK_MAX = 24`): per distinct session
  `cqe_park_begin_batch(k = clamp(pending_here/4, 2, pending_here))`, mandatory re-scan,
  `wait_any(entries, min(deadline, quantum = 50 µs))`, `cqe_park_end`; **sparse regime**: 4
  sweeps of `ticket_done` spin (sweep-bounded, not iteration-bounded — 64 sweeps over qd32 was
  2048 remote-line probes per empty pass), then the same park with `k = sparse_park_k` (1 for
  `pending ≤ 1` or `SQUEEZEFS_IL_SPARSE_BATCH_MARKS=0`, else the derived k), bound = 1 ms when
  kernel ops are pending (kernel completions cannot wake a futex), 5 ms ring-only, tightened to
  the quantum when any `k > 1`. No waitable session (all poisoned) ⇒ sleep 200 µs; the next pass
  resolves the tickets as `-EIO`. **Panic arm**: a panicked merge may have consumed tickets and
  written a prefix of the caller's array ⇒ poison, retro-neutralize, return `-EINTR` (never the
  real call) (`R:interpose.rs:2854-2865`).
- `io_cancel`: a ring-pending iocb ⇒ `-EINPROGRESS` (a ring op cannot be recalled); else the real
  call; its panic arm KEEPS the real-call fallback (read-only body) (`R:interpose.rs:2920-2957`).
- Poison law for the async shape: a poisoned session resolves in-flight tickets as `-EIO`
  events (never stranded, never a hang); the slot is never recycled (`R:interpose.rs:2080-2088`).

### 6.7 The lifecycle harness (`R:src/bin/aio_lifecycle_harness.rs`)

Resolves `libaio.so.1` through the GLOBAL scope (`dlopen(RTLD_GLOBAL)` + `dlsym(RTLD_DEFAULT)`)
so a preloaded shim's interposers win exactly as in a host app; carries LOCAL ABI mirrors (never
links the shim's rlib — the self-interposition tripwire). Modes `setup-first` (ctx before any
file on the target — the field crash shape), `open-first`, `mixed`; every thread runs phases of
`io_setup → qd-deep PWRITE/PREAD batches → io_getevents drain → io_destroy` and re-`io_setup`s
so the kernel recycles ring addresses; asserts per event cookie identity, original-iocb
identity, exact `res`, byte-exact read-back, and per run `events == submits` (the
double-dispatch tripwire). Exit 2 = no `libaio.so.1` (skip loudly). Gate rows 1d (plain fs
passthrough), 2j (armed mount, served merge path), 2k (establish-refused shape: full lifecycle
green, zero ipc data ops) (`R:tests/run_preload_gate.sh:83-107, 580-601`).

### 6.8 Measured

Device-true fio libaio jobs=16 qd=32 median **633,183 IOPS**, beating `-t 256` sync's 622,112 at
1/16th the client threads (`R:docs/design-preload-interception.md:439, 569`). Reap park max 24 →
2 → 24 history and the 1×qd8 75.5 k → 344.8 k IOPS sleep-vs-event-park decomposition
(`R:interpose.rs:2701-2730, 2793-2805`).

### 6.9 libaio's queue face (the port's addition; rung-9 finding F3)

libaio binds its INTERNAL calls locally — Ubuntu links it `-Bsymbolic-functions`, and
`objdump -R libaio.so.1` shows no `io_*` relocation — so `io_queue_init` reaches libaio's own
`io_setup`, never the shim's: fio's libaio engine (`io_queue_init(iodepth, &ctx)`) created a
context the registry never learned and every `io_submit` on it was an uncounted passthrough
(`LD_DEBUG=bindings` showed `io_submit`/`io_getevents` bound to the shim while `ipc_ops_*`
stayed 0). The port interposes the four symbols at their exported default versions
(`io_queue_init@LIBAIO_0.1`, `io_queue_release@LIBAIO_0.1`, `io_queue_run@LIBAIO_0.1`,
`io_queue_wait@LIBAIO_0.4`) with libaio's OWN bodies over the shim's wrappers — never the real
symbols: `io_queue_init` = `maxevents ≤ 0 ⇒ −EINVAL; *ctxp = 0; io_setup`; `io_queue_release`
= `io_destroy`; `io_queue_run` = `while io_getevents(ctx, 0, 1, &ev, &{0,0}) == 1:
((io_callback_t)ev.data)(ctx, ev.obj, ev.res, ev.res2)` (a zero cookie is skipped — libaio
would jump to 0); `io_queue_wait` = `io_getevents(ctx, 0, 0, NULL, timeout)`. The export
table is 65 interpositions + the API.

An unregistered context (past the 256-slot registry, or created by a path no interposition
saw — a static libaio, a raw `io_setup`) is a COUNTED passthrough: `io_submit` attributes
every iocb on a bound fd to its binding's session page (`aio_unknown_ctx_op`, row 21 —
folded into the daemon's `ipc_client_aio_unknown_ctx_ops` at session teardown, the
engagement law's missing term), `io_getevents`/`io_cancel` count per call
(`aio_unknown_ctx_call`, row 22). Measured on the venue (fio 3.36 libaio QD32 × 4, rand 4 KiB
reads, file and nvmet-loop substrates): `Δipc_ops_read 194,299 ≡ fio ops 194,299` and
`166,973 ≡ 166,973`, `ipc_client_aio_unknown_ctx_ops 0`.

## 7. Linked mode (SDK Tier 1, `R:docs/design-sdk.md:95-108`)

"Linked" = the shim pulled in by the dynamic linker other than via `LD_PRELOAD`: a `DT_NEEDED`
direct link (`-lsqueezefs_il`, wrapped in `-Wl,--no-as-needed` because modern toolchains drop an
entry the app references no symbol of), `dlopen`, or `/etc/ld.so.preload`. Because the shim is
ctor-free and `RTLD_NEXT` is position-relative, linked mode differs from preload in exactly one
way: other objects' constructors may call interposed symbols before `main` — served by the lazy
init. Two gaps found and fixed: no SONAME (by-path links embedded the build-tree path) and no
`-z nodelete`. Detection is purely diagnostic: `load_mode_from(LD_PRELOAD)` splits on `:`/` `,
matches the BASENAME by prefix `libsqueezefs_il` (versioned `.so.X` names match; a directory
component named like the shim does not); absent/empty/foreign-only ⇒ `Linked`
(`R:session.rs:129-168`). On the first blob decode the shim prints ONCE: `squeezefs-il: active via
direct link (DT_NEEDED), not LD_PRELOAD — same KD-7 build pairing applies`
(`R:interpose.rs:352-370`). A setuid/AT_SECURE binary whose IGNORED environment names the shim
misreports as Preload — the line is never a correctness input. KD-7 is mode-independent: a
linked app must still ship its `.so` with the daemon. Gate: 1e (SONAME/NODELETE `readelf`,
cc-compiled harness without `LD_PRELOAD`: ctor-context pwrite/pread round trip, `dladdr`
proof that `pread64` resolves into the shim, byte parity), 2b-linked (ring engagement +
the line exactly once) (`R:tests/run_preload_gate.sh:108-170, 312-333`).

## 8. The hybrid lane gate (`R:lane_gate.rs`; `R:sizing.rs:107-164`)

Model: `ring_time(size) ≈ ring_rtt + copies × size / memBW`; `kernel_time ≈ fuse_rtt` (the zc
kernel lane moves payload at line rate). Equal at `size* = memBW × (fuse_rtt − ring_rtt) /
copies`. `KERNEL_LANE_RTT_DELTA_NS = 2_300` — a MEASURED class constant: the D14 field bracket's
rand-4k rows, ring 594 k IOPS vs kernel ≈ 253 k at the same shape ⇒ `1/253k − 1/594k ≈ 2.27 µs`;
`KERNEL_LANE_RING_COPIES = 2` (the daemon's serve-into-arena copy + the client's arena→app
copy; writes pay one client copy and collapse to the same floor at every measured memBW, so
ONE threshold derived on the read side governs both). Rails, each physical: rounded DOWN to the
4 KiB grain (composes with O_DIRECT alignment); floor = slab (at or below the slab a ring op is
the single-flight allocation-free path — the IOPS lane the gate exists to protect); ceiling =
`max_op_bytes` (past it an op chunks into multiple flights). memBW = a startup micro-probe:
best-of-3 `memcpy` of a 4 MiB buffer (larger than L2 on every target part), ~1 ms once per
process, memoized, run lazily at the first establish under the held guard — never in a
constructor; 0 on degenerate timing ⇒ the slab floor (`R:lane_gate.rs:64-100`). Values:
~15 GB/s DRAM-class ⇒ 64 KiB floor; 100 GB/s ⇒ 112 KiB; the dev box derived **140–164 KiB**
(probe variance ~15 % run to run), the field box 86,016 B, gate runs 143,360 / 167,936 B
(`R:sizing.rs:312-329`; `R:.benchmarks/2026-08-07-shim-hybrid-lane-gate.md:41, 74, 89, 156`).
`SQUEEZEFS_IL_KERNEL_LANE_MIN` explicit wins VERBATIM (0 = gate off, the A/B lever; sub-slab
allowed), malformed announces once and keeps the derived default (`R:lane_gate.rs:53-62, 106-124`).

Predicate: `threshold != 0 && len > threshold` (STRICTLY greater) (`R:lane_gate.rs:49-51`).
Placement: POSITIONAL forms (`pread*`, `preadv*`, libaio iocbs) gate PER OP (no `f_pos`
state); OFFSETFUL forms (`read`/`write`/`readv`/`writev`) gate through the per-DESCRIPTION
sticky latch: the first over-threshold op latches the cell (dup siblings share it), flushes
and permanently demotes the mirror ONCE; every later offsetful op on that description takes
the real call at native cost — no per-op flapping (each ring→real transition would re-price
the fd at two syscalls forever) (`R:lane_gate.rs:27-39`; `R:fd_table.rs:127-138, 200-212`).
A vector op gates on its total bytes. Every route is counted on the client stats page
(`lane_gate_kernel_routes/bytes`, threshold gauge published at establish; the daemon exports
them as `ipc_lane_gate_kernel_{routes,bytes}` and `ipc_lane_gate_threshold_bytes` = max over
LIVE sessions — a gauge that drops with them) (`R:layout.rs:414-453`). A/B measured: psync
1 MiB reads +66 % with the gate ON, rand-4k parity (1.03–1.04×)
(`R:.benchmarks/2026-08-07-shim-hybrid-lane-gate.md:8`). O_DIRECT is deliberately not consulted.

## 9. Knobs, gauges, constants

### 9.1 Every knob the shim reads (client side; the shim never refuses its host over a value — it announces once and keeps the default: the ENG-10 asymmetry, `R:crates/squeezefs-ipc/src/env_knob_core.rs:17-23`)

| Knob | Read at | Default / range | Class |
|---|---|---|---|
| `SQUEEZEFS_IL_SESSIONS` | first classify | derived `clamp(ceil(cpus/4), 2, 16)`; override clamped `1..=16` (`R:interpose.rs:106-131`) | derived override |
| `SQUEEZEFS_IL_OP_TIMEOUT_MS` | establish | 30 000; `> 0` (`R:session.rs:40-42, 457-460`) | A/B lever (control arm = the 30 s bound) |
| `SQUEEZEFS_IL_SPINS` | first wait | adaptive `(4096, 256)`; explicit pins both (`R:session.rs:50-74`) | A/B lever |
| `SQUEEZEFS_IL_MAX_RUN_SLOTS` | first claim_run | 0 = uncapped; TESTING ONLY (`R:session.rs:76-92`) | test seam |
| `SQUEEZEFS_IL_REAP_PARK_MAX` | first reap | 24; clamp `0..=4096` (`R:interpose.rs:2701-2730, 2779-2791`) | A/B lever |
| `SQUEEZEFS_IL_REAP_QUANTUM_US` | first reap | 50; clamp `1..=1000` (0 would busy-spin) (`R:interpose.rs:2793-2825`) | A/B lever |
| `SQUEEZEFS_IL_SPARSE_BATCH_MARKS` | first reap | on; `=0` the A/B control (`R:interpose.rs:2732-2777`) | A/B lever |
| `SQUEEZEFS_IL_KERNEL_LANE_MIN` | establish | derived (§8); explicit verbatim; 0 = off (`R:lane_gate.rs:53-62`) | derived override |
| `SQUEEZEFS_IPC_ALLOW_DEV` | establish | off; announced once when on (`R:session.rs:329-353`) | switch (both ends) |
| `SQUEEZEFS_IPC_ARENA_THP` | establish | on (`R:session.rs:617-636`) | switch (both ends) |
| `LD_PRELOAD` | first blob decode | diagnostic only (§7) | — |

`SQUEEZEFS_IL_PARANOID_FSTAT` is named in the design (`R:docs/design-preload-interception.md:
319, 470`) but never implemented (grep: absent from the crate). Already registered in the port:
`SQUEEZEFS_IL_*` rows 143–155 and `SQUEEZEFS_IPC_ALLOW_DEV`/`ARENA_THP`
(`P:docs/porting/knobs_and_rails.md:372-400`).

### 9.2 Client stats page (display-only, client-writable; `R:layout.rs:402-486`)

`lane_gate_kernel_routes`, `lane_gate_kernel_bytes`, `lane_gate_threshold_bytes` (gauge),
`il_submit_harvested` (0 — the reap-on-submit scout never landed in the shim),
`il_park_eras`, `il_slot_reroutes` (true slot exhaustion only; gated on `!poisoned()`). The
daemon sums them verbatim; the trusted family is `ipc_*` (`R:docs/design-preload-
interception.md:504`). The `preload_passthrough_ops{reason}` / `preload_bailouts_*` /
`preload_bind_refused{reason}` counters the design §8 names (`:503`) do NOT exist in the shipped
page — the refusal stderr lines are the only client-side attribution (§11).

### 9.3 Constants with WHY and measured numbers

| Constant | Value | Why / measurement |
|---|---|---|
| `FUSE_SUPER_MAGIC` | `0x65735546` | Linux `statfs.f_type` for FUSE (`R:interpose.rs:58-59`) |
| `SESSION_REGISTRY_SLOTS` | 32 | the fixed static session table the alloc-free interposer needs; per-mount ceiling = half (two concurrently bound mounts' headroom) (`R:sizing.rs:43-50`) |
| `il_sessions_default` | `clamp(ceil(cpus/4), 2, 16)` | cpus/4 = the measured drain-thread saturation slope (2026-07-19); flat 4 vs cpus/4 threads left half the drain idle at 7.5 of 16.6 GB/s, sessions=8 by hand +44 % (`R:sizing.rs:1-41`) |
| `OP_TIMEOUT_DEFAULT` | 30 s | a daemon that stopped completing is sick ⇒ poison; the design named 1 s (`R:session.rs:40-42`; `R:docs/design-preload-interception.md:400`) |
| `WAIT_SPINS_DEFAULT` / `_PARKY` | 4096 / 256 | RTT single-digit µs (G-L4-1: echo p50 0.79–1.54 µs, syscalls/op 0.0001–0.0005); the parky floor measured 336 k → 605 k IOPS at 64 threads (`R:session.rs:44-59`; `R:.benchmarks/2026-07-18-l4-ipc-hop-ceiling.md:69-73`) |
| park slice | 50 ms | re-check cadence under the deadline (`R:session.rs:1348`) |
| `CTL_RECV_TIMEOUT` | 10 s | a dead daemon must not hang a bind; the daemon's handshake deadline is matched to it (`R:session.rs:94-95`; `R:src/ipc_host.rs:98-102`) |
| `SERVICE_PARK_MAX` (daemon) | 5 ms | bounds the client's lost-doorbell race (`R:src/ipc_host.rs:86-88`) |
| `KERNEL_LANE_RTT_DELTA_NS` | 2 300 | §8 |
| `REAP_EVENT_PARK_MAX` | 24 | §6.8 history, re-counted A-B-B-A: write 32×8 +14 % at 24 vs 2 (`R:interpose.rs:2720-2730`) |
| `REAP_BATCH_QUANTUM_US` | 50 | the park's age bound = the shipped blind-sleep posture (`R:interpose.rs:2793-2805`) |
| `KERNEL_LANE_SLICE` / `RING_PARK_RECHECK` | 1 ms / 5 ms | kernel completions cannot wake a futex / cross-thread submits on unregistered sessions (`R:interpose.rs:2688-2699`) |
| `FUTEX_WAITV_MAX`, `WAITV_FALLBACK_QUANTUM` | 128, 200 µs | kernel cap; pre-5.16 degradation (`R:session.rs:1919-1927`) |
| `REFUSAL_ONCE_CAP` | 128 | stops remembering, never printing (`R:session.rs:369-372`) |
| fd table geometry | 1024 fds/segment × 4096 segments | fds < 2^22 — beyond any `RLIMIT_NOFILE` (`R:fd_table.rs:253-258`) |
| `NegativeDevCache` | 1024 × 4-way | 8 KiB; ~100–200 ns per foreign open (`R:dev_cache.rs:15-20`) |
| `MappedInoSet` | 8192 slots, never evicts | a lost entry re-arms the ring under a mapping = corruption (`R:mapped_inos.rs:19-29, 47-50`) |
| `CTX_CAP` | 256 | fio holds one ctx per job (`R:aio_glue.rs:142-146`) |
| fork-flush try_lock spin | 1024 | an in-flight offsetful op holds the lock only for its serve window (`R:fd_table.rs:444-453`) |
| bootstrap read buffer | 512 B | the blob is 308 B (`R:interpose.rs:512`) |

## 10. Tests (name — one sentence; ★ = needs a real mount / root)

Crate-local (hermetic, `R:crates/squeezefs-preload/tests/`):
- `core_tests`: `bind_lookup_close_roundtrip`; `dup_propagates_and_close_original_keeps_dup_bound`;
  `dup2_over_live_binding_releases_the_clobbered_one`; `bind_over_stale_entry_releases_it_first`;
  `close_range_sweeps_inclusive_range`; `hygiene_sweep_is_refcount_aware_composition` (bind → dup
  → raw-close one → creator reuses the number → close the dup ⇒ exactly one unbind);
  `table_grows_to_large_fds_and_negative_fd_is_none`;
  `concurrent_dup_close_stress_unbinds_exactly_once`;
  `unbind_ino_walk_releases_every_sibling_in_process`; `classify_mirrors_the_daemon_screen_rows`;
  `rwf_screen_passes_known_safe_and_refuses_the_rest`;
  `negative_dev_cache_remembers_and_never_false_positives`; `…_is_bounded_evicts_gracefully`;
  `…_concurrent_probe_insert_stress`.
- `offset_mirror_tests`: `plain_bind_leaves_mirror_unarmed`; `bind_with_mirror_arms_and_tracks_offsets`;
  `stale_epoch_bind_never_arms`; `dup_shares_one_mirror_across_fd_numbers`;
  `dup_serialization_uses_one_lock`; `fork_demote_flushes_each_armed_cell_exactly_once`;
  `fork_demote_never_rewinds_a_kernel_authoritative_fd`;
  `disarm_if_current_captures_latest_offset_exactly_once`;
  `publish_after_demote_reports_write_through_needed`; `unbind_ino_reports_armed_flushes_per_cell`;
  `mirror_seek_matches_the_kernel_arithmetic`; `o_append_refusal_is_unchanged`;
  `unbound_fd_lseek_is_verbatim_passthrough`.
- `lane_gate_tests`: `gate_routes_strictly_larger_ops_kernel_and_zero_disables`;
  `resolve_env_wins_verbatim_absent_derives_malformed_keeps_derived`;
  `mem_bw_probe_measures_something_real`; `derived_threshold_composes_probe_and_geometry_rails`;
  `kernel_lane_latch_is_off_at_bind_and_sticky_once_set`;
  `dup_siblings_share_the_lane_latch_like_they_share_f_pos`; `a_fresh_bind_clears_the_latch_with_the_cell`.
- `aio_core_tests`: `mixed_batch_full_acceptance_dispatches_in_order`;
  `kernel_partial_acceptance_truncates_the_prefix_before_later_ring_ops`;
  `ring_refusal_reroutes_to_the_kernel_lane_never_eagain`;
  `kernel_first_op_errno_propagates_when_batch_starts_kernel`;
  `getevents_merges_ring_first_then_kernel_with_min_across_lanes`;
  `ring_error_completion_surfaces_negative_errno_in_res`;
  `min_zero_never_blocks_and_returns_whatever_is_ready`;
  `ring_only_pending_waits_by_poll_slices_not_kernel_blocking`;
  `zero_timeout_pass_never_blocks_the_kernel_lane_with_ring_pending`;
  `pending_tokens_snapshot_reports_inflight_ring_ops_oldest_first`;
  `pending_tokens_into_matches_the_allocating_form`; `destroy_abandons_ring_pendings_without_delivering`;
  `only_pread_pwrite_on_bound_fds_classify_ring`.
- `aio_glue_tests`: `raw_iocb_matches_libaio_userspace_layout`; `raw_io_event_matches_…`;
  `eligible_pread_and_pwrite_on_bound_fds_classify_ring`; `unbound_or_wrong_rights_classify_kernel`;
  `non_data_opcodes_classify_kernel_even_on_bound_fds`; `resfd_eventfd_completion_classifies_kernel`;
  `per_op_rwf_flags_follow_the_sync_allow_list_law`; `slab_fit_cap_and_zero_length_classify_kernel`;
  `libaio_symbols_carry_explicit_default_versions`; `register_lookup_remove_roundtrip`;
  `duplicate_register_retro_neutralizes_never_stacks`; `removed_ctx_value_is_re_registrable_with_fresh_state`;
  `recycled_ctx_value_with_missed_destroy_retro_neutralizes`; `collision_reregistration_consumes_one_slot`;
  `destroyed_state_is_observable_for_the_passthrough_fast_reject`;
  `full_registry_refuses_and_degrades_to_passthrough`; `lookups_race_free_across_threads`.
- `refusal_reason_tests`: `version_skew_names_both_build_commits`; `…_abi_mismatch_names_both_abis`;
  `…_degenerate_identity_names_the_dev_override`; `disabled_refusal_names_the_interception_remedy`;
  `socket_reasons_distinguish_eof_from_errno`;
  `every_daemon_refusal_class_has_distinct_text_and_unknown_carries_its_number`;
  `reason_codes_are_distinct_per_cause`; `refusal_once_dedups_per_mount_and_reason`;
  `refusal_once_is_exactly_once_under_racing_threads`; `refusal_once_full_set_never_silently_loses_a_new_reason`.
- `linked_mode_tests`: `absent_env_is_linked`; `empty_env_is_linked`; `bare_soname_is_preload`;
  `absolute_path_is_preload`; `colon_and_space_separated_lists_match`; `foreign_preloads_only_is_linked`;
  `near_miss_names_are_linked`; `versioned_soname_is_preload`.
- In-module: `sessions_default_ties_to_shared_derivation`; `sessions_env_override_is_a_lever`;
  `registry_ties_to_shared_structural_constant`; `sparse_park_k_derives_and_gates`;
  `reap_park_max_default_and_clamp`; `reap_quantum_default_and_clamp` (`R:interpose.rs:2960-3086`);
  the `fd_table_core` unit tests (`R:fd_table_core.rs:315-409`); `kernel_lane_min_derivation_values`,
  `reap_batch_wake_threshold_values` (`R:sizing.rs`).
- Loom (`R:loom-models/src/lib.rs:4981-5250`): `fd_table_refs_report_unbind_exactly_once`;
  `fd_table_zero_count_is_never_resurrected`; `fd_table_mirror_dekker_never_loses_the_final_offset`
  (either fence removed fails); `fd_table_disarm_is_once_per_cell`;
  `fd_table_fork_bump_stales_a_concurrent_bind`; `fd_table_fork_walk_demote_never_loses_the_final_offset`.

Root crate against an in-process daemon (sandbox fs, no root; `R:tests/`):
- `preload_authn_tests` (a `RogueDaemon` answering the raw protocol):
  `shim_refuses_a_daemon_whose_uid_is_neither_root_nor_the_mount_owner`;
  `shim_accepts_the_real_daemon_when_the_mount_owner_matches`; `shim_refuses_an_unsealed_session_memfd`;
  `shim_accepts_a_fully_sealed_session_memfd`; `shim_refuses_a_session_reply_carrying_extra_descriptors`;
  `shim_refuses_a_path_rendezvous_in_a_world_writable_directory`;
  `shim_accepts_a_path_rendezvous_under_an_owner_private_directory`;
  `host_refuses_to_bind_a_path_socket_in_a_group_or_world_writable_dir`;
  `host_binds_a_path_socket_in_an_owner_private_dir_and_serves_it`; `refused_rendezvous_never_leaks_descriptors`.
- `preload_session_tests`: `session_establish_bind_and_rw_parity`;
  `bind_refuses_directory_and_wrong_direction_is_ebadf`; `establish_refuses_on_commit_skew`;
  `stalled_serve_times_out_poisons_and_falls_through`; `ticket_park_wait_is_event_driven_not_quantum_polled`;
  `slot_completion_wakes_every_parked_waiter`; `service_thread_stays_hot_between_back_to_back_ops`;
  `new_session_on_a_busy_thread_is_served_promptly`; `shutdown_is_prompt_with_the_idle_reaper_armed`;
  `explicit_poison_falls_through_and_stops_ctl_traffic`;
  `establish_falls_back_to_the_path_socket_when_abstract_is_unreachable`;
  `large_{write,read}_is_one_ring_op_per_max_op_window`; `large_{write,read}_pipelines_all_chunks_before_waiting`;
  `large_{write,read}_short_first_completion_reports_posix_prefix`; `large_{write,read}_error_semantics_prefix_or_errno`;
  `large_{write,read}_releases_every_slot_after_serve`; `fragmented_slots_still_serve_large_{write,read}_via_smaller_runs`;
  `multi_slab_ring_write_full_parity_through_real_sink`.
- `preload_lifecycle_tests`: `fork_child_poison_never_severs_the_parent_session`;
  `idle_sessions_reap_and_active_sessions_survive`; `invalidation_fires_on_bind_and_rate_limited_writes_never_reads`;
  `teardown_with_parked_handoff_completes_without_poison`; `lifecycle_stats_fields_export`;
  `size_changing_ring_writes_fire_attrs_only_invalidations_inside_the_window`;
  `the_last_unbind_of_an_inode_fires_a_whole_inode_invalidation`.
- `preload_parity_tests` (daemon-side mostly; shim-relevant): `ring_read_matches_fuse_read_byte_parity`;
  `ryw_across_transports_both_directions`; `fsync_through_fuse_after_ring_writes`;
  `arena_severance_client_scribble_never_reaches_acked_bytes`; `interleaved_transports_match_oracle`;
  `adversarial_mid_serve_mutation_serves_the_snapshot`; `lane_gate_counters_ride_the_stats_page_to_the_stats_inode`;
  `mixed_lane_writes_on_one_file_stay_coherent_and_both_lanes_account`.

★ The preload gate (`R:tests/run_preload_gate.sh`): 1a sanctioned build + clippy/fmt/tests;
1b wrong-profile refusal; 1c plain-fs passthrough battery (cp/dd/cat/tar parity + exit codes);
1d libaio lifecycle passthrough (all orderings, multi-phase) + the self-interposition tripwire;
1e direct-link battery (SONAME/NODELETE, ctor-context I/O, `dladdr` occupancy, parity). Root:
2a mount `--interception`; 2b cp/dd parity + `ipc_ops_*` ENGAGEMENT proof (legacy rows pin
`SQUEEZEFS_IL_KERNEL_LANE_MIN=0`); 2b-linked engagement + the line exactly once; 2c dup
transparency; 2d `close_range`-then-`socket()` reuse; 2e lseek-SEEK_CUR-is-FUSE-free; 2f
offsetful `cat` parity; 2g fio/elbencho; 2g-aio libaio mixed batches with ring engagement;
2g-lane the derived gate (bs=1M routes kernel and is accounted, threshold gauge live, 4k stays
ring, cross-lane parity); 2g-netns `unshare -n` path-socket rendezvous; 2h kill-9 soak ×5
(sessions + shm bytes back to baseline; odd cycles bs=1M multi-slab flights); 2i
fork-then-kill-parent EOF; 2j/2k libaio on the armed / establish-refused mount; 2l direct-drive
kill-9 soak; 2m two concurrent interception mounts in one process (never-cross engagement)
(`R:tests/run_preload_gate.sh:4-28, 220-854`).

## 11. Quirks per decision 0002 — NOT carried, and the better design

| # | Reference quirk | Why it is poor | What the port does |
|---|---|---|---|
| S1 | A refused establish (`Disabled`, `Version`, `Budget`, socket error) is retried on EVERY eligible open — the mount is never negative-cached (`R:interpose.rs:550-563, 326-335`), so every `open` on a plain (non-`-o interception`) SqueezeFS mount pays `fstatfs` + `fgetxattr` + `connect` + HELLO + `recvmsg` + a daemon ctl thread | A per-open control-plane tax on every ordinary mount, by design; the daemon spawns a ctl thread per attempt | A per-`st_dev` refusal cache keyed by class: `Disabled`/`Version`/`PeerUntrusted`/`Unsealed`/`UntrustedRendezvous` are permanent for the mount's life (re-mounting changes `st_dev`); `Budget`/`Socket`/`Protocol` back off exponentially from the nonce TTL (60 s) to 15 min. The one line still prints once |
| S2 | `header.heartbeat` declared "client-side death detection" and never written or read (`R:layout.rs:316-317`); daemon death is noticed only by the 30 s op deadline (the design said 1 s, `:400`) | A dead field in a wire struct; a wedged daemon stalls every bound op 30 s before passthrough | The daemon stamps `heartbeat` (coarse monotonic seconds) at every drain pass and park wake; the client's 50 ms park slices compare it — stale > 2 s with an op outstanding ⇒ poison (fallthrough) long before the deadline; the deadline stays the backstop. The field earns its line |
| S3 | `open`/`fcntl` declare the C variadic as a fixed third argument and read a register that may not have been set (`R:interpose.rs:1424-1428, 1657-1659`) | Reads garbage when `O_CREAT`/`O_TMPFILE` is absent; forwards it | Zig defines true C-variadic exports (`@cVaStart`/`@cVaArg`): `mode` is read only when `O_CREAT|O_TMPFILE` is set, exactly as libc does |
| S4 | Three panic-recovery arms (`catch_unwind` → real call / `-EIO` / `-EINTR`) as the error boundary (`R:interpose.rs:965-976, 2343-2359, 2854-2865`); a whole build profile exists to keep unwinding alive (`R:Cargo.toml:9-19`) | Hides programming errors behind a counter (decision 0002 row 9); the async arms still hand the app a fabricated errno | No unwinding in Zig. Interposer bodies are TOTAL by construction (every bound a checked branch yielding `.real`; no `unreachable`/`.?`/`catch unreachable`/`@panic` in `src/il/` — a static rail); untrusted decoders fuzzed "never panic on arbitrary bytes"; the root `panic` handler poisons every session, prints one line, aborts (a true bug is loud, never UB in the host) |
| S5 | Client-side attribution counters the design promised (`preload_passthrough_ops{reason}`, `preload_bailouts_{mmap,locks,append,fork}`, `preload_bind_refused{reason}`; `R:docs/design-preload-interception.md:503`) never landed; only stderr lines exist; `il_submit_harvested` is a permanently-0 field | The engagement law needs the shim's side of the ledger; a 0-forever field is dead | The client stats page carries `passthrough_ops[reason]` (one `AtomicU64` per bail-out row of §4), `bind_refused[class]`, and the lane-gate words; `il_submit_harvested` is deleted. Still display-only on the daemon |
| S6 | `SQUEEZEFS_IL_PARANOID_FSTAT=1` documented as a triage knob (`:319, 470`) and never implemented | A documented knob nothing reads | Not registered (the rails would go red). The `close_range` + creator sweep posture stands |
| S7 | Sessions, binding cells, fd-table segments and aio ctx states are leaked by design (`R:fd_table.rs:57-64`; `R:interpose.rs:189-200`; `R:aio_glue.rs:152-157`) — "bounded by open churn" | Bounded in theory; a long-lived process re-opening files leaks ~32 B per bound open forever | Cells recycle through a per-process free list under an epoch (a cell is reusable once no in-flight lookup can hold it: the data-path lookup is a bounded read-copy under the TLS guard, so a per-thread "in lookup" epoch word suffices — the same reclamation the port's `arc_swap.zig` uses); sessions and segments stay leaked (a handful per process) |
| S8 | `claim_slot`/`claim_run` scan up to `slots` (1024) CAS attempts under contention; the rotating `slot_hint` is the only spread (`R:session.rs:1384-1393, 1417-1461`) | O(slots) per claim at saturation on the hot path | Per-thread claim cursors striped over the slot array (`cpus` stripes of `slots/cpus`), a stripe-local scan first; a free-slot bitmap (one `u64` per 64 slots) consulted before any CAS |
| S9 | The libaio reap allocates `Vec`s on the host app's malloc (`parks`, `tok_buf`, `entries`, `parked_tokens`, `ids`, `classes`; `R:interpose.rs:2412-2415, 2277-2326`) and `RealKernel::getevents` allocates a `Vec<RawIoEvent>` of `max` per pass (`:2130-2138`) | Allocation on an interposed hot path inside a foreign process | Fixed-capacity per-ctx arrays sized by the ctx's `maxevents` at `io_setup` (one allocation per ctx life), and the kernel-lane harvest writes straight into the caller's `events + got` window (no temporary) |
| S10 | `sendfile`/`copy_file_range`/`splice` are interposed solely to demote; `signalfd4` and `openat2` are in the design table but not the code (`R:docs/design-preload-interception.md:150-151`) | A table that does not match the binary | The port's symbol table is ONE comptime list that generates the exports, the rails census, and the docs table; `openat2` and `signalfd4` are in it |
| S11 | `SQUEEZEFS_IL_SESSIONS` clamps `0 → 1`, `99 → 16` silently (`R:interpose.rs:127-131`); `REAP_PARK_MAX`/`REAP_QUANTUM_US` clamp too | Clamping is banned (charter §1.6); the shim's asymmetry is announce-and-default, not clamp | Out of range ⇒ announce once, keep the DERIVED default (the registry row's range), never clamp |
| S12 | Linked-mode detection reads `LD_PRELOAD` and matches the basename prefix (`R:session.rs:150-162`) | A heuristic that misreports under AT_SECURE | `dl_iterate_phdr` finds our own `dlpi_name` and `getauxval(AT_SECURE)`; the line names the actual loader path. Still diagnostic |
| S13 | `wait_any`'s pre-5.16 fallback waits on the FIRST entry only, at a 200 µs quantum (`R:session.rs:1922-1927, 1956-1962`) | The 7.2.6-sqz kernel always has `futex_waitv`; the fallback is dead on the product venue | `futex_waitv` is required (probed once; `ENOSYS` ⇒ the session refuses to establish with its own reason line — passthrough) |
| S14 | Two spellings of the skew gate's degenerate predicate (`unknown` / `-dirty` / empty) and a 48-byte fixed commit field sized for `-dirty` (`R:wire.rs:28-32, 484-486`) | The identity is a string compared for equality | The blob carries the commit as 20 raw bytes + a `dirty` flag bit in `flags`; equality is `memcmp`; the stderr line formats hex on demand |

Carried as-is (charter mechanisms): the ladder order of §2, the VAL-4 three rungs, the
Dekker-paired mirror and its flush table, refcounted bindings, the per-inode mmap unbind + the
never-forgetting set, `close(2)`-never-`shutdown` in the child, the prefix laws, the libaio
reroute-never-EAGAIN, the hybrid gate's derivation and sticky latch, `k = clamp(pending/4, 2,
pending)`, `dlvsym` for libaio, SONAME + `-z nodelete`.

## 12. Zig notes — building an interposing C-ABI `.so` with Zig 0.17

Verified on this box (`zig 0.17.0`, cross `-target x86_64-linux-gnu`):

- **Artifact**: `b.addLibrary(.{ .linkage = .dynamic, .name = "squeezefs_il", .root_module =
  il_mod })` (`std/Build.zig:615-650`); the root module imports `squeezefs` for the shared
  codecs (blob, ctl, header, sizing) and `link_libc = true` (the interposer needs `dlsym`,
  `dlvsym`, `pthread_atfork`, `__errno_location`). Default SONAME is `lib<name>.so` — a plain
  `zig build-lib -dynamic --name squeezefs_il` emits `SONAME libsqueezefs_il.so` (verified by
  parsing the dynamic section); `--version` would make it `.so.<major>` — do not set a version
  (the SDK's `libsqueezefs.so` is the versioned one).
- **`-z nodelete`**: the CLI accepts `-z nodelete` and sets `DT_FLAGS_1 = NODELETE|NOW`
  (verified), but `std.Build.Step.Compile` exposes only `link_z_{notext,relro,lazy,defs,
  common_page_size,max_page_size}` (`std/Build/Step/Compile.zig:134-149`) — no `nodelete`, no
  `-fsoname`. Two options: (a) build the shim with an explicit `zig build-lib` `Run` step
  (`b.addSystemCommand(&.{ b.graph.zig_exe, "build-lib", "-dynamic", "-lc", "-z", "nodelete",
  "-fsoname=libsqueezefs_il.so", "-O", "ReleaseSafe", "-target", …, "--dep", "squeezefs",
  "-Mroot=…", "-Msqueezefs=…" })` with `addOutputFileArg`/`addInstallBinFile` (the port's
  split-debug step already drives external commands this way); (b) keep `addLibrary` and
  promote at runtime: the lazy init calls `dlopen("libsqueezefs_il.so", .{ .LAZY = true,
  .NOLOAD = true, .NODELETE = true })` — glibc applies `NODELETE` to the already-loaded link map
  (`std.c.RTLD` has the `NODELETE`/`NOLOAD` bits, `std/c.zig:7352-7362`). Do (a) and assert
  `readelf -d` in the gate as the reference does; (b) is the belt for a `dlopen`ed shim built
  some other way.
- **Exports**: `export fn pread64(fd: c_int, buf: ?[*]u8, n: usize, off: i64) callconv(.c)
  isize` — `export` gives a strong default-visibility symbol with the C ABI. Variadic exports
  work: `export fn open(path: [*:0]const u8, flags: c_int, ...) callconv(.c) c_int { var ap =
  @cVaStart(); defer @cVaEnd(&ap); const mode = if (flags & (O_CREAT|O_TMPFILE) != 0)
  @cVaArg(&ap, c_uint) else 0; … }` (verified). Generate the 59 symbols (61 with `openat2`
  and `signalfd4`) from ONE comptime
  table (`name, signature, shape`) so the rails census and the docs table come from the same
  list (§11 S10). The `.so` must export ONLY the interposed names and nothing of Zig's (no
  `-rdynamic`); check with `nm -D --defined-only`.
- **Real functions**: `std.c.dlsym(handle, name)` with `RTLD_NEXT = @as(*anyopaque,
  @ptrFromInt(std.math.maxInt(usize)))` (glibc `(void*)-1`; `std.c` has no `RTLD_NEXT`
  constant, `std/c.zig:11422-11424`). `dlvsym` is a GNU extension absent from `std.c`: declare
  `extern "c" fn dlvsym(?*anyopaque, [*:0]const u8, [*:0]const u8) ?*anyopaque;` yourself.
  Cache each pointer in a `std.atomic.Value(?*const fn…)` (`.monotonic`); `null` ⇒ the
  synthesize-the-kernel-answer arm. Never `std.DynLib`/`ElfDynLib` here (they open files and
  allocate; `RTLD_NEXT` is the contract).
- **Self-calls**: the shim's own libc use (`socket`, `connect`, `mmap`, `fcntl`, `fstat`,
  `fgetxattr`, `lseek`) must NOT go through `std.posix`/`std.c` — with libc linked, `std.posix.*`
  dispatches to `std.c.*` whose PLT entries resolve to OUR exports (self-interposition; the
  reference leans on its guard for this). Use `std.os.linux.*` raw syscall wrappers directly:
  `lseek`, `pread`, `mmap`, `munmap`, `fgetxattr`, `fcntl`, `getsockopt`, `sendmsg`, `recvmsg`,
  `memfd_create`, `close_range`, `futex_4arg`, `futex2_waitv`, `getpid` (`std/os/linux.zig:
  1167-1294, 1577, 2043, 2114, 2813-2840, 2964, 3205`) + `linux.errno(ret)`. There is NO
  `fstatfs` wrapper: `linux.syscall2(.fstatfs, fd, @intFromPtr(&st))` with your own `extern
  struct Statfs` (`f_type: i64` first on x86_64/aarch64) and `FUSE_SUPER_MAGIC = 0x65735546`
  (not in std). `SO.PEERCRED` is per-arch in `std.os.linux.SO` (`:5246`); `ucred = extern struct
  { pid: i32, uid: u32, gid: u32 }`; `F.GET_SEALS`/`SEAL_*` exist (`:2376-2378`); `MSG.CMSG_CLOEXEC`
  (`:6064`); `cmsghdr{len: usize, level: i32, type: i32}` + `cmsg_align` (`:11240-11248`) — write
  `CMSG_LEN/SPACE/FIRSTHDR/NXTHDR` as comptime helpers (std has none). The one libc call that
  stays libc is `dlsym`/`dlvsym` (and `pthread_atfork`), made only under the guard.
- **Reentrancy guard**: `threadlocal var reentry: bool = false;` — general-dynamic TLS in a
  shared object goes through `__tls_get_addr`; a plain `bool` has no destructor, so there is no
  "TLS destroyed" state to `try_with` around. Pattern: `if (reentry) return real(...); reentry =
  true; defer reentry = false;`.
- **Lazy init, no constructors**: no `linksection(".init_array")` (possible in Zig — `export
  const init linksection(".init_array") = &ctor;` — but the reference is ctor-free for a
  reason: other objects' ctors call interposed symbols before `main`). Globals are
  `std.atomic.Value` state machines (uninit → initializing → ready) with a bounded spin; the
  port's `sync/once.zig` takes an `Io` and is not usable here (hot paths never take an `Io`,
  decision 0001).
- **No panics escape — the Zig reading**: Zig has no unwinding; the root `pub const panic =
  std.debug.FullPanic(shimPanic)` (`std/debug.zig:103`) is `noreturn` — `shimPanic` poisons
  every session (flag stores + `shutdown`), writes one line with `linux.write(2, …)`, then
  `std.process.abort()`. Verified to compile in a lib root. Therefore the interposer bodies
  must be total (§11 S4): build ReleaseSafe (a bug trips the handler in the gate instead of
  becoming UB in a host), ban `unreachable`/`@panic`/`.?`/`catch unreachable`/`orelse
  unreachable` in `src/il/` with a `tests/rails.zig` scan, fuzz `BootstrapBlob`/`CtlMsg`/
  `SessionHeader` decoders with `std.testing.fuzz`, and never index shared memory without a
  geometry-derived bound check. `std.debug.no_panic` (`@trap`) is NOT acceptable (silent abort).
- **Allocation**: the data path allocates nothing (fixed arrays; the fd table's segments are
  `linux.mmap`'d once each; the registry is static). Control-plane allocation (the 64 KiB ctl
  receive buffer, the refusal set) uses `std.heap.c_allocator` under the guard —
  `page_allocator` would call libc `mmap` → our own export. `std.BoundedArray` does not exist
  in 0.17; use `[N]T` + a length.
- **Atomics**: `std.atomic.Value(u32)` with `.seq_cst`/`.acq_rel`/`.acquire`/`.release`/
  `.monotonic`; no `@fence` — the Dekker fences become `seq_cst` RMWs/loads on the same words
  (the port's `fd_table_core` model restates them; `P:docs/decisions/0002…` row 12's
  `sync/futex.zig` is the daemon's; the shim uses `linux.futex_4arg(ptr, .{ .cmd = .WAIT },
  expected, &ts)` SHARED — never `PRIVATE_FLAG` — and `linux.futex2_waitv` with `FUTEX2_SIZE_U32`).
  FUTEX_WAKE count is a C int (`maxInt(i32)`).
- **Shared-memory structs**: `extern struct` mirrors of the header/slot/ring cell/stats page
  with `comptime { std.debug.assert(@sizeOf(IpcSlot) == 128); }` and `align(128)`/`align(4096)`;
  descriptor fields as `std.atomic.Value` accessed `.monotonic`, ordered by the state word.
- **Build identity**: the shim imports the same `build_options.git_commit` the daemon prints
  (`P:build.zig:43-48`; `b.graph.poisonCache()` keeps it fresh); the port's `-dirty` posture
  follows the existing `gitCommit(b)`.
- **Tests**: the pure cores (fd table, mirror, aio core, blob/ctl codecs, lane gate maths,
  refusal text) are portable and run on macOS; the socket/futex/mmap client and every
  interposer test are Linux-only (`builtin.os.tag == .linux`, never stubbed); the gate's
  `LD_PRELOAD` battery runs in `scripts/venue` against the smoke mount. Model-check the three
  `fd_table_core` protocols and the client side of the CQE park with `src/model/explorer.zig`
  (`expectHolds` + the fence-removed `expectFails`).
- **Pitfalls met**: `std.Thread.setName` needs an `Io` (irrelevant here — the shim spawns no
  threads); `std.posix.iovec.base` cannot hold address 0 (a zero-length iovec with a NULL base
  from the app must be skipped before building any std iovec); `linux.lseek` returns `u64` —
  check with `linux.errno`; `std.c.RTLD` is a packed struct, so `dlopen` flags are
  `.{ .NOLOAD = true, .NODELETE = true }`; `callconv(.c)` not `.C` in 0.17.
