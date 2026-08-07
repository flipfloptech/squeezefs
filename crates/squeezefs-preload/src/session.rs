//! The shim's **session client** (§5.2 client side, §5.3 wake protocol,
//! §5.4.1 ring-op error ladder): ctl-socket handshake, shm mapping, and
//! the ring submit/wait machinery the interposers call.
//!
//! ## Locking posture
//!
//! The **data path** (`ring_pread`/`ring_pwrite`) takes no locks: slot
//! claim is a bounded CAS scan, the ring push is lock-free, the wait is
//! spin-then-futex on the slot's own state word. The **ctl path**
//! (`bind`/`unbind`) serializes request/response on one mutex — it is
//! control-plane (an `open(2)` cadence), and same-thread reentry (the
//! only self-deadlock shape) is impossible because the interposers'
//! TLS guard routes reentrant calls straight to the real libc function.
//!
//! ## Bounded waits (§5.4.1)
//!
//! Every wait on daemon-owned progress is deadline-bounded. A timeout
//! **poisons the session** (a daemon that stopped completing ops is
//! sick; every later op falls through immediately) — the timed-out
//! slot is never reused (the daemon may still complete it arbitrarily
//! late; recycling it would hand a stale completion to a new op).
//! Poisoning is AS-safe (one flag store + `shutdown(2)` on the ctl
//! socket — never `close`, so the fd number cannot be reused under a
//! racing ctl call), because the atfork child handler runs it.

use crate::fd_table::Binding;
use squeezefs_ipc::layout::{
    ClientStatsPage, Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_READ,
    OP_WRITE,
};
use squeezefs_ipc::ring_core::{MpscRingView, RingCell};
use squeezefs_ipc::slot_core::ParkOutcome;
use squeezefs_ipc::wire::{build_commit_degenerate, BootstrapBlob, CtlMsg, CTL_MSG_MAX};

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

/// Default per-op completion deadline (overridable via
/// `SQUEEZEFS_IL_OP_TIMEOUT_MS` or [`Session::establish_with_op_timeout_ms`]).
const OP_TIMEOUT_DEFAULT: Duration = Duration::from_secs(30);

/// Spin iterations on the slot state word before parking (the RTT is
/// single-digit µs on the measured rig — G-L4-1 — so warm ops normally
/// never park). Tunable (`SQUEEZEFS_IL_SPINS`): on handoff-heavy
/// device-true workloads with hundreds of client threads, the spin
/// window is pure CPU theft from the daemon — the 2026-07-19 sweep
/// showed inverse scaling from exactly this.
const WAIT_SPINS_DEFAULT: u32 = 4096;

/// The adaptive floor: a session whose LAST op parked spins only this
/// long before parking again (a parked op means daemon-side async work
/// — device reads, lock waits — where the full window is CPU theft
/// from the daemon; the 2026-07-19 device-true sweep measured the
/// difference as 336 k → 605 k IOPS at 64 threads, with 256 the best
/// floor: shorter floors over-park just-completing ops into futex
/// syscalls).
const WAIT_SPINS_PARKY: u32 = 256;

/// `(spin_window, parky_floor)`. An EXPLICIT `SQUEEZEFS_IL_SPINS`
/// pins both (the A/B lever disables adaptivity); the default pair is
/// adaptive.
fn wait_spins() -> (u32, u32) {
    static SPINS: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    *SPINS.get_or_init(|| {
        match std::env::var("SQUEEZEFS_IL_SPINS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
        {
            Some(v) => (v, v),
            None => (WAIT_SPINS_DEFAULT, WAIT_SPINS_PARKY),
        }
    })
}

/// `SQUEEZEFS_IL_MAX_RUN_SLOTS` — TESTING ONLY: cap on every claimed
/// run's slot count, so large ops chunk to `cap × slab` bytes and
/// pipeline their flights. This is the write-amplification rig's
/// deterministic stand-in for field slot fragmentation (a fleet under
/// arena churn degrades run length the same way — P3's
/// "fragmentation degrades run length, never refuses" pin);
/// unset/0 = uncapped (the production posture).
fn max_run_slots_cap() -> u32 {
    static CAP: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("SQUEEZEFS_IL_MAX_RUN_SLOTS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0)
    })
}

/// Ctl-socket receive deadline (a dead daemon must not hang a bind).
const CTL_RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// One ring op's outcome, as the interposer ladder consumes it
/// (§5.4.1): served (possibly short — POSIX-legal), a daemon errno
/// (kernel-identical for that op), or "take the real call".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingOutcome {
    Served(usize),
    Errno(i32),
    Fallthrough,
}

/// One in-flight no-wait ring op (the libaio ring lane). Holds the
/// claimed slot + its generation; consumed exactly once by
/// [`Session::poll_ticket`], or abandoned (never released — the slot is
/// GC'd with the session, §5.7 — recycling a possibly-still-completing
/// slot would hand a stale completion to a future op).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpTicket {
    slot: u32,
    gen: u64,
}

/// One admitted park entry — the slot state word plus the expected
/// value (`FUTEX_WAIT` admission semantics per word). Produced by
/// [`Session::ticket_wait_entry`], consumed by [`wait_any`]; entries
/// may span sessions (the aio pending set shards fds across them —
/// futex words are just addresses in this process's mapping).
#[derive(Clone, Copy)]
pub struct WaitEntry<'a> {
    word: &'a AtomicU32,
    expected: u32,
}

/// How the shim entered this process (SDK Tier 1 direct-link support,
/// `docs/design-sdk.md` §4). Purely diagnostic: the shim is ctor-free
/// and behaves identically under both loaders — the bootstrap path
/// prints one line in linked mode so operators (and the preload gate's
/// 2b-linked row) can tell the deployments apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMode {
    /// `LD_PRELOAD` names this shim.
    Preload,
    /// The dynamic linker pulled us in some other way: a DT_NEEDED
    /// direct link (`-lsqueezefs_il`), `dlopen`, or `/etc/ld.so.preload`.
    /// (A setuid/AT_SECURE binary whose *ignored* environment still
    /// names the shim misreports as `Preload` — the line is diagnostic,
    /// never a correctness input.)
    Linked,
}

/// Pure classification: does an `LD_PRELOAD` value name this shim?
/// glibc splits the list on colons and spaces; entries match on their
/// BASENAME (a directory component named like the shim must not
/// match), by prefix (packagers ship versioned `libsqueezefs_il.so.X`
/// names behind symlink chains).
pub fn load_mode_from(ld_preload: Option<&str>) -> LoadMode {
    let Some(list) = ld_preload else {
        return LoadMode::Linked;
    };
    for entry in list.split([':', ' ']).filter(|e| !e.is_empty()) {
        let base = entry.rsplit('/').next().unwrap_or(entry);
        if base.starts_with("libsqueezefs_il") {
            return LoadMode::Preload;
        }
    }
    LoadMode::Linked
}

/// Process-level detection (reads the environment on every call —
/// callers gate the announce on a `Once`).
pub fn load_mode() -> LoadMode {
    load_mode_from(std::env::var("LD_PRELOAD").ok().as_deref())
}

/// A successful bind's grant (mirrors the daemon's `BindOk`).
#[derive(Debug, Clone, Copy)]
pub struct BindGrant {
    pub binding_id: u64,
    pub ino: u64,
    pub read_ok: bool,
    pub write_ok: bool,
}

impl BindGrant {
    /// The fd-table entry for this grant, tagged with the session's
    /// registry token (how the closer routes the last-ref unbind).
    pub fn to_binding(self, session: usize) -> Binding {
        Binding {
            binding_id: self.binding_id,
            ino: self.ino,
            read_ok: self.read_ok,
            write_ok: self.write_ok,
            session,
        }
    }
}

/// Why a session could not establish / a bind failed. The interposers
/// branch on "did it work"; the variants carry the attribution the
/// refusal stderr line surfaces ([`SessionError::describe`] — user
/// directive 2026-07-25: the line must name the actual cause).
#[derive(Debug)]
pub enum SessionError {
    /// Client-side KD-7 pre-check: blob identity does not match ours
    /// (skew or degenerate identity without the dev override).
    VersionSkew,
    /// Socket/sendmsg/recvmsg failure (errno).
    Socket(i32),
    /// The daemon refused (class code as sent).
    Refused(u32),
    /// Malformed reply / missing fd / header validation failure.
    Protocol,
    /// mmap failure.
    Map(i32),
    /// Session is poisoned (bind on a poisoned session).
    Poisoned,
    /// VAL-4 step 2: the process answering the socket the MOUNT named is
    /// neither root nor the mount owner — the credential fd is not sent.
    PeerUntrusted { peer_uid: u32, mount_uid: u32 },
    /// VAL-4 step 3: the received session memfd is missing one of
    /// `F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW` (or the seals could
    /// not be read) — a mapping the other end can still resize.
    Unsealed { seals: i32 },
    /// VAL-4 step 1: the advertised path rendezvous does not resolve
    /// under a directory an accepted daemon could own — not connected.
    UntrustedRendezvous,
}

