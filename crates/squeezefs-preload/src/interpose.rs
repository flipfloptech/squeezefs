//! The libc **interposers** (§5.1 symbol table, §5.4 mechanics). Only
//! compiled into the shipped cdylib (`--features interposers`): these are
//! strong `#[no_mangle]` symbols that beat libc's in the dynamic scope —
//! an rlib carrying them would interpose inside any binary linking it.
//!
//! ## Discipline (each rule normative, §5.4)
//!
//! - **Chaining**: every symbol resolves its real function via
//!   `dlsym(RTLD_NEXT, …)` behind a lazily-initialized atomic — no ctor
//!   ordering games; first call initializes.
//! - **Reentrancy (TLS guard)**: any interposer entered while the guard
//!   is set calls the real function immediately. This is also what makes
//!   the shim's *own* plumbing safe: `Session::establish` calls
//!   `socket(2)`/`fcntl(2)` through libc, which the dynamic linker
//!   resolves against **our** exported symbols — the held guard routes
//!   those nested entries straight to the real functions.
//! - **Panics**: every body runs under `catch_unwind`; a panic poisons
//!   every session (global passthrough), writes ONE stderr line, and the
//!   op returns the real call's result. The `preload-release` profile +
//!   the crate-root `compile_error!` guard make `panic = "unwind"`
//!   structural, so this is enforceable, not aspirational.
//! - **Fallback-is-correctness (§5.4.2)**: every path that is not a
//!   served ring op is *literally the real call*. Wrong-direction ops on
//!   bound fds also passthrough — the kernel's own EBADF/EINVAL on the
//!   real fd is the correct, identical answer.
//! - **Offsetful ops (§5.4.3 as amended by PERF-7)**: bound fds whose
//!   binding the shim can prove unshared carry an **armed offset
//!   mirror** in the shared `BindingCell` (`fd_table.rs` module docs —
//!   the normative predicate lives there): `read`/`write`/`readv`/
//!   `writev` consume and advance the mirror with **zero syscalls**,
//!   and `lseek`/`lseek64` `SEEK_SET`/`SEEK_CUR` are pure arithmetic.
//!   Everything outside the predicate — unarmed bindings, post-fork
//!   descriptions, demoted fds — runs the original kernel-authoritative
//!   discipline: `lseek(SEEK_CUR)` → ring op → `lseek(SEEK_SET)`,
//!   serialized per description by the cell's offset lock (cold path
//!   by definition; positional ops never touch any of it). Every
//!   transition OUT of mirror authority flushes the mirror to kernel
//!   `f_pos` first (raw `SYS_lseek` — never the interposed symbol).
//!   Out-of-model f_pos consumers the shim cannot see (raw syscalls,
//!   io_uring, glibc-internal stdio/spawn paths, `SCM_RIGHTS`-passed
//!   fds) are the same residual class §5.4.3 already documented; the
//!   libc-visible ones (`sendfile`, `copy_file_range`, `splice`,
//!   `posix_spawn`) are interposed below as demote triggers.

use crate::bailout::{classify_fd, rwf_passthrough};
use crate::dev_cache::NegativeDevCache;
use crate::fd_table::{Binding, FdTable};
use crate::session::{refuse_reason, RefusalOnce, RingOutcome, Session, SessionError};
use squeezefs_ipc::wire::{BootstrapBlob, BOOTSTRAP_XATTR};

use libc::{c_char, c_int, c_long, c_uint, c_void, mode_t, off_t, size_t, ssize_t};
use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Once, OnceLock};

/// Linux `statfs.f_type` for FUSE filesystems ("FUSE" LE).
const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;

// ---------------------------------------------------------------------------
// global state
// ---------------------------------------------------------------------------

fn table() -> &'static FdTable {
    static TABLE: OnceLock<FdTable> = OnceLock::new();
    TABLE.get_or_init(FdTable::new)
}

fn dev_cache() -> &'static NegativeDevCache {
    static CACHE: OnceLock<NegativeDevCache> = OnceLock::new();
    CACHE.get_or_init(NegativeDevCache::new)
}

/// Session registry: (st_dev, shard) → leaked Session, slot index = the
/// binding's `session` token. Fixed-size lock-free probes (a process
/// talks to a handful of mounts); the CAS-insert loser's session drops
/// (its ctl socket EOF tears the daemon side — harmless).
///
/// **Why shards**: a session is pinned to ONE daemon service thread for
/// life (§5.5.1 single-consumer invariant), so one session bounds a
/// whole process to one dequeue thread — measured as the device-true
/// plateau (~285 k IOPS flat from 64 to 256 client threads, perf
/// showing only svc0 hot). Sharding bindings across K sessions BY FD
/// keeps every session single-consumer while letting the daemon's
/// admission spread them over service threads. K defaults to the shared
/// ingest-economy derivation `clamp(cpus/4, 2, SESSION_REGISTRY_SLOTS/2)`
/// (`squeezefs_ipc::sizing::il_sessions_default` — the same function
/// the daemon's service-thread ceiling rides); `SQUEEZEFS_IL_SESSIONS`
/// is an override lever only (clamped to the registry-derived ceiling).
///
/// The registry capacity itself is the shared STRUCTURAL constant
/// (`sizing::SESSION_REGISTRY_SLOTS` — the fixed static session table
/// the alloc-free interposer environment requires; derivation sweep
/// 2026-08-04): the per-mount ceiling derives from it on both sides,
/// so growing the table grows every ceiling with it.
const MAX_SESSIONS: usize = squeezefs_ipc::sizing::SESSION_REGISTRY_SLOTS;

fn sessions_per_mount() -> usize {
    static K: OnceLock<usize> = OnceLock::new();
    *K.get_or_init(|| {
        sessions_per_mount_from(
            std::env::var("SQUEEZEFS_IL_SESSIONS").ok().as_deref(),
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8),
        )
    })
}

/// Pure sizing form (unit-pinned below): default = the shared
/// ingest-economy derivation (`clamp(cpus/4, 2,
/// SESSION_REGISTRY_SLOTS/2)` — the SAME function the daemon's
/// service-thread ceiling rides, so the pair cannot drift); the env var
/// is an override lever only, clamped to the registry-derived ceiling.
fn sessions_per_mount_from(env: Option<&str>, cpus: usize) -> usize {
    env.and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, squeezefs_ipc::sizing::SESSION_REGISTRY_SLOTS / 2))
        .unwrap_or_else(|| squeezefs_ipc::sizing::il_sessions_default(cpus))
}

struct Registry {
    devs: [AtomicU64; MAX_SESSIONS],   // dev + 1; 0 = empty
    shards: [AtomicU64; MAX_SESSIONS], // shard index within the mount
    ptrs: [AtomicPtr<Session>; MAX_SESSIONS],
}

fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| Registry {
        devs: std::array::from_fn(|_| AtomicU64::new(0)),
        shards: std::array::from_fn(|_| AtomicU64::new(0)),
        ptrs: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
    })
}

impl Registry {
    fn by_dev_shard(&self, dev: u64, shard: u64) -> Option<(usize, &Session)> {
        let tagged = dev.wrapping_add(1);
        for i in 0..MAX_SESSIONS {
            if self.devs[i].load(Ordering::Acquire) == tagged
                && self.shards[i].load(Ordering::Acquire) == shard
            {
                let p = self.ptrs[i].load(Ordering::Acquire);
                if !p.is_null() {
                    // SAFETY: registered sessions are leaked (never freed).
                    return Some((i, unsafe { &*p }));
                }
            }
        }
        None
    }

    fn by_token(&self, token: usize) -> Option<&Session> {
        if token >= MAX_SESSIONS {
            return None;
        }
        let p = self.ptrs[token].load(Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            // SAFETY: registered sessions are leaked (never freed).
            Some(unsafe { &*p })
        }
    }

    fn insert(&self, dev: u64, shard: u64, session: Session) -> Option<(usize, &Session)> {
        let tagged = dev.wrapping_add(1);
        let boxed = Box::into_raw(Box::new(session));
        for i in 0..MAX_SESSIONS {
            if self.devs[i]
                .compare_exchange(0, tagged, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.shards[i].store(shard, Ordering::Release);
                self.ptrs[i].store(boxed, Ordering::Release);
                // SAFETY: just leaked; lives forever.
                return Some((i, unsafe { &*boxed }));
            }
            if self.devs[i].load(Ordering::Acquire) == tagged
                && self.shards[i].load(Ordering::Acquire) == shard
                && !self.ptrs[i].load(Ordering::Acquire).is_null()
            {
                // Racing establish for the same (mount, shard): keep the
                // winner. SAFETY: reclaiming our unpublished box.
                drop(unsafe { Box::from_raw(boxed) });
                return self.by_dev_shard(dev, shard);
            }
        }
        // Registry full: this mount stays passthrough (bounded, loud).
        // SAFETY: reclaiming our unpublished box.
        drop(unsafe { Box::from_raw(boxed) });
        stderr_line("squeezefs-il: session registry full — mount stays passthrough\n");
        None
    }

    /// Same-process poison (panic path): AS-safe flag + `shutdown(2)`
    /// per session.
    fn poison_all(&self) {
        for i in 0..MAX_SESSIONS {
            let p = self.ptrs[i].load(Ordering::Acquire);
            if !p.is_null() {
                // SAFETY: leaked session; poison is AS-safe by contract.
                unsafe { (*p).poison() };
            }
        }
    }

    /// Atfork **child** poison (§5.4.1 fork row): flag + `close(2)` of
    /// the child's inherited socket COPY — never `shutdown(2)`, which
    /// acts on the file description fork SHARES with the parent and
    /// would sever the parent's live session (the lifecycle suite's
    /// fork-law test is the pin). Slots also clear so the child's next
    /// intercepted op lazily establishes its OWN session; stale fd-table
    /// entries then route to empty registry slots ⇒ passthrough.
    fn poison_all_in_child(&self) {
        for i in 0..MAX_SESSIONS {
            let p = self.ptrs[i].swap(std::ptr::null_mut(), Ordering::AcqRel);
            self.devs[i].store(0, Ordering::Release);
            if !p.is_null() {
                // SAFETY: leaked session; poison_child is AS-safe by
                // contract and the child is single-threaded here.
                unsafe { (*p).poison_child() };
            }
        }
    }
}

thread_local! {
    static REENTRY: Cell<bool> = const { Cell::new(false) };
}

/// RAII reentrancy guard: `None` = already inside the shim on this
/// thread (or TLS unavailable) — the caller must take the real call.
struct Guard;

impl Guard {
    fn enter() -> Option<Guard> {
        REENTRY
            .try_with(|f| {
                if f.get() {
                    None
                } else {
                    f.set(true);
                    Some(Guard)
                }
            })
            .ok()
            .flatten()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = REENTRY.try_with(|f| f.set(false));
    }
}

/// Raw `SYS_lseek` — **never** the `lseek` symbol (which is now our
/// own interposer: an internal flush routed through it would be
/// swallowed by the mirror arm instead of reaching the kernel) and
/// never a `dlsym` resolve (the atfork-prepare flush runs in contexts
/// where `dlsym`'s allocator is off-limits). AS-safe by construction.
fn raw_lseek(fd: c_int, off: i64, whence: c_int) -> i64 {
    // SAFETY: raw syscall; the libc wrapper sets errno on failure.
    unsafe { libc::syscall(libc::SYS_lseek, fd, off, whence) as i64 }
}

/// Demote one fd's offset mirror to kernel authority (PERF-7): under
/// the description's offset lock, disarm-once and flush the captured
/// offset to kernel `f_pos`. Idempotent; a no-op for unarmed/unbound
/// fds. Callers: the unbind-class fcntl arms, the poison arms, and the
/// f_pos-consuming passthrough interposers (`sendfile`/`copy_file_range`/
/// `splice`). Never called while the same cell's offset lock is held
/// (those sites inline the disarm instead — std mutexes do not relock).
fn demote_and_flush(fd: c_int) {
    let Some((_b, m)) = table().lookup_with_mirror(fd) else {
        return;
    };
    let _g = m.lock_offsets();
    if let Some(off) = m.disarm_if_current() {
        raw_lseek(fd, off as i64, libc::SEEK_SET);
    }
}

