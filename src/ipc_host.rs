//! L4 IPC **session host** — the daemon side of the LD_PRELOAD
//! interception control plane (`docs/design-preload-interception.md` §5.2,
//! §5.3, §5.7; PR L4-3).
//!
//! One [`IpcHost`] per interception-enabled mount: an abstract AF_UNIX
//! `SOCK_SEQPACKET` listener (zero filesystem residue), per-connection ctl
//! threads speaking [`squeezefs_ipc::wire`], sealed-memfd shm sessions
//! ([`squeezefs_ipc::layout`]), and a service thread that drains session
//! rings. **No data plane lives here**: the one ring op this host serves is
//! `OP_ECHO` (the liveness/round-trip ping); correctly-directed READ/WRITE
//! descriptors are validated, direction-checked, and handed to the
//! [`SessionSink`] — PR L4-3's [`EchoSessionSink`] completes them
//! `-ENOSYS`, PR L4-4's service sink serves them for real.
//!
//! ## The §5.2 daemon fd screen (normative — THE security boundary)
//!
//! Any process can speak this socket protocol directly; the shim's bail-out
//! ladder is an optimization, never a substitute. Every received fd runs
//! the screen (`IpcHost::screen_fd`), in order, all daemon-side:
//!
//! 1. `F_GETFL`: reject any description carrying `O_PATH` — load-bearing,
//!    not belt-and-braces: an `O_PATH` fd is obtainable with *search-only*
//!    permission and its access-mode bits read `O_RDONLY` (0), so a naive
//!    mode check would grant ring reads of a file the uid cannot read
//!    (cross-user disclosure on `--allow-other` mounts).
//! 2. `fstat`: `st_mode` must be `S_ISREG` — directories, device nodes,
//!    FIFOs, sockets refuse (class `flags`).
//! 3. `st_dev` must equal this mount's device (class `mode` — the fd is
//!    not a rights-bearing capability on this mount; also refused while
//!    the mount device is not resolved yet).
//! 4. Status flags: `O_APPEND` (append needs an atomic size authority
//!    round trip — v1 refuses), `O_SYNC`/`O_DSYNC` (per-op durable
//!    barriers — the kernel path already provides), and the
//!    `O_TMPFILE`-class (unnamed regular file: the `O_TMPFILE` bits when
//!    the kernel exposes them, plus `st_nlink == 0` — which also refuses
//!    open-then-unlinked fds, conservatively correct: passthrough serves
//!    them) all refuse (class `flags`).
//! 5. Access mode ⇒ per-op rights **both directions**: `O_RDONLY` binding
//!    ⇒ ring writes refused, `O_WRONLY` binding ⇒ ring reads refused —
//!    surfaced as `EBADF` per op, matching the kernel for the
//!    wrong-direction op on that fd.
//!
//! Session-level gates ahead of the fd screen: KD-7 version lock
//! (`IPC_ABI` + `build_commit` equality, `unknown`/`-dirty` degenerate
//! identities refused on either side unless the counted dev override is
//! set), nonce freshness (current + immediately-previous, ~60 s TTL,
//! multi-use within it), and `SO_PEERCRED` defense-in-depth (claimed
//! pid/uid must match the kernel's — the fd stays the authorizer).
//!
//! ## Untrusted shm discipline (§5.3.1)
//!
//! Everything in the session mapping is client-writable for the session's
//! life. The drain snapshots each descriptor exactly once
//! ([`squeezefs_ipc::layout::IpcSlot::snapshot_descriptor`] after
//! `try_begin_serve`), validates the copy, serves from the copy; ECHO's
//! derived value (the byte sum) is computed from a **severed private
//! copy** of the payload (rule 2 — never a second arena read); every wait
//! that involves a client-writable word is timeout-bounded (rule 5);
//! teardown orders the `munmap` after the last accessor structurally (the
//! mapping lives in an `Arc` the service thread clones per pass — rule 4).
//! Impossible slot/ring observations (an index at a non-SUBMITTED slot,
//! an out-of-range index) poison the session loudly per §5.3 rule 4.

use crate::fuse_client::METRICS;
use squeezefs_ipc::layout::{
    Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_ECHO, OP_READ, OP_WRITE,
};
use squeezefs_ipc::ring_core::{MpscRingView, RingCell, RingConsumer};
use squeezefs_ipc::wire::{
    build_commit_degenerate, BootstrapBlob, CtlMsg, RefuseClass, CTL_MSG_MAX, NONCE_LEN,
};

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Nonce TTL (§5.2 nonce lifecycle): one nonce per daemon instance,
/// rotated on this cadence, current + immediately-previous accepted.
const NONCE_TTL: Duration = Duration::from_secs(60);

/// Service-thread park bound (§5.3.1 rule 5 / the D18 ladder cap): no
/// daemon wait on a client-writable word ever exceeds this.
const SERVICE_PARK_MAX: Duration = Duration::from_millis(5);

/// Empty-pass spin window before a service thread parks (time-based —
/// the §5.5.1 "spin → short wait" ladder's spin rung, restored; knob
/// `SQUEEZEFS_IPC_SPIN_US`, default 0 — see below). The former 64 bare `spin_loop`
/// hints were sub-µs — smaller than ANY per-session inter-arrival gap
/// once sessions spread across threads (13–55 µs on the 2026-07-26
/// fabric-rig shapes), so every burst paid a full doorbell park/wake
/// cycle: the measured sessions inversion (svc voluntary context
/// switches 32k → 394k /s from sessions=1 → 8 at one offered load).
/// The default is 0 — the window ships as an explicit fleet lever,
/// not an ambient tax: every nonzero setting measured on the sizing
/// grid bought its libaio-fleet gain (+3–5 % at 20 µs, more at 100 µs)
/// by stealing runnable-tokio CPU from the sync lane (−4 % at 20 µs,
/// −22 % at 100 µs with 8 service threads; evidence note §5–6), and
/// the sync lane is a protected row. The multi-session/fleet wins that
/// ship by default come from the event-driven reap + the epoch-cached
/// snapshot; libaio-only fleets raise this knob for the rest.
fn service_spin_window() -> Duration {
    service_spin_window_from(std::env::var("SQUEEZEFS_IPC_SPIN_US").ok().as_deref())
}

/// Pure sizing form (unit-pinned): the 2026-07-26 reap-economy A/B
/// adjudicated the DEFAULT as 0 (`1508ea9` — every nonzero ambient
/// window taxed the protected sync lane), but the landed code kept the
/// sweep side's 30 µs — a doc-code divergence this pin closes.
fn service_spin_window_from(v: Option<&str>) -> Duration {
    let us = v
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        .clamp(0, 10_000);
    Duration::from_micros(us)
}

#[cfg(test)]
mod spin_window_tests {
    use super::*;

    /// The adjudicated default (reap-economy note §5–6, commit
    /// `1508ea9`): **0** — the spin window is an explicit fleet lever,
    /// never an ambient tax on the sync lane.
    #[test]
    fn spin_window_default_is_zero() {
        assert_eq!(
            service_spin_window_from(None),
            Duration::ZERO,
            "the adjudicated SQUEEZEFS_IPC_SPIN_US default is 0 (an explicit \
             fleet lever, not an ambient tax) — 1508ea9 landed the verdict in \
             the doc comment but not the code"
        );
    }

    /// Explicit settings are honored verbatim within the clamp.
    #[test]
    fn spin_window_explicit_and_clamped() {
        assert_eq!(
            service_spin_window_from(Some("20")),
            Duration::from_micros(20)
        );
        assert_eq!(
            service_spin_window_from(Some("999999")),
            Duration::from_micros(10_000),
            "clamp ceiling"
        );
        assert_eq!(
            service_spin_window_from(Some("garbage")),
            service_spin_window_from(None),
            "unparseable falls back to the default"
        );
    }
}

/// IPC service-thread count (§5.5.1; knob
/// `SQUEEZEFS_IPC_SERVICE_THREADS`, clamp 1..=64). Default scales with
/// the box: `clamp(cpus/4, 2, 8)` — the 2026-07-19 sweep measured the
/// warm il row's ceiling as exactly this count (2 threads = 643 k
/// IOPS, 8 threads = 1.48 M on a 32-CPU box; fast-path serves execute
/// ON these threads), and 8 saturated it.
fn service_thread_count() -> usize {
    std::env::var("SQUEEZEFS_IPC_SERVICE_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, 64))
        .unwrap_or_else(|| {
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8);
            (cpus / 4).clamp(2, 8)
        })
}

/// Host configuration (mount-time; tests construct directly).
#[derive(Debug, Clone)]
pub struct IpcHostConfig {
    /// Abstract AF_UNIX socket name (no leading NUL — added on the wire).
    pub socket_name: String,
    /// OQ-6 (v1.1): when set, ALSO bind a filesystem-path
    /// `SOCK_SEQPACKET` socket at `<dir>/<socket_name>.sock` and
    /// advertise it in the blob's reserved `socket_path` field —
    /// abstract names are per network namespace, so containerized apps
    /// with the mount bind-mounted in need a path rendezvous. The path
    /// socket grants nothing the abstract one does not (SO_PEERCRED +
    /// the §5.2 fd screen remain the boundary; the file is 0666
    /// BECAUSE connecting is not a credential). Bind failure degrades
    /// loudly to abstract-only — never fails the spawn.
    pub socket_dir: Option<std::path::PathBuf>,
    /// This daemon's build-commit identity (`src/version.rs` form) — the
    /// KD-7 skew gate key.
    pub build_commit: String,
    /// `SQUEEZEFS_IPC_ALLOW_DEV=1`: admit degenerate (`unknown`/`-dirty`)
    /// identity pairs, counted in `ipc_binds_dev_override`.
    pub allow_dev: bool,
    /// Session shm geometry (validated at spawn).
    pub geometry: Geometry,
    /// Total session-shm byte cap across live sessions (the R5
    /// `ipc_session_arenas` admission bound — `mem_budget::ipc_arena_cap`).
    pub arena_cap_bytes: u64,
    /// Per-uid concurrent session cap (DoS posture §11).
    pub per_uid_session_cap: usize,
    /// Idle-session reap bound in seconds (§5.7 row 2): a session with
    /// no ring/ctl activity for this long is torn down with a
    /// generation bump (the client observes poison and lazily
    /// re-establishes). `0` disables the reaper (tests; production
    /// wires `SQUEEZEFS_IPC_IDLE_SECS`, default 300).
    pub idle_secs: u64,
    /// VL2 (design-volume-lifecycle §5.1.4): whether data-plane
    /// sessions are admitted. `false` = control-plane-only (the
    /// every-mount posture without `-o interception`): the listener +
    /// ctl threads run, ADMIN sessions work, but data-plane HELLOs
    /// refuse before any fd screen — no shm sessions, no arenas.
    pub data_plane: bool,
    /// The mount-owning uid: the ADMIN lane admits peercred uid 0 or
    /// this uid, nothing else.
    pub owner_uid: u32,
}