impl SessionError {
    /// Stable per-cause code — the [`RefusalOnce`] dedup key half
    /// (distinct causes on one mount must each print once). Socket/map
    /// errnos and daemon refusal classes fold into disjoint ranges so
    /// no two causes collide.
    pub fn reason_code(&self) -> u32 {
        match self {
            Self::VersionSkew => 1,
            Self::Protocol => 2,
            Self::Poisoned => 3,
            // VAL-4 ladder rungs — distinct keys so a mount that refuses
            // for one authentication cause and later another prints both.
            Self::PeerUntrusted { .. } => 4,
            Self::Unsealed { .. } => 5,
            Self::UntrustedRendezvous => 6,
            Self::Socket(e) => 0x1000 | (*e as u32 & 0xFFF),
            Self::Map(e) => 0x2000 | (*e as u32 & 0xFFF),
            Self::Refused(c) => 0x4000 | (*c & 0xFFF),
        }
    }

    /// The reason text for the refusal stderr line (user directive
    /// 2026-07-25): names the ACTUAL cause — and where one exists, the
    /// remedy. `daemon_abi`/`daemon_commit` come from the bootstrap
    /// blob; `my_commit` is this shim's build identity.
    pub fn describe(&self, daemon_abi: u32, daemon_commit: &str, my_commit: &str) -> String {
        match self {
            Self::VersionSkew => {
                // Recompute the establish pre-check's sub-cause in its
                // check order (abi → commit → degenerate identity).
                if daemon_abi != squeezefs_ipc::layout::IPC_ABI {
                    format!(
                        "ipc abi mismatch (shim {}, daemon {daemon_abi})",
                        squeezefs_ipc::layout::IPC_ABI
                    )
                } else if daemon_commit != my_commit {
                    format!("build mismatch (shim {my_commit}, daemon {daemon_commit})")
                } else {
                    format!(
                        "dev build identity ({my_commit}) — set SQUEEZEFS_IPC_ALLOW_DEV=1 \
                         on both ends"
                    )
                }
            }
            Self::Socket(0) => "daemon closed the ctl socket".into(),
            Self::Socket(e) => format!("ctl socket unreachable (os error {e})"),
            Self::Refused(c) => refuse_reason(*c),
            Self::Protocol => "malformed ctl reply / session layout".into(),
            Self::Map(e) => format!("session shm map failed (os error {e})"),
            Self::Poisoned => "session poisoned".into(),
            Self::PeerUntrusted {
                peer_uid,
                mount_uid,
            } => format!(
                "daemon authentication failed: socket peer uid {peer_uid} is neither \
                 root nor the mount owner ({mount_uid}) — not sending the credential fd"
            ),
            Self::Unsealed { seals } => format!(
                "daemon authentication failed: session memfd is not sealed \
                 (F_GET_SEALS = {seals:#x}; need SEAL|SHRINK|GROW)"
            ),
            Self::UntrustedRendezvous => "daemon authentication failed: the advertised \
                 socket path does not resolve under a root- or owner-private directory"
                .into(),
        }
    }
}

/// Reason text for one daemon refusal class (shared by the HELLO ladder
/// via [`SessionError::describe`] and the BIND refusal line). Unknown
/// classes surface their number — a newer daemon's refusal must never
/// hide.
pub fn refuse_reason(class: u32) -> String {
    use squeezefs_ipc::wire::RefuseClass as C;
    match C::from_u32(class) {
        Ok(C::Disabled) => {
            "interception not armed on this mount (mount with --interception)".into()
        }
        Ok(C::Version) => {
            // Daemon-side skew with the client pre-check passed means a
            // degenerate-identity refusal on the DAEMON side (its
            // override is per-end).
            "daemon refused: build/abi skew or dev identity \
             (daemon needs SQUEEZEFS_IPC_ALLOW_DEV=1 too?)"
                .into()
        }
        Ok(C::Nonce) => "daemon refused: stale bootstrap nonce".into(),
        Ok(C::Flags) => "daemon refused: credential fd screen (flags/type)".into(),
        Ok(C::Mode) => "daemon refused: credential fd is not on this mount".into(),
        Ok(C::Budget) => "daemon refused: session budget / admission cap".into(),
        Ok(C::Peercred) => "daemon refused: peer credential mismatch".into(),
        Ok(C::Internal) => "daemon refused: internal daemon failure (see daemon log)".into(),
        Err(_) => format!("daemon refused: unknown class {class}"),
    }
}

/// The message a shim announces exactly once when the KD-7 skew gate has
/// been relaxed (ENG-11). Pure so the gate's contract test can assert the
/// text without arming the lever.
pub fn allow_dev_notice() -> &'static str {
    "squeezefs-il: SQUEEZEFS_IPC_ALLOW_DEV is set — the KD-7 build-commit \
     skew gate is RELAXED (degenerate `unknown`/`-dirty` identities admitted). \
     Dev boxes only: a mismatched daemon/shim pair is undefined behavior.\n"
}

/// `SQUEEZEFS_IPC_ALLOW_DEV` under the shared convention, announced ONCE
/// per process on engagement (ENG-11 — it used to relax the skew gate
/// silently on both ends; `SQUEEZEFS_FUSE_NO_KILLPRIV` was the precedent
/// for saying so out loud). A malformed value keeps the safe default (off):
/// a shim never kills its host application over an env typo.
pub fn allow_dev_lever() -> bool {
    let on = crate::env_knob_core::parse_bool(
        "SQUEEZEFS_IPC_ALLOW_DEV",
        std::env::var("SQUEEZEFS_IPC_ALLOW_DEV").ok().as_deref(),
    )
    .ok()
    .flatten()
    .unwrap_or(false);
    if on {
        static ANNOUNCED: Once = Once::new();
        ANNOUNCED.call_once(|| {
            let msg = allow_dev_notice();
            // SAFETY: plain write(2) to stderr; best-effort, establish
            // context only (never a signal handler).
            unsafe {
                libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
            }
        });
    }
    on
}

/// Once-per-(mount, reason) print gate for the refusal lines: the
/// establish ladder retries on every eligible open BY DESIGN (the mount
/// stays ours — never negative-cached), so without this gate a
/// 32-thread benchmark prints a line per open (the field report).
///
/// A tiny mutexed set, NOT lock-free: exactly-once matters (the gate
/// asserts the count) and the refusal path is control-plane, runs only
/// in establish/bind context (never a signal handler — the §5.4
/// AS-safety constraint binds `close`/the fd table, not this path).
pub struct RefusalOnce {
    seen: Mutex<Vec<(u64, u32)>>,
}

/// Capacity bound: beyond it the set stops REMEMBERING, never stops
/// PRINTING (a new reason past capacity stays loud — loud beats lossy;
/// realistic processes see a handful of (mount, reason) pairs).
const REFUSAL_ONCE_CAP: usize = 128;

impl Default for RefusalOnce {
    fn default() -> Self {
        Self::new()
    }
}

impl RefusalOnce {
    pub const fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
        }
    }

    /// `true` exactly once per distinct `(dev, code)` key (then the
    /// caller prints); `false` on every repeat.
    pub fn first(&self, dev: u64, code: u32) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        if seen.iter().any(|&(d, c)| d == dev && c == code) {
            return false;
        }
        if seen.len() < REFUSAL_ONCE_CAP {
            seen.push((dev, code));
        }
        true
    }
}

pub struct Session {
    base: *mut u8,
    layout: SessionLayout,
    geometry: Geometry,
    /// Per-op payload ceiling: `min(max_op_bytes, arena_bytes / slots)` —
    /// the slab-per-slot discipline (each claimed slot owns the arena
    /// window `[slot × slab, slot × slab + slab)`; no cross-thread arena
    /// coordination exists or is needed).
    slab: u64,
    /// The hybrid lane gate's resolved threshold for this session
    /// (`crate::lane_gate` — derived from the memBW probe and this
    /// geometry, `SQUEEZEFS_IL_KERNEL_LANE_MIN` override verbatim;
    /// 0 = gate off). Published to the client stats page at establish.
    lane_gate_min: u64,
    ctl: Mutex<CtlSocket>,
    /// Raw ctl fd for the AS-safe poison `shutdown(2)` (the mutex-held
    /// owner is not lockable from an atfork handler).
    ctl_fd: RawFd,
    op_timeout: Duration,
    slot_hint: AtomicU32,
    /// Adaptive-spin hint: did this session's last completed op PARK?
    /// (One relaxed bit — a heuristic, never a correctness input.)
    last_parked: AtomicBool,
    poisoned: AtomicBool,
    /// Header generation observed at map time; a daemon bump = poison.
    my_generation: u64,
}

// SAFETY: `base` is a shared mapping accessed only through atomics
// (header/ring/slots) and bounded raw copies (arena slabs, each owned by
// the thread holding that slot's claim); the pointer itself is immutable
// after establish. Ctl I/O is mutex-serialized.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

struct CtlSocket {
    fd: RawFd,
}

