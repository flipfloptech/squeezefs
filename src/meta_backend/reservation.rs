//! NVMe Persistent Reservations for the single-writer mount guard —
//! D0 Layer B1 (`docs/design-metadata-throughput.md` §5.0).
//!
//! Cross-host mount exclusion is **enforcement-grade** only where the
//! device can enforce it: an NVMe namespace advertising reservation
//! support (Identify Namespace `RESCAP ≠ 0`) lets the mounting daemon
//! take a **Write Exclusive** reservation — reads (probes, `open_probe`)
//! keep working from every host, while every non-holder *write* is
//! rejected by the device itself. A fenced holder observes the rejection
//! at its first post-fence **barrier** (journal entry writes are buffered
//! page-cache writes; the reservation-conflict block status surfaces at
//! `fdatasync`/writeback as the `EBADE`-class errno — design §5.0 B1
//! pt 3, Issue 14), where the backend's barrier-failure escalation latches
//! the volume `failed` (`writer_guard_fenced`).
//!
//! The protocol here is control-plane only — Register / Acquire /
//! Preempt / Release / Report are one-shot NVMe commands issued via
//! passthru ioctl on the namespace fd at mount/unmount plus a
//! heartbeat-cadence Report re-check (the design explicitly sanctions
//! ioctl for this: io_uring-first governs *data* paths, not mount-time
//! admin plumbing — the `nvmeof.rs` nvme-cli precedent).
//!
//! Register rides the [`register_ladder`]: targets disagree on Register
//! semantics for a host that already holds a (stale, different-key)
//! registration — kernel nvmet replaces it in place (IEKEY), while
//! spec-strict targets (SPDK v26.05, measured in the 2026-07-17 scoping
//! pass) return Reservation Conflict, which used to brick kill-9 →
//! remount recovery there. The ladder proves from the device (Report +
//! the association's Get-Features host identifier) that the conflicting
//! registration is OUR OWN before unregistering it; foreign
//! registrations are never touched (they stay preempt/TTL/claim
//! territory).
//!
//! [`ReservationClient`] keeps the protocol cargo-testable: the mount
//! guard drives the trait, production resolves the real passthru client
//! by probing `RESCAP`, and tests install [`FakeReservationClient`]s
//! against a shared [`FakeNvmeNamespace`] via [`install_override`]
//! (the `uring_fs` fault-shim precedent: harness surface in the lib,
//! inert in production).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// The controller association identity a PR registration is scoped to
/// (design §5.0 B1 pt 6: "the key is only the credential" — the
/// registrant is the Host NQN / Host ID). The guard records it at mount
/// and re-verifies it at every heartbeat Report re-check; a mismatch
/// means our registration is **not ours** any more (fail-stop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    pub hostnqn: String,
    pub hostid: String,
}

/// One registrant in a Reservation Report: its reservation key, the
/// Host Identifier bytes the device attributes it to (16 B extended/EDS
/// form on every fabrics association; 8 B short form on 64-bit-hostid
/// PCIe controllers), and whether it holds the reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationRegistrant {
    pub rkey: u64,
    pub host_id: Vec<u8>,
    pub holds_reservation: bool,
}

/// One Reservation Report snapshot: the current holder's key (if any
/// reservation is held) and every registrant with its host identity —
/// the identity is what lets the register ladder tell OUR stale
/// registration (crashed incarnation, same host) from a foreign one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationReport {
    pub holder_key: Option<u64>,
    pub registrants: Vec<ReservationRegistrant>,
    /// The held reservation's TYPE (0 = none held). DLM S9 needs it: a
    /// co-writer's admission rung 5 must prove the standing hold is
    /// **Write Exclusive – Registrants Only** (rtype 3), because a
    /// registration under an rtype-1 Write Exclusive grants no write
    /// access at all — admitting such a co-writer would be a mount whose
    /// every DMA the device rejects.
    pub rtype: u8,
}

impl ReservationReport {
    /// Whether `key` appears among the registrants.
    pub fn registered(&self, key: u64) -> bool {
        self.registrants.iter().any(|r| r.rkey == key)
    }

    /// The registrant count (`REGCTL`).
    pub fn regctl(&self) -> usize {
        self.registrants.len()
    }

    /// `true` ⇔ a **Write Exclusive – Registrants Only** reservation
    /// (rtype 3 — the shared-data-namespace fence) is held: every
    /// registrant writes, unregistered hosts are rejected by the device.
    pub fn is_wero(&self) -> bool {
        self.holder_key.is_some() && u32::from(self.rtype) == RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY
    }
}

/// §5.2 rule 2's gauge input (design-full-multi-writer, KD-MW-3 /
/// `pr_registrant_shared`): does the device attribute ANOTHER
/// registration (a different rkey) to OUR association's Host
/// Identifier? That is the shared-identity shape — two co-located
/// mounts riding one hostnqn/hostid pair, so the device sees ONE host
/// for both and fencing between them degrades to process-local.
///
/// Computed from the device's ACTUAL answer (the Reservation Report's
/// registrant host ids vs the association's `wire_host_id`), never from
/// configured strings — a shared-connection degradation can never hide
/// behind a configured-but-inert knob. An empty own wire id matches
/// nothing (fail-closed, the register-ladder convention).
pub fn registrant_identity_shared(
    report: &ReservationReport,
    our_key: u64,
    our_wire_id: &[u8],
) -> bool {
    if our_wire_id.is_empty() {
        return false;
    }
    report
        .registrants
        .iter()
        .any(|r| r.rkey != our_key && r.host_id == our_wire_id)
}

// ---------------------------------------------------------------------------
// The Reservation Report codec — sized by REGCTL (design-symmetric-metadata
// §5.8.1 / KD-SYM-18): the data structure is a header plus one
// registered-controller structure per registrant, so the transfer is read
// in two steps — the header first, then `header + stride × REGCTL` — and
// every registrant the header names is decoded. The shipped read filled a
// fixed 4 KiB buffer and its parse loop broke at the 63rd extended
// registrant (`(4096 − 64) / 64`), so the 64th co-writer read as
// unregistered.
// ---------------------------------------------------------------------------

/// Header length of the Reservation Report data structure: 64 B in the
/// extended form (EDS = 1, 128-bit host ids — every fabrics association),
/// 24 B in the short form (64-bit host ids — PCIe controllers). Both
/// forms pad the registered-controller structures to the same stride
/// (verified byte-wise against kernel nvmet — the first extended
/// structure sits at 0x40).
pub fn report_header_len(extended: bool) -> usize {
    if extended {
        64
    } else {
        24
    }
}

/// Bytes of one Reservation Report holding `regctl` registrants:
/// `header + stride × REGCTL` (stride = the header length in both forms).
pub fn report_len_for(regctl: u16, extended: bool) -> usize {
    let unit = report_header_len(extended);
    unit + unit * usize::from(regctl)
}

/// Decode one Reservation Report image (either form). Total: a short
/// image refuses, a header naming more registrants than the image holds
/// decodes the ones that fit (the two-step read's first transfer is the
/// header alone, and a REGCTL that grew between the two reads is caught
/// at the next read). Both forms open with `gen u32 ‖ rtype u8 ‖ regctl
/// u16`; each structure opens with `cntlid u16 ‖ rcsts u8`, the short form
/// carries `hostid u64 @8 ‖ rkey u64 @16`, the extended `rkey u64 @8 ‖
/// hostid[16] @16`.
pub fn parse_reservation_report(data: &[u8], extended: bool) -> io::Result<ReservationReport> {
    let hdr = report_header_len(extended);
    if data.len() < hdr {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "reservation report image of {} bytes is shorter than its {hdr}-byte header",
                data.len()
            ),
        ));
    }
    let rtype = data[4];
    let regctl = usize::from(u16::from_le_bytes([data[5], data[6]]));
    let stride = hdr;
    let (rkey_off, hostid_off, hostid_len) = if extended { (8, 16, 16) } else { (16, 8, 8) };
    let fits = data.len().saturating_sub(hdr) / stride;
    let n = regctl.min(fits);
    let mut registrants = Vec::with_capacity(n);
    let mut holder_key = None;
    for i in 0..n {
        let base = hdr + i * stride;
        let rcsts = data[base + 2];
        let mut rkey = [0u8; 8];
        rkey.copy_from_slice(&data[base + rkey_off..base + rkey_off + 8]);
        let rkey = u64::from_le_bytes(rkey);
        let holds = rcsts & 0x1 != 0 && rtype != 0;
        registrants.push(ReservationRegistrant {
            rkey,
            host_id: data[base + hostid_off..base + hostid_off + hostid_len].to_vec(),
            holds_reservation: holds,
        });
        if holds {
            holder_key = Some(rkey);
        }
    }
    Ok(ReservationReport {
        holder_key,
        registrants,
        rtype,
    })
}

