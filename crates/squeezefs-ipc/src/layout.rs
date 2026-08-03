//! Shared-memory **session layout** — the one sealed memfd both sides map
//! (design-preload-interception §5.3):
//!
//! ```text
//! [ header page ]      magic, IPC_ABI, session generation, daemon heartbeat
//!                      word, doorbell + daemon-parked flag (WakeCoalescer),
//!                      geometry
//! [ submission ring ]  MPSC ring: one tail line + ring_entries cells
//! [ op slot array ]    slots × 128 B cache-aligned descriptors
//! [ client stats page ] shim-side preload_* counters (display-only,
//!                      client-writable — never a daemon input)
//! [ payload arena ]    4 KiB-aligned slabs; every slot references arena
//!                      offsets only (no pointers cross the boundary)
//! ```
//!
//! This module defines the geometry, the region arithmetic, and the
//! `#[repr(C)]` shared types. It performs **no** I/O and owns **no**
//! mappings: creating/sealing/mapping the memfd is the daemon host's job
//! (PR L4-3), casting mapped bytes into these types is the one unsafe
//! boundary there.
//!
//! **ABI discipline (design KD-7)**: the layout is version-locked to the
//! build commit — both sides of a session are the same build, checked at
//! bind. [`IPC_ABI`] exists as the *coarse* structural guard (bumped on any
//! layout change) so a skewed pair refuses before touching the mapping.
//! This is explicitly not a stable ABI.
//!
//! **Trust boundary (§5.3.1 rule 1)**: everything in the mapping is
//! client-writable for the life of the session. The daemon treats header
//! fields as write-once *outputs* it never re-reads after creation (its
//! geometry authority is private), and snapshots descriptors exactly once
//! ([`IpcSlot::snapshot_descriptor`]).

use crate::cqe_core::CqeDoorbell;
use crate::slot_core::SlotCore;
use crate::wake_core::WakeCoalescer;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Coarse structural ABI guard; bump on ANY change to this module's types
/// or the region arithmetic. Fine-grained skew is caught by the
/// build-commit equality check at bind (design §5.2 screen rule 4).
/// v2: the completion doorbell ([`crate::cqe_core::CqeDoorbell`]) took
/// the header's reserved line 2 (op-economy campaign, 2026-07-28).
/// v3: `AdminHello` gained the ABI / build-commit / nonce fields so the
/// control lane runs the same screening ladder as the data plane (VAL-7c,
/// pre-RC spec §3) — a ctl-wire layout change, hence the coarse bump.
pub const IPC_ABI: u32 = 3;

/// Session mapping magic: `SQZIPC01` little-endian.
pub const IPC_MAGIC: u64 = u64::from_le_bytes(*b"SQZIPC01");

/// Region granule: every region starts page-aligned.
pub const PAGE_BYTES: u64 = 4096;

/// One op slot is exactly one 128-byte cache-line pair (§5.3).
pub const SLOT_BYTES: u64 = 128;

/// The submission ring's tail word gets a full cacheline-pair to itself in
/// front of the cell array (producer-contended CAS word — never shares a
/// line with the cells).
pub const RING_TAIL_LINE_BYTES: u64 = 128;

/// Bytes per ring cell ([`crate::ring_core::RingCell`]: seq + value).
pub const RING_CELL_BYTES: u64 = 8;

/// Defaults (§5.3): ring/slot count 1024, arena 64 MiB, max op 1 MiB.
pub const DEFAULT_RING_ENTRIES: u32 = 1024;
pub const DEFAULT_ARENA_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_OP_BYTES: u32 = 1024 * 1024;

/// Sanity ceiling on the arena (per-session; real admission is the daemon
/// budget component, design §5.7 — this only bounds the arithmetic).
pub const MAX_ARENA_BYTES: u64 = 1 << 40;

/// v1 op codes (§5.3 op slot: READ | WRITE). ECHO is the L4-3 session-host
/// liveness/round-trip op (payload in, byte-sum result + inverted payload
/// out) — the one op served before the PR L4-4 data plane, kept after it
/// as the ping op.
pub const OP_READ: u32 = 1;
pub const OP_WRITE: u32 = 2;
pub const OP_ECHO: u32 = 3;