impl Session {
    /// Establish a session from a decoded bootstrap blob + a screened
    /// credential fd (any bound-eligible fd on the mount). `my_commit`
    /// is this shim's build identity (KD-7 equality with the daemon).
    ///
    /// `mount_uid` is the **mount root's `st_uid`** — the `fstat` the
    /// interposer already performed on `cred_fd` before classifying it
    /// (`interpose.rs`, never re-stat'ed here). It is the trust anchor of
    /// the VAL-4 daemon-authentication ladder: the blob (socket name
    /// included) comes from the mount, so the shim must independently
    /// establish that whoever answers is root or that owner.
    pub fn establish(
        blob: &BootstrapBlob,
        cred_fd: RawFd,
        my_commit: &str,
        mount_uid: u32,
    ) -> Result<Session, SessionError> {
        let ms = std::env::var("SQUEEZEFS_IL_OP_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0);
        Self::establish_inner(
            blob,
            cred_fd,
            my_commit,
            mount_uid,
            ms.map(Duration::from_millis).unwrap_or(OP_TIMEOUT_DEFAULT),
        )
    }

    /// Test seam: explicit per-op deadline (the §5.4.1 bounded-wait pin
    /// drives it to a small value against a stalled daemon).
    pub fn establish_with_op_timeout_ms(
        blob: &BootstrapBlob,
        cred_fd: RawFd,
        my_commit: &str,
        mount_uid: u32,
        timeout_ms: u64,
    ) -> Result<Session, SessionError> {
        Self::establish_inner(
            blob,
            cred_fd,
            my_commit,
            mount_uid,
            Duration::from_millis(timeout_ms),
        )
    }

    fn establish_inner(
        blob: &BootstrapBlob,
        cred_fd: RawFd,
        my_commit: &str,
        mount_uid: u32,
        op_timeout: Duration,
    ) -> Result<Session, SessionError> {
        // Client-side KD-7 pre-check: skip the doomed round trip (the
        // daemon enforces the same law authoritatively).
        let allow_dev = allow_dev_lever();
        if blob.abi != squeezefs_ipc::layout::IPC_ABI
            || blob.build_commit != my_commit
            || ((build_commit_degenerate(&blob.build_commit) || build_commit_degenerate(my_commit))
                && !allow_dev)
        {
            return Err(SessionError::VersionSkew);
        }

        // OQ-6 connect ladder: abstract first (same-netns fast path,
        // zero residue), then the advertised filesystem-path socket —
        // the container-netns rendezvous (abstract names are per netns;
        // the path rides any shared mount surface). Both rungs speak
        // the identical protocol; failure of both is the
        // bind_refused{socket} outcome.
        // (Last-attempt error semantics, like a connect retry chain.)
        //
        // VAL-4 step 1 (P0): the PATH rung is a filesystem rendezvous the
        // blob names, so it is only followed when it resolves under a
        // directory an accepted daemon could own ([`path_rung_trusted`]).
        // The abstract rung needs no such screen (no filesystem residue,
        // no permission bits) — for both rungs the peer check below is
        // what actually authorizes.
        let sock = match connect_abstract(&blob.socket) {
            Ok(fd) => fd,
            Err(_) if !blob.socket_path.is_empty() => {
                path_rung_trusted(&blob.socket_path, mount_uid)?;
                connect_path(&blob.socket_path)?
            }
            Err(e) => return Err(e),
        };
        let sock_guard = FdGuard(sock);
        set_recv_timeout(sock, CTL_RECV_TIMEOUT);

        // VAL-4 step 2 (P0) — AUTHENTICATE THE DAEMON BEFORE THE
        // CREDENTIAL FD IS SENT. Everything about this rendezvous came
        // from the mount (§5.2: the blob is answered by the filesystem),
        // so the shim independently establishes that whoever accepted
        // the connection is either root or the owner of the mount root
        // (`mount_uid` — the `fstat` the interposer already performed on
        // the credential fd). A daemon presenting any other uid gets
        // nothing: no HELLO, no `SCM_RIGHTS`, and the fd stays kernel-
        // served (§5.4.2 fallback-is-correctness).
        //
        // Deliberately NO third disjunct (e.g. "or our own uid"): the
        // conservative predicate can only cost interception on mounts
        // served by a third-party service account, and the cost is
        // passthrough — never an app-visible error. The daemon's own
        // SO_PEERCRED check on us (§5.2) is the mirror of this one; the
        // possession-of-the-fd argument only holds if BOTH ends know who
        // they are talking to.
        let peer_uid = peer_uid(sock)?;
        if peer_uid != 0 && peer_uid != mount_uid {
            return Err(SessionError::PeerUntrusted {
                peer_uid,
                mount_uid,
            });
        }

        // SAFETY: plain getpid/getuid.
        let (pid, uid) = unsafe { (libc::getpid() as u32, libc::getuid()) };
        send_ctl(
            sock,
            &CtlMsg::Hello {
                abi: squeezefs_ipc::layout::IPC_ABI,
                pid,
                uid,
                build_commit: my_commit.to_string(),
                nonce: blob.nonce,
            },
            Some(cred_fd),
        )?;
        let (reply, memfd) = recv_ctl(sock)?;
        let geometry = match reply {
            CtlMsg::SessionOk { geometry } => geometry,
            CtlMsg::Refuse { class } => return Err(SessionError::Refused(class as u32)),
            _ => return Err(SessionError::Protocol),
        };
        let memfd = memfd.ok_or(SessionError::Protocol)?;
        let memfd_guard = FdGuard(memfd);

        // VAL-4 step 3 (P0): the session memfd must carry the seals the
        // daemon's `create_session_shm` applies. Without F_SEAL_SHRINK a
        // hostile (or buggy) peer can `ftruncate` the file out from under
        // this mapping AFTER we validated the header — every later slot /
        // arena access then faults SIGBUS inside the app's own thread,
        // which is neither a POSIX-legal outcome nor recoverable by the
        // §5.4.1 ladder. F_SEAL_GROW + F_SEAL_SEAL keep the geometry and
        // the seal set itself immutable for the session's life.
        require_seals(memfd)?;
        let layout = SessionLayout::compute(&geometry).map_err(|_| SessionError::Protocol)?;

        // Shared mapping of the sealed memfd, full layout length —
        // PMD-aligned when possible (near-zero-copy 2026-07-31: the
        // daemon's admission-time collapse makes the memfd's page-cache
        // pages huge; this mapping only maps them through PMDs when its
        // base is 2 MiB-aligned). Plain mmap fallback on any refusal.
        let base = match crate::thp::map_shared_pmd_aligned(memfd, layout.total_bytes as usize) {
            Some(p) => p as *mut libc::c_void,
            None => {
                // SAFETY: shared mapping of the sealed memfd, full layout
                // length.
                let p = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        layout.total_bytes as usize,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        memfd,
                        0,
                    )
                };
                if p == libc::MAP_FAILED {
                    // SAFETY: errno read directly after the failing call.
                    return Err(SessionError::Map(unsafe { *libc::__errno_location() }));
                }
                p
            }
        };
        // Session-arena THP (near-zero-copy 2026-07-31), shim posture:
        // advise-only — the daemon's admission-time populate+collapse
        // made the memfd's page-cache pages PMD-sized where granted;
        // MADV_HUGEPAGE here lets this mapping's faults map them huge.
        // Best-effort inside an arbitrary app: refusals are invisible.
        // `SQUEEZEFS_IPC_ARENA_THP=0` disables (the daemon's lever's
        // client half).
        if crate::env_knob_core::parse_bool(
            "SQUEEZEFS_IPC_ARENA_THP",
            std::env::var("SQUEEZEFS_IPC_ARENA_THP").ok().as_deref(),
        )
        .ok()
        .flatten()
        .unwrap_or(true)
        {
            let _ = crate::thp::advise_hugepages(
                base as *mut u8,
                layout.total_bytes as usize,
                crate::thp::ThpMode::Advise,
            );
        }
        // The mapping holds the memory; the fd is no longer needed.
        drop(memfd_guard);

        // SAFETY: header page at offset 0 of a mapping sized by layout.
        let header = unsafe { &*(base as *const SessionHeader) };
        if header.validate().is_err() || header.geometry != geometry {
            // SAFETY: unmapping the mapping created above.
            unsafe { libc::munmap(base, layout.total_bytes as usize) };
            return Err(SessionError::Protocol);
        }
        let my_generation = header.generation.load(Ordering::Acquire);

        // The ONE slab law (`Geometry::slot_slab` — shared with the
        // daemon side): LBA-floored so every `slot × slab` arena offset
        // stays DMA-eligible (the 2026-08-04 bounce fix).
        let slab = geometry.slot_slab();
        if slab == 0 {
            // SAFETY: unmapping the mapping created above.
            unsafe { libc::munmap(base, layout.total_bytes as usize) };
            return Err(SessionError::Protocol);
        }

        // Hybrid lane gate (D14 corollary): resolve this session's
        // kernel-lane threshold — memBW probe (process-memoized, first
        // establish pays ~1 ms) through the shared derivation, railed by
        // THIS geometry, env override verbatim — and publish it as the
        // stats-page gauge before any op can consult it.
        let lane_gate_min =
            crate::lane_gate::kernel_lane_min_for_geometry(slab, u64::from(geometry.max_op_bytes));

