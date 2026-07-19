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
//! - **Offsetful ops (§5.4.3)**: the kernel's `f_pos` stays
//!   authoritative — `lseek(SEEK_CUR)` → ring op → `lseek(SEEK_SET)`,
//!   serialized per fd by a stripe mutex (cold path by definition;
//!   positional ops never touch it).

use crate::bailout::{classify_fd, rwf_passthrough};
use crate::dev_cache::NegativeDevCache;
use crate::fd_table::{Binding, FdTable};
use crate::session::{RingOutcome, Session};
use squeezefs_ipc::wire::{BootstrapBlob, BOOTSTRAP_XATTR};

use libc::{c_char, c_int, c_long, c_uint, c_void, mode_t, off_t, size_t, ssize_t};
use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, Once, OnceLock};

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
/// admission spread them over service threads. K =
/// `SQUEEZEFS_IL_SESSIONS` (default 4, clamp 1..=8).
const MAX_SESSIONS: usize = 32;

fn sessions_per_mount() -> usize {
    static K: OnceLock<usize> = OnceLock::new();
    *K.get_or_init(|| {
        std::env::var("SQUEEZEFS_IL_SESSIONS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .map(|n| n.clamp(1, 8))
            .unwrap_or(4)
    })
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

/// Offsetful-op serialization (§5.4.3): stripe of tiny mutexes keyed by
/// fd. Cold path by definition — positional ops never touch it.
fn offset_lock(fd: c_int) -> &'static Mutex<()> {
    static LOCKS: OnceLock<Vec<Mutex<()>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| (0..64).map(|_| Mutex::new(())).collect());
    &locks[(fd as usize) & 63]
}

/// One loud, allocation-free stderr line (panic/pathology paths).
fn stderr_line(msg: &str) {
    // SAFETY: plain write(2) to stderr; best-effort.
    unsafe {
        libc::write(2, msg.as_ptr() as *const c_void, msg.len());
    }
}

fn set_errno(e: c_int) {
    // SAFETY: thread-local errno write.
    unsafe { *libc::__errno_location() = e };
}

fn panic_poison() {
    registry().poison_all();
    stderr_line("squeezefs-il: PANIC in interposer — all sessions poisoned, passthrough\n");
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
fn classify_and_bind(fd: c_int) {
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
                    stderr_line("squeezefs-il: session establish failed — mount passthrough\n");
                    let _ = e;
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
    if let Ok(grant) = session.bind(fd) {
        route_release(table().bind(fd, grant.to_binding(token)));
    }
}

// ---------------------------------------------------------------------------
// ring data ops (shared shapes)
// ---------------------------------------------------------------------------

/// `Some(result)` = the ring served it (or a real daemon errno);
/// `None` = take the real call.
fn ring_positional(
    fd: c_int,
    buf: *mut u8,
    count: usize,
    offset: off_t,
    write: bool,
) -> Option<ssize_t> {
    if offset < 0 || count == 0 || buf.is_null() {
        return None; // the real call's errno/0 is the identical answer
    }
    let b = table().lookup(fd)?;
    if (write && !b.write_ok) || (!write && !b.read_ok) {
        return None; // kernel EBADF on the real fd is the correct answer
    }
    let session = registry().by_token(b.session)?;
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
        RingOutcome::Served(n) => Some(n as ssize_t),
        RingOutcome::Errno(e) => {
            set_errno(e);
            Some(-1)
        }
        RingOutcome::Fallthrough => {
            if session.poisoned() {
                // §5.4.1 error-class row: unbind (ctl already silent on a
                // poisoned session) and let the real fd serve.
                route_release(table().on_close(fd));
            }
            None
        }
    }
}

