//! **POSIX-7** (`docs/pre-rc-engineering-spec.md` §5): the process-wide
//! set of inodes that have received a `MAP_SHARED` mapping — the shim's
//! memory of "this file has a second writer the ring cannot see".
//!
//! Ring writes bypass the kernel page cache entirely, so a file that is
//! both mapped writable and served over the ring has two writers with no
//! arbiter: a dirty mapped page writes back over ring-written bytes, or
//! the reverse. The daemon's BIND screen refuses `O_APPEND`/`O_SYNC`/
//! `O_TMPFILE`, but a mapping is created AFTER the bind — it cannot be
//! screened there. The interposer's answer is to unbind the inode when
//! `mmap` runs (§5.4.1 Issue-22); this set is the missing MEMORY, so the
//! ring can never be re-armed underneath a live mapping:
//!
//! * `mmap`/`mmap64` with `MAP_SHARED` inserts the mapped `(st_dev,
//!   st_ino)` **before** the real call, and
//! * `classify_and_bind` refuses any fd whose `(st_dev, st_ino)` is in
//!   the set — the fd stays kernel-served for the process's lifetime.
//!
//! ## Why it never forgets
//!
//! [`crate::dev_cache::NegativeDevCache`] may evict: a lost entry there
//! costs one re-classification. A lost entry HERE re-arms the ring under
//! a live mapping — silent data corruption. So this set never evicts: it
//! is a fixed-capacity open-addressed table, and once full it latches
//! [`MappedInoSet::is_overflowed`], after which `contains` answers
//! **true for everything**. The mount degrades to kernel-served, which
//! is correct and merely slower. (Overflow needs
//! [`MappedInoSet::CAPACITY`] distinct shared-mapped files on the mount
//! in one process.)
//!
//! Entries are `(dev, ino)` **fingerprints**, so a hash collision can
//! only ever poison an innocent inode — passthrough, never incoherence.
//!
//! Async-signal-safe by construction: atomics only, zero allocation and
//! zero locks after `new()` (the interposer contract — `close(2)` is
//! AS-safe and apps do call it from signal handlers).
//!
//! **Not closed here (documented deviation, `docs/operations.md` R9):**
//! a mapping in ANOTHER process is invisible to this one. Writable
//! shared mmap of a file a different process is intercepting — and the
//! cross-node case — is a guarantee-table refusal, not a code fix: the
//! shim cannot see a peer's `mmap`, and the daemon is not told about
//! mappings by the kernel.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Slot count: 8192 × 8 B = 64 KiB. Overflow (see the module docs) is a
/// correctness-preserving degradation, not a failure, so this is sized
/// to make it merely unreachable in practice rather than impossible.
const SLOTS: usize = 8192;

/// Does a mapping with these `mmap` flags poison the inode's bindings?
///
/// `MAP_SHARED` and `MAP_SHARED_VALIDATE` (the sharing-type field is a
/// small enum in the low bits, NOT a bitmask — `MAP_SHARED_VALIDATE`
/// is `0x03`, which contains `MAP_SHARED`'s bit but is a distinct
/// value) are the writeback classes: their dirty pages reach the file.
/// `MAP_PRIVATE` never writes back — the interposer still unbinds the
/// inode so the mapping's reads stay coherent, but it does not poison
/// future binds.
pub fn mapping_poisons_bindings(flags: libc::c_int) -> bool {
    const MAP_SHARED_VALIDATE: libc::c_int = 0x03;
    let kind = flags & 0x0f;
    kind == libc::MAP_SHARED || kind == MAP_SHARED_VALIDATE
}

/// The never-forgetting set (module docs).
pub struct MappedInoSet {
    slots: Box<[AtomicU64; SLOTS]>,
    /// Latched when an insert found no free slot: `contains` then
    /// answers true for everything (fail safe).
    overflow: AtomicBool,
}

impl Default for MappedInoSet {
    fn default() -> Self {
        Self::new()
    }
}

impl MappedInoSet {
    /// Distinct shared-mapped files this process can remember before it
    /// degrades the whole mount to kernel-served.
    pub const CAPACITY: usize = SLOTS;

    pub fn new() -> Self {
        let v: Vec<AtomicU64> = (0..SLOTS).map(|_| AtomicU64::new(0)).collect();
        let slots: Box<[AtomicU64; SLOTS]> = v
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!("built with SLOTS entries"));
        Self {
            slots,
            overflow: AtomicBool::new(false),
        }
    }

    /// splitmix64 over the pair; 0 is reserved for "empty".
    #[inline]
    fn key(dev: u64, ino: u64) -> u64 {
        let mut x = dev
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(ino.wrapping_add(0x1234_5678_9abc_def0));
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^= x >> 31;
        if x == 0 {
            1
        } else {
            x
        }
    }

    #[inline]
    fn home(key: u64) -> usize {
        (key as usize) % SLOTS
    }

    /// Has this inode ever received a `MAP_SHARED` mapping in this
    /// process (or has the set overflowed)? A `true` answer means "never
    /// bind this fd".
    pub fn contains(&self, dev: u64, ino: u64) -> bool {
        if self.overflow.load(Ordering::Relaxed) {
            return true; // fail safe — see the module docs
        }
        let key = Self::key(dev, ino);
        let home = Self::home(key);
        for i in 0..SLOTS {
            let cur = self.slots[(home + i) % SLOTS].load(Ordering::Acquire);
            if cur == key {
                return true;
            }
            if cur == 0 {
                return false; // an empty slot ends the probe run
            }
        }
        true
    }

    /// Remember this inode forever (idempotent). Latches overflow if
    /// there is no free slot.
    pub fn insert(&self, dev: u64, ino: u64) {
        let key = Self::key(dev, ino);
        let home = Self::home(key);
        for i in 0..SLOTS {
            let slot = &self.slots[(home + i) % SLOTS];
            let cur = slot.load(Ordering::Acquire);
            if cur == key {
                return;
            }
            if cur == 0 {
                match slot.compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => return,
                    // Lost the race: re-read this slot before moving on
                    // (the winner may have written OUR key).
                    Err(now) if now == key => return,
                    Err(_) => continue,
                }
            }
        }
        self.overflow.store(true, Ordering::Relaxed);
    }

    /// `true` once the set degraded to "poison everything".
    pub fn is_overflowed(&self) -> bool {
        self.overflow.load(Ordering::Relaxed)
    }

    /// Live entry count (diagnostics + tests; O(SLOTS)).
    pub fn len(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.load(Ordering::Relaxed) != 0)
            .count()
    }

    /// Clippy's companion to [`Self::len`].
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
