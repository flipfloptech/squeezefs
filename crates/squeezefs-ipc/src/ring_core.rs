//! IPC **submission ring** protocol core — bounded MPSC ring of `u32` op-slot
//! indices (design-preload-interception §5.3, protocol rule 1).
//!
//! Producers are app threads inside the (untrusted) client process; the one
//! consumer is the daemon service thread that owns the session's drain. The
//! ring is the classic bounded MPSC: producers reserve a cell with a tail
//! CAS and publish with a per-cell sequence store; the consumer observes
//! publication through the cell sequence and frees the cell for the next
//! lap. Per-cell sequences are monotonic (they advance by `capacity` each
//! lap), which is what makes reuse ABA-safe.
//!
//! ## Single-consumer PRECONDITION (normative — the model cannot see its
//! violation)
//!
//! Exactly **one** consumer may drain a ring, ever. This is guaranteed
//! *structurally*, not by anything inside the ring: a session is pinned to
//! exactly one daemon service thread at admission, for its whole lifetime
//! (design §5.5.1 session-ownership invariant), and the consumer cursor
//! ([`RingConsumer`]) lives in the consumer's **private** memory — it is
//! deliberately not part of the shared-memory layout, so a hostile client
//! cannot fake or fork consumption. `RingConsumer` is intentionally
//! `!Clone`. The `ipc_ring_core` loom models state this precondition and
//! model exactly one consumer (design §5.3.2).
//!
//! ## Untrusted shared memory
//!
//! The tail word and the cells live in client-writable memory. A hostile
//! client can corrupt them; the guarantee this core provides the daemon is
//! **memory safety and bounded behavior, never semantic validity**: all
//! indexing is masked, corrupted sequences degrade to `full`/`empty`
//! observations (never out-of-bounds, never UB), and whatever indices a
//! corrupted ring yields are validated downstream against the daemon-owned
//! binding/slot tables (§5.3 protocol rule 4, §5.3.1).
//!
//! Dependency-free on purpose: `loom-models/src/lib.rs` `#[path]`-includes
//! this file (the `wake_core` house convention), so the models check the
//! shipped protocol, not a copy. The main build never sets `cfg(loom)`.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU32, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU32, Ordering};

/// Hard capacity ceiling: keeps `seq - pos` differences well inside `i32`
/// under wrapping `u32` arithmetic.
pub const MAX_RING_ENTRIES: u32 = 1 << 16;

/// Hard capacity floor. A 1-cell ring is structurally broken in the
/// cell-sequence scheme: position 1's claim check (`seq == 1`) is
/// indistinguishable from position 0's publication (`seq == 0 + 1`), so a
/// second producer would overwrite an unconsumed entry (found by the
/// ring-vs-reference property suite; Vyukov's original asserts the same
/// floor).
pub const MIN_RING_ENTRIES: u32 = 2;

/// One ring cell: a publication sequence plus the published op-slot index.
///
/// `seq` encodes the cell's lap state (Vyukov): `index` = empty and
/// claimable by the producer whose reserved position is `index`;
/// `position + 1` = published, consumable; `position + capacity` = freed
/// for the next lap.
#[repr(C)]
#[derive(Debug)]
pub struct RingCell {
    seq: AtomicU32,
    value: AtomicU32,
}

impl RingCell {
    fn seeded(index: u32) -> Self {
        Self {
            seq: AtomicU32::new(index),
            value: AtomicU32::new(0),
        }
    }
}

/// Geometry refusals for [`MpscRingView::from_parts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingGeometryError {
    /// Capacity must be a power of two (mask indexing).
    NotPowerOfTwo,
    /// Capacity below [`MIN_RING_ENTRIES`] (see there — a 1-cell ring is
    /// structurally broken).
    TooSmall,
    /// Capacity above [`MAX_RING_ENTRIES`].
    TooLarge,
}

impl core::fmt::Display for RingGeometryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotPowerOfTwo => write!(f, "ring capacity must be a power of two"),
            Self::TooSmall => write!(f, "ring capacity must be at least MIN_RING_ENTRIES"),
            Self::TooLarge => write!(f, "ring capacity exceeds MAX_RING_ENTRIES"),
        }
    }
}

impl std::error::Error for RingGeometryError {}

/// A borrowed view of a ring's shared words (tail + cells). Producers and
/// the consumer both operate through a view; the memory itself lives either
/// in an owned [`RingStorage`] (models, rig) or in the mapped session shm
/// (production — the daemon/shim cast the mapped region into these types).
#[derive(Debug, Clone, Copy)]
pub struct MpscRingView<'a> {
    tail: &'a AtomicU32,
    cells: &'a [RingCell],
    mask: u32,
}

