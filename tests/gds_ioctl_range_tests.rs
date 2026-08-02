//! VAL-1 (pre-RC engineering spec §3) — the GDS ioctl's caller-supplied
//! arguments must be range-checked, and the block-key resolve they
//! amplify into must carry a per-call ceiling.
//!
//! The reproduction the reviewer verified: `args` is read verbatim from
//! caller memory, `min(args.offset + args.size, file_size)` is an
//! UNCHECKED add in a release profile with no overflow-checks, and the
//! arm was **not** `#[cfg(feature = "gds")]`-gated — it shipped in the
//! default build. With `offset = 1, size = u64::MAX` the add wraps to 0,
//! `end_offset - 1` wraps, `as u32` truncates to `0xFFFF_FFFF`, and
//! `load_striped_block_keys` then ran `for b in 0..=u32::MAX`, building a
//! `Vec<(u32, Option<String>)>` of roughly 137 GB — with `panic = "abort"`
//! the allocation failure kills the daemon. The same loop is reachable
//! WITHOUT any overflow: a file `ftruncate`d to `max_file_size()` plus a
//! whole-file request is 4.29 G iterations.
//!
//! The ioctl arm delegates ALL of its `(offset, size) → (start_block,
//! end_block)` arithmetic to [`gds_read_block_range`], so these unit
//! tests are the arm's contract; the amplifier's own ceiling is pinned
//! against a live `DataRouter`.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{gds_read_block_range, SqueezefsFilesystem};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{max_block_keys_for_budget, max_block_keys_per_call, DataRouter};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

const BS: u64 = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// The overflow tuple + the arithmetic contract
// ---------------------------------------------------------------------------

#[test]
fn overflowing_offset_plus_size_is_einval_not_a_wrapped_range() {
    // The verified reproduction tuple.
    let e = gds_read_block_range(1, u64::MAX, 8 * BS, BS)
        .expect_err("offset + size must never wrap into a range");
    assert_eq!(e, libc::EINVAL, "the wrapping tuple must refuse EINVAL");

    // Every other add that cannot be represented refuses identically —
    // an `Ok(None)` ("nothing to read") is NOT an acceptable answer for
    // arguments that do not describe a range.
    for (offset, size) in [
        (u64::MAX, 1),
        (u64::MAX, u64::MAX),
        (u64::MAX - 1, 2),
        (1 << 63, 1 << 63),
    ] {
        let e = gds_read_block_range(offset, size, 8 * BS, BS)
            .expect_err("an unrepresentable add must be an error, never a range");
        assert_eq!(e, libc::EINVAL, "offset={offset} size={size}");
    }
}

#[test]
fn whole_file_at_max_file_size_is_refused_not_a_four_billion_block_span() {
    // `max_file_size()` = block_size * u32::MAX (u32 block-index
    // representability). A whole-file GDS request against it needs no
    // overflow at all — it is simply 4.29 G blocks.
    let file_size = BS.saturating_mul(u32::MAX as u64).min(i64::MAX as u64);
    let e = gds_read_block_range(0, file_size, file_size, BS)
        .expect_err("a 4.29 G-block span must refuse, never materialize");
    assert_eq!(e, libc::EINVAL);

    // ... and the boundary is the per-call ceiling, not a magic size:
    // exactly-at-cap resolves, one block past it refuses.
    let cap = u64::from(max_block_keys_per_call());
    let at_cap = gds_read_block_range(0, cap * BS, file_size, BS).expect("at-cap span resolves");
    let at_cap = at_cap.expect("at-cap span is a real range");
    assert_eq!(at_cap.start_block, 0);
    assert_eq!(u64::from(at_cap.end_block), cap - 1);

    let e = gds_read_block_range(0, (cap + 1) * BS, file_size, BS)
        .expect_err("one block past the ceiling refuses");
    assert_eq!(e, libc::EINVAL);
}

#[test]
fn empty_and_past_eof_requests_resolve_to_nothing_to_do() {
    // `end_offset == 0` must early-return instead of computing
    // `(end_offset - 1)` (the wrap the reviewer named).
    assert!(gds_read_block_range(0, 4096, 0, BS).expect("empty file").is_none());
    assert!(gds_read_block_range(0, 0, 8 * BS, BS).expect("zero-size read").is_none());
    assert!(gds_read_block_range(8 * BS, 4096, 8 * BS, BS)
        .expect("at EOF")
        .is_none());
    assert!(gds_read_block_range(u64::MAX - 1, 1, 8 * BS, BS)
        .expect("far past EOF")
        .is_none());
    // A zero block size can only come from a corrupt geometry; it must
    // refuse, never divide.
    assert_eq!(
        gds_read_block_range(0, 4096, 8 * BS, 0).expect_err("zero block size"),
        libc::EINVAL
    );
}