/// Session geometry, fixed at session creation and embedded in the header.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Submission ring capacity (cells). Power of two.
    pub ring_entries: u32,
    /// Op slot count. `slots ≤ ring_entries`, so the honest protocol (at
    /// most one ring entry per claimed slot) can never observe a full ring
    /// refusal caused by geometry.
    pub slots: u32,
    /// Payload arena bytes (4 KiB multiple).
    pub arena_bytes: u64,
    /// Per-op payload ceiling (`len ≤ max_op_bytes`, validated daemon-side
    /// per §5.3 rule 4).
    pub max_op_bytes: u32,
    pub _pad: u32,
}

impl Geometry {
    /// The v1 defaults.
    pub fn default_v1() -> Self {
        Self {
            ring_entries: DEFAULT_RING_ENTRIES,
            slots: DEFAULT_RING_ENTRIES,
            arena_bytes: DEFAULT_ARENA_BYTES,
            max_op_bytes: DEFAULT_MAX_OP_BYTES,
            _pad: 0,
        }
    }

    /// Validate every geometry rule (each refusal is a distinct variant —
    /// the daemon's bind refusal ledger wants attribution).
    pub fn validate(&self) -> Result<(), GeometryError> {
        if self.ring_entries < crate::ring_core::MIN_RING_ENTRIES {
            return Err(GeometryError::RingEntriesTooSmall);
        }
        if !self.ring_entries.is_power_of_two() {
            return Err(GeometryError::RingEntriesNotPowerOfTwo);
        }
        if self.ring_entries > crate::ring_core::MAX_RING_ENTRIES {
            return Err(GeometryError::RingEntriesTooLarge);
        }
        if self.slots == 0 {
            return Err(GeometryError::SlotsZero);
        }
        if self.slots > self.ring_entries {
            return Err(GeometryError::SlotsExceedRingEntries);
        }
        if self.arena_bytes == 0 {
            return Err(GeometryError::ArenaZero);
        }
        if !self.arena_bytes.is_multiple_of(PAGE_BYTES) {
            return Err(GeometryError::ArenaNotPageMultiple);
        }
        if self.arena_bytes > MAX_ARENA_BYTES {
            return Err(GeometryError::ArenaTooLarge);
        }
        if self.max_op_bytes == 0 {
            return Err(GeometryError::MaxOpZero);
        }
        if u64::from(self.max_op_bytes) > self.arena_bytes {
            return Err(GeometryError::MaxOpExceedsArena);
        }
        Ok(())
    }
}

/// Geometry / layout refusals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryError {
    RingEntriesNotPowerOfTwo,
    RingEntriesTooSmall,
    RingEntriesTooLarge,
    SlotsZero,
    SlotsExceedRingEntries,
    ArenaZero,
    ArenaNotPageMultiple,
    ArenaTooLarge,
    MaxOpZero,
    MaxOpExceedsArena,
    Overflow,
}

impl core::fmt::Display for GeometryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::RingEntriesNotPowerOfTwo => "ring_entries must be a power of two",
            Self::RingEntriesTooSmall => "ring_entries below MIN_RING_ENTRIES",
            Self::RingEntriesTooLarge => "ring_entries exceeds MAX_RING_ENTRIES",
            Self::SlotsZero => "slots must be nonzero",
            Self::SlotsExceedRingEntries => "slots must not exceed ring_entries",
            Self::ArenaZero => "arena_bytes must be nonzero",
            Self::ArenaNotPageMultiple => "arena_bytes must be a 4 KiB multiple",
            Self::ArenaTooLarge => "arena_bytes exceeds MAX_ARENA_BYTES",
            Self::MaxOpZero => "max_op_bytes must be nonzero",
            Self::MaxOpExceedsArena => "max_op_bytes must not exceed arena_bytes",
            Self::Overflow => "layout arithmetic overflow",
        };
        f.write_str(s)
    }
}