/// The per-namespace report gauges (`pr_registrants_per_namespace`,
/// `pr_report_bytes` — design §11 "Fencing family"): the last report's
/// `REGCTL` and the bytes its REGCTL-sized read transferred, keyed by the
/// namespace's device path.
type ReportGauges = Mutex<std::collections::BTreeMap<PathBuf, (u64, u64)>>;

fn report_gauges() -> &'static ReportGauges {
    static G: OnceLock<ReportGauges> = OnceLock::new();
    G.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()))
}

/// Record one namespace's report (`regctl`, transferred `bytes`). The
/// map is keyed by the client's own path and updated IN PLACE: one
/// allocation the first time a namespace reports, none on the heartbeat
/// cadence after (review round 1, Issue 14); the lock is poison-tolerant
/// like every gauge lock in the tree.
pub fn note_report_gauge(namespace: &Path, regctl: usize, bytes: u64) {
    let mut g = report_gauges().lock().unwrap_or_else(|e| e.into_inner());
    match g.get_mut(namespace) {
        Some(v) => *v = (regctl as u64, bytes),
        None => {
            g.insert(namespace.to_path_buf(), (regctl as u64, bytes));
        }
    }
}

/// Snapshot of the per-namespace report gauges (the stats inode's read).
pub fn pr_report_gauges() -> std::collections::BTreeMap<String, (u64, u64)> {
    report_gauges()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, v)| (k.display().to_string(), *v))
        .collect()
}

/// `SQUEEZEFS_PR_REGISTRANT_CAP` — a DECLARED registrant cap for a
/// third-party PR array whose behaviour is known (int 1..=65535; unset =
/// learn from the device, and the kernel `nvmet` target — THE target,
/// R-SYM-8 — has none).
pub const PR_REGISTRANT_CAP_ENV: &str = "SQUEEZEFS_PR_REGISTRANT_CAP";

/// The cap LEARNED from the device: the `REGCTL` a Reservation Report
/// read when a REGISTER was refused TWICE with the same device-answered
/// command-specific / vendor-specific NVMe status at the same registrant
/// count (a vendor array's "registration table full"). 0 = nothing
/// learned. Unlearned when a REGISTER later succeeds at or past it.
static PR_REGISTRANT_CAP_LEARNED: AtomicU64 = AtomicU64::new(0);
/// The learn arm's CANDIDATE: `(status << 32) | regctl` of the last
/// learnable refusal — the cap is learned only when the same pair
/// repeats (review round 1, Issue 5: one refusal is a candidate, not a
/// cap). 0 = none.
static PR_REGISTRANT_CAP_CANDIDATE: AtomicU64 = AtomicU64::new(0);
/// Joins refused at the cap (`pr_registrant_cap_refusals` — must stay 0
/// on nvmet by construction).
static PR_REGISTRANT_CAP_REFUSALS: AtomicU64 = AtomicU64::new(0);

/// A device-answered NVMe status (SCT ‖ SC, the DNR/More bits masked) on
/// one command — the `io::Error` payload [`NvmeReservationClient`]'s
/// passthru surfaces for every nonzero status that is not the
/// reservation conflict, so callers can CLASSIFY the refusal instead of
/// reading a message ([`nvme_status_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvmeStatusError {
    pub opcode: u8,
    pub status: u16,
}

impl std::fmt::Display for NvmeStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "nvme command 0x{:02x} failed with status 0x{:x}",
            self.opcode, self.status
        )
    }
}

impl std::error::Error for NvmeStatusError {}

/// An `io::Error` carrying a device-answered NVMe status.
pub fn nvme_status_error(opcode: u8, status: u16) -> io::Error {
    io::Error::other(NvmeStatusError {
        opcode,
        status: status & 0x7ff,
    })
}

/// The NVMe status `e` carries, if it is a device-answered status (an
/// errno — a transport failure, a disconnected controller, a
/// still-connecting one — carries none).
pub fn nvme_status_of(e: &io::Error) -> Option<u16> {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<NvmeStatusError>())
        .map(|s| s.status)
}

// NVMe generic command statuses (SCT 0) that mean "not now", never "the
// table is full" — the learn arm never reads a cap from them.
/// Command Abort Requested.
pub const NVME_SC_ABORT_REQ: u16 = 0x07;
/// Command Aborted due to SQ Deletion.
pub const NVME_SC_ABORT_QUEUE: u16 = 0x08;
/// Command Aborted due to Failed Fused Command.
pub const NVME_SC_FUSED_FAIL: u16 = 0x09;
/// Command Aborted due to Missing Fused Command.
pub const NVME_SC_FUSED_MISSING: u16 = 0x0a;
/// Command Interrupted.
pub const NVME_SC_CMD_INTERRUPTED: u16 = 0x21;
/// Transient Transport Error.
pub const NVME_SC_TRANSIENT_TRANSPORT: u16 = 0x22;
/// Namespace Not Ready.
pub const NVME_SC_NAMESPACE_NOT_READY: u16 = 0x82;
/// The path-related status class (SCT 3: internal path error, ANA
/// states, controller/host path errors, aborted-by-host).
const NVME_SCT_PATH_RELATED: u16 = 0x300;
/// The command-specific status class (SCT 1) — where a vendor array's
/// "registration table full" lives when it is spec-shaped.
const NVME_SCT_COMMAND_SPECIFIC: u16 = 0x100;
/// The vendor-specific status class (SCT 7).
const NVME_SCT_VENDOR_SPECIFIC: u16 = 0x700;

/// Whether a device-answered status is a TRANSIENT class — the aborts,
/// the interrupted / transient-transport pair, Namespace Not Ready, every
/// path-related status: a refusal that says nothing about the registrant
/// table.
pub fn nvme_status_is_transient(status: u16) -> bool {
    let s = status & 0x7ff;
    matches!(
        s,
        NVME_SC_ABORT_REQ
            | NVME_SC_ABORT_QUEUE
            | NVME_SC_FUSED_FAIL
            | NVME_SC_FUSED_MISSING
            | NVME_SC_CMD_INTERRUPTED
            | NVME_SC_TRANSIENT_TRANSPORT
            | NVME_SC_NAMESPACE_NOT_READY
    ) || (NVME_SCT_PATH_RELATED..NVME_SCT_PATH_RELATED + 0x80).contains(&s)
}

/// Whether a REGISTER refusal is one the registrant cap may be LEARNED
/// from: a device-answered status in the command-specific or
/// vendor-specific class — the only classes a "registration table full"
/// is expressed in — never the reservation conflict, never a generic or
/// transient status, never an errno.
fn nvme_status_learnable(status: u16) -> bool {
    let s = status & 0x7ff;
    if nvme_status_is_transient(s) || i32::from(s) == NVME_SC_RESERVATION_CONFLICT {
        return false;
    }
    (NVME_SCT_COMMAND_SPECIFIC..NVME_SCT_COMMAND_SPECIFIC + 0x100).contains(&s)
        || (NVME_SCT_VENDOR_SPECIFIC..=0x7ff).contains(&s)
}