/// The admin-verb handler behind the ADMIN lane (VL2 §5.1.4). Wired
/// post-spawn (`set_admin_sink`); admin requests refuse until it is.
/// Implementations run on host ctl threads (plain OS threads) and may
/// block on their own runtime handle.
pub trait AdminSink: Send + Sync + 'static {
    /// Execute one verb; returns `(ok, body)` — body is verb-specific
    /// JSON, or the error text when `!ok`.
    fn handle(&self, verb: &str, arg: &str) -> (bool, String);
}

/// Per-op rights derived from the screened fd's access mode (§5.2 rule 3),
/// plus the description's **read class** (2026-07-25 ipc-miss-path fix):
/// the kernel path hands the routing layer the description's O_DIRECT bit
/// on every READ (`fuse_read_in.flags` → `ReadClassHint`); ring ops must
/// carry the same class or an O_DIRECT app under the shim silently
/// reclassifies to buffered admission behavior.
#[derive(Debug, Clone, Copy)]
pub struct BindingRights {
    pub ino: u64,
    pub read_ok: bool,
    pub write_ok: bool,
    /// The screened description carried O_DIRECT at bind time. (An
    /// F_SETFL toggling O_DIRECT later unbinds shim-side — the class is
    /// per-description and captured once, like the rights.)
    pub odirect: bool,
    /// Killpriv-v2 il parity (2026-07-28 campaign): the session peer's
    /// privilege class — `true` ⇒ ring writes on this binding carry the
    /// daemon's `FUSE_WRITE_KILL_SUIDGID` clearing obligation (suid /
    /// group-exec sgid / security.capability), mirroring the kernel
    /// path's per-write `!capable(CAP_FSETID)`. Computed ONCE at HELLO
    /// from the SO_PEERCRED-verified identity ([`peer_kill_priv`]) —
    /// a documented approximation: the kernel samples the writing
    /// task's capability per syscall, the ring samples the session
    /// peer's at establishment (a client changing CAP_FSETID
    /// mid-session keeps its HELLO-time class until it reconnects).
    pub kill_priv: bool,
}

/// A validated data-plane op (direction-checked, bounds-checked) handed to
/// the [`SessionSink`].
pub struct DataOp {
    pub desc: SlotDescriptor,
    pub binding: BindingRights,
    /// The validated arena window `[arena_off, arena_off + len)` —
    /// **client-writable memory** (§5.3.1: single-read discipline; derived
    /// values from a severed copy only).
    pub payload: ArenaWindow,
}

/// The completion handle for one dequeued op (PR L4-4): owns everything
/// needed to publish the result from **any** thread — the sync fast path
/// completes it on the service thread, the async handoff from a tokio
/// worker after the parked handler finishes. The embedded mapping `Arc`
/// keeps the shm alive until the completion lands (§5.3.1 rule 4:
/// teardown's `munmap` is ordered after the last accessor structurally),
/// so a completion racing session teardown writes into live private
/// memory, never a freed mapping.
pub struct SlotCompletion {
    map: Arc<SessionMapping>,
    slot_index: u32,
}

impl SlotCompletion {
    /// Publish `result` (`bytes` or `-errno`) and wake a parked client.
    /// Consumes the handle: exactly one completion per dequeued op.
    pub fn complete(self, result: i64) {
        let slot = self
            .map
            .slot(self.slot_index)
            .expect("slot index validated at dequeue");
        slot.set_result(result);
        if slot.core.complete() {
            // Client(s) parked on this slot's state word: wake them ALL
            // (cross-process futex — never FUTEX_PRIVATE). Breadth is
            // load-bearing since the 2026-07-26 event-driven reap: a
            // split submitter/reaper pair may BOTH park on one in-flight
            // op's word (each re-snapshots the same pending set), and a
            // single-waiter wake strands the loser for its full bound —
            // pinned by `slot_completion_wakes_every_parked_waiter`.
            futex_wake(slot.core.state_futex_word(), i32::MAX);
        }
        // Completion doorbell (op-economy 2026-07-28): bump the session
        // cqe seq; pay the wake ONLY toward a parked reaper (the
        // `ipc_cqe_doorbell_*` loom-verified elision — an unparked
        // reaping client costs zero completion wake syscalls, the former
        // per-completion collect-and-wake serialization term). Breadth
        // i32::MAX: every parked reaper on the session re-scans.
        let cqe = &self.map.header().cqe;
        if cqe.complete() {
            METRICS.ipc_cqe_wake_writes.fetch_add(1, Ordering::Relaxed);
            futex_wake(cqe.seq_word(), i32::MAX);
        } else {
            METRICS.ipc_cqe_wake_elided.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The data-plane sink: receives validated READ/WRITE ops from the drain,
/// each with its [`SlotCompletion`]. The sink decides *where* the op
/// completes — synchronously on the service thread (the §5.5.1 fast path)
/// or from an async handoff — which is why the handle model replaced the
/// L4-3 return-value contract. [`EchoSessionSink`] completes `-ENOSYS`
/// inline; [`crate::ipc_service::DataPlaneSink`] serves for real.
pub trait SessionSink: Send + Sync + 'static {
    /// Serve one op; `completion` must be completed exactly once (`bytes`
    /// or `-errno`), from any thread.
    fn serve_data(&self, op: DataOp, completion: SlotCompletion);

    /// A BIND was granted on `ino` (§5.6.2 W1: the data plane fires the
    /// bind-time inode invalidation from here). Default: nothing — the
    /// host-isolation sink has no kernel cache to shoot down.
    fn on_bind(&self, _ino: u64) {}

    /// End-of-sweep hook: the service loop calls this once after every
    /// drain pass over its owned sessions. The direct-drive sink uses
    /// it to flush pushed-but-unsubmitted SQEs in ONE `io_uring_enter`
    /// per sweep (the submit-batch economy); the default is a no-op.
    /// LIVENESS RULE for implementors: any work deferred during
    /// `serve_data` MUST become kernel-visible here — the service
    /// thread may park for up to its bounded window right after.
    fn flush(&self) {}
}

/// The no-data-plane sink: every correctly-directed READ/WRITE completes
/// `-ENOSYS`. Kept as the host-isolation test sink (`tests/
/// ipc_host_tests.rs` exercises the §5.2 control plane without a
/// filesystem); production mounts wire `DataPlaneSink`.
#[derive(Debug, Default)]
pub struct EchoSessionSink;

impl SessionSink for EchoSessionSink {
    fn serve_data(&self, _op: DataOp, completion: SlotCompletion) {
        completion.complete(-libc::ENOSYS as i64);
    }
}

/// A bounds-validated window into a session's payload arena. Raw pointers
/// on purpose: the memory is shared with (and mutable by) an untrusted
/// process for the entire serve, so no `&[u8]`/`&mut [u8]` exclusivity or
/// immutability claim is ever made over it.
pub struct ArenaWindow {
    base: *mut u8,
    len: usize,
    /// Keeps the mapping alive for the window's lifetime (§5.3.1 rule 4).
    _map: Arc<SessionMapping>,
}

// SAFETY: the window is a bounds-checked view of a shared mapping whose
// lifetime is pinned by the `Arc`; all access is raw-pointer copies that
// tolerate concurrent client writes (torn bytes are the client's own
// POSIX-legal race, §5.3.1 rule 2).
unsafe impl Send for ArenaWindow {}
unsafe impl Sync for ArenaWindow {}

impl ArenaWindow {
    /// The §5.3.1 rule-2 severed copy: read the window exactly once into
    /// private memory. Every derived value is computed from this copy.
    pub fn read_severed(&self) -> Vec<u8> {
        let mut buf = vec![0u8; self.len];
        // SAFETY: `base..base+len` validated inside the arena at
        // construction; source may race (client-writable) — a byte-wise
        // copy of racing memory yields torn *content*, never UB here
        // (the region is plain shared memory mapped by this process).
        unsafe {
            std::ptr::copy_nonoverlapping(self.base, buf.as_mut_ptr(), self.len);
        }
        buf
    }

    /// The window's base pointer — the direct-drive DMA destination
    /// (`crate::ipc_direct`): raw on purpose, same non-exclusivity
    /// contract as the copies above. Alignment is the CALLER's screen
    /// (O_DIRECT-class DMA needs 4 KiB; unaligned windows bounce).
    pub(crate) fn as_base_ptr(&self) -> *mut u8 {
        self.base
    }

    /// Write `bytes` into the window (completion payloads).
    pub fn write(&self, bytes: &[u8]) {
        let n = bytes.len().min(self.len);
        // SAFETY: bounds as above; destination races are the client's own
        // concurrent-buffer-mutation POSIX hazard.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base, n);
        }
    }
}

/// The §5.5.1 serve-into-arena destination (op-economy campaign): tier
/// serve legs copy payload bytes straight into the validated window —
/// no intermediate heap buffer. Same non-exclusivity contract as
/// [`ArenaWindow::write`]; out-of-bounds spans clamp.
impl crate::PayloadSink for ArenaWindow {
    fn write_at(&self, off: usize, bytes: &[u8]) {
        let Some(room) = self.len.checked_sub(off) else {
            return;
        };
        let n = bytes.len().min(room);
        // SAFETY: `base + off .. base + off + n` stays inside the window
        // validated at construction; source is caller-private memory.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(off), n);
        }
    }

    fn zero_at(&self, off: usize, len: usize) {
        let Some(room) = self.len.checked_sub(off) else {
            return;
        };
        let n = len.min(room);
        // SAFETY: bounds as above.
        unsafe {
            std::ptr::write_bytes(self.base.add(off), 0, n);
        }
    }
}

