//! The node cache's **revalidation epoch** and **append gate** — the two
//! lock-free words that make one node cache safe to share with a second
//! process, in the only two shapes the pre-RC engineering spec sanctions:
//!
//! * §6.8 item 2 — a **coherent reader** that polls the A/B root ledger at
//!   a bounded cadence and drops every cached node not covered by the new
//!   roots ([`RevalidationEpoch`]);
//! * §6.2's closing verdict — *"the tractable answer is to ensure two
//!   writers never cache the same node: **partitioning, not cache
//!   coherence**"* ([`AppendGate`]).
//!
//! Dependency-free so `loom-models/` can `#[path]`-include it and check the
//! interleavings exhaustively against the invariants below. The main build
//! never sets `cfg(loom)`.
//!
//! ## The two-word publication law (the reason this is a core)
//!
//! A revalidation publishes **two** facts: the new checkpoint epoch and the
//! new durable journal tail. A loader must read them as a coherent pair,
//! because the tail is the §4.5 torn-tail classifier's input:
//!
//! * a node stamped "current as of epoch E" whose tail came from an
//!   **older** epoch may have silently dropped a tail bset that E's
//!   checkpoint covers — the node then claims currency it does not have,
//!   which is exactly the silent-divergence class this item exists to
//!   prevent;
//! * the reverse pairing (an older epoch stamp with a newer tail) is
//!   merely **strict**: the classifier can only be louder than necessary,
//!   and the node is dropped at the next poll anyway.
//!
//! So the orders are pinned in opposite directions and each side's is
//! load-bearing:
//!
//! | Side | Order | Ordering |
//! |---|---|---|
//! | publisher ([`RevalidationEpoch::publish`]) | **tail, then epoch** | both `Release` |
//! | loader ([`RevalidationEpoch::load_snapshot`]) | **epoch, then tail** | both `Acquire` |
//!
//! Together: any epoch a loader observes was published *before* the tail it
//! then reads, so `snapshot.tail ≥ tail(snapshot.epoch)` — never the other
//! way round. Reversing either order, or weakening either access, breaks
//! the loom model (`epoch_core_*` in `loom-models/src/lib.rs`).
//!
//! ## The hit path
//!
//! [`RevalidationEpoch::probe`] is what every node-cache hit pays: one
//! **relaxed** load, compared against the immutable epoch stamp the node
//! was published under. Relaxed is sufficient *because a mismatch is a
//! miss*: rejection routes to the load path, which re-reads the pair with
//! `Acquire`. An un-armed cache (every write mount today) holds
//! [`UNARMED_EPOCH`] and stamps every node with it, so the compare is
//! always equal and the branch is never taken.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
}

use atomic::{AtomicU32, AtomicU64, Ordering};

/// The epoch of a cache nobody armed: the shipped single-writer posture.
/// Every node is stamped with it, so the hit-path compare always agrees
/// and no revalidation machinery can ever drop a mapping the local mount
/// owns.
pub const UNARMED_EPOCH: u64 = 0;

/// Appender id of the volume's **root authority** — the one appender that
/// owns structural state (tree roots, interior nodes, SMOs). Must equal
/// `super::journal::ROOT_AUTHORITY_WRITER`; the tie is a test, because this
/// module is kept dependency-free for loom.
pub const ROOT_AUTHORITY: u16 = 0;

/// A coherent read of the revalidation pair (see the module's two-word
/// law): the epoch a node loaded now may claim, and the durable tail its
/// torn-tail classification must use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadSnapshot {
    pub epoch: u64,
    pub tail: u64,
}

/// The revalidation epoch + durable journal tail of one node cache.
#[derive(Debug)]
pub struct RevalidationEpoch {
    epoch: AtomicU64,
    tail: AtomicU64,
}

impl RevalidationEpoch {
    /// Un-armed, seeded with the mount's durable tail.
    pub fn new(tail: u64) -> Self {
        Self {
            epoch: AtomicU64::new(UNARMED_EPOCH),
            tail: AtomicU64::new(tail),
        }
    }