        let sock = sock_guard.release();
        let session = Session {
            base: base as *mut u8,
            layout,
            geometry,
            slab,
            lane_gate_min,
            ctl: Mutex::new(CtlSocket { fd: sock }),
            ctl_fd: sock,
            op_timeout,
            slot_hint: AtomicU32::new(0),
            last_parked: AtomicBool::new(false),
            poisoned: AtomicBool::new(false),
            my_generation,
        };
        session
            .stats_page()
            .lane_gate_threshold_bytes
            .store(lane_gate_min, Ordering::Relaxed);
        Ok(session)
    }

    /// Bind `fd` for the data plane. Ctl round trip (control-plane
    /// mutex; see the module locking posture).
    pub fn bind(&self, fd: RawFd) -> Result<BindGrant, SessionError> {
        if self.poisoned() {
            return Err(SessionError::Poisoned);
        }
        let ctl = self.ctl.lock().expect("ctl mutex never poisons");
        send_ctl(ctl.fd, &CtlMsg::Bind, Some(fd))?;
        let (reply, none) = recv_ctl(ctl.fd)?;
        drop(ctl);
        if none.is_some() {
            // SAFETY: closing an fd we own (unexpected attachment).
            unsafe { libc::close(none.unwrap_or(-1)) };
            return Err(SessionError::Protocol);
        }
        match reply {
            CtlMsg::BindOk {
                binding_id,
                ino,
                read_ok,
                write_ok,
            } => Ok(BindGrant {
                binding_id,
                ino,
                read_ok,
                write_ok,
            }),
            CtlMsg::BindRefused { class } => Err(SessionError::Refused(class as u32)),
            _ => Err(SessionError::Protocol),
        }
    }

    /// Release a binding (last-close path). Best-effort and silent: a
    /// poisoned session never emits ctl traffic (§5.4.1 normative — the
    /// atfork child must not unbind bindings the parent still uses).
    pub fn unbind(&self, binding_id: u64) {
        if self.poisoned() {
            return;
        }
        if let Ok(ctl) = self.ctl.lock() {
            let _ = send_ctl(ctl.fd, &CtlMsg::Unbind { binding_id }, None);
        }
    }

    /// AS-safe **same-process** poison: one flag store + `shutdown(2)`
    /// on the ctl socket (kills traffic without freeing the fd number,
    /// so a racing ctl call can never hit a reused fd). The op-timeout
    /// and panic paths call this. **Never call from an atfork child** —
    /// shutdown acts on the file description, which fork SHARES with
    /// the parent: it would sever the parent's live session. The child
    /// path is [`Session::poison_child`].
    pub fn poison(&self) {
        self.poisoned.store(true, Ordering::SeqCst);
        // SAFETY: shutdown(2) on our own ctl fd; AS-safe, idempotent.
        unsafe { libc::shutdown(self.ctl_fd, libc::SHUT_RDWR) };
    }

    /// AS-safe **atfork-child** poison (§5.4.1 fork row): one flag store
    /// plus `close(2)` of the child's inherited fd-table COPY. Close is
    /// per-process (the parent's description stays live), and dropping
    /// the child's ref is exactly what restores parent-death EOF
    /// semantics (§5.7: a surviving child must not hold the socket
    /// open). The poisoned flag makes every later ctl/ring path in the
    /// child a no-op, so the closed (and possibly reused) fd number is
    /// never touched again through this session.
    pub fn poison_child(&self) {
        self.poisoned.store(true, Ordering::SeqCst);
        // SAFETY: close(2) on the child's own fd-table entry; AS-safe.
        // Called exactly once per fork (the atfork handler), before any
        // other thread exists in the child (fork gives it one thread).
        unsafe { libc::close(self.ctl_fd) };
    }

    pub fn poisoned(&self) -> bool {
        if self.poisoned.load(Ordering::SeqCst) {
            return true;
        }
        // A daemon-side generation bump is the poison broadcast (§5.7).
        if self.header().generation.load(Ordering::Acquire) != self.my_generation {
            self.poisoned.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }

    /// Positional ring read into `buf` — the read twin of
    /// [`Session::ring_pwrite`]'s DIALED P3 **large-op ring economy**
    /// (read-saturation campaign, 2026-07-29):
    ///
    /// - Chunks size to the full `max_op_bytes` window via **contiguous
    ///   multi-slab slot runs** (`claim_run`) — a 1 MiB read is ONE ring
    ///   op at default geometry, not 16 serial slab RTTs (the daemon has
    ///   always validated `len ≤ max_op_bytes` + arena bounds; the il
    ///   streaming-read collapse was purely this client's chunk sizing:
    ///   measured 16 fabric round trips per MiB of sequential stream).
    /// - Every chunk of the op is **submitted before any is waited on**
    ///   (pipelined flights, reaped in offset order), so ops larger than
    ///   `max_op_bytes` overlap their windows instead of serializing.
    ///
    /// POSIX prefix semantics: the reported count is the contiguous
    /// served prefix ending at the first short (EOF) or failed chunk;
    /// later in-flight chunks are drained and their results ignored.
    /// Arena bytes copy out only AFTER the chunk's completion is
    /// consumed (`wait_consume`'s Acquire orders the daemon's arena
    /// write before the copy — same rule as the serial path).
    pub fn ring_pread(&self, binding_id: u64, buf: &mut [u8], offset: u64) -> RingOutcome {
        let slab = self.slab as usize;
        // Single-slab ops (the ≤64 KiB hot rows) keep the allocation-free
        // serial path: one claim, one wait, zero flight bookkeeping.
        if buf.len() <= slab {
            return match self.one_op(OP_READ, binding_id, offset, buf.len(), None) {
                OpResult::Done { slot, n } => {
                    let n = n.min(buf.len());
                    self.slab_read(slot, &mut buf[..n]);
                    self.release_slot(slot);
                    RingOutcome::Served(n)
                }
                OpResult::Errno(e) => RingOutcome::Errno(e),
                OpResult::Fallthrough => RingOutcome::Fallthrough,
            };
        }
        let max_chunk = (self.geometry.max_op_bytes as usize).max(1);
        let mut flights: std::collections::VecDeque<WriteFlight> =
            std::collections::VecDeque::new();
        let mut submitted = 0usize; // bytes handed to flights
        let mut done = 0usize; // contiguous served prefix
        let mut short = false; // a chunk completed short — stop extending
        let mut failed: Option<i32> = None; // first failure's errno
        loop {
            // Submit phase: claim + STAGE every chunk the slot table
            // allows, then ring-push the batch under ONE doorbell (the
            // shim-parity batch-publish shape; reads have no arena copy
            // to stage, so this is claim + descriptor publish only).
            let mut staged: Vec<WriteFlight> = Vec::new();
            while failed.is_none() && !short && submitted < buf.len() && !self.poisoned() {
                let remaining = buf.len() - submitted;
                let want_bytes = remaining.min(max_chunk);
                let want_slots = want_bytes.div_ceil(slab).max(1) as u32;
                let Some((base, gen, run)) = self.claim_run(want_slots) else {
                    break;
                };
                let chunk = want_bytes.min(run as usize * slab);
                let slot = self.slot(base);
                slot.publish_descriptor(&SlotDescriptor {
                    op: OP_READ,
                    flags: 0,
                    binding: binding_id,
                    offset: offset + submitted as u64,
                    len: chunk as u32,
                    arena_off: u64::from(base) * self.slab,
                });
                slot.core.publish_submitted();
                staged.push(WriteFlight {
                    base,
                    gen,
                    run,
                    chunk,
                });
                submitted += chunk;
            }
            if !staged.is_empty() {
                for fl in staged {
                    if !self.ring().push(fl.base) {
                        // Unreachable for an honest client (slots ≤ ring
                        // entries) — our state is corrupt; poison loudly.
                        self.poison();
                        break;
                    }
                    flights.push_back(fl);
                }
                let header = self.header();
                header.doorbell.fetch_add(1, Ordering::Release);
                if header.daemon_parked.load(Ordering::SeqCst) != 0 {
                    futex_wake(&header.doorbell);
                }
            }
            // Reap phase: consume the oldest (lowest-offset) flight.
            let Some(fl) = flights.pop_front() else {
                break; // nothing in flight and nothing submittable
            };
            match self.wait_consume(fl.base, fl.gen) {
                WaitConsume::Done(r) => {
                    if r < 0 {
                        if failed.is_none() && !short {
                            failed = Some((-r) as i32);
                        }
                    } else if failed.is_none() && !short {
                        let n = (r as usize).min(fl.chunk);
                        // The flight's window starts at the prefix edge by
                        // reap order (flights reap lowest-offset first and
                        // `done` only advances on full chunks).
                        self.slab_read_run(fl.base, &mut buf[done..done + n]);
                        done += n;
                        if n < fl.chunk {
                            short = true; // EOF short read ends the prefix
                        }
                    }
                    self.release_run(fl.base, fl.run);
                }
                WaitConsume::Abandoned => {
                    // §5.4.1 deadline (session now poisoned) or a poison
                    // broadcast: never reuse the run's slots — the daemon
                    // may complete into the run's arena arbitrarily late.
                    return if done > 0 {
                        RingOutcome::Served(done)
                    } else {
                        RingOutcome::Fallthrough
                    };
                }
            }
        }
        if let Some(e) = failed {
            return if done > 0 {
                RingOutcome::Served(done)
            } else if e == libc::EINVAL {
                // Protocol-class reject on the first chunk: OUR
                // bookkeeping diverged — the real call is the correct
                // answer (mirrors `consume`).
                RingOutcome::Fallthrough
            } else {
                RingOutcome::Errno(e)
            };
        }
        if done > 0 || buf.is_empty() {
            RingOutcome::Served(done)
        } else {
            // No slot was ever claimable (client-visible backpressure) or
            // the session is poisoned: this op takes the real call.
            RingOutcome::Fallthrough
        }
    }

    /// Positional ring write from `buf` — the DIALED P3 **large-op ring
    /// economy** (`.benchmarks/2026-07-27-write-side-economy.md`):
    ///
    /// - Chunks size to the full `max_op_bytes` window via **contiguous
    ///   multi-slab slot runs** (`claim_run`) — a 1 MiB write is ONE ring
    ///   op at default geometry, not 16 serial slab RTTs (the daemon has
    ///   always validated `len ≤ max_op_bytes`; the collapse was purely
    ///   this client's chunk sizing).
    /// - Every chunk of the op is **submitted before any is waited on**
    ///   (pipelined flights, reaped in offset order), so ops larger than
    ///   `max_op_bytes` overlap their windows instead of serializing.
    ///
    /// POSIX prefix semantics: the reported count is the contiguous acked
    /// prefix ending at the first short or failed chunk; later in-flight
    /// chunks are drained and their results ignored (their bytes may have
    /// landed — the same property as the kernel's split out-of-order
    /// O_DIRECT WRITE pipeline, converged by the caller's retry).
    pub fn ring_pwrite(&self, binding_id: u64, buf: &[u8], offset: u64) -> RingOutcome {
        let slab = self.slab as usize;
        // Single-slab ops (the ≤64 KiB hot rows — every W1 patch-path
        // write) keep the allocation-free serial path: one claim, one
        // wait, zero flight bookkeeping — byte-identical to the pre-P3
        // shape.
        if buf.len() <= slab {
            return match self.one_op(OP_WRITE, binding_id, offset, buf.len(), Some(buf)) {
                OpResult::Done { slot, n } => {
                    self.release_slot(slot);
                    RingOutcome::Served(n.min(buf.len()))
                }
                OpResult::Errno(e) => RingOutcome::Errno(e),
                OpResult::Fallthrough => RingOutcome::Fallthrough,
            };
        }
        let max_chunk = (self.geometry.max_op_bytes as usize).max(1);
        let mut flights: std::collections::VecDeque<WriteFlight> =
            std::collections::VecDeque::new();
        let mut submitted = 0usize; // bytes handed to flights
        let mut done = 0usize; // contiguous acked prefix
        let mut short = false; // a chunk completed short — stop extending
        let mut failed: Option<i32> = None; // first failure's errno
        loop {
            // Submit phase — TWO halves (shim-parity 2026-07-28, batch
            // publish): (a) claim + arena-copy + STAGE every chunk the
            // slot table allows, (b) ring-push them together under ONE
            // doorbell. The pre-batch shape (push + doorbell per chunk,
            // with a slab-run memcpy between pushes) let the daemon's
            // drain outrun the stream — sibling chunks of one block
            // landed in DIFFERENT drain passes, so the placed sever's
            // shared-assembly adoption raced the first merge (measured
            // ~50 % sever engagement at t16×4MiB). Arriving together,
            // the whole flight severs in one pass; it is also strictly
            // fewer doorbell/wake edges. `claim_run` refusing (all slots
            // busy) just ends the phase — reaping a flight below frees
            // slots and the loop re-enters here.
            let mut staged: Vec<WriteFlight> = Vec::new();
            while failed.is_none() && !short && submitted < buf.len() && !self.poisoned() {
                let remaining = buf.len() - submitted;
                let want_bytes = remaining.min(max_chunk);
                let want_slots = want_bytes.div_ceil(slab).max(1) as u32;
                let Some((base, gen, run)) = self.claim_run(want_slots) else {
                    break;
                };
                let chunk = want_bytes.min(run as usize * slab);
                self.slab_write_run(base, &buf[submitted..submitted + chunk]);
                let slot = self.slot(base);
                slot.publish_descriptor(&SlotDescriptor {
                    op: OP_WRITE,
                    flags: 0,
                    binding: binding_id,
                    offset: offset + submitted as u64,
                    len: chunk as u32,
                    arena_off: u64::from(base) * self.slab,
                });
                slot.core.publish_submitted();
                staged.push(WriteFlight {
                    base,
                    gen,
                    run,
                    chunk,
                });
                submitted += chunk;
            }
            if !staged.is_empty() {
                for fl in staged {
                    if !self.ring().push(fl.base) {
                        // Unreachable for an honest client (slots ≤ ring
                        // entries) — our state is corrupt; poison loudly.
                        // The submitted-but-unpushed run is abandoned with
                        // the session (never reused).
                        self.poison();
                        break;
                    }
                    flights.push_back(fl);
                }
                // ONE doorbell for the whole staged batch (§5.3 protocol
                // rule 1 is per-publication liveness; the coalescer
                // already made per-push doorbells elide-to-one — this
                // moves the batching to the publish side too).
                let header = self.header();
                header.doorbell.fetch_add(1, Ordering::Release);
                if header.daemon_parked.load(Ordering::SeqCst) != 0 {
                    futex_wake(&header.doorbell);
                }
            }
            // Reap phase: consume the oldest (lowest-offset) flight.
            let Some(fl) = flights.pop_front() else {
                break; // nothing in flight and nothing submittable
            };
            match self.wait_consume(fl.base, fl.gen) {
                WaitConsume::Done(r) => {
                    self.release_run(fl.base, fl.run);
                    if r < 0 {
                        if failed.is_none() && !short {
                            failed = Some((-r) as i32);
                        }
                    } else if failed.is_none() && !short {
                        let n = (r as usize).min(fl.chunk);
                        done += n;
                        if n < fl.chunk {
                            short = true;
                        }
                    }
                }
                WaitConsume::Abandoned => {
                    // §5.4.1 deadline (session now poisoned) or a poison
                    // broadcast: never reuse the run's slots — the daemon
                    // may complete into (or DMA-serve from) the run's
                    // arena arbitrarily late. Remaining flights are
                    // abandoned with the session.
                    return if done > 0 {
                        RingOutcome::Served(done)
                    } else {
                        RingOutcome::Fallthrough
                    };
                }
            }
        }
        if let Some(e) = failed {
            return if done > 0 {
                RingOutcome::Served(done)
            } else if e == libc::EINVAL {
                // Protocol-class reject on the first chunk: OUR
                // bookkeeping diverged — the real call is the correct
                // answer (mirrors `consume`).
                RingOutcome::Fallthrough
            } else {
                RingOutcome::Errno(e)
            };
        }
        if done > 0 || buf.is_empty() {
            RingOutcome::Served(done)
        } else {
            // No slot was ever claimable (client-visible backpressure) or
            // the session is poisoned: this op takes the real call.
            RingOutcome::Fallthrough
        }
    }

    // -----------------------------------------------------------------
    // one op: claim → publish → push → doorbell → spin/park → result
    // -----------------------------------------------------------------

    /// Claim + publish + push + doorbell — everything up to the wait.
    /// `None` = no slot (client-visible backpressure) or poisoned.
    fn submit_op(
        &self,
        op: u32,
        binding_id: u64,
        offset: u64,
        len: usize,
        payload: Option<&[u8]>,
    ) -> Option<(u32, u64)> {
        if self.poisoned() {
            return None;
        }
        let (slot_idx, gen) = self.claim_slot()?;
        let slot = self.slot(slot_idx);
        let arena_off = u64::from(slot_idx) * self.slab;
        if let Some(data) = payload {
            self.slab_write(slot_idx, data);
        }
        slot.publish_descriptor(&SlotDescriptor {
            op,
            flags: 0,
            binding: binding_id,
            offset,
            len: len as u32,
            arena_off,
        });
        slot.core.publish_submitted();
        if !self.ring().push(slot_idx) {
            // Unreachable for an honest client (slots ≤ ring_entries);
            // observing it means OUR state is corrupt — poison loudly.
            self.poison();
            return None;
        }
        // Doorbell (§5.3 protocol rule 1): publish first, then wake only
        // a parked daemon. If the daemon is mid-scan (parked flag clear),
        // its disarm→scan ordering finds our push without a wake; the
        // daemon's bounded park (≤ 5 ms) absorbs the residual race.
        let header = self.header();
        header.doorbell.fetch_add(1, Ordering::Release);
        if header.daemon_parked.load(Ordering::SeqCst) != 0 {
            futex_wake(&header.doorbell);
        }
        Some((slot_idx, gen))
    }

    // -----------------------------------------------------------------
    // no-wait tickets (the libaio ring lane, v1.1 OQ-1; also the
    // lifecycle suites' park-an-op-in-flight primitive)
    // -----------------------------------------------------------------

    /// Per-op payload ceiling (one ticket = one slot; the aio screen
    /// sizes ops against this before classifying them ring-eligible).
    pub fn slab_bytes(&self) -> u64 {
        self.slab
    }

    /// The hybrid lane gate's resolved threshold for this session
    /// (bytes; 0 = gate off): ops STRICTLY larger take the kernel FUSE
    /// lane (`crate::lane_gate::kernel_lane_route`).
    pub fn lane_gate_min(&self) -> u64 {
        self.lane_gate_min
    }

    /// Record one gate decision that routed an op to the kernel lane —
    /// the shim-side half of `ipc_lane_gate_kernel_{routes,bytes}` (the
    /// daemon sums the stats page into the stats-inode export; §5.3.1
    /// trust boundary: display-only over there).
    pub fn note_kernel_route(&self, bytes: u64) {
        self.stats_page().note_kernel_route(bytes);
    }

    /// Fire a positional read without waiting. `None` = no slot or
    /// poisoned (client-visible backpressure — the batch prefix ends
    /// there, §5.5.1). The claimed slot stays claimed until
    /// [`poll_ticket`](Self::poll_ticket) consumes it or the ticket is
    /// abandoned (slot GC'd with the session, §5.7).
    pub fn submit_pread_nowait(
        &self,
        binding_id: u64,
        len: usize,
        offset: u64,
    ) -> Option<OpTicket> {
        debug_assert!(len as u64 <= self.slab, "screened against slab_bytes()");
        let (slot, gen) = self.submit_op(OP_READ, binding_id, offset, len, None)?;
        Some(OpTicket { slot, gen })
    }

    /// Fire a positional write without waiting (payload copied to the
    /// slot's slab now — the caller's buffer is free the moment this
    /// returns, exactly libaio's contract after `io_submit`).
    pub fn submit_pwrite_nowait(
        &self,
        binding_id: u64,
        data: &[u8],
        offset: u64,
    ) -> Option<OpTicket> {
        debug_assert!(
            data.len() as u64 <= self.slab,
            "screened against slab_bytes()"
        );
        let (slot, gen) = self.submit_op(OP_WRITE, binding_id, offset, data.len(), Some(data))?;
        Some(OpTicket { slot, gen })
    }

    /// Non-consuming completion probe (the reap loop's pre-park spin):
    /// is this ticket's op DONE? Unlike [`poll_ticket`](Self::poll_ticket)
    /// the slot stays claimed — the caller still consumes exactly once.
    pub fn ticket_done(&self, t: OpTicket) -> bool {
        self.slot(t.slot).core.is_done_for(t.gen)
    }

    /// Completion-doorbell park entry (op-economy 2026-07-28; replaces
    /// the per-ticket slot-WAITER parks): register parked intent on the
    /// session's [`CqeDoorbell`] and snapshot the expected seq —
    /// register-then-snapshot, then the caller MUST re-scan its pending
    /// set (the disarm→scan law) before sleeping on the returned entry.
    /// One session = one wait word regardless of pending depth (the
    /// former shape built a `futex_waitv` array per PENDING TICKET —
    /// O(qd) setup per park — and made the daemon pay one wake syscall
    /// per completion toward the WAITER bits; the doorbell pays wakes
    /// only while a reaper is parked, elision loom-verified by
    /// `ipc_cqe_parked_reaper_never_stranded`). Balance every call with
    /// [`Self::cqe_park_end`].
    ///
    /// [`CqeDoorbell`]: squeezefs_ipc::cqe_core::CqeDoorbell
    pub fn cqe_park_begin(&self) -> WaitEntry<'_> {
        let cqe = &self.header().cqe;
        let expected = cqe.park_begin();
        WaitEntry {
            word: cqe.seq_word(),
            expected,
        }
    }

    /// Deregister a [`Self::cqe_park_begin`] after the wait returns
    /// (wake, EAGAIN, or timeout — every §5.3.1-rule-5-bounded exit).
    pub fn cqe_park_end(&self) {
        self.header().cqe.park_end();
    }

    /// Non-blocking completion probe. `Some(res)` CONSUMES the ticket
    /// (slot released): a successful read's payload is copied into
    /// `out` first (`res` bytes, capped by `out`), and `res` carries
    /// bytes-or-negative-errno — libaio `io_event.res` semantics.
    /// Post-acceptance there is no fallthrough: the op was already
    /// acknowledged as submitted, so even protocol-class rejects
    /// surface as their errno. `None` = still in flight.
    pub fn poll_ticket(&self, t: OpTicket, out: Option<&mut [u8]>) -> Option<i64> {
        let slot = self.slot(t.slot);
        if !slot.core.is_done_for(t.gen) {
            return None;
        }
        let r = slot.result();
        if r > 0 {
            if let Some(buf) = out {
                let n = (r as usize).min(buf.len());
                self.slab_read(t.slot, &mut buf[..n]);
            }
        }
        self.release_slot(t.slot);
        Some(r)
    }

    fn one_op(
        &self,
        op: u32,
        binding_id: u64,
        offset: u64,
        len: usize,
        payload: Option<&[u8]>,
    ) -> OpResult {
        let Some((slot_idx, gen)) = self.submit_op(op, binding_id, offset, len, payload) else {
            // Poisoned or full ring = client-visible backpressure
            // (§5.5.1): this op takes the real call; the binding stays.
            return OpResult::Fallthrough;
        };
        match self.wait_consume(slot_idx, gen) {
            WaitConsume::Done(_) => self.consume(slot_idx, gen),
            WaitConsume::Abandoned => OpResult::Fallthrough,
        }
    }

    /// Wait for one submitted slot's completion: bounded ADAPTIVE spin,
    /// then futex park with a hard deadline (§5.4.1). Sessions whose
    /// last op parked spin only the short floor — full-window spinning
    /// on handoff-heavy workloads is CPU theft from the daemon (see
    /// WAIT_SPINS_PARKY). `Done` returns the slot result WITHOUT
    /// releasing it (the caller consumes/releases); `Abandoned` means
    /// the deadline poisoned the session (or a poison broadcast landed)
    /// — the slot is never reused (the daemon may complete it
    /// arbitrarily late; recycling would hand that stale completion to
    /// a future op).
    fn wait_consume(&self, slot_idx: u32, gen: u64) -> WaitConsume {
        let slot = self.slot(slot_idx);
        let deadline = Instant::now() + self.op_timeout;
        let (window, parky) = wait_spins();
        let spins = if self.last_parked.load(Ordering::Relaxed) {
            parky
        } else {
            window
        };
        for _ in 0..spins {
            if slot.core.is_done_for(gen) {
                self.last_parked.store(false, Ordering::Relaxed);
                return WaitConsume::Done(slot.result());
            }
            std::hint::spin_loop();
        }
        self.last_parked.store(true, Ordering::Relaxed);
        loop {
            match slot.core.park_prepare() {
                ParkOutcome::Ready => return WaitConsume::Done(slot.result()),
                ParkOutcome::Park { expected } => {
                    let now = Instant::now();
                    if now >= deadline {
                        self.poison();
                        return WaitConsume::Abandoned;
                    }
                    let wait = (deadline - now).min(Duration::from_millis(50));
                    futex_wait(slot.core.state_futex_word(), expected, wait);
                    if slot.core.is_done_for(gen) {
                        return WaitConsume::Done(slot.result());
                    }
                    if self.poisoned() {
                        return WaitConsume::Abandoned;
                    }
                }
            }
        }
    }

    fn consume(&self, slot_idx: u32, gen: u64) -> OpResult {
        let slot = self.slot(slot_idx);
        debug_assert!(slot.core.is_done_for(gen));
        let r = slot.result();
        if r < 0 {
            self.release_slot(slot_idx);
            let e = (-r) as i32;
            // Protocol-class rejects mean OUR bookkeeping diverged from
            // the daemon (dead binding, bad descriptor) — kernel-identical
            // errno for EBADF (wrong direction is a real fd property);
            // EINVAL descriptor rejects fall through (the real call is
            // the correct answer for a shim-side bug).
            if e == libc::EINVAL {
                return OpResult::Fallthrough;
            }
            return OpResult::Errno(e);
        }
        OpResult::Done {
            slot: slot_idx,
            n: r as usize,
        }
    }

    fn claim_slot(&self) -> Option<(u32, u64)> {
        let slots = self.geometry.slots;
        let start = self.slot_hint.fetch_add(1, Ordering::Relaxed) % slots;
        for i in 0..slots {
            let idx = (start + i) % slots;
            if let Some(gen) = self.slot(idx).core.try_claim() {
                return Some((idx, gen));
            }
        }
        None
    }

    /// Claim a contiguous, non-wrapping run of up to `want` FREE slots —
    /// one large-op arena window (slot slabs are adjacent in the arena,
    /// so a run's window is `[base·slab, (base+run)·slab)`; runs never
    /// wrap the slot array because the arena window must be contiguous).
    /// Returns `(base, base_generation, run_len)` with `1 ≤ run_len ≤
    /// want` — a shorter run under fragmentation is progress, never a
    /// refusal. Only the BASE slot is ever submitted; the extension
    /// slots are CLAIMED arena holds the daemon never observes, released
    /// via [`release_run`](Self::release_run). `None` = not one slot
    /// free (client-visible backpressure).
    fn claim_run(&self, want: u32) -> Option<(u32, u64, u32)> {
        // `SQUEEZEFS_IL_MAX_RUN_SLOTS` (TESTING ONLY — the write-amplification
        // rig's fragmentation simulator, tests/write_amp_rig.sh): cap every
        // run's slot count so large ops chunk to `cap × slab` and pipeline
        // their flights exactly as a slot-fragmented fleet arrives in the
        // field (`.benchmarks/2026-07-27-shim-write-amplification.md`).
        // Unset/0 = uncapped (production posture).
        let want = match max_run_slots_cap() {
            0 => want,
            cap => want.min(cap),
        };
        let slots = self.geometry.slots;
        let start = self.slot_hint.fetch_add(1, Ordering::Relaxed) % slots;
        // Pass 1 — aligned stride: try bases at multiples of `want` first,
        // extending only from an aligned base. Concurrent large writers
        // then PACK runs instead of interleaving them (a random-start
        // greedy scan fragments the array: measured ring/op 1.86 instead
        // of 1.00 on the 16-thread 1 MiB row), and an aligned claimant
        // that loses one slot of its window loses to a whole run, not to
        // a stray mid-window claim.
        if want > 1 && slots >= want {
            let bases = slots / want;
            let first = (start / want) % bases;
            for i in 0..bases {
                let base = ((first + i) % bases) * want;
                if let Some(gen) = self.slot(base).core.try_claim() {
                    let mut run = 1u32;
                    while run < want && base + run < slots {
                        if self.slot(base + run).core.try_claim().is_some() {
                            run += 1;
                        } else {
                            break;
                        }
                    }
                    return Some((base, gen, run));
                }
            }
        }
        // Pass 2 — greedy: any FREE base, extend as far as possible
        // (fragmentation degrades run length, never refuses — the
        // fragmented-slots pin).
        for i in 0..slots {
            let base = (start + i) % slots;
            if let Some(gen) = self.slot(base).core.try_claim() {
                let mut run = 1u32;
                while run < want && base + run < slots {
                    if self.slot(base + run).core.try_claim().is_some() {
                        run += 1;
                    } else {
                        break;
                    }
                }
                return Some((base, gen, run));
            }
        }
        None
    }

    /// Release a completed flight's run: the consumed base (DONE → FREE)
    /// plus the never-submitted extension holds (CLAIMED → FREE).
    fn release_run(&self, base: u32, run: u32) {
        self.slot(base).core.release();
        for i in 1..run {
            self.slot(base + i).core.release_claimed();
        }
    }

    fn release_slot(&self, idx: u32) {
        self.slot(idx).core.release();
    }

    // -----------------------------------------------------------------
    // mapping accessors (mirrors of the daemon side, client trust rules)
    // -----------------------------------------------------------------

    fn header(&self) -> &SessionHeader {
        // SAFETY: header page at offset 0, written by the daemon before
        // the memfd was shared; validated at establish.
        unsafe { &*(self.base as *const SessionHeader) }
    }

    fn stats_page(&self) -> &ClientStatsPage {
        // SAFETY: the stats page region at `stats_off`, page-aligned and
        // page-sized by the layout; repr(C, align(4096)) protocol type,
        // zero-initialized by the fresh memfd (client-writable by design
        // — this side IS the client).
        unsafe { &*(self.base.add(self.layout.stats_off as usize) as *const ClientStatsPage) }
    }

    fn ring(&self) -> MpscRingView<'_> {
        // SAFETY: offsets from the layout that sized the mapping;
        // repr(C) protocol types; geometry validated at establish.
        unsafe {
            let tail = &*(self.base.add(self.layout.ring_off as usize) as *const AtomicU32);
            let cells = std::slice::from_raw_parts(
                self.base.add(self.layout.ring_cells_off as usize) as *const RingCell,
                self.geometry.ring_entries as usize,
            );
            MpscRingView::from_parts(tail, cells).expect("geometry validated at establish")
        }
    }

    fn slot(&self, i: u32) -> &IpcSlot {
        debug_assert!(i < self.geometry.slots);
        // SAFETY: bounds by claim_slot/geometry; slots at slots_off.
        unsafe {
            &*((self.base.add(self.layout.slots_off as usize) as *const IpcSlot).add(i as usize))
        }
    }

    fn slab_write(&self, slot_idx: u32, data: &[u8]) {
        debug_assert!(data.len() as u64 <= self.slab);
        self.slab_write_run(slot_idx, data);
    }

    /// Copy `data` into the contiguous arena window based at `base`'s
    /// slab — the multi-slab run write (`data` may span the run's whole
    /// window; the run's slots are all CLAIMED by this thread).
    fn slab_write_run(&self, base: u32, data: &[u8]) {
        let off = self.layout.arena_off + u64::from(base) * self.slab;
        debug_assert!(
            u64::from(base) * self.slab + data.len() as u64 <= self.geometry.arena_bytes,
            "run window inside the arena by claim_run construction"
        );
        // SAFETY: the run's slot slabs are adjacent and disjoint from
        // every other claimant's (all claimed by us), inside the arena
        // by the assert above.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.base.add(off as usize), data.len());
        }
    }

    fn slab_read(&self, slot_idx: u32, out: &mut [u8]) {
        debug_assert!(out.len() as u64 <= self.slab);
        self.slab_read_run(slot_idx, out);
    }

    /// Copy the contiguous arena window based at `base`'s slab out into
    /// `out` — the multi-slab run read (the run's slots are all CLAIMED
    /// by this thread; callers copy strictly AFTER the completion's
    /// Acquire and strictly BEFORE releasing the run, so no other
    /// claimant can overwrite the window mid-copy).
    fn slab_read_run(&self, base: u32, out: &mut [u8]) {
        let off = self.layout.arena_off + u64::from(base) * self.slab;
        debug_assert!(
            u64::from(base) * self.slab + out.len() as u64 <= self.geometry.arena_bytes,
            "run window inside the arena by claim_run construction"
        );
        // SAFETY: as slab_write; the daemon may race writes here only
        // for THIS op's completion, which is ordered by is_done_for's
        // Acquire before this copy.
        unsafe {
            std::ptr::copy_nonoverlapping(self.base.add(off as usize), out.as_mut_ptr(), out.len());
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: unmapping the establish-time mapping; closing our fd.
        unsafe {
            libc::munmap(
                self.base as *mut libc::c_void,
                self.layout.total_bytes as usize,
            );
            libc::close(self.ctl_fd);
        }
    }
}