impl ArenaWindow {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The daemon-side mapping of one session memfd. Unmapped on drop — which
/// is ordered after every accessor structurally (service passes and arena
/// windows clone the `Arc`; §5.3.1 rule 4).
struct SessionMapping {
    base: *mut u8,
    layout: SessionLayout,
    geometry: Geometry,
}

// SAFETY: the mapping is process-shared memory accessed only through
// atomics (header/ring/slots) and bounds-checked raw copies (arena); the
// base pointer itself is immutable after creation.
unsafe impl Send for SessionMapping {}
unsafe impl Sync for SessionMapping {}

impl SessionMapping {
    fn header(&self) -> &SessionHeader {
        // SAFETY: region 0 of a mapping sized/aligned by `SessionLayout`;
        // the daemon wrote the header at creation.
        unsafe { &*(self.base as *const SessionHeader) }
    }

    fn ring(&self) -> MpscRingView<'_> {
        // SAFETY: offsets computed by the same `SessionLayout` that sized
        // the mapping; the types are the repr(C) shm protocol types.
        unsafe {
            let tail = &*(self.base.add(self.layout.ring_off as usize) as *const AtomicU32);
            let cells = std::slice::from_raw_parts(
                self.base.add(self.layout.ring_cells_off as usize) as *const RingCell,
                self.geometry.ring_entries as usize,
            );
            MpscRingView::from_parts(tail, cells).expect("session geometry validated at admission")
        }
    }

    fn slot(&self, index: u32) -> Option<&IpcSlot> {
        if index >= self.geometry.slots {
            return None;
        }
        // SAFETY: index bounds-checked against the slot count the layout
        // sized; slots are repr(C, align(128)) at `slots_off`.
        unsafe {
            Some(
                &*((self.base.add(self.layout.slots_off as usize) as *const IpcSlot)
                    .add(index as usize)),
            )
        }
    }

    fn arena_window(self: &Arc<Self>, off: u64, len: u32) -> Option<ArenaWindow> {
        let len64 = u64::from(len);
        if off.checked_add(len64)? > self.geometry.arena_bytes {
            return None;
        }
        // SAFETY: `arena_off + off + len ≤ total_bytes` by the check above
        // plus the layout arithmetic (arena is the final region).
        let base = unsafe { self.base.add((self.layout.arena_off + off) as usize) };
        Some(ArenaWindow {
            base,
            len: len as usize,
            _map: Arc::clone(self),
        })
    }
}

impl Drop for SessionMapping {
    fn drop(&mut self) {
        // SAFETY: unmapping the mapping created in `IpcSession::establish`;
        // drop order is after every accessor by Arc ownership.
        unsafe {
            libc::munmap(
                self.base as *mut libc::c_void,
                self.layout.total_bytes as usize,
            );
        }
    }
}

/// One live session (per client process).
struct IpcSession {
    id: u64,
    uid: u32,
    /// Peer privilege class for the killpriv-v2 write obligation
    /// (`BindingRights::kill_priv` copies this per BIND) — computed at
    /// HELLO from the SO_PEERCRED identity via [`peer_kill_priv`].
    kill_priv: bool,
    /// The §5.5.1 session-ownership invariant: pinned to exactly ONE
    /// service thread at admission, for the session's whole lifetime (the
    /// `ipc_ring_core` single-consumer precondition). No drain handoff, no
    /// live rebalance — new sessions admit to the lightest thread.
    owner: usize,
    map: Arc<SessionMapping>,
    /// Session shm footprint charged to the gauge (the full mapping —
    /// header + rings + slots + stats + arena).
    charged_bytes: u64,
    /// Daemon-issued binding id → rights. Written by the ctl thread,
    /// read by the service thread — latch-free (`scc`).
    bindings: scc::HashMap<u64, BindingRights>,
    /// The single consumer cursor (daemon-private on purpose — §5.3.2
    /// single-consumer precondition). Only the owning service thread
    /// locks it (the mutex documents exclusivity; it is never contended).
    consumer: Mutex<RingConsumer>,
    /// Ctl socket (service side keeps it only to shut it down at
    /// teardown/poison; the ctl thread owns the I/O).
    sock: Arc<UnixStream>,
    /// Milliseconds-since-host-start of the last ring/ctl activity —
    /// the §5.7 idle-reap clock. Served ops and BINDs refresh it.
    last_active_ms: AtomicU64,
    torn_down: AtomicBool,
}

impl IpcSession {
    /// Drain + serve everything currently published on this session's
    /// ring. Returns ops served, or `None` when the session must be
    /// poisoned (§5.3 rule 4: impossible ring/slot observation).
    fn drain(&self, sink: &Arc<dyn SessionSink>) -> Option<u32> {
        let mut served = 0u32;
        let mut consumer = self
            .consumer
            .lock()
            .expect("ring consumer mutex never poisons (no panics under it)");
        let ring = self.map.ring();
        while let Some(index) = consumer.pop(&ring) {
            let Some(slot) = self.map.slot(index) else {
                log::error!(
                    "ipc session {}: ring published out-of-range slot index {index} — \
                     poisoning session (§5.3 rule 4)",
                    self.id
                );
                return None;
            };
            if !slot.core.try_begin_serve() {
                log::error!(
                    "ipc session {}: ring published slot {index} in a non-SUBMITTED state — \
                     poisoning session (§5.3 rule 4)",
                    self.id
                );
                return None;
            }
            // THE one linearization read of the descriptor (§5.3.1 rule 1):
            // validate the copy, serve from the copy, never re-read.
            let desc = slot.snapshot_descriptor();
            let completion = SlotCompletion {
                map: Arc::clone(&self.map),
                slot_index: index,
            };
            self.serve_validated(&desc, sink, completion);
            served += 1;
        }
        Some(served)
    }

    /// Validate a snapshot against daemon-owned state and dispatch it
    /// (§5.3 protocol rule 4: nothing in shm is trusted). Every path —
    /// reject, ECHO, sink — completes `completion` exactly once; rejects
    /// and ECHO complete inline, valid READ/WRITE ops hand the completion
    /// to the sink (which may complete synchronously or from a handoff).
    fn serve_validated(
        &self,
        desc: &SlotDescriptor,
        sink: &Arc<dyn SessionSink>,
        completion: SlotCompletion,
    ) {
        let reject = |why: &str| -> i64 {
            METRICS
                .ipc_descriptor_rejects
                .fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "ipc session {}: descriptor rejected ({why}): op={} binding={} offset={} \
                 len={} arena_off={}",
                self.id,
                desc.op,
                desc.binding,
                desc.offset,
                desc.len,
                desc.arena_off
            );
            -libc::EINVAL as i64
        };
        if !matches!(desc.op, OP_ECHO | OP_READ | OP_WRITE) {
            return completion.complete(reject("unknown op"));
        }
        let Some(binding) = self.bindings.read_sync(&desc.binding, |_, b| *b) else {
            return completion.complete(reject("dead binding id"));
        };
        if desc.len > self.map.geometry.max_op_bytes {
            return completion.complete(reject("len exceeds max_op_bytes"));
        }
        let Some(payload) = self.map.arena_window(desc.arena_off, desc.len) else {
            return completion.complete(reject("arena bounds violation"));
        };
        // Per-op rights, both directions (§5.2 screen rule 3): EBADF like
        // the kernel's wrong-direction op on that fd. Counted as
        // descriptor rejects — the attack/bug tripwire family.
        match desc.op {
            OP_READ if !binding.read_ok => {
                METRICS
                    .ipc_descriptor_rejects
                    .fetch_add(1, Ordering::Relaxed);
                return completion.complete(-libc::EBADF as i64);
            }
            OP_WRITE if !binding.write_ok => {
                METRICS
                    .ipc_descriptor_rejects
                    .fetch_add(1, Ordering::Relaxed);
                return completion.complete(-libc::EBADF as i64);
            }
            _ => {}
        }
        if desc.op == OP_ECHO {
            // The host's own liveness op (kept post-L4-4 as the ping):
            // §5.3.1 rule 2 — ONE arena read into a severed copy; the
            // derived byte sum and the inverted write-back both come from
            // the copy, never a second arena read.
            let severed = payload.read_severed();
            let sum: i64 = severed.iter().map(|b| i64::from(*b)).sum();
            let inverted: Vec<u8> = severed.iter().map(|b| !*b).collect();
            payload.write(&inverted);
            return completion.complete(sum);
        }
        sink.serve_data(
            DataOp {
                desc: *desc,
                binding,
                payload,
            },
            completion,
        );
    }
}

/// Nonce state (§5.2 lifecycle): current + immediately-previous, TTL
/// rotation. Mutex is fine — pure control plane (HELLO cadence).
struct NonceState {
    current: [u8; NONCE_LEN],
    previous: Option<[u8; NONCE_LEN]>,
    minted_at: Instant,
}

impl NonceState {
    fn fresh() -> Self {
        Self {
            current: rand_nonce(),
            previous: None,
            minted_at: Instant::now(),
        }
    }

    fn rotate(&mut self) {
        self.previous = Some(self.current);
        self.current = rand_nonce();
        self.minted_at = Instant::now();
    }

    fn rotate_if_stale(&mut self) {
        if self.minted_at.elapsed() >= NONCE_TTL {
            self.rotate();
        }
    }

    fn accepts(&self, nonce: &[u8; NONCE_LEN]) -> bool {
        nonce == &self.current || self.previous.as_ref() == Some(nonce)
    }
}

fn rand_nonce() -> [u8; NONCE_LEN] {
    use rand::RngCore;
    let mut n = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut n);
    n
}

