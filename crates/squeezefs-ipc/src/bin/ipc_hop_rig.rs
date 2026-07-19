//! Two-process IPC-hop rig — the G-L4-1 go/no-go spike
//! (design-preload-interception §5.8 / PR L4-2).
//!
//! REAL mechanism, not a mock: the orchestrator (daemon role) creates a
//! **sealed memfd** session (`memfd_create` + `F_SEAL_GROW|SHRINK|SEAL`),
//! listens on an **abstract AF_UNIX `SOCK_SEQPACKET`** socket, re-spawns
//! itself as the client-role process, and hands the session over with
//! **`SCM_RIGHTS`** after a HELLO/SESSION exchange. Ops then flow over the
//! shipped `squeezefs-ipc` protocol: MPSC submission ring + completion-in-
//! place slots + futex doorbell under the shipped `WakeCoalescer` elision
//! discipline (§5.3 rules 1–3). Cross-process futexes are non-private, and
//! every daemon park is timeout-bounded (§5.3.1 rule 5, D18 ladder).
//!
//! Three legs (G-L4-1):
//! - **echo** — tier-hit RTT shape, 4 KiB payload each way, minimal
//!   service: the daemon reads the request payload (one summing pass —
//!   the result echoes the sum, proving the read) and writes a derived
//!   response pattern back into the slab (one filling pass); the client
//!   pays the user→arena and arena→user copies. No maps, no locks.
//! - **serve** — echo's transport plus the §5.8.2 serve-shaped stand-ins:
//!   descriptor validation against a binding table, an `scc::HashMap`
//!   tier probe, 2 × payload memcpy (request absorb into private scratch
//!   + tier-block fill into the arena), stats increments.
//! - **handoff** — echo's op, but the payload work + completion post run
//!   in a task on an embedded tokio runtime (service thread → channel
//!   send → task wake → complete): the demote-path adder, measured
//!   against echo under the identical driver shape.
//!
//! Output: machine-parsable `SUMMARY k=v …` + per-service-thread
//! `SERVICE …` lines; the client process reports its half over stdout
//! (`CSTATS …`), aggregated by the orchestrator. Correctness is pinned by
//! `tests/rig_smoke.rs` (--verify: full pattern verification both
//! directions, zero tolerance).
//!
//! Single-consumer discipline: the submission ring is drained by exactly
//! ONE service thread (`ring_core`'s precondition); with N service
//! threads the extra threads are serve *helpers* fed by daemon-private
//! SPSC hand-offs after the single dequeue point — the same shape the
//! daemon host uses for multi-session pinning (§5.5.1).

use squeezefs_ipc::layout::{
    Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_READ, PAGE_BYTES,
};
use squeezefs_ipc::ring_core::{MpscRingView, RingCell, RingConsumer};
use squeezefs_ipc::slot_core::ParkOutcome;

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// args
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Args {
    leg: String,
    role: Option<String>,
    socket: Option<String>,
    threads: u32,
    service_threads: u32,
    runtime_workers: u32,
    ops: u64,
    warmup: u64,
    payload: u32,
    spin_ns: u64,
    verify: bool,
    kill_client_after_ms: Option<u64>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut a = Args {
            leg: "echo".to_string(),
            role: None,
            socket: None,
            threads: 4,
            service_threads: 1,
            runtime_workers: 4,
            ops: 200_000,
            warmup: 20_000,
            payload: 4096,
            spin_ns: 4_000,
            verify: false,
            kill_client_after_ms: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut val = |name: &str| -> Result<String, String> {
                it.next().ok_or(format!("{name} needs a value"))
            };
            fn num<T: std::str::FromStr>(s: String) -> Result<T, String>
            where
                T::Err: std::fmt::Display,
            {
                s.parse().map_err(|e| format!("{e}"))
            }
            match flag.as_str() {
                "--leg" => a.leg = val("--leg")?,
                "--role" => a.role = Some(val("--role")?),
                "--socket" => a.socket = Some(val("--socket")?),
                "--threads" => a.threads = num(val("--threads")?)?,
                "--service-threads" => a.service_threads = num(val("--service-threads")?)?,
                "--runtime-workers" => a.runtime_workers = num(val("--runtime-workers")?)?,
                "--ops" => a.ops = num(val("--ops")?)?,
                "--warmup" => a.warmup = num(val("--warmup")?)?,
                "--payload" => a.payload = num(val("--payload")?)?,
                "--spin-ns" => a.spin_ns = num(val("--spin-ns")?)?,
                "--verify" => a.verify = true,
                "--kill-client-after-ms" => {
                    a.kill_client_after_ms = Some(num(val("--kill-client-after-ms")?)?)
                }
                other => return Err(format!("unknown flag {other}")),
            }
        }
        if !matches!(a.leg.as_str(), "echo" | "serve" | "handoff") {
            return Err(format!("unknown leg {}", a.leg));
        }
        if a.threads == 0 || a.threads > 64 {
            return Err("--threads must be 1..=64".into());
        }
        if a.service_threads == 0 || a.service_threads > 8 {
            return Err("--service-threads must be 1..=8".into());
        }
        if a.payload == 0 || !u64::from(a.payload).is_multiple_of(8) {
            return Err("--payload must be a nonzero multiple of 8".into());
        }
        if a.warmup >= a.ops {
            return Err("--warmup must be < --ops".into());
        }
        Ok(a)
    }
}

// ---------------------------------------------------------------------------
// libc plumbing: memfd, mmap, SCM_RIGHTS, futex, thread CPU
// ---------------------------------------------------------------------------

fn errno_str(ctx: &str) -> String {
    format!("{ctx}: {}", std::io::Error::last_os_error())
}

/// cmsg control buffer with the alignment `cmsghdr` requires (a plain
/// `[u8; N]` on the stack is align-1 — writing headers through it is a
/// misaligned dereference).
#[repr(C, align(8))]
struct CmsgBuf([u8; 64]);