enum OpResult {
    Done { slot: u32, n: usize },
    Errno(i32),
    Fallthrough,
}

/// One in-flight chunk of a pipelined large op (write OR read — the
/// read-saturation campaign reuses the flight shape verbatim): the
/// submitted base slot (+ its ABA generation) and the arena-extension
/// run behind it.
struct WriteFlight {
    base: u32,
    gen: u64,
    run: u32,
    chunk: usize,
}

/// [`Session::wait_consume`]'s outcome: the slot's raw result (slot NOT
/// yet released), or the deadline/poison abandonment (slot never reused).
enum WaitConsume {
    Done(i64),
    Abandoned,
}

// ---------------------------------------------------------------------------
// raw socket plumbing (client twins of the daemon host's helpers — this
// crate cannot depend on the daemon crate, and needs raw-libc control
// anyway for AS-safety discipline)
// ---------------------------------------------------------------------------

/// RAII guard for a raw fd on error paths.
struct FdGuard(RawFd);

impl FdGuard {
    fn release(self) -> RawFd {
        let fd = self.0;
        std::mem::forget(self);
        fd
    }
}

impl Drop for FdGuard {
    fn drop(&mut self) {
        // SAFETY: closing an fd this guard owns.
        unsafe { libc::close(self.0) };
    }
}

