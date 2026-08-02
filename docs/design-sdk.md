# Design Doc: S — the SqueezeFS native SDK (`squeezefs-sdk` + the `libsqueezefs` C ABI)

| | |
|---|---|
| **Title** | S: the native SDK — link-time, first-class SqueezeFS access for applications (arena-native buffers, io_uring-shaped batch API, explicit durability classes) |
| **Author** | Justin (repo owner) — SDK program owner |
| **Date** | 2026-08-04 |
| **Status** | **Draft — rev 1** (design phase; implementation is a follow-on campaign — the PR ladder in §13 is the plan of record, not concurrent work) |
| **Repo** | `dev` @ `68e8474`; branch `feat/sdk-design` |
| **User intent (2026-08-02, verbatim)** | *"build an application with 100% squeezefs support for the absolute fastest/safest access"* — link-time integration instead of LD_PRELOAD, up to a first-class SDK. |
| **Evidence base (normative)** | `.benchmarks/2026-08-02-read-copy-count.md` (the closed READ copy ledger + the §7 write-side field audit — **S1 ≈ 3 DRAM B/B is the prize**); `.benchmarks/2026-07-31-near-zero-copy.md` (the write copy census: the ring sever and kernel merge are DECLARED load-bearing — the SDK deletes neither); `.benchmarks/2026-07-19-l4-interception-closing.md` + `.benchmarks/2026-07-28-ipc-op-economy.md` (the shim data plane the SDK wraps: 1.0 M warm IOPS, completion doorbell, allocation-free serve prelude); `docs/design-preload-interception.md` (the L4 laws the SDK composes with) |
| **Inviolable contracts** | AGENTS.md non-negotiables (portable-by-default, zero-copy + latch-free hot paths, no dead code, TDD, forward-only); the §5.2 daemon fd screen as THE security boundary; the §5.5.2 severance law + §5.3.1 self-protection rules (unchanged — the SDK relies on them); the §5.4 zero-copy-write payload-lease discipline (the in-house prior art for the buffer-lease law); D0 single-writer guard + DLM/fencing (untouched); the charter labeling discipline (§3 of design-preload-interception — SDK rows are il-class rows) |
| **Related** | Tier-1 direct-link enablement (Deliverable 1, shipped WITH this design — §4); `docs/design-rewrite-program.md` §9.3 (Idea 8 named durability classes — SKD-6 composes with it); `crates/squeezefs-ipc` (the protocol crate the SDK wraps); `.benchmarks/2026-08-04-sdk-design.md` (campaign evidence note) |

**Revision history**

| Rev | Date | Change |
|---|---|---|
| 1 | 2026-08-04 | Initial design: keyed decisions SKD-1..6 resolved on paper; Tier-1 direct-link support shipped alongside (`crates/squeezefs-preload` SONAME/nodelete + linked-mode detection line + gate rows + operations.md) |

---

## 1. Overview

The L4 interception shim (`libsqueezefs_il.so`) already gives unmodified binaries a direct app→daemon data plane: shared-memory rings, zero syscalls at saturation, 1.0 M warm IOPS measured. But it is shaped by its constraint — *the host application must not know it exists*. That constraint costs, on both sides of the boundary:

