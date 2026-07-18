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
}

impl ReservationReport {
    /// Whether `key` appears among the registrants.
    pub fn registered(&self, key: u64) -> bool {
        self.registrants.iter().any(|r| r.rkey == key)
    }
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

    /// Reservation Acquire with the PREEMPT action: unregister
    /// `victim_key` and take the Write Exclusive reservation for `key`.
    /// Only safe against a TTL-stale holder — the device fences the
    /// victim (design §5.0 B1 pt 3).
    fn preempt(&self, key: u64, victim_key: u64) -> io::Result<()>;

    /// Reservation Release (clean unmount).
    fn release(&self, key: u64) -> io::Result<()>;

    /// Reservation Report: current holder + registrants (the heartbeat
    /// PTPL-lapse re-check, §5.0 B1 pt 6).
    fn report(&self) -> io::Result<ReservationReport>;
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
        return Ok(RegisterOutcome::Registered);
    };
    if !is_reservation_conflict(&conflict) {
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
            return Err(io::Error::other(format!(
                "nvme command 0x{:02x} failed with status 0x{rc:x}",
                cmd.opcode
            )));
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

    /// One Reservation Report transfer: header (gen u32, rtype u8,
    /// regctl u16, …, PTPLS) + regctl registered-controller structures —
    /// 24 B each in the short form (64-bit hostid), 64 B each in the
    /// extended form (128-bit hostid; rkey sits before the hostid there).
    fn report_with(&self, extended: bool) -> io::Result<ReservationReport> {
        let mut data = vec![0u8; 4096];
        let numd = (data.len() / 4 - 1) as u32; // 0-based dword count
        let mut cmd = NvmePassthruCmd {
            opcode: NVME_CMD_RESV_REPORT,
            nsid: self.nsid,
            addr: data.as_mut_ptr() as u64,
            data_len: data.len() as u32,
            cdw10: numd,
            cdw11: u32::from(extended), // EDS
            ..Default::default()
        };
        self.passthru(NVME_IOCTL_IO_CMD, &mut cmd)?;
        let rtype = data[4];
        let regctl = u16::from_le_bytes([data[5], data[6]]) as usize;
        // The short form packs 24 B controller structures after a 24 B
        // header; the extended form pads BOTH to 64 B (verified byte-wise
        // against kernel nvmet in the M1 root session — first regctlext
        // at 0x40).
        let (hdr, stride) = if extended { (64, 64) } else { (24, 24) };
        let mut registrants = Vec::with_capacity(regctl);
        let mut holder_key = None;
        for i in 0..regctl {
            let base = hdr + i * stride;
            if base + stride > data.len() {
                break;
            }
            // Both forms open with: cntlid u16, rcsts u8, rsvd…; the
            // short form carries hostid u64 @8 then rkey u64 @16; the
            // extended form carries rkey u64 @8 then hostid[16] @16.
            let rcsts = data[base + 2];
            let (rkey_off, hostid_off, hostid_len) =
                if extended { (8, 16, 16) } else { (16, 8, 8) };
            let rkey = u64::from_le_bytes(
                data[base + rkey_off..base + rkey_off + 8]
                    .try_into()
                    .unwrap(),
            );
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
        })
    }

    /// Reservation Acquire / Preempt: 16 B payload `[crkey, prkey]`.
    fn resv_acquire(&self, racqa: u32, crkey: u64, prkey: u64) -> io::Result<()> {
        let mut data = [0u8; 16];
        data[..8].copy_from_slice(&crkey.to_le_bytes());
        data[8..].copy_from_slice(&prkey.to_le_bytes());
        let mut cmd = NvmePassthruCmd {
            opcode: NVME_CMD_RESV_ACQUIRE,
            nsid: self.nsid,
            addr: data.as_mut_ptr() as u64,
            data_len: data.len() as u32,
            cdw10: (RTYPE_WRITE_EXCLUSIVE << 8) | racqa,
            ..Default::default()
        };
        self.passthru(NVME_IOCTL_IO_CMD, &mut cmd)
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
        self.resv_acquire(0, key, 0)
    }

    fn preempt(&self, key: u64, victim_key: u64) -> io::Result<()> {
        self.resv_acquire(1, key, victim_key)
    }

    fn release(&self, key: u64) -> io::Result<()> {
        let mut data = [0u8; 8];
        data.copy_from_slice(&key.to_le_bytes());
        let mut cmd = NvmePassthruCmd {
            opcode: NVME_CMD_RESV_RELEASE,
            nsid: self.nsid,
            addr: data.as_mut_ptr() as u64,
            data_len: data.len() as u32,
            cdw10: RTYPE_WRITE_EXCLUSIVE << 8, // RRELA 0: release
            ..Default::default()
        };
        self.passthru(NVME_IOCTL_IO_CMD, &mut cmd)?;
        // NVMe release does NOT unregister; drop the registration too so
        // a clean unmount leaves zero residue on the namespace (a stale
        // registration would make this host's next fresh-key register
        // conflict — observed against kernel nvmet in the M1 session,
        // and refused outright by spec-strict targets like SPDK).
        self.unregister(key)
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
    }

    /// Test priming: a target power cycle on a PTPL-less target —
    /// reservation AND registrations silently cleared (design §5.0 B1
    /// pt 6 "PTPL").
    pub fn power_cycle(&self) {
        let mut st = self.state.lock().unwrap();
        st.holder = None;
        st.registered.clear();
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
        }
        self.ns.unregisters.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn acquire_write_exclusive(&self, key: u64) -> io::Result<()> {
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
                Ok(())
            }
            Some(h) if h == key => Ok(()),
            Some(_) => Err(reservation_conflict_error()),
        }
    }

    fn preempt(&self, key: u64, victim_key: u64) -> io::Result<()> {
        let me = self.my_wire_id();
        let mut st = self.ns.state.lock().unwrap();
        if !st
            .registered
            .iter()
            .any(|r| r.host_id == me && r.key == key)
        {
            return Err(reservation_conflict_error());
        }
        // PREEMPT is the one device-sanctioned foreign-registration
        // removal: every registrant under the victim key goes.
        st.registered.retain(|r| r.key != victim_key);
        st.holder = Some(key);
        self.ns.preempts.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn release(&self, key: u64) -> io::Result<()> {
        let me = self.my_wire_id();
        let mut st = self.ns.state.lock().unwrap();
        if st.holder == Some(key) {
            st.holder = None;
        }
        st.registered.retain(|r| !(r.host_id == me && r.key == key));
        Ok(())
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
        })
    }
}