/// §5.4.3 offsetful shape: kernel `f_pos` stays authoritative.
fn ring_offsetful(fd: c_int, buf: *mut u8, count: usize, write: bool) -> Option<ssize_t> {
    table().lookup(fd)?; // cheap pre-check before taking the stripe lock
    let _l = offset_lock(fd).lock().ok()?;
    // SAFETY: plain lseek64(2) — SEEK_CUR touches only f_pos (no FUSE
    // request; pinned by the gate script's lseek row).
    let off = unsafe { libc::lseek64(fd, 0, libc::SEEK_CUR) };
    if off < 0 {
        return None; // unseekable: the real call is the answer
    }
    let n = ring_positional(fd, buf, count, off as off_t, write)?;
    if n > 0 {
        // SAFETY: restore the kernel offset to cover the served bytes.
        unsafe { libc::lseek64(fd, off + n as i64, libc::SEEK_SET) };
    }
    Some(n)
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
    let b = table().lookup(fd)?;
    if (write && !b.write_ok) || (!write && !b.read_ok) {
        return None;
    }
    // Hold the stripe lock across the whole chain for the offsetful
    // shape (one atomic f_pos advance for the vector op).
    let (_l, mut off) = if offset < 0 {
        let l = offset_lock(fd).lock().ok()?;
        // SAFETY: lseek64 as in ring_offsetful.
        let cur = unsafe { libc::lseek64(fd, 0, libc::SEEK_CUR) };
        if cur < 0 {
            return None;
        }
        (Some(l), cur)
    } else {
        (None, offset)
    };
    let start = off;
    let mut total: ssize_t = 0;
    for i in 0..iovcnt {
        // SAFETY: the app's iovec array contract, same as libc.
        let e = unsafe { &*iov.add(i as usize) };
        if e.iov_len == 0 {
            continue;
        }
        match ring_positional(fd, e.iov_base as *mut u8, e.iov_len, off as off_t, write) {
            Some(n) if n >= 0 => {
                total += n;
                off += n as i64;
                if (n as usize) < e.iov_len {
                    break; // short segment ends the vector op
                }
            }
            Some(_) => {
                // Daemon errno mid-chain: partial progress returns short
                // (POSIX-legal); zero progress surfaces the errno.
                if total == 0 {
                    return Some(-1); // errno already set
                }
                break;
            }
            None => {
                if total == 0 {
                    return None; // clean fallthrough, nothing served
                }
                break; // partial: return short, the app continues
            }
        }
    }
    if _l.is_some() && total > 0 {
        // SAFETY: restore the kernel offset over the served span.
        unsafe { libc::lseek64(fd, start + total as i64, libc::SEEK_SET) };
    }
    Some(total)
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
// detection / lifecycle symbols
// ---------------------------------------------------------------------------

type OpenFn = unsafe extern "C" fn(*const c_char, c_int, mode_t) -> c_int;
type OpenatFn = unsafe extern "C" fn(c_int, *const c_char, c_int, mode_t) -> c_int;
type CreatFn = unsafe extern "C" fn(*const c_char, mode_t) -> c_int;

fn atfork_init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        extern "C" fn child() {
            // AS-safe by contract (§5.4.1 fork row) — the CHILD variant:
            // close(2) of the inherited copies, never shutdown (see
            // poison_all_in_child).
            registry().poison_all_in_child();
        }
        // SAFETY: registering an AS-safe child handler.
        unsafe { libc::pthread_atfork(None, None, Some(child)) };
    });
}