/// The fork/spawn flush (PERF-7): epoch bump + one kernel `SEEK_SET`
/// per armed cell, so the child inherits — and the demoted parent
/// resyncs from — the true offsets. Wired to `pthread_atfork` PREPARE
/// (runs in the parent, pre-fork) and the `posix_spawn` interposers
/// (glibc spawns via clone, which never runs atfork handlers).
fn fork_flush_all() {
    table().fork_demote_flush(|fd, off| {
        raw_lseek(fd, off as i64, libc::SEEK_SET);
    });
}

/// One loud, allocation-free stderr line (panic/pathology paths).
fn stderr_line(msg: &str) {
    // SAFETY: plain write(2) to stderr; best-effort.
    unsafe {
        libc::write(2, msg.as_ptr() as *const c_void, msg.len());
    }
}

/// Once-per-(mount, reason) gate for the refusal lines (user directive
/// 2026-07-25): the establish ladder retries on every eligible open by
/// design; the LOGGING must not (the field report's 32-thread run
/// printed a line per open). Establish/bind context only — never a
/// signal handler — so the tiny mutexed set is safe here (§5.4).
fn refusal_once() -> &'static RefusalOnce {
    static ONCE: RefusalOnce = RefusalOnce::new();
    &ONCE
}

/// Bind-refusal keys live in their own code space: the same daemon
/// class refusing a HELLO and a BIND on one mount are distinct events,
/// each worth its one line.
const BIND_REFUSAL_KEY_BASE: u32 = 0x8000;

fn set_errno(e: c_int) {
    // SAFETY: thread-local errno write.
    unsafe { *libc::__errno_location() = e };
}

fn panic_poison() {
    registry().poison_all();
    stderr_line("squeezefs-il: PANIC in interposer — all sessions poisoned, passthrough\n");
}

/// SDK Tier-1 direct-link support (`docs/design-sdk.md` §4): one line,
/// once per process, on the FIRST bootstrap-blob decode — the moment
/// the shim knows it is live against an interception-armed SqueezeFS
/// mount — naming the load mode when it is NOT the LD_PRELOAD idiom
/// (DT_NEEDED direct link, dlopen, ld.so.preload). KD-7 is
/// mode-independent; the line makes a linked deployment auditable (the
/// gate's 2b-linked row pins exactly-once). Establish/bind context
/// only — never a signal handler (the RefusalOnce precedent).
fn announce_load_mode() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if crate::session::load_mode() == crate::session::LoadMode::Linked {
            stderr_line(
                "squeezefs-il: active via direct link (DT_NEEDED), not LD_PRELOAD — \
                 same KD-7 build pairing applies\n",
            );
        }
    });
}

// ---------------------------------------------------------------------------
// real-function chaining
// ---------------------------------------------------------------------------

macro_rules! real {
    ($sym:literal, $sig:ty) => {{
        static PTR: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
        let mut p = PTR.load(Ordering::Relaxed);
        if p.is_null() {
            // SAFETY: dlsym(RTLD_NEXT) with a NUL-terminated literal.
            p = unsafe {
                libc::dlsym(
                    libc::RTLD_NEXT,
                    concat!($sym, "\0").as_ptr() as *const c_char,
                )
            };
            PTR.store(p, Ordering::Relaxed);
        }
        if p.is_null() {
            None
        } else {
            // SAFETY: the symbol's C signature is $sig by libc contract.
            Some(unsafe { std::mem::transmute::<*mut c_void, $sig>(p) })
        }
    }};
}

/// Real-function chaining for the **libaio** symbols — `dlvsym` with the
/// symbol's explicit default version (`aio_glue::libaio_default_version`),
/// plain `dlsym` only when that version is absent (unversioned/static
/// libaio builds).
///
/// Why not `real!`: glibc < 2.36 `dlsym(RTLD_NEXT, …)` returns the BASE
/// version of a multi-versioned symbol — on EL8 that is
/// `io_getevents@LIBAIO_0.1`, a 4-argument compat wrapper whose internal
/// PLT call re-enters this interposer; called with the 5-argument 0.4
/// convention it register-shuffles until an integer lands in its
/// timeout register and faults inside libaio (the 2026-07-25 Rocky 8
/// field segfault, frames 0xF00/0xF1B). `dlvsym` is version-exact on
/// every glibc.
macro_rules! real_aio {
    ($sym:literal, $sig:ty) => {{
        static PTR: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
        let mut p = PTR.load(Ordering::Relaxed);
        if p.is_null() {
            let name = concat!($sym, "\0").as_ptr() as *const c_char;
            if let Some(ver) = crate::aio_glue::libaio_default_version($sym) {
                // One tiny NUL-terminated copy on the first resolve only.
                let mut vbuf = [0u8; 16];
                let vb = ver.as_bytes();
                if vb.len() < vbuf.len() {
                    vbuf[..vb.len()].copy_from_slice(vb);
                    // SAFETY: dlvsym(RTLD_NEXT) with NUL-terminated
                    // symbol + version strings.
                    p = unsafe {
                        libc::dlvsym(libc::RTLD_NEXT, name, vbuf.as_ptr() as *const c_char)
                    };
                }
            }
            if p.is_null() {
                // SAFETY: dlsym(RTLD_NEXT) with a NUL-terminated literal
                // (unversioned providers; the versioned trap cannot
                // apply where the version does not exist).
                p = unsafe { libc::dlsym(libc::RTLD_NEXT, name) };
            }
            PTR.store(p, Ordering::Relaxed);
        }
        if p.is_null() {
            None
        } else {
            // SAFETY: the symbol's C signature is $sig by libaio contract.
            Some(unsafe { std::mem::transmute::<*mut c_void, $sig>(p) })
        }
    }};
}

// ---------------------------------------------------------------------------
// bind / release plumbing
// ---------------------------------------------------------------------------

fn route_release(released: Option<Binding>) {
    if let Some(b) = released {
        if let Some(session) = registry().by_token(b.session) {
            session.unbind(b.binding_id);
        }
    }
}

/// Post-`open` classification (§5.4): negative-cache probe → FUSE magic →
/// bootstrap xattr → establish (once per mount) → screen → BIND. Called
/// with the guard held; every failure path leaves the fd passthrough.
///
/// `epoch` is the fork-epoch snapshot taken BEFORE the real `open`
/// syscall (PERF-7 bind-vs-fork race closure — `fd_table.rs` module
/// docs): a fork between snapshot and bind stales it, and the mirror
/// installs permanently unarmed (kernel-authoritative, conservative).
fn classify_and_bind(fd: c_int, epoch: u64) {
    if fd < 0 {
        return;
    }
    // Any stale entry on this number is released exactly as close would
    // (raw-syscall-closed fd whose number came back — §5.4.1).
    route_release(table().sweep_stale(fd));

    // SAFETY: fstat into a zeroed buf.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: plain fstat(2).
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return;
    }
    if dev_cache().contains(st.st_dev) {
        return;
    }

    // Shard by fd (the §5.5.1 concurrency lever — see Registry docs).
    let shard = (fd as u64) % sessions_per_mount() as u64;
    let session = match registry().by_dev_shard(st.st_dev, shard) {
        Some(s) => Some(s),
        None => {
            // SAFETY: fstatfs into a zeroed buf.
            let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
            // SAFETY: plain fstatfs(2).
            if unsafe { libc::fstatfs(fd, &mut fs) } != 0 {
                return;
            }
            #[allow(clippy::unnecessary_cast)] // f_type width varies by target
            if fs.f_type as i64 != FUSE_SUPER_MAGIC {
                dev_cache().insert(st.st_dev);
                return;
            }
            // FUSE, maybe ours: the once-per-mount xattr bootstrap.
            let mut blob = [0u8; 512];
            let name = concat!("user.squeezefs.il0", "\0");
            debug_assert_eq!(name.trim_end_matches('\0'), BOOTSTRAP_XATTR);
            // SAFETY: fgetxattr(2) into our buffer.
            let n = unsafe {
                libc::fgetxattr(
                    fd,
                    name.as_ptr() as *const c_char,
                    blob.as_mut_ptr() as *mut c_void,
                    blob.len(),
                )
            };
            if n <= 0 {
                // ENODATA/ENOTSUP: a foreign FUSE mount — cheap negative.
                dev_cache().insert(st.st_dev);
                return;
            }
            let Ok(blob) = BootstrapBlob::decode(&blob[..n as usize]) else {
                dev_cache().insert(st.st_dev);
                return;
            };
            // A decoded blob = an interception-armed SqueezeFS mount:
            // the shim is live in this process — say how it got here
            // (once; linked mode only).
            announce_load_mode();
            // The HELLO credential must itself survive the screen; an
            // ineligible first fd just defers establishment (never a
            // negative-cache entry — the MOUNT is ours).
            // SAFETY: F_GETFL on a live fd.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || classify_fd(flags, st.st_mode, st.st_nlink as u64).is_err() {
                return;
            }
            match Session::establish(&blob, fd, crate::BUILD_COMMIT) {
                Ok(s) => registry().insert(st.st_dev, shard, s),
                Err(e) => {
                    // Reason-bearing refusal line (user directive
                    // 2026-07-25), once per (mount, reason) for the
                    // process lifetime — the ladder itself retries on
                    // every eligible open by design.
                    if refusal_once().first(st.st_dev, e.reason_code()) {
                        let line = format!(
                            "squeezefs-il: session refused: {} — mount passthrough\n",
                            e.describe(blob.abi, &blob.build_commit, crate::BUILD_COMMIT)
                        );
                        stderr_line(&line);
                    }
                    return;
                }
            }
        }
    };
    let Some((token, session)) = session else {
        return;
    };

    // SAFETY: F_GETFL on a live fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || classify_fd(flags, st.st_mode, st.st_nlink as u64).is_err() {
        return; // §5.4.1 bind-time passthrough rows
    }
    match session.bind(fd) {
        Ok(grant) => {
            // PERF-7: arm the offset mirror with the fd's live kernel
            // offset (a fresh open is 0, but read it — one cold
            // syscall keeps any future bind site honest). A failed
            // read installs unarmed (kernel stays authoritative).
            let seed = raw_lseek(fd, 0, libc::SEEK_CUR);
            let displaced = if seed >= 0 {
                table().bind_with_mirror(fd, grant.to_binding(token), seed as u64, epoch)
            } else {
                table().bind(fd, grant.to_binding(token))
            };
            route_release(displaced);
        }
        Err(SessionError::Refused(class)) => {
            // Daemon BIND refusal: the fd stays kernel-served (KD-6
            // silence for the OP; the REASON prints once per
            // (mount, class) — its own key space, distinct from the
            // same class refusing a HELLO).
            if refusal_once().first(st.st_dev, BIND_REFUSAL_KEY_BASE | (class & 0xFFF)) {
                let line = format!(
                    "squeezefs-il: bind refused: {} — fd stays kernel-served\n",
                    refuse_reason(class)
                );
                stderr_line(&line);
            }
        }
        // Socket/poison-class failures on a live session: the poison
        // machinery owns that loudness (§5.4.1) — no line here.
        Err(_) => {}
    }
}

// ---------------------------------------------------------------------------
// ring data ops (shared shapes)
// ---------------------------------------------------------------------------

/// Inner ring verdict shared by the positional and offsetful shapes.
enum RingServe {
    /// Ring served `n ≥ 0` bytes (short/0 is POSIX-legal).
    Served(ssize_t),
    /// Daemon errno — kernel-identical for this op; caller sets errno.
    Errno(i32),
    /// Take the real call. `poisoned` = the session died (§5.4.1
    /// error-class row: the caller demotes the mirror and unbinds);
    /// `false` covers every clean fallthrough incl. transient ring
    /// backpressure (the binding stays).
    Real { poisoned: bool },
}

fn ring_positional_core(
    fd: c_int,
    buf: *mut u8,
    count: usize,
    offset: off_t,
    write: bool,
) -> RingServe {
    const REAL: RingServe = RingServe::Real { poisoned: false };
    if offset < 0 || count == 0 || buf.is_null() {
        return REAL; // the real call's errno/0 is the identical answer
    }
    let Some(b) = table().lookup(fd) else {
        return REAL;
    };
    if (write && !b.write_ok) || (!write && !b.read_ok) {
        return REAL; // kernel EBADF on the real fd is the correct answer
    }
    let Some(session) = registry().by_token(b.session) else {
        return REAL;
    };
    let out = if write {
        // SAFETY: the app's own (buf, count) contract, same as libc.
        let data = unsafe { std::slice::from_raw_parts(buf as *const u8, count) };
        session.ring_pwrite(b.binding_id, data, offset as u64)
    } else {
        // SAFETY: as above, writable.
        let data = unsafe { std::slice::from_raw_parts_mut(buf, count) };
        session.ring_pread(b.binding_id, data, offset as u64)
    };
    match out {
        RingOutcome::Served(n) => RingServe::Served(n as ssize_t),
        RingOutcome::Errno(e) => RingServe::Errno(e),
        RingOutcome::Fallthrough => RingServe::Real {
            poisoned: session.poisoned(),
        },
    }
}