impl std::error::Error for GeometryError {}

/// Computed byte offsets of every region inside the session mapping. All
/// offsets are page-aligned, strictly ordered, and non-overlapping; the
/// arena is last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLayout {
    pub header_off: u64,
    pub header_bytes: u64,
    /// Ring region: tail line + cell array.
    pub ring_off: u64,
    pub ring_bytes: u64,
    /// Cell array start (inside the ring region, after the tail line).
    pub ring_cells_off: u64,
    pub slots_off: u64,
    pub slots_bytes: u64,
    pub stats_off: u64,
    pub stats_bytes: u64,
    pub arena_off: u64,
    pub arena_bytes: u64,
    /// Total mapping length (== memfd size).
    pub total_bytes: u64,
}

impl SessionLayout {
    /// Compute the layout for a geometry (validates it first).
    pub fn compute(geometry: &Geometry) -> Result<Self, GeometryError> {
        geometry.validate()?;

        fn page_round(bytes: u64) -> Result<u64, GeometryError> {
            bytes
                .checked_add(PAGE_BYTES - 1)
                .map(|v| v / PAGE_BYTES * PAGE_BYTES)
                .ok_or(GeometryError::Overflow)
        }
        fn add(a: u64, b: u64) -> Result<u64, GeometryError> {
            a.checked_add(b).ok_or(GeometryError::Overflow)
        }

        let header_off = 0;
        let header_bytes = PAGE_BYTES;
        let ring_off = add(header_off, header_bytes)?;
        let ring_bytes = add(
            RING_TAIL_LINE_BYTES,
            u64::from(geometry.ring_entries) * RING_CELL_BYTES,
        )?;
        let ring_cells_off = add(ring_off, RING_TAIL_LINE_BYTES)?;
        let slots_off = page_round(add(ring_off, ring_bytes)?)?;
        let slots_bytes = u64::from(geometry.slots) * SLOT_BYTES;
        let stats_off = page_round(add(slots_off, slots_bytes)?)?;
        let stats_bytes = PAGE_BYTES;
        let arena_off = page_round(add(stats_off, stats_bytes)?)?;
        let arena_bytes = geometry.arena_bytes;
        let total_bytes = add(arena_off, arena_bytes)?;

        Ok(Self {
            header_off,
            header_bytes,
            ring_off,
            ring_bytes,
            ring_cells_off,
            slots_off,
            slots_bytes,
            stats_off,
            stats_bytes,
            arena_off,
            arena_bytes,
            total_bytes,
        })
    }
}

/// The header page (region 0). Write-once identity + the session's shared
/// control words. See the module docs for the trust boundary.
#[repr(C, align(4096))]
pub struct SessionHeader {
    // -- line 0: write-once identity (daemon-written at creation) --------
    pub magic: u64,
    pub abi: u32,
    pub _pad0: u32,
    /// Session generation: bumped by the daemon to poison the session
    /// (client observes and goes passthrough — design §5.7).
    pub generation: AtomicU64,
    /// Daemon heartbeat word (client-side death detection, §5.7).
    pub heartbeat: AtomicU64,
    pub _pad1: [u8; 32],
    // -- line 1: wake words (client-written, daemon-parked) --------------
    /// Futex doorbell: clients bump + `FUTEX_WAKE` when [`Self::
    /// doorbell_coalescer`] arms AND [`Self::daemon_parked`] is set (§5.3
    /// protocol rule 1).
    pub doorbell: AtomicU32,
    /// Daemon-parked flag: nonzero while the service thread is inside (or
    /// committing to) `FUTEX_WAIT` on the doorbell.
    pub daemon_parked: AtomicU32,
    /// The L3 wake-elision protocol core, shared source-identical from
    /// fuse3 (`crate::wake_core` — §5.3.2 sharing direction).
    pub doorbell_coalescer: WakeCoalescer,
    pub _pad2: [u8; 55],
    // -- line 2: completion wake words (daemon-written, reaper-parked) ---
    /// The completion doorbell (op-economy campaign, 2026-07-28): the
    /// per-session completion seq + parked-reaper count the daemon's
    /// wake elision and the libaio reaper's event parks ride. Its own
    /// cache line on purpose — completion-side bumps never contend the
    /// submit wake words above, and the producer-contended ring tail
    /// still lives in the ring region ([`SessionLayout::ring_off`]).
    pub cqe: CqeDoorbell,
    pub _pad3: [u8; 56],
    // -- geometry copy (write-once, for the CLIENT's map-time read) ------
    pub geometry: Geometry,
}

