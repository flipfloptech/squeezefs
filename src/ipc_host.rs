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
    ClientStatsPage, Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_ECHO,
    OP_READ, OP_WRITE,
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

/// VAL-5b: `SO_RCVTIMEO` on every accepted ctl connection. This is a
/// LIVENESS poll, not a deadline — an established session's ctl lane is
/// idle between opens, so an expiry just re-checks `shutting_down` and
/// parks again. The registry sever in `shutdown` is the primary unblock;
/// this is the backstop that bounds teardown even if a sever is missed
/// (a connection accepted in the same instant as the sever sweep).
const CTL_RECV_POLL: Duration = Duration::from_millis(500);

/// VAL-5b: how long an UNESTABLISHED peer may stay silent before its
/// connection is dropped. Matched to the shim's own ctl deadline
/// (`session.rs` `CTL_RECV_TIMEOUT`, 10 s): a client that has not sent
/// its HELLO by then has already given up on us.
const CTL_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
fn service_spin_window() -> Option<Duration> {
    service_spin_window_from(std::env::var("SQUEEZEFS_IPC_SPIN_US").ok().as_deref())
}

/// One `/proc/stat` aggregate reading for the spin governor's headroom
/// gauge: `(busy_jiffies, total_jiffies)` over the whole box (busy =
/// total − idle − iowait). Cadence-gated by the gauge (≤ 10 reads/s
/// across all lanes); None on any parse surprise (the gauge keeps its
/// last honest percent).
fn read_proc_stat_busy() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let vals: Vec<u64> = fields.take(8).filter_map(|f| f.parse().ok()).collect();
    if vals.len() < 5 {
        return None;
    }
    let total: u64 = vals.iter().sum();
    let idle = vals[3] + vals.get(4).copied().unwrap_or(0);
    Some((total.saturating_sub(idle), total))
}

/// The message the daemon logs exactly once when the KD-7 skew gate has
/// been relaxed (ENG-11). Pure so the contract test can assert the text
/// without arming the lever.
pub fn allow_dev_notice() -> &'static str {
    "SQUEEZEFS_IPC_ALLOW_DEV is set — the KD-7 build-commit skew gate is \
     RELAXED for this mount (degenerate `unknown`/`-dirty` identities will be \
     admitted, counted in ipc_binds_dev_override). Dev boxes only: a \
     mismatched daemon/shim pair is undefined behavior."
}

/// `SQUEEZEFS_IPC_ALLOW_DEV` under the shared ENG-10 convention, announced
/// ONCE per process on engagement.
///
/// ENG-11: this knob relaxed the skew gate **silently on both sides** — the
/// only trace was a per-admission warning that never fires until a
/// degenerate pair actually binds, so a production mount could carry the
/// relaxed posture invisibly. `SQUEEZEFS_FUSE_NO_KILLPRIV` was the good
/// precedent (it logs when it disables the negotiation); this matches it,
/// and the shim half does the same on its side
/// (`squeezefs_preload::session::allow_dev_lever`).
pub fn allow_dev_lever() -> bool {
    let on = crate::env_knobs::bool_knob("SQUEEZEFS_IPC_ALLOW_DEV", false);
    if on {
        static ANNOUNCED: std::sync::Once = std::sync::Once::new();
        ANNOUNCED.call_once(|| log::warn!("{}", allow_dev_notice()));
    }
    on
}

/// Pure sizing form (unit-pinned): the 2026-07-26 reap-economy A/B
/// adjudicated the STATIC default as 0 (`1508ea9` — every nonzero
/// ambient window taxed the protected sync lane). Since the 2026-08-14
/// client-topology campaign the ABSENT knob means the ADAPTIVE governor
/// (`crate::spin_governor` — churn-derived window, theft-guarded by the
/// derived busy ceiling); an EXPLICIT value (including `0`) is the
/// static window verbatim, governor off (explicit-wins-verbatim, the
/// ipc-cap law).
fn service_spin_window_from(v: Option<&str>) -> Option<Duration> {
    let v = v?;
    let us = v.trim().parse::<u64>().ok()?.clamp(0, 10_000);
    Some(Duration::from_micros(us))
}

#[cfg(test)]
mod spin_window_tests {
    use super::*;

    /// The absent knob routes to the ADAPTIVE governor (2026-08-14
    /// client-topology campaign); the 2026-07-26 static-0 verdict lives
    /// on as the governor's theft guard (its busy ceiling), and an
    /// explicit `0` still pins the static-off posture verbatim.
    #[test]
    fn spin_window_absent_is_adaptive() {
        assert_eq!(
            service_spin_window_from(None),
            None,
            "absent = the adaptive governor path"
        );
        assert_eq!(
            service_spin_window_from(Some("0")),
            Some(Duration::ZERO),
            "explicit 0 = static off, verbatim (governor off)"
        );
    }

    /// Explicit settings are honored verbatim within the clamp.
    #[test]
    fn spin_window_explicit_and_clamped() {
        assert_eq!(
            service_spin_window_from(Some("20")),
            Some(Duration::from_micros(20))
        );
        assert_eq!(
            service_spin_window_from(Some("999999")),
            Some(Duration::from_micros(10_000)),
            "clamp ceiling"
        );
        assert_eq!(
            service_spin_window_from(Some("garbage")),
            None,
            "unparseable = announced by the registry gate; the governor \
             path here (the daemon refuses malformed knobs at startup, so \
             this arm is unreachable in production)"
        );
    }
}

/// IPC service-thread CEILING (§5.5.1; knob
/// `SQUEEZEFS_IPC_SERVICE_THREADS`, clamp 1..=64). Default = the shared
/// drain-LANE derivation [`squeezefs_ipc::sizing::il_drain_lanes_default`]
/// (`clamp(3×cpus/8, 2, 64)` — the counted 2026-08-06 field width sweep,
/// `.benchmarks/2026-08-06-dd-width-slope.md`) — the SAME function the
/// direct-drive shard width rides, so the lane pair cannot drift (the
/// lane routing makes a split dark shards or shared reapers — the
/// ingest-economy DEFAULTS-MISMATCH class), and it DOMINATES the shim's
/// per-mount session default at every machine size, so the 2026-07-28
/// field conviction's topology (4 shim sessions vs 8 daemon threads left
/// half the drain capacity idle at 7.5 GB/s of a 16.6 GB/s ceiling)
/// stays unrepresentable: every default session owns a drain thread, and
/// spawn-on-bind means a wider ceiling never parks a spare. Threads
/// spawn ON SESSION ADMISSION up to this ceiling
/// ([`IpcHost::ensure_service_threads`]) — a session-less host owns zero
/// service threads.
fn service_thread_count() -> usize {
    service_thread_ceiling_from(
        std::env::var("SQUEEZEFS_IPC_SERVICE_THREADS")
            .ok()
            .as_deref(),
        // PROCESS parallelism, never `available_parallelism()` — the
        // Hang-1 pinned-first-toucher sizing law, and the runtime half
        // of the lane-pair equality: `ipc_direct::dd_shards` sizes from
        // the same mask, so ceiling == shard width on every box.
        crate::cpu::process_parallelism(),
    )
}

/// Pure sizing form (unit-pinned by `tests/ingest_economy_tests.rs` —
/// lane-pair equality with `ipc_direct::dd_shards_from` + dominance over
/// the shim's session default, whose own pin is `session_sizing_tests`
/// in `squeezefs-preload`).
pub fn service_thread_ceiling_from(env: Option<&str>, cpus: usize) -> usize {
    env.and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, 64))
        .unwrap_or_else(|| squeezefs_ipc::sizing::il_drain_lanes_default(cpus))
}

/// VAL-5d: the concurrent ctl-thread (= live connection) bound, DERIVED
/// from the session budget rather than picked (AGENTS.md: resource caps
/// derive from system resources).
///
/// A connection exists to become a session, and the R5-derived
/// `arena_cap_bytes` already says how many sessions this host can ever
/// admit: `arena_cap_bytes / session footprint`. Doubling that leaves
/// room for the honest concurrent shapes — a handshake in flight while
/// every admitted session holds its own connection — plus the ADMIN lane.
///
/// * floor 32: the ADMIN lane plus a small handshake burst must work even
///   on a host whose arena budget admits one session (a shed-to-zero
///   mount still has to be administrable);
/// * rail 4096: every ctl connection is an OS thread, so a pathological
///   env-set arena cap must not turn into thread exhaustion (the same
///   rail-the-derivation pattern `SeveredPool::new` uses on its queue).
fn ctl_conn_cap_from(arena_cap_bytes: u64, session_footprint: u64) -> usize {
    let sessions = arena_cap_bytes / session_footprint.max(1);
    (sessions.saturating_mul(2).clamp(32, 4096)) as usize
}

/// The ctl socket listen(2) backlog — DERIVED from the ctl connection
/// cap (durable-write decomposition, 2026-08-05: the former hardcoded
/// 64 sat exactly at one quarter of the field's 256-process fleet, and
/// a full SEQPACKET backlog makes connect(2) BLOCK — the fleet's
/// simultaneous session burst convoyed through 64-slot accept windows,
/// part of the measured ~150–190 ms per-process launch term at w256).
///
/// * floor 64: the pre-derivation shipped constant (the never-regress-
///   below-shipped posture);
/// * rail 4096: the `ctl_conn_cap_from` thread rail — a backlog deeper
///   than the connections we would ever admit buys nothing;
/// * the kernel additionally truncates to `net.core.somaxconn`
///   (listen(2)) — that clamp stays kernel-owned, not ours.
pub fn ctl_listen_backlog(ctl_cap: usize) -> libc::c_int {
    ctl_cap.clamp(64, 4096) as libc::c_int
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
    /// Dequeue-instant (CLOCK_MONOTONIC ns, the drain's ONE clock read —
    /// r5 single-read law): the daemon-residence t0 every
    /// `ipc_direct_phase_ns` span anchors against.
    pub t0_ns: u64,
    /// op-trace (audit A2): the op's trace id — the slot ticket under
    /// the IL namespace bit when the op is in the sample, else 0.
    pub trace_id: u64,
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
    /// VAL-5c: the session's in-flight ledger entry for THIS op. Taken
    /// (and released) by `complete`; the `Drop` impl releases it on any
    /// path that drops the handle without completing, so a sink bug can
    /// never wedge the session's admission (it only loses the reply,
    /// which the §5.4.1 client deadline already covers).
    inflight: Option<Arc<SessionInflight>>,
}