/// `Some(result)` = the ring served it (or a real daemon errno);
/// `None` = take the real call. Positional shape — no offset state.
fn ring_positional(
    fd: c_int,
    buf: *mut u8,
    count: usize,
    offset: off_t,
    write: bool,
) -> Option<ssize_t> {
    match ring_positional_core(fd, buf, count, offset, write) {
        RingServe::Served(n) => Some(n),
        RingServe::Errno(e) => {
            set_errno(e);
            Some(-1)
        }
        RingServe::Real { poisoned } => {
            if poisoned {
                // §5.4.1 error-class row: the fd lives on kernel-served —
                // flush any armed mirror to kernel f_pos, then unbind
                // (ctl already silent on a poisoned session).
                demote_and_flush(fd);
                route_release(table().on_close(fd));
            }
            None
        }
    }
}

/// Offsetful completion (PERF-7): armed mirrors publish (the Dekker
/// pair — a lost arm means a concurrent demote raced this op, and the
/// op owns the kernel write-through of its final offset); unarmed fds
/// restore kernel `f_pos` exactly as before PERF-7.
fn finish_offsetful(fd: c_int, m: &crate::fd_table::MirrorHandle<'_>, armed: bool, new_off: i64) {
    if armed {
        if !m.publish(new_off as u64) {
            raw_lseek(fd, new_off, libc::SEEK_SET);
        }
    } else {
        raw_lseek(fd, new_off, libc::SEEK_SET);
    }
}

/// §5.4.3 offsetful shape (PERF-7): the armed mirror is the offset
/// authority — **zero syscalls** per served op; unarmed bindings keep
/// the kernel-authoritative resync discipline (`SEEK_CUR` → ring op →
/// `SEEK_SET`). Any fallthrough to the real call flushes an armed
/// mirror to kernel `f_pos` first and demotes the fd (the mirror never
/// re-arms — transient backpressure simply prices that fd back at the
/// pre-PERF-7 two-syscall discipline).
fn ring_offsetful(fd: c_int, buf: *mut u8, count: usize, write: bool) -> Option<ssize_t> {
    let (_b, m) = table().lookup_with_mirror(fd)?; // cheap pre-check
    let _l = m.lock_offsets()?;
    let armed = m.armed();
    let off = if armed {
        m.load() as i64
    } else {
        // SEEK_CUR touches only f_pos (no FUSE request; pinned by the
        // gate script's lseek row).
        let off = raw_lseek(fd, 0, libc::SEEK_CUR);
        if off < 0 {
            return None; // unseekable: the real call is the answer
        }
        off
    };
    match ring_positional_core(fd, buf, count, off as off_t, write) {
        RingServe::Served(n) => {
            if n > 0 {
                finish_offsetful(fd, &m, armed, off + n as i64);
            }
            Some(n)
        }
        RingServe::Errno(e) => {
            set_errno(e);
            Some(-1) // no bytes consumed: neither offset moves
        }
        RingServe::Real { poisoned } => {
            if armed {
                // The real call consumes kernel f_pos next: single-
                // issuer flush under the held lock (disarm-once).
                if let Some(cur) = m.disarm_if_current() {
                    raw_lseek(fd, cur as i64, libc::SEEK_SET);
                }
            }
            if poisoned {
                route_release(table().on_close(fd));
            }
            None
        }
    }
}

/// iovec chain: serve segment-by-segment; stop on a short segment.
/// `offset < 0` = offsetful (`readv`/`writev` shape, per preadv2 docs).
fn ring_iovec(
    fd: c_int,
    iov: *const libc::iovec,
    iovcnt: c_int,
    offset: i64,
    write: bool,
) -> Option<ssize_t> {
    if iov.is_null() || iovcnt <= 0 {
        return None;
    }
    let (b, m) = table().lookup_with_mirror(fd)?;
    if (write && !b.write_ok) || (!write && !b.read_ok) {
        return None;
    }
    // Hold the description's offset lock across the whole chain for the
    // offsetful shape (one atomic f_pos advance for the vector op).
    let (guard, armed, mut off) = if offset < 0 {
        let l = m.lock_offsets()?;
        let armed = m.armed();
        let cur = if armed {
            m.load() as i64
        } else {
            let cur = raw_lseek(fd, 0, libc::SEEK_CUR);
            if cur < 0 {
                return None;
            }
            cur
        };
        (Some(l), armed, cur)
    } else {
        (None, false, offset)
    };
    let start = off;
    let mut total: ssize_t = 0;
    let mut poisoned = false;
    let mut fell_through = false;
    for i in 0..iovcnt {
        // SAFETY: the app's iovec array contract, same as libc.
        let e = unsafe { &*iov.add(i as usize) };
        if e.iov_len == 0 {
            continue;
        }
        match ring_positional_core(fd, e.iov_base as *mut u8, e.iov_len, off as off_t, write) {
            RingServe::Served(n) => {
                total += n;
                off += n as i64;
                if (n as usize) < e.iov_len {
                    break; // short segment (incl. EOF 0) ends the vector op
                }
            }
            RingServe::Errno(e) => {
                // Daemon errno mid-chain: partial progress returns short
                // (POSIX-legal); zero progress surfaces the errno (no
                // bytes consumed — neither offset moves).
                if total == 0 {
                    set_errno(e);
                    return Some(-1);
                }
                break;
            }
            RingServe::Real { poisoned: p } => {
                poisoned = p;
                fell_through = total == 0; // partial returns short instead
                break;
            }
        }
    }
    if guard.is_some() {
        if total > 0 {
            if poisoned {
                // Short-served AND dying: park the final offset in the
                // kernel (disarm under the held lock — the fd unbinds
                // below and lives on kernel-served).
                if armed {
                    if let Some(_cur) = m.disarm_if_current() {
                        raw_lseek(fd, start + total as i64, libc::SEEK_SET);
                    }
                } else {
                    raw_lseek(fd, start + total as i64, libc::SEEK_SET);
                }
            } else {
                finish_offsetful(fd, &m, armed, start + total as i64);
            }
        } else if fell_through && armed {
            // Clean zero-progress fallthrough: the real vector call
            // consumes kernel f_pos next — flush + demote first
            // (single-issuer under the held lock).
            if let Some(cur) = m.disarm_if_current() {
                raw_lseek(fd, cur as i64, libc::SEEK_SET);
            }
        }
        // total == 0 without fallthrough (EOF Served(0) / all-empty
        // segments): a legitimate served 0 — no offset moves, no demote.
    }
    drop(guard);
    if poisoned {
        demote_and_flush(fd); // no-op for the offsetful shape (already demoted)
        route_release(table().on_close(fd));
    }
    if fell_through {
        None // real call serves (offsetful: kernel f_pos just restored)
    } else {
        Some(total)
    }
}

// ---------------------------------------------------------------------------
// interposer plumbing macro: guard + catch_unwind + real-call fallback
// ---------------------------------------------------------------------------

/// Interposer plumbing: resolve the real POINTER eagerly, but the real
/// CALL stays lazy — it executes only on the passthrough paths (no
/// guard, body says `None`, body panicked). The first sudo gate run
/// caught the eager-call form double-applying every bound-fd write
/// (real write + ring write = 2× file content); this laziness is the
/// load-bearing fix, pinned by the gate's cp/dd parity rows.
macro_rules! interposed {
    ($resolve:expr, $fallback:expr, |$f:ident| $call:expr, $body:expr) => {{
        let Some($f) = $resolve else {
            // No RTLD_NEXT symbol: nothing to chain to. $fallback must
            // synthesize the kernel answer (raw syscall or errno).
            return $fallback;
        };
        let Some(_g) = Guard::enter() else {
            return $call;
        };
        match catch_unwind(AssertUnwindSafe(|| $body)) {
            Ok(Some(r)) => r,
            Ok(None) => $call,
            Err(_) => {
                panic_poison();
                $call
            }
        }
    }};
}

// ---------------------------------------------------------------------------
// data symbols
// ---------------------------------------------------------------------------

type PreadFn = unsafe extern "C" fn(c_int, *mut c_void, size_t, off_t) -> ssize_t;
type RwFn = unsafe extern "C" fn(c_int, *mut c_void, size_t) -> ssize_t;
type VecFn = unsafe extern "C" fn(c_int, *const libc::iovec, c_int) -> ssize_t;
type PVecFn = unsafe extern "C" fn(c_int, *const libc::iovec, c_int, off_t) -> ssize_t;
type PVec2Fn = unsafe extern "C" fn(c_int, *const libc::iovec, c_int, off_t, c_int) -> ssize_t;

macro_rules! pread_like {
    ($name:ident, $sym:literal, $write:expr) => {
        /// §5.1 positional data row.
        ///
        /// # Safety
        /// C ABI interposer; argument contracts are libc's own.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            fd: c_int,
            buf: *mut c_void,
            count: size_t,
            offset: off_t,
        ) -> ssize_t {
            interposed!(
                real!($sym, PreadFn),
                {
                    set_errno(libc::ENOSYS);
                    -1
                },
                |f| unsafe { f(fd, buf, count, offset) },
                ring_positional(fd, buf as *mut u8, count, offset, $write)
            )
        }
    };
}

pread_like!(pread, "pread", false);
pread_like!(pread64, "pread64", false);
pread_like!(pwrite, "pwrite", true);
pread_like!(pwrite64, "pwrite64", true);

/// §5.1 offsetful data row (`read`) — kernel-offset discipline §5.4.3.
///
/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t {
    interposed!(
        real!("read", RwFn),
        {
            set_errno(libc::ENOSYS);
            -1
        },
        |f| unsafe { f(fd, buf, count) },
        ring_offsetful(fd, buf as *mut u8, count, false)
    )
}

/// §5.1 offsetful data row (`write`).
///
/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn write(fd: c_int, buf: *const c_void, count: size_t) -> ssize_t {
    interposed!(
        real!("write", RwFn),
        {
            set_errno(libc::ENOSYS);
            -1
        },
        |f| unsafe { f(fd, buf as *mut c_void, count) },
        ring_offsetful(fd, buf as *mut u8, count, true)
    )
}

/// §5.1 offsetful vector rows.
///
/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn readv(fd: c_int, iov: *const libc::iovec, iovcnt: c_int) -> ssize_t {
    interposed!(
        real!("readv", VecFn),
        {
            set_errno(libc::ENOSYS);
            -1
        },
        |f| unsafe { f(fd, iov, iovcnt) },
        ring_iovec(fd, iov, iovcnt, -1, false)
    )
}

/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn writev(fd: c_int, iov: *const libc::iovec, iovcnt: c_int) -> ssize_t {
    interposed!(
        real!("writev", VecFn),
        {
            set_errno(libc::ENOSYS);
            -1
        },
        |f| unsafe { f(fd, iov, iovcnt) },
        ring_iovec(fd, iov, iovcnt, -1, true)
    )
}

/// §5.1 positional vector rows (both LFS spellings — see the LFS-64
/// discipline note below).
macro_rules! pvec_like {
    ($name:ident, $sym:literal, $write:expr) => {
        /// # Safety
        /// C ABI interposer; argument contracts are libc's own.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            fd: c_int,
            iov: *const libc::iovec,
            iovcnt: c_int,
            offset: off_t,
        ) -> ssize_t {
            interposed!(
                real!($sym, PVecFn),
                {
                    set_errno(libc::ENOSYS);
                    -1
                },
                |f| unsafe { f(fd, iov, iovcnt, offset) },
                ring_iovec(fd, iov, iovcnt, offset, $write)
            )
        }
    };
}

pvec_like!(preadv, "preadv", false);
pvec_like!(preadv64, "preadv64", false);
pvec_like!(pwritev, "pwritev", true);
pvec_like!(pwritev64, "pwritev64", true);