fn errno() -> i32 {
    // SAFETY: thread-local errno read.
    unsafe { *libc::__errno_location() }
}

fn connect_abstract(name: &str) -> Result<RawFd, SessionError> {
    // SAFETY: socket(2); ownership handled by the caller's guard.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(SessionError::Socket(errno()));
    }
    let guard = FdGuard(fd);
    // SAFETY: zeroed sockaddr_un is a valid all-default value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = name.as_bytes();
    if bytes.len() + 1 > addr.sun_path.len() {
        return Err(SessionError::Protocol);
    }
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i + 1] = *b as libc::c_char; // abstract: leading NUL
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len();
    // SAFETY: connect with a correctly-sized sockaddr_un.
    if unsafe {
        libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len as libc::socklen_t,
        )
    } != 0
    {
        return Err(SessionError::Socket(errno()));
    }
    Ok(guard.release())
}

/// OQ-6: connect to the advertised filesystem-path socket (the
/// container-netns rendezvous rung of the establish ladder).
fn connect_path(path: &str) -> Result<RawFd, SessionError> {
    // SAFETY: socket(2); ownership handled by the caller's guard.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(SessionError::Socket(errno()));
    }
    let guard = FdGuard(fd);
    // SAFETY: zeroed sockaddr_un is a valid all-default value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_bytes();
    if bytes.len() + 1 > addr.sun_path.len() {
        return Err(SessionError::Protocol);
    }
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i] = *b as libc::c_char; // filesystem path: NUL-terminated
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    // SAFETY: connect with a correctly-sized sockaddr_un.
    if unsafe {
        libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len as libc::socklen_t,
        )
    } != 0
    {
        return Err(SessionError::Socket(errno()));
    }
    Ok(guard.release())
}