impl SlotCompletion {
    /// Publish `result` (`bytes` or `-errno`) and wake a parked client.
    /// Consumes the handle: exactly one completion per dequeued op.
    pub fn complete(mut self, result: i64) {
        // VAL-5c: leave the ledger FIRST — the slot goes FREE for the
        // client the instant `core.complete()` lands below, so a
        // resubmission racing this completion must find the room already
        // returned (releasing after would make an honest client's
        // back-to-back submit look like an over-admission).
        if let Some(ledger) = self.inflight.take() {
            ledger.finish();
        }
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
        // i32::MAX: every parked reaper on the session re-scans. Since
        // the wake-economy campaign (2026-08-14) the latch arm bounds a
        // park ERA at one syscall — `Collapsed` counts the elisions
        // (the L1 engagement instrument; loom `ipc_cqe_latched_*`).
        let cqe = &self.map.header().cqe;
        match cqe.complete(cqe_wake_latch()) {
            squeezefs_ipc::cqe_core::CompleteOutcome::Wake => {
                METRICS.ipc_cqe_wake_writes.fetch_add(1, Ordering::Relaxed);
                futex_wake(cqe.seq_word(), i32::MAX);
            }
            squeezefs_ipc::cqe_core::CompleteOutcome::Collapsed => {
                METRICS
                    .ipc_cqe_wake_collapsed
                    .fetch_add(1, Ordering::Relaxed);
            }
            squeezefs_ipc::cqe_core::CompleteOutcome::Elided => {
                METRICS.ipc_cqe_wake_elided.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// The wake-collapse latch arm, resolved ONCE (the `reap_event_park_max`
/// OnceLock pattern): `cqe_core.rs` is dependency-free (loom
/// `#[path]`-includes it), so the knob rides the `complete(latch)`
/// signature instead of an env read in the core. Default ON — the
/// counted L1 lever; `SQUEEZEFS_IPC_CQE_WAKE_LATCH=0` is the A/B control
/// (the shipped wake-per-mark-passed body, verbatim).
fn cqe_wake_latch() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| crate::env_knobs::bool_knob("SQUEEZEFS_IPC_CQE_WAKE_LATCH", true))
}

impl Drop for SlotCompletion {
    fn drop(&mut self) {
        // A completion handle dropped without `complete` is a sink bug
        // (the client's op stalls until its §5.4.1 deadline), but it must
        // not leak an in-flight reservation — that would slowly starve
        // the session's admission and end in a spurious poison.
        if let Some(ledger) = self.inflight.take() {
            ledger.finish();
        }
    }
}

/// VAL-5c: one session's in-flight op ledger. Incremented when the drain
/// dequeues an op, decremented when its [`SlotCompletion`] resolves (or
/// is dropped). The honest protocol can never exceed `slots` — one ring
/// entry per CLAIMED slot — so a higher count means the client
/// re-published a slot it did not own: a §5.3 rule-4 protocol violation.
#[derive(Debug, Default)]
struct SessionInflight {
    live: AtomicU32,
}

impl SessionInflight {
    /// Admit one dequeued op; returns the resulting in-flight count.
    fn begin(&self) -> u32 {
        self.live.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Release one op's reservation (completion or dropped handle). The
    /// saturating floor keeps a hypothetical double-release from wrapping
    /// into a permanent over-admission.
    fn finish(&self) {
        let _ = self
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                Some(v.saturating_sub(1))
            });
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

    /// The LAST binding on `ino` — across every live session — is gone
    /// (POSIX-8: the data plane fires one whole-inode invalidation from
    /// here, so page invalidations the W1 write window suppressed cannot
    /// outlive the bindings). Default: nothing.
    fn on_last_unbind(&self, _ino: u64) {}

    /// End-of-sweep hook: the service loop calls this once after every
    /// drain pass over its owned sessions. The direct-drive sink uses
    /// it to flush pushed-but-unsubmitted SQEs in ONE `io_uring_enter`
    /// per sweep (the submit-batch economy); the default is a no-op.
    /// LIVENESS RULE for implementors: any work deferred during
    /// `serve_data` MUST become kernel-visible here — the service
    /// thread may park for up to its bounded window right after.
    fn flush(&self) {}

    /// RES-12 (pre-RC spec §7): tear down whatever OS threads this sink
    /// owns. Called from [`IpcHost::shutdown`], which the daemon already
    /// runs inside `spawn_blocking` — so a sink whose teardown JOINS a
    /// thread (`DataPlaneSink`'s direct-drive reaper) blocks a blocking-pool
    /// thread instead of a tokio worker. Must be idempotent: the sink's own
    /// `Drop` still calls the same teardown as the last-resort backstop
    /// (nothing guarantees the host is shut down before it is dropped).
    /// Default: nothing.
    fn shutdown_threads(&self) {}
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

    /// The data-plane write sever ([`Self::read_severed`] semantics —
    /// ONE arena read into private memory, §5.3.1 rule 2) as a
    /// [`bytes::Bytes`] payload source for the write handler. The
    /// destination is a pooled, page-warm buffer ([`SeveredPool`]) that
    /// returns when the last `Bytes` clone drops — same single copy,
    /// none of the per-op slab-alloc fault storm.
    pub fn read_severed_bytes(&self) -> bytes::Bytes {
        let pool = Arc::clone(&self._map.severed_pool);
        let mut buf = pool.get();
        if buf.capacity() < self.len {
            // Structurally unreachable (desc.len ≤ max_op_bytes validated
            // at dequeue) — reserve keeps the copy sound anyway.
            buf.reserve(self.len - buf.len());
        }
        // SAFETY: `base..base+len` validated inside the arena at
        // construction; destination capacity ensured above; source may
        // race (client-writable) — a byte-wise copy of racing memory
        // yields torn *content*, never UB. `set_len(len)` covers exactly
        // the bytes the copy initialized.
        unsafe {
            std::ptr::copy_nonoverlapping(self.base, buf.as_mut_ptr(), self.len);
            buf.set_len(self.len);
        }
        bytes::Bytes::from_owner(PooledSevered {
            buf: Some(buf),
            pool,
        })
    }

    /// The window's base pointer — the direct-drive DMA destination
    /// (`crate::ipc_direct`): raw on purpose, same non-exclusivity
    /// contract as the copies above. Alignment is the CALLER's screen
    /// (O_DIRECT-class DMA needs 4 KiB; unaligned windows bounce).
    pub(crate) fn as_base_ptr(&self) -> *mut u8 {
        self.base
    }

    /// The dense NUMA node the session arena's pages landed on (the
    /// locality instrument's memory-node source + the node-targeted
    /// handoff venue key). `None` = unknown.
    pub(crate) fn arena_node(&self) -> Option<usize> {
        self._map.arena_node
    }

    /// Write `bytes` into the window (completion payloads).
    pub fn write(&self, bytes: &[u8]) {
        let n = bytes.len().min(self.len);
        // SAFETY: bounds as above; destination races are the client's own
        // concurrent-buffer-mutation POSIX hazard.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base, n);
        }
        // Locality instrument: one CPU pass over arena bytes.
        crate::numa::count_current_pass(self._map.arena_node, n);
        // Copy ledger: the il boundary copy (read-copy-count 2026-08-02).
        crate::fuse_client::METRICS
            .ipc_arena_copy_bytes
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
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
        // Locality instrument: the §5.5.1 serve-into-arena CPU pass.
        crate::numa::count_current_pass(self._map.arena_node, n);
        // Copy ledger: the il boundary copy (read-copy-count 2026-08-02).
        crate::fuse_client::METRICS
            .ipc_arena_copy_bytes
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
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

/// Severed-write buffer recycle pool (ingest-economy 2026-07-28): the
/// §5.5.2 sever copy's DESTINATION buffers, one pool per host. The
/// profiled engine it deletes: `vec![0u8; len]` per ring write is a
/// slab-sized (`max_op_bytes`) allocation past jemalloc's tcache, so a
/// streaming write paid arena mutex + extent recycle + MADV purge +
/// ~256 page re-faults (with kernel re-zeroing the sever memcpy
/// immediately overwrites) per op — measured 1.76 M minor faults/s
/// across 5 saturated svc threads at 12.7 GB/s (the field's 81–87 %
/// %system signature). Recycled buffers keep their pages mapped and
/// warm: the sever becomes ONE user-space memcpy, exactly the copy the
/// severance law already owns (zero new copies).
///
/// Sizing is derived, not constant: buffers are `max_op_bytes` (uniform
/// class, `len` set per op), and the queue's slot count is the
/// structural in-flight bound `arena_cap_bytes / max_op_bytes` —
/// severed bytes in flight can never exceed the live session arenas R5
/// admitted, so retained bytes are worst-case bounded by the same
/// session-shm cap (and only reach it if that many severed bytes were
/// ever simultaneously in flight). `ipc_severed_pool_bytes` gauges
/// retention; hits/misses are the reuse-health counters. Lock-free
/// (`crossbeam` `ArrayQueue` — a shipped dependency core, not a new
/// house lock-free algorithm, hence no loom model).
// `pub` (not `pub(crate)`) for the microbench program (2026-08-04):
// `benches/copy_path_bench.rs` measures the get→copy→put hit cycle vs the
// per-op-alloc miss path — the ingest-economy campaign's engine
// (`.benchmarks/2026-07-28-ingest-economy.md`).
pub struct SeveredPool {
    q: crossbeam::queue::ArrayQueue<Vec<u8>>,
    buf_cap: usize,
}

impl SeveredPool {
    pub fn new(max_op_bytes: u32, arena_cap_bytes: u64) -> Self {
        let buf_cap = (max_op_bytes as usize).max(1);
        // Rail 1..=65536 slots: a pathological env override can size the
        // arena cap huge — the rail caps the (pointer-array) queue, and
        // past-capacity returns simply free (graceful degradation).
        let slots = ((arena_cap_bytes / buf_cap as u64).clamp(1, 65536)) as usize;
        Self {
            q: crossbeam::queue::ArrayQueue::new(slots),
            buf_cap,
        }
    }

    pub fn get(&self) -> Vec<u8> {
        match self.q.pop() {
            Some(buf) => {
                METRICS
                    .ipc_severed_pool_hits
                    .fetch_add(1, Ordering::Relaxed);
                METRICS
                    .ipc_severed_pool_bytes
                    .fetch_sub(self.buf_cap as u64, Ordering::Relaxed);
                buf
            }
            None => {
                METRICS
                    .ipc_severed_pool_misses
                    .fetch_add(1, Ordering::Relaxed);
                Vec::with_capacity(self.buf_cap)
            }
        }
    }

    pub fn put(&self, mut buf: Vec<u8>) {
        // Only full-class buffers recycle (a resized stray would skew the
        // uniform-class accounting); a full queue drops — dealloc is then
        // exactly the pre-pool behavior.
        if buf.capacity() < self.buf_cap {
            return;
        }
        buf.clear();
        if self.q.push(buf).is_ok() {
            METRICS
                .ipc_severed_pool_bytes
                .fetch_add(self.buf_cap as u64, Ordering::Relaxed);
        }
    }
}

/// The [`bytes::Bytes::from_owner`] owner for a pooled sever: the buffer
/// returns to its pool when the LAST `Bytes` clone drops — custody can
/// cross the async handoff and the write handler freely; retention by a
/// parked payload only delays that buffer's reuse (the pool refills by
/// allocation, never blocks).
struct PooledSevered {
    /// `Some` until drop (no unsafe take-dance needed for `AsRef`).
    buf: Option<Vec<u8>>,
    pool: Arc<SeveredPool>,
}

impl AsRef<[u8]> for PooledSevered {
    fn as_ref(&self) -> &[u8] {
        self.buf.as_deref().unwrap_or(&[])
    }
}

impl Drop for PooledSevered {
    fn drop(&mut self) {
        if let Some(buf) = self.buf.take() {
            self.pool.put(buf);
        }
    }
}

/// The daemon-side mapping of one session memfd. Unmapped on drop — which
/// is ordered after every accessor structurally (service passes and arena
/// windows clone the `Arc`; §5.3.1 rule 4).
struct SessionMapping {
    base: *mut u8,
    layout: SessionLayout,
    geometry: Geometry,
    /// The host's severed-write buffer pool (write severs ride the arena
    /// window, which holds this mapping — the natural conduit).
    severed_pool: Arc<SeveredPool>,
    /// The dense NUMA node the arena's pages ACTUALLY landed on (queried
    /// once at admission, post-placement — the locality instrument's
    /// memory-node source and the node-targeted handoff venue's key).
    /// `None` = query failed / unknown: stays out of the instrument,
    /// handoffs ride the global rotation.
    arena_node: Option<usize>,
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

    /// The client stats page (hybrid lane gate ledger) — CLIENT-writable
    /// for the session's life, DISPLAY-ONLY here (§5.3.1 rule 1: these
    /// words are summed verbatim into the stats-inode export and never
    /// feed a control decision).
    fn stats_page(&self) -> &ClientStatsPage {
        // SAFETY: the stats region at `stats_off`, page-aligned/-sized by
        // the layout; repr(C, align(4096)) protocol type, zero-initialized
        // by the fresh memfd.
        unsafe { &*(self.base.add(self.layout.stats_off as usize) as *const ClientStatsPage) }
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
    /// SO_PEERCRED pid — the same-pid sibling counter's key (NUMA
    /// session rotation; accounting only, never authorization).
    pid: u32,
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
    /// VAL-5c: ops dequeued and not yet completed. Shared with every
    /// live [`SlotCompletion`] of this session.
    inflight: Arc<SessionInflight>,
    torn_down: AtomicBool,
}

impl IpcSession {
    /// Drain + serve up to `budget` ops published on this session's ring.
    /// Returns ops served, or `None` when the session must be poisoned
    /// (§5.3 rule 4: impossible ring/slot observation).
    ///
    /// **VAL-5e — the budget is a fairness bound, not a throughput cap.**
    /// Sessions are pinned to one service thread for life (§5.5.1) and
    /// the loop walks them serially, so an unbudgeted drain lets one
    /// client that keeps its ring non-empty own the thread forever (it
    /// never even returns to the pass top to re-collect the registry).
    /// The quantum is `geometry.slots` — one session's HONEST in-flight
    /// bound (VAL-5c makes that exact), so a fully-loaded honest client
    /// still drains its whole window in a single pass and pays nothing;
    /// what the bound costs is only the ability to starve a sibling.
    fn drain(&self, sink: &Arc<dyn SessionSink>, budget: u32) -> Option<u32> {
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
            // VAL-5c admission gate: count this op in BEFORE it is served
            // (the serve severs up to `max_op_bytes` and may park in a
            // sink). The honest protocol cannot exceed `slots` — one ring
            // entry per CLAIMED slot — so a higher count proves the
            // client re-published a slot it does not own (the state word
            // it forged is client-writable memory). §5.3 rule 4: poison.
            let live = self.inflight.begin();
            if live > self.map.geometry.slots {
                log::error!(
                    "ipc session {}: {live} ops in flight with only {} slots — the client \
                     re-published an in-flight slot; poisoning session (§5.3 rule 4, VAL-5c)",
                    self.id,
                    self.map.geometry.slots
                );
                self.inflight.finish();
                return None;
            }
            // THE one linearization read of the descriptor (§5.3.1 rule 1):
            // validate the copy, serve from the copy, never re-read.
            let desc = slot.snapshot_descriptor();
            // Ring-ingress residence (reap-fanin 2026-08-08) + the
            // r5 single-read law: ONE clock read per dequeue — its low
            // 32 bits close the ingress delta (display-only; hostile /
            // implausible stamps discarded by the law, never clamped),
            // its u64 anchors the op's daemon-residence t0 (the
            // `ipc_direct_phase_ns` admit/total base), so the probe
            // pays no clock read of its own.
            let t0_ns = crate::mono_core::monotonic_ns_u64();
            // op-trace (audit A2): the il op id is the slot TICKET
            // `(session, slot, generation)` under the IL namespace bit;
            // the generation word is read only when the ring is armed
            // (one pointer load otherwise). The dequeue read stamps
            // `ipc_dequeue`, and the measured ingress delta places the
            // client's publish (`ipc_ingress`) on the same clock.
            let trace_id = if crate::op_trace::is_armed() {
                crate::op_trace::traced(crate::op_trace::il_op_id(
                    self.id,
                    index,
                    slot.core.generation(),
                ))
            } else {
                0
            };
            let ingress =
                squeezefs_ipc::layout::ingress_delta_ns(t0_ns as u32, slot.ingress_stamp());
            if let Some(ns) = ingress {
                crate::fuse_client::ipc_ingress_record_ns(ns);
            }
            if trace_id != 0 {
                if let Some(ns) = ingress {
                    crate::op_trace::stamp_mono(
                        trace_id,
                        crate::op_trace::Stage::IpcIngress,
                        t0_ns.saturating_sub(ns),
                    );
                }
                crate::op_trace::stamp_mono(trace_id, crate::op_trace::Stage::IpcDequeue, t0_ns);
            }
            let completion = SlotCompletion {
                map: Arc::clone(&self.map),
                slot_index: index,
                inflight: Some(Arc::clone(&self.inflight)),
            };
            self.serve_validated(&desc, t0_ns, trace_id, sink, completion);
            served += 1;
            if served >= budget {
                break;
            }
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
        t0_ns: u64,
        trace_id: u64,
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
                t0_ns,
                trace_id,
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
    /// Live ctl connections — **every** accepted connection, admin and
    /// data plane alike (VAL-5b; before the pre-RC pass only the ADMIN
    /// lane registered here). Severed at shutdown so their ctl threads
    /// unblock and join: a lingering — or deliberately silent — client
    /// must never wedge daemon teardown.
    conns: Mutex<Vec<Arc<UnixStream>>>,
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
    /// Owner index → dense NUMA node (NUMA-affinity campaign 2026-07-31):
    /// the `numa_core::owner_nodes` CPU-weighted interleaved partition,
    /// computed once at spawn. Single-node maps produce all-zeros and the
    /// partition is inert (pins refuse, picks reduce to load-then-index —
    /// the structural no-op law).
    owner_nodes: Vec<usize>,
    /// Owner index → live-session count — THE admission balance ledger
    /// (530k-ceiling campaign, 2026-08-05), sized to the service-thread
    /// CEILING (§5.5.1): sessions are assigned an owner index in
    /// `0..len` at admission, forever; threads spawn on demand as owners
    /// first receive a session ([`Self::ensure_service_threads`],
    /// ingest-economy 2026-07-28) — a session-less host owns ZERO
    /// service threads. The pick and its accounting are one atomic act
    /// under this mutex (reserve at pick, release at teardown/refusal):
    /// the retired registry-scan count read owners that concurrent
    /// fleet-launch admissions had not yet inserted, so balance was
    /// schedule-dependent — 32 simultaneous HELLOs could convoy onto
    /// low indices. Also feeds the `ipc_session_owners` gauge (owners
    /// with ≥1 session — `ipc_direct_shards`' admission-time face).
    owner_loads: Mutex<Vec<usize>>,
    /// Spawned service threads (dense: owners fill lowest-first, so
    /// spawned == the highest owner ever assigned + 1). Monotonic within
    /// a host's lifetime — a thread that has served stays (its empty
    /// park is the 5 ms-bounded doorbell wait); the field bug was
    /// threads that NEVER had a ring to drain.
    svc_spawned: std::sync::atomic::AtomicUsize,
    /// Host epoch for the sessions' `last_active_ms` clocks.
    started: Instant,
    /// The EXPLICIT empty-pass spin window ([`service_spin_window`],
    /// None = the adaptive governor) — resolved ONCE at [`Self::spawn`],
    /// on the caller's thread, so the knob has a deterministic read
    /// point. Reading the env on each service
    /// thread's first pass raced the spawner (threads spawn lazily at
    /// session admission — ingest-economy 2026-07-28), which is exactly
    /// the race `preload_session_tests::service_thread_stays_hot_…`
    /// kept losing under in-binary contention (set_var → spawn →
    /// remove_var vs the admission-time thread start).
    spin_static: Option<Duration>,
    /// Adaptive spin governor state (client-topology campaign,
    /// 2026-08-14 — `crate::spin_governor` module docs): the shared
    /// headroom gauge plus the lanes/cores pair its theft ceiling
    /// derives from. Live only when `spin_static` is None and
    /// `SQUEEZEFS_IPC_SPIN_ADAPTIVE` (default on) holds.
    spin_adaptive: bool,
    spin_headroom: crate::spin_governor::HeadroomGauge,
    spin_lanes: usize,
    spin_cores: usize,
    shutting_down: AtomicBool,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Live ctl connections (VAL-5b/VAL-5d): incremented in the accept
    /// loop BEFORE the thread spawns (so the admission bound is exact
    /// against a connect storm) and decremented by the connection's own
    /// exit guard.
    ctl_live: std::sync::atomic::AtomicUsize,
    /// VAL-5d: the derived concurrent-connection bound
    /// ([`ctl_conn_cap_from`]), resolved once at spawn.
    ctl_cap: usize,
    /// VAL-5e: ops one session may be served per drain pass — the
    /// geometry's `slots`, i.e. exactly one honest in-flight window.
    drain_budget: u32,
    /// One loud line per cap episode, not per refused connect (a storm
    /// must not turn the log into the DoS).
    ctl_cap_logged: AtomicBool,
    /// The severed-write buffer recycle pool (one per host; every
    /// session's mapping holds an `Arc` conduit).
    severed_pool: Arc<SeveredPool>,
    /// Deferred session-arena THP prep (shim fleet parity, 2026-08-05):
    /// the sender feeding the lazily-spawned `sqz-ipc-thp` worker.
    /// `None` until the first prep enqueues (a session-less host owns
    /// zero prep threads — the spawn-on-bind law); taken (dropped) at
    /// shutdown so the worker's `recv` disconnects promptly.
    thp_prep_tx: Mutex<Option<std::sync::mpsc::Sender<ThpPrepJob>>>,
    /// Prep ledger (host-local truth for the admission + liveness
    /// contract tests; the process-global mirrors are
    /// `ipc_arena_prep_{queued,done,skipped_dead,skipped_pressure}`).
    /// Closure law: `queued == done + skipped_dead + skipped_pressure`
    /// at quiesce.
    arena_prep_queued: AtomicU64,
    arena_prep_done: AtomicU64,
    arena_prep_skipped_dead: AtomicU64,
    arena_prep_skipped_pressure: AtomicU64,
    /// Hybrid lane gate (D14 corollary): the reaped-session fold of the
    /// client stats pages' `lane_gate_kernel_{routes,bytes}` — captured
    /// exactly once per session at teardown (the `torn_down` swap), so
    /// the exported counters stay monotone across session churn. The
    /// live half is summed on demand ([`Self::lane_gate_snapshot`]).
    /// Untrusted client words, display-only (§5.3.1 rule 1).
    lane_gate_reaped_routes: AtomicU64,
    lane_gate_reaped_bytes: AtomicU64,
    /// Wake-economy v6 reaped folds (same discipline as the lane-gate
    /// pair above: dead sessions fold once at teardown, live sessions
    /// sum on demand — untrusted client words, display-only).
    il_reaped_submit_harvested: AtomicU64,
    il_reaped_park_eras: AtomicU64,
    il_reaped_slot_reroutes: AtomicU64,
}

/// One deferred arena-prep job — a `Weak` on purpose (prep-liveness fix,
/// fleet-parity round 2): the QUEUE must never own a session's memory.
/// The field capture: fio fleets exit between rows, and every queued
/// job's former mapping `Arc` pinned a dead session's full arena until
/// the single-lane worker reached it (256 × 120 MiB ≈ 30 GiB held past
/// teardown → R5 Red → the write-pipeline admission clamp). A dead
/// mapping needs no ordering protection — it needs the prep to NOT run;
/// the §5.3.1 rule-4 accessor ordering is preserved where it matters by
/// the worker's `upgrade()`: the strong ref exists exactly across an
/// actually-running live prep, so the madvise can never touch an
/// unmapped range, and `munmap` follows the last accessor structurally.
struct ThpPrepJob {
    session: std::sync::Weak<IpcSession>,
}

/// Why a prep job did not run (the ledger's skip arms). Pure decision
/// form ([`prep_skip_reason`]) so the arms are unit-pinned without
/// forcing the process-global R5 authority into Red.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrepSkip {
    /// The session was reaped/torn down (or fully dropped) before the
    /// worker reached the job — the fleet-exit shape.
    Dead,
    /// R5 read Red at run time: eager 64–120 MiB commits + huge-folio
    /// allocation (compaction) under Red are anti-useful — prep follows
    /// the house refuse-new-work posture. Yellow deliberately does NOT
    /// skip: it is a common transient (enter ≥ 80 %), and permanently
    /// forfeiting the session-lifetime PMD upgrade on a transient is a
    /// bad trade; Red is the codebase's refuse-new-work line (jobs
    /// pause, sessions refuse) and prep holds to the same law.
    Pressure,
}

/// The run/skip decision for a job the worker just picked up.
fn prep_skip_reason(torn_down: bool, level: crate::mem_budget::Level) -> Option<PrepSkip> {
    if torn_down {
        return Some(PrepSkip::Dead);
    }
    if level == crate::mem_budget::Level::Red {
        return Some(PrepSkip::Pressure);
    }
    None
}

#[cfg(test)]
mod prep_skip_tests {
    use super::*;
    use crate::mem_budget::Level;

    /// The decision table: dead always skips (and outranks pressure —
    /// the honest attribution when both hold), Red skips live jobs,
    /// Yellow and Green run them.
    #[test]
    fn skip_arms_are_exactly_dead_and_red() {
        assert_eq!(prep_skip_reason(true, Level::Green), Some(PrepSkip::Dead));
        assert_eq!(prep_skip_reason(true, Level::Red), Some(PrepSkip::Dead));
        assert_eq!(
            prep_skip_reason(false, Level::Red),
            Some(PrepSkip::Pressure)
        );
        assert_eq!(
            prep_skip_reason(false, Level::Yellow),
            None,
            "Yellow is a common transient — forfeiting the session-lifetime \
             PMD upgrade on it is a bad trade (Red is the refuse-new-work line)"
        );
        assert_eq!(prep_skip_reason(false, Level::Green), None);
    }
}

impl IpcHost {
    /// Spawn the host: bind + listen on the abstract socket, start the
    /// accept thread and the drain thread.
    pub fn spawn(cfg: IpcHostConfig, sink: Arc<dyn SessionSink>) -> io::Result<Arc<Self>> {
        cfg.geometry
            .validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        // Layout computes now so admission can charge the exact footprint.
        // VAL-5d: the connection bound rides the SAME session-footprint
        // arithmetic admission uses.
        let session_footprint = SessionLayout::compute(&cfg.geometry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?
            .total_bytes;
        let ctl_cap = ctl_conn_cap_from(cfg.arena_cap_bytes, session_footprint);
        // The listen backlog derives from that cap (durable-write
        // decomposition 2026-08-05 — the hardcoded 64 convoyed a
        // 256-process fleet's simultaneous connect burst through 64-slot
        // accept windows; connect(2) on a full SEQPACKET backlog BLOCKS,
        // so the constant was a launch serialization stage, not a safety
        // bound).
        let backlog = ctl_listen_backlog(ctl_cap);
        let listener_fd = abstract_listen(&cfg.socket_name, backlog)?;
        // OQ-6: the optional path rendezvous. Failure is loud but never
        // fatal — the mount (and same-netns interception) must not be
        // held hostage by optional plumbing.
        let path_listener = cfg.socket_dir.as_ref().and_then(|dir| {
            let path = dir.join(format!("{}.sock", cfg.socket_name));
            match path_listen(dir, &path, backlog) {
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
        let cfg_max_op_bytes = cfg.geometry.max_op_bytes;
        let cfg_arena_cap_bytes = cfg.arena_cap_bytes;
        // VAL-5e: the fair per-pass quantum = one honest in-flight window.
        let cfg_slots = cfg.geometry.slots.max(1);
        let host = Arc::new(Self {
            cfg,
            sink,
            admin: arc_swap::ArcSwap::from_pointee(None),
            conns: Mutex::new(Vec::new()),
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
            owner_nodes: crate::numa_core::topology().owner_nodes(service_threads),
            owner_loads: Mutex::new(vec![0; service_threads]),
            svc_spawned: std::sync::atomic::AtomicUsize::new(0),
            started: Instant::now(),
            spin_static: service_spin_window(),
            spin_adaptive: crate::env_knobs::bool_knob("SQUEEZEFS_IPC_SPIN_ADAPTIVE", true),
            spin_headroom: crate::spin_governor::HeadroomGauge::new(),
            spin_lanes: service_thread_count(),
            spin_cores: crate::cpu::process_parallelism(),
            shutting_down: AtomicBool::new(false),
            threads: Mutex::new(Vec::new()),
            ctl_live: std::sync::atomic::AtomicUsize::new(0),
            ctl_cap,
            ctl_cap_logged: AtomicBool::new(false),
            drain_budget: cfg_slots,
            severed_pool: Arc::new(SeveredPool::new(cfg_max_op_bytes, cfg_arena_cap_bytes)),
            thp_prep_tx: Mutex::new(None),
            arena_prep_queued: AtomicU64::new(0),
            arena_prep_done: AtomicU64::new(0),
            arena_prep_skipped_dead: AtomicU64::new(0),
            arena_prep_skipped_pressure: AtomicU64::new(0),
            lane_gate_reaped_routes: AtomicU64::new(0),
            lane_gate_reaped_bytes: AtomicU64::new(0),
            il_reaped_submit_harvested: AtomicU64::new(0),
            il_reaped_park_eras: AtomicU64::new(0),
            il_reaped_slot_reroutes: AtomicU64::new(0),
        });
        // Spawn-on-bind (ingest-economy 2026-07-28): the gauge reports
        // SPAWNED service threads — 0 until a session admits. No thread
        // spawns here; `ensure_service_threads` runs at admission.
        METRICS.ipc_service_threads.store(0, Ordering::Relaxed);

        let accept_host = Arc::clone(&host);
        let accept = std::thread::Builder::new()
            .name(squeezefs_ipc::comm_core::comm_name("sqz-ipc-accept"))
            .spawn(move || accept_host.accept_loop(AcceptOn::Abstract))?;
        let mut spawned = vec![accept];
        if host.path_listener.is_some() {
            let path_host = Arc::clone(&host);
            spawned.push(
                std::thread::Builder::new()
                    // Base shortened from "sqz-ipc-accept-p" (PR 3,
                    // comm suffixes): the old 16-char base both blew the
                    // 15-char comm budget AND base-truncated onto the
                    // abstract accept thread's exact spelling.
                    .name(squeezefs_ipc::comm_core::comm_name("sqz-ipc-accp"))
                    .spawn(move || path_host.accept_loop(AcceptOn::Path))?,
            );
        }
        if host.cfg.idle_secs > 0 {
            let reap_host = Arc::clone(&host);
            spawned.push(
                std::thread::Builder::new()
                    .name(squeezefs_ipc::comm_core::comm_name("sqz-ipc-reap"))
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

    /// Live ctl connections — the VAL-5b/VAL-5d bound's gauge (accepted
    /// and not yet exited, admin and data plane alike).
    pub fn ctl_conns_live(&self) -> usize {
        self.ctl_live.load(Ordering::Relaxed)
    }

    /// The derived concurrent ctl-connection bound (VAL-5d) — the
    /// admission ceiling the accept loop enforces.
    pub fn ctl_conn_cap(&self) -> usize {
        self.ctl_cap
    }

    /// `JoinHandle`s the host still retains (VAL-5d): the accept loop
    /// prunes finished ones, so this stays bounded by the live thread
    /// population instead of counting every connection ever accepted.
    pub fn retained_thread_handles(&self) -> usize {
        self.threads
            .lock()
            .expect("thread registry mutex never poisons")
            .len()
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
        // Sever EVERY live ctl connection (VAL-5b): their threads park
        // in recv — between verbs for an established peer, on the FIRST
        // datagram for one that never speaks — and all of them must
        // unblock for the join below.
        for conn in std::mem::take(
            &mut *self
                .conns
                .lock()
                .expect("ctl conn registry mutex never poisons"),
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
        // Drop the prep sender so the `sqz-ipc-thp` worker's recv
        // disconnects immediately (its 500 ms poll tick is the backstop);
        // queued jobs drop with it — Weaks, pinning nothing.
        self.thp_prep_tx
            .lock()
            .expect("thp prep sender mutex never poisons")
            .take();
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
        // RES-12: the sink's own OS threads (the direct-drive reaper) are
        // joined HERE — inside the caller's `spawn_blocking` hop — instead
        // of waiting for `Drop for DataPlaneSink` to fire on whichever
        // tokio worker happens to drop the last filesystem clone.
        // Idempotent, so the `Drop` backstop stays intact.
        self.sink.shutdown_threads();
    }

    /// POSIX-8: is `ino` still bound by ANY live session? Answered from
    /// the session registry (control-plane cadence — see the Unbind
    /// arm); a peer session still serving the inode keeps the ring
    /// authoritative, so the last-unbind shootdown must not fire.
    fn ino_bound_anywhere(&self, ino: u64) -> bool {
        let sessions: Vec<Arc<IpcSession>> = self
            .sessions
            .lock()
            .expect("session registry mutex never poisons")
            .values()
            .cloned()
            .collect();
        sessions
            .iter()
            .any(|s| s.bindings.any_sync(|_, rights| rights.ino == ino).is_some())
    }

    /// Hybrid lane gate display snapshot `(kernel_routes, kernel_bytes,
    /// threshold_gauge)` for the stats-inode export: the counters are the
    /// reaped fold plus the sum over LIVE sessions' client stats pages;
    /// the threshold gauge is the MAX over live sessions (0 with none —
    /// a gauge, it drops with them). Untrusted client words, summed
    /// verbatim, display-only (§5.3.1 rule 1: never a daemon input).
    pub fn lane_gate_snapshot(&self) -> (u64, u64, u64) {
        // Accumulators read + live set cloned under ONE lock hold — the
        // teardown fold runs remove+fold under the same lock, so every
        // session lands on exactly one half of the sum.
        let (mut routes, mut bytes, sessions) = {
            let sessions = self
                .sessions
                .lock()
                .expect("session registry mutex never poisons");
            (
                self.lane_gate_reaped_routes.load(Ordering::Relaxed),
                self.lane_gate_reaped_bytes.load(Ordering::Relaxed),
                sessions.values().cloned().collect::<Vec<Arc<IpcSession>>>(),
            )
        };
        let mut threshold = 0u64;
        for s in sessions {
            let (r, b, t) = s.map.stats_page().snapshot();
            routes = routes.wrapping_add(r);
            bytes = bytes.wrapping_add(b);
            threshold = threshold.max(t);
        }
        (routes, bytes, threshold)
    }

    /// Wake-economy v6 display snapshot `(il_submit_harvested,
    /// il_park_eras, il_slot_reroutes)`: reaped folds + live-session
    /// page sums, same one-lock-hold discipline as
    /// [`Self::lane_gate_snapshot`]. Untrusted client words, summed
    /// verbatim, display-only (§5.3.1 rule 1) — the PR 3 scout's gate
    /// reads `il_slot_reroutes` off this export on poison-free rows.
    pub fn wake_economy_snapshot(&self) -> (u64, u64, u64) {
        let (mut harv, mut eras, mut reroutes, sessions) = {
            let sessions = self
                .sessions
                .lock()
                .expect("session registry mutex never poisons");
            (
                self.il_reaped_submit_harvested.load(Ordering::Relaxed),
                self.il_reaped_park_eras.load(Ordering::Relaxed),
                self.il_reaped_slot_reroutes.load(Ordering::Relaxed),
                sessions.values().cloned().collect::<Vec<Arc<IpcSession>>>(),
            )
        };
        for s in sessions {
            let (h, e, r) = s.map.stats_page().snapshot_wake_economy();
            harv = harv.wrapping_add(h);
            eras = eras.wrapping_add(e);
            reroutes = reroutes.wrapping_add(r);
        }
        (harv, eras, reroutes)
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
            // VAL-5d: admission BEFORE the thread exists. The bound is
            // derived from the session budget (`ctl_conn_cap_from`), and
            // a refused peer hears why (class Budget) instead of being
            // parked or silently dropped.
            if self.ctl_live.load(Ordering::Relaxed) >= self.ctl_cap {
                METRICS
                    .ipc_admission_refusals
                    .fetch_add(1, Ordering::Relaxed);
                if !self.ctl_cap_logged.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "ipc host: {} concurrent ctl connections — at the derived cap; \
                         refusing new connections until it drains (VAL-5d)",
                        self.ctl_cap
                    );
                }
                let _ = send_ctl(
                    &sock,
                    &CtlMsg::Refuse {
                        class: RefuseClass::Budget,
                    },
                    None,
                );
                drop(sock);
                continue;
            }
            self.ctl_live.fetch_add(1, Ordering::Relaxed);
            let host = Arc::clone(&self);
            let handle = std::thread::Builder::new()
                .name(squeezefs_ipc::comm_core::comm_name("sqz-ipc-ctl"))
                .spawn(move || host.connection_loop(sock));
            match handle {
                Ok(h) => {
                    let mut threads = self
                        .threads
                        .lock()
                        .expect("thread registry mutex never poisons");
                    // VAL-5d: prune finished handles on every accept —
                    // the registry tracks LIVE threads, not history (the
                    // `admin_conns` retain pattern, applied where the
                    // unbounded growth actually was).
                    threads.retain(|h| !h.is_finished());
                    threads.push(h);
                }
                Err(e) => {
                    self.ctl_live.fetch_sub(1, Ordering::Relaxed);
                    log::error!("ipc host: ctl thread spawn failed: {e}");
                }
            }
            // Re-arm the cap log once the population has genuinely
            // drained (one line per episode, never per refusal).
            if self.ctl_live.load(Ordering::Relaxed) < self.ctl_cap / 2 {
                self.ctl_cap_logged.store(false, Ordering::Relaxed);
            }
        }
    }

    /// VAL-5b: register a live ctl connection so `shutdown` can sever it;
    /// the returned guard removes it on every exit path.
    fn register_conn(&self, sock: &Arc<UnixStream>) -> CtlConnRegistration<'_> {
        self.conns
            .lock()
            .expect("ctl conn registry mutex never poisons")
            .push(Arc::clone(sock));
        CtlConnRegistration {
            host: self,
            sock: Arc::clone(sock),
        }
    }

    /// VAL-5b: the FIRST datagram, under a handshake deadline. A peer
    /// that connects and says nothing is dropped rather than holding a
    /// ctl thread (and, before this bound, the whole teardown) forever.
    /// `None` = give up on this connection (silence, EOF, malformed, or
    /// host teardown).
    fn recv_handshake(&self, sock: &Arc<UnixStream>) -> Option<(CtlMsg, Option<OwnedFd>)> {
        let deadline = Instant::now() + CTL_HANDSHAKE_TIMEOUT;
        loop {
            match recv_ctl(sock) {
                Ok(v) => return Some(v),
                Err(e) if is_recv_timeout(&e) => {
                    if self.shutting_down.load(Ordering::SeqCst) {
                        return None;
                    }
                    if Instant::now() >= deadline {
                        log::warn!(
                            "ipc host: connection sent no HELLO within {} s — dropping (VAL-5b)",
                            CTL_HANDSHAKE_TIMEOUT.as_secs()
                        );
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// One connection = at most one session: HELLO (screen) → SessionOk /
    /// Refuse; then BIND/UNBIND until EOF (client death — §5.7).
    fn connection_loop(self: Arc<Self>, sock: UnixStream) {
        let _live = CtlConnLive(&self);
        let sock = Arc::new(sock);
        // VAL-5b: every accepted connection is registered (so `shutdown`
        // can sever it) and every receive on it is bounded (so a peer
        // that stops speaking cannot pin this thread past teardown).
        let _reg = self.register_conn(&sock);
        let _ = sock.set_read_timeout(Some(CTL_RECV_POLL));
        // First datagram routes the connection: AdminHello opens the
        // control-plane lane (VL2 §5.1.4), Hello the data plane. An
        // unestablished peer gets `CTL_HANDSHAKE_TIMEOUT` to say
        // something and is then dropped — it holds no state worth
        // waiting on, and the slot it occupies is bounded (VAL-5d).
        let first = match self.recv_handshake(&sock) {
            Some(v) => v,
            None => return,
        };
        if let (
            CtlMsg::AdminHello {
                abi,
                pid,
                uid,
                build_commit,
                nonce,
            },
            _,
        ) = &first
        {
            self.admin_loop(&sock, *abi, *pid, *uid, build_commit, nonce);
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
                // VAL-5b: the receive poll expiring is not an event — an
                // ESTABLISHED session's ctl lane is legitimately idle
                // between opens. Re-check teardown and park again.
                Err(e) if is_recv_timeout(&e) => {
                    if self.shutting_down.load(Ordering::SeqCst) {
                        return;
                    }
                    continue;
                }
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
                    let ino = session
                        .bindings
                        .remove_sync(&binding_id)
                        .map(|(_, rights)| rights.ino);
                    // POSIX-8: was that the last binding on the inode,
                    // anywhere? Control-plane cadence (one unbind per
                    // close of the last dup), so the registry walk is
                    // affordable and the answer is host-wide — a peer
                    // session still serving the inode keeps the ring
                    // authoritative and must NOT be shot down.
                    if let Some(ino) = ino {
                        if !self.ino_bound_anywhere(ino) {
                            self.sink.on_last_unbind(ino);
                        }
                    }
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
    /// The ADMIN lane (VL2 §5.1.4): the SAME screening ladder the data
    /// plane runs — version (KD-7 ABI + build-commit equality, degenerate
    /// identities refused unless the counted dev override) → nonce
    /// freshness (anti-replay) → peercred (uid 0 or the mount owner;
    /// claimed pid/uid must match the kernel's) — then a request/reply
    /// verb loop against the wired [`AdminSink`]. No fd screen, no
    /// session, no shm: strictly more restrictive than the data plane.
    ///
    /// VAL-7c (pre-RC spec §3): the peercred half was always correct, but
    /// the version and nonce rungs were MISSING while this lane serves
    /// mutating verbs (`job-cancel`, `volume-add-data`, `volume-disable`)
    /// against durable state whose encodings are version-locked to the
    /// build. Kept in the same order as [`Self::handle_hello_msg`] so the
    /// two ladders cannot drift.
    fn admin_loop(
        self: &Arc<Self>,
        sock: &Arc<UnixStream>,
        abi: u32,
        pid: u32,
        uid: u32,
        build_commit: &str,
        nonce: &[u8; squeezefs_ipc::wire::NONCE_LEN],
    ) {
        let refuse = |class: RefuseClass| {
            count_refusal(class);
            log::warn!("ipc host: AdminHello refused ({class:?}) from pid {pid} uid {uid}");
            let _ = send_ctl(sock, &CtlMsg::Refuse { class }, None);
        };
        // Rung 1 — KD-7 version lock (identical to the data lane's).
        if abi != squeezefs_ipc::layout::IPC_ABI || build_commit != self.cfg.build_commit {
            return refuse(RefuseClass::Version);
        }
        if build_commit_degenerate(build_commit) || build_commit_degenerate(&self.cfg.build_commit)
        {
            if !self.cfg.allow_dev {
                return refuse(RefuseClass::Version);
            }
            log::warn!(
                "ipc host: ADMIN lane admitted a degenerate build identity via the dev \
                 override (SQUEEZEFS_IPC_ALLOW_DEV) — fleet-hygiene alarm outside dev boxes"
            );
        }
        // Rung 2 — nonce freshness (current + immediately-previous).
        {
            let mut n = self.nonce.lock().expect("nonce mutex never poisons");
            n.rotate_if_stale();
            if !n.accepts(nonce) {
                drop(n);
                return refuse(RefuseClass::Nonce);
            }
        }
        // Rung 3 — peercred (the lane's authorizer; unchanged).
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
        // (Registration happens once, for every connection, in
        // `connection_loop` — VAL-5b.)
        loop {
            match recv_ctl(sock) {
                // VAL-5b: an admin client parks between verbs; the poll
                // expiring only re-checks teardown.
                Err(e) if is_recv_timeout(&e) => {
                    if self.shutting_down.load(Ordering::SeqCst) {
                        return;
                    }
                    continue;
                }
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
        // The fleet-launch convoy instrument (durable-write decomposition
        // 2026-08-05): HELLO receipt → SessionOk sent, recorded per
        // ADMITTED session (refusals stay on the refusal counters). One
        // Instant + one bucket add per SESSION — always-on.
        let admit_t0 = Instant::now();
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

        // Session→node inference (NUMA-affinity campaign 2026-07-31,
        // daemon-side only — no wire/ABI change): the peer's last-run
        // CPU via the SO_PEERCRED-verified pid, mapped through the
        // runtime topology; SIBLING sessions of the same pid (fd-sharded
        // multi-threaded apps) rotate across exec nodes nearest-first —
        // one app must never pile every arena + service thread onto one
        // socket (the drain-capacity-halving shape). `None` on
        // single-node maps.
        let session_node = crate::numa::session_node_for_pid(cred.pid as u32).map(|base| {
            let siblings = self
                .sessions
                .lock()
                .expect("session registry mutex never poisons")
                .values()
                .filter(|s| s.pid == cred.pid as u32)
                .count();
            crate::numa_core::topology().rotate_exec_from(base, siblings)
        });
        // §5.5.1 pinning: admit BALANCE-FIRST (530k-ceiling campaign,
        // 2026-08-05 — live sessions never rebalance; natural churn is
        // the only mover). The pick minimizes (load, distance, index):
        // load dominates so a process fleet engages the FULL derived
        // width — the retired locality-first order confined a fork-
        // clustered fleet inference to ONE node's owner subset (4 of 8
        // lanes on the 2×16 field box, the ~530k rand-4k il ceiling) —
        // while the inference node breaks ties so an idle box still
        // admits local (`numa_core::pick_owner`; single-node maps /
        // `SQUEEZEFS_NUMA=0` reduce to load-then-index, ties to the
        // LOWEST index so owners fill densely — the invariant
        // `ensure_service_threads` relies on). The pick and its
        // accounting are ONE atomic act on the owner-load ledger
        // (reserve here, release at teardown/refusal): the retired
        // registry-scan count raced concurrent fleet-launch HELLOs'
        // not-yet-inserted sessions, making balance schedule-dependent.
        let owner = self.reserve_owner(if crate::numa_core::placement_active() {
            session_node
        } else {
            None
        });
        // Spawn-on-bind (ingest-economy 2026-07-28): the owner's thread
        // must exist before the session publishes — a session pinned to
        // a never-spawned owner would strand its ops forever.
        if let Err(e) = self.ensure_service_threads(owner) {
            log::error!("ipc host: service thread spawn failed: {e}");
            self.release_owner(owner);
            return refuse(RefuseClass::Internal);
        }
        // Establish: sealed memfd + mapping + registry entry. The arena
        // bind hint is the CHOSEN owner's partition node — memory
        // follows thread, so the service thread sits where the memory
        // is BY CONSTRUCTION whichever owner balance picked (gating
        // inside `bind_session_arena` keeps single-node /
        // `SQUEEZEFS_NUMA=0` mounts untouched).
        let arena_node_hint = if crate::numa_core::placement_active() {
            self.owner_nodes.get(owner).copied()
        } else {
            None
        };
        let (memfd, map) = match create_session_shm(
            &self.cfg.geometry,
            layout,
            Arc::clone(&self.severed_pool),
            arena_node_hint,
        ) {
            Ok(v) => v,
            Err(e) => {
                log::error!("ipc host: session shm creation failed: {e}");
                self.release_owner(owner);
                return refuse(RefuseClass::Internal);
            }
        };
        let session = Arc::new(IpcSession {
            id: self.next_session_id.fetch_add(1, Ordering::Relaxed),
            uid: cred.uid,
            pid: cred.pid as u32,
            kill_priv: peer_kill_priv(cred.uid, cred.pid as u32),
            owner,
            map: Arc::new(map),
            charged_bytes: footprint,
            bindings: scc::HashMap::new(),
            consumer: Mutex::new(RingConsumer::new()),
            sock: Arc::clone(sock),
            last_active_ms: AtomicU64::new(0),
            inflight: Arc::new(SessionInflight::default()),
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

        // Deferred arena THP prep (shim fleet parity, 2026-08-05): queue
        // BEFORE the SessionOk send — the enqueue is O(1), so a client
        // that observes SessionOk observes the job queued (the admission
        // contract test's ordering), while the prep WORK runs off-path.
        // The job holds a Weak (prep-liveness): a session that dies in
        // the queue costs nothing and skips at its turn.
        self.enqueue_arena_prep(&session);

        // Record the establishment latency BEFORE the SessionOk send —
        // the same ordering law as the prep enqueue above: anything a
        // client may observe after SessionOk (the admission-contract
        // tests read this histogram the instant `establish` returns)
        // must be visible before the send. The cost of the stronger
        // order is one sample recorded for the rare SessionOk-send
        // failure below — a torn-down admission's latency datum in a
        // diagnostic histogram, harmless; the refusal counters remain
        // the refusal ledger.
        METRICS.ipc_session_admission_ns.record(admit_t0.elapsed());

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
        {
            let mut sessions = self
                .sessions
                .lock()
                .expect("session registry mutex never poisons");
            sessions.remove(&session.id);
            // Hybrid lane gate: fold this session's stats-page ledger
            // into the host accumulators — exactly once (the swap
            // above), under the SAME lock hold as the registry removal
            // so a concurrent [`Self::lane_gate_snapshot`] counts the
            // session on exactly one half. The threshold gauge
            // deliberately dies with the session. (The client may still
            // write the page until it observes the poison — a straggler
            // route landing after this fold is display noise, never
            // accounting truth: §5.3.1 rule 1.)
            let (routes, bytes, _thr) = session.map.stats_page().snapshot();
            self.lane_gate_reaped_routes
                .fetch_add(routes, Ordering::Relaxed);
            self.lane_gate_reaped_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            // Wake-economy v6: same one-half-of-the-sum discipline.
            let (harv, eras, reroutes) = session.map.stats_page().snapshot_wake_economy();
            self.il_reaped_submit_harvested
                .fetch_add(harv, Ordering::Relaxed);
            self.il_reaped_park_eras.fetch_add(eras, Ordering::Relaxed);
            self.il_reaped_slot_reroutes
                .fetch_add(reroutes, Ordering::Relaxed);
        }
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
        // Owner ledger closure: the torn_down swap above makes this
        // exactly-once per session (530k-ceiling campaign).
        self.release_owner(session.owner);
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

    /// Deferred session-arena THP prep (shim fleet parity, 2026-08-05 —
    /// the D12 board-item-2 fix): queue the arena's populate+collapse for
    /// the `sqz-ipc-thp` worker instead of running it on the ctl thread
    /// inside the HELLO window. The admission-time posture serialized
    /// `MADV_POPULATE_WRITE` + `MADV_COLLAPSE` over the FULL arena
    /// (64–120 MiB — fault+zero, collapse re-copy, huge-folio allocation
    /// → direct compaction on fragmented fleet boxes) in front of every
    /// client's first op, O(width × arena_bytes) at every fleet launch:
    /// the 256-session process-fleet knee (local width bracket, 64 MiB
    /// arenas pinned: w256 il/kernel 0.914 inline vs 1.074 without —
    /// falsifying nothing about the PMD law itself, which the worker
    /// still delivers moments later). A 4 KiB-paged session is correct
    /// by construction; the collapse is an upgrade, never a readiness
    /// gate. Gated by the same `SQUEEZEFS_IPC_ARENA_THP` lever (off ⇒
    /// nothing queues, exactly the old disabled arm).
    ///
    /// Worker-spawn failure degrades loud: the session simply stays
    /// 4 KiB-paged (the pre-near-zero-copy posture), never a refusal.
    ///
    /// Prep-liveness (fleet-parity round 2): the job carries a **`Weak`**
    /// on the SESSION — the queue owns no memory, a reaped session's
    /// arena unmaps at teardown regardless of the backlog, and the
    /// worker's upgrade-or-skip ladder (see [`ThpPrepJob`]) is what
    /// keeps the madvise-on-live-mapping property.
    fn enqueue_arena_prep(self: &Arc<Self>, session: &Arc<IpcSession>) {
        if !arena_thp_enabled() {
            return;
        }
        let mut tx = self
            .thp_prep_tx
            .lock()
            .expect("thp prep sender mutex never poisons");
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        if tx.is_none() {
            let (sender, rx) = std::sync::mpsc::channel::<ThpPrepJob>();
            let worker_host = Arc::clone(self);
            match std::thread::Builder::new()
                .name(squeezefs_ipc::comm_core::comm_name("sqz-ipc-thp"))
                .spawn(move || worker_host.thp_prep_loop(rx))
            {
                Ok(handle) => {
                    self.threads
                        .lock()
                        .expect("thread registry mutex never poisons")
                        .push(handle);
                    *tx = Some(sender);
                }
                Err(e) => {
                    log::warn!(
                        "ipc host: arena THP prep worker failed to spawn ({e}) — \
                         session arenas stay 4 KiB-paged this mount"
                    );
                    return;
                }
            }
        }
        if let Some(sender) = tx.as_ref() {
            self.arena_prep_queued.fetch_add(1, Ordering::Relaxed);
            METRICS
                .ipc_arena_prep_queued
                .fetch_add(1, Ordering::Relaxed);
            // A send on a disconnected worker (it observed shutdown) just
            // drops the job — a Weak, so nothing is pinned either way.
            let _ = sender.send(ThpPrepJob {
                session: Arc::downgrade(session),
            });
        }
    }

    /// The `sqz-ipc-thp` worker: one prep at a time (deliberately —
    /// bounding prep concurrency to 1 keeps a 256-session fleet launch
    /// from thundering-herding populate/collapse across every ctl
    /// thread, which was exactly the admission-time shape). Exits on
    /// host shutdown (sender dropped in [`IpcHost::shutdown`], or the
    /// flag observed on the poll tick).
    fn thp_prep_loop(self: Arc<Self>, rx: std::sync::mpsc::Receiver<ThpPrepJob>) {
        while !self.shutting_down.load(Ordering::SeqCst) {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(job) => {
                    // TEST SEAM (`SQUEEZEFS_TEST_THP_PREP_STALL_MS`, the
                    // WRITE_STALL pattern): hold the job so the admission
                    // contract test can observe the deferred window.
                    if let Some(ms) = std::env::var("SQUEEZEFS_TEST_THP_PREP_STALL_MS")
                        .ok()
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .filter(|ms| *ms > 0)
                    {
                        std::thread::sleep(Duration::from_millis(ms));
                    }
                    if self.shutting_down.load(Ordering::SeqCst) {
                        return;
                    }
                    // Liveness ladder (prep-liveness fix): upgrade-or-skip.
                    // A failed upgrade IS the dead arm (last accessor
                    // already gone); a successful upgrade can still be a
                    // torn-down session briefly kept alive by a stale
                    // service-thread snapshot or an in-flight completion —
                    // `torn_down` is the authoritative liveness word (set
                    // first thing in `teardown_session`). While the
                    // upgraded Arc is held, the mapping cannot unmap
                    // (§5.3.1 rule-4 accessor ordering, now scoped to
                    // exactly the running prep).
                    let session = job.session.upgrade();
                    let torn = session
                        .as_ref()
                        .map(|s| s.torn_down.load(Ordering::SeqCst))
                        .unwrap_or(true);
                    match prep_skip_reason(torn, crate::mem_budget::level()) {
                        Some(PrepSkip::Dead) => {
                            self.arena_prep_skipped_dead.fetch_add(1, Ordering::Relaxed);
                            METRICS
                                .ipc_arena_prep_skipped_dead
                                .fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        Some(PrepSkip::Pressure) => {
                            self.arena_prep_skipped_pressure
                                .fetch_add(1, Ordering::Relaxed);
                            METRICS
                                .ipc_arena_prep_skipped_pressure
                                .fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        None => {}
                    }
                    let map = &session
                        .as_ref()
                        .expect("live-arm session upgraded above (torn == false ⇒ Some)")
                        .map;
                    let outcome = crate::thp::advise_hugepages(
                        map.base,
                        map.layout.total_bytes as usize,
                        crate::thp::ThpMode::PopulateCollapse,
                    );
                    log::debug!(
                        "ipc session shm THP (deferred): madvise_ok={} collapse_ok={} ({} bytes)",
                        outcome.madvise_ok,
                        outcome.collapse_ok,
                        map.layout.total_bytes
                    );
                    self.arena_prep_done.fetch_add(1, Ordering::Relaxed);
                    METRICS.ipc_arena_prep_done.fetch_add(1, Ordering::Relaxed);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Prep ledger snapshot `(queued, done, skipped_dead,
    /// skipped_pressure)` — the admission + liveness contract tests'
    /// observable. Closure law: `queued == done + skipped_dead +
    /// skipped_pressure` at quiesce; the difference is the live backlog.
    pub fn arena_prep_counts(&self) -> (u64, u64, u64, u64) {
        (
            self.arena_prep_queued.load(Ordering::Relaxed),
            self.arena_prep_done.load(Ordering::Relaxed),
            self.arena_prep_skipped_dead.load(Ordering::Relaxed),
            self.arena_prep_skipped_pressure.load(Ordering::Relaxed),
        )
    }

    /// Spawn-on-bind (ingest-economy 2026-07-28): guarantee owner index
    /// `owner`'s service thread exists before its first session
    /// publishes. Owners fill lowest-first (the admission pick resolves
    /// ties to the lowest index), so spawned threads stay DENSE — this
    /// spawns every missing index up to `owner`. Serialized on the
    /// `threads` mutex (which also orders it against `shutdown`'s
    /// handle take: a spawn that wins the mutex before the take lands
    /// its handle in the joined vec; one that loses observes
    /// `shutting_down` and refuses — no leaked thread either way).
    /// Pick + reserve the session's owner in ONE atomic act (530k-ceiling
    /// campaign, 2026-08-05): balance-first `(load, distance, index)` via
    /// `numa_core::pick_owner` when a placement node is supplied, plain
    /// load-then-index otherwise. The increment happens under the same
    /// lock as the read, so concurrent fleet-launch admissions can never
    /// convoy onto stale counts (the retired registry-scan count raced
    /// its own insert). Pairs with [`Self::release_owner`].
    fn reserve_owner(&self, placement_node: Option<usize>) -> usize {
        let mut loads = self
            .owner_loads
            .lock()
            .expect("owner ledger mutex never poisons");
        let owner = match placement_node {
            Some(n) => crate::numa_core::pick_owner(
                crate::numa_core::topology(),
                n,
                &self.owner_nodes,
                &loads,
            ),
            None => loads
                .iter()
                .enumerate()
                .min_by_key(|(_, n)| **n)
                .map(|(i, _)| i)
                .unwrap_or(0),
        };
        loads[owner] += 1;
        METRICS.ipc_session_owners.store(
            loads.iter().filter(|&&n| n > 0).count() as u64,
            Ordering::Relaxed,
        );
        owner
    }

    /// Release one owner reservation (teardown, or a refusal after the
    /// pick). A leaked reservation would bias every later pick toward
    /// the other owners — the ledger's closure is what the
    /// `teardown_releases_the_owner_ledger` contract pins.
    fn release_owner(&self, owner: usize) {
        let mut loads = self
            .owner_loads
            .lock()
            .expect("owner ledger mutex never poisons");
        if let Some(n) = loads.get_mut(owner) {
            *n = n.saturating_sub(1);
        }
        METRICS.ipc_session_owners.store(
            loads.iter().filter(|&&n| n > 0).count() as u64,
            Ordering::Relaxed,
        );
    }

    /// Live sessions per owner index — the admission-balance observable
    /// (`tests/ipc_admission_balance_tests.rs`).
    pub fn session_owner_spread(&self) -> Vec<usize> {
        self.owner_loads
            .lock()
            .expect("owner ledger mutex never poisons")
            .clone()
    }

    fn ensure_service_threads(self: &Arc<Self>, owner: usize) -> io::Result<()> {
        if owner < self.svc_spawned.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut threads = self
            .threads
            .lock()
            .expect("thread registry mutex never poisons");
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(io::Error::other("host shutting down"));
        }
        let mut spawned = self.svc_spawned.load(Ordering::Acquire);
        while spawned <= owner {
            let service_host = Arc::clone(self);
            threads.push(
                std::thread::Builder::new()
                    .name(squeezefs_ipc::comm_core::comm_name(&format!(
                        "sqz-ipc-svc{spawned}"
                    )))
                    .spawn(move || service_host.service_loop(spawned))?,
            );
            spawned += 1;
            self.svc_spawned.store(spawned, Ordering::Release);
            // The gauge reports SPAWNED (live) service threads.
            METRICS
                .ipc_service_threads
                .store(spawned as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    /// One budgeted, round-robin drain sweep over `sessions` (VAL-5e).
    /// `start` rotates per pass, so the per-session budget bounds how far
    /// ahead of its siblings a saturating session can get AND no session
    /// permanently sits at the head of the queue. Returns ops served;
    /// a session that reports a protocol violation is poisoned here.
    fn drain_pass(&self, sessions: &[Arc<IpcSession>], start: usize) -> u32 {
        if sessions.is_empty() {
            return 0;
        }
        let mut served = 0u32;
        let now = self.now_ms();
        for k in 0..sessions.len() {
            let s = &sessions[(start.wrapping_add(k)) % sessions.len()];
            match s.drain(&self.sink, self.drain_budget) {
                Some(0) => {}
                Some(n) => {
                    served += n;
                    // §5.7 idle clock: served ring ops are activity.
                    s.last_active_ms.store(now, Ordering::Relaxed);
                }
                None => self.poison_session(s, "ring/slot protocol violation"),
            }
        }
        served
    }

    fn service_loop(self: Arc<Self>, idx: usize) {
        // NUMA-affinity (2026-07-31): pin this owner to its partition
        // node's CPU set (∩ process mask — taskset never widened).
        // Gated internally (`SQUEEZEFS_NUMA=0` / single-node no-op);
        // refusal leaves the thread free-running exactly as before.
        if let Some(&node) = self.owner_nodes.get(idx) {
            crate::numa::pin_service_thread(node);
        }
        // Direct-drive lane (D12 randread-shim residual, 2026-08-05):
        // this owner's governed submissions ride shard `idx % width` —
        // one lane per service thread, the shard/reaper partition
        // mirroring the owner partition above.
        crate::ipc_direct::set_service_lane(idx);
        // Owned-session snapshot, re-collected ONLY when the registry
        // epoch moves (admission/teardown) — the drain hot pass is
        // registry-mutex-free (2026-07-26 service economy). Staleness
        // bound: one pass (a torn-down session drains at most once more,
        // safely; a new session is picked up at the next pass top —
        // pinned by `new_session_on_a_busy_thread_is_served_promptly`).
        let mut sessions: Vec<Arc<IpcSession>> = Vec::new();
        let mut seen_epoch = u64::MAX; // != any real epoch ⇒ first pass collects
                                       // VAL-5e: the round-robin cursor — every sweep starts one session
                                       // further along, so the per-session budget cannot be gamed by
                                       // always being first in the collected order.
        let mut rr_start = 0usize;
        let mut last_progress = Instant::now();
        // Adaptive spin state (client-topology 2026-08-14): the lane's
        // own park-duration EWMA is the churn signal; `spin_active`
        // marks passes inside a live window so an absorbed park (work
        // arrived before we parked) is countable.
        let mut ewma_park_ns: u64 = 0;
        let mut spin_active = false;
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
            // Drain-funnel instrument (2026-08-08 r3): time the WHOLE
            // per-pass ceremony — drains + flush + inline reap — for
            // non-empty sweeps; count empty sweeps (the spin cadence).
            // One Instant per pass, always-on (the write_pipeline cost
            // contract; the pass body dwarfs the clock read).
            let t_pass = Instant::now();
            let served = self.drain_pass(&sessions, rr_start);
            rr_start = rr_start.wrapping_add(1);
            // One flush per sweep (SessionSink::flush liveness rule):
            // direct-drive SQEs published during the drain become
            // kernel-visible before this thread can park.
            let t_flush = Instant::now();
            self.sink.flush();
            if served > 0 {
                crate::fuse_client::ipc_drain_flush_record(t_flush);
                crate::fuse_client::ipc_drain_pass_record(t_pass);
                last_progress = Instant::now();
                if spin_active {
                    // The spin absorbed what would have been a
                    // park/wake cycle — the governor's win, counted.
                    METRICS
                        .ipc_spin_absorbed_parks
                        .fetch_add(1, Ordering::Relaxed);
                    spin_active = false;
                }
                continue;
            }
            METRICS
                .ipc_drain_empty_passes
                .fetch_add(1, Ordering::Relaxed);
            // The spin window: explicit knob verbatim, else the adaptive
            // governor (churn-derived, theft-guarded — module docs on
            // `crate::spin_governor`). Sessions-empty lanes never spin
            // (no doorbell to absorb; the park below is the right park).
            let spin_window = match self.spin_static {
                Some(w) => w,
                None if self.spin_adaptive && !sessions.is_empty() => {
                    let now = crate::mono_core::monotonic_ns_u64();
                    if self.spin_headroom.should_sample(now) {
                        if let Some((busy, total)) = read_proc_stat_busy() {
                            self.spin_headroom.publish(busy, total);
                        }
                    }
                    let w = crate::spin_governor::window(
                        ewma_park_ns,
                        self.spin_headroom.busy_pct(),
                        self.spin_lanes,
                        self.spin_cores,
                        sessions.len(),
                    );
                    METRICS
                        .ipc_spin_window_us
                        .store(w.as_micros() as u64, Ordering::Relaxed);
                    if w.is_zero()
                        && sessions.len() >= 2
                        && ewma_park_ns > 0
                        && ewma_park_ns <= crate::spin_governor::SPIN_RAIL_US * 1_000
                    {
                        // In the churn regime but refused by the busy
                        // ceiling — the theft guard engaging.
                        METRICS
                            .ipc_spin_disengaged_busy
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    w
                }
                None => Duration::ZERO,
            };
            if last_progress.elapsed() < spin_window {
                spin_active = true;
                std::hint::spin_loop();
                continue;
            }
            spin_active = false;
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
            let t_rescan = Instant::now();
            let rescan_served = self.drain_pass(&sessions, rr_start);
            rr_start = rr_start.wrapping_add(1);
            // Same liveness rule on the pre-park rescan sweep.
            let t_flush2 = Instant::now();
            self.sink.flush();
            if rescan_served > 0 {
                // The rescan is a drain pass too (the funnel instrument
                // must see every serving sweep or ops/pass lies).
                crate::fuse_client::ipc_drain_flush_record(t_flush2);
                crate::fuse_client::ipc_drain_pass_record(t_rescan);
            }
            if rescan_served == 0 {
                if sessions.is_empty() {
                    std::thread::park_timeout(SERVICE_PARK_MAX);
                } else {
                    METRICS.ipc_service_parks.fetch_add(1, Ordering::Relaxed);
                    let t_park = Instant::now();
                    futex_wait_many(
                        sessions
                            .iter()
                            .zip(&observed)
                            .map(|(s, o)| (&s.map.header().doorbell, *o)),
                        SERVICE_PARK_MAX,
                    );
                    // The governor's churn signal: how long this park
                    // actually lasted (short = a spin would have
                    // absorbed it; the SERVICE_PARK_MAX-bounded idle
                    // park folds in long and disengages the window).
                    ewma_park_ns = crate::spin_governor::fold_park(
                        ewma_park_ns,
                        t_park.elapsed().as_nanos() as u64,
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

/// RAII half of the VAL-5b connection registry: a connection is
/// deregistered when its ctl thread leaves, whatever the exit path.
struct CtlConnRegistration<'a> {
    host: &'a IpcHost,
    sock: Arc<UnixStream>,
}

impl Drop for CtlConnRegistration<'_> {
    fn drop(&mut self) {
        self.host
            .conns
            .lock()
            .expect("ctl conn registry mutex never poisons")
            .retain(|c| !Arc::ptr_eq(c, &self.sock));
    }
}

/// Is this receive error the SO_RCVTIMEO poll expiring (VAL-5b) rather
/// than a real transport failure? Linux reports `EAGAIN` on a timed-out
/// `recvmsg`; the std mapping is `WouldBlock`, and `TimedOut` covers
/// platforms/paths that report `ETIMEDOUT`.
fn is_recv_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// RAII half of the [`IpcHost::ctl_conns_live`] gauge: the accept loop
/// counts a connection in before spawning its thread, and the thread's
/// guard counts it out on EVERY exit path (return, break, panic).
struct CtlConnLive<'a>(&'a IpcHost);

impl Drop for CtlConnLive<'_> {
    fn drop(&mut self) {
        self.0.ctl_live.fetch_sub(1, Ordering::Relaxed);
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

/// `SQUEEZEFS_IPC_ARENA_THP` (default on; `0` disables) — the session
/// huge-page A/B lever, read once per process.
fn arena_thp_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| crate::env_knobs::bool_knob("SQUEEZEFS_IPC_ARENA_THP", true))
}

/// Create + seed + seal one session memfd, and map it daemon-side.
fn create_session_shm(
    geometry: &Geometry,
    layout: SessionLayout,
    severed_pool: Arc<SeveredPool>,
    node_hint: Option<usize>,
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
    // Shared RW mapping of the full memfd — PMD-aligned when possible
    // (huge shmem folios only map through PMDs on aligned vmas; plain
    // mmap fallback keeps alignment an optimization, never a
    // correctness need).
    let base =
        match crate::thp::map_shared_pmd_aligned(memfd.as_raw_fd(), layout.total_bytes as usize) {
            Some(p) => p as *mut libc::c_void,
            None => {
                // SAFETY: shared RW mapping of the full memfd.
                let p = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        layout.total_bytes as usize,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        memfd.as_raw_fd(),
                        0,
                    )
                };
                if p == libc::MAP_FAILED {
                    return Err(io::Error::last_os_error());
                }
                p
            }
        };
    // NUMA arena placement (NUMA-affinity campaign 2026-07-31): bind the
    // session to prefer the peer's node BEFORE first touch, so the
    // (deferred) THP populate faults the pages on the right node and the
    // collapse keeps them there (compose order is load-bearing — the
    // bind is on the DAEMON's vma, so the deferred populate AND the
    // collapse's huge-folio allocation both follow it; a client
    // first-touch racing the deferred populate places at most its first
    // op's window, which the collapse then re-places per this policy).
    // Gated internally (`SQUEEZEFS_NUMA=0` / single-node maps);
    // best-effort.
    crate::numa::bind_session_arena(base as *mut u8, layout.total_bytes as usize, node_hint);
    // Session-arena THP (near-zero-copy 2026-07-31): DEFERRED to the
    // host's `sqz-ipc-thp` prep worker (shim fleet parity, 2026-08-05 —
    // see `IpcHost::enqueue_arena_prep`). Running populate+collapse here
    // put O(fleet-width × arena_bytes) of memory work in front of every
    // client's first op at fleet launch; a 4 KiB-paged session is
    // correct by construction and the collapse is an upgrade, never a
    // readiness gate.
    //
    // The locality instrument's memory-node truth: where the arena's
    // pages ACTUALLY land (post-bind — get_mempolicy faults the base
    // page per the vma policy while the arena is still untouched). One
    // syscall per admission, off the data path.
    let arena_node = crate::numa_core::topology().node_of_addr(base as *const u8);
    let map = SessionMapping {
        base: base as *mut u8,
        layout,
        geometry: *geometry,
        severed_pool,
        arena_node,
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

fn abstract_listen(name: &str, backlog: libc::c_int) -> io::Result<OwnedFd> {
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
    // SAFETY: listen(2). Backlog is the derived `ctl_listen_backlog`
    // (the kernel truncates to net.core.somaxconn).
    if unsafe { libc::listen(fd.as_raw_fd(), backlog) } != 0 {
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
///
/// **VAL-4 (pre-RC spec §3, P0) — the directory is screened, never
/// conjured.** The path rung is a filesystem rendezvous any process in
/// the namespace can see, and the default non-root location lives under
/// world-writable `/tmp`; a directory an attacker owns lets them place
/// their own listener at the name the mount advertises. So:
///
/// 1. create it 0755 if missing (`DirBuilder` + `mode` — never the
///    umask's guess), then
/// 2. open it `O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC` and `fstat` the **fd**:
///    `st_uid == geteuid()` (we must own the rendezvous) and
///    `st_mode & 0o022 == 0` (nobody else may create/replace entries),
/// 3. do every subsequent operation **through that dirfd**
///    (`unlinkat`/`bind` via `/proc/self/fd/<dirfd>/<name>`/`fchmodat`),
///    so a directory swapped after the check cannot redirect them, and
/// 4. **refuse** (loud `Err`) rather than fall back anywhere else — the
///    caller degrades to abstract-only, which is a working rendezvous
///    for every same-netns client.
///
/// The socket file itself stays 0666 (connecting is not a credential —
/// SO_PEERCRED + the §5.2 fd screen are the boundary); a same-name stale
/// file is OUR crash residue (names embed pid+random) and is replaced.
fn path_listen(
    dir: &std::path::Path,
    path: &std::path::Path,
    backlog: libc::c_int,
) -> io::Result<OwnedFd> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
    {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "socket path has no file name")
    })?;
    // The screened handle: everything below rides THIS inode, not the
    // path (a swap after the check cannot redirect an open fd).
    let dirfd = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)?;
    {
        use std::os::unix::fs::MetadataExt;
        let md = dirfd.metadata()?;
        // SAFETY: geteuid is trivially safe.
        let euid = unsafe { libc::geteuid() };
        if md.uid() != euid || md.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "socket directory {} is uid {} mode {:o} — refusing to bind a \
                     rendezvous we do not exclusively own (need uid {euid}, no \
                     group/world write)",
                    dir.display(),
                    md.uid(),
                    md.mode() & 0o7777,
                ),
            ));
        }
    }
    // dirfd-relative names for bind/chmod/unlink. AF_UNIX has no
    // `bindat(2)`: `/proc/self/fd/<dirfd>/<name>` is the kernel-provided
    // equivalent — it resolves through the ALREADY-OPEN, already-screened
    // directory inode, so no ancestor rename/symlink swap can move it.
    let cname = std::ffi::CString::new(name.as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket name has a NUL"))?;
    // SAFETY: unlinkat on our own dirfd with a NUL-terminated name;
    // ENOENT is the normal (no residue) case.
    unsafe { libc::unlinkat(dirfd.as_raw_fd(), cname.as_ptr(), 0) };
    let dir_rel = std::path::PathBuf::from(format!("/proc/self/fd/{}", dirfd.as_raw_fd()));
    let bind_path = dir_rel.join(name);
    // SAFETY: socket(2); ownership taken immediately.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let (addr, len) = path_sockaddr(&bind_path)?;
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
    // SAFETY: fchmodat on our own dirfd + NUL-terminated name.
    if unsafe { libc::fchmodat(dirfd.as_raw_fd(), cname.as_ptr(), 0o666, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: listen(2). Backlog is the derived `ctl_listen_backlog`
    // (the kernel truncates to net.core.somaxconn).
    if unsafe { libc::listen(fd.as_raw_fd(), backlog) } != 0 {
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
///
/// **VAL-5a (pre-RC spec §3, P0) — the SCM_RIGHTS bound.** This runs
/// before ANY validation, on a socket any process in the namespace can
/// reach, so the descriptor accounting has to be exact:
///
/// * the fd count comes from `cmsg_len` (a single cmsg can carry many —
///   the pre-fix loop copied exactly one per cmsg and silently left the
///   rest installed in this process with no owner and no close);
/// * **every** received descriptor is wrapped in an `OwnedFd` at once, so
///   the refusal paths below close them by drop, not by remembering to;
/// * `MSG_CTRUNC` ⇒ refuse: truncated control data means the kernel
///   installed descriptors we cannot enumerate;
/// * more than one descriptor ⇒ refuse: no ctl message in the protocol
///   carries two (HELLO/BIND carry exactly one, everything else none),
///   so a second attachment is a protocol violation by construction.
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
    // Own EVERY descriptor the kernel installed, derived from `cmsg_len`
    // (VAL-5a). `rx_fds` is the complete set: the refusals below drop it.
    let mut rx_fds: Vec<OwnedFd> = Vec::new();
    // SAFETY: CMSG walk over the kernel-filled control buffer; each
    // `CMSG_DATA` span is `cmsg_len - CMSG_LEN(0)` bytes of `RawFd`s the
    // kernel just installed in this process.
    unsafe {
        let hdr_bytes = libc::CMSG_LEN(0) as usize;
        let mut cmsg = libc::CMSG_FIRSTHDR(&hdr);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let payload = ((*cmsg).cmsg_len as usize).saturating_sub(hdr_bytes);
                let count = payload / std::mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg);
                for i in 0..count {
                    let mut fd: RawFd = -1;
                    std::ptr::copy_nonoverlapping(
                        data.add(i * std::mem::size_of::<RawFd>()),
                        &mut fd as *mut RawFd as *mut u8,
                        std::mem::size_of::<RawFd>(),
                    );
                    if fd >= 0 {
                        rx_fds.push(OwnedFd::from_raw_fd(fd));
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&hdr, cmsg);
        }
    }
    if hdr.msg_flags & libc::MSG_CTRUNC != 0 {
        let dropped = rx_fds.len();
        drop(rx_fds); // close what the kernel did install
        METRICS
            .ipc_descriptor_rejects
            .fetch_add(1, Ordering::Relaxed);
        log::warn!(
            "ipc host: ctl datagram with TRUNCATED control data ({dropped} fds installed \
             and closed) — refusing (VAL-5a)"
        );
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ctl datagram control data truncated (MSG_CTRUNC)",
        ));
    }
    if rx_fds.len() > 1 {
        let count = rx_fds.len();
        drop(rx_fds);
        METRICS
            .ipc_descriptor_rejects
            .fetch_add(1, Ordering::Relaxed);
        log::warn!(
            "ipc host: ctl datagram carried {count} descriptors (protocol allows at most \
             one) — refusing, all closed (VAL-5a)"
        );
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ctl datagram carried more than one descriptor",
        ));
    }
    let rx_fd = rx_fds.pop();
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
        // RES-22: whether any waiter registered is a schedule property
        // (a racing reaper can vacate every slot between the caller's
        // snapshot and this call) — report, never panic a service thread.
        crate::note_invariant_tripwire(
            "futex_wait_many_no_waiters",
            "futex_wait_many called with no waiters",
        );
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