/// Forget the learned cap and its candidate (the unlearn arm; the
/// contract suite's reset).
pub fn clear_learned_registrant_cap() {
    PR_REGISTRANT_CAP_LEARNED.store(0, Ordering::Release);
    PR_REGISTRANT_CAP_CANDIDATE.store(0, Ordering::Release);
}

/// The registrant cap in force: declared wins, else learned, else 0 =
/// unbounded (`pr_registrant_cap` on the stats inode).
pub fn pr_registrant_cap() -> u64 {
    match crate::env_knobs::opt_int_knob::<u64>(PR_REGISTRANT_CAP_ENV) {
        Some(declared) => declared,
        None => PR_REGISTRANT_CAP_LEARNED.load(Ordering::Relaxed),
    }
}

/// `pr_registrant_cap_refusals`.
pub fn pr_registrant_cap_refusals() -> u64 {
    PR_REGISTRANT_CAP_REFUSALS.load(Ordering::Relaxed)
}

/// Learn a cap from the device (monotone — the smallest observed) — ONLY
/// on the REPEAT of the same learnable `status` at the same `regctl`
/// (the first is the candidate). Returns whether the cap was learned.
fn learn_registrant_cap(status: u16, regctl: usize) -> bool {
    if regctl == 0 {
        return false;
    }
    let pair = (u64::from(status) << 32) | regctl as u64;
    let prior = PR_REGISTRANT_CAP_CANDIDATE.swap(pair, Ordering::AcqRel);
    if prior != pair {
        return false;
    }
    let n = regctl as u64;
    let _ = PR_REGISTRANT_CAP_LEARNED.fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
        (cur == 0 || n < cur).then_some(n)
    });
    true
}

/// A REGISTER SUCCEEDED with `regctl` registrants on the namespace: a
/// learned cap at or below that count was wrong — unlearned (the
/// declared knob is the operator's and is never touched).
fn unlearn_registrant_cap_on_success(regctl: usize) {
    let learned = PR_REGISTRANT_CAP_LEARNED.load(Ordering::Acquire);
    if learned != 0 && regctl as u64 >= learned {
        clear_learned_registrant_cap();
        log::info!(
            "reservation REGISTER succeeded with {regctl} registrant(s) on the namespace — the \
             learned registrant cap {learned} was wrong and is unlearned (pr_registrant_cap \
             reads unbounded again)"
        );
    }
}

/// **The registrant-cap gate** (KD-SYM-18 / KD-SYM-23): before a join
/// registers on `namespace`, its report's `REGCTL` is read against the
/// cap in force; at or past it the join refuses LOUD, naming the count,
/// the namespace and the remedy — nvmet in front of the array, or fewer
/// hosts per namespace. Unbounded (0) never refuses. Records the
/// namespace's report gauges as a side effect (every join reads the
/// report sized by REGCTL).
pub fn registrant_cap_gate(
    namespace: &Path,
    report: &ReservationReport,
    report_bytes: u64,
) -> io::Result<()> {
    let regctl = report.regctl();
    note_report_gauge(namespace, regctl, report_bytes);
    let cap = pr_registrant_cap();
    if cap != 0 && regctl as u64 >= cap {
        PR_REGISTRANT_CAP_REFUSALS.fetch_add(1, Ordering::Relaxed);
        return Err(io::Error::other(format!(
            "namespace {} already carries {regctl} registrant(s) and its registrant cap is \
             {cap} ({}) — refusing to register a further host: put the kernel nvmet target in \
             front of the array (its registrant list is unbounded — the ONE supported target, \
             R-SYM-8) or use fewer hosts per namespace (pr_registrant_cap_refusals)",
            namespace.display(),
            if crate::env_knobs::opt_int_knob::<u64>(PR_REGISTRANT_CAP_ENV).is_some() {
                format!("declared by {PR_REGISTRANT_CAP_ENV}")
            } else {
                "learned from the device's repeated refused REGISTER".to_string()
            },
        )));
    }
    Ok(())
}

/// The reservation-conflict errno class (design §5.0 B1 pt 3: "the
/// kernel-mapped errno for the reservation-conflict block status —
/// `EBADE` class; M1 pins the exact mapping"): the block layer maps
/// `BLK_STS_RESV_CONFLICT` (né `BLK_STS_NEXUS`) to `-EBADE`, which is
/// what a fenced holder's `fdatasync` returns. Anything else falls back
/// to the generic consecutive-barrier-failure escalation.
pub fn reservation_conflict_error() -> io::Error {
    io::Error::from_raw_os_error(libc::EBADE)
}

/// Whether `e` is the reservation-conflict errno class (see
/// [`reservation_conflict_error`]).
pub fn is_reservation_conflict(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EBADE)
}

/// The reservation protocol surface the mount guard drives (design §5.0
/// B1). Methods are synchronous one-shot commands (micro-ioctls on the
/// real client, pure memory on the fake); async call sites wrap them in
/// `spawn_blocking`.
pub trait ReservationClient: Send + Sync + std::fmt::Debug {
    /// Identify Namespace `RESCAP` byte. `0` = the namespace advertises
    /// no reservation support (the guard degrades to detection grade).
    fn rescap(&self) -> io::Result<u8>;

    /// The host identity this client's controller association carries
    /// (stable-hostnqn/hostid requirement, §5.0 B1 pt 6).
    fn host_identity(&self) -> io::Result<HostIdentity>;

    /// The Host Identifier bytes THIS controller association is
    /// registered under **as the device sees them** — the value a
    /// Reservation Report registrant carries for us (Get Features
    /// `Host Identifier`, 16 B extended form on every fabrics
    /// association). Empty when the device cannot report one; the
    /// register ladder then fails closed (it can no longer prove a
    /// conflicting registration is OURS). Distinct from
    /// [`Self::host_identity`]: the `/etc/nvme` convention can diverge
    /// from the association's actual on-wire identifier (observed in
    /// the 2026-07-17 SPDK scoping session), and only the device's
    /// answer is authoritative for matching registrants.
    fn wire_host_id(&self) -> io::Result<Vec<u8>>;

    /// Reservation Register (RREGA = register) with PTPL requested where
    /// supported. Registering an already-registered key is idempotent.
    fn register(&self, key: u64) -> io::Result<()>;

    /// Reservation Register with the UNREGISTER action (RREGA = 1,
    /// `crkey` = `key`): removes THIS host's registration under `key`.
    /// Same-host-scoped **by the device**: the target validates `crkey`
    /// against the issuing host's own registration, so a foreign
    /// registration can never be removed through this verb (foreign
    /// removal is exclusively [`Self::preempt`] territory). Unregistering
    /// the reservation holder's key releases the reservation with it.
    fn unregister(&self, key: u64) -> io::Result<()>;

    /// Reservation Acquire, Write Exclusive. A conflict (another
    /// registrant holds the reservation) returns the
    /// [`reservation_conflict_error`] class.
    fn acquire_write_exclusive(&self, key: u64) -> io::Result<()>;

    /// Reservation Acquire, **Write Exclusive – Registrants Only**
    /// (WERO, rtype 3 — design-volume-lifecycle §5.1.6 / KD-15): every
    /// *registered* host keeps writing; only unregistered hosts are
    /// write-blocked. The job-wire coordinator's fence on shared **data**
    /// namespaces while remote workers are enrolled — disjoint from D0's
    /// rtype-1 Write Exclusive on meta volumes (different namespaces,
    /// different rtypes, no interaction).
    fn acquire_write_exclusive_registrants_only(&self, key: u64) -> io::Result<()>;

    /// Reservation Acquire with the PREEMPT action: unregister
    /// `victim_key` and take the Write Exclusive reservation for `key`.
    /// Only safe against a TTL-stale holder — the device fences the
    /// victim (design §5.0 B1 pt 3).
    fn preempt(&self, key: u64, victim_key: u64) -> io::Result<()>;