/// VAL-4 step 2: the connected peer's uid (`SO_PEERCRED`). A failure to
/// read it is a refusal, never an assumption — the kernel is the only
/// source of the identity on the other end of this socket.
fn peer_uid(sock: RawFd) -> Result<u32, SessionError> {
    // SAFETY: getsockopt(SO_PEERCRED) into a correctly-sized ucred.
    let (rc, cred) = unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = libc::getsockopt(
            sock,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        );
        (rc, cred)
    };
    if rc != 0 {
        return Err(SessionError::Socket(errno()));
    }
    Ok(cred.uid)
}

/// The seal set the daemon applies to every session memfd
/// (`create_session_shm`): geometry immutable, size immutable both ways,
/// seal set closed.
const REQUIRED_SEALS: libc::c_int = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;

/// VAL-4 step 3: every required seal must be present on the received
/// memfd before it is mapped and trusted.
fn require_seals(memfd: RawFd) -> Result<(), SessionError> {
    // SAFETY: F_GET_SEALS on a received (owned) fd; no memory is touched.
    let seals = unsafe { libc::fcntl(memfd, libc::F_GET_SEALS) };
    if seals < 0 || seals & REQUIRED_SEALS != REQUIRED_SEALS {
        return Err(SessionError::Unsealed { seals });
    }
    Ok(())
}