/// The session host. One per interception-enabled mount; owns the
/// listener, the session registry, the nonce, and the drain thread.
pub struct IpcHost {
    cfg: IpcHostConfig,
    sink: Arc<dyn SessionSink>,
    admin: arc_swap::ArcSwap<Option<Arc<dyn AdminSink>>>,
    /// Live ADMIN connections (raw fds): severed at shutdown so their
    /// ctl threads unblock and join — a lingering admin client must
    /// never wedge daemon teardown (caught by the VL2 red suite).
    admin_conns: Mutex<Vec<Arc<UnixStream>>>,
    listener_fd: OwnedFd,
    /// OQ-6 path-socket listener + its on-disk path (unlinked at
    /// shutdown). `None` = disabled or degraded to abstract-only.
    path_listener: Option<(OwnedFd, std::path::PathBuf)>,
    nonce: Mutex<NonceState>,
    /// The mount's `st_dev` (screen rule: `st_dev` must match). `u64::MAX`
    /// = not resolved yet — every bind refuses class `mode` until the
    /// post-mount resolver stores it.
    expected_st_dev: AtomicU64,
    /// Host-local session-shm gauge (== the process `ipc_arena_bytes`
    /// metric while this is the only live host, which mount wiring
    /// guarantees per process).
    arena_bytes: AtomicU64,
    /// R5 shed demand: refuse NEW sessions until the gauge is ≤ this
    /// (`u64::MAX` = inactive); one-shot — clears when met. Live sessions
    /// are never torn (never-lossy).
    shed_target: AtomicU64,
    sessions: Mutex<HashMap<u64, Arc<IpcSession>>>,
    /// Bumped on every session admission/teardown — service threads
    /// re-collect their owned set only when this moves, so the drain
    /// hot pass never touches the registry mutex (the 2026-07-26
    /// service economy; per-pass re-collect was measurable coherence
    /// traffic across 8 spinning threads).
    session_epoch: AtomicU64,
    uid_sessions: Mutex<HashMap<u32, usize>>,
    next_session_id: AtomicU64,
    next_binding_id: AtomicU64,
    /// Pinned service-thread count (§5.5.1) — sessions are assigned an
    /// owner index in `0..service_threads` at admission, forever.
    service_threads: usize,
    /// Host epoch for the sessions' `last_active_ms` clocks.
    started: Instant,
    shutting_down: AtomicBool,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl IpcHost {
    /// Spawn the host: bind + listen on the abstract socket, start the
    /// accept thread and the drain thread.
    pub fn spawn(cfg: IpcHostConfig, sink: Arc<dyn SessionSink>) -> io::Result<Arc<Self>> {
        cfg.geometry
            .validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        // Layout computes now so admission can charge the exact footprint.
        SessionLayout::compute(&cfg.geometry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let listener_fd = abstract_listen(&cfg.socket_name)?;
        // OQ-6: the optional path rendezvous. Failure is loud but never
        // fatal — the mount (and same-netns interception) must not be
        // held hostage by optional plumbing.
        let path_listener = cfg.socket_dir.as_ref().and_then(|dir| {
            let path = dir.join(format!("{}.sock", cfg.socket_name));
            match path_listen(dir, &path) {
                Ok(fd) => {
                    log::info!("ipc host: path ctl socket at {}", path.display());
                    Some((fd, path))
                }
                Err(e) => {
                    log::warn!(
                        "ipc host: path ctl socket bind failed at {} ({e}) — \
                         abstract-only (foreign-netns clients will passthrough)",
                        path.display()
                    );
                    None
                }
            }
        });
        let service_threads = service_thread_count();
        let host = Arc::new(Self {
            cfg,
            sink,
            admin: arc_swap::ArcSwap::from_pointee(None),
            admin_conns: Mutex::new(Vec::new()),
            listener_fd,
            path_listener,
            nonce: Mutex::new(NonceState::fresh()),
            expected_st_dev: AtomicU64::new(u64::MAX),
            arena_bytes: AtomicU64::new(0),
            shed_target: AtomicU64::new(u64::MAX),
            sessions: Mutex::new(HashMap::new()),
            session_epoch: AtomicU64::new(0),
            uid_sessions: Mutex::new(HashMap::new()),
            next_session_id: AtomicU64::new(1),
            next_binding_id: AtomicU64::new(1),
            service_threads,
            started: Instant::now(),
            shutting_down: AtomicBool::new(false),
            threads: Mutex::new(Vec::new()),
        });
        METRICS
            .ipc_service_threads
            .store(service_threads as u64, Ordering::Relaxed);

        let accept_host = Arc::clone(&host);
        let accept = std::thread::Builder::new()
            .name("sqz-ipc-accept".into())
            .spawn(move || accept_host.accept_loop(AcceptOn::Abstract))?;
        let mut spawned = vec![accept];
        if host.path_listener.is_some() {
            let path_host = Arc::clone(&host);
            spawned.push(
                std::thread::Builder::new()
                    .name("sqz-ipc-accept-p".into())
                    .spawn(move || path_host.accept_loop(AcceptOn::Path))?,
            );
        }
        for idx in 0..service_threads {
            let service_host = Arc::clone(&host);
            spawned.push(
                std::thread::Builder::new()
                    .name(format!("sqz-ipc-svc{idx}"))
                    .spawn(move || service_host.service_loop(idx))?,
            );
        }
        if host.cfg.idle_secs > 0 {
            let reap_host = Arc::clone(&host);
            spawned.push(
                std::thread::Builder::new()
                    .name("sqz-ipc-reap".into())
                    .spawn(move || reap_host.reap_loop())?,
            );
        }
        host.threads
            .lock()
            .expect("thread registry mutex never poisons")
            .extend(spawned);
        Ok(host)
    }

    /// The mount device the fd screen requires (`fstat(mountpoint)` once
    /// the mount is live; tests inject).
    pub fn set_expected_st_dev(&self, st_dev: u64) {
        self.expected_st_dev.store(st_dev, Ordering::Relaxed);
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// §5.7 idle reaper: tear down sessions whose activity clock is
    /// older than `idle_secs`. Reap = generation bump (the client
    /// observes poison and degrades lazily) + ordinary teardown —
    /// counted in `ipc_sessions_reaped`, never in the poison tripwire
    /// (idleness is not a protocol violation).
    fn reap_loop(self: Arc<Self>) {
        let idle_ms = self.cfg.idle_secs.saturating_mul(1000);
        let tick = Duration::from_millis((idle_ms / 4).clamp(100, 5000));
        while !self.shutting_down.load(Ordering::SeqCst) {
            std::thread::park_timeout(tick);
            let now = self.now_ms();
            let idle: Vec<Arc<IpcSession>> = self
                .sessions
                .lock()
                .expect("session registry mutex never poisons")
                .values()
                .filter(|s| now.saturating_sub(s.last_active_ms.load(Ordering::Relaxed)) >= idle_ms)
                .cloned()
                .collect();
            for s in idle {
                METRICS.ipc_sessions_reaped.fetch_add(1, Ordering::Relaxed);
                log::info!(
                    "ipc session {}: idle past {} s — reaping (client degrades to passthrough)",
                    s.id,
                    self.cfg.idle_secs
                );
                s.map.header().generation.fetch_add(1, Ordering::Release);
                self.teardown_session(&s, "idle reap");
            }
        }
    }

    /// Current anti-replay nonce (bootstrap-blob synthesis + tests).
    /// Lazily rotates on the TTL.
    pub fn current_nonce(&self) -> [u8; NONCE_LEN] {
        let mut n = self.nonce.lock().expect("nonce mutex never poisons");
        n.rotate_if_stale();
        n.current
    }

    /// Force a rotation (tests; production rotates by TTL).
    pub fn rotate_nonce_for_test(&self) {
        self.nonce
            .lock()
            .expect("nonce mutex never poisons")
            .rotate();
    }

    /// The abstract socket name (bootstrap-blob synthesis).
    pub fn socket_name(&self) -> &str {
        &self.cfg.socket_name
    }

    /// This host's build-commit identity (bootstrap-blob synthesis).
    pub fn build_commit(&self) -> &str {
        &self.cfg.build_commit
    }

    /// Live session-shm bytes (the R5 component gauge source).
    pub fn arena_bytes(&self) -> u64 {
        self.arena_bytes.load(Ordering::Relaxed)
    }

    /// R5 shed entry (never-lossy): refuse NEW sessions until the gauge is
    /// back at/under `target`; never tears a live session.
    pub fn shed_to(&self, target: u64) {
        self.shed_target.fetch_min(target, Ordering::Relaxed);
        log::warn!(
            "ipc host: shed demanded to {target} B (gauge {} B) — new sessions refused \
             until under target; live sessions are never torn",
            self.arena_bytes()
        );
    }

    /// Stop accepting, tear down every live session, join the threads.
    pub fn shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        // Unblock the accept loops (shutdown(2) — closing an fd does NOT
        // reliably unblock a blocking accept on Linux).
        // SAFETY: plain shutdown(2) on our own listener fds.
        unsafe {
            libc::shutdown(self.listener_fd.as_raw_fd(), libc::SHUT_RDWR);
        }
        if let Some((fd, path)) = &self.path_listener {
            // SAFETY: plain shutdown(2) on our own path listener fd.
            unsafe {
                libc::shutdown(fd.as_raw_fd(), libc::SHUT_RDWR);
            }
            // Zero residue restored: the socket file dies with the host.
            let _ = std::fs::remove_file(path);
        }
        // Sever live ADMIN connections: their ctl threads park in recv
        // between verbs and must unblock for the join below (a
        // lingering admin client must never wedge teardown).
        for conn in std::mem::take(
            &mut *self
                .admin_conns
                .lock()
                .expect("admin conn registry mutex never poisons"),
        ) {
            // SAFETY: plain shutdown(2) on a connection we own an Arc of.
            unsafe {
                libc::shutdown(conn.as_raw_fd(), libc::SHUT_RDWR);
            }
        }
        let sessions: Vec<Arc<IpcSession>> = self
            .sessions
            .lock()
            .expect("session registry mutex never poisons")
            .values()
            .cloned()
            .collect();
        for s in sessions {
            self.teardown_session(&s, "host shutdown");
        }
        let threads = std::mem::take(
            &mut *self
                .threads
                .lock()
                .expect("thread registry mutex never poisons"),
        );
        // Cut every `park_timeout` short (the reap thread's tick is up
        // to 5 s — sleeping it out under the join added a flat 5 s to
        // EVERY interception-mount umount). Doorbell futex parks are
        // 5 ms-bounded and need no wake.
        for t in &threads {
            t.thread().unpark();
        }
        for t in threads {
            let _ = t.join();
        }
    }

    /// Synthesize the bootstrap virtual-xattr blob (§5.2) for this host.
    pub fn bootstrap_blob(&self) -> Vec<u8> {
        BootstrapBlob {
            abi: squeezefs_ipc::layout::IPC_ABI,
            flags: 0,
            build_commit: self.cfg.build_commit.clone(),
            socket: self.cfg.socket_name.clone(),
            // OQ-6 (v1.1): the filesystem-path rendezvous for container
            // netns — empty when disabled or degraded to abstract-only.
            socket_path: self
                .path_listener
                .as_ref()
                .map(|(_, p)| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            nonce: self.current_nonce(),
        }
        .encode()
    }

    // ---------------------------------------------------------------
    // accept + ctl plane
    // ---------------------------------------------------------------

    fn accept_loop(self: Arc<Self>, on: AcceptOn) {
        let listener = match on {
            AcceptOn::Abstract => self.listener_fd.as_raw_fd(),
            AcceptOn::Path => match &self.path_listener {
                Some((fd, _)) => fd.as_raw_fd(),
                None => return,
            },
        };
        loop {
            // SAFETY: accept4 on our listening socket; the fd is owned
            // immediately below.
            let fd = unsafe {
                libc::accept4(
                    listener,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if fd < 0 {
                if self.shutting_down.load(Ordering::SeqCst) {
                    return;
                }
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                log::error!("ipc host: accept failed: {err} — accept loop exiting");
                return;
            }
            // SAFETY: fresh owned fd from accept4.
            let sock = unsafe { UnixStream::from_raw_fd(fd) };
            let host = Arc::clone(&self);
            let handle = std::thread::Builder::new()
                .name("sqz-ipc-ctl".into())
                .spawn(move || host.connection_loop(sock));
            match handle {
                Ok(h) => self
                    .threads
                    .lock()
                    .expect("thread registry mutex never poisons")
                    .push(h),
                Err(e) => log::error!("ipc host: ctl thread spawn failed: {e}"),
            }
        }
    }

    /// One connection = at most one session: HELLO (screen) → SessionOk /
    /// Refuse; then BIND/UNBIND until EOF (client death — §5.7).
    fn connection_loop(self: Arc<Self>, sock: UnixStream) {
        let sock = Arc::new(sock);
        // First datagram routes the connection: AdminHello opens the
        // control-plane lane (VL2 §5.1.4), Hello the data plane.
        let first = match recv_ctl(&sock) {
            Ok(v) => v,
            Err(_) => return,
        };
        if let (CtlMsg::AdminHello { pid, uid }, _) = &first {
            self.admin_loop(&sock, *pid, *uid);
            return;
        }
        if let (CtlMsg::AdminReq { .. } | CtlMsg::AdminReply { .. } | CtlMsg::AdminOk, _) = &first {
            // Admin traffic from an unestablished peer: refuse loudly
            // (a silent drop reads as a hang to a buggy client).
            count_refusal(RefuseClass::Flags);
            log::warn!("ipc host: admin frame before AdminHello — refusing");
            let _ = send_ctl(
                &sock,
                &CtlMsg::Refuse {
                    class: RefuseClass::Flags,
                },
                None,
            );
            return;
        }
        let session = match self.handle_hello_msg(&sock, first) {
            Some(s) => s,
            None => return, // refused (counted) or transport error
        };
        loop {
            match recv_ctl(&sock) {
                Ok((CtlMsg::Bind, Some(fd))) => {
                    let reply = match self.screen_fd(fd.as_raw_fd()) {
                        Ok((rights_ino, read_ok, write_ok, odirect)) => {
                            let binding_id = self.next_binding_id.fetch_add(1, Ordering::Relaxed);
                            let rights = BindingRights {
                                ino: rights_ino,
                                read_ok,
                                write_ok,
                                odirect,
                                kill_priv: session.kill_priv,
                            };
                            let _ = session.bindings.insert_sync(binding_id, rights);
                            METRICS.ipc_binds.fetch_add(1, Ordering::Relaxed);
                            // §5.7 idle clock: ctl activity refreshes.
                            session
                                .last_active_ms
                                .store(self.now_ms(), Ordering::Relaxed);
                            // §5.6.2 W1: the bind-time inode invalidation
                            // (the data plane decides what that means).
                            self.sink.on_bind(rights_ino);
                            CtlMsg::BindOk {
                                binding_id,
                                ino: rights_ino,
                                read_ok,
                                write_ok,
                            }
                        }
                        Err(class) => {
                            count_refusal(class);
                            log::warn!("ipc session {}: BIND refused ({class:?})", session.id);
                            CtlMsg::BindRefused { class }
                        }
                    };
                    // The daemon's dup closes here (`fd` drops): it needed
                    // (st_dev, st_ino, flags, mode), never a live handle.
                    drop(fd);
                    if send_ctl(&sock, &reply, None).is_err() {
                        break;
                    }
                }
                Ok((CtlMsg::Bind, None)) => {
                    // BIND without an fd attachment: protocol violation.
                    self.poison_session(&session, "BIND carried no fd");
                    return;
                }
                Ok((CtlMsg::Unbind { binding_id }, _)) => {
                    let _ = session.bindings.remove_sync(&binding_id);
                }
                Ok((other, _)) => {
                    self.poison_session(&session, &format!("unexpected ctl message {other:?}"));
                    return;
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break, // client death
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    self.poison_session(&session, "malformed ctl datagram");
                    return;
                }
                Err(_) => break, // transport torn (shutdown / reset)
            }
        }
        self.teardown_session(&session, "ctl socket EOF");
    }

    /// HELLO validation ladder (§5.2 diagram order): version → nonce →
    /// peercred → fd screen → budget admission. Any refusal is counted +
    /// replied; success establishes the session and replies `SessionOk`
    /// with the sealed memfd.
    /// The ADMIN lane (VL2 §5.1.4): peercred-gated (uid 0 or the mount
    /// owner; claimed pid/uid must match the kernel's), then a
    /// request/reply verb loop against the wired [`AdminSink`]. No fd
    /// screen, no session, no shm — strictly more restrictive than the
    /// data plane.
    fn admin_loop(self: &Arc<Self>, sock: &Arc<UnixStream>, pid: u32, uid: u32) {
        let refuse = |class: RefuseClass| {
            count_refusal(class);
            log::warn!("ipc host: AdminHello refused ({class:?}) from pid {pid} uid {uid}");
            let _ = send_ctl(sock, &CtlMsg::Refuse { class }, None);
        };
        let Some(cred) = peer_cred(sock) else {
            return refuse(RefuseClass::Peercred);
        };
        if cred.pid as u32 != pid || cred.uid != uid {
            return refuse(RefuseClass::Peercred);
        }
        if cred.uid != 0 && cred.uid != self.cfg.owner_uid {
            return refuse(RefuseClass::Peercred);
        }
        if send_ctl(sock, &CtlMsg::AdminOk, None).is_err() {
            return;
        }
        log::info!("ipc host: ADMIN session opened by pid {pid} uid {uid}");
        self.admin_conns
            .lock()
            .expect("admin conn registry mutex never poisons")
            .push(Arc::clone(sock));
        loop {
            match recv_ctl(sock) {
                Ok((CtlMsg::AdminReq { verb, arg }, _)) => {
                    let reply = match self.admin.load().as_ref() {
                        Some(sink) => {
                            let (ok, body) = sink.handle(&verb, &arg);
                            CtlMsg::AdminReply { ok, body }
                        }
                        None => CtlMsg::AdminReply {
                            ok: false,
                            body: "admin sink not wired".to_string(),
                        },
                    };
                    if send_ctl(sock, &reply, None).is_err() {
                        break;
                    }
                }
                Ok((other, _)) => {
                    log::warn!("ipc host: unexpected ctl message on ADMIN lane: {other:?}");
                    break;
                }
                Err(_) => break, // EOF / transport torn / shutdown sever
            }
        }
        self.admin_conns
            .lock()
            .expect("admin conn registry mutex never poisons")
            .retain(|c| !Arc::ptr_eq(c, sock));
    }

    /// Wire the admin-verb handler (post-spawn; requests refuse until
    /// then).
    pub fn set_admin_sink(&self, sink: Arc<dyn AdminSink>) {
        self.admin.store(Arc::new(Some(sink)));
    }

    fn handle_hello_msg(
        self: &Arc<Self>,
        sock: &Arc<UnixStream>,
        first: (CtlMsg, Option<OwnedFd>),
    ) -> Option<Arc<IpcSession>> {
        let (msg, rx_fd) = first;
        let CtlMsg::Hello {
            abi,
            pid,
            uid,
            build_commit,
            nonce,
        } = msg
        else {
            // First datagram must be HELLO; anything else is a protocol
            // violation from an unestablished peer — drop it.
            log::warn!("ipc host: first ctl datagram was not HELLO — dropping connection");
            return None;
        };

        let refuse = |class: RefuseClass| -> Option<Arc<IpcSession>> {
            count_refusal(class);
            log::warn!("ipc host: HELLO refused ({class:?}) from pid {pid} uid {uid}");
            let _ = send_ctl(sock, &CtlMsg::Refuse { class }, None);
            None
        };

        // VL2 control-plane-only posture (§5.1.4): on a mount without
        // `-o interception`, data-plane sessions refuse BEFORE any fd
        // screen — no shm, no arenas, no service dispatch. The ADMIN
        // lane above is the only admitted traffic. The class names the
        // cause on the wire (user directive 2026-07-25): the shim's
        // refusal line tells the operator to mount with --interception
        // instead of hiding behind the Flags screen.
        if !self.cfg.data_plane {
            return refuse(RefuseClass::Disabled);
        }

        // KD-7 version lock: coarse ABI + build-commit equality; degenerate
        // identities (`unknown`/`-dirty`/empty) prove nothing by equality —
        // refused on EITHER side's value unless the counted dev override.
        if abi != squeezefs_ipc::layout::IPC_ABI || build_commit != self.cfg.build_commit {
            return refuse(RefuseClass::Version);
        }
        if build_commit_degenerate(&build_commit) || build_commit_degenerate(&self.cfg.build_commit)
        {
            if !self.cfg.allow_dev {
                return refuse(RefuseClass::Version);
            }
            METRICS
                .ipc_binds_dev_override
                .fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "ipc host: degenerate build identity admitted via dev override \
                 (SQUEEZEFS_IPC_ALLOW_DEV) — fleet-hygiene alarm outside dev boxes"
            );
        }

        // Nonce freshness (anti-replay; current + immediately-previous).
        {
            let mut n = self.nonce.lock().expect("nonce mutex never poisons");
            n.rotate_if_stale();
            if !n.accepts(&nonce) {
                drop(n);
                return refuse(RefuseClass::Nonce);
            }
        }

        // SO_PEERCRED defense-in-depth: the kernel's (pid, uid) is the
        // truth; a claimed identity that contradicts it refuses. The fd
        // remains the authorizer — this is accounting integrity.
        let Some(cred) = peer_cred(sock) else {
            return refuse(RefuseClass::Internal);
        };
        if cred.pid as u32 != pid || cred.uid != uid {
            return refuse(RefuseClass::Peercred);
        }

        // THE fd screen (normative §5.2).
        let Some(rx_fd) = rx_fd else {
            return refuse(RefuseClass::Flags); // HELLO carried no credential fd
        };
        if let Err(class) = self.screen_fd(rx_fd.as_raw_fd()) {
            return refuse(class);
        }
        drop(rx_fd); // dup validated + closed (never held as a live handle)

        // Budget admission (R5 component + per-uid caps). The shed target
        // is a one-shot demand: refuse-new until the gauge is under it.
        let layout =
            SessionLayout::compute(&self.cfg.geometry).expect("geometry validated at spawn");
        let footprint = layout.total_bytes;
        let current = self.arena_bytes.load(Ordering::Relaxed);
        let shed = self.shed_target.load(Ordering::Relaxed);
        if shed != u64::MAX {
            if current > shed {
                METRICS
                    .ipc_admission_refusals
                    .fetch_add(1, Ordering::Relaxed);
                return refuse(RefuseClass::Budget);
            }
            // Demand met: clear it and fall through to normal admission.
            self.shed_target.store(u64::MAX, Ordering::Relaxed);
        }
        if current.saturating_add(footprint) > self.cfg.arena_cap_bytes {
            METRICS
                .ipc_admission_refusals
                .fetch_add(1, Ordering::Relaxed);
            return refuse(RefuseClass::Budget);
        }
        {
            let uid_sessions = self
                .uid_sessions
                .lock()
                .expect("uid session mutex never poisons");
            if uid_sessions.get(&cred.uid).copied().unwrap_or(0) >= self.cfg.per_uid_session_cap {
                METRICS
                    .ipc_admission_refusals
                    .fetch_add(1, Ordering::Relaxed);
                return refuse(RefuseClass::Budget);
            }
        }

        // Establish: sealed memfd + mapping + registry entry.
        let (memfd, map) = match create_session_shm(&self.cfg.geometry, layout) {
            Ok(v) => v,
            Err(e) => {
                log::error!("ipc host: session shm creation failed: {e}");
                return refuse(RefuseClass::Internal);
            }
        };
        // §5.5.1 pinning: admit to the lightest service thread (live
        // sessions never rebalance — natural churn is the only mover).
        let owner = {
            let sessions = self
                .sessions
                .lock()
                .expect("session registry mutex never poisons");
            let mut counts = vec![0usize; self.service_threads];
            for s in sessions.values() {
                counts[s.owner] += 1;
            }
            counts
                .iter()
                .enumerate()
                .min_by_key(|(_, n)| **n)
                .map(|(i, _)| i)
                .unwrap_or(0)
        };
        let session = Arc::new(IpcSession {
            id: self.next_session_id.fetch_add(1, Ordering::Relaxed),
            uid: cred.uid,
            kill_priv: peer_kill_priv(cred.uid, cred.pid as u32),
            owner,
            map: Arc::new(map),
            charged_bytes: footprint,
            bindings: scc::HashMap::new(),
            consumer: Mutex::new(RingConsumer::new()),
            sock: Arc::clone(sock),
            last_active_ms: AtomicU64::new(0),
            torn_down: AtomicBool::new(false),
        });
        session
            .last_active_ms
            .store(self.now_ms(), Ordering::Relaxed);
        self.arena_bytes.fetch_add(footprint, Ordering::Relaxed);
        METRICS
            .ipc_arena_bytes
            .fetch_add(footprint, Ordering::Relaxed);
        METRICS.ipc_sessions_active.fetch_add(1, Ordering::Relaxed);
        METRICS.ipc_sessions_total.fetch_add(1, Ordering::Relaxed);
        self.sessions
            .lock()
            .expect("session registry mutex never poisons")
            .insert(session.id, Arc::clone(&session));
        // AFTER the insert: a service thread seeing the new epoch always
        // finds the session in the map.
        self.session_epoch.fetch_add(1, Ordering::Release);
        *self
            .uid_sessions
            .lock()
            .expect("uid session mutex never poisons")
            .entry(cred.uid)
            .or_insert(0) += 1;

        if send_ctl(
            sock,
            &CtlMsg::SessionOk {
                geometry: self.cfg.geometry,
            },
            Some(memfd.as_raw_fd()),
        )
        .is_err()
        {
            self.teardown_session(&session, "SessionOk send failed");
            return None;
        }
        // The daemon's memfd handle closes here; the mapping (already
        // established) keeps the memory alive, and the client holds its
        // own fd. Zero persistent residue by construction.
        drop(memfd);
        Some(session)
    }

    /// The normative §5.2 daemon fd screen (module docs). Returns
    /// `(st_ino, read_ok, write_ok, odirect)` for accepted fds.
    fn screen_fd(&self, fd: RawFd) -> Result<(u64, bool, bool, bool), RefuseClass> {
        // SAFETY: F_GETFL on a received (owned) fd.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(RefuseClass::Flags);
        }
        // Rule 1 (load-bearing): O_PATH descriptions are refused outright —
        // obtainable with search-only permission, access-mode bits read 0.
        if flags & libc::O_PATH != 0 {
            return Err(RefuseClass::Flags);
        }
        // SAFETY: fstat into a zeroed stat buf.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: plain fstat(2) on the received fd.
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return Err(RefuseClass::Flags);
        }
        // Rule 2: regular files only.
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(RefuseClass::Flags);
        }
        // Rule 3: the fd must be a rights-bearing capability ON THIS
        // MOUNT; also refused while the mount device is unresolved.
        let expected = self.expected_st_dev.load(Ordering::Relaxed);
        if expected == u64::MAX || st.st_dev != expected {
            return Err(RefuseClass::Mode);
        }
        // Rule 4: semantics screens (enforced daemon-side, not merely
        // shim-side): O_APPEND (atomic size authority), O_SYNC/O_DSYNC
        // (per-op durable barriers), and the O_TMPFILE class — the
        // O_TMPFILE status bits when the kernel exposes them, plus
        // `st_nlink == 0` (an unnamed regular file; conservatively also
        // refuses open-then-unlinked fds — passthrough serves them).
        if flags & libc::O_APPEND != 0
            || flags & libc::O_SYNC == libc::O_SYNC
            || flags & libc::O_DSYNC == libc::O_DSYNC
            || flags & libc::O_TMPFILE == libc::O_TMPFILE
            || st.st_nlink == 0
        {
            return Err(RefuseClass::Flags);
        }
        // Rule 5: rights strictly from the access mode, both directions.
        let (read_ok, write_ok) = match flags & libc::O_ACCMODE {
            libc::O_RDONLY => (true, false),
            libc::O_WRONLY => (false, true),
            libc::O_RDWR => (true, true),
            _ => return Err(RefuseClass::Flags),
        };
        // Read class (BindingRights doc): the description's O_DIRECT bit,
        // carried into every ring op exactly as the kernel path carries
        // `fuse_read_in.flags`.
        let odirect = flags & libc::O_DIRECT != 0;
        Ok((st.st_ino, read_ok, write_ok, odirect))
    }

    // ---------------------------------------------------------------
    // teardown / poison
    // ---------------------------------------------------------------

    fn teardown_session(&self, session: &Arc<IpcSession>, why: &str) {
        if session.torn_down.swap(true, Ordering::SeqCst) {
            return;
        }
        log::info!("ipc session {} torn down ({why})", session.id);
        session.bindings.clear_sync();
        let _ = session.sock.shutdown(std::net::Shutdown::Both);
        self.sessions
            .lock()
            .expect("session registry mutex never poisons")
            .remove(&session.id);
        // A stale snapshot may drain this session for at most one more
        // pass (safe: the mapping is Arc-held, bindings are cleared, the
        // torn_down flag skips it at the next refresh).
        self.session_epoch.fetch_add(1, Ordering::Release);
        {
            let mut uid_sessions = self
                .uid_sessions
                .lock()
                .expect("uid session mutex never poisons");
            if let Some(n) = uid_sessions.get_mut(&session.uid) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    uid_sessions.remove(&session.uid);
                }
            }
        }
        self.arena_bytes
            .fetch_sub(session.charged_bytes, Ordering::Relaxed);
        METRICS
            .ipc_arena_bytes
            .fetch_sub(session.charged_bytes, Ordering::Relaxed);
        METRICS.ipc_sessions_active.fetch_sub(1, Ordering::Relaxed);
        // The mapping unmaps when the last accessor's Arc drops (§5.3.1
        // rule 4 — structural, not by convention).
    }

    /// §5.3 rule 4 / §5.7: protocol violation ⇒ loud poison. The header
    /// generation bump is what a live client observes (all fds
    /// passthrough); the socket shutdown ends the ctl thread.
    fn poison_session(&self, session: &Arc<IpcSession>, why: &str) {
        METRICS
            .ipc_sessions_poisoned
            .fetch_add(1, Ordering::Relaxed);
        log::error!(
            "ipc session {} POISONED ({why}) — tearing down; client degrades to passthrough",
            session.id
        );
        session
            .map
            .header()
            .generation
            .fetch_add(1, Ordering::Release);
        self.teardown_session(session, "poisoned");
    }

    // ---------------------------------------------------------------
    // service threads (§5.5.1): each drains ITS pinned sessions —
    // validation + ECHO inline, READ/WRITE through the sink (fast path
    // or async handoff; PR L4-4)
    // ---------------------------------------------------------------

    fn service_loop(self: Arc<Self>, idx: usize) {
        // Owned-session snapshot, re-collected ONLY when the registry
        // epoch moves (admission/teardown) — the drain hot pass is
        // registry-mutex-free (2026-07-26 service economy). Staleness
        // bound: one pass (a torn-down session drains at most once more,
        // safely; a new session is picked up at the next pass top —
        // pinned by `new_session_on_a_busy_thread_is_served_promptly`).
        let mut sessions: Vec<Arc<IpcSession>> = Vec::new();
        let mut seen_epoch = u64::MAX; // != any real epoch ⇒ first pass collects
        let mut last_progress = Instant::now();
        let spin_window = service_spin_window();
        // Reused park snapshot (op-economy): the doorbell-snapshot Vec is
        // cleared+refilled per park, never reallocated — qd1 RTT shapes
        // park once per op, so per-park allocations are per-op costs.
        let mut observed: Vec<u32> = Vec::new();
        while !self.shutting_down.load(Ordering::SeqCst) {
            let epoch = self.session_epoch.load(Ordering::Acquire);
            if epoch != seen_epoch {
                sessions = self
                    .sessions
                    .lock()
                    .expect("session registry mutex never poisons")
                    .values()
                    .filter(|s| s.owner == idx && !s.torn_down.load(Ordering::SeqCst))
                    .cloned()
                    .collect();
                seen_epoch = epoch;
            }
            let mut served = 0u32;
            let now = self.now_ms();
            for s in &sessions {
                match s.drain(&self.sink) {
                    Some(0) => {}
                    Some(n) => {
                        served += n;
                        // §5.7 idle clock: served ring ops are activity.
                        s.last_active_ms.store(now, Ordering::Relaxed);
                    }
                    None => self.poison_session(s, "ring/slot protocol violation"),
                }
            }
            // One flush per sweep (SessionSink::flush liveness rule):
            // direct-drive SQEs published during the drain become
            // kernel-visible before this thread can park.
            self.sink.flush();
            if served > 0 {
                last_progress = Instant::now();
                continue;
            }
            if last_progress.elapsed() < spin_window {
                std::hint::spin_loop();
                continue;
            }
            // Park (bounded — §5.3.1 rule 5): set every owned session's
            // parked flag, SNAPSHOT every doorbell, re-scan (the
            // disarm→scan law: a submission published before the flag was
            // visible is found by this scan; one published after sees the
            // flag and wakes), then wait on ALL owned doorbells against
            // the pre-rescan snapshots.
            //
            // Two lost-wake classes closed here (2026-07-25 ipc-miss-path
            // fix; each was a measured 5 ms-bound latency term on the
            // miss-dominated shape):
            // 1. **Snapshot BEFORE rescan** (order is load-bearing —
            //    `ipc_park_snapshot_before_rescan_never_strands` in
            //    loom-models): snapshotting after the rescan folds a
            //    submission that raced the rescan's tail INTO the wait's
            //    expected value, so the wait admits and sleeps its full
            //    bound with a servable op published (13 % of parks were
            //    expiring by timeout under load). With the snapshot
            //    first, any doorbell bump after it fails the wait's
            //    admission (or the rescan already served it — one
            //    spurious re-loop, never a strand).
            // 2. **Wait on ALL owned doorbells** (`futex_waitv(2)`): the
            //    former single-doorbell wait deafened the thread to every
            //    other owned session's wake — with sessions > service
            //    threads, non-first sessions ate the full 5 ms bound on
            //    EVERY submission burst (the measured sessions=8 <
            //    sessions=4 inversion). Kernels without futex_waitv fall
            //    back to the first-session wait; the 5 ms bound still
            //    caps the damage (§5.3.1 rule 5 semantics unchanged).
            for s in &sessions {
                s.map.header().daemon_parked.store(1, Ordering::SeqCst);
            }
            observed.clear();
            observed.extend(
                sessions
                    .iter()
                    .map(|s| s.map.header().doorbell.load(Ordering::SeqCst)),
            );
            let mut rescan_served = 0u32;
            let now = self.now_ms();
            for s in &sessions {
                match s.drain(&self.sink) {
                    Some(0) => {}
                    Some(n) => {
                        rescan_served += n;
                        s.last_active_ms.store(now, Ordering::Relaxed);
                    }
                    None => self.poison_session(s, "ring/slot protocol violation"),
                }
            }
            // Same liveness rule on the pre-park rescan sweep.
            self.sink.flush();
            if rescan_served == 0 {
                if sessions.is_empty() {
                    std::thread::park_timeout(SERVICE_PARK_MAX);
                } else {
                    METRICS.ipc_service_parks.fetch_add(1, Ordering::Relaxed);
                    futex_wait_many(
                        sessions
                            .iter()
                            .zip(&observed)
                            .map(|(s, o)| (&s.map.header().doorbell, *o)),
                        SERVICE_PARK_MAX,
                    );
                }
            } else {
                last_progress = Instant::now();
            }
            for s in &sessions {
                s.map.header().daemon_parked.store(0, Ordering::SeqCst);
            }
        }
    }
}