impl<'a> MpscRingView<'a> {
    /// Build a view over `tail` + `cells`. `cells.len()` must be a power of
    /// two in [`MIN_RING_ENTRIES`]..=[`MAX_RING_ENTRIES`].
    pub fn from_parts(
        tail: &'a AtomicU32,
        cells: &'a [RingCell],
    ) -> Result<Self, RingGeometryError> {
        let len = cells.len();
        if len < MIN_RING_ENTRIES as usize {
            return Err(RingGeometryError::TooSmall);
        }
        if !len.is_power_of_two() {
            return Err(RingGeometryError::NotPowerOfTwo);
        }
        if len > MAX_RING_ENTRIES as usize {
            return Err(RingGeometryError::TooLarge);
        }
        Ok(Self {
            tail,
            cells,
            mask: (len - 1) as u32,
        })
    }

    /// Ring capacity in entries.
    pub fn capacity(&self) -> u32 {
        self.mask + 1
    }

    /// Producer side (any thread, any number of racing producers): publish
    /// one op-slot index. Returns `false` when the ring is full (client-
    /// visible backpressure — the caller spins/parks/falls through per the
    /// design §5.4.1 ladder; the daemon is never blocked by a full ring).
    pub fn push(&self, value: u32) -> bool {
        let mut tail = self.tail.load(Ordering::Relaxed);
        loop {
            let cell = &self.cells[(tail & self.mask) as usize];
            // Acquire pairs with the consumer's cell-freeing Release store:
            // a producer reusing the cell on lap N+1 must see it freed.
            let seq = cell.seq.load(Ordering::Acquire);
            let dif = seq.wrapping_sub(tail) as i32;
            match dif.cmp(&0) {
                core::cmp::Ordering::Equal => {
                    // Cell is empty at exactly our position: reserve it.
                    // Relaxed CAS on the tail is the classic bounded-MPSC
                    // shape — publication rides the cell seq below, never
                    // the tail word.
                    match self.tail.compare_exchange_weak(
                        tail,
                        tail.wrapping_add(1),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            cell.value.store(value, Ordering::Relaxed);
                            // Publish: seq = position + 1. Release pairs
                            // with the consumer's Acquire so the value
                            // store above is visible with it.
                            cell.seq.store(tail.wrapping_add(1), Ordering::Release);
                            return true;
                        }
                        Err(observed) => tail = observed,
                    }
                }
                core::cmp::Ordering::Less => {
                    // Previous-lap occupant not yet freed: the ring is
                    // full. (A hostile client corrupting seqs can force
                    // this observation — bounded backpressure, never UB.)
                    return false;
                }
                core::cmp::Ordering::Greater => {
                    // Another producer already published past us; reload.
                    tail = self.tail.load(Ordering::Relaxed);
                }
            }
        }
    }
}

/// The single consumer's private cursor. Lives in daemon-private memory —
/// never in the shared mapping (see the module-level single-consumer
/// precondition). Intentionally `!Clone`: one cursor per ring, owned by the
/// one service thread the session is pinned to.
#[derive(Debug, Default)]
pub struct RingConsumer {
    head: u32,
}

impl RingConsumer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume the next published index, if any. Must only ever be called
    /// by the one consumer (module-level precondition).
    pub fn pop(&mut self, ring: &MpscRingView<'_>) -> Option<u32> {
        let head = self.head;
        let cell = &ring.cells[(head & ring.mask) as usize];
        // Acquire pairs with the producer's publishing Release store.
        let seq = cell.seq.load(Ordering::Acquire);
        if seq.wrapping_sub(head.wrapping_add(1)) as i32 == 0 {
            let value = cell.value.load(Ordering::Relaxed);
            // Free the cell for lap N+1: seq = position + capacity.
            // Release pairs with a reusing producer's Acquire (our value
            // read above must not be reordered past the hand-back).
            cell.seq
                .store(head.wrapping_add(ring.capacity()), Ordering::Release);
            self.head = head.wrapping_add(1);
            Some(value)
        } else {
            // Not yet published (or a hostile client scribbled the seq —
            // observed as empty; bounded behavior, never out-of-bounds).
            None
        }
    }
}

/// Owned ring storage: the loom models' and the rig daemon-role's way to
/// hold a ring outside a shared mapping (production casts mapped session
/// memory instead — PR L4-3).
#[derive(Debug)]
pub struct RingStorage {
    tail: AtomicU32,
    cells: Box<[RingCell]>,
}

impl RingStorage {
    /// Allocate + seed a ring. `capacity` must satisfy
    /// [`MpscRingView::from_parts`]'s geometry rules.
    pub fn with_capacity(capacity: u32) -> Result<Self, RingGeometryError> {
        if capacity < MIN_RING_ENTRIES {
            return Err(RingGeometryError::TooSmall);
        }
        if !capacity.is_power_of_two() {
            return Err(RingGeometryError::NotPowerOfTwo);
        }
        if capacity > MAX_RING_ENTRIES {
            return Err(RingGeometryError::TooLarge);
        }
        Ok(Self {
            tail: AtomicU32::new(0),
            cells: (0..capacity).map(RingCell::seeded).collect(),
        })
    }