fn create_sealed_memfd(total: u64) -> Result<OwnedFd, String> {
    // SAFETY: plain syscalls; the returned fd is owned immediately.
    unsafe {
        let fd = libc::memfd_create(
            c"squeezefs-ipc-rig".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        );
        if fd < 0 {
            return Err(errno_str("memfd_create"));
        }
        let fd = OwnedFd::from_raw_fd(fd);
        if libc::ftruncate(fd.as_raw_fd(), total as libc::off_t) != 0 {
            return Err(errno_str("ftruncate"));
        }
        let seals = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        if libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) != 0 {
            return Err(errno_str("F_ADD_SEALS"));
        }
        Ok(fd)
    }
}

fn map_shared(fd: RawFd, len: usize) -> Result<*mut u8, String> {
    // SAFETY: shared mapping of a sealed memfd; len is the layout total,
    // checked against fstat by the caller.
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if p == libc::MAP_FAILED {
            return Err(errno_str("mmap"));
        }
        Ok(p as *mut u8)
    }
}

fn fd_size(fd: RawFd) -> Result<u64, String> {
    // SAFETY: fstat into a zeroed buffer.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return Err(errno_str("fstat"));
        }
        Ok(st.st_size as u64)
    }
}

/// Send one SEQPACKET message (+ optionally one fd via SCM_RIGHTS).
fn send_msg(sock: RawFd, data: &[u8], fd: Option<RawFd>) -> Result<(), String> {
    // SAFETY: standard sendmsg + CMSG plumbing; every buffer outlives the
    // call; MSG_NOSIGNAL so a dead peer is EPIPE, not SIGPIPE.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: data.as_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        let mut cbuf = CmsgBuf([0u8; 64]);
        if let Some(fd) = fd {
            msg.msg_control = cbuf.0.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
            std::ptr::copy_nonoverlapping(
                &fd as *const RawFd as *const u8,
                libc::CMSG_DATA(cmsg),
                std::mem::size_of::<RawFd>(),
            );
        }
        if libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) < 0 {
            return Err(errno_str("sendmsg"));
        }
        Ok(())
    }
}

/// Receive one SEQPACKET message (+ optionally one fd).
fn recv_msg(sock: RawFd, buf: &mut [u8]) -> Result<(usize, Option<OwnedFd>), String> {
    // SAFETY: standard recvmsg + CMSG walk; a received fd's ownership is
    // taken exactly once.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        let mut cbuf = CmsgBuf([0u8; 64]);
        msg.msg_control = cbuf.0.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cbuf.0.len();
        let n = libc::recvmsg(sock, &mut msg, 0);
        if n < 0 {
            return Err(errno_str("recvmsg"));
        }
        let mut fd = None;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let mut raw: RawFd = -1;
                std::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(cmsg),
                    &mut raw as *mut RawFd as *mut u8,
                    std::mem::size_of::<RawFd>(),
                );
                fd = Some(OwnedFd::from_raw_fd(raw));
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        Ok((n as usize, fd))
    }
}

/// Abstract-namespace AF_UNIX (zero filesystem residue). std's listener
/// API wants paths, so bind/connect are raw.
fn abstract_sockaddr(name: &str) -> (libc::sockaddr_un, libc::socklen_t) {
    // SAFETY: a zeroed sockaddr_un is a valid default.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = name.as_bytes();
    assert!(
        bytes.len() + 1 < addr.sun_path.len(),
        "socket name too long"
    );
    for (i, b) in bytes.iter().enumerate() {
        // sun_path[0] stays 0 → abstract namespace; the name follows.
        addr.sun_path[i + 1] = *b as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len();
    (addr, len as libc::socklen_t)
}

fn abstract_listen(name: &str) -> Result<UnixListener, String> {
    // SAFETY: socket/bind/listen with a validated abstract sockaddr.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(errno_str("socket"));
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let (addr, len) = abstract_sockaddr(name);
        if libc::bind(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len,
        ) != 0
        {
            return Err(errno_str("bind"));
        }
        if libc::listen(fd.as_raw_fd(), 1) != 0 {
            return Err(errno_str("listen"));
        }
        Ok(UnixListener::from(fd))
    }
}

fn abstract_connect(name: &str) -> Result<UnixStream, String> {
    // SAFETY: socket/connect with a validated abstract sockaddr.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(errno_str("socket"));
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let (addr, len) = abstract_sockaddr(name);
        if libc::connect(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len,
        ) != 0
        {
            return Err(errno_str("connect"));
        }
        Ok(UnixStream::from(fd))
    }
}

/// Cross-process futex wait (NON-private — the word lives in shm). Returns
/// `true` on a clean wake; `false` on EAGAIN/ETIMEDOUT/EINTR (caller
/// re-checks). Daemon-side parks always pass a timeout (§5.3.1 rule 5).
fn futex_wait(word: &AtomicU32, expected: u32, timeout: Option<Duration>) -> bool {
    let ts = timeout.map(|d| libc::timespec {
        tv_sec: d.as_secs() as libc::time_t,
        tv_nsec: i64::from(d.subsec_nanos()),
    });
    // SAFETY: FUTEX_WAIT on a live shm word; the timespec outlives the
    // call.
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT,
            expected,
            ts.as_ref()
                .map_or(std::ptr::null(), |t| t as *const libc::timespec),
            std::ptr::null::<u32>(),
            0u32,
        )
    };
    r == 0
}

fn futex_wake(word: &AtomicU32, n: i32) {
    // SAFETY: FUTEX_WAKE on a live shm word.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAKE,
            n,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        );
    }
}