fn count_refusal(class: RefuseClass) {
    let counter = match class {
        RefuseClass::Version => &METRICS.ipc_bind_refused_version,
        RefuseClass::Nonce => &METRICS.ipc_bind_refused_nonce,
        RefuseClass::Flags => &METRICS.ipc_bind_refused_flags,
        RefuseClass::Mode => &METRICS.ipc_bind_refused_mode,
        RefuseClass::Budget => &METRICS.ipc_bind_refused_budget,
        RefuseClass::Peercred => &METRICS.ipc_bind_refused_peercred,
        // Expected posture on non-interception mounts (every preloaded
        // app's establish probe lands here) — counted separately so the
        // security-signal classes above stay unpolluted.
        RefuseClass::Disabled => &METRICS.ipc_bind_refused_disabled,
        // Internal failures are daemon faults, logged loud at the site;
        // ledger them with the budget/admission family is wrong — count
        // them as poisons? No: they refuse a NEW session, nothing lives
        // to poison. They ride the flags-agnostic refusal log only.
        RefuseClass::Internal => {
            log::error!("ipc host: internal refusal (daemon-side failure — see prior log line)");
            return;
        }
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Create + seed + seal one session memfd, and map it daemon-side.
fn create_session_shm(
    geometry: &Geometry,
    layout: SessionLayout,
) -> io::Result<(OwnedFd, SessionMapping)> {
    // SAFETY: memfd_create with a static name; ownership taken immediately.
    let fd = unsafe {
        libc::memfd_create(
            c"sqz-ipc-session".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd.
    let memfd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: ftruncate to the layout size on our own memfd.
    if unsafe { libc::ftruncate(memfd.as_raw_fd(), layout.total_bytes as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: shared RW mapping of the full memfd.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            layout.total_bytes as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            memfd.as_raw_fd(),
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let map = SessionMapping {
        base: base as *mut u8,
        layout,
        geometry: *geometry,
    };
    // Write-once initialization before the fd is shared (the sharing act
    // is the synchronization point): header placement-write, ring cell
    // seeding. Slots + stats page are valid all-zeroes (fresh memfd pages).
    // SAFETY: region 0 is page-aligned and header-sized by the layout.
    unsafe {
        std::ptr::write(
            map.base as *mut SessionHeader,
            SessionHeader::new(*geometry),
        );
    }
    map.ring().seed_for_sharing();
    // Seal: the geometry can never change under either side. (F_SEAL_SEAL
    // last — no further seals.)
    // SAFETY: F_ADD_SEALS on our own memfd.
    let sealed = unsafe {
        libc::fcntl(
            memfd.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    };
    if sealed != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((memfd, map))
}

// -------------------------------------------------------------------
// socket plumbing (abstract AF_UNIX SOCK_SEQPACKET + SCM_RIGHTS)
// -------------------------------------------------------------------

/// Which listener an accept thread drains (identical connection
/// handling — the rendezvous is the only difference).
#[derive(Clone, Copy)]
enum AcceptOn {
    Abstract,
    Path,
}

fn abstract_sockaddr(name: &str) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: zeroed sockaddr_un is a valid all-default value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = name.as_bytes();
    if bytes.len() + 1 > addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "abstract socket name too long",
        ));
    }
    // Abstract namespace: sun_path[0] = NUL, name follows (not
    // NUL-terminated; length carried in socklen).
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i + 1] = *b as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len();
    Ok((addr, len as libc::socklen_t))
}

fn abstract_listen(name: &str) -> io::Result<OwnedFd> {
    // SAFETY: socket(2); ownership taken immediately.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let (addr, len) = abstract_sockaddr(name)?;
    // SAFETY: bind with a correctly-sized sockaddr_un.
    if unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: listen(2).
    if unsafe { libc::listen(fd.as_raw_fd(), 64) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn path_sockaddr(path: &std::path::Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: zeroed sockaddr_un is a valid all-default value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() + 1 > addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long for sun_path",
        ));
    }
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i] = *b as libc::c_char;
    }
    // NUL-terminated filesystem path.
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