1. **The boundary copies.** The app hands the shim *its own* buffers, so every write pays the app→arena copy (**S1** in the write ledger — ≈ 3 DRAM B/B of the il write row's ≈ 6.2, `.benchmarks/2026-08-02-read-copy-count.md` §7) and every read pays the arena→app consume copy (the `slab_read` pass — the same ≈ 3 DRAM B/B class, §9 "structural POSIX (the app hands us ITS buffer)"). Both are *structural to interposition*, not to the transport: an application that allocates its I/O buffers **from the session arena** never needs either.
2. **The interposition machinery.** dlsym chains, the TLS reentrancy guard, fd-table probes on every data call, the lseek offset discipline, the libaio emulation state machine — all taxes paid to stay invisible. An app that *links* SqueezeFS needs none of them.
3. **The same-commit law.** KD-7 locks the shim to the daemon's build commit — correct for an injected library the operator deploys *with* the daemon, wrong for an application that ships on its own release cadence.

The SDK program answers the user intent in two tiers:

* **Tier 1 (shipped with this design — §4)**: `-lsqueezefs_il` becomes a supported linkage of the *existing* shim. Zero new API. An app that links the shim gets the full interception data plane without LD_PRELOAD — surviving setuid/AT_SECURE env-scrubbing, `sudo`, systemd `Environment=` sanitization, and container images that never propagate env vars. Same KD-7 rules, same fallback ladder, same engagement discipline.
* **The SDK proper (this design; implementation = the §13 follow-on ladder)**: `crates/squeezefs-sdk` (Rust) + a `libsqueezefs` C ABI — an explicit handle API (open/pread/pwrite/close, batch submit/reap, per-open durability class) over the existing `squeezefs-ipc` protocol, whose centerpiece is **arena-native buffer allocation**: the app allocates I/O buffers from the session arena, so writes sever with **zero app-side copy** (S1 deleted) and reads complete **in place** (the consume copy deleted). The daemon side changes almost nothing: the §5.5.2 severance law, the §5.3.1 untrusted-shm discipline, and the §5.2 fd screen already treat the arena as client-writable memory for the whole serve — the SDK changes *who wrote the bytes into the arena*, which the daemon cannot observe and does not need to.

The prize, priced from the field ledgers (§11): the il write row drops from ≈ 6.2 to ≈ 3.2 adjusted DRAM B/B, the cold il read row from 6.59 to ≈ 3.6 — each by deleting one full CPU pass per payload byte on the client. The E-IL1 precedent (one deleted pass = +5.9 % on the copy-governed cold row, order-independent A-B-B-A) is the conversion floor; on client-CPU-walled fleets (the 2×200GbE ingest box class) the expected conversion is larger. The numbers are SDK-2's counted A/B to earn, not this document's to claim.

---

## 2. Background & Motivation

### 2.1 What the shim already proved (the SDK's foundation)

Everything hard about the app→daemon data plane is **landed, measured machinery** (`docs/design-preload-interception.md`, closing `.benchmarks/2026-07-19-l4-interception-closing.md`):

| Landed mechanism | Where | The SDK's relationship to it |
|---|---|---|
| Session rendezvous: bootstrap xattr + AF_UNIX + `SCM_RIGHTS` fd-passing; the §5.2 **daemon fd screen** as THE security boundary | `src/ipc_host.rs` | **Reused verbatim** — the SDK speaks the same HELLO/BIND wire |
| Sealed-memfd session: MPSC ring, completion-in-place slots, payload arena, futex doorbell, `WakeCoalescer` wake economy | `crates/squeezefs-ipc` | **Reused verbatim** — the SDK is a second *client* of the same protocol crate |
| Completion doorbell (IPC_ABI 2): completion wakes paid only toward registered parked reapers | `cqe_core::CqeDoorbell` | **Exposed natively** — `sqz_reap` parks on it directly (no libaio emulation between) |
| §5.5.1 sync fast path + §5.5.2 sever-at-dequeue custody + severed-buffer pool + placed sever | `src/ipc_service.rs`, `src/placed_sever.rs` | **Unchanged** — the daemon-side copy discipline is load-bearing (near-zero-copy census); the SDK deletes *client*-side passes only |
| §5.3.1 daemon self-protection: snapshot-then-validate, single-read discipline, lease-refcounted unmap, bounded parks | `src/ipc_service.rs` | **Load-bearing dependency** — these rules are exactly why arena-native app buffers add zero daemon attack surface (§7.2) |
| Engagement verification (charter §3 rule 4): a row is INVALID unless daemon-side `ipc_ops_*` accounts for it | `tests/run_scoreboard.sh`, gate | **Inherited** — SDK rows are il-class rows under the same labeling discipline |

### 2.2 What interposition structurally cannot delete (the SDK's reason to exist)

The read-copy-count campaign closed the read ledger at "ONE CPU pass per served byte on both transports" — *daemon-side*. The residuals it explicitly priced and stopped on are the **client-side boundary passes**:

* Write: **S1** — `app buffer → arena` (`ring_pwrite`'s copy-in). Field-measured as the ≈ 3 DRAM B/B cached-copy term of the il write row's 7.22 raw / ≈ 6.2 adjusted (`.benchmarks/2026-08-02-read-copy-count.md` §7).
* Read: the `slab_read` consume copy — `arena → app buffer`. Ledger §9 disposition: *"structural POSIX (the app hands us ITS buffer) — priced"*. Same ≈ 3 DRAM B/B class.

Both exist **only because the app's buffers are foreign to the session**. A first-party API that hands the app arena-backed buffers deletes both without touching the daemon's copy ledger: the write descriptor points at bytes already in the arena (the daemon's ONE sever/merge pass is unchanged — it is the §5.2 isolation boundary and stays); the read completion leaves bytes in a buffer the app may read directly (E-IL2's serve-into-arena machinery already lands them there — today the shim then copies them out because the app's destination is elsewhere).

### 2.3 Why not more of the same interposition

The alternatives ladder from L4 §10 still holds (client-side NVMe rejected for authority/key reasons; path interception rejected as a userspace permission surface). The SDK is deliberately the *smallest* step past the shim: same wire, same daemon, same trust model — a new client library whose API removes the constraint (app opacity) that forces the boundary copies.

---

## 3. Goals & Non-Goals

### Goals (program gates — measured, paired, same-substrate; adjudicated by the follow-on campaign)

| # | Gate | Criterion | Method |
|---|---|---|---|
| **G-S1** | **API completeness + fallback parity** (SDK-1): every `sqz_*` op on a non-interception mount / absent daemon / refused HELLO behaves byte- and errno-identically to plain POSIX on the mount | zero divergence, contract-pinned | parity suite (SDK ops vs POSIX ops interleaved, both mount postures) |
| **G-S2** | **The S1 deletion converts** (SDK-2, the go/no-go): arena-native write path shows ≥ the E-IL1 conversion class on the copy-governed large-sequential il row — **≥ +5 % throughput or ≥ −30 % DRAM B/B** (uncore-counted) vs the boundary-copy SDK path, A-B-B-A, engagement exact; read consume-deletion adjudicated on the cold EXA read row with the same bar | miss ⇒ the arena-native surface self-deletes (no dead code); the plain-pointer SDK API remains | `tests/copy_census_rig.sh` venue (devsub-tcp + field window), medians of 3 |
| **G-S3** | **Batch API parity with libaio-over-shim** (SDK-3): `sqz_submit`/`sqz_reap` ≥ the fio libaio-shim row at matched shape (the interposition state machine deleted must never cost throughput) | ≥ 633 k device-true class on the reference substrate (the v1.1 libaio row) | fio external-engine or dedicated driver, engagement exact |
| **G-S4** | **Compat window honored** (SKD-4): an SDK binary built at release train N establishes against daemon N and N+1; N+2 refuses cleanly to POSIX fallback with the reason line | contract tests both directions | version-skew matrix (the KD-7 test lineage) |
| **G-S5** | **House gates**: full cargo gate on the new crate; loom on the buffer-ownership core (`sdk_buf_core`); the preload/session suites untouched-green; no dead code | — | per-PR, tiered per AGENTS |

### Non-Goals — what the SDK explicitly does NOT do (SKD-5, the scope fence)

* **No metadata-plane bypass.** `sqz_open` performs a real `open(2)` on the real mount (kernel permissions, namespaces, the daemon's open handler, lease setup) and then BINDs that fd — exactly the shim's handshake. `stat`/readdir/rename/xattrs/locks/`fsync` ride kernel FUSE. The pil4dfs-style path plane stays rejected (L4 §10-A: a userspace permission surface we refuse).
* **No client-side device access, no crypto keys, no fencing tokens in app space** (L4 §10-E verbatim — the daemon remains the single writer and single device-I/O owner).
* **No multi-daemon routing.** One mount = one daemon = one session set. Apps spanning mounts hold one `sqz_fs_t` per mount.
* **No cross-host transport.** Same-host by construction (shm).
* **No mmap surface, no `FILE*` surface, no O_APPEND/O_PATH/O_TMPFILE handles** (the bind screen rows are unchanged; `sqz_open` refuses what BIND would refuse — but loudly, with errno, instead of silently passing through).
* **No language bindings beyond C in v1.** The C ABI is the binding surface; Rust is first-party. (Python/Go ride the C ABI unofficially; official bindings are a post-v1 decision.)
* **No new durability semantics in SDK-1..3.** Acked-via-ring == acked-via-FUSE-WRITE, fsync is the durable barrier (§5.6.3 of L4 — inherited verbatim). SDK-4's per-open classes compose with rewrite-program Idea 8 (§10) and land only after Idea 8 does.
* **No stable wire ABI beyond the stated window.** The SDK freezes a *versioned subset* (§9); the shim keeps the same-commit law; the protocol remains forward-only.

---

## 4. Tier 1 (shipped with this design): direct-link support for the existing shim

**What shipped** (branch `feat/sdk-design`; evidence `.benchmarks/2026-08-04-sdk-design.md`):

1. **Constructor-ordering audit — verdict: already safe, by design.** The shim is deliberately ctor-free (*"no ctor ordering games; first call initializes"* — `interpose.rs` module doc): every global is a lazily-initialized `OnceLock`/atomic behind the first interposer call, real functions chain via `dlsym(RTLD_NEXT)` (position-relative, correct wherever the object sits in the link map, and libc is always *after* the shim — it is both the app's implicit `-lc` tail and the shim's own DT_NEEDED), and the TLS reentrancy guard uses `try_with` (TLS-unavailable ⇒ real call). A DT_NEEDED load therefore differs from LD_PRELOAD in exactly one way that matters — *other* libraries' constructors and C++ static initializers may call interposed symbols before `main` — and the lazy-init design already serves that window (pinned by the gate's ctor-context I/O row). Two real gaps were found and fixed:
   * **No SONAME**: rustc sets none on cdylibs, so a consumer linking the shim *by path* embedded the build-tree path as its DT_NEEDED entry. Fixed: `-Wl,-soname,libsqueezefs_il.so` (build.rs, cdylib-only).
   * **Unload safety**: `pthread_atfork` handlers can never be unregistered, so any load of this object must be permanent. LD_PRELOAD and DT_NEEDED never unload; `-Wl,-z,nodelete` closes the `dlopen` edge structurally (dlclose becomes a no-op).
2. **Linked-mode detection line** (the bootstrap path): on the first bootstrap-blob decode — the moment the shim knows it is live against an interception-armed mount — it prints once per process when LD_PRELOAD does not name it: `squeezefs-il: active via direct link (DT_NEEDED), not LD_PRELOAD — same KD-7 build pairing applies`. Pure classification in `session::load_mode_from` (unit-pinned, `crates/squeezefs-preload/tests/linked_mode_tests.rs`); exactly-once pinned by the gate.
3. **Gate battery rows** (`tests/run_preload_gate.sh`): leg 1e (unprivileged) — SONAME/NODELETE asserts, a cc-compiled harness linked `-Wl,--no-as-needed <shim>` running **without** LD_PRELOAD: ctor-context I/O round trip (the ordering pin), `dladdr` scope-occupancy proof (`pread64` resolves into the shim — the root-free engagement tell), byte parity; leg 2b-linked (root) — the same harness on the armed mount: ring engagement (`ipc_ops_*` deltas) + the detection line exactly once.
4. **Operator docs** (`docs/operations.md`): the link-order requirement (before `-lc`, wrap in `-Wl,--no-as-needed` — modern toolchains' `--as-needed` default drops the entry if the app happens to reference no interposed symbol: silent no-interception), the setuid/AT_SECURE + env-scrubbing wins, the missing-library failure-mode difference (a missing DT_NEEDED is fatal at exec; a missing LD_PRELOAD is a warning), and the unchanged KD-7 pairing rules.

**What Tier 1 deliberately does not change**: the same-commit KD-7 law (a linked app must still ship/refresh its `.so` with the daemon — the `dist/<target>` pairing-folder discipline; refusal is safe, the app runs passthrough with the reason line). That friction is *the* motivation for SKD-4's compat window below — the SDK's answer, not the shim's.

---

## 5. SKD-1 — API surface: Rust crate + C ABI, handle model, io_uring-shaped batch

### 5.1 Crate shape

* **`crates/squeezefs-sdk`** (Rust, the first-party surface): safe wrapper over `squeezefs-ipc`'s client machinery (the session/establish/ring code today living in `crates/squeezefs-preload/src/session.rs` is **factored into `squeezefs-ipc` as a `client` module** in SDK-1, consumed by both the shim and the SDK — one implementation, two frontends; the `sizing::il_sessions_default` tie-test precedent extends to the whole client core).
* **`libsqueezefs.so`** (C ABI, `crate-type = ["cdylib"]` from the same crate, `sqz_` symbol prefix): the binding surface for C/C++ and unofficial higher-language use. **No libc interposition, no `#[no_mangle]` libc names** — this library exports only `sqz_*` symbols and is safe to link into anything (no panic-profile collision either: it never `catch_unwind`s across foreign frames on data paths; the C ABI layer converts errors to errno returns).
* Dependencies: `libc` + `squeezefs-ipc` only (the shim's discipline — no tokio, no allocator games in host processes).

### 5.2 Handle model (C ABI; Rust mirrors with lifetimes)

```c
/* --- session --- */
sqz_fs_t   *sqz_fs_connect(const char *mount_path, const sqz_fs_opts_t *opts);
int         sqz_fs_mode(const sqz_fs_t *fs);        /* SQZ_MODE_RING | SQZ_MODE_POSIX */
const char *sqz_fs_mode_reason(const sqz_fs_t *fs); /* why POSIX: the refusal ladder, honestly */
void        sqz_fs_disconnect(sqz_fs_t *fs);

/* --- files (control plane rides kernel FUSE — scope fence) --- */
sqz_file_t *sqz_open(sqz_fs_t *fs, const char *path, int oflags, mode_t mode,
                     const sqz_open_opts_t *opts);  /* opts.durability: SKD-6 */
int         sqz_close(sqz_file_t *f);               /* real close(2); kernel FLUSH/RELEASE unchanged */
int         sqz_fsync(sqz_file_t *f);               /* real fsync(2) — §5.6.3 ordering soundness inherited */
int         sqz_fd(const sqz_file_t *f);            /* the real fd, for everything the SDK does not do */

/* --- arena-native buffers (SKD-2, the S2 PR) --- */
sqz_buf_t  *sqz_buf_alloc(sqz_fs_t *fs, size_t len); /* 4 KiB-aligned, arena-backed; aligned heap in POSIX mode */
void       *sqz_buf_data(sqz_buf_t *b);
size_t      sqz_buf_len(const sqz_buf_t *b);
int         sqz_buf_free(sqz_buf_t *b);              /* -EBUSY while submitted (§6 lease law) */

/* --- sync convenience (SDK-1: plain-pointer; SDK-2 adds the buf forms) --- */
ssize_t     sqz_pread(sqz_file_t *f, sqz_buf_t *b, size_t len, off_t off);
ssize_t     sqz_pwrite(sqz_file_t *f, const sqz_buf_t *b, size_t len, off_t off);
ssize_t     sqz_pread_raw(sqz_file_t *f, void *buf, size_t len, off_t off);   /* pays the boundary copy */
ssize_t     sqz_pwrite_raw(sqz_file_t *f, const void *buf, size_t len, off_t off);

/* --- io_uring-shaped batch (SDK-3) --- */
int         sqz_submit(sqz_fs_t *fs, const sqz_sqe_t *sqes, unsigned n);  /* ring pushes + ONE doorbell */
int         sqz_reap(sqz_fs_t *fs, sqz_cqe_t *cqes, unsigned max,
                     const struct timespec *timeout);                     /* parks on the CqeDoorbell */
```

Decisions inside the surface:

* **`sqz_open` = real `open(2)` + BIND.** The fd is the credential (§5.2 of L4 — unchanged); the handle wraps `{fd, binding, session shard, durability class}`. What BIND would silently passthrough in the shim (O_APPEND, non-regular, wrong mount), `sqz_open` either serves in POSIX mode or — for flags the SDK cannot honor ring-side *or* POSIX-side identically — refuses with errno. **Silent is for interposition; the SDK is explicit** (`sqz_fs_mode_reason` exists for the same reason the refusal lines do).
* **Offsetful ops do not exist.** No `sqz_read`/`sqz_write` without offsets — the lseek discipline (§5.4.3 of L4) was interposition-tax; a first-party API simply requires offsets (io_uring made the same call). Apps that want a cursor keep one.
* **`sqz_sqe_t` maps 1:1 onto the wire `IpcSlot` descriptor** (op, binding, offset, len, buffer, user_data), and `sqz_cqe_t` onto the completion (`result`, user_data). One `sqz_submit(n)` claims n slots, publishes them, and rings the doorbell **once** (the batch-publish discipline the shim's pipelined large-write path already proved). Ops larger than `max_op_bytes` chunk into pipelined flights with POSIX short-prefix semantics (client-side, exactly `ring_pwrite`'s law).
* **Sync convenience wrappers** are submit(1)+reap-inline with the adaptive spin window (`WAIT_SPINS` lineage) — the warm-read RTT stays the §5.5.1 fast-path 2–3 µs.
* **Rust surface**: `SqzFs`, `SqzFile`, `SqzBuf` with the lease law lifted into types — `submit` takes `SqzBuf` by value and the completion returns it, making write-after-submit and free-while-inflight **compile errors** in Rust (the C ABI enforces the same at runtime, §6). This is the cheapest place the lease law gets teeth.

### 5.3 Threading model

Sessions stay fd-sharded exactly as the shim's registry (K = `il_sessions_default`); handles are `Send`, ops on one handle are thread-safe (slot claiming is lock-free), `sqz_reap` is multi-reaper-safe via the doorbell's register→snapshot→re-scan law. No SDK-owned threads in v1 (no completion callbacks — reap is pull; a callback executor invites runtime entanglement the shim deliberately avoided).

---

## 6. SKD-2 — the buffer-lease law (the hard part)

### 6.1 Prior art, named

The in-house precedent is the transport payload lease (`docs/design-zero-copy-write-path.md` §5.4): a kernel-owned buffer is lent to the handler as `Bytes::from_owner`, consumed within one handler invocation (the lease-severance boundary), with **never-write-while-leased** enforced by parked COMMITs and the arena `Arc` making early unmap unrepresentable. The SDK's buffer-lease law is the same shape with the roles mirrored: there, the *daemon* leases *transport* memory to its own handler; here, the *app* leases *its own arena buffer* to the daemon for the duration of one op.

### 6.2 Ownership states (normative)

A `sqz_buf_t` is a client-side object over an arena range. Its state machine (client-enforced; loom model `sdk_buf_core`):

```
APP_OWNED ──sqz_submit (referenced by ≥1 SQE)──▶ SUBMITTED ──all referencing CQEs reaped──▶ APP_OWNED
    │                                                 │
    └─ sqz_buf_free ⇒ range returns to allocator      └─ sqz_buf_free ⇒ -EBUSY (never deferred-free:
                                                          a hidden deferral would recycle custody the
                                                          daemon may still be serving — the §5.4.1
                                                          timed-out-slot never-reuse law, verbatim)
    On session poison/timeout with the buffer SUBMITTED:
    the buffer enters POISONED — never recycled, never returned to the
    allocator (the daemon may complete into it arbitrarily late; the
    arena Arc teardown rule §5.3.1-4 already makes unmap-before-quiesce
    unrepresentable). Priced: leaked-by-design at ≤ arena scale, exactly
    like timed-out slots today.
```

**What the app may touch, when (normative):**

| State | App reads | App writes |
|---|---|---|
| APP_OWNED | yes | yes |
| SUBMITTED (write op) | yes (its own bytes) | **contract violation** — priced below |
| SUBMITTED (read op) | **torn** (serve in progress) — contract violation to *rely* on | contract violation |
| POISONED | yes (stale bytes) | contract violation (daemon may still DMA into it) |

### 6.3 Violation detection and pricing (honest, not aspirational)

* **Free/realloc-while-SUBMITTED: structurally refused** (`-EBUSY`; the inflight refcount lives in the buffer header). This is the violation class that could hurt *someone else's* op (custody recycling), so it is the one made unrepresentable.
* **Write-while-SUBMITTED: undetectable cheaply, priced at exactly the `write(2)` bar.** POSIX already says an app racing its own buffer against an in-flight write gets torn *content* — its own hazard, nobody else's. The daemon is already immune by construction: §5.3.1 rule 1 (snapshot-then-validate — descriptors are copied out before validation) and rule 2 (every derived value — checksums, verification compares, transform inputs — computes from the severed private copy, never a second arena read). **This is the load-bearing composition fact: the daemon was hardened against hostile mid-serve arena mutation from day one, so an app violating its lease can corrupt only its own file bytes on a file it holds a mode-checked, kernel-granted fd for** — which `write(2)` already lets it do.
* **Debug pricing** (`SQUEEZEFS_SDK_PARANOID=1`): xxh3 stamp at submit, re-hash at completion, one loud stderr line per mismatch (diagnostic only; the transport-lease debug-stamp precedent). Never a default — one hash pass per op re-spends the deleted copy.
* **Rust surface: the violations above are compile errors** (by-value submit, completion returns the buffer). The C ABI is where the runtime pricing applies.

### 6.4 Why S1 dies (write path) and the consume copy dies (read path)

* **Write**: the app assembles its payload *directly in* `sqz_buf_data()` — bytes are already in the arena when `sqz_submit` publishes the descriptor. The shim's `ring_pwrite` copy-in (S1) has no equivalent; the daemon's side is byte-identical to today: dequeue → snapshot → **the ONE §5.5.2 sever/merge pass** (severed-pool or placed-sever adoption) → DMA. The write ledger becomes `S2 (~2, NT) + DMA (~1.2) ≈ 3.2` adjusted DRAM B/B.
* **Read**: the descriptor's `arena_off` names the app's own buffer; the daemon's E-IL2 serve-into-arena-dest machinery (landed, engagement-gauged by `ipc_read_dest_serves`) already writes completions to descriptor-named arena windows. At DONE the bytes are where the app wants them — `slab_read` has no equivalent. Alignment: `sqz_buf_alloc` guarantees 4 KiB alignment, so the direct-drive DMA legs' dest-pointer gates pass and cold reads keep their `read_dest_dma_bytes` zero-daemon-copy arm.
* **What does NOT die, stated so nobody tries**: the daemon's sever/merge on writes (it IS the §5.2 isolation boundary — the near-zero-copy census *declared* it load-bearing; deleting it would let a client mutate bytes after validation but before DMA/retention, i.e. would move the security boundary), and the daemon's one lawful serve pass on warm reads (tier → dest; handing the app the tier's memory is isolation-fatal, rejected).

### 6.5 Composition with the §5.2 isolation boundary (WITHOUT weakening it)

Point by point, because this is the design's safety argument:

1. **The arena stays a sealed memfd, per-session, per-process.** Arena-native allocation changes the *client-side allocator policy* over the same mapping (today: slab-per-slot; SDK: a client-side range allocator decoupled from slots). The wire already carries arbitrary `arena_off` values and the daemon already validates every descriptor against *its own* geometry (`arena_off + len` in bounds, `len ≤ max_op_bytes`) — **no wire or daemon-validation change is needed for decoupled buffers**. Slot-slab exclusivity was only ever the client's own anti-aliasing discipline; the SDK's allocator provides the same exclusivity a different way.
2. **The fd screen + SO_PEERCRED remain THE boundary.** The SDK cannot express anything a hostile raw-socket client could not already send; every such shape is screened (the L4-3 adversarial matrix). No new daemon code paths trust anything new.
3. **An SDK app only ever endangers its own session**: its arena is invisible to other processes, its bindings are mode-checked against its own kernel-granted fds, its lease violations tear its own payloads, its resource use is bounded by the same per-uid caps + R5 admission (`ipc_session_arenas`). Cross-user secrecy and DoS posture are unchanged (§11 of L4).
4. **Daemon self-protection rules are untouched and load-bearing** (§6.3 above). The SDK adds *reliance* on them, not exceptions to them.

### 6.6 Geometry

Arena-native apps will want more than the 64 MiB/session default (`SQUEEZEFS_IPC_ARENA_MB`). SDK-2 adds a HELLO-time geometry *request* (client asks, daemon clamps under the R5-derived admission cap — the 2026-08-02 derived-cap law, `mem_budget::ipc_arena_cap`; refusal degrades to the default geometry, never fails the session). Buffers larger than the granted arena fall back to plain-pointer ops (the boundary copy returns for that op — counted, `sdk_buf_fallback_ops`).

---

## 7. SKD-3 — the fallback ladder (portable by default)

Every rung lands on **plain POSIX against the real mount fd — semantics identical**, because the control plane already rides kernel FUSE and `sqz_fd()` is a real fd:

| Rung | Detection | Behavior |
|---|---|---|
| Not a SqueezeFS mount at all | statfs magic / bootstrap xattr ENODATA | `sqz_fs_connect` succeeds in **POSIX mode** (the SDK is usable against any filesystem — apps need one code path); `sqz_fs_mode_reason` says why |
| SqueezeFS mount, interception not armed | HELLO → `RefuseClass::Disabled` | POSIX mode + reason (names `--interception`, the Rev-8 line discipline) |
| Version window exceeded (SKD-4) | HELLO refuse (version) | POSIX mode + reason naming both versions |
| Budget / admission refused | HELLO refuse (budget) | POSIX mode + reason |
| Daemon dies mid-flight | futex timeout → socket probe → generation (the §5.7 ladder) | session poisons; in-flight ops complete `-EIO`-or-retry per op class; **subsequent** ops on every handle run POSIX on the real fd (which surfaces exactly what kernel FUSE surfaces for a dead daemon — no better, no worse) |
| Per-op ineligibility (arena exhausted, op class unservable) | per op | that op runs POSIX on the real fd; the handle stays ring-bound |

Contract pins (SDK-1 tests): the parity suite runs the full API against (a) an armed mount, (b) an unarmed mount, (c) a plain tmpfs — byte- and errno-identical results, and mode/reason honestly reported. Engagement honesty: benchmark rows remain valid only via daemon-side `ipc_ops_*` deltas (charter rule 4 — `sqz_fs_mode` is client-truth, `ipc_*` is measurement-truth).

---

## 8. SKD-6 — durability classes per open (composes with rewrite Idea 8)

`docs/design-rewrite-program.md` §9.3 defines the mount-level classes (`data={ordered|writeback|sync}`, INIT-time, immutable per mount) with the standing law *"per-op override wins toward durability, never away."* The SDK's per-open class is exactly that override, named:

* `SQZ_D_MOUNT` (default): the mount's class governs (today: `ordered`).
* `SQZ_D_SYNC`: this handle's writes are per-write durable (upload + commit before the CQE) — the O_SYNC-equivalent the bind screen currently refuses, expressible because the SDK can carry the barrier per op instead of per description flag. Strengthen-only: legal on any mount class.
* **There is deliberately no `SQZ_D_WRITEBACK` per-open weakener.** A handle may never weaken below its mount's class (the Idea-8 law verbatim); relaxed-retention workloads mount `data=writeback`.

Sequencing: SDK-4 lands **after** Idea 8 (its classes and forced-device triggers are the semantics carrier); until then `sqz_open_opts_t.durability` accepts only `SQZ_D_MOUNT` and the field exists so the ABI does not churn. `sqz_fsync` is identical in all classes (Idea 8's own keyed decision).

---

## 9. SKD-4 — versioning: the SDK handshake and its compat window

**The problem**: KD-7's same-commit law is correct for the shim (deployed with the daemon, injected into arbitrary binaries) and wrong for SDK apps (shipped on app cadence — a daemon upgrade must not silently degrade every fleet app to POSIX until each is rebuilt).

**The design**:

* **HELLO gains `client_kind` (`SHIM=0` | `SDK=1`) and `sdk_abi: u32`** — an IPC_ABI bump (3) carried by SDK-1; the shim's own handshake is unchanged in behavior (kind SHIM ⇒ the same-commit law verbatim, including the degenerate-identity refusals and the counted dev override).
* **Kind SDK ⇒ the window law**: the daemon accepts `sdk_abi ∈ {SDK_ABI_CURRENT, SDK_ABI_CURRENT − 1}` and refuses older/newer (refusal ⇒ POSIX fallback + reason line naming both values). `SDK_ABI` starts at 1 (SDK-1) and **bumps only on breaking changes to the frozen subset** below; additive evolution rides `SessionOk` feature bits.
* **The frozen subset** (what `SDK_ABI` versions): session header lines 0–2 (geometry, wake words, doorbell), the `IpcSlot` descriptor layout, the MPSC ring cell protocol, the arena addressing rules, and the ctl messages `HELLO/BIND/UNBIND/SESSION_OK/REFUSE`. Everything else (daemon internals, serve paths, stats) stays free.
* **The stated window**: one release train. An SDK app built against train N keeps its ring on daemon N and N+1; daemon N+2 may retire `sdk_abi = N`'s support **as a release act** (release-notes item, the versioning-policy section of operations.md). Refusal is always safe — POSIX fallback is complete (§7) — so the window is a *performance* promise, not a correctness one; forward-only is preserved (no compat shims in the daemon beyond honoring a frozen layout it already speaks; a change that cannot be expressed additively forces the bump and the sunset).
* **Why the shim does not get the window**: the shim's client code and the daemon's service code co-evolve against unfrozen internals every campaign (Rev 11/12/13 all changed client behavior); freezing them would tax every future campaign. The SDK pays the freeze deliberately, on a narrow subset, because app-cadence delivery is its charter. (KD-7 stays normative for kind SHIM — this section *adds* a second kind, it relaxes nothing existing.)

---

## 10. Observability

Daemon-side (trusted, the measurement surface): SDK sessions are `ipc_*` sessions — plus `ipc_sessions_by_kind_{shim,sdk}` (admission split), `ipc_sdk_abi_refusals` (the window instrument, joining `ipc_bind_refused_*`), and — SDK-2 — `ipc_arena_dest_writes` (writes whose payload was already arena-resident: **the S1-deletion engagement gauge**; a G-S2 row is INVALID unless it accounts for the row's write ops). Client-side (untrusted, display-only — the shm stats-page law): `sdk_buf_{allocs,frees,busy_refusals,poisoned,inflight_bytes,fallback_ops}`. Existing engagement instruments (`ipc_ops_*`, `ipc_bytes_*`, `ipc_read_dest_serves`) apply to SDK rows unchanged.

---

## 11. Expected wins (priced against the field ledgers — the reviewer's numbers)

Venue for all rows: the 2026-08 field client (32-CPU 2-socket SPR, dual-200GbE, nvme-tcp nullblk substrate), `.benchmarks/2026-08-02-read-copy-count.md` (§3, §7) + `.benchmarks/2026-07-31-near-zero-copy.md`. "adj" = ramp-adjusted uncore DRAM bytes per payload byte.

| Row | Today (il shim) | SDK v1 (arena-native) | Deleted term | Basis |
|---|---|---|---|---|
| Write, 1 MiB stream (wri il: 25.69 GB/s, amp 1.17) | ≈ 6.2 adj (7.22 raw): **S1 app→arena ≈ 3** + S2 NT sever ≈ 2 + DMA ≈ 1.2 | **≈ 3.2 adj** | **S1 — the prize** (−~48 % DRAM/byte; one full client CPU pass/byte) | ledger §7; S2/DMA untouched (load-bearing, census verdict) |
| Read, cold 1 MiB (rd-il: 34.41 GB/s med, 6.59 raw) | RX + fill→arena (1 daemon pass) + **slab_read consume ≈ 3** | **≈ 3.6 raw class** | the consume copy (client pass/byte) | ledger §3.2/§9 ("structural POSIX — the app hands us ITS buffer" — the SDK removes the premise) |
| Read, warm 4k (§5.5.1 fast path, ~2–3 µs RTT) | tier→arena serve + consume copy (2 passes) | tier→app-buffer (1 pass) | the consume copy | E-IL2 dest machinery, landed; sub-floor NT exemptions unchanged |
| Throughput conversion | — | **floor: the E-IL1 class (+5.9 %** for one deleted pass on the copy-governed cold row, A-B-B-A, order-independent); expected larger on client-CPU-walled fleets (the ingest-economy 7.5-of-16.6 GB/s wall was client-side cost of exactly this class) | — | ledger §4; G-S2 is the counted adjudication |
| Per-op interposition tax (fd-table probe, TLS guard, dlsym chains; libaio emulation for async) | ~20–40 ns/op + the aio_core lane machinery | direct calls; native sqe/cqe batch | measurable at high-IOPS shapes only; G-S3 pins ≥ the 633 k libaio-shim row | L4 §5.8.2 |
| Sync RTT / qd1 4k | 2–3 µs warm | unchanged | (nothing — the transport is already 0-syscall) | honesty row |

Not claimed: any daemon-side CPU/DRAM change (the daemon path is byte-identical); any win on rows below the copy-governance threshold (rand-4k warm is machinery-bound, ledger §10.2); any specific field GB/s until SDK-2's A-B-B-A lands. Sustained-state rows and write-amplification columns per the standing measurement laws apply to every SDK acceptance row.

---

## 12. Risk register

| Risk | Sev | Mitigation |
|---|---|---|
| Lease-law violation classes corrupt beyond the violator | **P0 if true** | §6.5 composition argument: daemon hardening (§5.3.1) predates and covers hostile arena mutation; free-while-inflight structurally refused; POISONED custody never recycled; adversarial suite (SDK-2) mutates buffers mid-serve on every op class |
| The frozen subset (SKD-4) taxes future transport campaigns | Med | subset is minimal (layout + ctl verbs, not serve behavior); breaking pressure ⇒ SDK_ABI bump + windowed sunset as a release act; the shim keeps full-speed co-evolution |
| Arena-native allocator fragments under mixed buffer lifetimes (long-lived buffers pinning arena ranges the slot path wants) | Med | SDK sessions negotiate their own geometry (§6.6); allocator is range-based, not slot-coupled; fragmentation degrades to plain-pointer fallback per op (counted), never to failure — the P3 "fragmentation degrades, never refuses" law |
| G-S2 does not convert (the deleted pass is not the wall on the acceptance venue) | Med | the E-IL1 precedent is the floor argument; if it still misses, the arena-native surface self-deletes (no dead code) and the SDK remains a cleaner-API shim peer — stated in G-S2 |
| C ABI misuse (double-free, use-after-free of handles) | Med | handles are opaque + generation-stamped; frees are idempotent-refused with errno; ASan job in the SDK gate; the Rust surface is the recommended path |
| Fleet skew confusion (three client kinds: shim-preload, shim-linked, SDK) | Low | one detection surface: `.stats` kinds split + reason-bearing refusal lines everywhere + `sqz_fs_mode_reason`; operations.md carries the matrix |

---

## 13. PR ladder (implementation = a FOLLOW-ON campaign; this section is its charter)

Branch prefix `feat/sdk-…` off `dev`; every PR tests-first, full cargo gate, loom where marked.

* **PR SDK-0 — `docs(design): land design-sdk.md`** (this document; cross-links from AGENTS.md + design-preload-interception.md). *Shipped with the design campaign together with Tier 1 (§4).*
* **PR SDK-1 — `feat(sdk): crate skeleton + sync ops + fallback ladder`**: factor the session client out of `squeezefs-preload` into `squeezefs-ipc::client` (shim consumes it — behavior-pinned by the existing preload gate); `crates/squeezefs-sdk` + `libsqueezefs.so` C ABI; `sqz_fs_connect/open/pread_raw/pwrite_raw/close/fsync`; IPC_ABI 3 (`client_kind`, `sdk_abi`); POSIX-mode parity suite (G-S1); `tests/run_sdk_gate.sh` (leg 1 unprivileged: build + ABI-surface lint + tmpfs parity; leg 2 root: armed-mount engagement). Gate: full cargo + both client gates.
* **PR SDK-2 — `feat(sdk): arena-native buffers + the lease law`** (the campaign centerpiece + go/no-go): the client range allocator, `sqz_buf_*`, the §6.2 state machine + `-EBUSY`/POISONED laws, loom `sdk_buf_core` (weakening-verified), HELLO geometry request, `ipc_arena_dest_writes` engagement gauge, adversarial mid-serve mutation suite, **the G-S2 counted A-B-B-A** (devsub-tcp first, field window for the closing note). Miss ⇒ the surface self-deletes.
* **PR SDK-3 — `feat(sdk): batch submit/reap`**: `sqz_sqe/cqe`, one-doorbell batch publish, doorbell-parked reap (multi-reaper), >max_op flight chunking; G-S3 vs the libaio-shim row; fio external-engine driver for the house rigs.
* **PR SDK-4 — `feat(sdk): per-open durability classes`** (blocks on rewrite-program Idea 8): `SQZ_D_SYNC` strengthen-only law, class column in the scoreboard's durability-leveled rows (RW6 precedent). 
* **PR SDK-5 — `feat(tests): scoreboard sdk mode + closing report`**: `SQUEEZEFS_SB_MODES=sdk` rows (separately-labeled, never top-3-gating — the §3 charter extends verbatim), closing report with the §11 table re-printed as measured.

---

## 14. Alternatives considered

* **A. Shim-only forever (Tier 1 is enough).** Keeps S1 + consume copies and the same-commit law for apps. Rejected as the end state; shipped as the bridge (§4).
* **B. Fold the SDK into `squeezefs-preload`.** One crate, two personalities (interposers + `sqz_*`). Rejected: the shim's build discipline (panic-profile guard, feature-gated interposers, no-alloc constraints) exists *because* it injects into foreign processes; the SDK wants a normal library ABI. Shared code goes to `squeezefs-ipc::client` instead (one implementation, two frontends).
* **C. Expose the raw `squeezefs-ipc` protocol as the public API.** No stable surface, no lease law, every consumer re-derives custody rules. Rejected — the protocol stays explicitly non-stable except the SKD-4 frozen subset behind the SDK.
* **D. Client-side io_uring into NVMe (the recurring P3-A shape).** Re-rejected on L4 §10-E grounds (writer multiplication, key/token leakage, device access for unprivileged apps). Nothing about an SDK changes that calculus.
* **E. Kernel-mediated zero-copy (FUSE passthrough / zc-receive / devmem-TCP).** Interface-class programs with their own charters (read ledger §10.1); orthogonal to — and composable with — deleting the *client* boundary passes.

---

## 15. Open questions

| # | Question | Recommendation |
|---|---|---|
| OQ-S1 | Rust async (tokio) adapter in v1? | v1.1 — pull-mode reap composes fine under `spawn_blocking`; a native reactor integration deserves its own measurement |
| OQ-S2 | Register SDK arenas as uring fixed buffers unconditionally (deepening the §5.5.3 staged item)? | Ride the existing evidence-gated posture; registration slots are finite — budget-gated, measured at SDK-2 |
| OQ-S3 | Official non-C bindings cadence | Post-v1; the C ABI is designed binding-friendly (opaque handles, errno returns, no callbacks) |
| OQ-S4 | Should the shim's libaio lane machinery re-base onto `sqz_submit` internals? | Evaluate at SDK-3; only if it deletes code without perturbing the pinned aio contracts |

---

## Key decisions

| # | Decision | Rationale |
|---|---|---|
| SKD-1 | **Rust crate + C ABI (`sqz_*`), explicit handles, positional-only ops, io_uring-shaped batch over the existing wire; session client factored into `squeezefs-ipc::client`** | One implementation under both frontends; the API's shape is the transport's shape (slots/CQEs), so nothing is emulated; C ABI carries bindings without libc interposition hazards |
| SKD-2 | **Arena-native buffers under the buffer-lease law**: APP_OWNED→SUBMITTED→APP_OWNED with structural refusal only where violations cross custody (free-while-inflight, POISONED recycling); content races priced at the `write(2)` bar; daemon discipline (§5.3.1) unchanged and load-bearing; §5.2 boundary untouched — an SDK app endangers only its own session | Deletes S1 (≈ 3 DRAM B/B) and the read consume copy without moving the security boundary or touching the daemon's declared-load-bearing passes |
| SKD-3 | **Fallback ladder lands on plain POSIX on the real mount fd, semantics identical, reasons surfaced** (`sqz_fs_mode[_reason]`) | Portable-by-default; correctness never depends on the ring (the KD-6 lineage); explicit-not-silent because first-party callers deserve attribution |
| SKD-4 | **`client_kind` + `sdk_abi` handshake (IPC_ABI 3); SDK window = current + previous train over a minimal frozen subset; shim keeps the same-commit law** | App-cadence delivery without freezing daemon internals; refusal-is-safe makes the window a performance promise, not a correctness one; forward-only preserved |
| SKD-5 | **Scope fence**: no metadata bypass, no device access, no multi-daemon, no mmap/`FILE*`, C-only bindings, no new durability semantics before Idea 8 | Every fence line inherits a settled adjudication (L4 §10, D0, the survey) — the SDK re-litigates none of them |
| SKD-6 | **Durability = per-open strengthen-only override (`SQZ_D_MOUNT`/`SQZ_D_SYNC`) composing with rewrite Idea 8's mount classes; SDK-4 blocks on Idea 8** | "Toward durability, never away" is Idea 8's own law; the SDK adds the per-handle expression, not new semantics |

---

## References

* `docs/design-preload-interception.md` — the L4 program: §5.2 (fd screen — THE boundary), §5.3.1 (untrusted-shm self-protection), §5.5.2 (severance law / copy ledger), §5.4.1 (bail-out + never-reuse laws), KD-6/KD-7/KD-11, §10 alternatives.
* `docs/design-zero-copy-write-path.md` §5.4 — the payload-lease prior art (never-write-while-leased, lease-severance boundary, arena-Arc unmap rule).
* `.benchmarks/2026-08-02-read-copy-count.md` — the READ copy ledger + §7 write-side audit: **S1 ≈ 3 DRAM B/B**, the consume-copy disposition, the E-IL1 conversion precedent.
* `.benchmarks/2026-07-31-near-zero-copy.md` — the write copy census: sever/merge declared load-bearing; NT cost levers.
* `.benchmarks/2026-07-28-ipc-op-economy.md` — the completion doorbell (IPC_ABI 2) `sqz_reap` parks on; the allocation-free serve prelude.
* `docs/design-rewrite-program.md` §9.3 — Idea 8 named durability classes (SKD-6's carrier).
* `.benchmarks/2026-08-04-sdk-design.md` — this campaign's evidence note (Tier-1 verification, the priced table's provenance).