/// §5.1 RWF-flagged vector rows: the flag screen (`rwf_passthrough`)
/// gates ring eligibility; `offset == -1` is the offsetful shape.
macro_rules! pvec2_like {
    ($name:ident, $sym:literal, $write:expr) => {
        /// # Safety
        /// C ABI interposer; argument contracts are libc's own.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            fd: c_int,
            iov: *const libc::iovec,
            iovcnt: c_int,
            offset: off_t,
            flags: c_int,
        ) -> ssize_t {
            interposed!(
                real!($sym, PVec2Fn),
                {
                    set_errno(libc::ENOSYS);
                    -1
                },
                |f| unsafe { f(fd, iov, iovcnt, offset, flags) },
                if rwf_passthrough(flags) {
                    if offset < 0 {
                        // The real vector call consumes kernel f_pos:
                        // an armed mirror must flush + demote first
                        // (PERF-7 demote law; the binding itself stays —
                        // only offset authority returns to the kernel).
                        demote_and_flush(fd);
                    }
                    None
                } else {
                    ring_iovec(fd, iov, iovcnt, offset, $write)
                }
            )
        }
    };
}

pvec2_like!(preadv2, "preadv2", false);
pvec2_like!(preadv64v2, "preadv64v2", false);
pvec2_like!(pwritev2, "pwritev2", true);
pvec2_like!(pwritev64v2, "pwritev64v2", true);

// ---------------------------------------------------------------------------
// lseek / lseek64 (PERF-7): the mirror's query/steer surface
// ---------------------------------------------------------------------------

type LseekFn = unsafe extern "C" fn(c_int, off_t, c_int) -> off_t;

/// Served-`lseek` arm (guard held): `Some(result)` = answered from the
/// armed mirror (or a kernel-identical errno); `None` = the real call
/// (unbound, unarmed, or a poisoned lock — fallback-is-correctness).
fn lseek_served(fd: c_int, offset: off_t, whence: c_int) -> Option<off_t> {
    let (_b, m) = table().lookup_with_mirror(fd)?;
    let _l = m.lock_offsets()?;
    if !m.armed() {
        return None; // kernel-authoritative (resync mode): real call
    }
    match whence {
        libc::SEEK_SET | libc::SEEK_CUR => {
            // Pure arithmetic over the mirror — zero syscalls (the
            // ftell/`lseek(fd, 0, SEEK_CUR)` idiom lands here).
            match crate::fd_table::mirror_seek(m.load(), offset, whence) {
                Ok(new) => {
                    m.store(new);
                    Some(new as off_t)
                }
                Err(e) => {
                    set_errno(e);
                    Some(-1)
                }
            }
        }
        _ => {
            // SEEK_END / SEEK_DATA / SEEK_HOLE / unknown: the kernel
            // owns the size/extent authority — the SAME source the
            // pre-PERF-7 path trusted, so the POSIX-8 i_size-staleness
            // scope is unchanged (not widened). None of these read
            // f_pos (they compute from arguments/i_size), so the stale
            // kernel offset cannot skew the result; on success the
            // kernel f_pos now equals the result and the mirror
            // re-seeds to the same value — both sides coherent, the
            // arm survives. Unknown whence: kernel EINVAL, verbatim.
            let r = raw_lseek(fd, offset, whence);
            if r >= 0 {
                m.store(r as u64);
            }
            Some(r as off_t)
        }
    }
}

macro_rules! lseek_like {
    ($name:ident, $sym:literal) => {
        /// PERF-7 offset row (both LFS spellings — the standing LFS-64
        /// alias discipline; glibc has no further `llseek` alias in the
        /// families this table covers on 64-bit targets).
        ///
        /// # Safety
        /// C ABI interposer; argument contracts are libc's own.
        #[no_mangle]
        pub unsafe extern "C" fn $name(fd: c_int, offset: off_t, whence: c_int) -> off_t {
            interposed!(
                real!($sym, LseekFn),
                {
                    // No RTLD_NEXT symbol: synthesize via the raw
                    // syscall (kernel answer, errno set by the wrapper).
                    raw_lseek(fd, offset, whence) as off_t
                },
                |f| unsafe { f(fd, offset, whence) },
                lseek_served(fd, offset, whence)
            )
        }
    };
}

lseek_like!(lseek, "lseek");
lseek_like!(lseek64, "lseek64");

// ---------------------------------------------------------------------------
// f_pos-consuming passthrough ops (PERF-7 demote triggers)
// ---------------------------------------------------------------------------
//
// `sendfile`/`copy_file_range`/`splice` stay kernel-served (§5.1
// "not intercepted") and consume a description's f_pos whenever the
// respective offset pointer is NULL. Pre-PERF-7 that was coherent for
// free (kernel authority); with an armed mirror it must demote first —
// flush + disarm, then the real call proceeds against a current
// kernel offset, and that fd prices at the resync discipline
// thereafter. Non-NULL offset pointers never touch f_pos: no demote.

macro_rules! fpos_consumer {
    ($name:ident, $sym:literal, $sysno:expr,
     ($($arg:ident: $ty:ty),*), |$a:ident| $demote:block) => {
        /// # Safety
        /// C ABI interposer; argument contracts are libc's own.
        #[no_mangle]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> ssize_t {
            type F = unsafe extern "C" fn($($ty),*) -> ssize_t;
            let real = real!($sym, F);
            if let Some(_g) = Guard::enter() {
                let $a = ($($arg,)*);
                if catch_unwind(AssertUnwindSafe(|| $demote)).is_err() {
                    panic_poison();
                }
            }
            match real {
                // SAFETY: chaining the real call verbatim.
                Some(f) => unsafe { f($($arg),*) },
                // SAFETY: raw syscall fallback, same argument contract.
                None => unsafe { libc::syscall($sysno, $($arg),*) as ssize_t },
            }
        }
    };
}

fpos_consumer!(sendfile, "sendfile", libc::SYS_sendfile,
(out_fd: c_int, in_fd: c_int, offset: *mut off_t, count: size_t),
|a| {
    let (out_fd, in_fd, offset, _count) = a;
    // out_fd's f_pos is ALWAYS consumed (sendfile has no out
    // offset); in_fd's only when `offset` is NULL.
    demote_and_flush(out_fd);
    if offset.is_null() {
        demote_and_flush(in_fd);
    }
});
fpos_consumer!(sendfile64, "sendfile64", libc::SYS_sendfile,
(out_fd: c_int, in_fd: c_int, offset: *mut off_t, count: size_t),
|a| {
    let (out_fd, in_fd, offset, _count) = a;
    demote_and_flush(out_fd);
    if offset.is_null() {
        demote_and_flush(in_fd);
    }
});
fpos_consumer!(copy_file_range, "copy_file_range", libc::SYS_copy_file_range,
(fd_in: c_int, off_in: *mut libc::off64_t, fd_out: c_int,
 off_out: *mut libc::off64_t, len: size_t, flags: c_uint),
|a| {
    let (fd_in, off_in, fd_out, off_out, _len, _flags) = a;
    if off_in.is_null() {
        demote_and_flush(fd_in);
    }
    if off_out.is_null() {
        demote_and_flush(fd_out);
    }
});
fpos_consumer!(splice, "splice", libc::SYS_splice,
(fd_in: c_int, off_in: *mut libc::off64_t, fd_out: c_int,
 off_out: *mut libc::off64_t, len: size_t, flags: c_uint),
|a| {
    let (fd_in, off_in, fd_out, off_out, _len, _flags) = a;
    // Only the non-pipe side can be bound; a pipe fd never
    // resolves in the table, so the demote is a no-op there.
    if off_in.is_null() {
        demote_and_flush(fd_in);
    }
    if off_out.is_null() {
        demote_and_flush(fd_out);
    }
});

// ---------------------------------------------------------------------------
// posix_spawn (PERF-7): the fork flush for the no-atfork spawn path
// ---------------------------------------------------------------------------
//
// glibc's posix_spawn clones without running pthread_atfork handlers,
// so the prepare-side mirror flush never fires — yet the child inherits
// every live description. Same law as fork: bump + flush BEFORE the
// spawn, so the child starts on true kernel offsets and the parent's
// bindings demote to the resync discipline. (glibc-internal spawners —
// `system`/`popen` on spawn-based ports, the `execl*` variadic family —
// bypass libc interposition entirely and stay in the documented
// out-of-model residual class; fork-based ports run the atfork flush.)

macro_rules! spawn_like {
    ($name:ident, $sym:literal) => {
        /// # Safety
        /// C ABI interposer; argument contracts are libc's own.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            pid: *mut libc::pid_t,
            path: *const c_char,
            file_actions: *const c_void,
            attrp: *const c_void,
            argv: *const *mut c_char,
            envp: *const *mut c_char,
        ) -> c_int {
            type F = unsafe extern "C" fn(
                *mut libc::pid_t,
                *const c_char,
                *const c_void,
                *const c_void,
                *const *mut c_char,
                *const *mut c_char,
            ) -> c_int;
            let Some(f) = real!($sym, F) else {
                return libc::ENOSYS; // posix_spawn returns errno directly
            };
            if let Some(_g) = Guard::enter() {
                if catch_unwind(AssertUnwindSafe(fork_flush_all)).is_err() {
                    panic_poison();
                }
            }
            // SAFETY: chaining the real spawn verbatim.
            unsafe { f(pid, path, file_actions, attrp, argv, envp) }
        }
    };
}

spawn_like!(posix_spawn, "posix_spawn");
spawn_like!(posix_spawnp, "posix_spawnp");

// ---------------------------------------------------------------------------
// detection / lifecycle symbols
// ---------------------------------------------------------------------------

type OpenFn = unsafe extern "C" fn(*const c_char, c_int, mode_t) -> c_int;
type OpenatFn = unsafe extern "C" fn(c_int, *const c_char, c_int, mode_t) -> c_int;
type CreatFn = unsafe extern "C" fn(*const c_char, mode_t) -> c_int;

fn atfork_init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        extern "C" fn prepare() {
            // PERF-7 fork flush — runs in the PARENT, pre-fork: bump
            // the fork epoch and write every armed mirror through to
            // kernel f_pos, so the child (poisoned → passthrough) and
            // the demoted parent (lseek-resync mode) both resume on
            // one authoritative kernel offset. AS-safe: table atomics,
            // bounded try_lock spins, raw SYS_lseek — no dlsym, no
            // parking (fd_table.rs `fork_demote_flush` docs).
            fork_flush_all();
        }
        extern "C" fn child() {
            // AS-safe by contract (§5.4.1 fork row) — the CHILD variant:
            // close(2) of the inherited copies, never shutdown (see
            // poison_all_in_child). Mirrors were already demoted by the
            // inherited (pre-fork) prepare flush.
            registry().poison_all_in_child();
        }
        // SAFETY: registering AS-safe prepare/child handlers.
        unsafe { libc::pthread_atfork(Some(prepare), None, Some(child)) };
    });
}

/// The fork-epoch snapshot for a bind-to-be (PERF-7): taken BEFORE the
/// real `open` syscall so a fork landing anywhere between snapshot and
/// bind stales it (the binding then never arms). Insurance-wrapped —
/// a first-touch table-init panic must not escape an `extern "C"`
/// frame; `u64::MAX` never matches a live epoch, so failure = unarmed.
fn epoch_snapshot() -> u64 {
    catch_unwind(|| table().fork_epoch()).unwrap_or(u64::MAX)
}

/// Shared open-family tail: real call already made; classify + maybe
/// bind the fresh fd (guard held by the macro body). `epoch` per
/// [`epoch_snapshot`].
fn after_open(fd: c_int, epoch: u64) -> Option<c_int> {
    atfork_init();
    classify_and_bind(fd, epoch);
    Some(fd)
}

macro_rules! open_like {
    ($name:ident, $sym:literal) => {
        /// §5.1 detection row. Declared with an explicit `mode` third
        /// argument (the C variadic): reading a register that was not
        /// set is harmless on every Linux ABI this repo targets, and the
        /// value is forwarded verbatim either way — the standard
        /// LD_PRELOAD idiom (Rust stable has no C-variadic defs).
        ///
        /// # Safety
        /// C ABI interposer; argument contracts are libc's own.
        #[no_mangle]
        pub unsafe extern "C" fn $name(path: *const c_char, oflag: c_int, mode: mode_t) -> c_int {
            let Some(f) = real!($sym, OpenFn) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // PERF-7: the fork-epoch snapshot precedes the fd's birth.
            let ep = epoch_snapshot();
            // SAFETY: chaining the real open.
            let fd = unsafe { f(path, oflag, mode) };
            let Some(_g) = Guard::enter() else { return fd };
            match catch_unwind(AssertUnwindSafe(|| after_open(fd, ep))) {
                Ok(_) => fd,
                Err(_) => {
                    panic_poison();
                    fd
                }
            }
        }
    };
}