    /// A borrowed protocol view over this storage.
    pub fn view(&self) -> MpscRingView<'_> {
        MpscRingView::from_parts(&self.tail, &self.cells)
            .expect("RingStorage geometry validated at construction")
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn geometry_refusals_exact() {
        assert_eq!(
            RingStorage::with_capacity(0).unwrap_err(),
            RingGeometryError::TooSmall
        );
        assert_eq!(
            RingStorage::with_capacity(1).unwrap_err(),
            RingGeometryError::TooSmall,
            "a 1-cell ring is structurally broken (see MIN_RING_ENTRIES)"
        );
        assert_eq!(
            RingStorage::with_capacity(3).unwrap_err(),
            RingGeometryError::NotPowerOfTwo
        );
        assert_eq!(
            RingStorage::with_capacity(MAX_RING_ENTRIES * 2).unwrap_err(),
            RingGeometryError::TooLarge
        );
        let tail = AtomicU32::new(0);
        assert_eq!(
            MpscRingView::from_parts(&tail, &[]).unwrap_err(),
            RingGeometryError::TooSmall
        );
        let cells: Vec<RingCell> = (0..3).map(RingCell::seeded).collect();
        assert_eq!(
            MpscRingView::from_parts(&tail, &cells).unwrap_err(),
            RingGeometryError::NotPowerOfTwo
        );
    }

    #[test]
    fn fifo_single_producer_single_consumer() {
        let storage = RingStorage::with_capacity(8).unwrap();
        let ring = storage.view();
        let mut consumer = RingConsumer::new();
        assert_eq!(consumer.pop(&ring), None, "fresh ring must be empty");
        for v in 1..=8u32 {
            assert!(ring.push(v), "ring must accept up to capacity");
        }
        for v in 1..=8u32 {
            assert_eq!(consumer.pop(&ring), Some(v), "FIFO order");
        }
        assert_eq!(consumer.pop(&ring), None, "drained ring must be empty");
    }

    #[test]
    fn full_ring_refuses_then_reopens_after_pop() {
        let storage = RingStorage::with_capacity(4).unwrap();
        let ring = storage.view();
        let mut consumer = RingConsumer::new();
        for v in 0..4u32 {
            assert!(ring.push(v));
        }
        assert!(!ring.push(99), "full ring must refuse (backpressure)");
        assert_eq!(consumer.pop(&ring), Some(0));
        assert!(ring.push(99), "pop must reopen exactly one cell");
        assert!(!ring.push(100), "and only one");
        for expect in [1, 2, 3, 99] {
            assert_eq!(consumer.pop(&ring), Some(expect), "wrap keeps FIFO");
        }
    }

    #[test]
    fn many_laps_keep_order_and_capacity_exact() {
        let storage = RingStorage::with_capacity(2).unwrap();
        let ring = storage.view();
        let mut consumer = RingConsumer::new();
        for lap in 0..1000u32 {
            let (a, b) = (lap * 2, lap * 2 + 1);
            assert!(ring.push(a));
            assert!(ring.push(b));
            assert!(!ring.push(u32::MAX), "capacity 2 holds exactly 2");
            assert_eq!(consumer.pop(&ring), Some(a));
            assert_eq!(consumer.pop(&ring), Some(b));
            assert_eq!(consumer.pop(&ring), None);
        }
    }

    /// Real-atomics stress (loom covers the small interleavings
    /// exhaustively; this covers big-N contention): 4 producers × 4096
    /// values through a capacity-64 ring into one consumer — every value
    /// delivered exactly once.
    #[test]
    fn multi_producer_stress_exactly_once() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        const PRODUCERS: u32 = 4;
        const PER_PRODUCER: u32 = 4096;

        let storage = Arc::new(RingStorage::with_capacity(64).unwrap());
        let done = Arc::new(AtomicBool::new(false));

        let producers: Vec<_> = (0..PRODUCERS)
            .map(|p| {
                let storage = Arc::clone(&storage);
                std::thread::spawn(move || {
                    let ring = storage.view();
                    for i in 0..PER_PRODUCER {
                        let v = p * PER_PRODUCER + i;
                        while !ring.push(v) {
                            std::hint::spin_loop();
                        }
                    }
                })
            })
            .collect();

        let consumer_done = Arc::clone(&done);
        let consumer_storage = Arc::clone(&storage);
        let consumer = std::thread::spawn(move || {
            let ring = consumer_storage.view();
            let mut cursor = RingConsumer::new();
            let mut seen = vec![0u32; (PRODUCERS * PER_PRODUCER) as usize];
            let mut count = 0u32;
            while count < PRODUCERS * PER_PRODUCER {
                match cursor.pop(&ring) {
                    Some(v) => {
                        seen[v as usize] += 1;
                        count += 1;
                    }
                    None => {
                        if consumer_done.load(Ordering::Acquire) && cursor.pop(&ring).is_none() {
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }
            seen
        });

        for p in producers {
            p.join().unwrap();
        }
        done.store(true, Ordering::Release);
        let seen = consumer.join().unwrap();
        for (v, n) in seen.iter().enumerate() {
            assert_eq!(
                *n, 1,
                "value {v} delivered {n} times (must be exactly once)"
            );
        }
    }
}