/// VAL-4 step 1: is the advertised path rendezvous one an accepted daemon
/// could own?
///
/// The socket's PARENT directory (opened `O_DIRECTORY|O_NOFOLLOW|O_PATH`,
/// so a swapped symlink cannot redirect the check) must be
///
/// * owned by uid 0 or by `mount_uid` — exactly the identities the peer
///   ladder accepts as the daemon (the spec's "root-owned directory",
///   widened by the one disjunct that keeps non-root mounts' own runtime
///   dir — `$XDG_RUNTIME_DIR/squeezefs`, `/tmp/squeezefs-il-<uid>` —
///   usable; every other owner is refused), and
/// * not group- or world-writable (`mode & 0o022 == 0`), so nobody else
///   can replace the socket inode with their own listener.
///
/// This is the same predicate the daemon applies before binding
/// (`ipc_host::path_listen`): if either end's view of the directory is
/// untrusted, the rung is not used. The abstract rung is unaffected.
fn path_rung_trusted(path: &str, mount_uid: u32) -> Result<(), SessionError> {
    let Some(dir) = std::path::Path::new(path).parent() else {
        return Err(SessionError::UntrustedRendezvous);
    };
    let Ok(cdir) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return Err(SessionError::UntrustedRendezvous);
    };
    // SAFETY: open(2) with a NUL-terminated path; ownership taken below.
    let fd = unsafe {
        libc::open(
            cdir.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(SessionError::UntrustedRendezvous);
    }
    let guard = FdGuard(fd);
    // SAFETY: fstat into a zeroed buf on the fd just opened (O_PATH fds
    // are valid fstat targets).
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: plain fstat(2).
    let rc = unsafe { libc::fstat(guard.0, &mut st) };
    if rc != 0 {
        return Err(SessionError::UntrustedRendezvous);
    }
    if (st.st_uid != 0 && st.st_uid != mount_uid) || st.st_mode & 0o022 != 0 {
        return Err(SessionError::UntrustedRendezvous);
    }
    Ok(())
}

fn set_recv_timeout(fd: RawFd, timeout: Duration) {
    let tv = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: i64::from(timeout.subsec_micros()) as libc::suseconds_t,
    };
    // SAFETY: setsockopt with a valid timeval; best-effort.
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }
}

const CMSG_FD_SPACE: usize = 64;

fn send_ctl(sock: RawFd, msg: &CtlMsg, fd: Option<RawFd>) -> Result<(), SessionError> {
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
        // SAFETY: standard CMSG_FIRSTHDR/CMSG_DATA over the buffer above.
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
    let n = unsafe { libc::sendmsg(sock, &hdr, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(SessionError::Socket(errno()));
    }
    Ok(())
}

fn recv_ctl(sock: RawFd) -> Result<(CtlMsg, Option<RawFd>), SessionError> {
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
    // SAFETY: recvmsg into the buffers above; CLOEXEC on received fds.
    let n = unsafe { libc::recvmsg(sock, &mut hdr, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(SessionError::Socket(errno()));
    }
    // VAL-5a (client side): own EVERY descriptor the kernel installed —
    // the count comes from `cmsg_len`, never "one per cmsg". A daemon
    // that attaches extras (or truncates the control data) is refused
    // and everything it installed is closed: the app's fd table is not
    // the daemon's to fill, and the establish ladder runs on every
    // eligible open.
    let mut rx_fds: Vec<FdGuard> = Vec::new();
    // SAFETY: CMSG walk over the kernel-filled control buffer; each
    // `CMSG_DATA` span is `cmsg_len - CMSG_LEN(0)` bytes of fds the
    // kernel just installed in this process.
    unsafe {
        let hdr_bytes = libc::CMSG_LEN(0) as usize;
        let mut cmsg = libc::CMSG_FIRSTHDR(&hdr);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let payload = ((*cmsg).cmsg_len as usize).saturating_sub(hdr_bytes);
                let data = libc::CMSG_DATA(cmsg);
                for i in 0..payload / std::mem::size_of::<RawFd>() {
                    let mut fd: RawFd = -1;
                    std::ptr::copy_nonoverlapping(
                        data.add(i * std::mem::size_of::<RawFd>()),
                        &mut fd as *mut RawFd as *mut u8,
                        std::mem::size_of::<RawFd>(),
                    );
                    if fd >= 0 {
                        rx_fds.push(FdGuard(fd));
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&hdr, cmsg);
        }
    }
    // Truncated control data ⇒ the kernel installed fds we cannot
    // enumerate; more than one ⇒ no reply in this protocol carries two.
    // Both refuse; the guards close what arrived.
    if hdr.msg_flags & libc::MSG_CTRUNC != 0 || rx_fds.len() > 1 {
        return Err(SessionError::Protocol);
    }
    if n == 0 {
        return Err(SessionError::Socket(0)); // EOF: daemon gone
    }
    match CtlMsg::decode(&buf[..n as usize]) {
        Ok(m) => Ok((m, rx_fds.pop().map(|g| g.release()))),
        Err(_) => Err(SessionError::Protocol),
    }
}

// ---------------------------------------------------------------------------
// futex plumbing (cross-process shm words — never FUTEX_PRIVATE)
// ---------------------------------------------------------------------------

/// `futex_waitv(2)` wait-multiple entry (Linux ≥ 5.16). Mirrors
/// `include/uapi/linux/futex.h` — 32 bytes, `val`/`uaddr` as u64.
#[repr(C)]
struct FutexWaitv {
    val: u64,
    uaddr: u64,
    flags: u32,
    __reserved: u32,
}

/// `FUTEX2_SIZE_U32` — the only size the slot state words use.
const FUTEX2_SIZE_U32: u32 = 0x02;
/// `futex_waitv(2)` hard cap (`FUTEX_WAITV_MAX`).
const FUTEX_WAITV_MAX: usize = 128;
/// No-`futex_waitv` degradation quantum: a single-word wait cannot be
/// woken by the OTHER entries' completions, so its bound caps their
/// detection latency (pre-5.16 kernels — the el8/el9 fleet posture;
/// strictly better than the pre-2026-07-26 sleep ladder, which no
/// completion could cut short).
const WAITV_FALLBACK_QUANTUM: Duration = Duration::from_micros(200);

/// Bounded sleep until ANY entry's word is woken or its value moves
/// (the event-driven reap park — the daemon's completion `FUTEX_WAKE`s
/// the slot word whose WAITER bit [`Session::ticket_wait_entry`] set).
/// Admission is per word: any entry already differing returns
/// immediately (EAGAIN). Beyond [`FUTEX_WAITV_MAX`] entries the excess
/// is not waited on — the timeout still bounds their latency. Kernels
/// without `futex_waitv` (ENOSYS) degrade to a single wait on the
/// FIRST (oldest) entry, capped at [`WAITV_FALLBACK_QUANTUM`].
pub fn wait_any(entries: &[WaitEntry<'_>], timeout: Duration) {
    let Some(first) = entries.first() else {
        return;
    };
    // Userspace admission pre-check: any entry whose word already moved
    // makes the kernel's own admission fail (EAGAIN) — the syscall is
    // pure waste. At qd32 saturation 42 % of waits failed admission
    // (2026-07-26 sizing); one relaxed-load scan deletes those
    // syscalls. Semantics identical to the EAGAIN return: caller
    // re-harvests.
    if entries
        .iter()
        .any(|e| e.word.load(Ordering::Acquire) != e.expected)
    {
        return;
    }
    if entries.len() == 1 {
        return futex_wait(first.word, first.expected, timeout);
    }
    static WAITV_SUPPORTED: AtomicBool = AtomicBool::new(true);
    if !WAITV_SUPPORTED.load(Ordering::Relaxed) {
        return futex_wait(
            first.word,
            first.expected,
            timeout.min(WAITV_FALLBACK_QUANTUM),
        );
    }
    let mut vec = [const {
        FutexWaitv {
            val: 0,
            uaddr: 0,
            flags: 0,
            __reserved: 0,
        }
    }; FUTEX_WAITV_MAX];
    let n = entries.len().min(FUTEX_WAITV_MAX);
    for (dst, e) in vec.iter_mut().zip(entries.iter().take(n)) {
        dst.val = u64::from(e.expected);
        dst.uaddr = e.word.as_ptr() as u64;
        dst.flags = FUTEX2_SIZE_U32;
    }
    // futex_waitv takes an ABSOLUTE CLOCK_MONOTONIC timeout.
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
    // ETIMEDOUT, EINTR) resolves to "caller re-harvests".
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex_waitv,
            vec.as_ptr(),
            n as u32,
            0u32,
            &ts as *const libc::timespec,
            libc::CLOCK_MONOTONIC,
        )
    };
    if r < 0 {
        // SAFETY: errno read directly after the failing call.
        let errno = unsafe { *libc::__errno_location() };
        if errno == libc::ENOSYS {
            // Pre-5.16 kernel: remember, degrade to the quantum wait.
            WAITV_SUPPORTED.store(false, Ordering::Relaxed);
            futex_wait(
                first.word,
                first.expected,
                timeout.min(WAITV_FALLBACK_QUANTUM),
            );
        }
    }
}

fn futex_wake(word: &AtomicU32) {
    // SAFETY: FUTEX_WAKE on a live atomic's address; no memory is read.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAKE,
            i32::MAX,
            0usize,
            0usize,
            0u32,
        );
    }
}

fn futex_wait(word: &AtomicU32, expected: u32, timeout: Duration) {
    let ts = libc::timespec {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_nsec: i64::from(timeout.subsec_nanos()) as libc::c_long,
    };
    // SAFETY: FUTEX_WAIT with a valid timespec; EINTR/EAGAIN both fine —
    // the caller re-checks and re-parks under its own deadline.
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