    /// PREEMPT under a standing WERO reservation (rtype 3): remove the
    /// expired worker host's registration — its resumed DMA is
    /// device-rejected — while the reservation (and every other
    /// registrant's write access) stands (design-volume-lifecycle
    /// §5.1.6 rung 2).
    fn preempt_registrants_only(&self, key: u64, victim_key: u64) -> io::Result<()>;

    /// Reservation Release (clean unmount).
    fn release(&self, key: u64) -> io::Result<()>;

    /// Release a WERO (rtype 3) reservation — the job-wire coordinator's
    /// last-remote-departure teardown; like [`Self::release`] it leaves
    /// zero residue (the registration is dropped with it).
    fn release_registrants_only(&self, key: u64) -> io::Result<()>;

    /// Reservation Report: current holder + registrants (the heartbeat
    /// PTPL-lapse re-check, §5.0 B1 pt 6). Read sized by `REGCTL` — every
    /// registrant the header names is decoded ([`parse_reservation_report`]).
    fn report(&self) -> io::Result<ReservationReport>;

    /// Bytes the last [`Self::report`] transferred (`pr_report_bytes`):
    /// the REGCTL-sized second read's length on the real client.
    fn report_bytes(&self) -> u64;
}

/// Outcome of [`register_ladder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// Plain Register succeeded — the fast path, byte-identical to the
    /// pre-ladder guard. Lenient targets (kernel nvmet: IEKEY register
    /// replaces this host's stale key in place) never leave it.
    Registered,
    /// The ladder fired: a spec-strict target (SPDK v26.05, measured
    /// 2026-07-17) refused Register because this HOST already held a
    /// registration under a different key — a crashed incarnation's
    /// residue. The ladder proved via Reservation Report that the
    /// conflicting registration carries OUR association's Host
    /// Identifier, unregistered exactly those keys (dropping any
    /// reservation held under them), and re-registered fresh.
    RecoveredOwnStale { unregistered: Vec<u64> },
}

/// The register ladder (design-metadata-throughput §5.0 B1; SPDK-strict
/// Register recovery, `.benchmarks/2026-07-17-spdk-target-scoping.md` §4/§7 Q1):
///
/// 1. Plain `register(key)` — success is today's fast path, untouched.
/// 2. On the reservation-conflict class ONLY: Reservation Report →
///    identify registrations carrying **our own** association Host
///    Identifier ([`ReservationClient::wire_host_id`]) with a stale
///    (≠ `key`) rkey — the kill-9'd incarnation's residue on a target
///    with spec-strict Register semantics.
/// 3. Unregister exactly those own-stale keys (device-validated `crkey`;
///    unregistering a holder key releases its reservation), then
///    register `key` fresh.
///
/// **Never touches a foreign registration**: registrants whose Host
/// Identifier differs from ours — including an unreadable/empty own
/// identifier — fall through **fail-closed** with the original conflict
/// error (foreign arbitration stays the acquire-conflict / claim /
/// preempt path, which this ladder must not widen).
pub fn register_ladder(client: &dyn ReservationClient, key: u64) -> io::Result<RegisterOutcome> {
    let Err(conflict) = client.register(key) else {
        // A success at or past a LEARNED cap disproves it (Issue 5's
        // unlearn arm); the report is read only while a cap is learned,
        // so the shipped fast path pays nothing.
        if PR_REGISTRANT_CAP_LEARNED.load(Ordering::Acquire) != 0 {
            if let Ok(report) = client.report() {
                unlearn_registrant_cap_on_success(report.regctl());
            }
        }
        return Ok(RegisterOutcome::Registered);
    };
    if !is_reservation_conflict(&conflict) {
        // A REGISTER refused by the DEVICE with a command-specific or
        // vendor-specific status — the classes a vendor array's
        // "registration table full" is expressed in — on a namespace
        // that reports registrants is the table binding (KD-SYM-18): the
        // count it holds is the cap this process learns, so the next
        // join refuses BEFORE registering instead of failing here again.
        // Learned only when the SAME status repeats at the same count
        // (one refusal is a candidate); never from a transport errno
        // (EIO on a path failover, ENXIO on a disconnect, ENOTTY on a
        // still-connecting controller), never from a generic, transient
        // or path-related status (review round 1, Issue 5).
        if let Some(status) = nvme_status_of(&conflict).filter(|s| nvme_status_learnable(*s)) {
            if let Ok(report) = client.report() {
                if report.regctl() > 0 && learn_registrant_cap(status, report.regctl()) {
                    log::warn!(
                        "reservation REGISTER refused twice with status {status:#x} at {} \
                         registrant(s) on the namespace — learned as this array's registrant \
                         cap (pr_registrant_cap); the next join refuses at it",
                        report.regctl()
                    );
                }
            }
        }
        return Err(conflict);
    }
    // Spec-strict Register (SPDK v26.05 measured, scoping §4 pt 2): the
    // expected cause is OUR OWN stale registration — a kill-9'd
    // incarnation's residue under the same host identity, which a
    // lenient target (kernel nvmet) would have silently replaced. Prove
    // ownership from the device before touching anything; every
    // unprovable shape falls through with the ORIGINAL conflict error
    // (classification preserved) and loud diagnostics.
    let ours = match client.wire_host_id() {
        Ok(id) if !id.is_empty() => id,
        Ok(_) => {
            log::warn!(
                "register ladder: register conflicted and the device reports no host \
                 identifier for this association — failing closed (nothing unregistered)"
            );
            return Err(conflict);
        }
        Err(e) => {
            log::warn!(
                "register ladder: register conflicted and the association host id is \
                 unreadable ({e}) — failing closed with the conflict (nothing unregistered)"
            );
            return Err(conflict);
        }
    };
    let report = match client.report() {
        Ok(r) => r,
        Err(e) => {
            log::warn!(
                "register ladder: register conflicted and the reservation report failed \
                 ({e}) — failing closed with the conflict (nothing unregistered)"
            );
            return Err(conflict);
        }
    };
    let own_stale: Vec<u64> = report
        .registrants
        .iter()
        .filter(|r| r.host_id == ours && r.rkey != key)
        .map(|r| r.rkey)
        .collect();
    if own_stale.is_empty() {
        log::warn!(
            "register ladder: register conflicted but the report shows no registration \
             under our host id {ours:02x?} (registrants: {:?}) — a foreign registration \
             is not ours to remove; failing closed (arbitration stays the \
             acquire-conflict / claim / preempt path)",
            report.registrants
        );
        return Err(conflict);
    }
    for stale in &own_stale {
        // Device-validated crkey: even here, the target itself refuses
        // to remove anything not registered to THIS host.
        client.unregister(*stale).map_err(|e| {
            io::Error::other(format!(
                "unregistering our own stale registration {stale:#018x} failed: {e} \
                 (register ladder; original conflict: {conflict})"
            ))
        })?;
    }
    client.register(key).map_err(|e| {
        io::Error::other(format!(
            "fresh register after recovering own stale registration(s) {own_stale:#018x?} \
             failed: {e} (register ladder)"
        ))
    })?;
    Ok(RegisterOutcome::RecoveredOwnStale {
        unregistered: own_stale,
    })
}

// ---------------------------------------------------------------------------
// Test override registry: `KvMetaBackend::open` resolves a volume's
// reservation client through here first, so the cargo tier can exercise
// the full B1 protocol against the in-memory fake (design §5.0 B1 pt 5).
// ---------------------------------------------------------------------------

