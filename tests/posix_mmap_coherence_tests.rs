//! Pre-RC POSIX-semantics contract, spec §5 **POSIX-7**: an interception
//! mount vs `MAP_SHARED`.
//!
//! Ring writes bypass the kernel page cache entirely, so a file that is
//! BOTH mapped writable and served over the ring has two writers with no
//! arbiter: a dirty mapped page writes back over ring-written bytes, or
//! the reverse. The daemon's bind screen refuses `O_APPEND`/`O_SYNC`/
//! `O_TMPFILE` at BIND, but a mapping is created AFTER the bind, so the
//! screen cannot see it.
//!
//! The shim already unbinds every in-process binding on the mapped inode
//! when it interposes `mmap`/`mmap64` (§5.4.1 Issue-22, landed with the
//! L4 interposer wave — the spec's "cannot refuse mmap" reading is
//! stale for that half). What was missing is MEMORY: the unbind is a
//! point-in-time act, so the very next `open()` of the same file — or an
//! `mmap()` that precedes the first `open()` of it — re-armed the ring
//! underneath a live mapping. This suite pins the poison set that closes
//! that: an inode with a live `MAP_SHARED` mapping in this process is
//! never bound again for the process's lifetime.
//!
//! **Coverage boundary (repro-port mandate):** the set and the mapping
//! predicate are pinned here, in-process. The interposer WIRING
//! (`mmap` → poison, `open` → refusal) cannot be: the `interposers`
//! feature is deliberately off for rlib consumers — strong `open`/`read`
//! symbols in a test binary would interpose the test itself — so the
//! end-to-end leg lives in the root-only preload gate
//! (`tests/run_preload_gate.sh`, leg 2).

use squeezefs_il::mapped_inos::{mapping_poisons_bindings, MappedInoSet};

/// The core law: a mapped inode is remembered, and remembering is what
/// keeps a later `open()` of the same file off the ring.
#[test]
fn a_map_shared_inode_is_remembered_and_never_bindable_again() {
    let set = MappedInoSet::new();
    assert!(!set.contains(0x10, 4242), "nothing is poisoned to start");

    set.insert(0x10, 4242);
    assert!(set.contains(0x10, 4242), "the mapped inode stays poisoned");
    assert!(
        !set.contains(0x10, 4243),
        "an unrelated inode on the same mount is untouched"
    );
    assert!(
        !set.contains(0x11, 4242),
        "the same ino number on another mount is a different file"
    );

    // Idempotent: mapping the same file twice consumes one slot.
    set.insert(0x10, 4242);
    assert_eq!(set.len(), 1);
}

/// Overflow FAILS SAFE. An evicting cache (the negative `st_dev` cache's
/// shape) would silently re-arm the ring under a live mapping — the
/// exact corruption this set exists to prevent — so a full set refuses
/// EVERY bind instead: the mount degrades to kernel-served, which is
/// correct and merely slower.
#[test]
fn overflow_refuses_every_binding_rather_than_forgetting_one() {
    let set = MappedInoSet::new();
    let cap = MappedInoSet::CAPACITY;
    for i in 0..cap as u64 {
        set.insert(0x20, 1000 + i);
    }
    assert!(!set.is_overflowed(), "exactly at capacity still fits");
    assert!(set.contains(0x20, 1000), "the first entry is still there");

    set.insert(0x20, 999_999);
    assert!(set.is_overflowed(), "past capacity latches overflow");
    assert!(
        set.contains(0x99, 7),
        "an overflowed set poisons everything — fail safe, never forget"
    );
}

/// `MAP_SHARED` (and `MAP_SHARED_VALIDATE`) is the writable-writeback
/// class that poisons. `MAP_PRIVATE` keeps the pre-existing behavior
/// (the interposer's unbind still fires; a private mapping can never
/// write back, and the W1 invalidation covers its reads).
#[test]
fn only_shared_mappings_poison() {
    const MAP_SHARED_VALIDATE: libc::c_int = 0x03;
    assert!(mapping_poisons_bindings(libc::MAP_SHARED));
    assert!(mapping_poisons_bindings(
        libc::MAP_SHARED | libc::MAP_FIXED
    ));
    assert!(mapping_poisons_bindings(MAP_SHARED_VALIDATE));
    assert!(!mapping_poisons_bindings(libc::MAP_PRIVATE));
    assert!(!mapping_poisons_bindings(
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS
    ));
}

/// The set is used from interposer context, which must stay
/// async-signal-safe: no allocation and no locks after construction.
/// Concurrency is therefore also free — pin it.
#[test]
fn concurrent_inserts_and_probes_never_lose_an_entry() {
    let set = std::sync::Arc::new(MappedInoSet::new());
    let mut hs = Vec::new();
    for t in 0..8u64 {
        let set = std::sync::Arc::clone(&set);
        hs.push(std::thread::spawn(move || {
            for i in 0..64u64 {
                let ino = t * 64 + i;
                set.insert(7, ino);
                assert!(set.contains(7, ino), "own insert must be visible");
            }
        }));
    }
    for h in hs {
        h.join().expect("thread");
    }
    for ino in 0..512u64 {
        assert!(set.contains(7, ino), "ino {ino} was lost");
    }
}