impl SessionHeader {
    /// A fresh header for `geometry` (daemon side, at session creation).
    pub fn new(geometry: Geometry) -> Self {
        Self {
            magic: IPC_MAGIC,
            abi: IPC_ABI,
            _pad0: 0,
            generation: AtomicU64::new(1),
            heartbeat: AtomicU64::new(0),
            _pad1: [0; 32],
            doorbell: AtomicU32::new(0),
            daemon_parked: AtomicU32::new(0),
            doorbell_coalescer: WakeCoalescer::new(),
            _pad2: [0; 55],
            cqe: CqeDoorbell::new(),
            _pad3: [0; 56],
            geometry,
        }
    }

    /// Client-side map-time check: magic + ABI + geometry validity. (The
    /// daemon never calls this on a live mapping — §5.3.1 rule 1.)
    pub fn validate(&self) -> Result<(), HeaderError> {
        if self.magic != IPC_MAGIC {
            return Err(HeaderError::BadMagic);
        }
        if self.abi != IPC_ABI {
            return Err(HeaderError::AbiMismatch {
                theirs: self.abi,
                ours: IPC_ABI,
            });
        }
        self.geometry.validate().map_err(HeaderError::Geometry)
    }
}

/// Header refusals (client-side map-time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    BadMagic,
    AbiMismatch { theirs: u32, ours: u32 },
    Geometry(GeometryError),
}

impl core::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadMagic => write!(f, "session header magic mismatch"),
            Self::AbiMismatch { theirs, ours } => {
                write!(f, "IPC_ABI mismatch: theirs {theirs}, ours {ours}")
            }
            Self::Geometry(e) => write!(f, "session header geometry invalid: {e}"),
        }
    }
}

impl std::error::Error for HeaderError {}

/// A descriptor snapshot — the daemon's private copy of the client-written
/// op fields, read **once** after `try_begin_serve` succeeds (§5.3.1 rule
/// 1: validate the copy, serve from the copy, never re-read the slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotDescriptor {
    pub op: u32,
    pub flags: u32,
    pub binding: u64,
    pub offset: u64,
    pub len: u32,
    pub arena_off: u64,
}

/// One op slot: the SQE and CQE in one, completion-in-place (§5.3). The
/// protocol words are the [`SlotCore`]; the descriptor fields are atomics
/// so a hostile client's mid-serve mutation is bounded-behavior, never a
/// data race (§5.3.1) — all descriptor access is Relaxed, ordered by the
/// core's state-word edges.
#[repr(C, align(128))]
pub struct IpcSlot {
    pub core: SlotCore,
    op: AtomicU32,
    flags: AtomicU32,
    binding: AtomicU64,
    offset: AtomicU64,
    len: AtomicU32,
    _reserved: AtomicU32,
    arena_off: AtomicU64,
    result: AtomicI64,
}

impl Default for IpcSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl IpcSlot {
    /// A fresh FREE slot.
    pub fn new() -> Self {
        Self {
            core: SlotCore::new(),
            op: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            binding: AtomicU64::new(0),
            offset: AtomicU64::new(0),
            len: AtomicU32::new(0),
            _reserved: AtomicU32::new(0),
            arena_off: AtomicU64::new(0),
            result: AtomicI64::new(0),
        }
    }