open_like!(open, "open");
open_like!(open64, "open64");

// LFS-64 discipline (found by the first sudo gate run): glibc ≥ 2.28
// compiles `_FILE_OFFSET_BITS=64` callers against the 64-suffixed
// symbols (`openat64`, `creat64`, `fcntl64`, `preadv64`, …) — an
// interposer table missing them silently loses those events (CPython's
// `os.dup` was the tell: its `fcntl64(F_DUPFD_CLOEXEC)` made a dup
// invisible, so closing the original unbound the survivor). Every
// interposed family therefore covers both spellings.

macro_rules! openat_like {
    ($name:ident, $sym:literal) => {
        /// # Safety
        /// C ABI interposer; argument contracts are libc's own.
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            dirfd: c_int,
            path: *const c_char,
            oflag: c_int,
            mode: mode_t,
        ) -> c_int {
            let Some(f) = real!($sym, OpenatFn) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // PERF-7: the fork-epoch snapshot precedes the fd's birth.
            let ep = epoch_snapshot();
            // SAFETY: chaining the real openat.
            let fd = unsafe { f(dirfd, path, oflag, mode) };
            let Some(_g) = Guard::enter() else { return fd };
            match catch_unwind(AssertUnwindSafe(|| after_open(fd, ep))) {
                Ok(_) => fd,
                Err(_) => {
                    panic_poison();
                    fd
                }
            }
        }
    };
}

openat_like!(openat, "openat");
openat_like!(openat64, "openat64");

macro_rules! creat_like {
    ($name:ident, $sym:literal) => {
        /// # Safety
        /// C ABI interposer; argument contracts are libc's own.
        #[no_mangle]
        pub unsafe extern "C" fn $name(path: *const c_char, mode: mode_t) -> c_int {
            let Some(f) = real!($sym, CreatFn) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // PERF-7: the fork-epoch snapshot precedes the fd's birth.
            let ep = epoch_snapshot();
            // SAFETY: chaining the real creat.
            let fd = unsafe { f(path, mode) };
            let Some(_g) = Guard::enter() else { return fd };
            match catch_unwind(AssertUnwindSafe(|| after_open(fd, ep))) {
                Ok(_) => fd,
                Err(_) => {
                    panic_poison();
                    fd
                }
            }
        }
    };
}

creat_like!(creat, "creat");
creat_like!(creat64, "creat64");

type CloseFn = unsafe extern "C" fn(c_int) -> c_int;
type CloseRangeFn = unsafe extern "C" fn(c_uint, c_uint, c_int) -> c_int;
type DupFn = unsafe extern "C" fn(c_int) -> c_int;
type Dup2Fn = unsafe extern "C" fn(c_int, c_int) -> c_int;
type Dup3Fn = unsafe extern "C" fn(c_int, c_int, c_int) -> c_int;

/// §5.1 close row: bookkeeping FIRST (an fd number can be reused the
/// instant the real close returns), then the real call always executes
/// (kernel FLUSH/RELEASE unchanged).
///
/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    let Some(f) = real!("close", CloseFn) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    if let Some(_g) = Guard::enter() {
        if catch_unwind(AssertUnwindSafe(|| route_release(table().on_close(fd)))).is_err() {
            panic_poison();
        }
    }
    // SAFETY: the real close always executes.
    unsafe { f(fd) }
}

/// §5.1 `close_range` row (glibc ≥ 2.34; the systemd/container idiom).
///
/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn close_range(first: c_uint, last: c_uint, flags: c_int) -> c_int {
    let real = real!("close_range", CloseRangeFn);
    if let Some(_g) = Guard::enter() {
        // CLOSE_RANGE_CLOEXEC only marks fds; it closes nothing.
        let marks_only = flags as c_uint & libc::CLOSE_RANGE_CLOEXEC != 0;
        if !marks_only
            && catch_unwind(AssertUnwindSafe(|| {
                let mut released = Vec::new();
                let last_fd = last.min(i32::MAX as c_uint) as i32;
                table().on_close_range(first as i32, last_fd, &mut released);
                for b in released {
                    route_release(Some(b));
                }
            }))
            .is_err()
        {
            panic_poison();
        }
    }
    match real {
        // SAFETY: chaining the real close_range.
        Some(f) => unsafe { f(first, last, flags) },
        // SAFETY: raw syscall fallback (older glibc without the wrapper).
        None => unsafe { libc::syscall(libc::SYS_close_range, first, last, flags) as c_int },
    }
}

fn after_dup(oldfd: c_int, newfd: c_int) {
    if newfd >= 0 {
        route_release(table().on_dup(oldfd, newfd));
    }
}

/// §5.1 dup rows: real call first, then propagate the binding
/// (refcounted — Issue-14).
///
/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn dup(oldfd: c_int) -> c_int {
    let Some(f) = real!("dup", DupFn) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: chaining the real dup.
    let newfd = unsafe { f(oldfd) };
    if let Some(_g) = Guard::enter() {
        if catch_unwind(AssertUnwindSafe(|| after_dup(oldfd, newfd))).is_err() {
            panic_poison();
        }
    }
    newfd
}

/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn dup2(oldfd: c_int, newfd: c_int) -> c_int {
    let Some(f) = real!("dup2", Dup2Fn) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: chaining the real dup2.
    let r = unsafe { f(oldfd, newfd) };
    if r >= 0 {
        if let Some(_g) = Guard::enter() {
            if catch_unwind(AssertUnwindSafe(|| after_dup(oldfd, r))).is_err() {
                panic_poison();
            }
        }
    }
    r
}

/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn dup3(oldfd: c_int, newfd: c_int, flags: c_int) -> c_int {
    let Some(f) = real!("dup3", Dup3Fn) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: chaining the real dup3.
    let r = unsafe { f(oldfd, newfd, flags) };
    if r >= 0 {
        if let Some(_g) = Guard::enter() {
            if catch_unwind(AssertUnwindSafe(|| after_dup(oldfd, r))).is_err() {
                panic_poison();
            }
        }
    }
    r
}

type FcntlFn = unsafe extern "C" fn(c_int, c_int, *mut c_void) -> c_int;

/// §5.1 fcntl row: F_DUPFD* propagate; F_SETFL adding O_APPEND and every
/// F_SETLK-class command unbind (lock users deserve the single-transport
/// shape — DAOS-parity conservatism). Declared with a pointer third
/// argument (the variadic slot), forwarded verbatim.
///
/// Interposed under BOTH names, `fcntl` and `fcntl64`: glibc ≥ 2.28
/// compiles `_FILE_OFFSET_BITS=64` callers (CPython's `os.dup` among
/// them — the first sudo gate caught its `F_DUPFD_CLOEXEC` slipping
/// past an fcntl-only shim as an invisible dup) against the 64 symbol.
///
/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn fcntl(fd: c_int, cmd: c_int, arg: *mut c_void) -> c_int {
    let real = real!("fcntl", FcntlFn);
    // SAFETY: same contract, shared body.
    unsafe { fcntl_body(real, fd, cmd, arg) }
}

/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn fcntl64(fd: c_int, cmd: c_int, arg: *mut c_void) -> c_int {
    let real = real!("fcntl64", FcntlFn);
    // SAFETY: same contract, shared body.
    unsafe { fcntl_body(real, fd, cmd, arg) }
}

/// # Safety
/// Caller is one of the fcntl interposers; contracts are libc's own.
unsafe fn fcntl_body(real: Option<FcntlFn>, fd: c_int, cmd: c_int, arg: *mut c_void) -> c_int {
    let Some(f) = real else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    if let Some(_g) = Guard::enter() {
        let pre = catch_unwind(AssertUnwindSafe(|| {
            match cmd {
                libc::F_SETLK | libc::F_SETLKW | libc::F_OFD_SETLK | libc::F_OFD_SETLKW => {
                    // Unbind BEFORE the real call: the lock must be taken
                    // with the kernel as the only data transport. The fd
                    // lives on kernel-served — flush any armed mirror to
                    // kernel f_pos first (PERF-7 demote law).
                    demote_and_flush(fd);
                    route_release(table().on_close(fd));
                }
                libc::F_SETFL => {
                    let new_flags = arg as usize as c_int;
                    if new_flags & libc::O_APPEND != 0 {
                        demote_and_flush(fd);
                        route_release(table().on_close(fd));
                    } else if table().lookup(fd).is_some() {
                        // O_DIRECT toggle = read-CLASS change (the daemon
                        // captured the description's class at bind —
                        // BindingRights::odirect; the kernel path reads
                        // the live description on every request). A
                        // binding whose class diverged would serve the
                        // wrong admission semantics: unbind, kernel
                        // takes the fd with the correct live class.
                        // (F_GETFL only for BOUND fds — control-plane,
                        // one extra syscall.)
                        let cur = unsafe { f(fd, libc::F_GETFL, std::ptr::null_mut()) };
                        if cur >= 0 && (cur ^ new_flags) & libc::O_DIRECT != 0 {
                            demote_and_flush(fd);
                            route_release(table().on_close(fd));
                        }
                    }
                }
                _ => {}
            }
        }));
        if pre.is_err() {
            panic_poison();
        }
        // SAFETY: chaining the real fcntl.
        let r = unsafe { f(fd, cmd, arg) };
        if r >= 0
            && (cmd == libc::F_DUPFD || cmd == libc::F_DUPFD_CLOEXEC)
            && catch_unwind(AssertUnwindSafe(|| after_dup(fd, r))).is_err()
        {
            panic_poison();
        }
        return r;
    }
    // SAFETY: chaining the real fcntl (reentrant path).
    unsafe { f(fd, cmd, arg) }
}

type MmapFn = unsafe extern "C" fn(*mut c_void, size_t, c_int, c_int, c_int, off_t) -> *mut c_void;

/// §5.4.1 mmap bail-out (Issue-22 / §5.6.2 W3(a)): unbind ALL in-process
/// bindings on the mapped inode FIRST, then the real mmap proceeds —
/// the kernel page cache becomes that file's authority.
///
/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn mmap(
    addr: *mut c_void,
    length: size_t,
    prot: c_int,
    flags: c_int,
    fd: c_int,
    offset: off_t,
) -> *mut c_void {
    let Some(f) = real!("mmap", MmapFn) else {
        set_errno(libc::ENOSYS);
        return libc::MAP_FAILED;
    };
    if fd >= 0 {
        if let Some(_g) = Guard::enter() {
            let walk = catch_unwind(AssertUnwindSafe(|| {
                if let Some(b) = table().lookup(fd) {
                    let mut released = Vec::new();
                    let mut flushes = Vec::new();
                    table().unbind_ino(b.ino, &mut released, &mut flushes);
                    // PERF-7: the unbound siblings live on kernel-served —
                    // restore kernel f_pos from each armed mirror before
                    // any real call consumes it (once per cell).
                    for (ffd, off) in flushes {
                        raw_lseek(ffd, off as i64, libc::SEEK_SET);
                    }
                    for rb in released {
                        route_release(Some(rb));
                    }
                }
            }));
            if walk.is_err() {
                panic_poison();
            }
        }
    }
    // SAFETY: chaining the real mmap.
    unsafe { f(addr, length, prot, flags, fd, offset) }
}

/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn mmap64(
    addr: *mut c_void,
    length: size_t,
    prot: c_int,
    flags: c_int,
    fd: c_int,
    offset: off_t,
) -> *mut c_void {
    // SAFETY: identical contract; on this target off64_t == off_t.
    unsafe { mmap(addr, length, prot, flags, fd, offset) }
}

// ---------------------------------------------------------------------------
// fd-creator hygiene sweep (§5.1 creator rows, Issue-21)
// ---------------------------------------------------------------------------

fn sweep(fd: c_int) {
    if fd >= 0 {
        if let Some(_g) = Guard::enter() {
            if catch_unwind(AssertUnwindSafe(|| route_release(table().sweep_stale(fd)))).is_err() {
                panic_poison();
            }
        }
    }
}