fn overrides() -> &'static Mutex<HashMap<PathBuf, Arc<dyn ReservationClient>>> {
    static OVERRIDES: OnceLock<Mutex<HashMap<PathBuf, Arc<dyn ReservationClient>>>> =
        OnceLock::new();
    OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Install a reservation client for `path` (guard-harness surface; the
/// next `open` of that volume drives the B1 protocol against it instead
/// of probing the real device).
pub fn install_override(path: impl AsRef<Path>, client: Arc<dyn ReservationClient>) {
    overrides()
        .lock()
        .unwrap()
        .insert(path.as_ref().to_path_buf(), client);
}

/// Remove the override for `path` (test teardown).
pub fn clear_override(path: impl AsRef<Path>) {
    overrides().lock().unwrap().remove(path.as_ref());
}

/// The override installed for `path`, if any.
pub fn override_for(path: &Path) -> Option<Arc<dyn ReservationClient>> {
    overrides().lock().unwrap().get(path).cloned()
}

/// Resolve the reservation client the mount guard will drive for `path`
/// (design §5.0 B1): the test override wins; otherwise probe the real
/// device. Returns `Some` only when the namespace advertises reservation
/// support (`RESCAP ≠ 0`) — everything else degrades to detection grade
/// (`None`), **never** failing the mount. Blocking (one-shot ioctls);
/// call off the async runtime.
pub fn resolve_for_mount(path: &Path) -> Option<Arc<dyn ReservationClient>> {
    let client: Arc<dyn ReservationClient> = match override_for(path) {
        Some(c) => c,
        None => NvmeReservationClient::open(path)?,
    };
    match client.rescap() {
        Ok(cap) if cap != 0 => Some(client),
        Ok(_) => {
            log::info!(
                "meta volume {}: namespace advertises no reservation support (RESCAP=0) \
                 — single-writer guard degrades to detection grade (flock+claim)",
                path.display()
            );
            None
        }
        Err(e) => {
            log::debug!(
                "meta volume {}: RESCAP probe failed ({e}) — detection grade",
                path.display()
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The real client: NVMe passthru ioctls on the namespace fd.
// ---------------------------------------------------------------------------

/// `struct nvme_passthru_cmd` (uapi/linux/nvme_ioctl.h) — one layout for
/// admin and I/O passthru.
#[repr(C)]
#[derive(Default)]
struct NvmePassthruCmd {
    opcode: u8,
    flags: u8,
    rsvd1: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    addr: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    result: u32,
}

/// `_IO('N', 0x40)` — returns the namespace id.
const NVME_IOCTL_ID: libc::c_ulong = 0x4E40;
/// `_IOWR('N', 0x41, struct nvme_admin_cmd)`.
const NVME_IOCTL_ADMIN_CMD: libc::c_ulong = 0xC048_4E41;
/// `_IOWR('N', 0x43, struct nvme_passthru_cmd)`.
const NVME_IOCTL_IO_CMD: libc::c_ulong = 0xC048_4E43;

/// NVMe opcodes (NVM command set + admin).
const NVME_ADMIN_IDENTIFY: u8 = 0x06;
const NVME_ADMIN_GET_FEATURES: u8 = 0x0a;
const NVME_CMD_RESV_REGISTER: u8 = 0x0d;
const NVME_CMD_RESV_REPORT: u8 = 0x0e;
const NVME_CMD_RESV_ACQUIRE: u8 = 0x11;
const NVME_CMD_RESV_RELEASE: u8 = 0x15;

/// Reservation type: Write Exclusive (reads from all hosts, writes from
/// the holder only — probes keep working, §5.0 B1 pt 2).
const RTYPE_WRITE_EXCLUSIVE: u32 = 1;
/// Reservation type: Write Exclusive – Registrants Only (registrants
/// write, unregistered hosts blocked — the §5.1.6 job-wire fence).
///
/// **3, per the NVMe Base Spec / the kernel's `enum nvme_pr_type`**
/// (`include/linux/nvme.h`: 1 = Write Exclusive, 2 = Exclusive Access,
/// 3 = Write Exclusive – Registrants Only). This constant shipped as
/// `2` until the `fix/wero-rtype` rung: rtype 2 is EXCLUSIVE ACCESS,
/// under which a REGISTERED second host is refused reads AND writes
/// (measured live on kernel nvmet, 2026-08-15) — the opposite of the
/// registrants-may-write fence every consumer means. Pinned against
/// the enum values in `tests/wero_rtype_tests.rs`.
const RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY: u32 = 3;
/// NVMe generic status: Reservation Conflict.
const NVME_SC_RESERVATION_CONFLICT: i32 = 0x83;

/// The passthru ioctls on a real NVMe namespace: `RESCAP` via Identify
/// Namespace, Register / Acquire / Preempt / Release / Report as NVM I/O
/// commands, and the host identity from the controller's sysfs (fabrics)
/// or `/etc/nvme` (the nvme-cli convention the repo's connect path
/// defers to, `nvmeof.rs`).
#[derive(Debug)]
pub struct NvmeReservationClient {
    file: std::fs::File,
    nsid: u32,
    /// The namespace's device path — the report gauges' key.
    path: PathBuf,
    /// Bytes the last Reservation Report transferred (`pr_report_bytes`).
    last_report_bytes: AtomicU64,
}

impl NvmeReservationClient {
    /// Open `path` as an NVMe namespace: block device + answering
    /// `NVME_IOCTL_ID`. `None` for regular files, loop devices, and
    /// anything else that is not an NVMe namespace.
    pub fn open(path: &Path) -> Option<Arc<Self>> {
        use std::os::unix::fs::FileTypeExt;
        let meta = std::fs::metadata(path).ok()?;
        if !meta.file_type().is_block_device() {
            return None;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .ok()?;
        // SAFETY: NVME_IOCTL_ID takes no argument and returns the nsid
        // (or -1/errno on non-NVMe nodes).
        let rc = unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&file), NVME_IOCTL_ID) };
        if rc <= 0 {
            return None;
        }
        Some(Arc::new(Self {
            file,
            nsid: rc as u32,
            path: path.to_path_buf(),
            last_report_bytes: AtomicU64::new(0),
        }))
    }

    fn fd(&self) -> i32 {
        std::os::fd::AsRawFd::as_raw_fd(&self.file)
    }

    /// Issue one passthru command; maps the NVMe Reservation Conflict
    /// status to the [`reservation_conflict_error`] class and every other
    /// nonzero status / errno to a descriptive `io::Error`.
    fn passthru(&self, ioctl: libc::c_ulong, cmd: &mut NvmePassthruCmd) -> io::Result<()> {
        // SAFETY: cmd is a properly-initialized repr(C) struct whose
        // addr/data_len name a live buffer for the command's transfer.
        let rc = unsafe { libc::ioctl(self.fd(), ioctl, cmd as *mut NvmePassthruCmd) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            // Some paths surface the conflict as the mapped block-status
            // errno rather than an NVMe status value.
            if is_reservation_conflict(&e) {
                return Err(reservation_conflict_error());
            }
            return Err(e);
        }
        if rc != 0 {
            // Positive return = NVMe status (SCT/SC, sans phase); the
            // reservation-conflict status is the arbitration signal.
            if rc & 0x7ff == NVME_SC_RESERVATION_CONFLICT {
                return Err(reservation_conflict_error());
            }
            // Typed, so the register ladder can classify the refusal
            // (learnable vs transient) without reading a message.
            return Err(nvme_status_error(cmd.opcode, (rc & 0x7ff) as u16));
        }
        Ok(())
    }

    /// Reservation Register / Unregister / Replace share one shape:
    /// 16 B payload `[crkey, nrkey]`, action in CDW10.
    fn resv_register(&self, cdw10: u32, crkey: u64, nrkey: u64) -> io::Result<()> {
        let mut data = [0u8; 16];
        data[..8].copy_from_slice(&crkey.to_le_bytes());
        data[8..].copy_from_slice(&nrkey.to_le_bytes());
        let mut cmd = NvmePassthruCmd {
            opcode: NVME_CMD_RESV_REGISTER,
            nsid: self.nsid,
            addr: data.as_mut_ptr() as u64,
            data_len: data.len() as u32,
            cdw10,
            ..Default::default()
        };
        self.passthru(NVME_IOCTL_IO_CMD, &mut cmd)
    }

    /// One Reservation Report transfer of `len` bytes (a multiple of 4;
    /// CDW10 carries the 0-based dword count).
    fn report_transfer(&self, extended: bool, len: usize) -> io::Result<Vec<u8>> {
        let mut data = vec![0u8; len];
        let numd = (len / 4 - 1) as u32;
        let mut cmd = NvmePassthruCmd {
            opcode: NVME_CMD_RESV_REPORT,
            nsid: self.nsid,
            addr: data.as_mut_ptr() as u64,
            data_len: len as u32,
            cdw10: numd,
            cdw11: u32::from(extended), // EDS
            ..Default::default()
        };
        self.passthru(NVME_IOCTL_IO_CMD, &mut cmd)?;
        Ok(data)
    }

    /// The Reservation Report, read SIZED BY `REGCTL` in two steps: the
    /// header first (it carries the registrant count), then `header +
    /// stride × REGCTL` — so every registrant is decoded whatever their
    /// number ([`parse_reservation_report`]). A count that grew between
    /// the two reads is re-read once at the new size; the parse decodes
    /// what the final transfer holds. The transfer length is recorded for
    /// `pr_report_bytes`.
    fn report_with(&self, extended: bool) -> io::Result<ReservationReport> {
        let hdr = report_header_len(extended);
        let head = self.report_transfer(extended, hdr)?;
        let mut regctl = u16::from_le_bytes([head[5], head[6]]);
        let mut data = self.report_transfer(extended, report_len_for(regctl, extended))?;
        let seen = u16::from_le_bytes([data[5], data[6]]);
        if seen > regctl {
            regctl = seen;
            data = self.report_transfer(extended, report_len_for(regctl, extended))?;
        }
        let bytes = data.len() as u64;
        self.last_report_bytes.store(bytes, Ordering::Relaxed);
        let report = parse_reservation_report(&data, extended)?;
        note_report_gauge(&self.path, report.regctl(), bytes);
        Ok(report)
    }

    /// Reservation Acquire / Preempt: 16 B payload `[crkey, prkey]`,
    /// reservation type in CDW10 bits 15:8 (rtype 1 = D0's Write
    /// Exclusive, rtype 3 = the §5.1.6 WERO fence).
    fn resv_acquire(&self, racqa: u32, rtype: u32, crkey: u64, prkey: u64) -> io::Result<()> {
        let mut data = [0u8; 16];
        data[..8].copy_from_slice(&crkey.to_le_bytes());
        data[8..].copy_from_slice(&prkey.to_le_bytes());
        let mut cmd = NvmePassthruCmd {
            opcode: NVME_CMD_RESV_ACQUIRE,
            nsid: self.nsid,
            addr: data.as_mut_ptr() as u64,
            data_len: data.len() as u32,
            cdw10: (rtype << 8) | racqa,
            ..Default::default()
        };
        self.passthru(NVME_IOCTL_IO_CMD, &mut cmd)
    }

    /// Reservation Release for `rtype` (the release must name the held
    /// reservation type), followed by unregister for zero residue.
    fn resv_release(&self, rtype: u32, key: u64) -> io::Result<()> {
        let mut data = [0u8; 8];
        data.copy_from_slice(&key.to_le_bytes());
        let mut cmd = NvmePassthruCmd {
            opcode: NVME_CMD_RESV_RELEASE,
            nsid: self.nsid,
            addr: data.as_mut_ptr() as u64,
            data_len: data.len() as u32,
            cdw10: rtype << 8, // RRELA 0: release
            ..Default::default()
        };
        self.passthru(NVME_IOCTL_IO_CMD, &mut cmd)?;
        // NVMe release does NOT unregister; drop the registration too so
        // a clean teardown leaves zero residue on the namespace (a stale
        // registration would make this host's next fresh-key register
        // conflict — observed against kernel nvmet in the M1 session,
        // and refused outright by spec-strict targets like SPDK).
        self.unregister(key)
    }
}

impl ReservationClient for NvmeReservationClient {
    fn rescap(&self) -> io::Result<u8> {
        // Identify Namespace (CNS 0): RESCAP is byte 31.
        let mut data = vec![0u8; 4096];
        let mut cmd = NvmePassthruCmd {
            opcode: NVME_ADMIN_IDENTIFY,
            nsid: self.nsid,
            addr: data.as_mut_ptr() as u64,
            data_len: data.len() as u32,
            cdw10: 0, // CNS 0: Identify Namespace
            ..Default::default()
        };
        self.passthru(NVME_IOCTL_ADMIN_CMD, &mut cmd)?;
        Ok(data[31])
    }

    fn host_identity(&self) -> io::Result<HostIdentity> {
        // The nvme-cli convention (the repo's connect path defers to it):
        // /etc/nvme/hostnqn + /etc/nvme/hostid. Absent files read as
        // empty — stability then means "still absent" at the re-check.
        let read = |p: &str| -> String {
            std::fs::read_to_string(p)
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        Ok(HostIdentity {
            hostnqn: read("/etc/nvme/hostnqn"),
            hostid: read("/etc/nvme/hostid"),
        })
    }

    fn wire_host_id(&self) -> io::Result<Vec<u8>> {
        // Get Features, FID 0x81 (Host Identifier) — mandatory on
        // controllers that support reservations. Fabrics controllers
        // require the extended (128-bit) form: CDW11 bit 0 EXHID = 1,
        // 16 B data transfer (kernel nvmet rejects EXHID = 0 with
        // Invalid Field; SPDK likewise). Fall back to the 64-bit form
        // for PCIe controllers without 128-bit support. All-zero (no
        // host identifier set) reads as "unknown" — empty, so the
        // register ladder fails closed rather than matching zeros.
        // Deliberately NO /etc/nvme fallback: the 2026-07-17 scoping
        // session measured the association identity diverging from the
        // config files, and matching a value the device did not
        // attribute to this association could cross the foreign-
        // registration line.
        let get = |exhid: bool, len: usize| -> io::Result<Vec<u8>> {
            let mut data = vec![0u8; len];
            let mut cmd = NvmePassthruCmd {
                opcode: NVME_ADMIN_GET_FEATURES,
                nsid: 0,
                addr: data.as_mut_ptr() as u64,
                data_len: len as u32,
                cdw10: 0x81, // FID: Host Identifier (SEL 0: current)
                cdw11: u32::from(exhid),
                ..Default::default()
            };
            self.passthru(NVME_IOCTL_ADMIN_CMD, &mut cmd)?;
            Ok(data)
        };
        let id = get(true, 16).or_else(|_| get(false, 8))?;
        if id.iter().all(|b| *b == 0) {
            return Ok(Vec::new());
        }
        Ok(id)
    }

    fn register(&self, key: u64) -> io::Result<()> {
        // RREGA 0 (register) + IEKEY (bit 3: ignore existing key) +
        // CPTPL 11b (bits 31:30: persist through power loss where
        // supported). NOTE (2026-07-17, guard-pr-register-ladder): IEKEY
        // replace-on-register is NOT portable — spec-strict targets
        // (SPDK v26.05 AND kernel nvmet ≥ its pr.c implementation, both
        // measured) conflict on a different-key re-register; crash
        // recovery is [`register_ladder`]'s job, never this shape's.
        let cptpl_set = 0b11u32 << 30;
        match self.resv_register(cptpl_set | 0x8, 0, key) {
            Ok(()) => Ok(()),
            Err(e) if is_reservation_conflict(&e) => Err(e),
            Err(_) => {
                // Targets vary on IEKEY/CPTPL support (both are probed in
                // the M1 root session): retry the plain shape.
                self.resv_register(0x8, 0, key).or_else(|_| {
                    // Last resort: no IEKEY (key not previously registered).
                    self.resv_register(0, 0, key)
                })
            }
        }
    }

    fn unregister(&self, key: u64) -> io::Result<()> {
        // RREGA 1: unregister, crkey = our key (the device validates
        // crkey against THIS host's registration — same-host-scoped).
        self.resv_register(1, key, 0)
    }

    fn acquire_write_exclusive(&self, key: u64) -> io::Result<()> {
        self.resv_acquire(0, RTYPE_WRITE_EXCLUSIVE, key, 0)
    }

    fn acquire_write_exclusive_registrants_only(&self, key: u64) -> io::Result<()> {
        self.resv_acquire(0, RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY, key, 0)
    }

    fn preempt(&self, key: u64, victim_key: u64) -> io::Result<()> {
        self.resv_acquire(1, RTYPE_WRITE_EXCLUSIVE, key, victim_key)
    }

    fn preempt_registrants_only(&self, key: u64, victim_key: u64) -> io::Result<()> {
        self.resv_acquire(1, RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY, key, victim_key)
    }

    fn release(&self, key: u64) -> io::Result<()> {
        self.resv_release(RTYPE_WRITE_EXCLUSIVE, key)
    }

    fn release_registrants_only(&self, key: u64) -> io::Result<()> {
        self.resv_release(RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY, key)
    }

    fn report(&self) -> io::Result<ReservationReport> {
        // Reservation Report. Controllers with 128-bit host identifiers —
        // every fabrics association, hence the repo's nvmet-loop shape —
        // require the EXTENDED data structure (EDS = 1, CDW11 bit 0);
        // asking for the short form there fails with Host Identifier
        // Inconsistent Format (SC 0x18 — observed against kernel nvmet in
        // the M1 root session). Try extended first, fall back to the
        // short form for 64-bit-hostid (PCIe) controllers.
        match self.report_with(true) {
            Ok(rep) => Ok(rep),
            Err(e) if is_reservation_conflict(&e) => Err(e),
            Err(_) => self.report_with(false),
        }
    }

    fn report_bytes(&self) -> u64 {
        self.last_report_bytes.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// In-memory fake: one shared "namespace" (the reservation state lives in
// the *device*), any number of per-"host" clients against it.
// ---------------------------------------------------------------------------

/// The Register semantics a [`FakeNvmeNamespace`] models — the one
/// behavior axis the 2026-07-17 SPDK scoping pass measured targets
/// disagreeing on (scoping-report §4 pt 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterSemantics {
    /// Spec-strict (SPDK v26.05, measured): Register (RREGA = 0, IEKEY
    /// or not) from a host that already holds a different-key
    /// registration returns Reservation Conflict.
    SpecStrict,
    /// Kernel-nvmet observed behavior (M1 root session): the guard's
    /// IEKEY register from an already-registered host replaces its key
    /// in place (the reservation, if held under the old key, follows).
    LenientReplace,
}

/// The device-side reservation state a [`FakeReservationClient`] operates
/// on. Shared (`Arc`) between fake clients to model multiple hosts
/// against one namespace; carries test-priming and observation hooks.
/// Registrations are attributed to the registrant's **host identifier**,
/// as on a real target.
#[derive(Debug)]
pub struct FakeNvmeNamespace {
    rescap: u8,
    semantics: RegisterSemantics,
    state: Mutex<FakeNsState>,
    preempts: AtomicU64,
    unregisters: AtomicU64,
}

/// One registered host on the fake namespace.
#[derive(Debug)]
struct FakeRegistrant {
    host_id: Vec<u8>,
    key: u64,
}

#[derive(Debug, Default)]
struct FakeNsState {
    holder: Option<u64>,
    /// The held reservation's type — meaningful only while `holder` is
    /// `Some` (rtype 1 = Write Exclusive, rtype 3 = WERO).
    rtype: u32,
    registered: Vec<FakeRegistrant>,
}

/// The synthetic host identifier [`FakeNvmeNamespace::seed_holder`]
/// attributes its foreign registration to.
const SEEDED_FOREIGN_HOST_ID: &[u8] = b"seeded-foreign-host";

impl FakeNvmeNamespace {
    /// A PR-capable namespace (`RESCAP` = PTPL + Write Exclusive bits)
    /// with **spec-strict** Register semantics — the stronger law
    /// (SPDK-measured); the default so every guard test exercises the
    /// strict target unless it opts into the lenient model.
    pub fn new() -> Arc<Self> {
        Self::with_semantics(RegisterSemantics::SpecStrict)
    }

    /// A PR-capable namespace modeling kernel nvmet's lenient
    /// IEKEY-replace Register (the M1-era behavior the guard was built
    /// against).
    pub fn lenient_register() -> Arc<Self> {
        Self::with_semantics(RegisterSemantics::LenientReplace)
    }

    fn with_semantics(semantics: RegisterSemantics) -> Arc<Self> {
        Arc::new(Self {
            rescap: 0x03,
            semantics,
            state: Mutex::new(FakeNsState::default()),
            preempts: AtomicU64::new(0),
            unregisters: AtomicU64::new(0),
        })
    }

    /// A namespace advertising **no** reservation support (`RESCAP` = 0):
    /// the guard must degrade to detection grade.
    pub fn without_pr_support() -> Arc<Self> {
        Arc::new(Self {
            rescap: 0,
            semantics: RegisterSemantics::SpecStrict,
            state: Mutex::new(FakeNsState::default()),
            preempts: AtomicU64::new(0),
            unregisters: AtomicU64::new(0),
        })
    }

    /// Test priming: register `key` to a synthetic FOREIGN host and hand
    /// it the Write Exclusive reservation, as if a foreign host had
    /// mounted.
    pub fn seed_holder(&self, key: u64) {
        let mut st = self.state.lock().unwrap();
        if !st.registered.iter().any(|r| r.key == key) {
            st.registered.push(FakeRegistrant {
                host_id: SEEDED_FOREIGN_HOST_ID.to_vec(),
                key,
            });
        }
        st.holder = Some(key);
        st.rtype = RTYPE_WRITE_EXCLUSIVE;
    }

    /// Test priming: a target power cycle on a PTPL-less target —
    /// reservation AND registrations silently cleared (design §5.0 B1
    /// pt 6 "PTPL").
    pub fn power_cycle(&self) {
        let mut st = self.state.lock().unwrap();
        st.holder = None;
        st.rtype = 0;
        st.registered.clear();
    }

    /// Whether the device would admit a WRITE from the host registered
    /// under `wire_host_id` — the reservation-gating law the real target
    /// enforces per command (the §5.1.6 fence-observation hook): no
    /// reservation ⇒ open; Write Exclusive (rtype 1) ⇒ only the holder's
    /// host; Write Exclusive – Registrants Only (rtype 3) ⇒ any
    /// registered host, unregistered hosts rejected.
    pub fn write_allowed(&self, wire_host_id: &[u8]) -> bool {
        let st = self.state.lock().unwrap();
        let Some(holder) = st.holder else {
            return true;
        };
        match st.rtype {
            RTYPE_WRITE_EXCLUSIVE => st
                .registered
                .iter()
                .any(|r| r.key == holder && r.host_id == wire_host_id),
            RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY => {
                st.registered.iter().any(|r| r.host_id == wire_host_id)
            }
            // Unmodeled rtypes fail closed: the fake never grants what
            // it cannot attribute.
            _ => false,
        }
    }

    /// Current Write Exclusive holder key.
    pub fn holder(&self) -> Option<u64> {
        self.state.lock().unwrap().holder
    }

    /// Whether `key` is registered (any host).
    pub fn is_registered(&self, key: u64) -> bool {
        self.state
            .lock()
            .unwrap()
            .registered
            .iter()
            .any(|r| r.key == key)
    }

    /// PREEMPT actions executed so far.
    pub fn preempt_count(&self) -> u64 {
        self.preempts.load(Ordering::Relaxed)
    }

    /// UNREGISTER actions executed so far (the ladder-law observation
    /// hook: lenient targets and clean fast paths must show 0 outside
    /// the clean-unmount release).
    pub fn unregister_count(&self) -> u64 {
        self.unregisters.load(Ordering::Relaxed)
    }
}

/// One "host"'s view of a [`FakeNvmeNamespace`]: implements the full
/// [`ReservationClient`] protocol against the shared device state, with a
/// mutable host identity so tests can model identity instability
/// (design §5.0 B1 pt 6).
#[derive(Debug)]
pub struct FakeReservationClient {
    ns: Arc<FakeNvmeNamespace>,
    identity: Mutex<HostIdentity>,
}

impl FakeReservationClient {
    pub fn new(ns: Arc<FakeNvmeNamespace>, hostnqn: &str, hostid: &str) -> Arc<Self> {
        Arc::new(Self {
            ns,
            identity: Mutex::new(HostIdentity {
                hostnqn: hostnqn.to_string(),
                hostid: hostid.to_string(),
            }),
        })
    }

    /// Flip this client's host identity (models an unstable
    /// hostnqn/hostid across re-association — must be treated as
    /// *not our registration*).
    pub fn set_identity(&self, hostnqn: &str, hostid: &str) {
        let mut id = self.identity.lock().unwrap();
        id.hostnqn = hostnqn.to_string();
        id.hostid = hostid.to_string();
    }
}

impl FakeReservationClient {
    /// This client's on-wire host identifier: the RFC-4122 bytes when
    /// the hostid parses as a UUID (the real fabrics form), else the raw
    /// string bytes (opaque test identities). Empty hostid ⇒ empty.
    fn my_wire_id(&self) -> Vec<u8> {
        let id = self.identity.lock().unwrap().hostid.clone();
        let id = id.trim().to_string();
        if id.is_empty() {
            return Vec::new();
        }
        match uuid::Uuid::parse_str(&id) {
            Ok(u) => u.as_bytes().to_vec(),
            Err(_) => id.into_bytes(),
        }
    }

    /// Shared Acquire model for both reservation types: the acquiring
    /// host must be registered under `key`; a free namespace takes the
    /// reservation with `rtype`; re-acquiring the held key with the same
    /// rtype is idempotent; everything else conflicts (including an
    /// rtype change on a standing reservation — the real device refuses
    /// that shape too).
    fn fake_acquire(&self, key: u64, rtype: u32) -> io::Result<()> {
        let me = self.my_wire_id();
        let mut st = self.ns.state.lock().unwrap();
        if !st
            .registered
            .iter()
            .any(|r| r.host_id == me && r.key == key)
        {
            // An unregistered host's acquire is a reservation conflict.
            return Err(reservation_conflict_error());
        }
        match st.holder {
            None => {
                st.holder = Some(key);
                st.rtype = rtype;
                Ok(())
            }
            Some(h) if h == key && st.rtype == rtype => Ok(()),
            Some(_) => Err(reservation_conflict_error()),
        }
    }

    /// Shared PREEMPT model: the device-sanctioned foreign-registration
    /// removal — every registrant under the victim key goes; the
    /// preemptor takes/keeps the reservation under `rtype`.
    fn fake_preempt(&self, key: u64, victim_key: u64, rtype: u32) -> io::Result<()> {
        let me = self.my_wire_id();
        let mut st = self.ns.state.lock().unwrap();
        if !st
            .registered
            .iter()
            .any(|r| r.host_id == me && r.key == key)
        {
            return Err(reservation_conflict_error());
        }
        st.registered.retain(|r| r.key != victim_key);
        st.holder = Some(key);
        st.rtype = rtype;
        self.ns.preempts.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Shared Release model: releasing the held key under the named
    /// rtype drops the reservation; this host's registration under `key`
    /// goes with it (zero residue, matching the real client's
    /// release-then-unregister). Releasing the held key under the WRONG
    /// rtype fails loud (the real device refuses a mismatched-type
    /// release) — nothing changes.
    fn fake_release(&self, key: u64, rtype: u32) -> io::Result<()> {
        let me = self.my_wire_id();
        let mut st = self.ns.state.lock().unwrap();
        if st.holder == Some(key) {
            if st.rtype != rtype {
                return Err(reservation_conflict_error());
            }
            st.holder = None;
            st.rtype = 0;
        }
        st.registered.retain(|r| !(r.host_id == me && r.key == key));
        Ok(())
    }
}

impl ReservationClient for FakeReservationClient {
    fn rescap(&self) -> io::Result<u8> {
        Ok(self.ns.rescap)
    }

    fn host_identity(&self) -> io::Result<HostIdentity> {
        Ok(self.identity.lock().unwrap().clone())
    }

    fn wire_host_id(&self) -> io::Result<Vec<u8>> {
        Ok(self.my_wire_id())
    }

    fn register(&self, key: u64) -> io::Result<()> {
        let me = self.my_wire_id();
        let mut st = self.ns.state.lock().unwrap();
        match st.registered.iter_mut().find(|r| r.host_id == me) {
            Some(r) if r.key == key => Ok(()), // idempotent re-register
            Some(r) => match self.ns.semantics {
                // Spec-strict (SPDK-measured): an existing different-key
                // registration for this host conflicts.
                RegisterSemantics::SpecStrict => Err(reservation_conflict_error()),
                // Lenient (kernel nvmet): IEKEY register replaces this
                // host's key; a reservation held under the old key
                // follows the replacement.
                RegisterSemantics::LenientReplace => {
                    let old = r.key;
                    r.key = key;
                    if st.holder == Some(old) {
                        st.holder = Some(key);
                    }
                    Ok(())
                }
            },
            None => {
                st.registered.push(FakeRegistrant { host_id: me, key });
                Ok(())
            }
        }
    }

    fn unregister(&self, key: u64) -> io::Result<()> {
        let me = self.my_wire_id();
        let mut st = self.ns.state.lock().unwrap();
        // Same-host-scoped by the device: only THIS host's registration
        // with a matching crkey can be removed — a foreign registration
        // under the same key value is untouchable here.
        let Some(pos) = st
            .registered
            .iter()
            .position(|r| r.host_id == me && r.key == key)
        else {
            return Err(reservation_conflict_error());
        };
        st.registered.remove(pos);
        if st.holder == Some(key) {
            // Unregistering the holder's key releases the reservation
            // (Write Exclusive is holder-keyed).
            st.holder = None;
            st.rtype = 0;
        }
        self.ns.unregisters.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn acquire_write_exclusive(&self, key: u64) -> io::Result<()> {
        self.fake_acquire(key, RTYPE_WRITE_EXCLUSIVE)
    }

    fn acquire_write_exclusive_registrants_only(&self, key: u64) -> io::Result<()> {
        self.fake_acquire(key, RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY)
    }

    fn preempt(&self, key: u64, victim_key: u64) -> io::Result<()> {
        self.fake_preempt(key, victim_key, RTYPE_WRITE_EXCLUSIVE)
    }

    fn preempt_registrants_only(&self, key: u64, victim_key: u64) -> io::Result<()> {
        self.fake_preempt(key, victim_key, RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY)
    }

    fn release(&self, key: u64) -> io::Result<()> {
        self.fake_release(key, RTYPE_WRITE_EXCLUSIVE)
    }

    fn release_registrants_only(&self, key: u64) -> io::Result<()> {
        self.fake_release(key, RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY)
    }

    fn report(&self) -> io::Result<ReservationReport> {
        let st = self.ns.state.lock().unwrap();
        Ok(ReservationReport {
            holder_key: st.holder,
            registrants: st
                .registered
                .iter()
                .map(|r| ReservationRegistrant {
                    rkey: r.key,
                    host_id: r.host_id.clone(),
                    holds_reservation: st.holder == Some(r.key),
                })
                .collect(),
            rtype: if st.holder.is_some() {
                st.rtype as u8
            } else {
                0
            },
        })
    }

    /// The fake models a fabrics association (128-bit host ids), so its
    /// report is the extended form's length for its registrant count.
    fn report_bytes(&self) -> u64 {
        let n = self.ns.state.lock().unwrap().registered.len();
        report_len_for(n.min(usize::from(u16::MAX)) as u16, true) as u64
    }
}
