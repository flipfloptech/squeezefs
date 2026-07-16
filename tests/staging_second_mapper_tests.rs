//! FIND-VS-A (part 1, teardown SIGBUS): a second `NvmeCache` mapper over
//! EXISTING segment files must never shrink them.
//!
//! Root cause pinned by the 2026-07-16 forensics: `squeezefs umount`
//! constructed an `NvmeCache` over the LIVE daemon's `staging_segment/`
//! dir with a hardcoded 100 MiB budget; `NvmeShard::new` unconditionally
//! `set_len(capacity)`'d every existing 128 MiB segment down to 6.25 MiB
//! while the daemon held full-size mmaps. Every teardown-drain access
//! beyond the new EOF then faulted `SIGBUS (BUS_ADRERR)` — the daemon
//! died mid-flush (8 coredumps across the scoreboard session, stacks in
//! `NvmeStaging::{remove_active_block,get_staged_fencing_token}`) — and
//! the truncation destroyed staged payload bytes on disk.
//!
//! Invariant: opening a shard over an existing file may GROW it to the
//! requested capacity but never shrink it, so no second mapper can
//! invalidate a live mapping's pages (or destroy staged data).
use bytes::Bytes;
use squeezefs::tiering::nvme::NvmeCache;
use tempfile::tempdir;

/// A value large enough that its extent sits beyond the small-mapper
/// capacity used below — the exact byte range the umount-CLI truncation
/// destroyed.
const BIG_VAL: usize = 3 * 1024 * 1024;
const FULL_CAP: usize = 8 * 1024 * 1024;
const SMALL_CAP: usize = 1024 * 1024;

#[test]
fn second_mapper_with_smaller_capacity_never_shrinks_segment_files() {
    let dir = tempdir().expect("tempdir");
    let full = NvmeCache::new(&[dir.path()], &[FULL_CAP], 1).expect("full-size cache");

    // Fill past SMALL_CAP so live bytes sit in the range a shrink would
    // destroy (and a live mmap would SIGBUS on).
    let keys: Vec<Bytes> = (0..2)
        .map(|i| Bytes::from(format!("active_block:9:{i}")))
        .collect();
    for (i, key) in keys.iter().enumerate() {
        let val = Bytes::from(vec![0xA0 + i as u8; BIG_VAL]);
        let evicted = full.put(key.clone(), val);
        assert!(evicted.is_empty(), "8 MiB ring must hold 6 MiB of entries");
    }

    let seg = dir.path().join("segment_0.bin");
    let len_before = std::fs::metadata(&seg).expect("segment exists").len();
    assert_eq!(len_before, FULL_CAP as u64, "full-size segment on disk");

    // The umount-CLI shape: a second mapper over the SAME directory with a
    // much smaller capacity (100 MiB / 16 shards in production).
    let small = NvmeCache::new(&[dir.path()], &[SMALL_CAP], 1).expect("second mapper");

    let len_after = std::fs::metadata(&seg).expect("segment exists").len();
    assert_eq!(
        len_after, FULL_CAP as u64,
        "opening an existing segment with a smaller capacity must NOT \
         shrink it (a live mmap of the full size would SIGBUS beyond the \
         new EOF, and staged payload bytes beyond it are destroyed)"
    );

    // The first mapper's view must still serve every byte (pre-fix this
    // range is beyond EOF: reading it through the mmap faults).
    for (i, key) in keys.iter().enumerate() {
        let guard = full.get(key).expect("entry survives the second mapper");
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        assert_eq!(bytes.len(), BIG_VAL);
        assert!(
            bytes.iter().all(|b| *b == 0xA0 + i as u8),
            "staged bytes intact after the second mapper"
        );
    }
    drop(small);
}

#[test]
fn opening_with_larger_capacity_grows_the_segment_file() {
    let dir = tempdir().expect("tempdir");
    {
        let _small = NvmeCache::new(&[dir.path()], &[SMALL_CAP], 1).expect("small cache");
    }
    let seg = dir.path().join("segment_0.bin");
    assert_eq!(
        std::fs::metadata(&seg).unwrap().len(),
        SMALL_CAP as u64,
        "created at the small capacity"
    );
    let full = NvmeCache::new(&[dir.path()], &[FULL_CAP], 1).expect("grown cache");
    assert_eq!(
        std::fs::metadata(&seg).unwrap().len(),
        FULL_CAP as u64,
        "growing to the requested capacity is safe and required"
    );
    // And the grown ring admits a block-size entry.
    let key = Bytes::from_static(b"grown");
    let evicted = full.put(key.clone(), Bytes::from(vec![7u8; BIG_VAL]));
    assert!(evicted.is_empty());
    assert!(full.get(&key).is_some());
}