fn sweep_pair(fds: *const c_int) {
    if !fds.is_null() {
        // SAFETY: the creator filled this 2-fd array (its own contract).
        unsafe {
            sweep(*fds);
            sweep(*fds.add(1));
        }
    }
}

macro_rules! creator1 {
    ($name:ident, $sym:literal, ($($arg:ident: $ty:ty),*)) => {
        /// §5.1 fd-creator hygiene row: real call, then release any
        /// stale entry on the returned number exactly as `close` would.
        ///
        /// # Safety
        /// C ABI interposer; argument contracts are libc's own.
        #[no_mangle]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> c_int {
            type F = unsafe extern "C" fn($($ty),*) -> c_int;
            let Some(f) = real!($sym, F) else {
                set_errno(libc::ENOSYS);
                return -1;
            };
            // SAFETY: chaining the real creator.
            let fd = unsafe { f($($arg),*) };
            sweep(fd);
            fd
        }
    };
}

creator1!(socket, "socket", (domain: c_int, ty: c_int, protocol: c_int));
creator1!(eventfd, "eventfd", (initval: c_uint, flags: c_int));
creator1!(epoll_create, "epoll_create", (size: c_int));
creator1!(epoll_create1, "epoll_create1", (flags: c_int));
creator1!(timerfd_create, "timerfd_create", (clockid: c_int, flags: c_int));
creator1!(inotify_init, "inotify_init", ());
creator1!(inotify_init1, "inotify_init1", (flags: c_int));
creator1!(memfd_create, "memfd_create", (name: *const c_char, flags: c_uint));
creator1!(accept, "accept", (fd: c_int, addr: *mut libc::sockaddr, len: *mut libc::socklen_t));
creator1!(accept4, "accept4", (fd: c_int, addr: *mut libc::sockaddr, len: *mut libc::socklen_t, flags: c_int));
creator1!(signalfd, "signalfd", (fd: c_int, mask: *const libc::sigset_t, flags: c_int));

type PipeFn = unsafe extern "C" fn(*mut c_int) -> c_int;
type Pipe2Fn = unsafe extern "C" fn(*mut c_int, c_int) -> c_int;
type SocketpairFn = unsafe extern "C" fn(c_int, c_int, c_int, *mut c_int) -> c_int;

/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn pipe(fds: *mut c_int) -> c_int {
    let Some(f) = real!("pipe", PipeFn) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: chaining the real pipe.
    let r = unsafe { f(fds) };
    if r == 0 {
        sweep_pair(fds);
    }
    r
}

/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn pipe2(fds: *mut c_int, flags: c_int) -> c_int {
    let Some(f) = real!("pipe2", Pipe2Fn) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: chaining the real pipe2.
    let r = unsafe { f(fds, flags) };
    if r == 0 {
        sweep_pair(fds);
    }
    r
}

/// # Safety
/// C ABI interposer; argument contracts are libc's own.
#[no_mangle]
pub unsafe extern "C" fn socketpair(
    domain: c_int,
    ty: c_int,
    protocol: c_int,
    fds: *mut c_int,
) -> c_int {
    let Some(f) = real!("socketpair", SocketpairFn) else {
        set_errno(libc::ENOSYS);
        return -1;
    };
    // SAFETY: chaining the real socketpair.
    let r = unsafe { f(domain, ty, protocol, fds) };
    if r == 0 {
        sweep_pair(fds);
    }
    r
}

// ---------------------------------------------------------------------------
// libaio (v1.1 OQ-1): io_setup / io_submit / io_getevents / io_destroy
// ---------------------------------------------------------------------------
//
// These interpose `libaio.so.1`'s wrappers (userspace ABI — the
// `aio_glue::RawIocb`/`RawIoEvent` mirrors), NOT raw syscalls. Return
// convention is libaio's: 0-or-count on success, NEGATIVE errno on
// failure (never -1/errno).
//
// Lane split per iocb (`aio_glue::screen_iocb` + the hermetic
// `aio_core::AioCtxState` mixed-batch machine): ring-eligible PREAD/
// PWRITE on bound fds ride session slots as no-wait tickets; everything
// else — and every unregistered context — is literally the real call
// (§5.4.2 fallback-is-correctness).
//
// Locking: ONE Mutex per registered ctx (state + ticket table
// co-located). The lock is never held across a wait when ring ops are
// pending — those passes are NON-BLOCKING (harvest + kernel probes) and
// the reap parks OUTSIDE the lock on the pending slots' futex words
// (event-driven; 2026-07-26 reap economy). Only the kernel-lane-only
// shape waits inside a pass (bounded ~50 ms holds, as before), so a
// split submitter/reaper pair (a legal libaio shape) degrades to 50 ms
// granularity there and to the park bound elsewhere — never a deadlock.
//
// Poison law: a poisoned session resolves its in-flight tickets as
// `-EIO` events (never silently stranded, never a hang — the app sees
// the failed I/O exactly as if the device errored). Poisoned/full
// paths never re-enter interception: fresh submits classify Kernel
// because `Session::submit_op` refuses when poisoned.

use crate::aio_core::{AioCtxState, AioEvent, IocbClass, KernelLane, RingLane, RingToken};
use crate::aio_glue::{screen_iocb, AioCtxRegistry, RawIoEvent, RawIocb, IOCB_CMD_PREAD};
use crate::session::OpTicket;

type IoSetupFn = unsafe extern "C" fn(c_int, *mut u64) -> c_int;
type IoDestroyFn = unsafe extern "C" fn(u64) -> c_int;
type IoSubmitFn = unsafe extern "C" fn(u64, c_long, *mut *mut RawIocb) -> c_int;
type IoGetEventsFn =
    unsafe extern "C" fn(u64, c_long, c_long, *mut RawIoEvent, *mut libc::timespec) -> c_int;
type IoCancelFn = unsafe extern "C" fn(u64, *mut RawIocb, *mut RawIoEvent) -> c_int;

/// One in-flight ring-lane iocb: everything `poll` needs without
/// re-touching the fd table (the fd may close while the op flies —
/// libaio completes it anyway, and so do we).
struct LiveTicket {
    session: usize,
    ticket: OpTicket,
    is_read: bool,
    buf: u64,
    len: u32,
}

/// Per-ctx payload: merge state + the ticket table its RingTokens
/// index. One lock covers both (the registry's `Mutex<AioCtx>`).
#[derive(Default)]
struct AioCtx {
    state: AioCtxState,
    tickets: Vec<Option<LiveTicket>>,
}

fn aio_registry() -> &'static AioCtxRegistry<AioCtx> {
    static REG: OnceLock<AioCtxRegistry<AioCtx>> = OnceLock::new();
    REG.get_or_init(AioCtxRegistry::new)
}

/// The ring lane over session no-wait tickets. `iocb_id` IS the iocb
/// pointer (the app owns it until the completion event, per libaio).
struct SessionRing<'a> {
    tickets: &'a mut Vec<Option<LiveTicket>>,
}

impl RingLane for SessionRing<'_> {
    fn try_submit(&mut self, iocb_id: u64) -> Option<RingToken> {
        // SAFETY: iocb_id is the app's live iocb pointer for the
        // duration of io_submit (libaio contract).
        let io = unsafe { &*(iocb_id as *const RawIocb) };
        let b = table().lookup(io.aio_fildes)?;
        let session = registry().by_token(b.session)?;
        let is_read = io.aio_lio_opcode == IOCB_CMD_PREAD;
        let ticket = if is_read {
            session.submit_pread_nowait(b.binding_id, io.nbytes as usize, io.offset as u64)?
        } else {
            // SAFETY: PWRITE buf/nbytes are the app's contract with
            // io_submit; the payload is copied to the slab NOW, so the
            // app's buffer is free the moment io_submit returns.
            let data =
                unsafe { std::slice::from_raw_parts(io.buf as *const u8, io.nbytes as usize) };
            session.submit_pwrite_nowait(b.binding_id, data, io.offset as u64)?
        };
        let live = LiveTicket {
            session: b.session,
            ticket,
            is_read,
            buf: io.buf,
            len: io.nbytes as u32,
        };
        let idx = match self.tickets.iter().position(Option::is_none) {
            Some(i) => {
                self.tickets[i] = Some(live);
                i
            }
            None => {
                self.tickets.push(Some(live));
                self.tickets.len() - 1
            }
        };
        Some(RingToken(idx as u64))
    }

    fn poll(&mut self, tok: RingToken) -> Option<i64> {
        let slot = self.tickets.get_mut(tok.0 as usize)?;
        let t = slot.as_ref()?;
        let session = registry().by_token(t.session)?;
        let res = if t.is_read {
            // SAFETY: the app owns buf until the completion event is
            // delivered (libaio contract); len was screened ≤ slab.
            let out = unsafe { std::slice::from_raw_parts_mut(t.buf as *mut u8, t.len as usize) };
            session.poll_ticket(t.ticket, Some(out))
        } else {
            session.poll_ticket(t.ticket, None)
        };
        let res = match res {
            Some(r) => r,
            None if session.poisoned() => {
                // §5.4.1 poison law, async shape: the slot may complete
                // arbitrarily late — never recycle it (GC'd with the
                // session); the op resolves as a failed I/O.
                -(libc::EIO as i64)
            }
            None => return None,
        };
        *slot = None;
        Some(res)
    }

    fn abandon(&mut self, tok: RingToken) {
        // Destroy path: drop the bookkeeping, never release the slot —
        // it is GC'd with the session (§5.7).
        if let Some(slot) = self.tickets.get_mut(tok.0 as usize) {
            *slot = None;
        }
    }
}

/// The kernel lane: literally the real libaio on the real ctx.
struct RealKernel {
    ctx: u64,
}

impl KernelLane for RealKernel {
    fn submit_run(&mut self, iocb_ids: &[u64]) -> isize {
        let Some(f) = real_aio!("io_submit", IoSubmitFn) else {
            return -(libc::ENOSYS as isize);
        };
        // SAFETY: a &[u64] of iocb pointers is layout-identical to the
        // `struct iocb *ios[]` array io_submit takes; the real call
        // does not mutate the array itself.
        let r = unsafe {
            f(
                self.ctx,
                iocb_ids.len() as c_long,
                iocb_ids.as_ptr() as *mut *mut RawIocb,
            )
        };
        r as isize
    }

    fn getevents(&mut self, min: usize, max: usize, timeout_ms: Option<u64>) -> Vec<AioEvent> {
        let Some(f) = real_aio!("io_getevents", IoGetEventsFn) else {
            return Vec::new();
        };
        let mut raw: Vec<RawIoEvent> = vec![
            RawIoEvent {
                data: 0,
                obj: 0,
                res: 0,
                res2: 0,
            };
            max
        ];
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let tsp: *mut libc::timespec = match timeout_ms {
            Some(ms) => {
                ts.tv_sec = (ms / 1000) as libc::time_t;
                ts.tv_nsec = ((ms % 1000) * 1_000_000) as libc::c_long;
                &mut ts
            }
            None => std::ptr::null_mut(),
        };
        // SAFETY: chaining the real io_getevents with our sized buffer.
        let n = unsafe {
            f(
                self.ctx,
                min as c_long,
                max as c_long,
                raw.as_mut_ptr(),
                tsp,
            )
        };
        if n <= 0 {
            // Errors (EINTR included) surface as an empty harvest; the
            // caller's pass loop re-drives or returns partial — libaio
            // itself allows returning fewer than min_nr on interrupt.
            return Vec::new();
        }
        raw[..n as usize]
            .iter()
            .map(|e| AioEvent {
                iocb_id: e.obj,
                data: e.data,
                res: e.res,
                res2: e.res2,
            })
            .collect()
    }
}

/// Retro-neutralize one ctx (panic paths): vacate the registry entry so
/// every later call on the value is the verbatim real call — a
/// half-mutated merge state must never keep owning a live context.
/// Registry ops are libc-free atomics, safe without the TLS guard.
fn aio_retro_neutralize(ctx: u64) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        aio_registry().remove(ctx);
    }));
}