    /// **The hit-path probe**: one relaxed load (see the module docs on why
    /// relaxed is sufficient — a mismatch is a miss, and the miss path
    /// re-reads with `Acquire`).
    #[inline]
    pub fn probe(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// Whether a reader has armed revalidation on this cache.
    #[inline]
    pub fn is_armed(&self) -> bool {
        self.probe() != UNARMED_EPOCH
    }

    /// The durable journal tail (§4.5 torn-tail classifier input).
    pub fn tail(&self) -> u64 {
        self.tail.load(Ordering::Acquire)
    }

    /// Advance the tail alone — the write mount's `set_durable_tail`, and
    /// the reader's seed at arm time. Monotone.
    pub fn advance_tail(&self, tail: u64) {
        self.tail.fetch_max(tail, Ordering::AcqRel);
    }

    /// Arm at `epoch` (the mounted ledger record's seq). Returns `false` if
    /// already armed or if `epoch` is [`UNARMED_EPOCH`] — arming is a
    /// once-per-mount declaration, not a mutable mode.
    pub fn arm(&self, epoch: u64) -> bool {
        if epoch == UNARMED_EPOCH {
            return false;
        }
        self.epoch
            .compare_exchange(UNARMED_EPOCH, epoch, Ordering::Release, Ordering::Relaxed)
            .is_ok()
    }

    /// Publish a newer checkpoint: **tail first, then the epoch** (the
    /// two-word law). Returns `Some((previous, new))` when the epoch
    /// actually moved, `None` when the record is not newer (an inert poll)
    /// or the cache is un-armed (a write mount can never be dropped into
    /// revalidation by a stray call).
    pub fn publish(&self, tail: u64, epoch: u64) -> Option<(u64, u64)> {
        let mut cur = self.epoch.load(Ordering::Relaxed);
        if cur == UNARMED_EPOCH || epoch <= cur {
            return None;
        }
        // The tail must be visible BEFORE the epoch that vouches for it.
        self.tail.fetch_max(tail, Ordering::AcqRel);
        loop {
            match self
                .epoch
                .compare_exchange_weak(cur, epoch, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return Some((cur, epoch)),
                Err(observed) => {
                    if observed >= epoch {
                        // A concurrent poller published this epoch or newer:
                        // its sweep covers ours.
                        return None;
                    }
                    cur = observed;
                }
            }
        }
    }

    /// **The load-path snapshot**: epoch first, then the tail — both
    /// `Acquire`, so the pair is never inverted (module docs).
    pub fn load_snapshot(&self) -> LoadSnapshot {
        let epoch = self.epoch.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        LoadSnapshot { epoch, tail }
    }
}

/// The decoded [`AppendGate`] word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateState {
    /// This mount declared itself a coherent READER: it may not mutate any
    /// node of this volume.
    pub reader: bool,
    /// Appenders the volume's structures are partitioned for (`0`/`1` =
    /// solo — today's every mount).
    pub writers: u16,
    /// This mount's appender id.
    pub writer_id: u16,
}

impl GateState {
    /// Whether this appender owns the volume's structural state — interior
    /// nodes, tree roots, SMOs (lock order 4b).
    #[inline]
    pub fn is_authority(&self) -> bool {
        self.writer_id == ROOT_AUTHORITY
    }

    /// A volume with exactly one appender: the shipped posture, where every
    /// partitioning check is structurally inert.
    #[inline]
    pub fn is_solo(&self) -> bool {
        self.writers <= 1
    }
}

/// One packed word carrying both mutation gates, so the ONE RAM-mutation
/// choke point ([`super::node_cache::CachedNode::apply_locked`]) pays a
/// single relaxed load instead of two: bit 31 = reader, bits 16..31 =
/// appender count, bits 0..16 = appender id.
#[derive(Debug)]
pub struct AppendGate {
    word: AtomicU32,
}

const READER_BIT: u32 = 1 << 31;
const WRITERS_SHIFT: u32 = 16;
const WRITERS_MASK: u32 = 0x7fff;

impl AppendGate {
    /// Solo authority, not a reader: the shipped posture (word 0).
    pub fn new() -> Self {
        Self {
            word: AtomicU32::new(0),
        }
    }

    /// One relaxed load — the whole cost of both gates.
    #[inline]
    pub fn load(&self) -> GateState {
        let w = self.word.load(Ordering::Relaxed);
        GateState {
            reader: w & READER_BIT != 0,
            writers: ((w >> WRITERS_SHIFT) & WRITERS_MASK) as u16,
            writer_id: (w & 0xffff) as u16,
        }
    }

    /// Declare this mount a coherent reader (idempotent).
    pub fn set_reader(&self) {
        self.word.fetch_or(READER_BIT, Ordering::AcqRel);
    }

    /// Declare this mount appender `writer_id` of `writers`. `Err` on an
    /// out-of-range partition (the appender count must fit the 15 bits the
    /// word reserves, and an id must be inside its own count) — structural
    /// nonsense must never reach the partitioning checks.
    pub fn set_appender(&self, writers: u16, writer_id: u16) -> Result<(), ()> {
        if writers == 0 || u32::from(writers) > WRITERS_MASK || writer_id >= writers {
            return Err(());
        }
        let bits = (u32::from(writers) << WRITERS_SHIFT) | u32::from(writer_id);
        loop {
            let cur = self.word.load(Ordering::Relaxed);
            let next = (cur & READER_BIT) | bits;
            match self
                .word
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return Ok(()),
                Err(_) => continue,
            }
        }
    }
}

impl Default for AppendGate {
    fn default() -> Self {
        Self::new()
    }
}