#[test]
fn in_bounds_requests_keep_their_exact_historical_range() {
    let file_size = 10 * BS + 1234;

    // Whole file.
    let r = gds_read_block_range(0, file_size, file_size, BS)
        .expect("ok")
        .expect("range");
    assert_eq!((r.start_block, r.end_block), (0, 10));
    assert_eq!(r.end_offset, file_size);

    // Interior, block-aligned.
    let r = gds_read_block_range(BS, 2 * BS, file_size, BS)
        .expect("ok")
        .expect("range");
    assert_eq!((r.start_block, r.end_block), (1, 2));
    assert_eq!(r.end_offset, 3 * BS);

    // Unaligned, spanning a boundary.
    let r = gds_read_block_range(BS - 1, 2, file_size, BS)
        .expect("ok")
        .expect("range");
    assert_eq!((r.start_block, r.end_block), (0, 1));

    // A size that runs past EOF clamps to the file's real block count.
    let r = gds_read_block_range(9 * BS, u64::MAX / 2, file_size, BS)
        .expect("ok")
        .expect("range");
    assert_eq!((r.start_block, r.end_block), (9, 10));
    assert_eq!(r.end_offset, file_size);
}

// ---------------------------------------------------------------------------
// The ceiling's derivation (drift-is-red tie test)
// ---------------------------------------------------------------------------

#[test]
fn the_per_call_ceiling_derives_from_the_memory_budget_with_a_physical_floor() {
    // An unresolved (0) budget can never refuse honest work: the floor
    // is a physical minimum, not a tuning constant.
    let floor = max_block_keys_for_budget(0);
    assert!(floor >= 262_144, "floor must cover the largest legitimate span");
    assert_eq!(max_block_keys_for_budget(1), floor);

    // It grows with the budget and never wraps at the extremes.
    let big = max_block_keys_for_budget(1 << 40);
    assert!(big > floor, "a 1 TiB budget must derive above the floor");
    assert!(max_block_keys_for_budget(u64::MAX) <= u32::MAX);
    assert!(max_block_keys_for_budget(u64::MAX) >= big);

    // The live accessor is the same function against the live budget.
    assert!(max_block_keys_per_call() >= floor);
}

// ---------------------------------------------------------------------------
// The amplifier: `load_striped_block_keys` must bound itself
// ---------------------------------------------------------------------------

struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    _backing: NamedTempFile,
    _staging: tempfile::TempDir,
}

async fn fixture(tag: &str) -> Fx {
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), tag)
            .await
            .expect("allocator"),
    );
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, alloc, nvme);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    Fx {
        fs: Arc::new(fs),
        _backing: backing,
        _staging: staging,
    }
}

fn striped_meta(size: u64) -> squeezefs::routing::CachedMetadata {
    squeezefs::routing::CachedMetadata {
        file_type: "striped".into(),
        size,
        block_prefix: Some(Arc::from("blk/val1")),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_striped_block_keys_refuses_a_span_past_the_per_call_ceiling() {
    let fx = fixture("val1-cap").await;
    let path = squeezefs::keys::inode_path(42);
    let meta = striped_meta(BS * 8);
    let cap = max_block_keys_per_call();

    // The 137 GB shape: the amplifier must refuse BEFORE it allocates.
    let started = std::time::Instant::now();
    let err = fx
        .fs
        .router
        .load_striped_block_keys(&path, &meta, 0, u32::MAX)
        .await
        .expect_err("a 4.29 G-entry resolve must refuse, never allocate");
    assert!(
        format!("{err:?}").contains("block-key"),
        "the refusal must name the bound it hit: {err:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the refusal is a bounds check, not a materialization"
    );

    // One past the ceiling refuses; the ceiling itself is a real answer
    // only when the caller asks for it (kept small here — this test
    // pins the refusal edge, not the allocation).
    let err = fx
        .fs
        .router
        .load_striped_block_keys(&path, &meta, 0, cap)
        .await
        .expect_err("cap + 1 entries must refuse");
    assert!(format!("{err:?}").contains("block-key"), "{err:?}");

    // Legitimate read spans are untouched.
    let keys = fx
        .fs
        .router
        .load_striped_block_keys(&path, &meta, 2, 5)
        .await
        .expect("an ordinary 4-block span still resolves");
    assert_eq!(keys.len(), 4);
    assert_eq!(keys[0].0, 2);
    assert_eq!(keys[3].0, 5);
}

// ---------------------------------------------------------------------------
// Feature gating: the arm is not in the default build
// ---------------------------------------------------------------------------

#[cfg(not(feature = "gds"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gds_ioctl_arm_is_absent_from_the_default_build() {
    use fuse3::raw::prelude::Filesystem;
    use fuse3::raw::Request;

    let fx = fixture("val1-gate").await;
    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: std::process::id(),
    };
    let e = fx
        .fs
        .ioctl(
            req,
            1,
            0,
            0,
            squeezefs::fuse_client::SQUEEZEFS_IOC_GDS_READ,
            0,
            std::mem::size_of::<squeezefs::fuse_client::GdsReadArgs>() as u32,
            0,
        )
        .await
        .expect_err("without the gds feature the arm must not exist");
    assert_eq!(
        e,
        libc::ENOTTY.into(),
        "the GDS ioctl must answer ENOTTY in a build without the feature"
    );
}