    /// Client: write the descriptor (between `try_claim` and
    /// `publish_submitted` — the submit Release publishes these).
    pub fn publish_descriptor(&self, d: &SlotDescriptor) {
        self.op.store(d.op, Ordering::Relaxed);
        self.flags.store(d.flags, Ordering::Relaxed);
        self.binding.store(d.binding, Ordering::Relaxed);
        self.offset.store(d.offset, Ordering::Relaxed);
        self.len.store(d.len, Ordering::Relaxed);
        self.arena_off.store(d.arena_off, Ordering::Relaxed);
    }

    /// Daemon: the ONE linearization read of the descriptor (§5.3.1 rule
    /// 1). Call exactly once, after `core.try_begin_serve()` returned
    /// `true`; validate the returned copy and serve exclusively from it.
    pub fn snapshot_descriptor(&self) -> SlotDescriptor {
        SlotDescriptor {
            op: self.op.load(Ordering::Relaxed),
            flags: self.flags.load(Ordering::Relaxed),
            binding: self.binding.load(Ordering::Relaxed),
            offset: self.offset.load(Ordering::Relaxed),
            len: self.len.load(Ordering::Relaxed),
            arena_off: self.arena_off.load(Ordering::Relaxed),
        }
    }

    /// Daemon: write the completion result (bytes or `-errno`) — before
    /// `core.complete()`, whose Release half publishes it.
    pub fn set_result(&self, result: i64) {
        self.result.store(result, Ordering::Relaxed);
    }

    /// Client: read the result — after `core.is_done_for(gen)` returned
    /// `true` (its Acquire load orders this).
    pub fn result(&self) -> i64 {
        self.result.load(Ordering::Relaxed)
    }
}