/// OQ-6: bind + listen a filesystem-path `SOCK_SEQPACKET` socket.
/// The dir is created 0755 if missing; a same-name stale file is OUR
/// crash residue (names embed pid+random — never a live foreign
/// daemon) and is replaced; the socket file goes 0666 (connecting is
/// not a credential — SO_PEERCRED + the fd screen are the boundary).
fn path_listen(dir: &std::path::Path, path: &std::path::Path) -> io::Result<OwnedFd> {
    std::fs::create_dir_all(dir)?;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // SAFETY: socket(2); ownership taken immediately.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let (addr, len) = path_sockaddr(path)?;
    // SAFETY: bind with a correctly-sized sockaddr_un.
    if unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    }
    // SAFETY: listen(2).
    if unsafe { libc::listen(fd.as_raw_fd(), 64) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Client-side connect to a filesystem-path `SOCK_SEQPACKET` socket
/// (OQ-6 rendezvous; tests use it as the raw-protocol harness).
pub fn path_connect(path: &str) -> io::Result<UnixStream> {
    // SAFETY: socket(2); ownership taken immediately.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let (addr, len) = path_sockaddr(std::path::Path::new(path))?;
    // SAFETY: connect with a correctly-sized sockaddr_un.
    if unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(UnixStream::from(fd))
}

/// Client-side connect to an abstract-namespace `SOCK_SEQPACKET` socket
/// (the shim's rendezvous; tests use it as the raw-protocol harness).
pub fn abstract_connect(name: &str) -> io::Result<UnixStream> {
    // SAFETY: socket(2); ownership taken immediately.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let (addr, len) = abstract_sockaddr(name)?;
    // SAFETY: connect with a correctly-sized sockaddr_un.
    if unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(UnixStream::from(fd))
}

const CMSG_FD_SPACE: usize = 64; // CMSG_SPACE(sizeof(int)) with headroom

/// Send one ctl datagram, optionally attaching one fd via `SCM_RIGHTS`.
pub fn send_ctl(sock: &UnixStream, msg: &CtlMsg, fd: Option<RawFd>) -> io::Result<()> {
    let bytes = msg.encode();
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let mut cmsg_buf = [0u8; CMSG_FD_SPACE];
    // SAFETY: zeroed msghdr is a valid all-default value.
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    if let Some(fd) = fd {
        hdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        // SAFETY: CMSG_SPACE for one int fits the buffer by construction.
        hdr.msg_controllen =
            unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as libc::size_t;
        // SAFETY: standard CMSG_FIRSTHDR/CMSG_DATA dance over the buffer
        // sized above.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&hdr);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as libc::size_t;
            std::ptr::copy_nonoverlapping(
                &fd as *const RawFd as *const u8,
                libc::CMSG_DATA(cmsg),
                std::mem::size_of::<RawFd>(),
            );
        }
    }
    // SAFETY: sendmsg with the msghdr assembled above.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &hdr, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receive one ctl datagram (+ at most one attached fd). EOF surfaces as
/// `UnexpectedEof`; undecodable bytes as `InvalidData` (the caller
/// poisons loudly, never crashes).
pub fn recv_ctl(sock: &UnixStream) -> io::Result<(CtlMsg, Option<OwnedFd>)> {
    let mut buf = [0u8; CTL_MSG_MAX];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut cmsg_buf = [0u8; CMSG_FD_SPACE];
    // SAFETY: zeroed msghdr is a valid all-default value.
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    hdr.msg_controllen = cmsg_buf.len() as libc::size_t;
    // SAFETY: recvmsg into the buffers above; CLOEXEC on any received fd.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut hdr, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut rx_fd = None;
    // SAFETY: CMSG walk over the kernel-filled control buffer.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&hdr);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let mut fd: RawFd = -1;
                std::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(cmsg),
                    &mut fd as *mut RawFd as *mut u8,
                    std::mem::size_of::<RawFd>(),
                );
                if fd >= 0 {
                    rx_fd = Some(OwnedFd::from_raw_fd(fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&hdr, cmsg);
        }
    }
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "ctl socket EOF",
        ));
    }
    let msg = CtlMsg::decode(&buf[..n as usize])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok((msg, rx_fd))
}

