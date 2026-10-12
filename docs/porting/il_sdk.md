# PORTING SPEC — the native SDK (`libsqueezefs`, the C ABI): a second client kind on the il wire, the buffer-lease law, the one-release version window, strengthen-only per-open durability

> **Checksum amendment (2026-10-11, decision 0003 §M; 0002 row 480).** Every `xxh3` / `xxh3-64` /
> `xxh3_64` below names the hash the reference used and this port used until format v5. The port's
> ONE hash is now **sqzhash128** (`src/core/sqzhash.zig`, the pinned definition): every 8-byte checksum
> and identity field is its LOW 64 bits under the fixed public format key (`checksum.hash64`,
> `hash64FieldZeroed`, `stamp`/`verify`, the streaming `checksum.Hasher`); the seeded name hashes
> (`hash54`/`hash56`) are `hash64Keyed` under the per-mount name key (`key.NameKey`: the volume's
> `hash_seed` zero-extended, expanded once at mount). Field names on the wire read `csum`. Byte
> layouts, offsets and widths below are unchanged; only the function behind the 8 bytes is.

Charter authority: §7 (`docs/charter.md:244` — "**SDK**: the same wire as a second client kind; positional-only ops on arena-backed 4 KiB-aligned buffers under a buffer-lease law (`free` while SUBMITTED = EBUSY, never deferred; poison ⇒ never recycled); versioned over a one-release window; strengthen-only per-open durability"), §12 (`charter.md:298` — "the SDK exports a C ABI"), §11 (`charter.md:287` — "the daemon and shim refuse to pair across builds"), §13 rung 7 (`charter.md:312` — "The interception shim + direct-drive lanes + doorbell; the SDK").

## 0. Status of the reference: the SDK is DESIGNED, NOT BUILT — what this spec is derived from

**There is no SDK in the reference tree.** Verified: `ls crates/` shows only `fuse3`, `squeezefs-ipc`, `squeezefs-preload`, `squeezefs-testkit` (no `squeezefs-sdk`); a tree-wide grep for `sqz_sdk|squeezefs_sdk|sdk_buf|client_kind|ClientKind|buffer_lease|BufferLease|sqz_buf_alloc|sqz_pread|sqz_pwrite|sdk_abi|SDK_ABI|SQZ_D_|durability_class` outside the design doc hits exactly ONE line, `AGENTS.md:953`, which itself says "Design phase 2026-08-04; implementation is the follow-on `feat/sdk-…` ladder". The design doc's own status line is "**Draft — rev 1** (design phase; implementation is a follow-on campaign — the PR ladder in §13 is the plan of record, not concurrent work)" (`docs/design-sdk.md:8`), and the evidence note repeats it: "implementation is the follow-on `feat/sdk-…` ladder, NOT this campaign" (`.benchmarks/2026-08-04-sdk-design.md:9-10`). The daemon-side gauges the design names (`ipc_arena_dest_writes`, `ipc_sessions_by_kind_*`, `ipc_sdk_abi_refusals`, `sdk_buf_*`) exist nowhere in code (grep, 0 hits). What DID ship with the design is **Tier 1** — direct-link (`-lsqueezefs_il`) support for the existing shim: SONAME + `-z nodelete` in `crates/squeezefs-preload/build.rs`, `session::load_mode_from` (`crates/squeezefs-preload/src/session.rs:129-168`), and gate legs 1e / 2b-linked (`.benchmarks/2026-08-04-sdk-design.md:19-86`). Tier 1 is a shim concern (the il shim spec), not this spec's.

Therefore this spec is derived from (abbreviations used in citations below):

| Tag | File | What it contributes |
|---|---|---|
| **D** | `~/Source/squeezefs/docs/design-sdk.md` (362 lines) | the API, the lease law, the client kind, the window, durability — the design of record |
| **E** | `~/Source/squeezefs/.benchmarks/2026-08-04-sdk-design.md` | keyed-decision summary, the priced table |
| **W** | `crates/squeezefs-ipc/src/wire.rs` | HELLO / SESSION_OK / BIND / BIND_OK / REFUSE / UNBIND, the bootstrap blob, refusal classes |
| **L** | `crates/squeezefs-ipc/src/layout.rs` | the session header, geometry, slot descriptor, arena arithmetic, `IPC_ABI` history |
| **S** | `crates/squeezefs-ipc/src/slot_core.rs` | slot states, generation guard, park/complete edges |
| **C** | `crates/squeezefs-ipc/src/cqe_core.rs` | the completion doorbell `sqz_reap` parks on |
| **P** | `crates/squeezefs-preload/src/session.rs` | the shim's session client the SDK shares (establish, bind, submit, wait, poison, never-reuse) |
| **H** | `src/ipc_host.rs` | the daemon's HELLO ladder + fd screen + descriptor validation |
| **V** | `src/ipc_service.rs` | the arena-dest read serve the SDK's in-place reads ride |
| **Z** | `crates/squeezefs-ipc/src/sizing.rs`, **M** `src/mem_budget.rs` | sessions-per-mount, drain lanes, the arena admission cap |
| **PD** | `docs/design-preload-interception.md`, **R** `docs/design-rewrite-program.md` | bail-out/never-reuse law, fsync soundness, KD-6/KD-7; Idea 8 durability classes |

Where the design leaves a contract open, this spec RESOLVES it (marked **[resolution]**) and, where the design embodies a quirk, departs per decision 0002 (§9 below). Nothing here owes compatibility with the reference wire (`docs/decisions/0002-no-compatibility-obligation.md:13-17`).