// ---- layout pins (production shm shapes; loom never builds this file) ----
const _: () = {
    assert!(std::mem::size_of::<IpcSlot>() == SLOT_BYTES as usize);
    assert!(std::mem::align_of::<IpcSlot>() == SLOT_BYTES as usize);
    assert!(std::mem::size_of::<SessionHeader>() == PAGE_BYTES as usize);
    assert!(std::mem::align_of::<SessionHeader>() == PAGE_BYTES as usize);
    // The shared WakeCoalescer is one AtomicBool — the header arithmetic
    // (_pad2) assumes exactly one byte.
    assert!(std::mem::size_of::<WakeCoalescer>() == 1);
    assert!(std::mem::size_of::<crate::ring_core::RingCell>() == RING_CELL_BYTES as usize);
    // Wake words start at line 1, ring-tail pad at line 2 (cacheline
    // separation of identity / wake / producer-contended words).
    assert!(std::mem::offset_of!(SessionHeader, doorbell) == 64);
    assert!(std::mem::offset_of!(SessionHeader, cqe) == 128);
    assert!(std::mem::size_of::<CqeDoorbell>() == 8);
    assert!(std::mem::offset_of!(SessionHeader, geometry) == 192);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_geometry_validates_and_computes() {
        let g = Geometry::default_v1();
        g.validate().expect("v1 defaults must be valid");
        let l = SessionLayout::compute(&g).expect("v1 defaults must compute");

        // Region map: header, ring, slots, stats, arena — in order,
        // page-aligned, non-overlapping, arena last.
        assert_eq!(l.header_off, 0);
        assert_eq!(l.header_bytes, PAGE_BYTES);
        for off in [l.ring_off, l.slots_off, l.stats_off, l.arena_off] {
            assert_eq!(off % PAGE_BYTES, 0, "region offsets are page-aligned");
        }
        assert_eq!(l.ring_off, PAGE_BYTES);
        assert_eq!(l.ring_cells_off, l.ring_off + RING_TAIL_LINE_BYTES);
        assert!(l.ring_off + l.ring_bytes <= l.slots_off);
        assert!(l.slots_off + l.slots_bytes <= l.stats_off);
        assert!(l.stats_off + l.stats_bytes <= l.arena_off);
        assert_eq!(l.arena_off + l.arena_bytes, l.total_bytes);
        assert_eq!(l.arena_bytes, g.arena_bytes);
        assert_eq!(l.slots_bytes, u64::from(g.slots) * SLOT_BYTES);
        assert!(l.ring_bytes >= RING_TAIL_LINE_BYTES + u64::from(g.ring_entries) * RING_CELL_BYTES);
        assert_eq!(l.stats_bytes, PAGE_BYTES);
    }

    #[test]
    fn geometry_refusals_attributed_exactly() {
        let ok = Geometry::default_v1();
        let cases: Vec<(Geometry, GeometryError)> = vec![
            (
                Geometry {
                    ring_entries: 0,
                    ..ok
                },
                GeometryError::RingEntriesTooSmall,
            ),
            (
                Geometry {
                    ring_entries: 1,
                    slots: 1,
                    ..ok
                },
                GeometryError::RingEntriesTooSmall,
            ),
            (
                Geometry {
                    ring_entries: 3,
                    slots: 2,
                    ..ok
                },
                GeometryError::RingEntriesNotPowerOfTwo,
            ),
            (
                Geometry {
                    ring_entries: crate::ring_core::MAX_RING_ENTRIES * 2,
                    ..ok
                },
                GeometryError::RingEntriesTooLarge,
            ),
            (Geometry { slots: 0, ..ok }, GeometryError::SlotsZero),
            (
                Geometry {
                    ring_entries: 4,
                    slots: 8,
                    ..ok
                },
                GeometryError::SlotsExceedRingEntries,
            ),
            (
                Geometry {
                    arena_bytes: 0,
                    ..ok
                },
                GeometryError::ArenaZero,
            ),
            (
                Geometry {
                    arena_bytes: 4097,
                    ..ok
                },
                GeometryError::ArenaNotPageMultiple,
            ),
            (
                Geometry {
                    arena_bytes: MAX_ARENA_BYTES + PAGE_BYTES,
                    ..ok
                },
                GeometryError::ArenaTooLarge,
            ),
            (
                Geometry {
                    max_op_bytes: 0,
                    ..ok
                },
                GeometryError::MaxOpZero,
            ),
            (
                Geometry {
                    arena_bytes: PAGE_BYTES,
                    max_op_bytes: 8192,
                    ..ok
                },
                GeometryError::MaxOpExceedsArena,
            ),
        ];
        for (g, expect) in cases {
            assert_eq!(g.validate().unwrap_err(), expect, "geometry {g:?}");
            assert_eq!(
                SessionLayout::compute(&g).unwrap_err(),
                expect,
                "compute must refuse exactly like validate for {g:?}"
            );
        }
    }

    #[test]
    fn header_roundtrip_and_refusals() {
        let h = SessionHeader::new(Geometry::default_v1());
        h.validate().expect("fresh header must validate");

        let mut bad_magic = SessionHeader::new(Geometry::default_v1());
        bad_magic.magic = 0xdead;
        assert_eq!(bad_magic.validate().unwrap_err(), HeaderError::BadMagic);

        let mut bad_abi = SessionHeader::new(Geometry::default_v1());
        bad_abi.abi = IPC_ABI + 1;
        assert_eq!(
            bad_abi.validate().unwrap_err(),
            HeaderError::AbiMismatch {
                theirs: IPC_ABI + 1,
                ours: IPC_ABI
            }
        );

        let mut bad_geometry = SessionHeader::new(Geometry {
            ring_entries: 3,
            ..Geometry::default_v1()
        });
        bad_geometry.magic = IPC_MAGIC;
        assert_eq!(
            bad_geometry.validate().unwrap_err(),
            HeaderError::Geometry(GeometryError::RingEntriesNotPowerOfTwo)
        );
    }

    #[test]
    fn slot_descriptor_publish_snapshot_roundtrip() {
        let slot = IpcSlot::new();
        let d = SlotDescriptor {
            op: OP_WRITE,
            flags: 7,
            binding: 0xfeed_beef_dead_cafe,
            offset: 4096 * 3,
            len: 4096,
            arena_off: 8192,
        };
        slot.publish_descriptor(&d);
        assert_eq!(slot.snapshot_descriptor(), d);
        slot.set_result(-5);
        assert_eq!(slot.result(), -5);
    }
}