fn peer_cred(sock: &UnixStream) -> Option<libc::ucred> {
    // SAFETY: getsockopt(SO_PEERCRED) into a ucred-sized buffer.
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        ) != 0
        {
            return None;
        }
        Some(cred)
    }
}

/// CAP_FSETID is bit 4 of a `/proc/<pid>/status` `CapEff:` word
/// (linux/capability.h). A malformed word classifies `false`
/// (NOT-privileged — the conservative direction for the killpriv
/// obligation: clearing where the kernel might not is safe; preserving
/// where the kernel would clear is the security hole). Pinned in
/// tests/ipc_host_tests.rs.
pub fn capeff_hex_has_fsetid(hex: &str) -> bool {
    const CAP_FSETID_BIT: u64 = 1 << 4;
    u64::from_str_radix(hex.trim(), 16)
        .map(|word| word & CAP_FSETID_BIT != 0)
        .unwrap_or(false)
}

/// The killpriv-v2 il-parity peer class (2026-07-28 campaign): does
/// this session's writes carry the daemon's suid/sgid/caps clearing
/// obligation? Mirrors the kernel path's `!capable(CAP_FSETID)`:
/// uid 0 is exempt; a non-root peer is exempt only when its effective
/// capability set carries CAP_FSETID (read once from
/// `/proc/<pid>/status` at HELLO — the pid is SO_PEERCRED-verified).
/// An unreadable /proc (peer already died, hidepid) classifies kill —
/// conservative, see [`capeff_hex_has_fsetid`].
pub fn peer_kill_priv(uid: u32, pid: u32) -> bool {
    if uid == 0 {
        return false;
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return true;
    };
    let has_fsetid = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .map(capeff_hex_has_fsetid)
        .unwrap_or(false);
    !has_fsetid
}