/// # Safety
/// C ABI interposer; argument contracts are libaio's own.
#[no_mangle]
pub unsafe extern "C" fn io_setup(maxevents: c_int, ctxp: *mut u64) -> c_int {
    let Some(f) = real_aio!("io_setup", IoSetupFn) else {
        return -libc::ENOSYS;
    };
    // SAFETY: chaining the real io_setup.
    let r = unsafe { f(maxevents, ctxp) };
    if r != 0 || ctxp.is_null() {
        return r;
    }
    let Some(_g) = Guard::enter() else {
        return r;
    };
    if catch_unwind(AssertUnwindSafe(|| {
        // Registration failure (capacity) is fine: the ctx simply
        // passthroughs wholesale. A colliding value retro-neutralizes
        // inside register() — the kernel recycled a ring address whose
        // destroy bookkeeping was missed; the corpse must not own it.
        // SAFETY: the real call just wrote *ctxp.
        aio_registry().register(unsafe { *ctxp });
    }))
    .is_err()
    {
        panic_poison();
        // A panic mid-register must leave the value with a definite
        // owner: the real kernel (passthrough wholesale).
        // SAFETY: the real call wrote *ctxp above (r == 0 checked).
        aio_retro_neutralize(unsafe { *ctxp });
    }
    r
}

/// # Safety
/// C ABI interposer; argument contracts are libaio's own.
#[no_mangle]
pub unsafe extern "C" fn io_destroy(ctx: u64) -> c_int {
    // Bookkeeping FIRST and unconditionally: it is libc-free (registry
    // atomics + one process-local mutex), so it runs even when the TLS
    // guard is unavailable (reentrant entry — e.g. a signal handler
    // destroying a context over an interposed frame) and even when the
    // real symbol is missing. A skipped vacate leaves a stale entry
    // that a recycled ctx value would adopt (the 2026-07-25 field
    // crash class). Vacate BEFORE draining: even if the drain panics,
    // the id no longer resolves.
    if catch_unwind(AssertUnwindSafe(|| {
        if let Some(m) = aio_registry().lookup(ctx) {
            aio_registry().remove(ctx);
            let mut c = m.lock().unwrap_or_else(|p| p.into_inner());
            let AioCtx { state, tickets } = &mut *c;
            let mut ring = SessionRing { tickets };
            state.destroy(&mut ring);
        }
    }))
    .is_err()
    {
        panic_poison();
    }
    let Some(f) = real_aio!("io_destroy", IoDestroyFn) else {
        return -libc::ENOSYS;
    };
    // SAFETY: chaining the real io_destroy (kernel-lane ops die with
    // the kernel ctx, exactly as un-interposed libaio).
    unsafe { f(ctx) }
}

/// # Safety
/// C ABI interposer; argument contracts are libaio's own.
#[no_mangle]
pub unsafe extern "C" fn io_submit(ctx: u64, nr: c_long, ios: *mut *mut RawIocb) -> c_int {
    let real = real_aio!("io_submit", IoSubmitFn);
    let fallback = |f: Option<IoSubmitFn>| -> c_int {
        match f {
            // SAFETY: chaining the real io_submit verbatim.
            Some(f) => unsafe { f(ctx, nr, ios) },
            None => -libc::ENOSYS,
        }
    };
    let Some(_g) = Guard::enter() else {
        return fallback(real);
    };
    let served = catch_unwind(AssertUnwindSafe(|| {
        if nr <= 0 || ios.is_null() {
            return None; // real call answers the edge cases
        }
        let m = aio_registry().lookup(ctx)?;
        // SAFETY: ios[0..nr] are the app's iocb pointers (libaio
        // contract for io_submit).
        let ids: Vec<u64> = unsafe { std::slice::from_raw_parts(ios, nr as usize) }
            .iter()
            .map(|p| *p as u64)
            .collect();
        let classes: Vec<IocbClass> = ids
            .iter()
            .map(|&id| {
                if id == 0 {
                    return IocbClass::Kernel; // real call faults it
                }
                // SAFETY: non-null app iocb pointer, live for the call.
                let io = unsafe { &*(id as *const RawIocb) };
                let (rights, slab) = match table().lookup(io.aio_fildes) {
                    Some(b) => {
                        let slab = registry()
                            .by_token(b.session)
                            .map(|s| s.slab_bytes())
                            .unwrap_or(0);
                        (Some((b.read_ok, b.write_ok)), slab)
                    }
                    None => (None, 0),
                };
                screen_iocb(io, rights, slab)
            })
            .collect();
        if !classes.contains(&IocbClass::Ring) {
            return None; // pure-kernel batch: the real call, verbatim
        }
        let mut c = m.lock().unwrap_or_else(|p| p.into_inner());
        let AioCtx { state, tickets } = &mut *c;
        let mut ring = SessionRing { tickets };
        let mut kern = RealKernel { ctx };
        // SAFETY: data_of derefs live iocb pointers from `ids`.
        let out = state.submit_batch(&mut ring, &mut kern, &ids, &classes, &|id| unsafe {
            (*(id as *const RawIocb)).data
        });
        Some(match out {
            crate::aio_core::SubmitOutcome::Submitted(n) => n as c_int,
            crate::aio_core::SubmitOutcome::Errno(e) => -e,
        })
    }));
    match served {
        Ok(Some(r)) => r,
        Ok(None) => fallback(real),
        Err(_) => {
            // A panicked body may have already dispatched part of the
            // batch (kernel runs / ring tickets) — replaying the REAL
            // call would double-submit: duplicate completions corrupt
            // the host app's accounting (the sync interposers' lazy-
            // real lesson, async shape). Retro-neutralize the ctx
            // (every later call is the verbatim real call; tracked
            // ring ops die with the poisoned session) and fail this
            // submit loud with a kernel-legal errno.
            panic_poison();
            aio_retro_neutralize(ctx);
            -libc::EIO
        }
    }
}

/// The served-path reap: bounded merge passes over the ctx lock.
/// `None` = not ours / nothing in flight — the caller's REAL function
/// (io_getevents or io_pgetevents, each with its own exact semantics)
/// answers verbatim.
///
/// # Safety
/// `events` must point to `nr` writable `io_event`s and `timeout`, when
/// non-null, to a live timespec — libaio's own contracts for both
/// symbols.
unsafe fn aio_reap_served(
    ctx: u64,
    min_nr: c_long,
    nr: c_long,
    events: *mut RawIoEvent,
    timeout: *mut libc::timespec,
) -> Option<c_int> {
    {
        if nr <= 0 || events.is_null() {
            return None;
        }
        let m = aio_registry().lookup(ctx)?;
        {
            // Fast reject: nothing this shim tracks is in flight on the
            // ctx ⇒ the real call is exact (most contexts in a mixed
            // process never touch SqueezeFS fds). A destroyed corpse
            // (reachable only through registry staleness — belt-and-
            // braces below the collision retro-neutralization) serves
            // nothing and must forward to the real call too, never
            // merge-loop on empty harvests (a host-app hang).
            let c = m.lock().unwrap_or_else(|p| p.into_inner());
            if c.state.destroyed() || (c.state.ring_pending() == 0 && c.state.kernel_pending() == 0)
            {
                return None;
            }
        }
        // SAFETY: the caller's timeout, read once (libaio contract).
        let deadline: Option<std::time::Instant> = if timeout.is_null() {
            None
        } else {
            let ts = unsafe { &*timeout };
            let d = std::time::Duration::new(ts.tv_sec.max(0) as u64, ts.tv_nsec.max(0) as u32);
            Some(std::time::Instant::now() + d)
        };
        let (min_nr, nr) = (min_nr.max(0) as usize, nr as usize);
        let mut got = 0usize;
        loop {
            // One merge pass per lock hold. Ring-involved passes carry a
            // ZERO budget (non-blocking: harvest + kernel probes only) —
            // the waiting policy lives OUTSIDE the lock as an
            // event-driven ticket park (below), so a split submitter is
            // never blocked behind a sleeping reaper. Only the
            // kernel-lane-only shape waits inside the pass (the real
            // io_getevents wake is already event-driven there), bounded
            // ≤ ~50 ms per hold as before.
            let mut parks: Vec<(usize, OpTicket)> = Vec::new();
            let (evs, ring_pending, kernel_pending) = {
                let mut c = m.lock().unwrap_or_else(|p| p.into_inner());
                let AioCtx { state, tickets } = &mut *c;
                let pass_ms: u64 = if state.ring_pending() == 0 && state.kernel_pending() > 0 {
                    match deadline {
                        None => 50,
                        Some(d) => {
                            let left = d.saturating_duration_since(std::time::Instant::now());
                            (left.as_millis() as u64).min(50)
                        }
                    }
                } else {
                    0
                };
                let mut ring = SessionRing { tickets };
                let mut kern = RealKernel { ctx };
                let evs = state.getevents(
                    &mut ring,
                    &mut kern,
                    min_nr.saturating_sub(got).min(nr - got),
                    nr - got,
                    Some(pass_ms),
                );
                let ring_pending = state.ring_pending();
                if evs.is_empty() && ring_pending > 0 {
                    // Park snapshot: (session token, ticket) per pending
                    // ring op — resolved to wait entries outside the lock.
                    for tok in state.pending_tokens() {
                        if let Some(Some(t)) = tickets.get(tok.0 as usize).map(Option::as_ref) {
                            parks.push((t.session, t.ticket));
                        }
                    }
                }
                (evs, ring_pending, state.kernel_pending())
            };
            for (i, ev) in evs.iter().enumerate() {
                // SAFETY: events[0..nr] is the caller's array (libaio
                // contract); got+i < nr by the pass's max.
                unsafe {
                    *events.add(got + i) = RawIoEvent {
                        data: ev.data,
                        obj: ev.iocb_id,
                        res: ev.res,
                        res2: ev.res2,
                    };
                }
            }
            got += evs.len();
            if got >= min_nr || got >= nr {
                return Some(got as c_int);
            }
            if let Some(d) = deadline {
                if std::time::Instant::now() >= d {
                    return Some(got as c_int);
                }
            }
            if !evs.is_empty() || (ring_pending == 0 && kernel_pending > 0) {
                continue; // progress, or the pass itself carried the wait
            }
            if ring_pending == 0 && kernel_pending == 0 {
                // Nothing tracked in flight with min unmet: only a racing
                // submitter (another thread, legal libaio) can change
                // that — idle-wait politely, nothing to park on.
                std::thread::sleep(std::time::Duration::from_micros(200));
                continue;
            }
            // Ring ops pending, nothing ready. Deep-pending sets batch
            // on a SHORT bounded sleep instead of event-parking (sized
            // 2026-07-26 under the per-ticket WAITER economics; the
            // completion doorbell collapsed those costs — see the
            // REAP_EVENT_PARK_MAX doc — so the threshold is a Phase B
            // re-measure candidate). The quantum is 50 µs, only ever
            // taken with ≥ REAP_EVENT_PARK_MAX ops in flight, so its
            // latency contribution is bounded by depth; the SPARSE
            // regime — where the 200 µs/5 ms quantum actually shaped
            // completion latency and max-latency tails — stays fully
            // event-driven below.
            if parks.len() > reap_event_park_max() {
                let quantum = std::time::Duration::from_micros(50);
                let bound = match deadline {
                    None => quantum,
                    Some(d) => d
                        .saturating_duration_since(std::time::Instant::now())
                        .min(quantum),
                };
                std::thread::sleep(bound);
                continue;
            }
            // Sparse pending set: EVENT-DRIVEN park outside
            // the lock (2026-07-26 reap economy — replaces the 200 µs
            // sleep ladder whose quantum shaped every cold completion).
            // A short spin first covers the just-completing case without
            // a futex round trip; it is deliberately tiny (the CPU-theft
            // lesson: reaper spin starves the daemon at fleet scale) and
            // SWEEP-bounded, not iteration-bounded — 64 sweeps over a
            // qd32 pending set was 2048 cross-cacheline shm probes per
            // empty pass, measurable client CPU theft on the t32qd32
            // shape (2026-07-26 sizing).
            let mut ready = false;
            'spin: for _ in 0..4 {
                for (token, t) in &parks {
                    if registry()
                        .by_token(*token)
                        .is_some_and(|s| s.ticket_done(*t))
                    {
                        ready = true;
                        break 'spin;
                    }
                }
                std::hint::spin_loop();
            }
            if ready {
                continue;
            }
            // Park admission per DISTINCT session (op-economy
            // 2026-07-28; replaces the per-ticket WAITER parks): register
            // on each session's completion doorbell — register-then-
            // snapshot (`cqe_park_begin`) — then RE-SCAN the pending set
            // (the disarm→scan law: a completion that beat the
            // registration is found here, and one that lands after it
            // either fails the wait's admission or pays the wake — the
            // `ipc_cqe_parked_reaper_never_stranded` loom model). One
            // wait word per session (≤ the IL_SESSIONS shard count)
            // instead of a `futex_waitv` array per pending ticket, and
            // the daemon pays completion wakes ONLY while this park is
            // registered. The wait stays bounded: ring-only shapes
            // re-check on a coarse cap (cross-thread submits on OTHER
            // ctxs sharing a session wake us spuriously — bounded,
            // re-scan); both-lanes shapes cap at the kernel-probe slice
            // (kernel completions cannot wake a futex).
            let mut entries: Vec<crate::session::WaitEntry<'static>> = Vec::with_capacity(4);
            let mut parked_tokens: Vec<usize> = Vec::with_capacity(4);
            for (token, _) in parks.iter() {
                if parked_tokens.contains(token) {
                    continue;
                }
                let Some(session) = registry().by_token(*token) else {
                    continue; // session gone: its poll resolves next pass
                };
                entries.push(session.cqe_park_begin());
                parked_tokens.push(*token);
            }
            // The mandatory post-registration re-scan.
            for (token, t) in parks.iter() {
                if registry()
                    .by_token(*token)
                    .is_some_and(|s| s.ticket_done(*t))
                {
                    ready = true;
                    break;
                }
            }
            if ready || entries.is_empty() {
                for token in &parked_tokens {
                    if let Some(s) = registry().by_token(*token) {
                        s.cqe_park_end();
                    }
                }
                if entries.is_empty() && !ready {
                    // No waitable session (all poisoned/gone): the next
                    // pass resolves the tickets as -EIO; don't spin the
                    // lock at full speed while it does.
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
                continue;
            }
            let cap = if kernel_pending > 0 {
                KERNEL_LANE_SLICE
            } else {
                RING_PARK_RECHECK
            };
            let bound = match deadline {
                None => cap,
                Some(d) => d
                    .saturating_duration_since(std::time::Instant::now())
                    .min(cap),
            };
            crate::session::wait_any(&entries, bound);
            for token in &parked_tokens {
                if let Some(s) = registry().by_token(*token) {
                    s.cqe_park_end();
                }
            }
        }
    }
}