/// Pin the calling thread to the i-th CPU of the CURRENT affinity mask
/// (so taskset rails compose: pinning picks within the rail, never
/// escapes it).
fn pin_thread_to_cpu(index: u32) {
    // SAFETY: standard affinity get/set on self.
    unsafe {
        let mut cur: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut cur) != 0 {
            return;
        }
        let allowed: Vec<usize> = (0..libc::CPU_SETSIZE as usize)
            .filter(|&c| libc::CPU_ISSET(c, &cur))
            .collect();
        if allowed.is_empty() {
            return;
        }
        let target = allowed[index as usize % allowed.len()];
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(target, &mut set);
        let _ = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

/// Whole-process CPU ns (user+sys) via getrusage — the daemon side's
/// dispatcher + runtime workers together, for the §5.8.4 CPU/op rows.
fn process_cpu_ns() -> u64 {
    // SAFETY: getrusage into a zeroed buffer.
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1_000;
        tv(ru.ru_utime) + tv(ru.ru_stime)
    }
}

fn thread_cpu_ns() -> u64 {
    // SAFETY: clock_gettime into a zeroed timespec.
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }
}

// ---------------------------------------------------------------------------
// the mapped session (both roles)
// ---------------------------------------------------------------------------

/// One mapped session; leaked for process lifetime (a rig process maps
/// exactly one), which is what makes the `&'static` protocol refs sound.
struct Session {
    header: &'static SessionHeader,
    ring_tail: &'static AtomicU32,
    cells: &'static [RingCell],
    slots: &'static [IpcSlot],
    arena: ArenaPtr,
}

/// Raw arena base. Slab windows are disjoint per slot by layout; slot
/// claim exclusivity is what serializes access per (slot, op) — exactly
/// the production discipline.
#[derive(Clone, Copy)]
struct ArenaPtr {
    base: *mut u8,
    len: usize,
}
// SAFETY: the arena is shared memory addressed through bounds-checked raw
// pointers; per-(slot,op) disjointness comes from slot claim exclusivity.
unsafe impl Send for ArenaPtr {}
unsafe impl Sync for ArenaPtr {}

impl ArenaPtr {
    fn slab(&self, slot_index: usize, max_op: usize) -> *mut u8 {
        let off = slot_index * max_op;
        assert!(off + max_op <= self.len, "slab out of arena bounds");
        // SAFETY: bounds asserted; base is a live mapping.
        unsafe { self.base.add(off) }
    }
}

impl Session {
    /// Map + cast a session. `init` = daemon role (writes the header,
    /// seeds the ring); the client role validates what it finds instead.
    fn from_fd(fd: RawFd, geometry: Geometry, init: bool) -> Result<&'static Session, String> {
        let layout = SessionLayout::compute(&geometry).map_err(|e| e.to_string())?;
        let size = fd_size(fd)?;
        if size != layout.total_bytes {
            return Err(format!(
                "memfd size {size} != layout total {}",
                layout.total_bytes
            ));
        }
        let base = map_shared(fd, layout.total_bytes as usize)?;
        // SAFETY: the mapping spans the full layout (checked above); every
        // cast below lands inside it at the layout's page-aligned offsets,
        // and each cast type is the repr(C) shm type the layout arithmetic
        // sized. Leak = process lifetime.
        unsafe {
            let header_ptr = base as *mut SessionHeader;
            if init {
                header_ptr.write(SessionHeader::new(geometry));
            }
            let header: &'static SessionHeader = &*header_ptr;
            if !init {
                header.validate().map_err(|e| e.to_string())?;
                if header.geometry != geometry {
                    return Err("client/daemon geometry disagreement".into());
                }
            }
            let ring_tail: &'static AtomicU32 =
                &*(base.add(layout.ring_off as usize) as *const AtomicU32);
            let cells: &'static [RingCell] = std::slice::from_raw_parts(
                base.add(layout.ring_cells_off as usize) as *const RingCell,
                geometry.ring_entries as usize,
            );
            let slots: &'static [IpcSlot] = std::slice::from_raw_parts(
                base.add(layout.slots_off as usize) as *const IpcSlot,
                geometry.slots as usize,
            );
            let arena = ArenaPtr {
                base: base.add(layout.arena_off as usize),
                len: layout.arena_bytes as usize,
            };
            let session = Box::leak(Box::new(Session {
                header,
                ring_tail,
                cells,
                slots,
                arena,
            }));
            if init {
                session.ring().seed_for_sharing();
            }
            Ok(session)
        }
    }

    fn ring(&self) -> MpscRingView<'static> {
        MpscRingView::from_parts(self.ring_tail, self.cells)
            .expect("session geometry validated at construction")
    }

    fn max_op(&self) -> usize {
        self.header.geometry.max_op_bytes as usize
    }
}

fn rig_geometry(args: &Args) -> Geometry {
    let slots: u32 = 1024;
    let max_op = u64::from(args.payload).next_multiple_of(PAGE_BYTES) as u32;
    Geometry {
        ring_entries: 1024,
        slots,
        arena_bytes: u64::from(slots) * u64::from(max_op),
        max_op_bytes: max_op,
        _pad: 0,
    }
}

// ---------------------------------------------------------------------------
// doorbell (§5.3 rule 1 mapped onto the header words)
// ---------------------------------------------------------------------------