// -------------------------------------------------------------------
// futex plumbing (cross-process shm words — never FUTEX_PRIVATE)
// -------------------------------------------------------------------

/// `FUTEX_WAKE` up to `n` waiters parked on `word` (cross-process).
pub fn futex_wake(word: &AtomicU32, n: i32) {
    // SAFETY: FUTEX_WAKE on a live atomic's address; no memory is read.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAKE,
            n,
            0usize,
            0usize,
            0u32,
        );
    }
}

/// Bounded `FUTEX_WAIT` for the in-process test harnesses (the op-economy
/// suite's raw-protocol reaper parks on the session cqe word exactly like
/// the shim's reap loop). Production daemon code never waits through this
/// — the daemon only wakes.
pub fn futex_wait_for_test(word: &AtomicU32, expected: u32, timeout: Duration) {
    futex_wait(word, expected, timeout);
}

/// Bounded `FUTEX_WAIT`: sleep while `*word == expected`, at most
/// `timeout` (§5.3.1 rule 5 — every wait on client-writable memory is
/// timeout-bounded).
fn futex_wait(word: &AtomicU32, expected: u32, timeout: Duration) {
    let ts = libc::timespec {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_nsec: i64::from(timeout.subsec_nanos()) as libc::c_long,
    };
    // SAFETY: FUTEX_WAIT with a valid timespec; spurious wakeups and
    // EAGAIN (value changed) are both fine — the caller rescans.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT,
            expected,
            &ts as *const libc::timespec,
            0usize,
            0u32,
        );
    }
}

/// `futex_waitv(2)` wait-multiple entry (Linux ≥ 5.16). Mirrors
/// `include/uapi/linux/futex.h` — 32 bytes, `val`/`uaddr` as u64.
#[repr(C)]
#[derive(Clone, Copy)]
struct FutexWaitv {
    val: u64,
    uaddr: u64,
    flags: u32,
    __reserved: u32,
}

/// `FUTEX2_SIZE_U32` — the only size the doorbell words use.
const FUTEX2_SIZE_U32: u32 = 0x02;
/// `futex_waitv(2)` hard cap (`FUTEX_WAITV_MAX`).
const FUTEX_WAITV_MAX: usize = 128;

/// Bounded wait on MANY futex words at once (the multi-session service
/// park — one wake on ANY owned doorbell returns). Each `(word, expected)`
/// pair admits like `FUTEX_WAIT`: if any word already differs, returns
/// immediately (EAGAIN). Beyond `FUTEX_WAITV_MAX` words the excess is not
/// waited on — the `timeout` bound still caps their latency (§5.3.1 rule
/// 5). Kernels without `futex_waitv` (ENOSYS) fall back to the
/// first-word single wait — the pre-fix posture, same bound.
///
/// Iterator-fed + stack waiter array (op-economy campaign): the park
/// path is allocation-free — the former per-park `Vec` pair (caller
/// waiter list + this array) was measurable allocator traffic at qd1
/// RTT rates (one park cycle per op).
fn futex_wait_many<'a>(waiters: impl Iterator<Item = (&'a AtomicU32, u32)>, timeout: Duration) {
    let mut arr = [FutexWaitv {
        val: 0,
        uaddr: 0,
        flags: FUTEX2_SIZE_U32,
        __reserved: 0,
    }; FUTEX_WAITV_MAX];
    let mut first: Option<(&AtomicU32, u32)> = None;
    let mut n = 0usize;
    for (word, expected) in waiters {
        if first.is_none() {
            first = Some((word, expected));
        }
        if n == FUTEX_WAITV_MAX {
            break;
        }
        arr[n] = FutexWaitv {
            val: u64::from(expected),
            uaddr: word.as_ptr() as u64,
            flags: FUTEX2_SIZE_U32,
            __reserved: 0,
        };
        n += 1;
    }
    let Some((first_word, first_expected)) = first else {
        debug_assert!(false, "futex_wait_many with no waiters");
        return;
    };
    if n == 1 {
        futex_wait(first_word, first_expected, timeout);
        return;
    }
    static WAITV_SUPPORTED: AtomicBool = AtomicBool::new(true);
    if !WAITV_SUPPORTED.load(Ordering::Relaxed) {
        futex_wait(first_word, first_expected, timeout);
        return;
    }
    let vec = &arr[..n];
    // futex_waitv takes an ABSOLUTE timeout (CLOCK_MONOTONIC).
    // SAFETY: clock_gettime into a zeroed timespec.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: plain clock_gettime(2).
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let mut nsec = ts.tv_nsec as i128 + i128::from(timeout.subsec_nanos());
    let mut sec = ts.tv_sec as i128 + timeout.as_secs() as i128;
    if nsec >= 1_000_000_000 {
        nsec -= 1_000_000_000;
        sec += 1;
    }
    ts.tv_sec = sec as libc::time_t;
    ts.tv_nsec = nsec as libc::c_long;
    // SAFETY: SYS_futex_waitv with a valid waiter array + absolute
    // timespec; every return class (wake index, EAGAIN value-changed,
    // ETIMEDOUT, EINTR) resolves to "caller rescans".
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex_waitv,
            vec.as_ptr(),
            vec.len() as u32,
            0u32,
            &ts as *const libc::timespec,
            libc::CLOCK_MONOTONIC,
        )
    };
    if r < 0 {
        // SAFETY: errno read directly after the failing call.
        let errno = unsafe { *libc::__errno_location() };
        if errno == libc::ENOSYS {
            // Pre-5.16 kernel: remember, degrade to the single wait.
            WAITV_SUPPORTED.store(false, Ordering::Relaxed);
            futex_wait(first_word, first_expected, timeout);
        }
    }
}