/// The per-cache environment every [`super::node_cache::CachedNode`] shares:
/// the revalidation pair and the mutation gates. One `Arc` per cache, handed
/// to every node at construction — the node needs it to answer *"may I be
/// mutated?"* at the choke point, and the cache needs it to answer *"is this
/// node still current?"* on every hit.
#[derive(Debug)]
pub struct NodeEnv {
    pub epoch: RevalidationEpoch,
    pub gate: AppendGate,
    /// The structural-hold ledger the flush-ceiling audit reads
    /// (symmetric PR 13c, F-B1): the monotone Σ of the time the volume's
    /// SMO mutex was held by an actor OTHER than the flush pass, per
    /// class. A leaf stamps the Σ at its clean → dirty transition; the
    /// audit's `Σ(now) − Σ(stamp)` is EXACTLY the hold time overlapping
    /// the leaf's dirty window (holds are serialized by the mutex, so the
    /// Σ never double counts).
    pub holds: StructuralHolds,
}

impl NodeEnv {
    pub fn new(durable_tail: u64) -> Self {
        Self {
            epoch: RevalidationEpoch::new(durable_tail),
            gate: AppendGate::new(),
            holds: StructuralHolds::new(),
        }
    }
}

/// The class of a structural hold of the SMO mutex — the two actors
/// whose hold time the flush-ceiling audit excludes from a leaf's age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldClass {
    /// A dead appender's recovery (PR 10 §5.9 steps 4–7; its excusable
    /// overlap is capped at the published `appender_recovery_bound_ms` —
    /// defect 33's law).
    Recovery = 0,
    /// The manager's or a joiner's SERVICE of the fleet under the mutex:
    /// a wire appender's slot grant / release, a transfer's adoption, a
    /// projection refresh, a region's release, the joiner's own wire
    /// refill inside its flush pass (its excusable overlap is the
    /// measured hold — the service is priced on `manager_service_ns`).
    Service = 1,
}

/// Per class: the Σ of completed hold time, the active hold's start
/// (0 = none) and its nesting depth (a class's guards may nest — the
/// recovery poll around one region's steps; the outermost brackets the
/// hold). Both classes hold the ONE mutex, so the Σs are exact.
#[derive(Debug)]
pub struct StructuralHolds {
    held_ns: [atomic::AtomicU64; 2],
    active_since_ns: [atomic::AtomicU64; 2],
    depth: [atomic::AtomicU32; 2],
}

impl Default for StructuralHolds {
    fn default() -> Self {
        Self::new()
    }
}

impl StructuralHolds {
    pub fn new() -> Self {
        Self {
            held_ns: [atomic::AtomicU64::new(0), atomic::AtomicU64::new(0)],
            active_since_ns: [atomic::AtomicU64::new(0), atomic::AtomicU64::new(0)],
            depth: [atomic::AtomicU32::new(0), atomic::AtomicU32::new(0)],
        }
    }

    /// A hold of `class` began at `now_ns` (CLOCK_MONOTONIC); a nested
    /// guard of an already-active hold only deepens it.
    pub fn begin(&self, class: HoldClass, now_ns: u64) {
        let i = class as usize;
        if self.depth[i].fetch_add(1, atomic::Ordering::AcqRel) == 0 {
            self.active_since_ns[i].store(now_ns.max(1), atomic::Ordering::Release);
        }
    }

    /// A guard of `class` dropped at `now_ns`: the outermost one's
    /// duration joins the Σ.
    pub fn end(&self, class: HoldClass, now_ns: u64) {
        let i = class as usize;
        if self.depth[i].fetch_sub(1, atomic::Ordering::AcqRel) == 1 {
            let since = self.active_since_ns[i].swap(0, atomic::Ordering::AcqRel);
            if since != 0 {
                self.held_ns[i].fetch_add(now_ns.saturating_sub(since), atomic::Ordering::AcqRel);
            }
        }
    }

    /// The Σ of `class` hold time up to `now_ns`, the active hold's
    /// elapsed part included — the stamp a leaf takes at its dirty
    /// transition and the value the audit compares it against.
    pub fn held_ns(&self, class: HoldClass, now_ns: u64) -> u64 {
        let done = self.held_ns[class as usize].load(atomic::Ordering::Acquire);
        let since = self.active_since_ns[class as usize].load(atomic::Ordering::Acquire);
        if since != 0 {
            done.saturating_add(now_ns.saturating_sub(since))
        } else {
            done
        }
    }

    /// Both classes' Σ at `now_ns` (`[recovery, service]`).
    pub fn snapshot(&self, now_ns: u64) -> [u64; 2] {
        [
            self.held_ns(HoldClass::Recovery, now_ns),
            self.held_ns(HoldClass::Service, now_ns),
        ]
    }
}
