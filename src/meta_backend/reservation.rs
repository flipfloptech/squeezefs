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

/// One Reservation Report snapshot: the current holder's key (if any
/// reservation is held) and every registered key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationReport {
    pub holder_key: Option<u64>,
    pub registered_keys: Vec<u64>,
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

    /// Reservation Register (RREGA = register) with PTPL requested where
    /// supported. Registering an already-registered key is idempotent.
    fn register(&self, key: u64) -> io::Result<()>;

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

// ---------------------------------------------------------------------------
// In-memory fake: one shared "namespace" (the reservation state lives in
// the *device*), any number of per-"host" clients against it.
// ---------------------------------------------------------------------------

/// The device-side reservation state a [`FakeReservationClient`] operates
/// on. Shared (`Arc`) between fake clients to model multiple hosts
/// against one namespace; carries test-priming and observation hooks.
#[derive(Debug)]
pub struct FakeNvmeNamespace {
    rescap: u8,
    state: Mutex<FakeNsState>,
    preempts: AtomicU64,
}

#[derive(Debug, Default)]
struct FakeNsState {
    holder: Option<u64>,
    registered: Vec<u64>,
}

impl FakeNvmeNamespace {
    /// A PR-capable namespace (`RESCAP` = PTPL + Write Exclusive bits).
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            rescap: 0x03,
            state: Mutex::new(FakeNsState::default()),
            preempts: AtomicU64::new(0),
        })
    }

    /// A namespace advertising **no** reservation support (`RESCAP` = 0):
    /// the guard must degrade to detection grade.
    pub fn without_pr_support() -> Arc<Self> {
        Arc::new(Self {
            rescap: 0,
            state: Mutex::new(FakeNsState::default()),
            preempts: AtomicU64::new(0),
        })
    }

    /// Test priming: register `key` and hand it the Write Exclusive
    /// reservation, as if a foreign host had mounted.
    pub fn seed_holder(&self, key: u64) {
        let mut st = self.state.lock().unwrap();
        if !st.registered.contains(&key) {
            st.registered.push(key);
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

    /// Whether `key` is registered.
    pub fn is_registered(&self, key: u64) -> bool {
        self.state.lock().unwrap().registered.contains(&key)
    }

    /// PREEMPT actions executed so far.
    pub fn preempt_count(&self) -> u64 {
        self.preempts.load(Ordering::Relaxed)
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

impl ReservationClient for FakeReservationClient {
    fn rescap(&self) -> io::Result<u8> {
        Ok(self.ns.rescap)
    }

    fn host_identity(&self) -> io::Result<HostIdentity> {
        Ok(self.identity.lock().unwrap().clone())
    }

    fn register(&self, key: u64) -> io::Result<()> {
        let mut st = self.ns.state.lock().unwrap();
        if !st.registered.contains(&key) {
            st.registered.push(key);
        }
        Ok(())
    }

    fn acquire_write_exclusive(&self, key: u64) -> io::Result<()> {
        let mut st = self.ns.state.lock().unwrap();
        if !st.registered.contains(&key) {
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
        let mut st = self.ns.state.lock().unwrap();
        if !st.registered.contains(&key) {
            return Err(reservation_conflict_error());
        }
        st.registered.retain(|k| *k != victim_key);
        st.holder = Some(key);
        self.ns.preempts.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn release(&self, key: u64) -> io::Result<()> {
        let mut st = self.ns.state.lock().unwrap();
        if st.holder == Some(key) {
            st.holder = None;
        }
        st.registered.retain(|k| *k != key);
        Ok(())
    }

    fn report(&self) -> io::Result<ReservationReport> {
        let st = self.ns.state.lock().unwrap();
        Ok(ReservationReport {
            holder_key: st.holder,
            registered_keys: st.registered.clone(),
        })
    }
}