**Where the port stands.** Nothing of the il plane exists in `src/` yet: `src/wire/` is the CLUSTER wire (TCP frames, `src/wire/{frame,verbs,session}.zig`), not the same-host IPC — do not conflate them. The il shim spec (rung 7's other half) owns the session layout, the slot/ring/doorbell cores and the shim; this spec names the pieces it depends on in §3.2 so the two specs agree on the module split (§10.1). Rung order (`charter.md:312`): shim + direct-drive lanes + doorbell first, the SDK on top of the shared client core.

## 1. The C ABI surface

### 1.1 Shape and discipline

* One library, `libsqueezefs` (`.so` with SONAME + `.a`), exporting **only `sqz_*` symbols** — "No libc interposition, no `#[no_mangle]` libc names — this library exports only `sqz_*` symbols and is safe to link into anything" (D:115). Dependencies: libc + the shared session client only (D:116 — "no tokio, no allocator games in host processes").
* **Explicit, never silent**: "What BIND would silently passthrough in the shim …, `sqz_open` either serves in POSIX mode or … refuses with errno. Silent is for interposition; the SDK is explicit" (D:154). Hence `sqz_fs_mode` / `sqz_fs_mode_reason` (D:123-124).
* **Positional-only**: "Offsetful ops do not exist. No `sqz_read`/`sqz_write` without offsets — the lseek discipline (§5.4.3 of L4) was interposition-tax; a first-party API simply requires offsets (io_uring made the same call)" (D:155).
* **Control plane rides kernel FUSE** (the scope fence): "`sqz_open` performs a real `open(2)` on the real mount … and then BINDs that fd — exactly the shim's handshake. `stat`/readdir/rename/xattrs/locks/`fsync` ride kernel FUSE" (D:84). No mmap, no `FILE*`, no `O_APPEND/O_PATH/O_TMPFILE` handles (D:88), no metadata bypass, no client-side device access or keys (D:85), one `sqz_fs_t` per mount (D:86), same-host only (D:87).
* **Error convention [resolution; 0002 row A]**: EVERY `sqz_*` call that can fail returns a **negative errno** (`-EBUSY`, `-EINVAL`, …) and never writes `errno(3)`; success is `≥ 0` (byte counts for data ops, 0 for void-ish ops, a non-null pointer for constructors with the errno available through `sqz_fs_mode_reason`/`sqz_open_errno` — see 1.2). The design sketched a MIX (`ssize_t sqz_pread` in POSIX style next to `sqz_buf_free` "-EBUSY" in io_uring style, D:138, D:141); one convention, the io_uring one, because "the API's shape is the transport's shape (slots/CQEs)" (D:345) and a thread-local errno is a libc artefact that bindings in other languages pay for. The errno VALUES are POSIX-identical (G-S1: "byte- and errno-identically to plain POSIX on the mount", D:76).
* **Thread-safety**: "handles are `Send`, ops on one handle are thread-safe (slot claiming is lock-free), `sqz_reap` is multi-reaper-safe via the doorbell's register→snapshot→re-scan law. No SDK-owned threads in v1 (no completion callbacks — reap is pull …)" (D:162). Port: every data-path call is lock-free (slot CAS `S:123-141`, ring push, buffer-state CAS §2.3); the ctl path (`sqz_open`'s BIND, `sqz_close`'s UNBIND, connect/disconnect) serializes on one mutex per session exactly like the shim's (`P:5-14`).

### 1.2 The surface (normative signatures; `include/squeezefs.h` is written by hand — §10.2)

```c
/* ---- opaque handles (generation-stamped SDK-owned objects, D:302) ---- */
typedef struct sqz_fs   sqz_fs_t;     /* one per mount: K fd-sharded sessions + their arenas */
typedef struct sqz_file sqz_file_t;   /* {fd, per-session bindings, durability class} */
typedef struct sqz_buf  sqz_buf_t;    /* one arena range + its lease state (§2) */

/* ---- session ---- */
typedef struct sqz_fs_opts {
    uint32_t struct_size;   /* = sizeof(sqz_fs_opts_t): additive evolution of the opts struct */
    uint32_t sessions;      /* 0 = derived clamp(ceil(cpus/4), 2, 16) (Z:55-59, charter:244) */
    uint64_t arena_bytes;   /* per-session arena REQUEST, 0 = daemon default (D:224); clamped, never refused */
    uint32_t op_timeout_ms; /* 0 = 30 000 (P:42); per-op completion deadline — a miss POISONS the session */
    uint32_t _reserved;
} sqz_fs_opts_t;
sqz_fs_t   *sqz_fs_connect(const char *mount_path, const sqz_fs_opts_t *opts); /* NULL only on ENOMEM/EINVAL(opts) */
int         sqz_fs_mode(const sqz_fs_t *fs);          /* SQZ_MODE_RING = 1 | SQZ_MODE_POSIX = 2 */
const char *sqz_fs_mode_reason(const sqz_fs_t *fs);   /* static-lifetime text; "" in RING mode */
int         sqz_fs_disconnect(sqz_fs_t *fs);          /* -EBUSY while ANY buffer is SUBMITTED (never deferred, §2.4) */

/* ---- files (control plane on the kernel mount; D:84, D:154) ---- */
typedef struct sqz_open_opts {
    uint32_t struct_size;
    uint32_t durability;    /* SQZ_D_MOUNT = 0 | SQZ_D_SYNC = 1 (§5); strengthen-only */
} sqz_open_opts_t;
sqz_file_t *sqz_open(sqz_fs_t *fs, const char *path, int oflags, mode_t mode, const sqz_open_opts_t *opts);
int         sqz_open_errno(const sqz_fs_t *fs);       /* the thread-local last constructor errno (positive) */
int         sqz_close(sqz_file_t *f);                 /* UNBIND(s) then real close(2); kernel FLUSH/RELEASE unchanged (D:130) */
int         sqz_fsync(sqz_file_t *f);                 /* real fsync(2) — identical in every class (D:131, D:253) */
int         sqz_fd(const sqz_file_t *f);              /* the real fd for everything the SDK does not do (D:132) */

/* ---- arena-native buffers (the lease law, §2) ---- */
sqz_buf_t  *sqz_buf_alloc(sqz_fs_t *fs, size_t len); /* 4 KiB-aligned, arena-backed; aligned heap in POSIX mode (D:135); NULL = ENOMEM */
void       *sqz_buf_data(sqz_buf_t *b);
size_t      sqz_buf_len(const sqz_buf_t *b);
int         sqz_buf_free(sqz_buf_t *b);               /* -EBUSY while SUBMITTED; 0 for POISONED (range quarantined, never recycled) */

/* ---- sync positional ops = submit(1) + inline reap with the adaptive spin (D:157) ---- */
ssize_t     sqz_pread (sqz_file_t *f, sqz_buf_t *b, size_t len, off_t off);        /* in place: no consume copy (D:210) */
ssize_t     sqz_pwrite(sqz_file_t *f, const sqz_buf_t *b, size_t len, off_t off);  /* no S1 copy-in (D:209) */
ssize_t     sqz_pread_raw (sqz_file_t *f, void *buf, size_t len, off_t off);       /* pays the boundary copy (D:143) */
ssize_t     sqz_pwrite_raw(sqz_file_t *f, const void *buf, size_t len, off_t off);

/* ---- io_uring-shaped batch (D:147-149, D:156) ---- */
enum { SQZ_OP_PREAD = 1, SQZ_OP_PWRITE = 2 };          /* == OP_READ / OP_WRITE on the wire (L:106-107) */
typedef struct sqz_sqe {                               /* 48 bytes, pinned */
    uint8_t     opcode;  uint8_t flags;  uint16_t _pad; uint32_t len;
    uint64_t    offset;                                /* file offset (positional only) */
    sqz_file_t *file;
    sqz_buf_t  *buf;     uint64_t buf_off;             /* op window = [buf_off, buf_off+len) inside buf */
    uint64_t    user_data;                             /* client-private: NEVER crosses the wire (§3.2) */
} sqz_sqe_t;
typedef struct sqz_cqe { uint64_t user_data; int64_t res; } sqz_cqe_t;   /* 16 bytes; res = bytes or -errno */
int         sqz_submit(sqz_fs_t *fs, const sqz_sqe_t *sqes, unsigned n);  /* ≥1 submitted prefix, or -EAGAIN / -EINVAL */
int         sqz_reap  (sqz_fs_t *fs, sqz_cqe_t *cqes, unsigned max, const struct timespec *timeout); /* 0 = timeout */
```

Decisions inside the surface, each with its source:

* `sqz_open` = real `open(2)` + BIND; "the handle wraps `{fd, binding, session shard, durability class}`" (D:154). Port **[resolution]**: the handle wraps `{fd, durability, bindings[K]}` — one lazily-established binding per session (see §3.3 for why).
* `sqz_sqe_t` "maps 1:1 onto the wire `IpcSlot` descriptor"; `sqz_submit(n)` "claims n slots, publishes them, and rings the doorbell **once**"; ops larger than `max_op_bytes` "chunk into pipelined flights with POSIX short-prefix semantics (client-side, exactly `ring_pwrite`'s law)" (D:156; the law itself `P:784-794`).
* `sqz_submit` returns the submitted PREFIX when slots run out ("client-visible backpressure … the batch prefix ends there", `P:1177-1181`); `-EAGAIN` only when the first SQE could not be submitted AND could not be served inline (§3.4). An SQE that is ring-ineligible (buffer not arena-resident, op class unservable, slot exhaustion) **completes inline on the real fd** and its CQE is queued on a client-side ready list `sqz_reap` drains before parking — the design's per-op rung ("that op runs POSIX on the real fd; the handle stays ring-bound", D:239) applied to the batch surface so a batch caller always gets its CQE through `sqz_reap`.
* Sync wrappers "are submit(1)+reap-inline with the adaptive spin window (`WAIT_SPINS` lineage) — the warm-read RTT stays the §5.5.1 fast-path 2–3 µs" (D:157; the window `P:44-59`: 4096 spins, 256 after a park, the 64-thread device-true sweep 336 k → 605 k IOPS).
* `sqz_reap` parks on the completion doorbell "directly (no libaio emulation between)" (D:50) with `k = clamp(pending/4, 2, pending)` (`charter.md:244`; `C:43-70` the batch mark law and why an admitted sleeper is never stranded).
* Buffers larger than the granted arena, or any op whose buffer is not arena-resident, "fall back to plain-pointer ops (the boundary copy returns for that op — counted, `sdk_buf_fallback_ops`)" (D:224).

### 1.3 The fallback ladder — every rung lands on plain POSIX on the real fd (D:230-239)

| Rung | Detection | Behavior |
|---|---|---|
| Not a SqueezeFS mount | statfs magic / bootstrap xattr ENODATA (`W:21-24`: the xattr is synthesized "only on interception-enabled mounts") | connect succeeds in **POSIX mode** + reason (D:234) |
| Interception not armed | HELLO → `RefuseClass::Disabled` (`W:91-98`, `H:2214-2216`) | POSIX + reason naming `--interception` (D:235) |
| Version window exceeded | pre-check against the blob, then HELLO refuse (`Version`) | POSIX + reason naming both values (D:236, §4) |
| Budget / admission | HELLO refuse (`Budget`, `H:2266-2301`) | POSIX + reason (D:237) |
| Daemon dies mid-flight | op deadline → poison (`P:1343-1346`), generation bump (`P:762-772`) | in-flight ops complete `-EIO`; every later op POSIX on the real fd; SUBMITTED buffers → POISONED (D:238) |
| Per-op ineligibility | per op | that op POSIX; handle stays bound (D:239) |

"Engagement honesty: benchmark rows remain valid only via daemon-side `ipc_ops_*` deltas (charter rule 4 — `sqz_fs_mode` is client-truth, `ipc_*` is measurement-truth)" (D:241).

### 1.4 Per-call contract (RING mode / POSIX mode / errors) **[resolution — the parity suite's table]**

| Call | RING mode | POSIX mode (any rung of 1.3) | Errors (negative) |
|---|---|---|---|
| `sqz_fs_connect` | classify → establish K sessions (§3.3) | classify only; no socket, no arena | `NULL` + `ENOMEM`; `EINVAL` for a bad `opts.struct_size` |
| `sqz_fs_mode/_reason` | `RING`, `""` | `POSIX`, the rung's text (§4) | never fail |
| `sqz_fs_disconnect` | UNBIND nothing (files own their bindings), `munmap` every arena, `close` sockets | free heap buffers | `-EBUSY` while any buffer is SUBMITTED or any file open; `-EINVAL` stale handle |
| `sqz_open` | real `open(2)`; bindings lazy per session (§3.3); `O_SYNC/O_DSYNC` ⇒ `durability = SYNC` | real `open(2)` only | `NULL` + the kernel's errno; `EINVAL` for `O_APPEND`/`O_PATH`/`O_TMPFILE`/a bad durability value (D:88) |
| `sqz_close` | UNBIND every established binding (best-effort, silent on a poisoned session `P:721-731`), then `close(2)` | `close(2)` | `-EBUSY` while an op on this handle is SUBMITTED; else the kernel's |
| `sqz_fsync` | real `fsync(2)` (D:131) | same | the kernel's |
| `sqz_fd` | the fd | the fd | `-EINVAL` stale handle |
| `sqz_buf_alloc` | buddy range in the thread's affine arena, 4 KiB-aligned; heap fallback when the arena cannot serve (flagged non-resident, counted `sdk_buf_fallback_ops`) | `aligned_alloc(4096, len)` (D:135) | `NULL` + `ENOMEM`; `len == 0` ⇒ `EINVAL` |
| `sqz_buf_free` | §2.3 | free | `-EBUSY` SUBMITTED; `-EINVAL` double free (state FREED) |
| `sqz_pread/pwrite` | submit(1) on `buf.session` + inline reap; in place | `pread64/pwrite64` on the fd from `sqz_buf_data` | the CQE's `res`; `-EBADF` wrong direction (`H:1100-1117`); `-EBUSY` POISONED buffer; `-EINVAL` `buf_off+len > buf.len` |
| `sqz_pread_raw/pwrite_raw` | slot-slab bounce (the shim's `ring_pread/ring_pwrite` law, `P:774-794`), pipelined chunks, prefix semantics | `pread64/pwrite64` | as the kernel |
| `sqz_submit` | claim+publish each SQE on its buffer's session, ONE doorbell per session touched; ineligible SQEs complete inline (POSIX) onto the ready list | every SQE inline, CQEs on the ready list | count submitted (≥ 1); `-EINVAL` bad opcode/file/buf; `-EAGAIN` nothing could be submitted or served |
| `sqz_reap` | drain the ready list, scan tickets, park on the doorbell with `k` (§3.4) up to `timeout` | drain the ready list | count (0 on timeout); `-EINVAL` `max == 0` |

POSIX-mode byte parity is exact because POSIX mode IS the kernel path on the same fd (D:230: "semantics identical, because the control plane already rides kernel FUSE and `sqz_fd()` is a real fd").

### 1.5 Handle lifecycle: fork, exec, dup, close **[resolution — the shim's §5.4.1 rows without the fd table]**

| Event | Shim (`PD:305-318`) | SDK |
|---|---|---|
| `fork()` | atfork child flag-poisons + `close(2)`s the socket copy; bindings → passthrough (`PD:315`) | same atfork child handler on every session (`P:754-760`): the child's `sqz_fs_t` is POSIX mode (reason `forked child — sessions are per process`), its SUBMITTED buffers POISONED, its arena mapping left in place (`munmap` is not AS-safe) and released at the child's `sqz_fs_disconnect`; the parent is untouched (the child `close`s, never `shutdown`s — `P:733-739`) |
| `exec()` | fresh init; inherited fds unknown → passthrough | sockets and memfds are `O_CLOEXEC`; nothing survives — the new image connects afresh |
| `dup(sqz_fd())` | bindings refcounted across dup'd fds (`PD:317`) | a dup is a plain kernel fd; the binding belongs to the `sqz_file_t`, which owns ONE fd; the app closing that fd behind the SDK is the same bug as with any wrapper |
| `mmap` / `fcntl` locks on the fd | unbind walk (`PD:311-312`) | not interposed: mmap on `sqz_fd()` is a documented non-goal (D:88); the SDK does not detect it — the W3 page-cache hazard (`PD:311`) is the app's, like `write(2)` racing its own `mmap` |
| process exit | socket EOF → daemon tears down (`PD:398`) | identical: daemon-side reclamation needs nothing from the SDK |
| daemon idle reap (`SQUEEZEFS_IPC_IDLE_SECS`, `PD:399`) | generation bump → lazy re-bind | generation bump → POISON (SUBMITTED buffers POISONED); the SDK re-establishes on the next `sqz_open`, never silently mid-handle |

## 2. THE BUFFER-LEASE LAW (SKD-2, "the hard part")

### 2.1 Prior art and the mirrored roles

"The in-house precedent is the transport payload lease (`docs/design-zero-copy-write-path.md` §5.4): a kernel-owned buffer is lent to the handler … with **never-write-while-leased** enforced … The SDK's buffer-lease law is the same shape with the roles mirrored: … the *app* leases *its own arena buffer* to the daemon for the duration of one op" (D:170).

### 2.2 States (normative, D:174-189)

```
APP_OWNED ──sqz_submit (referenced by ≥1 SQE)──▶ SUBMITTED ──all referencing CQEs reaped──▶ APP_OWNED
    │                                                 │
    └─ sqz_buf_free ⇒ range returns to allocator      └─ sqz_buf_free ⇒ -EBUSY (never deferred-free:
                                                          a hidden deferral would recycle custody the
                                                          daemon may still be serving — the §5.4.1
                                                          timed-out-slot never-reuse law, verbatim)
    On session poison/timeout with the buffer SUBMITTED: the buffer enters POISONED — never recycled,
    never returned to the allocator (the daemon may complete into it arbitrarily late …).
    Priced: leaked-by-design at ≤ arena scale, exactly like timed-out slots today.
```

The never-reuse law it quotes is the shim's: "the timed-out slot is never reused (the daemon may still complete it arbitrarily late; recycling it would hand a stale completion to a new op)" (`P:17-21`; enforced at `P:1343-1346` and `P:1312-1321`; the abandoned-ticket form `P:107-111`; extended to multi-slab holds `PD:255`).

**What the app may touch, when** (D:191-198):

| State | App reads | App writes |
|---|---|---|
| APP_OWNED | yes | yes |
| SUBMITTED (write op) | yes (its own bytes) | **contract violation** — priced below |
| SUBMITTED (read op) | **torn** (serve in progress) — contract violation to *rely* on | contract violation |
| POISONED | yes (stale bytes) | contract violation (daemon may still DMA into it) |

### 2.3 Port representation **[resolution]**

* `sqz_buf_t` is an SDK-private record `{arena_base_off u64, len u64, session u8, inflight: atomic u32, state: atomic u8 {APP_OWNED, FREED, POISONED}, generation u32}`; the record lives in a pool OUTSIDE the arena (one record per 4 KiB granule worst case: 64 MiB arena → 16 384 × 32 B = 512 KiB; allocation-free per op). The design says "the inflight refcount lives in the buffer header" (D:202) — the port keeps that header out of client-writable-by-the-daemon memory so no arena write (DMA into a neighbouring window, a daemon bug) can corrupt the lease bookkeeping.
* **SUBMITTED ≡ `inflight > 0`** (a buffer may be referenced by several SQEs, D:177). `sqz_submit` increments `inflight` (acq_rel) BEFORE publishing the slot; the reap path decrements AFTER the result is consumed (the Acquire of `is_done_for`, `S:207-217`, orders the daemon's arena write before the decrement).
* `sqz_buf_free`: `cmpxchg(state, APP_OWNED → FREED)` guarded by `inflight == 0` in ONE CAS on a packed word `{state u8 | inflight u24}` → the free either observes zero inflight and takes the range, or fails `-EBUSY`. Never a deferred free: "This is the violation class that could hurt *someone else's* op (custody recycling), so it is the one made unrepresentable" (D:202).
* **Poison**: on session poison (op deadline `P:1343`, generation bump `P:766-770`, impossible ring push `P:1117-1121`, the atfork child `P:754-760`), every buffer with `inflight > 0` is marked POISONED; its granules are **never returned to the buddy allocator** (quarantined for the session's life; `sdk_buf_poisoned` counts them). `sqz_buf_free` on a POISONED buffer returns 0 (the app is done with it) and the record flips to FREED while the range stays quarantined.
* **Model**: `sdk_buf_core` (the design's loom model, D:80, D:174) → port `src/sdk/buf_core_model.zig` over `model/explorer.zig`: threads {submitter, reaper, freer, poisoner}; invariant "a FREED range never overlaps a range the daemon may still serve"; the WEAKENING = splitting the packed CAS into `load(inflight)==0; store(state)` (a submit between the two frees a leased range) must `expectFails`.
* Allocator: a **buddy allocator at 4 KiB granule** over the session arena (natural power-of-two alignment ⇒ every buffer 4 KiB-aligned regardless of size — "`sqz_buf_alloc` guarantees 4 KiB alignment, so the direct-drive DMA legs' dest-pointer gates pass and cold reads keep their `read_dest_dma_bytes` zero-daemon-copy arm", D:210; the LBA law the shim's slab derives from `L:140-149`). Decoupled from slots: "a client-side range allocator decoupled from slots … Slot-slab exclusivity was only ever the client's own anti-aliasing discipline" (D:217). Fragmentation "degrades to plain-pointer fallback per op (counted), never to failure" (D:300).

### 2.4 Violation detection and pricing (D:200-205) — and what poisons a buffer

* Free/realloc-while-SUBMITTED: structurally refused `-EBUSY` (above). Likewise `sqz_fs_disconnect` with any SUBMITTED buffer: `-EBUSY`, never deferred **[resolution]** (an unmap under a live lease is the same custody hazard).
* Write-while-SUBMITTED: "undetectable cheaply, priced at exactly the `write(2)` bar … The daemon is already immune by construction: §5.3.1 rule 1 (snapshot-then-validate …) and rule 2 (every derived value … computes from the severed private copy, never a second arena read) … an app violating its lease can corrupt only its own file bytes on a file it holds a mode-checked, kernel-granted fd for" (D:203). The daemon side that makes this true: one descriptor snapshot after `try_begin_serve` (`L:574-586`, `S:163-183`), the severed copy rule (`H:1118-1125`), the arena window validated once at dequeue (`H:1094-1099`, `H:881-894`).
* `SQUEEZEFS_SDK_PARANOID=1`: "xxh3 stamp at submit, re-hash at completion, one loud stderr line per mismatch (diagnostic only …). Never a default — one hash pass per op re-spends the deleted copy" (D:204).
* What poisons a BUFFER (exhaustive): the SESSION poisons while the buffer is SUBMITTED. Session poison causes (shared client core): op completion deadline (`P:1344-1346`, default 30 s `P:42`), daemon generation bump (`P:766-770`; the daemon's idle reap `PD:399` and teardown), ring push impossible (`P:1117-1121`), fork child (`P:754-760`). An app-visible `-EIO`/`-EBADF` completion does NOT poison anything — it is a normal CQE.

### 2.4a The transition matrix (every `(state, call)` pair; the model's alphabet)

| Call ↓ / state → | APP_OWNED (`inflight = 0`) | SUBMITTED (`inflight > 0`) | POISONED | FREED |
|---|---|---|---|---|
| `sqz_submit` referencing it | `inflight += 1` → SUBMITTED | `inflight += 1` (several SQEs, D:177) | `-EINVAL` for that SQE (the range may still be written by the dead session's late completion; the app may `memcpy` out — reads are legal in the table — and free) | `-EINVAL` |
| CQE reaped for it | — | `inflight -= 1`; 0 ⇒ APP_OWNED | stays POISONED (late CQE of a dead session is never delivered: `sqz_reap` on a poisoned session returns the ready list only) | — |
| `sqz_buf_free` | → FREED, range to the allocator | **`-EBUSY`, state unchanged, never deferred** | → FREED, range QUARANTINED (never to the allocator) | `-EINVAL` (idempotent-refused, D:302) |
| session poison | unchanged (APP_OWNED buffers of a dead session are still valid memory; future ops on them run POSIX) | → POISONED, `sdk_buf_poisoned += 1` | — | — |
| `sqz_fs_disconnect` | freed with the arena | `-EBUSY` | unmapped with the arena (the daemon's own mapping keeps the memfd pages alive for its late write, `PD:398`) | — |
| app writes bytes | legal | violation, priced at the `write(2)` bar (D:203) | violation (daemon may still DMA, D:198) | UB (use after free) |

Lease identity (tested): `allocs ≡ frees + live + poisoned_live`, where `poisoned_live` are POISONED records not yet freed; the quarantine set only grows for a session's life (never-recycle).

### 2.5 Why S1 and the consume copy die — and what does NOT (D:209-211)

* Write: bytes are already in the arena at submit; "the daemon's side is byte-identical to today: dequeue → snapshot → **the ONE §5.5.2 sever/merge pass** … → DMA. The write ledger becomes `S2 (~2, NT) + DMA (~1.2) ≈ 3.2` adjusted DRAM B/B" (D:209; today ≈ 6.2 with S1 ≈ 3, D:27).
* Read: the descriptor's `arena_off` names the app's buffer; the E-IL2 dest machinery lands bytes in place (`V:265-275` hands the handler the validated window; `V:299-305` counts `ipc_read_dest_serves` when the reply is window-backed) — "`slab_read` has no equivalent" (D:210).
* NOT deleted: the daemon's sever/merge (it IS the isolation boundary — "deleting it would let a client mutate bytes after validation but before DMA/retention") and the daemon's one lawful tier→dest pass on warm reads (D:211).

### 2.6 Composition with the security boundary — unchanged (D:213-220)

(1) the arena stays a sealed per-session memfd and "the daemon already validates every descriptor against *its own* geometry (`arena_off + len` in bounds, `len ≤ max_op_bytes`) — **no wire or daemon-validation change is needed for decoupled buffers**" (D:217; the checks `H:1094-1099`); (2) the fd screen + `SO_PEERCRED` remain THE boundary (D:218); (3) "An SDK app only ever endangers its own session" (D:219); (4) the self-protection rules are "untouched and load-bearing" (D:220).

## 3. THE SECOND CLIENT KIND on the same wire

### 3.1 What differs from the shim (the interposition machinery that does not exist here)

The shim pays "dlsym chains, the TLS reentrancy guard, fd-table probes on every data call, the lseek offset discipline, the libaio emulation state machine — all taxes paid to stay invisible. An app that *links* SqueezeFS needs none of them" (D:28). Concretely absent in the SDK: the fd table + binding refcounts across `dup` (`PD:317`), the bound-fd offset mirror (`PD:326-331`), `mmap`/`fcntl` demotion walks (`PD:311-312`), the per-op 20–40 ns interposition tax (D:287). Present and shared: session rendezvous + fd screen ("**Reused verbatim** — the SDK speaks the same HELLO/BIND wire", D:48), the sealed-memfd session (D:49), the completion doorbell (D:50), the daemon serve paths (D:51), engagement verification (D:53).

### 3.2 The wire the SDK depends on — the FROZEN SUBSET (D:265)

"session header lines 0–2 (geometry, wake words, doorbell), the `IpcSlot` descriptor layout, the MPSC ring cell protocol, the arena addressing rules, and the ctl messages `HELLO/BIND/UNBIND/SESSION_OK/REFUSE`. Everything else (daemon internals, serve paths, stats) stays free." As shipped today (the port re-derives these as `extern struct`s with `comptime` size pins; it does NOT owe these bytes):

* Header page (`L:307-343`, pins `L:660-668`): line 0 `{magic u64 "SQZIPC01" (L:77), abi u32, _pad0 u32, generation u64, heartbeat u64}`; line 1 @64 `{doorbell u32, daemon_parked u32, coalescer u8}`; line 2 @128 `CqeDoorbell` 16 B `{seq, parked, wake_at, wake_paid_mark}`; geometry @192. PERF-18 falsified at `L:642-659`: splitting the wake pairs onto their own lines measured 2.46× WORSE in the symmetric shape — keep them co-located.
* `Geometry {ring_entries u32, slots u32, arena_bytes u64, max_op_bytes u32, _pad}` (`L:111-126`), defaults 1024 / 1024 / 64 MiB / 1 MiB (`L:94-96`), rules `L:163-195` (`slots ≤ ring_entries`, arena a 4 KiB multiple ≤ 2^40, `max_op ≤ arena`).
* Slot = 128 B (`L:83`): `SlotCore {state u32, _pad, generation u64}` (`S:94-100`) + descriptor `{op u32, flags u32, binding u64, offset u64, len u32, ingress_stamp u32, arena_off u64, result i64}` (`L:506-524`); states FREE 0 → CLAIMED 1 → SUBMITTED 2 → SERVING 3 → DONE 4, WAITER = 1<<8 (`S:57-68`); the generation guard against ABA (`S:29-37`).
* Region arithmetic (`L:258-303`): header 4096 | ring (128 B tail line + 8 B cells) | slots | stats page | arena, each page-rounded; defaults ⇒ arena at 151 552, total 67 260 416 B.
* Ctl (`W:261-271`, encodings `W:345-421`): `Hello{abi, pid, uid, build_commit[48], nonce[32]}` = 96 B with the credential fd on `SCM_RIGHTS`; `SessionOk{geometry}` = 24 B with the sealed memfd; `Refuse{class}`; `Bind` (+fd); `BindOk{binding_id u64, ino u64, read_ok, write_ok}`; `BindRefused{class}`; `Unbind{binding_id}`. `RefuseClass` 1 Version, 2 Nonce, 3 Flags, 4 Mode, 5 Budget, 6 Peercred, 7 Internal, 8 Disabled (`W:68-98`). Bootstrap blob `user.squeezefs.il0` (`W:24`): `{magic "SQZIL0", abi, flags, build_commit[48], socket[108], socket_path[108], nonce[32]}` (`W:210-237`).
* `user_data` is NOT a wire field (the descriptor has none, `L:491-499`): it lives in a client-private side table indexed by slot (`ticket = {slot, generation}`, `P:113-116`). Nothing in shm is trusted (`L:28-32`), so nothing the app needs back may live there.

**Additions for kind SDK [the port's wire, day one — no bump history]:**

```
Hello      += { client_kind u32 (0 = SHIM, 1 = SDK), sdk_abi u32, arena_bytes_req u64, slots_req u32, _pad u32 }   (D:263, D:224)
Bind       += { durability u32 (0 = MOUNT, 1 = SYNC) }                                                         (§5)
BindOk     += { durability_granted u32 }                                                                       (§5)
Blob       += { sdk_abi_min u16, sdk_abi_max u16 }  (the client-side pre-check names both values, the KD-7 pattern P:495-504)
Header line 0: _pad0 → sdk_abi u32  (the frozen-subset version word, distinct from the coarse ipc_abi; §9 row B)
Descriptor flags bit 0 = F_ARENA_NATIVE (client-asserted, display-only — like the ingress stamp L:514-521)
```

### 3.3 How the binding is established — yes, the SDK still opens the file on the kernel mount

1. `sqz_fs_connect(mount)`: `fstatfs` magic → read `user.squeezefs.il0` (ENODATA ⇒ POSIX mode, D:234) → decode blob (total decoder, `W:240-255`) → client pre-check (`abi`, `sdk_abi ∈ [min,max]`) → connect abstract then path rung with the path-trust screen (`P:506-527`) → **authenticate the daemon before sending anything**: peer uid ∈ {0, mount owner} (`P:531-554`) → HELLO with kind SDK + the credential fd → `SessionOk` + memfd → require all three seals `F_SEAL_SEAL|SHRINK|GROW` (`P:578-587`, `P:1721`) → map, validate header + geometry equality (`P:640-647`) → repeat for K sessions (fd-sharded registry, D:162). **The HELLO credential [resolution; 0002 row D]**: `open(mount_path, O_RDONLY|O_DIRECTORY|O_CLOEXEC)` — the mount root. The reference HELLO screen demands `S_ISREG` (`H:26-27`) because it is the BIND screen reused (`H:2452-2504`); for kind SDK the daemon's HELLO screen admits `S_ISDIR` (still refusing `O_PATH` — "obtainable with *search-only* permission", `H:21-25` — and still requiring `st_dev == mount`, `H:28-30`). Why eager: `sqz_buf_alloc(fs, len)` precedes any `sqz_open` in the API's natural order (D:135-141), so an arena must exist at connect; and `sqz_fs_mode` must be truthful at connect (D:154). The BIND screen is unchanged.
2. `sqz_open(fs, path, oflags, mode, opts)`: real `open(2)` → classify (`fstatfs` by `st_dev`, cached — `charter.md:244`) → the fd is the credential: BIND it with the durability word. The daemon screens it (`H:2452-2504`): `O_PATH` refused; `S_ISREG`; `st_dev == mount`; `O_APPEND`/`O_TMPFILE`-class/`st_nlink == 0` refused; `O_SYNC|O_DSYNC` refused for kind SHIM and for kind SDK **admitted iff `durability == SYNC`** (§5.4); access mode ⇒ `read_ok`/`write_ok` (wrong direction = `EBADF` per op, `H:1100-1117`). A refused BIND is NOT silent: `sqz_open` returns the handle in POSIX mode for the refusals POSIX can honor identically (`Flags`/`Mode`/`Budget`) and records the reason; `O_APPEND` and `O_PATH` requests return `NULL` with `EINVAL` (D:88, D:154).
3. **Which session [resolution; 0002 row C]**: the BUFFER names the session — an arena offset is meaningful only in its own session's memfd. `sqz_buf_alloc` allocates from the calling thread's affine session (round-robin assignment at a thread's first alloc); `sqz_submit` routes each SQE to `buf.session` and BINDs `(file, session)` lazily on first use (≤ K ctl round trips per file over its life; a `_raw` op uses the thread's affine session and that session's slot slab). `sqz_close` UNBINDs every established binding, then `close(2)`.

### 3.3a The kind-SDK frames, byte-exact (little-endian, one SEQPACKET datagram each, `W:9-11`; tag first)

| Frame | Layout (offset: field) | Size |
|---|---|---|
| `Hello` (tag 1) | 0: tag u32 · 4: ipc_abi u32 · 8: pid u32 · 12: uid u32 · 16: build_commit [48] NUL-padded · 64: nonce [32] · **96: client_kind u32 · 100: sdk_abi u32 · 104: arena_bytes_req u64 · 112: slots_req u32 · 116: _pad u32** + the credential fd on `SCM_RIGHTS` | 120 (was 96, `W:355-360`) |
| `SessionOk` (tag 2) | 0: tag · 4: ring_entries u32 · 8: slots u32 · 12: arena_bytes u64 · 20: max_op_bytes u32 · **24: features u64** (additive feature bits, D:264) + the sealed memfd | 32 (was 24, `W:362-368`) |
| `Refuse` (tag 3) | 0: tag · 4: class u32 (`W:68-98`) | 8 |
| `Bind` (tag 4) | 0: tag · **4: durability u32** + the file fd on `SCM_RIGHTS` | 8 (was 4) |
| `BindOk` (tag 5) | 0: tag · 4: binding_id u64 · 12: ino u64 · 20: read_ok u32 · 24: write_ok u32 · **28: durability_granted u32** | 32 (was 28, `W:380-384`) |
| `BindRefused` (tag 6) / `Unbind` (tag 7) | unchanged (`W:386-393`) | 8 / 12 |
| blob `user.squeezefs.il0` | 0: magic "SQZIL0\0\0" · 8: ipc_abi u32 · 12: flags u32 · 16: build_commit [48] · 64: socket [108] · 172: socket_path [108] · 280: nonce [32] · **312: sdk_abi_min u16 · 314: sdk_abi_max u16** | 316 (was 312, `W:227-237`) |

Kind SHIM sends `client_kind = 0, sdk_abi = 0, arena_bytes_req = 0, slots_req = 0` and the daemon runs the KD-7 ladder on it unchanged — "this section *adds* a second kind, it relaxes nothing existing" (D:267). The daemon decodes totally: a `client_kind` outside `{0, 1}` is `Refuse{Version}` (counted `ipc_sdk_abi_refusals` — an unknown kind is a version event, not a flags event).

### 3.3b What an SDK descriptor must pass daemon-side (unchanged code, `H:1065-1117`) — and the CQE it gets

| Check | Failure → `res` | Counted |
|---|---|---|
| `op ∈ {ECHO, READ, WRITE}` (`H:1088-1090`) | `-EINVAL` | `ipc_descriptor_rejects` |
| `binding` live in this session (`H:1091-1093`) | `-EINVAL` (a stale binding after poison/unbind) | same |
| `len ≤ max_op_bytes` (`H:1094-1096`) | `-EINVAL` | same |
| `arena_off + len ≤ arena_bytes`, overflow-checked (`H:1097-1099`, `H:881-885`) | `-EINVAL` | same |
| `READ` on a binding without `read_ok` / `WRITE` without `write_ok` (`H:1104-1115`) | `-EBADF` (kernel-identical for the wrong direction) | same |
| the serve itself | bytes or the data plane's errno (`-EIO`, `-ENOSPC`, `-EFBIG`, …) | `ipc_ops_*` |

The SDK never emits the first four in a correct build; any of them is `sdk_descriptor_rejects` (must stay 0, §9 row F). Note the daemon validates `arena_off` as an arbitrary offset — exactly why decoupled buffers need no daemon change (D:217).

### 3.4 Submit / reap on the shared core

Submit = `claim_slot` (CAS scan with a rotating hint, `P:1384-1394`) → `publish_descriptor{op, flags, binding, offset, len, arena_off = buf.base + buf_off}` (`L:565-572`) → ingress stamp (`L:548-555`) → `publish_submitted` (Release, `S:146-155`) → ring push → doorbell bump + `FUTEX_WAKE` iff `daemon_parked` (`P:1123-1131`). ONE doorbell per `sqz_submit` batch (D:156). Reap = scan tickets (`is_done_for`, `S:207-217`) → `park_begin_batch(k)` → mandatory re-scan → `FUTEX_WAIT(seq)` bounded ≤ 50 ms slices under the op deadline (`P:1339-1357`) → `park_end` (`C:17-24`, `C:285-352`). Consume maps a negative result to the CQE's `res` verbatim (`P:1361-1376`; the `EINVAL ⇒ fallthrough` arm becomes a counted fallback, §9 row F).

`sqz_submit(fs, sqes, n)` step by step **[resolution]**:

1. Validate every SQE up front (opcode, live `file`, live `buf`, `buf_off + len ≤ buf.len`, `len ≤ 2^31`): the first bad one makes the whole call `-EINVAL` with nothing submitted (a batch is atomic in validation, never in execution — io_uring's shape).
2. For SQE *i*: `s = buf.session`; if `buf` is non-resident (heap fallback) or `s` is poisoned → run the op inline on the real fd (`pread64/pwrite64`), push `{user_data, res}` on `fs.ready` (an MPSC list sized `Σ slots`), count `sdk_posix_fallback_ops`; else ensure `(file, s)` is bound (lazy BIND, ctl mutex, `P:691-719`); `buf.inflight += 1`; claim a slot on `s` — none free ⇒ stop here and return *i* if *i > 0*, else serve this one inline and continue (the design's "arena exhausted ⇒ POSIX per op", D:239, applied to slots as the shim's `il_slot_reroutes` does, `L:436-443`); chunk `len > max_op_bytes` into flights (each its own slot/ticket, the prefix law `P:784-794`); record `tickets[slot] = {gen, user_data, buf, chunk}`.
3. Per session touched: one doorbell bump + conditional wake. Return the count of SQEs whose first chunk was published or served.

`sqz_reap(fs, cqes, max, timeout)` **[resolution]**: drain `fs.ready` first; then until `max` or the deadline: scan the caller's visible tickets (per-session ticket tables; a done ticket is consumed exactly once by a CAS on its `gen`), fold chunked flights to one CQE in offset order (`P:789-791`), `buf.inflight -= 1` per completed op, `release` the slot (`S:258-262`); if nothing completed, `park_begin_batch(k)` on EVERY session the fs holds (the entries may span sessions — `P:118-127`), re-scan, `futex_waitv`/`wait_any` with the smaller of `timeout` and the 50 ms slice (`P:1937-2017`), `park_end`. A session whose oldest ticket exceeds `op_timeout` poisons (`P:1344-1346`); poisoned tickets complete `-EIO` and their buffers POISON (§2.4).

### 3.5 The arena geometry request (D:224) — the daemon's clamp **[resolution]**

```
req_arena  = hello.arena_bytes_req orelse default_arena            // SQUEEZEFS_IPC_ARENA_MB / derived
granted    = min(req_arena, MAX_ARENA_BYTES, cap_bytes − arena_bytes_in_use) rounded DOWN to 4 KiB
if granted < max_op_bytes: granted = default_arena                 // never below the default geometry
slots      = clamp(hello.slots_req orelse default_slots, 1, ring_entries)
footprint  = SessionLayout.compute({ring_entries, slots, granted, max_op_bytes}).total_bytes
admit iff footprint fits the cap (H:2284-2289) and the per-uid cap (H:2290-2301); else Refuse{Budget}
```

"Refusal degrades to the default geometry, never fails the session" (D:224) is the `granted < max_op_bytes` arm; a session that cannot fit even the default refuses `Budget` exactly as today. The grant is what `SessionOk.geometry` says and what the header carries (`L:340-342`); the client re-derives `slot_slab` from it (`L:150-159`) for `_raw` ops.

## 4. VERSIONING over a one-release window (SKD-4)

* The problem: "KD-7's same-commit law is correct for the shim … and wrong for SDK apps (shipped on app cadence — a daemon upgrade must not silently degrade every fleet app to POSIX until each is rebuilt)" (D:259). The shim keeps KD-7 verbatim (`PD:602`; the daemon's check `H:2218-2236`; degenerate `unknown`/`-dirty` identities `W:484-486`).
* **Words**: HELLO `client_kind` (`SHIM = 0 | SDK = 1`) and `sdk_abi: u32` (D:263). "Kind SDK ⇒ the window law: the daemon accepts `sdk_abi ∈ {SDK_ABI_CURRENT, SDK_ABI_CURRENT − 1}` and refuses older/newer (refusal ⇒ POSIX fallback + reason line naming both values). `SDK_ABI` starts at 1 … and **bumps only on breaking changes to the frozen subset**; additive evolution rides `SessionOk` feature bits" (D:264).
* **"One release" means one release TRAIN**: "An SDK app built against train N keeps its ring on daemon N and N+1; daemon N+2 may retire `sdk_abi = N`'s support **as a release act** (release-notes item …). Refusal is always safe — POSIX fallback is complete (§7) — so the window is a *performance* promise, not a correctness one" (D:266). The train is the charter's semantic release train "bumped only as a release act across all first-party components; git commit is the fine-grained identity" (`charter.md:287`). G-S4: "an SDK binary built at release train N establishes against daemon N and N+1; N+2 refuses cleanly to POSIX fallback with the reason line" (D:79).
* Port **[resolution]**: `sdk.SDK_ABI_CURRENT: u32 = 1`; the daemon's accepted set is the comptime array `sdk_abi_accepted = .{1}` (becomes `.{2, 1}` at a bump and is pruned to `.{2}` as a release act, each a ledger row); the blob carries `sdk_abi_min/max` so the client refuses without the round trip; the daemon enforces authoritatively with `RefuseClass.Version` and counts `ipc_sdk_abi_refusals` (D:273). The SDK does NOT read `SQUEEZEFS_IPC_ALLOW_DEV` — degenerate commit identities are a commit-equality concept; an SDK app pairs with a `-dirty` daemon whose `sdk_abi` is in the window (§9 row G).
* **The window over a release timeline** (what G-S4's matrix enumerates):

| Daemon train | daemon `sdk_abi_accepted` | App built at train N (`sdk_abi` = 1) | App built at train N+1 (`sdk_abi` = 2, after a frozen-subset break) |
|---|---|---|---|
| N (first SDK release) | `{1}` | RING | — (does not exist yet) |
| N+1 (a breaking change shipped) | `{2, 1}` | RING (the window) | RING |
| N+2 (sunset of 1 as a release act) | `{2}` | **POSIX** + `sdk abi outside the daemon's window (app 1, daemon accepts 2..2)` | RING |
| N+1 with NO breaking change | `{1}` | RING | (no bump happened: app is still `sdk_abi` = 1) |

A bump is forced only by "a change that cannot be expressed additively" (D:266); a train without such a change leaves `SDK_ABI_CURRENT` alone — the window is counted in ABI numbers, retired per train.

* **Refusal texts** (once per `(mount st_dev, reason)` like the shim's `RefusalOnce`, `P:356-399`; the shim's own texts `P:249-318` are the lineage), on stderr with the `squeezefs-sdk:` prefix and ALSO returned by `sqz_fs_mode_reason`:
  * `sdk abi outside the daemon's window (app 1, daemon accepts 2..3) — POSIX mode` (D:236: "naming both versions");
  * `ipc abi mismatch (sdk 7, daemon 6) — POSIX mode` (the coarse word, `P:255-258` form);
  * `interception not armed on this mount (mount with --interception) — POSIX mode` (`P:299-301`);
  * `daemon refused: session budget / admission cap — POSIX mode` (`P:313`);
  * `daemon authentication failed: socket peer uid … is neither root nor the mount owner (…) — not sending the credential fd` (`P:277-280`), `… session memfd is not sealed (F_GET_SEALS = …)` (`P:281-284`);
  * `not a squeezefs mount (statfs) — POSIX mode`, `no bootstrap xattr — POSIX mode` (new, the SDK's connect rungs).

## 5. STRENGTHEN-ONLY per-open durability (SKD-6)

* Carrier: Idea 8's mount classes `-o data={ordered|writeback|sync}` — `ordered` (today's default: ACK-parked custody, eager upload, durable at fsync/close), `writeback` (deferred upload under an R5 dirty share + idle window), `sync` (per-write durability: upload + commit before ACK) — with the standing law "per-op override wins toward durability, never away" and "fsync semantics identical in all classes" (`R:601-622`).
* The per-open word (D:249-251): `SQZ_D_MOUNT` (default; the mount's class governs), `SQZ_D_SYNC` ("this handle's writes are per-write durable (upload + commit before the CQE) — the O_SYNC-equivalent the bind screen currently refuses, expressible because the SDK can carry the barrier per op … Strengthen-only: legal on any mount class"), and **deliberately no `SQZ_D_WRITEBACK`**: "A handle may never weaken below its mount's class (the Idea-8 law verbatim); relaxed-retention workloads mount `data=writeback`" (D:251).
* Why strengthen-only: weakening per handle would make the mount's crash-exposure cadence a function of which app opened the file — Idea 8 makes retention "a *stated* crash-exposure cadence instead of an accident" (`R:608-614`); a per-handle weakener would un-state it.
* How `fsync` changes: it does not. "`sqz_fsync` is identical in all classes (Idea 8's own keyed decision)" (D:253); it remains the real `fsync(2)` (D:131) whose soundness after ring writes is program-order → ring-ack → fsync-issue → daemon flush (`PD:386-388`). On a SYNC handle a ring write's CQE already implies the barrier, so `fsync` finds nothing volatile for that handle's writes — same call, same contract, cheaper.
* Daemon side **[resolution]**: BIND carries `durability`; the daemon stores it on the binding; for `OP_WRITE` on a SYNC binding the serve completes the slot only after the write's durable barrier (the port's `data/fsync.zig` single-barrier contract applied per op). Requests below the mount class are impossible to express (no weakener value); an unknown value is `BindRefused{Flags}` → `sqz_open` `-EINVAL`. `O_SYNC`/`O_DSYNC` in `oflags` ⇒ the SDK forces `durability = SYNC` (the open keeps the flag, so the POSIX-mode fallback is also per-write durable) and the kind-SDK BIND screen admits the flag (the shim-era reason "kernel path already provides" `H:32-33` no longer applies: the ring provides too). Engagement gauges: `ipc_bind_durability_sync` (bindings), `ipc_ops_write_sync` (writes); a durability row is INVALID unless they account for it.
* Sequencing in the reference: "SDK-4 lands **after** Idea 8 … until then `sqz_open_opts_t.durability` accepts only `SQZ_D_MOUNT` and the field exists so the ABI does not churn" (D:253). The port lands the field and the law together with its data plane (rung 4's fsync is already in `data/fsync.zig`); the `data=` mount class surface itself is rung 4/5 work this spec does not own.

## 6. DAEMON-SIDE service differences (everything else is byte-identical, D:34, D:290)

1. HELLO: decode `client_kind`, `sdk_abi`, geometry request; kind SHIM runs commit equality (`H:2219-2236`), kind SDK runs the window; the SDK credential may be `S_ISDIR` (§3.3); nonce (`H:2238-2246`), peercred (`H:2248-2256`), admission (`H:2266-2301`: footprint = `layout.total_bytes`, shed target, `arena_cap`, per-uid cap) unchanged.
2. Geometry request: "client asks, daemon clamps under the R5-derived admission cap … refusal degrades to the default geometry, never fails the session" (D:224). Clamp: `arena = min(req, cap − current, MAX_ARENA_BYTES)` rounded down to 4 KiB; `slots ≤ ring_entries` (`L:176-178`).
3. BIND: the durability word + the `O_SYNC` admission for kind SDK (§5).
4. Serve: descriptor validation unchanged (`H:1088-1099`); per-op rights unchanged (`H:1100-1117`); SYNC-binding writes barrier before DONE; the arena-dest read serve is already the default (`V:117-124`, `SQUEEZEFS_IL_READ_DEST`).
5. Stats: `ipc_sessions_by_kind_{shim,sdk}`, `ipc_sdk_abi_refusals` (D:273), `ipc_bind_durability_sync`, `ipc_ops_write_sync`; the client stats page gains the `sdk_*` words (summed verbatim, display-only, the `L:402-413` law).
6. Service threads: unchanged — one session per thread for life, spawned on admission up to `clamp(3 × cpus/8, 2, 64)` (`charter.md:244`, `Z:100-105`).

## 7. Knobs, gauges, constants — with WHY and the measured numbers

**Knobs (registry rows; class per `src/core/knobs.zig:57-71`):**

| Knob | Kind / class | Default | WHY (reader) |
|---|---|---|---|
| `SQUEEZEFS_SDK_PARANOID` | bool / `switch_` | 0 | xxh3 stamp at submit, re-hash at completion, one stderr line per mismatch; never default — "one hash pass per op re-spends the deleted copy" (D:204). Reader: `sdk/ops.zig` |
| `SQUEEZEFS_IL_OP_TIMEOUT_MS` | int 1..MS_MAX / `ab_lever` (shared with the shim) | 30 000 | per-op completion deadline; a miss poisons (`P:42`, `P:457-460`); `opts.op_timeout_ms` overrides per fs |
| `SQUEEZEFS_IL_SPINS` | int / `ab_lever` (shared) | adaptive 4096 / 256 | explicit pins both and disables adaptivity (`P:62-70`); 256 floor measured 336 k → 605 k IOPS at 64 threads (`P:52-58`) |
| `SQUEEZEFS_IL_SESSIONS` | int 1..16 / `derived_override` (shared) | `clamp(ceil(cpus/4), 2, 16)` | the ingest-economy derivation both sides ride (`Z:1-29`: +44 % when the mismatch was fixed by hand); `opts.sessions` is the app's lever |
| `SQUEEZEFS_IPC_ARENA_THP` | bool / `switch_` (shared, client half) | 1 | `MADV_HUGEPAGE` on the mapping (`P:616-636`) |
| `SQUEEZEFS_IL_KERNEL_LANE_MIN` | int / `derived_override` (shared) | derived | hybrid lane gate override for `_raw` ops (`P:410-414`); arena-native ops derive with `copies = 1` (§7 constants) |
| daemon: `SQUEEZEFS_IPC_ARENA_MB`, `SQUEEZEFS_IPC_MEM_MAX/PCT`, `SQUEEZEFS_IPC_SERVICE_THREADS`, `SQUEEZEFS_IL_READ_DEST` | existing rows | — | the SDK adds no daemon knob; `IL_READ_DEST=0` is the control arm that restores the reply bounce (`V:117-124`) |
| NOT added: `SQUEEZEFS_SDK_ARENA_MB` | — | — | the arena request is the app's `opts.arena_bytes`; the daemon's `SQUEEZEFS_IPC_ARENA_MB` is the operator's default; a third lever has no distinct reader |

**Gauges / counters:** daemon-side (measurement-truth): `ipc_ops_{read,write}`, `ipc_bytes_*`, `ipc_read_dest_serves` (`V:305`), `ipc_bind_refused_{version,nonce,flags,mode,budget,peercred,internal,disabled}` (`H:3136-3145`), `ipc_descriptor_rejects` (`H:1073-1075`), `ipc_sessions_{active,total,poisoned,reaped}` (`H:2396-2397`, `H:2567-2580`), new `ipc_sessions_by_kind_{shim,sdk}`, `ipc_sdk_abi_refusals`, `ipc_bind_durability_sync`, `ipc_ops_write_sync`. Client-side (display-only, on the stats page): `sdk_buf_{allocs,frees,busy_refusals,poisoned,inflight_bytes,fallback_ops}` (D:273) + `sdk_arena_native_writes` (replaces the unobservable `ipc_arena_dest_writes`, §9 row E) + `sdk_posix_fallback_ops` + the must-stay-0 tripwire `sdk_descriptor_rejects`. Closure identities: `allocs ≡ frees + live + poisoned`; `submitted ≡ reaped + inflight + poisoned_ops`; `sqz ops ≡ ipc_ops_* + sdk_posix_fallback_ops`.

**Constants (reason on the line):**

| Constant | Value | WHY |
|---|---|---|
| `SDK_ABI_CURRENT` | 1 | starts at 1, bumps only on frozen-subset breaks (D:263) |
| window | `{cur, cur−1}` | one release train (D:265) |
| `SQZ_BUF_ALIGN` | 4096 | O_DIRECT LBA / DMA dest gate (`L:140-149`, D:210) |
| `sizeof(sqz_sqe_t)` / `sizeof(sqz_cqe_t)` | 48 / 16 | pinned by `comptime` + `_Static_assert` (§10.2) |
| `OP_TIMEOUT_DEFAULT` | 30 s | `P:42` |
| `WAIT_SPINS` / `WAIT_SPINS_PARKY` | 4096 / 256 | `P:44-59` |
| `CTL_RECV_TIMEOUT` | 10 s | `P:95` |
| reap `k` | `clamp(pending/4, 2, pending)` | `charter.md:244`; `C:43-70` |
| geometry defaults | 1024 / 1024 / 64 MiB / 1 MiB | `L:94-96`; slab 64 KiB derived (`L:150-159`) |
| `MAX_ARENA_BYTES` | 2^40 | arithmetic sanity, admission is the budget (`L:98-100`) |
| arena admission cap | `budget / 8` (12.5 %), no absolute ceiling | `M:681-701` (the 2 GiB ceiling starved the 251 GB box: 13/48 HELLOs refused) |
| sessions per mount | `clamp(ceil(cpus/4), 2, 16)` | `Z:55-59`; drain lanes `clamp(ceil(3cpus/8), 2, 64)` `Z:100-105` (W8 622–636 k → W12 695–700 k) |
| `BUILD_COMMIT_LEN` / `NONCE_LEN` / `SOCKET_NAME_LEN` / `CTL_MSG_MAX` | 48 / 32 / 108 / 64 KiB | `W:27-36`, `W:276` (48 so `-dirty` fits and the gate SEES it) |
| hybrid lane gate | `memBW × 2.3 µs / copies`, copies = 2 raw, 1 arena-native | `charter.md:244` (≈ 140–164 KiB at 2 copies); the 1-copy form is a tie-tested derivation awaiting G-S2's bracket |
| DONE-wake economics | wakes only toward a parked reaper | `C:9-16` (the per-completion wake was the serialization term past ~525 k IOPS; flat park herd 17.8 M wakes / 22.2 M ops `C:48-49`) |

**Measured numbers the design prices against (D:280-290, E:143-156):** write 1 MiB stream il 25.69 GB/s at ≈ 6.2 adj DRAM B/B (S1 ≈ 3) → target ≈ 3.2 (−~48 %/byte); cold 1 MiB read 34.41 GB/s at 6.59 raw → ≈ 3.6; warm 4k 2 passes → 1; conversion floor +5.9 % per deleted pass (E-IL1, A-B-B-A); interposition tax ~20–40 ns/op; sync RTT 2–3 µs unchanged; G-S3 bar ≥ 633 k device-true (the libaio-shim row, D:78); charter il numbers warm 1,017,548 IOPS, device-true 622,112 (`charter.md:244`). None of these are the SDK's to claim before G-S2's counted A-B-B-A (D:36).

## 8. Tests (name — one sentence)

Portable (every host):
* `sdk: hello/bind/bindok/blob codecs round-trip and decode total on garbage` — fuzz every ctl frame with the new fields (`W:551-565` lineage).
* `sdk: version window accepts cur and cur−1, refuses cur−2 and cur+1` — the daemon's acceptance set and the client pre-check agree (G-S4, D:79).
* `sdk: refusal text names both abi values, once per (mount, reason)` — `RefusalOnce` semantics (`P:356-399`).
* `sdk: buffer lease — free while SUBMITTED is -EBUSY and the range stays allocated` — never deferred.
* `sdk: buffer lease — poisoned ranges never return to the allocator` — allocator census after a poison equals before minus the quarantined set.
* `sdk: buf_core model holds; the split-CAS weakening fails` — `expectHolds`/`expectFails` over `model/explorer.zig`.
* `sdk: buddy allocator — every alloc 4 KiB-aligned, no overlap, free coalesces` — property test over random alloc/free sequences.
* `sdk: fragmentation degrades to fallback, never refuses` — an unservable size yields a heap buffer flagged non-resident.
* `sdk: sqe/cqe extern structs are 48/16 bytes with the pinned offsets` — comptime.
* `sdk: durability word strengthen-only — SYNC admitted on every class, no weakener value decodes` — the enum has two values.
* `sdk: derivation ties — sessions default, drain lanes, lane gate at 1 and 2 copies, reap k` — each restates its formula.
* `sdk: header census — every export fn sqz_* has a prototype in squeezefs.h and no non-sqz_ symbol is exported` — a rails test over the source and the header.
* `sdk: warm pread/pwrite allocate nothing` — counting allocator (charter §12 "warm shim serve 0").

Linux venue (`scripts/venue zig build gate`):
* `sdk: C ABI pin — a C program compiled against squeezefs.h links the static and the shared library, _Static_asserts the sizes, calls every symbol` (§10.2).
* `sdk: parity — every op byte- and errno-identical on an armed mount, an unarmed mount and tmpfs; mode/reason honest` (G-S1, D:241).
* `sdk: engagement — ipc_ops_* deltas account for every ring op; sdk_posix_fallback_ops for the rest` (D:53, `charter.md:244` engagement law).
* `sdk: positional only — no offsetful symbol exists; sqz_fd() + read(2) still works` (D:155, D:132).
* `sdk: SYNC handle — kill -9 after the CQE, remount, bytes present` and `sdk: fsync after ring writes — write via ring, fsync, kill -9, verify` (`PD:386-388`).
* `sdk: batch — prefix semantics under slot exhaustion; inline POSIX completions reaped before parking; one doorbell per batch`.
* `sdk: multi-reaper — N reapers over one session never strand and never double-reap` (`C:26-41`).
* `sdk: op timeout poisons — later ops POSIX on the real fd, SUBMITTED buffers POISONED, disconnect succeeds, no UAF under ASan`.
* `sdk: fork — child poisons and closes its socket copy; parent session intact` (`P:754-760`).
* `sdk: geometry request clamps under the cap and degrades to default, never fails` (D:224).
* `sdk: adversarial — mutate a SUBMITTED buffer mid-serve on every op class; only the app's own file bytes change; ipc_descriptor_rejects and tripwires stay 0` (D:298).
* `sdk: paranoid lever — one stderr line per torn write, none when clean` (`log.testing.expectErrorLogs` if logged at error level).
* `sdk: dlclose after connect does not unload; a subsequent fork does not crash` (§10.3).
* Measurement rungs (not gate): G-S2 A-B-B-A arena-native vs `_raw` (≥ +5 % or ≥ −30 % DRAM B/B, D:77), G-S3 batch vs the libaio-shim row (D:78).

## 9. QUIRKS per decision 0002 — and the better designs (new ledger rows; numbers at landing)

| # | Reference quirk | Why it is poor | What we do |
|---|---|---|---|
| A | The sketched C ABI mixes conventions: POSIX `ssize_t`+errno (`sqz_pread`, D:141) beside io_uring-style negative returns (`sqz_buf_free` "-EBUSY", D:138) | Two signaling conventions in one 20-function API; a thread-local errno is a C-library artefact every binding pays for | ONE convention: every fallible `sqz_*` returns `-errno`, `errno(3)` is never written; constructors return NULL with the cause in `sqz_open_errno`/`sqz_fs_mode_reason` (§1.1) |
| B | ONE coarse `IPC_ABI` header word that bumps for stats-page-only changes (v4 `L:47-50`, v6 `L:56-63`) while the SDK's window is defined over a frozen SUBSET excluding the stats page (D:265); the client's `SessionHeader.validate` refuses on that word (`L:366-377`) | A stats-page field addition would refuse every in-window SDK app — the window would be void | Header line 0 carries `ipc_abi` (coarse, the shim's equality word) AND `sdk_abi` (the frozen-subset word) in the former `_pad0`; the SDK validates the window on `sdk_abi` only (§3.2) |
| C | `sqz_buf_alloc(fs, len)` with K fd-sharded sessions leaves "which arena" undefined (D:135, D:162): an arena offset is meaningful only in its own memfd | A buffer from arena j submitted on session i is an arena-bounds reject (`H:1097-1099`) or a bounce copy — S1 back by accident | The buffer names the session; `(file, session)` bindings establish lazily; thread-affine allocation (§3.3) |
| D | The HELLO credential screen is the BIND screen (`S_ISREG`, `H:26-27`), so a connect before the first open has no credential | `sqz_buf_alloc` precedes `sqz_open`; a lazy session would have no arena, and `sqz_fs_mode` would lie at connect | Kind SDK HELLO admits the mount-root directory fd (`O_PATH` still refused, `st_dev` still checked); BIND unchanged (§3.3) |
| E | `ipc_arena_dest_writes` named as THE daemon-side S1-deletion engagement gauge (D:273) while the same doc states the daemon "cannot observe" who wrote the arena bytes (D:34) | A measurement-truth gauge for a client-side fact | Client stats page `sdk_arena_native_writes` (display-only) + descriptor bit `F_ARENA_NATIVE` (display-only); engagement for G-S2 = `ipc_ops_write` + the uncore DRAM B/B instrument itself (§7) |
| F | The shim maps a daemon `EINVAL` descriptor reject to a silent real-call fallthrough ("the real call is the correct answer for a shim-side bug", `P:1368-1376`) | Hides a client-library bug behind correctness | The SDK falls back too (parity) but counts `sdk_descriptor_rejects` as a must-stay-0 tripwire (§7) |
| G | `SQUEEZEFS_IPC_ALLOW_DEV` admits degenerate `-dirty`/`unknown` identities (`P:329-353`, `H:2224-2236`); the design says the SDK's handshake only adds a kind and leaves the rest "unchanged" (D:263) without saying what degenerate identities mean under a window | A commit-equality override has no meaning for a client that never checks commits | The SDK never reads it; pairing is `sdk_abi ∈ window` regardless of either side's commit form (§4) |
| H | `sqz_pread/sqz_pwrite(f, buf, len, off)` can only address a buffer's base (D:141-142) | The natural io_uring shape slices one big buffer into many ops | `sqz_sqe_t.buf_off` (§1.2); the sync wrappers stay base-only |

Not quirks, carried as-is: the never-reuse law (`P:17-21`), snapshot-then-validate (`L:574-586`), the seal check (`P:578-587`), the daemon-authentication-before-credential order (`P:531-554`), the doorbell protocol (`C:26-41`), PERF-18's co-located wake words (`L:642-659`).

## 10. ZIG NOTES (0.17)

### 10.1 Module layout — one session client, two frontends, zero interposition in the core

```
src/ipc/layout.zig      extern structs: SessionHeader (4096, align 4096), IpcSlot (128, align 128), Geometry,
                        ClientStatsPage; comptime size/offset pins (doorbell @64, cqe @128, geometry @192)
src/ipc/wire.zig        HELLO/SESSION_OK/REFUSE/BIND/BIND_OK/BIND_REFUSED/UNBIND + blob: canonical total codecs
src/ipc/slot_core.zig   + slot_core_model.zig (the generation guard, park_prepare — explorer-checked)
src/ipc/cqe_core.zig    + cqe_core_model.zig (the doorbell; weakening: drop a SeqCst, fails)
src/ipc/ring_core.zig   the bounded MPSC index ring (+ model)
src/ipc/client.zig      Session: establish/bind/unbind/claim/submit/isDone/consume/release/parkBegin(k)/parkEnd/
                        waitAny/poison/poisonChild/poisoned/arena()  — NO fd table, NO offset mirror, NO TLS guard,
                        NO interposers; allocation-free after establish (the shim's stricter law, shared)
src/il/…                the shim: interposers + fd table + mirror + aio lanes OVER client.zig   (il shim spec)
src/sdk/buf_alloc.zig   buddy allocator over one arena (portable, property-tested)
src/sdk/buf_lease.zig   the lease state machine (+ buf_core_model.zig)
src/sdk/fs.zig, file.zig, ops.zig   handles, routing (buffer → session), submit/reap, the fallback ladder
src/sdk/abi.zig         the C ABI: export fns only; converts error unions → -errno through core/errors.toErrno
include/squeezefs.h     hand-written (§10.2)
```

Gating: `src/sdk/{buf_alloc,buf_lease,version_window}.zig` and the codecs are portable; `client.zig`, `fs.zig`, `abi.zig` touch memfd/futex/`SCM_RIGHTS` and compile their tests only on Linux via `if (builtin.os.tag == .linux) @import(...) else struct {}` in `src/squeezefs.zig` (the existing idiom, `src/squeezefs.zig:50-61`). The daemon imports `ipc/{layout,wire,slot_core,cqe_core,ring_core}` and never `ipc/client`.

**No `std.Io` in the client core.** The library runs inside a foreign process with no executor: establish and the ctl path use raw `std.os.linux` wrappers (`socket`, `sendmsg`/`recvmsg` with `SCM_RIGHTS`, `mmap`, `fcntl(F_GET_SEALS)`), the data path `sync/futex.zig` — the same posture the shim needs (AS-safe poison `P:733-745`). The daemon-side pieces take `io` as usual. Tests of the client core run against the daemon on the venue.

### 10.2 Exporting the C ABI and pinning the header

* `export fn sqz_pread(f: ?*SqzFile, b: ?*SqzBuf, len: usize, off: i64) callconv(.c) isize` — `callconv(.c)` explicit (the port's existing style, `src/cli/daemon.zig:324`); opaque handles are `pub const SqzFs = opaque {}` in `abi.zig` with the private struct behind `@ptrCast(@alignCast(...))`; ABI structs are `extern struct` with `comptime { std.debug.assert(@sizeOf(SqzSqe) == 48); }`; `off_t`/`ssize_t`/`size_t`/`mode_t` map to `i64`/`isize`/`usize`/`u32` on the Linux LP64 targets the product supports; `*const std.os.linux.timespec` for `struct timespec`.
* Every `export fn` body: no `unreachable` on app input; every error union → `-@intFromEnum(errors.toErrno(err))` (ONE table, `src/core/errors.zig:189-257`); a `null` handle → `-EINVAL`; a freed/stale handle generation → `-EINVAL` (D:302 "handles are opaque + generation-stamped; frees are idempotent-refused with errno"). ReleaseSafe stays on (decision 0002 row 23): a bounds violation inside the SDK is an SDK bug and aborts the host loud — the charter's §1.8 posture; expected failures never reach it.
* **No header emission in 0.17**: `zig build-lib --help` lists `-femit-h` and `std.Build.Step.Compile.getEmittedH` exists, but on 0.17.0 `zig build-lib t.zig -femit-h=t.h -dynamic` exits 0 and writes NO file (verified on this box). `include/squeezefs.h` is written by hand and pinned two ways: (a) a portable rails test parses `export fn sqz_(\w+)` from `src/sdk/abi.zig` and `sqz_(\w+)\s*\(` prototypes from the header and requires set equality (and no non-`sqz_` export); (b) a venue build step `sdk-abi-check`: `b.addExecutable` whose root module `addCSourceFile(tests/sdk_abi_check.c)`, `addIncludePath(b.path("include"))`, `linkLibrary(sdk_static)` — Zig compiles C natively, no `zig cc` subprocess needed — the program `_Static_assert`s `sizeof(sqz_sqe_t) == 48`, `sizeof(sqz_cqe_t) == 16`, `offsetof(sqz_sqe_t, user_data) == 40`, calls every symbol against tmpfs (POSIX mode) and exits 0; the gate depends on `b.addRunArtifact(abi_check)`; a second variant links the `.so` through `linkLibrary(sdk_shared)`.

### 10.3 Static + shared library build

```zig
const sdk_mod = b.createModule(.{ .root_source_file = b.path("src/sdk/abi.zig"), .target = target, .optimize = optimize,
    .imports = &.{ .{ .name = "squeezefs", .module = squeezefs_mod } } });
sdk_mod.link_libc = true;
const sdk_shared = b.addLibrary(.{ .name = "squeezefs", .linkage = .dynamic, .root_module = sdk_mod,
    .version = .{ .major = 1, .minor = 0, .patch = 0 } });   // SONAME libsqueezefs.so.1 by default
const sdk_static = b.addLibrary(.{ .name = "squeezefs", .linkage = .static, .root_module = sdk_mod });
sdk_static.bundle_compiler_rt = true;  // a static lib does NOT bundle compiler-rt by default
                                       // (std/Build/Step/Compile.zig:653: exe or dynamic only) — a gcc consumer would miss __zig/compiler-rt symbols
sdk_shared.installHeader(b.path("include/squeezefs.h"), "squeezefs.h");
b.installArtifact(sdk_shared); b.installArtifact(sdk_static);
```

`std.Build.addLibrary` + `LibraryOptions{name, linkage, version, root_module}` (`std/Build.zig:637-651`); `Compile.installHeader` (`std/Build/Step/Compile.zig:476`); `setVersionScript` (`Compile.zig:577`) can hide everything but `sqz_*` for the `.so`. LTO as the exe: `.thin` gate / `.full` dist on Linux only. The shared lib's SONAME: the Compile step emits a default SONAME when `.version` is set (`-fsoname` "Override the default SONAME value" in `zig build-lib --help`); the venue gate asserts it with `readelf -d` (the Tier-1 row's lineage, E:38-42).

**Unload safety.** The SDK registers a `pthread_atfork` child handler (the child must poison and `close(2)` its socket copy — session hygiene, not interposition: `P:754-760`, `PD:315`), and atfork handlers can never be unregistered (E:43-47), so the object must never unload. `std.Build.Step.Compile` exposes no `-z nodelete` field (only `link_z_{notext,relro,lazy,defs,…}`, `Compile.zig:134-149`) although `zig build-lib -z nodelete` works (verified for `x86_64-linux-gnu`). Port: pin at runtime — on the first `sqz_fs_connect`, `dladdr(&sqz_fs_connect)` → `dlopen(dli_fname, RTLD_NOW|RTLD_NOLOAD|RTLD_NODELETE)` makes `dlclose` a no-op for this object with zero build-system support; the venue test "dlclose after connect does not unload" pins it. The static library is unaffected.

### 10.4 Sketches: the lease word and one export fn (shape only; the port writes the real code red-first)

```zig
// src/sdk/buf_lease.zig — portable; the model in buf_core_model.zig restates exactly this word.
pub const State = enum(u8) { app_owned = 0, freed = 1, poisoned = 2 };
/// ONE 32-bit word: state in the top byte, inflight count in the low 24 bits, so "free iff
/// APP_OWNED and inflight == 0" is a single CAS (never a load-then-store pair — that is the
/// WEAKENING the model proves fatal: a submit between the two frees a leased range).
pub const Word = packed struct(u32) { inflight: u24, state: State };
pub const Lease = struct {
    word: std.atomic.Value(u32),
    pub fn submit(l: *Lease) error{Invalid}!void {           // APP_OWNED|SUBMITTED → inflight+1
        var cur: Word = @bitCast(l.word.load(.acquire));
        while (true) {
            if (cur.state != .app_owned) return error.Invalid; // FREED / POISONED never take new ops
            const next = Word{ .inflight = cur.inflight + 1, .state = .app_owned };
            // acq_rel: pairs with the reaper's release decrement and the freer's CAS below.
            cur = @bitCast(l.word.cmpxchgWeak(@bitCast(cur), @bitCast(next), .acq_rel, .acquire) orelse return);
        }
    }
    pub fn complete(l: *Lease) void {                         // reaper, after is_done_for's Acquire
        _ = l.word.fetchSub(1, .release);                     // low bits only; state byte untouched
    }
    pub fn free(l: *Lease) error{ Busy, Invalid }!enum { recycle, quarantine } {
        const cur: Word = @bitCast(l.word.load(.acquire));
        switch (cur.state) {
            .freed => return error.Invalid,
            .poisoned => { l.word.store(@bitCast(Word{ .inflight = 0, .state = .freed }), .release); return .quarantine; },
            .app_owned => {
                if (cur.inflight != 0) return error.Busy;     // -EBUSY, never deferred
                const want: u32 = @bitCast(Word{ .inflight = 0, .state = .app_owned });
                const next: u32 = @bitCast(Word{ .inflight = 0, .state = .freed });
                if (l.word.cmpxchgStrong(want, next, .acq_rel, .acquire) != null) return error.Busy;
                return .recycle;
            },
        }
    }
    pub fn poison(l: *Lease) bool {                          // session poison: SUBMITTED → POISONED
        var cur: Word = @bitCast(l.word.load(.acquire));
        while (cur.state == .app_owned and cur.inflight != 0) {
            const next = Word{ .inflight = cur.inflight, .state = .poisoned };
            cur = @bitCast(l.word.cmpxchgWeak(@bitCast(cur), @bitCast(next), .acq_rel, .acquire) orelse return true);
        }
        return false;
    }
};

// src/sdk/abi.zig — Linux-only; every export converts error unions to -errno through ONE table.
pub const SqzBuf = opaque {};
export fn sqz_buf_free(b: ?*SqzBuf) callconv(.c) c_int {
    const buf: *Buf = @ptrCast(@alignCast(b orelse return -@as(c_int, @intFromEnum(linux.E.INVAL))));
    const outcome = buf.lease.free() catch |e| return -@as(c_int, @intFromEnum(errors.toErrno(e)));
    switch (outcome) {
        .recycle => buf.session.alloc.free(buf.range),
        .quarantine => buf.session.stats.sdk_buf_poisoned_freed.add(1), // range NEVER returns to alloc
    }
    buf.session.stats.sdk_buf_frees.add(1);
    return 0;
}
```

`errors.toErrno` is the port's one two-way table (`src/core/errors.zig:189-257`): `error.Busy ↔ EBUSY` (`errors.zig:229`), `error.InvalidArgument ↔ EINVAL` (`errors.zig:225`); the SDK adds no table of its own. The header line for the same symbol is `int sqz_buf_free(sqz_buf_t *b); /* 0; -EBUSY while submitted; -EINVAL stale/double free */`.

### 10.5 Rails and conventions this feature must satisfy

Every `SQUEEZEFS_*` literal a registry row read by a module (`tests/rails.zig`); thread names unchanged (the SDK spawns no threads, D:162); counters through `counters.ShardedCounter`/`Gauge`/`Tripwire` on the daemon side, plain atomics on the client stats page (shm has no sharding; display-only); format codecs total (fuzz with `std.testing.fuzz`); `Environ.Map` knob reads only through `knobs.*Knob` with the row at comptime; `std.ArrayList` unmanaged (`.empty`, pass the allocator) in the non-hot connect path only — the data path owns its pools and allocates nothing per op (pinned by the counting-allocator test).