/// Shared open-family tail: real call already made; classify + maybe
/// bind the fresh fd (guard held by the macro body).
fn after_open(fd: c_int) -> Option<c_int> {
    atfork_init();
    classify_and_bind(fd);
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
            // SAFETY: chaining the real open.
            let fd = unsafe { f(path, oflag, mode) };
            let Some(_g) = Guard::enter() else { return fd };
            match catch_unwind(AssertUnwindSafe(|| after_open(fd))) {
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
            // SAFETY: chaining the real openat.
            let fd = unsafe { f(dirfd, path, oflag, mode) };
            let Some(_g) = Guard::enter() else { return fd };
            match catch_unwind(AssertUnwindSafe(|| after_open(fd))) {
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
            // SAFETY: chaining the real creat.
            let fd = unsafe { f(path, mode) };
            let Some(_g) = Guard::enter() else { return fd };
            match catch_unwind(AssertUnwindSafe(|| after_open(fd))) {
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
                    // with the kernel as the only data transport.
                    route_release(table().on_close(fd));
                }
                libc::F_SETFL => {
                    let new_flags = arg as usize as c_int;
                    if new_flags & libc::O_APPEND != 0 {
                        route_release(table().on_close(fd));
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
                    table().unbind_ino(b.ino, &mut released);
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
// co-located). The lock is never held across a long kernel wait —
// `io_getevents` drives bounded ~50 ms merge passes and re-acquires
// between them, so a split submitter/reaper pair (a legal libaio shape)
// degrades to 50 ms granularity instead of deadlocking on the lock.
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
        let Some(f) = real!("io_submit", IoSubmitFn) else {
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
        let Some(f) = real!("io_getevents", IoGetEventsFn) else {
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

/// # Safety
/// C ABI interposer; argument contracts are libaio's own.
#[no_mangle]
pub unsafe extern "C" fn io_setup(maxevents: c_int, ctxp: *mut u64) -> c_int {
    let Some(f) = real!("io_setup", IoSetupFn) else {
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
        // passthroughs wholesale.
        // SAFETY: the real call just wrote *ctxp.
        aio_registry().register(unsafe { *ctxp });
    }))
    .is_err()
    {
        panic_poison();
    }
    r
}

/// # Safety
/// C ABI interposer; argument contracts are libaio's own.
#[no_mangle]
pub unsafe extern "C" fn io_destroy(ctx: u64) -> c_int {
    let Some(f) = real!("io_destroy", IoDestroyFn) else {
        return -libc::ENOSYS;
    };
    if let Some(_g) = Guard::enter() {
        if catch_unwind(AssertUnwindSafe(|| {
            if let Some(m) = aio_registry().lookup(ctx) {
                let mut c = m.lock().unwrap_or_else(|p| p.into_inner());
                let AioCtx { state, tickets } = &mut *c;
                let mut ring = SessionRing { tickets };
                state.destroy(&mut ring);
                drop(c);
                aio_registry().remove(ctx);
            }
        }))
        .is_err()
        {
            panic_poison();
        }
    }
    // SAFETY: chaining the real io_destroy (kernel-lane ops die with
    // the kernel ctx, exactly as un-interposed libaio).
    unsafe { f(ctx) }
}

/// # Safety
/// C ABI interposer; argument contracts are libaio's own.
#[no_mangle]
pub unsafe extern "C" fn io_submit(ctx: u64, nr: c_long, ios: *mut *mut RawIocb) -> c_int {
    let real = real!("io_submit", IoSubmitFn);
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
            panic_poison();
            fallback(real)
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
            // process never touch SqueezeFS fds).
            let c = m.lock().unwrap_or_else(|p| p.into_inner());
            if c.state.ring_pending() == 0 && c.state.kernel_pending() == 0 {
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
        let mut empty_passes = 0u32;
        loop {
            // One bounded merge pass per lock hold (≤ ~50 ms): a split
            // submitter/reaper pair shares the ctx at that granularity.
            let pass_ms: u64 = match deadline {
                None => 50,
                Some(d) => {
                    let left = d.saturating_duration_since(std::time::Instant::now());
                    (left.as_millis() as u64).min(50)
                }
            };
            let evs = {
                let mut c = m.lock().unwrap_or_else(|p| p.into_inner());
                let AioCtx { state, tickets } = &mut *c;
                let mut ring = SessionRing { tickets };
                let mut kern = RealKernel { ctx };
                state.getevents(
                    &mut ring,
                    &mut kern,
                    min_nr.saturating_sub(got).min(nr - got),
                    nr - got,
                    Some(pass_ms),
                )
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
            // Ring-pending passes return in ~5 ms slices with nothing
            // to show; park briefly OUTSIDE the lock so a spinning
            // reaper is not CPU theft from the daemon (the adaptive-
            // spin lesson) and a split submitter can take the ctx.
            if evs.is_empty() {
                empty_passes += 1;
                if empty_passes > 2 {
                    std::thread::sleep(std::time::Duration::from_micros(200));
                } else {
                    std::thread::yield_now();
                }
            } else {
                empty_passes = 0;
            }
        }
    }
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
    let real = real!("io_getevents", IoGetEventsFn);
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
            panic_poison();
            fallback(real)
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
    let real = real!("io_pgetevents", IoPGetEventsFn);
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
            panic_poison();
            fallback(real)
        }
    }
}

/// # Safety
/// C ABI interposer; argument contracts are libaio's own.
#[no_mangle]
pub unsafe extern "C" fn io_cancel(ctx: u64, iocb: *mut RawIocb, evt: *mut RawIoEvent) -> c_int {
    let real = real!("io_cancel", IoCancelFn);
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