/// Both-lanes park cap: kernel-lane completions cannot wake a futex, so
/// the ring park doubles as the kernel probe cadence. 1 ms replaces the
/// pre-2026-07-26 hard-coded 5 ms in-lock kernel slice (the field's
/// completion quantizer); the kernel lane only carries slot-exhaustion
/// reroutes, so the probe rate stays trivial.
const KERNEL_LANE_SLICE: std::time::Duration = std::time::Duration::from_millis(1);

/// Ring-only park cap: the wake is event-driven (the daemon FUTEX_WAKEs
/// the session completion doorbell while a reaper is registered), so
/// this bound only covers what the wait does not — cross-thread submits
/// on sessions this park never registered on.
const RING_PARK_RECHECK: std::time::Duration = std::time::Duration::from_millis(5);

/// Above this many in-flight ring ops the reap batches on a 50 µs
/// bounded sleep instead of event-parking. RE-SIZED **24 → 2** by the
/// 2026-07-28 Phase B counted A/B (fabric-latency venue, medians of 3,
/// engagement exact — `.benchmarks/2026-07-28-ipc-op-economy.md` §B.3):
/// under the completion DOORBELL, an event-parked reaper on a SHARED
/// session is woken by EVERY completion on that session — at qd16 ×
/// shared sessions that is a wake herd (one syscall per completion +
/// spurious rescans; measured −21 % on elbencho t16qd16 at the old 24,
/// −52 % at qd32 fully event-parked), where the 50 µs batch quantum's
/// latency share is invisible at depth. qd ≤ 2 stays event-driven —
/// the near-idle/latency shapes where the wake IS the contract (qd1
/// RTT clat 250–251 µs preserved; batching them cost −17 % and
/// +50 µs). At 2: t16qd16 505 k (> the 487 k pre-doorbell baseline),
/// t32qd32/fleet wash-or-better with −10 % daemon CPU, completion
/// wakes ≈ 0 at depth. `SQUEEZEFS_IL_REAP_PARK_MAX` is the measurement
/// lever (clamp 0..=4096, read once).
const REAP_EVENT_PARK_MAX: usize = 2;

fn reap_event_park_max() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        reap_event_park_max_from(std::env::var("SQUEEZEFS_IL_REAP_PARK_MAX").ok().as_deref())
    })
}

/// Pure sizing form (unit-pinned like `service_spin_window_from`).
fn reap_event_park_max_from(v: Option<&str>) -> usize {
    v.and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(0, 4096))
        .unwrap_or(REAP_EVENT_PARK_MAX)
}

/// # Safety
/// C ABI interposer; argument contracts are libaio's own.
#[no_mangle]
pub unsafe extern "C" fn io_getevents(
    ctx: u64,
    min_nr: c_long,
    nr: c_long,
    events: *mut RawIoEvent,
    timeout: *mut libc::timespec,
) -> c_int {
    let real = real_aio!("io_getevents", IoGetEventsFn);
    let fallback = |f: Option<IoGetEventsFn>| -> c_int {
        match f {
            // SAFETY: chaining the real io_getevents verbatim.
            Some(f) => unsafe { f(ctx, min_nr, nr, events, timeout) },
            None => -libc::ENOSYS,
        }
    };
    let Some(_g) = Guard::enter() else {
        return fallback(real);
    };
    // SAFETY: forwarding the caller's own array/timespec contracts.
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        aio_reap_served(ctx, min_nr, nr, events, timeout)
    })) {
        Ok(Some(r)) => r,
        Ok(None) => fallback(real),
        Err(_) => {
            // A panicked merge may have already consumed ring tickets
            // and written a prefix of the caller's event array —
            // replaying the REAL call would overwrite that prefix and
            // orphan those completions. Retro-neutralize the ctx and
            // answer -EINTR (kernel-legal, app-retryable; every later
            // call is the verbatim real call).
            panic_poison();
            aio_retro_neutralize(ctx);
            -libc::EINTR
        }
    }
}

type IoPGetEventsFn = unsafe extern "C" fn(
    u64,
    c_long,
    c_long,
    *mut RawIoEvent,
    *mut libc::timespec,
    *const libc::sigset_t,
) -> c_int;

/// # Safety
/// C ABI interposer; argument contracts are libaio's own.
///
/// Served reaps run the same bounded-pass merge as `io_getevents`; the
/// caller's sigmask is NOT applied during our short waits (signals stay
/// deliverable under the thread's own mask — strictly more wakeful,
/// never less; a ctx with no tracked ops takes the real call with exact
/// pgetevents semantics).
#[no_mangle]
pub unsafe extern "C" fn io_pgetevents(
    ctx: u64,
    min_nr: c_long,
    nr: c_long,
    events: *mut RawIoEvent,
    timeout: *mut libc::timespec,
    sigmask: *const libc::sigset_t,
) -> c_int {
    let real = real_aio!("io_pgetevents", IoPGetEventsFn);
    let fallback = |f: Option<IoPGetEventsFn>| -> c_int {
        match f {
            // SAFETY: chaining the real io_pgetevents verbatim.
            Some(f) => unsafe { f(ctx, min_nr, nr, events, timeout, sigmask) },
            None => -libc::ENOSYS,
        }
    };
    let Some(_g) = Guard::enter() else {
        return fallback(real);
    };
    // SAFETY: forwarding the caller's own array/timespec contracts.
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        aio_reap_served(ctx, min_nr, nr, events, timeout)
    })) {
        Ok(Some(r)) => r,
        Ok(None) => fallback(real),
        Err(_) => {
            // Same law as io_getevents' panic arm (see there).
            panic_poison();
            aio_retro_neutralize(ctx);
            -libc::EINTR
        }
    }
}

/// # Safety
/// C ABI interposer; argument contracts are libaio's own.
///
/// The panic arm keeps the real-call fallback (unlike submit/getevents):
/// the served body is read-only — it can neither dispatch nor consume,
/// so the real call stays exact.
#[no_mangle]
pub unsafe extern "C" fn io_cancel(ctx: u64, iocb: *mut RawIocb, evt: *mut RawIoEvent) -> c_int {
    let real = real_aio!("io_cancel", IoCancelFn);
    let fallback = |f: Option<IoCancelFn>| -> c_int {
        match f {
            // SAFETY: chaining the real io_cancel verbatim.
            Some(f) => unsafe { f(ctx, iocb, evt) },
            None => -libc::ENOSYS,
        }
    };
    let Some(_g) = Guard::enter() else {
        return fallback(real);
    };
    let served = catch_unwind(AssertUnwindSafe(|| {
        let m = aio_registry().lookup(ctx)?;
        let c = m.lock().unwrap_or_else(|p| p.into_inner());
        if c.state.ring_pending_contains(iocb as u64) {
            // A ring op cannot be recalled — the honest kernel answer
            // for an uncancellable in-flight op.
            Some(-libc::EINPROGRESS)
        } else {
            None // kernel-lane or unknown iocb: the real call decides
        }
    }));
    match served {
        Ok(Some(r)) => r,
        Ok(None) => fallback(real),
        Err(_) => {
            panic_poison();
            fallback(real)
        }
    }
}

#[cfg(test)]
mod session_sizing_tests {
    use super::*;

    /// Ingest-economy contract (2026-07-28): the shim's per-mount session
    /// default IS `squeezefs_ipc::sizing::il_sessions_default` — the same
    /// function the daemon's service-thread ceiling default rides (its
    /// tie test lives in the root suite), so the pair cannot drift. The
    /// field's flat-4-sessions vs cpus/4-threads mismatch left half the
    /// daemon's drain capacity idle at 7.5 GB/s of a 16.6 GB/s ceiling.
    #[test]
    fn sessions_default_ties_to_shared_derivation() {
        for cpus in [1, 2, 4, 8, 16, 22, 32, 48, 64, 128, 256] {
            assert_eq!(
                sessions_per_mount_from(None, cpus),
                squeezefs_ipc::sizing::il_sessions_default(cpus),
                "shim session default must ride the shared derivation \
                 (cpus={cpus})"
            );
        }
    }

    /// `SQUEEZEFS_IL_SESSIONS` remains an override LEVER only: honored
    /// verbatim within the clamp (1..=SESSION_REGISTRY_SLOTS/2 — the
    /// registry-derived ceiling), unparseable falls back to the derived
    /// default.
    #[test]
    fn sessions_env_override_is_a_lever() {
        assert_eq!(sessions_per_mount_from(Some("3"), 32), 3);
        assert_eq!(sessions_per_mount_from(Some("0"), 32), 1, "clamp floor");
        assert_eq!(
            sessions_per_mount_from(Some("99"), 32),
            squeezefs_ipc::sizing::SESSION_REGISTRY_SLOTS / 2,
            "clamp ceiling = the registry-derived derivation ceiling"
        );
        assert_eq!(
            sessions_per_mount_from(Some("garbage"), 32),
            squeezefs_ipc::sizing::il_sessions_default(32),
            "unparseable falls back to the derivation"
        );
    }

    /// Derivation sweep 2026-08-04: the shim's registry capacity IS the
    /// shared structural constant — drift between the static table and
    /// the sizing derivation's binder is red here.
    #[test]
    fn registry_ties_to_shared_structural_constant() {
        assert_eq!(
            MAX_SESSIONS,
            squeezefs_ipc::sizing::SESSION_REGISTRY_SLOTS,
            "the shim session table and the sizing binder must be one constant"
        );
    }
}

#[cfg(test)]
mod reap_park_max_tests {
    use super::*;

    /// The adjudicated default is **2** (the 2026-07-28 Phase B counted
    /// A/B — see the constant's doc: shared-session wake herding above
    /// qd2, latency-contract event parks at/below); the env form is an
    /// explicit measurement lever, clamped, unparseable ⇒ default.
    #[test]
    fn reap_park_max_default_and_clamp() {
        assert_eq!(reap_event_park_max_from(None), 2);
        assert_eq!(reap_event_park_max_from(Some("0")), 0);
        assert_eq!(reap_event_park_max_from(Some("64")), 64);
        assert_eq!(
            reap_event_park_max_from(Some("999999")),
            4096,
            "clamp ceiling"
        );
        assert_eq!(reap_event_park_max_from(Some("garbage")), 2);
    }
}