/// Client side: publish (the ring push) FIRST, then arm; wake only when
/// the daemon is parked. Returns whether a FUTEX_WAKE syscall was issued.
fn doorbell_ring(header: &SessionHeader) -> bool {
    if header.doorbell_coalescer.arm() && header.daemon_parked.load(Ordering::SeqCst) == 1 {
        header.doorbell.fetch_add(1, Ordering::SeqCst);
        futex_wake(&header.doorbell, 1);
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// payload patterns (verification: the daemon PROVES it read the request —
// the result echoes its sum — and the client verifies the response fill)
// ---------------------------------------------------------------------------

const PATTERN_MULT: u64 = 0x9e37_79b9_7f4a_7c15;
/// The serve leg's tier blocks carry this fixed seed.
const TIER_SEED: u64 = 7777;

fn fill_pattern(ptr: *mut u8, len: usize, seed: u64) {
    // SAFETY: caller passes an in-bounds window; u64-stride writes.
    unsafe {
        let p = ptr as *mut u64;
        for i in 0..len / 8 {
            p.add(i).write(seed ^ (i as u64).wrapping_mul(PATTERN_MULT));
        }
    }
}

fn sum_words(ptr: *const u8, len: usize) -> u64 {
    // SAFETY: caller passes an in-bounds window; u64-stride reads.
    unsafe {
        let p = ptr as *const u64;
        let mut acc = 0u64;
        for i in 0..len / 8 {
            acc = acc.wrapping_add(p.add(i).read());
        }
        acc
    }
}

fn check_pattern(ptr: *const u8, len: usize, seed: u64) -> bool {
    // SAFETY: caller passes an in-bounds window; u64-stride reads.
    unsafe {
        let p = ptr as *const u64;
        for i in 0..len / 8 {
            if p.add(i).read() != seed ^ (i as u64).wrapping_mul(PATTERN_MULT) {
                return false;
            }
        }
        true
    }
}

fn expected_sum(seed: u64, len: usize) -> u64 {
    let mut acc = 0u64;
    for i in 0..len / 8 {
        acc = acc.wrapping_add(seed ^ (i as u64).wrapping_mul(PATTERN_MULT));
    }
    acc
}

fn response_seed(request_seed: u64) -> u64 {
    request_seed ^ 0xa5a5_a5a5_5a5a_5a5a
}

// ---------------------------------------------------------------------------
// daemon role
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DaemonCounters {
    pops: AtomicU64,
    served: AtomicU64,
    slot_wakes: AtomicU64,
    doorbell_waits: AtomicU64,
    doorbell_wait_timeouts: AtomicU64,
    serve_rejects: AtomicU64,
    // serve-shaped stats stand-in (the "stats increments" cost).
    stat_ops: AtomicU64,
    stat_bytes_in: AtomicU64,
    stat_bytes_out: AtomicU64,
    stat_tier_hits: AtomicU64,
}

/// Serve-shaped stand-ins (§5.8.2 leg ii): binding authority + tier map.
struct ServeShaped {
    /// binding id → (mode bits, len limit): the validation stand-in.
    bindings: scc::HashMap<u64, (u32, u32)>,
    /// (hashed key) → payload-sized block: the tier probe + fill source.
    tier: scc::HashMap<u64, Box<[u8]>>,
}

impl ServeShaped {
    fn build(payload: usize) -> Arc<Self> {
        let bindings = scc::HashMap::new();
        for b in 1..=64u64 {
            let _ = bindings.insert_sync(b, (1u32, 1 << 20));
        }
        let tier = scc::HashMap::new();
        for k in 0..1024u64 {
            let mut block = vec![0u8; payload].into_boxed_slice();
            fill_pattern(block.as_mut_ptr(), payload, TIER_SEED);
            let _ = tier.insert_sync(k, block);
        }
        Arc::new(Self { bindings, tier })
    }
}

fn complete_slot(counters: &DaemonCounters, slot: &IpcSlot, result: i64) {
    slot.set_result(result);
    if slot.core.complete() {
        counters.slot_wakes.fetch_add(1, Ordering::Relaxed);
        futex_wake(slot.core.state_futex_word(), 1);
    }
    counters.served.fetch_add(1, Ordering::Relaxed);
}

/// Serve one op in place. Snapshot-then-validate discipline (§5.3.1 rule
/// 1): ONE descriptor read; validation + serve from the copy.
fn serve_slot(
    session: &'static Session,
    counters: &DaemonCounters,
    serve_shaped: Option<&ServeShaped>,
    idx: u32,
    payload: usize,
) {
    let Some(slot) = session.slots.get(idx as usize) else {
        counters.serve_rejects.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if !slot.core.try_begin_serve() {
        counters.serve_rejects.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // THE single linearization read.
    let d: SlotDescriptor = slot.snapshot_descriptor();
    let max_op = session.max_op();
    let len = d.len as usize;
    // Bounds validation against the daemon's own layout authority is part
    // of the transport's honest per-op cost (always on).
    if d.op != OP_READ || len == 0 || len > max_op || len != payload {
        complete_slot(counters, slot, -i64::from(libc::EINVAL));
        return;
    }
    if d.arena_off as usize != idx as usize * max_op {
        complete_slot(counters, slot, -i64::from(libc::EINVAL));
        return;
    }
    let slab = session.arena.slab(idx as usize, max_op);

    let result = if let Some(serve) = serve_shaped {
        // Validation stand-in: binding lookup + mode/limit screen.
        let mut mode_ok = false;
        serve.bindings.read_sync(&d.binding, |_, (mode, limit)| {
            mode_ok = *mode & 1 == 1 && d.len <= *limit;
        });
        if !mode_ok {
            complete_slot(counters, slot, -i64::from(libc::EBADF));
            return;
        }
        // memcpy #1 — request absorb: arena → private scratch (+ the
        // summing read rides the scratch copy, §5.3.1 rule 2 shape:
        // derived values from the private copy, never a second arena
        // read).
        let mut scratch = vec![0u8; len];
        // SAFETY: slab window validated in-bounds; scratch sized len.
        unsafe { std::ptr::copy_nonoverlapping(slab, scratch.as_mut_ptr(), len) };
        let sum = sum_words(scratch.as_ptr(), len);
        // The scc tier probe + memcpy #2 — tier block → arena.
        let key = d
            .binding
            .wrapping_mul(31)
            .wrapping_add(d.offset / PAGE_BYTES)
            % 1024;
        let hit = serve
            .tier
            .read_sync(&key, |_, block| {
                // SAFETY: slab validated; block is payload-sized.
                unsafe { std::ptr::copy_nonoverlapping(block.as_ptr(), slab, len) };
            })
            .is_some();
        // Stats increments (the §5.8.2 stand-in's last component).
        counters.stat_ops.fetch_add(1, Ordering::Relaxed);
        counters
            .stat_bytes_in
            .fetch_add(len as u64, Ordering::Relaxed);
        counters
            .stat_bytes_out
            .fetch_add(len as u64, Ordering::Relaxed);
        counters
            .stat_tier_hits
            .fetch_add(u64::from(hit), Ordering::Relaxed);
        sum as i64
    } else {
        // echo: one summing read pass + one pattern-fill write pass — the
        // 4 KiB-each-way tier-hit shape with minimal service.
        let sum = sum_words(slab, len);
        fill_pattern(slab, len, response_seed(d.offset));
        sum as i64
    };
    complete_slot(counters, slot, result);
}

/// What the (single) ring consumer does with a dequeued index.
enum ServiceKind {
    Echo,
    Serve(Arc<ServeShaped>),
    /// The §5.5.1 demote-path shape: every op becomes its own task on the
    /// embedded runtime (spawn = the task wake the leg prices); the task
    /// does the payload work and posts the completion.
    Handoff(tokio::runtime::Handle),
}

/// Daemon-private SPSC hand-off (producer = the one ring consumer,
/// consumer = one helper service thread).
struct SpscQueue {
    buf: Vec<AtomicU32>,
    head: AtomicU64,
    tail: AtomicU64,
}

impl SpscQueue {
    fn new(cap: usize) -> Self {
        Self {
            buf: (0..cap).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
        }
    }
    fn push(&self, v: u32) {
        loop {
            let t = self.tail.load(Ordering::Relaxed);
            let h = self.head.load(Ordering::Acquire);
            if (t - h) as usize == self.buf.len() {
                std::hint::spin_loop();
                continue;
            }
            self.buf[t as usize % self.buf.len()].store(v, Ordering::Relaxed);
            self.tail.store(t + 1, Ordering::Release);
            return;
        }
    }
    fn pop(&self) -> Option<u32> {
        let h = self.head.load(Ordering::Relaxed);
        if h == self.tail.load(Ordering::Acquire) {
            return None;
        }
        let v = self.buf[h as usize % self.buf.len()].load(Ordering::Relaxed);
        self.head.store(h + 1, Ordering::Release);
        Some(v)
    }
}

struct ServiceShared {
    session: &'static Session,
    counters: Arc<DaemonCounters>,
    shutdown: Arc<AtomicBool>,
    fanout: Arc<Vec<SpscQueue>>,
    payload: usize,
    service_threads: u32,
}

/// Service thread main. Thread 0 is THE single ring consumer (ring_core
/// precondition) and owns the doorbell park protocol; helper threads
/// serve from their SPSC hand-off. D18 ladder: spin → bounded futex park
/// (50 µs escalating to 5 ms).
fn service_thread(shared: Arc<ServiceShared>, kind: ServiceKind, index: u32) -> (u64, u64) {
    let session = shared.session;
    let ring = session.ring();
    let mut cursor = (index == 0).then(RingConsumer::new);
    let mut last_parked_tail = 0u32;
    let cpu0 = thread_cpu_ns();
    let wall0 = Instant::now();
    let mut empty_polls = 0u64;
    loop {
        let mut did_work = false;
        if let Some(cursor) = cursor.as_mut() {
            while let Some(idx) = cursor.pop(&ring) {
                shared.counters.pops.fetch_add(1, Ordering::Relaxed);
                did_work = true;
                let target = idx % shared.service_threads;
                if target == 0 {
                    dispatch(session, &shared.counters, &kind, idx, shared.payload);
                } else {
                    shared.fanout[target as usize].push(idx);
                }
            }
        } else if let Some(idx) = shared.fanout[index as usize].pop() {
            did_work = true;
            dispatch(session, &shared.counters, &kind, idx, shared.payload);
        }

        if did_work {
            empty_polls = 0;
            continue;
        }
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        empty_polls += 1;
        if empty_polls < 4096 {
            std::hint::spin_loop();
            continue;
        }
        if cursor.is_some() {
            // Park protocol (§5.3 rules 1+3): parked flag up → note the
            // doorbell token → disarm → mandatory rescan → bounded wait.
            session.header.daemon_parked.store(1, Ordering::SeqCst);
            let token = session.header.doorbell.load(Ordering::SeqCst);
            session.header.doorbell_coalescer.disarm();
            let tail_now = session.ring_tail.load(Ordering::SeqCst);
            let maybe_nonempty = tail_now != last_parked_tail;
            last_parked_tail = tail_now;
            if !maybe_nonempty {
                shared
                    .counters
                    .doorbell_waits
                    .fetch_add(1, Ordering::Relaxed);
                // Escalating 50 µs → 5 ms (D18); §5.3.1 rule 5 bounds
                // every park on client-writable memory.
                let step = (empty_polls - 4096).min(100);
                let timeout = Duration::from_micros(50 * (1 + step)).min(Duration::from_millis(5));
                if !futex_wait(&session.header.doorbell, token, Some(timeout)) {
                    shared
                        .counters
                        .doorbell_wait_timeouts
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            session.header.daemon_parked.store(0, Ordering::SeqCst);
        } else {
            std::thread::yield_now();
        }
    }
    (thread_cpu_ns() - cpu0, wall0.elapsed().as_nanos() as u64)
}

fn dispatch(
    session: &'static Session,
    counters: &Arc<DaemonCounters>,
    kind: &ServiceKind,
    idx: u32,
    payload: usize,
) {
    match kind {
        ServiceKind::Echo => serve_slot(session, counters, None, idx, payload),
        ServiceKind::Serve(s) => serve_slot(session, counters, Some(s), idx, payload),
        ServiceKind::Handoff(handle) => {
            // The demote path: one task per op onto the runtime (§5.5.1
            // "package the op as a future onto the existing runtime");
            // the spawn's task wake + scheduling IS the adder this leg
            // prices. Payload work + completion post run in the task,
            // parallel across runtime workers.
            let counters = Arc::clone(counters);
            handle.spawn(async move {
                serve_slot(session, &counters, None, idx, payload);
            });
        }
    }
}

// ---------------------------------------------------------------------------
// client role
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ClientCounters {
    doorbell_wakes: AtomicU64,
    slot_waits: AtomicU64,
    slot_wait_early_returns: AtomicU64,
    spin_completions: AtomicU64,
    ring_full: AtomicU64,
    verify_failures: AtomicU64,
}

struct ClientCtx {
    session: &'static Session,
    args: Args,
    counters: Arc<ClientCounters>,
    serve_leg: bool,
}

/// One sync client thread: claim → fill → publish → push → doorbell →
/// bounded spin → two-phase park → consume → verify. Returns (measured
/// ops, measured-window wall ns, latencies).
fn client_thread(ctx: &ClientCtx, thread_index: u32) -> (u64, u64, Vec<u64>) {
    let session = ctx.session;
    let args = &ctx.args;
    let ring = session.ring();
    let max_op = session.max_op();
    let payload = args.payload as usize;
    let slots_per_thread = session.slots.len() as u32 / args.threads;
    assert!(slots_per_thread > 0, "more threads than slots");
    let base = thread_index * slots_per_thread;
    let mut user_buf = vec![0u8; payload];
    let mut latencies = Vec::with_capacity((args.ops - args.warmup) as usize);
    let spin = Duration::from_nanos(args.spin_ns);
    let mut window_start: Option<Instant> = None;
    let mut measured = 0u64;

    for op in 0..args.ops {
        if op == args.warmup {
            window_start = Some(Instant::now());
        }
        let slot_idx = base + (op % u64::from(slots_per_thread)) as u32;
        let slot = &session.slots[slot_idx as usize];
        let t0 = Instant::now();

        let gen = loop {
            match slot.core.try_claim() {
                Some(g) => break g,
                None => std::hint::spin_loop(),
            }
        };
        // Request payload: user buf → arena (the app-side transport copy).
        let seed = (u64::from(thread_index) << 40) | (op & 0xff_ffff_ffff);
        fill_pattern(user_buf.as_mut_ptr(), payload, seed);
        let slab = session.arena.slab(slot_idx as usize, max_op);
        // SAFETY: disjoint slab per slot; claim exclusivity serializes.
        unsafe { std::ptr::copy_nonoverlapping(user_buf.as_ptr(), slab, payload) };
        slot.publish_descriptor(&SlotDescriptor {
            op: OP_READ,
            flags: 0,
            binding: u64::from(thread_index) + 1,
            offset: seed, // the request seed rides the offset field
            len: args.payload,
            arena_off: (slot_idx as usize * max_op) as u64,
        });
        slot.core.publish_submitted();
        while !ring.push(slot_idx) {
            ctx.counters.ring_full.fetch_add(1, Ordering::Relaxed);
            std::hint::spin_loop();
        }
        if doorbell_ring(session.header) {
            ctx.counters.doorbell_wakes.fetch_add(1, Ordering::Relaxed);
        }

        // Bounded spin window, then the two-phase park.
        let spin_start = Instant::now();
        let mut done = false;
        while spin_start.elapsed() < spin {
            if slot.core.is_done_for(gen) {
                done = true;
                ctx.counters
                    .spin_completions
                    .fetch_add(1, Ordering::Relaxed);
                break;
            }
            std::hint::spin_loop();
        }
        while !done {
            match slot.core.park_prepare() {
                ParkOutcome::Ready => done = true,
                ParkOutcome::Park { expected } => {
                    ctx.counters.slot_waits.fetch_add(1, Ordering::Relaxed);
                    if !futex_wait(
                        slot.core.state_futex_word(),
                        expected,
                        Some(Duration::from_secs(1)),
                    ) {
                        ctx.counters
                            .slot_wait_early_returns
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    if slot.core.is_done_for(gen) {
                        done = true;
                    }
                }
            }
        }

        // Consume: result + response payload (arena → user buf).
        let result = slot.result();
        // SAFETY: disjoint slab; ordering via is_done_for's Acquire.
        unsafe { std::ptr::copy_nonoverlapping(slab, user_buf.as_mut_ptr(), payload) };
        slot.core.release();

        if op >= args.warmup {
            latencies.push(t0.elapsed().as_nanos() as u64);
            measured += 1;
        }

        // Verification: the result must echo the request sum (proves the
        // daemon read the request), the response must carry the expected
        // fill (proves the response write). --verify = every op; default
        // = sampled.
        if args.verify || op & 1023 == 0 {
            let mut ok = result as u64 == expected_sum(seed, payload);
            ok &= if ctx.serve_leg {
                check_pattern(user_buf.as_ptr(), payload, TIER_SEED)
            } else {
                check_pattern(user_buf.as_ptr(), payload, response_seed(seed))
            };
            if !ok {
                ctx.counters.verify_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let wall = window_start.map_or(0, |w| w.elapsed().as_nanos() as u64);
    (measured, wall, latencies)
}

fn client_main(args: &Args) -> Result<(), String> {
    let socket_name = args.socket.as_deref().ok_or("--socket required")?;
    let stream = abstract_connect(socket_name)?;
    let sock = stream.as_raw_fd();
    send_msg(sock, b"HELLO", None)?;
    let mut buf = [0u8; 128];
    let (n, fd) = recv_msg(sock, &mut buf)?;
    if &buf[..n] != b"SESSION" {
        return Err(format!("expected SESSION, got {:?}", &buf[..n]));
    }
    let memfd = fd.ok_or("SESSION carried no fd")?;
    let session = Session::from_fd(memfd.as_raw_fd(), rig_geometry(args), false)?;

    let ctx = Arc::new(ClientCtx {
        session,
        args: args.clone(),
        counters: Arc::new(ClientCounters::default()),
        serve_leg: args.leg == "serve",
    });

    let mut handles = Vec::new();
    for t in 0..args.threads {
        let ctx = Arc::clone(&ctx);
        handles.push(std::thread::spawn(move || {
            let out = client_thread(&ctx, t);
            (out, thread_cpu_ns())
        }));
    }
    let mut measured_total = 0u64;
    let mut wall_max = 0u64;
    let mut cpu_total = 0u64;
    let mut lat_all: Vec<u64> = Vec::new();
    for h in handles {
        let ((measured, wall, lat), cpu) = h.join().map_err(|_| "client thread panicked")?;
        measured_total += measured;
        wall_max = wall_max.max(wall);
        cpu_total += cpu;
        lat_all.extend(lat);
    }

    lat_all.sort_unstable();
    let pct = |p: f64| -> u64 {
        if lat_all.is_empty() {
            0
        } else {
            lat_all[((lat_all.len() as f64 * p) as usize).min(lat_all.len() - 1)]
        }
    };
    let mean = if lat_all.is_empty() {
        0
    } else {
        lat_all.iter().sum::<u64>() / lat_all.len() as u64
    };
    let c = &ctx.counters;
    println!(
        "CSTATS measured_ops={} measured_wall_ns={} rtt_p50_ns={} rtt_p90_ns={} rtt_p99_ns={} \
         rtt_mean_ns={} client_syscalls={} doorbell_wakes={} slot_waits={} early_returns={} \
         spin_completions={} ring_full={} verify_failures={} client_threads_cpu_ns={}",
        measured_total,
        wall_max,
        pct(0.50),
        pct(0.90),
        pct(0.99),
        mean,
        c.doorbell_wakes.load(Ordering::Relaxed) + c.slot_waits.load(Ordering::Relaxed),
        c.doorbell_wakes.load(Ordering::Relaxed),
        c.slot_waits.load(Ordering::Relaxed),
        c.slot_wait_early_returns.load(Ordering::Relaxed),
        c.spin_completions.load(Ordering::Relaxed),
        c.ring_full.load(Ordering::Relaxed),
        c.verify_failures.load(Ordering::Relaxed),
        cpu_total,
    );
    send_msg(sock, b"DONE", None)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// orchestrator (daemon role + child management)
// ---------------------------------------------------------------------------

fn orchestrate(args: &Args) -> Result<(), String> {
    let socket_name = format!(
        "squeezefs-ipc-rig-{}-{:x}",
        std::process::id(),
        thread_cpu_ns()
    );
    let listener = abstract_listen(&socket_name)?;

    // The second REAL process (client role), same binary.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(["--role", "client", "--socket", &socket_name])
        .args(["--leg", &args.leg])
        .args(["--threads", &args.threads.to_string()])
        .args(["--ops", &args.ops.to_string()])
        .args(["--warmup", &args.warmup.to_string()])
        .args(["--payload", &args.payload.to_string()])
        .args(["--spin-ns", &args.spin_ns.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if args.verify {
        cmd.arg("--verify");
    }
    let mut child: Child = cmd.spawn().map_err(|e| e.to_string())?;

    let (stream, _) = listener.accept().map_err(|e| e.to_string())?;
    let sock = stream.as_raw_fd();
    let mut buf = [0u8; 128];
    let (n, _) = recv_msg(sock, &mut buf)?;
    if &buf[..n] != b"HELLO" {
        let _ = child.kill();
        return Err(format!("expected HELLO, got {:?}", &buf[..n]));
    }

    let daemon_cpu0 = process_cpu_ns();
    // The real sealed-memfd session.
    let geometry = rig_geometry(args);
    let layout = SessionLayout::compute(&geometry).map_err(|e| e.to_string())?;
    let memfd = create_sealed_memfd(layout.total_bytes)?;
    let session = Session::from_fd(memfd.as_raw_fd(), geometry, true)?;

    let payload = args.payload as usize;
    let counters = Arc::new(DaemonCounters::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let fanout: Arc<Vec<SpscQueue>> = Arc::new(
        (0..args.service_threads as usize)
            .map(|_| SpscQueue::new(session.slots.len()))
            .collect(),
    );
    let shared = Arc::new(ServiceShared {
        session,
        counters: Arc::clone(&counters),
        shutdown: Arc::clone(&shutdown),
        fanout,
        payload,
        service_threads: args.service_threads,
    });

    // Handoff leg: the embedded minimal runtime (workers = --runtime-
    // workers); ops become per-op tasks (see dispatch).
    let mut runtime = None;
    let handoff_handle = if args.leg == "handoff" {
        // global_queue_interval(1): remote spawns land on the inject
        // queue; hot workers must check it every poll or a busy runtime
        // adds queue-check latency to every handoff (the §5.8.4 handoff
        // row's runtime is also PINNED — dedicated CPUs, no migration).
        let workers = args.runtime_workers as usize;
        let pin_base = AtomicU32::new(0);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .global_queue_interval(1)
            .on_thread_start(move || {
                let i = pin_base.fetch_add(1, Ordering::Relaxed);
                pin_thread_to_cpu(i);
            })
            .build()
            .map_err(|e| e.to_string())?;
        let handle = rt.handle().clone();
        runtime = Some(rt);
        Some(handle)
    } else {
        None
    };
    let serve_shaped = (args.leg == "serve").then(|| ServeShaped::build(payload));

    let mut service_handles = Vec::new();
    for i in 0..args.service_threads {
        let shared = Arc::clone(&shared);
        let kind = match args.leg.as_str() {
            "echo" => ServiceKind::Echo,
            "serve" => ServiceKind::Serve(Arc::clone(
                serve_shaped.as_ref().expect("serve leg builds the maps"),
            )),
            _ => ServiceKind::Handoff(
                handoff_handle
                    .clone()
                    .expect("handoff leg builds the runtime"),
            ),
        };
        service_handles.push(std::thread::spawn(move || service_thread(shared, kind, i)));
    }
    drop(handoff_handle);

    // Hand the session over.
    send_msg(sock, b"SESSION", Some(memfd.as_raw_fd()))?;

    // Optional client kill (lifecycle smoke).
    if let Some(ms) = args.kill_client_after_ms {
        std::thread::sleep(Duration::from_millis(ms));
        let _ = child.kill();
    }

    // Wait for DONE or EOF (client death — the §5.7 detection shape).
    let (n, _) = recv_msg(sock, &mut buf)?;
    let done = n > 0 && &buf[..n] == b"DONE";

    shutdown.store(true, Ordering::Release);
    let mut service_cpu = Vec::new();
    for h in service_handles {
        service_cpu.push(h.join().map_err(|_| "service thread panicked")?);
    }
    if let Some(rt) = runtime {
        rt.shutdown_timeout(Duration::from_secs(1));
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    let mut child_out = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut child_out);
    }
    if !done || !status.success() {
        eprintln!(
            "ipc_hop_rig: client did not complete cleanly (done={done}, status={status}); \
             daemon observed EOF and tore down bounded"
        );
        return Err("client failure".into());
    }
    let cstats = child_out
        .lines()
        .find(|l| l.starts_with("CSTATS "))
        .map(str::to_string)
        .ok_or("client emitted no CSTATS")?;
    let get = |key: &str| -> u64 {
        cstats
            .split_whitespace()
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };

    let measured_ops = get("measured_ops").max(1);
    let wall_ns = get("measured_wall_ns").max(1);
    let daemon_cpu_ns = process_cpu_ns() - daemon_cpu0;
    let served_total = counters.served.load(Ordering::Relaxed).max(1);
    let daemon_syscalls = counters.slot_wakes.load(Ordering::Relaxed)
        + counters.doorbell_waits.load(Ordering::Relaxed);
    let service_cpu_total: u64 = service_cpu.iter().map(|(c, _)| *c).sum();
    let ops_per_sec = measured_ops as f64 / (wall_ns as f64 / 1e9);

    for (i, (cpu, wall)) in service_cpu.iter().enumerate() {
        println!(
            "SERVICE index={i} cpu_ns={cpu} wall_ns={wall} occupancy_pct={:.1}",
            *cpu as f64 / (*wall).max(1) as f64 * 100.0
        );
    }
    println!(
        "SUMMARY leg={} threads={} service_threads={} runtime_workers={} ops={} \
         measured_ops={measured_ops} ops_per_sec={ops_per_sec:.0} \
         rtt_p50_ns={} rtt_p90_ns={} rtt_p99_ns={} rtt_mean_ns={} \
         client_syscalls_per_op={:.4} daemon_syscalls_per_op={:.4} \
         client_doorbell_wakes={} client_slot_waits={} client_spin_completions={} \
         daemon_slot_wakes={} daemon_doorbell_waits={} daemon_wait_timeouts={} \
         daemon_pops={} daemon_served={} serve_rejects={} tier_hits={} ring_full={} \
         verify_failures={} service_cpu_ns_total={service_cpu_total} \
         daemon_process_cpu_ns={daemon_cpu_ns} daemon_cpu_ns_per_op={:.0} \
         client_threads_cpu_ns={} payload={} spin_ns={}",
        args.leg,
        args.threads,
        args.service_threads,
        if args.leg == "handoff" {
            args.runtime_workers
        } else {
            0
        },
        args.ops,
        get("rtt_p50_ns"),
        get("rtt_p90_ns"),
        get("rtt_p99_ns"),
        get("rtt_mean_ns"),
        get("client_syscalls") as f64 / measured_ops as f64,
        daemon_syscalls as f64 / measured_ops as f64,
        get("doorbell_wakes"),
        get("slot_waits"),
        get("spin_completions"),
        counters.slot_wakes.load(Ordering::Relaxed),
        counters.doorbell_waits.load(Ordering::Relaxed),
        counters.doorbell_wait_timeouts.load(Ordering::Relaxed),
        counters.pops.load(Ordering::Relaxed),
        counters.served.load(Ordering::Relaxed),
        counters.serve_rejects.load(Ordering::Relaxed),
        counters.stat_tier_hits.load(Ordering::Relaxed),
        get("ring_full"),
        get("verify_failures"),
        daemon_cpu_ns as f64 / served_total as f64,
        get("client_threads_cpu_ns"),
        args.payload,
        args.spin_ns,
    );

    if get("verify_failures") != 0 {
        return Err("payload verification failed".into());
    }
    // Sanity: the daemon must have served every op the client issued
    // (--ops is per client thread, warmup included) — a silent no-op
    // daemon must not pass.
    let expected_total = args.ops * u64::from(args.threads);
    if counters.served.load(Ordering::Relaxed) < expected_total {
        return Err(format!(
            "daemon served {} < expected {expected_total}",
            counters.served.load(Ordering::Relaxed),
        ));
    }
    Ok(())
}

fn main() {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ipc_hop_rig: {e}");
            eprintln!(
                "usage: ipc_hop_rig --leg echo|serve|handoff [--threads N] \
                 [--service-threads N] [--runtime-workers N] [--ops N] [--warmup N] \
                 [--payload BYTES] [--spin-ns NS] [--verify] [--kill-client-after-ms MS]"
            );
            std::process::exit(2);
        }
    };
    let result = match args.role.as_deref() {
        Some("client") => client_main(&args),
        Some(other) => Err(format!("unknown role {other}")),
        None => orchestrate(&args),
    };
    if let Err(e) = result {
        eprintln!("ipc_hop_rig: {e}");
        std::process::exit(1);
    }
}
